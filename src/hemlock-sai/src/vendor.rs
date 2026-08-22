//! The real SAI backend: dlopen the vendor's libsai and drive it through
//! the generated FFI. Linux-only at runtime; compiles anywhere with
//! libclang (CI builds it to keep the FFI honest).
//!
//! Unsafe is confined to this crate by design. Every `unsafe` block here
//! wraps a single FFI call or a read of data the vendor library just wrote.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use tokio::sync::mpsc;

use crate::{ffi, PortId, SaiBackend, SaiError, SaiEvent, SaiPort, SwitchInfo, SwitchInit};

/// SAI profile key/value store handed to the vendor library. Static because
/// the profile callbacks are plain C function pointers with no user data.
static PROFILE: OnceLock<Vec<(CString, CString)>> = OnceLock::new();
static PROFILE_ITER: AtomicUsize = AtomicUsize::new(0);

/// Destination for vendor notification callbacks (same constraint).
static EVENT_TX: OnceLock<mpsc::UnboundedSender<SaiEvent>> = OnceLock::new();

/// The vendor library is process-global state; permit exactly one instance.
static INSTANCE_LIVE: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn profile_get_value(
    _profile_id: ffi::sai_switch_profile_id_t,
    variable: *const c_char,
) -> *const c_char {
    if variable.is_null() {
        return std::ptr::null();
    }
    let wanted = unsafe { std::ffi::CStr::from_ptr(variable) };
    PROFILE
        .get()
        .and_then(|profile| {
            profile
                .iter()
                .find(|(key, _)| key.as_c_str() == wanted)
                .map(|(_, value)| value.as_ptr())
        })
        .unwrap_or(std::ptr::null())
}

unsafe extern "C" fn profile_get_next_value(
    _profile_id: ffi::sai_switch_profile_id_t,
    variable: *mut *const c_char,
    value: *mut *const c_char,
) -> c_int {
    let Some(profile) = PROFILE.get() else {
        return -1;
    };
    // SAI contract: a NULL `value` restarts enumeration (Broadcom's SAI
    // really does call this with NULLs during init — writing through
    // them was a boot-time segfault on the E1031).
    if value.is_null() {
        PROFILE_ITER.store(0, Ordering::SeqCst);
        return 0;
    }
    if variable.is_null() {
        return -1;
    }
    let idx = PROFILE_ITER.fetch_add(1, Ordering::SeqCst);
    match profile.get(idx) {
        Some((key, val)) => {
            unsafe {
                *variable = key.as_ptr();
                *value = val.as_ptr();
            }
            0
        }
        None => {
            PROFILE_ITER.store(0, Ordering::SeqCst);
            -1
        }
    }
}

unsafe extern "C" fn on_port_state_change(
    count: u32,
    data: *const ffi::sai_port_oper_status_notification_t,
) {
    let Some(tx) = EVENT_TX.get() else { return };
    if data.is_null() {
        return;
    }
    let items = unsafe { std::slice::from_raw_parts(data, count as usize) };
    for item in items {
        let _ = tx.send(SaiEvent::PortOperStatus {
            port: PortId(item.port_id),
            up: item.port_state == ffi::_sai_port_oper_status_t::SAI_PORT_OPER_STATUS_UP,
        });
    }
}

fn check(call: &'static str, status: ffi::sai_status_t) -> Result<(), SaiError> {
    if status == 0 {
        Ok(())
    } else {
        Err(SaiError::Status { call, status })
    }
}

pub struct VendorSai {
    /// Keeps the vendor library mapped for the lifetime of the backend.
    library: libloading::Library,
    /// Service table the vendor library may hold a pointer to; boxed so its
    /// address is stable for our whole lifetime.
    _services: Box<ffi::sai_service_method_table_t>,
    switch_api: *mut ffi::sai_switch_api_t,
    port_api: *mut ffi::sai_port_api_t,
    switch_oid: Option<ffi::sai_object_id_t>,
    events_rx: Option<mpsc::UnboundedReceiver<SaiEvent>>,
    src_mac: Option<[u8; 6]>,
    name: String,
}

// SAFETY: the raw API-table pointers are only dereferenced from the single
// task that owns this backend; syncd never shares it across threads.
unsafe impl Send for VendorSai {}

impl VendorSai {
    /// Load the vendor library named by the platform manifest and stage its
    /// SAI profile (`SAI_INIT_CONFIG_FILE` -> config.bcm).
    pub fn new(init: &SwitchInit) -> Result<Self, SaiError> {
        if INSTANCE_LIVE.swap(true, Ordering::SeqCst) {
            return Err(SaiError::Other(
                "a VendorSai instance already exists in this process".into(),
            ));
        }

        let mut profile = vec![(
            CString::new("SAI_INIT_CONFIG_FILE").expect("static key"),
            path_to_cstring(&init.config_bcm_path)?,
        )];
        for (key, value) in &init.profile {
            profile.push((
                CString::new(key.as_str())
                    .map_err(|_| SaiError::Other(format!("NUL in profile key {key:?}")))?,
                CString::new(value.as_str())
                    .map_err(|_| SaiError::Other(format!("NUL in profile value for {key:?}")))?,
            ));
        }
        for (key, value) in &profile {
            tracing::info!(key = ?key, value = ?value, "SAI profile entry");
        }
        PROFILE
            .set(profile)
            .map_err(|_| SaiError::Other("SAI profile already initialized".into()))?;

        let (tx, rx) = mpsc::unbounded_channel();
        EVENT_TX
            .set(tx)
            .map_err(|_| SaiError::Other("SAI event channel already initialized".into()))?;

        // SAFETY: loading the vendor-provided SAI shared object; its ctors
        // are trusted vendor code, which is the entire premise of a NOS.
        let library = unsafe { libloading::Library::new(&init.libsai_path) }
            .map_err(|e| SaiError::Load(format!("{}: {e}", init.libsai_path.display())))?;

        let services = Box::new(ffi::sai_service_method_table_t {
            profile_get_value: Some(profile_get_value),
            profile_get_next_value: Some(profile_get_next_value),
        });

        // SAFETY: symbol lookup + the documented SAI bootstrap sequence.
        let (switch_api, port_api) = unsafe {
            let api_initialize: libloading::Symbol<
                unsafe extern "C" fn(
                    u64,
                    *const ffi::sai_service_method_table_t,
                ) -> ffi::sai_status_t,
            > = library
                .get(b"sai_api_initialize\0")
                .map_err(|e| SaiError::Load(format!("sai_api_initialize: {e}")))?;
            check("sai_api_initialize", api_initialize(0, &*services))?;

            let api_query: libloading::Symbol<
                unsafe extern "C" fn(ffi::sai_api_t, *mut *mut c_void) -> ffi::sai_status_t,
            > = library
                .get(b"sai_api_query\0")
                .map_err(|e| SaiError::Load(format!("sai_api_query: {e}")))?;

            let mut switch_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(SWITCH)",
                api_query(ffi::_sai_api_t::SAI_API_SWITCH, &mut switch_api),
            )?;
            let mut port_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(PORT)",
                api_query(ffi::_sai_api_t::SAI_API_PORT, &mut port_api),
            )?;
            (
                switch_api as *mut ffi::sai_switch_api_t,
                port_api as *mut ffi::sai_port_api_t,
            )
        };
        Ok(Self {
            library,
            _services: services,
            switch_api,
            port_api,
            switch_oid: None,
            events_rx: Some(rx),
            src_mac: init.src_mac,
            name: format!("vendor:{}", init.libsai_path.display()),
        })
    }

    fn switch_oid(&self) -> Result<ffi::sai_object_id_t, SaiError> {
        self.switch_oid.ok_or(SaiError::NoSwitch)
    }

    fn zeroed_attr(id: u32) -> ffi::sai_attribute_t {
        // SAFETY: sai_attribute_t is POD; an all-zero value is a valid
        // starting point before the union field is assigned.
        let mut attr: ffi::sai_attribute_t = unsafe { std::mem::zeroed() };
        attr.id = id;
        attr
    }
}

fn path_to_cstring(path: &Path) -> Result<CString, SaiError> {
    let s = path
        .to_str()
        .ok_or_else(|| SaiError::Other(format!("non-UTF8 path {path:?}")))?;
    CString::new(s).map_err(|_| SaiError::Other(format!("NUL in path {path:?}")))
}

impl SaiBackend for VendorSai {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn create_switch(&mut self) -> Result<SwitchInfo, SaiError> {
        if self.switch_oid.is_some() {
            return Err(SaiError::Other("switch already created".into()));
        }

        let mut init_attr = Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_INIT_SWITCH);
        init_attr.value.booldata = true;

        let mut profile_attr =
            Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_SWITCH_PROFILE_ID);
        profile_attr.value.u32_ = 0;

        let mut notify_attr =
            Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_PORT_STATE_CHANGE_NOTIFY);
        let notify_cb: unsafe extern "C" fn(u32, *const ffi::sai_port_oper_status_notification_t) =
            on_port_state_change;
        notify_attr.value.ptr = notify_cb as *mut c_void;

        let mut attrs = vec![init_attr, profile_attr, notify_attr];
        if let Some(mac) = self.src_mac {
            // Without this, Broadcom's SAI tries to discover a "local MAC
            // address" itself and fails create_switch on boards where that
            // lookup finds nothing (the E1031).
            let mut mac_attr =
                Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_SRC_MAC_ADDRESS);
            mac_attr.value.mac = mac;
            attrs.push(mac_attr);
        }
        let mut oid: ffi::sai_object_id_t = 0;

        tracing::info!("creating SAI switch (this can take a while on real hardware)");
        // SAFETY: switch_api comes from a successful sai_api_query; the
        // attr array outlives the call.
        unsafe {
            let create = (*self.switch_api)
                .create_switch
                .ok_or(SaiError::Other("switch api lacks create_switch".into()))?;
            check(
                "create_switch",
                create(&mut oid, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        self.switch_oid = Some(oid);
        tracing::info!(oid = format_args!("{oid:#x}"), "SAI switch created");
        Ok(SwitchInfo { oid })
    }

    fn ports(&mut self) -> Result<Vec<SaiPort>, SaiError> {
        let switch = self.switch_oid()?;

        // SAFETY per block below: valid api tables, buffers outlive calls.
        let get_switch_attr = unsafe {
            (*self.switch_api)
                .get_switch_attribute
                .ok_or(SaiError::Other(
                    "switch api lacks get_switch_attribute".into(),
                ))?
        };
        let get_port_attr = unsafe {
            (*self.port_api)
                .get_port_attribute
                .ok_or(SaiError::Other("port api lacks get_port_attribute".into()))?
        };

        // How many ports did config.bcm produce?
        let count = {
            let mut attr = Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_PORT_NUMBER);
            // SAFETY: single-attr get.
            unsafe {
                check("get(PORT_NUMBER)", get_switch_attr(switch, 1, &mut attr))?;
                attr.value.u32_
            }
        };

        tracing::info!(count, "ASIC reports active ports");

        // Fetch their OIDs.
        let mut oids: Vec<ffi::sai_object_id_t> = vec![0; count as usize];
        {
            let mut attr = Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_PORT_LIST);
            attr.value.objlist.count = count;
            attr.value.objlist.list = oids.as_mut_ptr();
            // SAFETY: list buffer sized to `count`, alive across the call.
            unsafe {
                check("get(PORT_LIST)", get_switch_attr(switch, 1, &mut attr))?;
                oids.truncate(attr.value.objlist.count as usize);
            }
        }

        let mut ports = Vec::with_capacity(oids.len());
        for oid in oids {
            let mut lanes: Vec<u32> = vec![0; 8];
            let mut lane_attr =
                Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_HW_LANE_LIST);
            lane_attr.value.u32list.count = lanes.len() as u32;
            lane_attr.value.u32list.list = lanes.as_mut_ptr();

            let speed_attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_SPEED);
            let admin_attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_ADMIN_STATE);
            let oper_attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_OPER_STATUS);

            let mut attrs = [lane_attr, speed_attr, admin_attr, oper_attr];
            // SAFETY: attr array + lane buffer valid across the call; union
            // reads match the attr ids just fetched.
            unsafe {
                check(
                    "get_port_attribute",
                    get_port_attr(oid, attrs.len() as u32, attrs.as_mut_ptr()),
                )?;
                lanes.truncate(attrs[0].value.u32list.count as usize);
                ports.push(SaiPort {
                    id: PortId(oid),
                    lanes,
                    speed_mbps: attrs[1].value.u32_,
                    admin_up: attrs[2].value.booldata,
                    oper_up: attrs[3].value.s32
                        == ffi::_sai_port_oper_status_t::SAI_PORT_OPER_STATUS_UP as i32,
                });
            }
        }
        Ok(ports)
    }

    fn set_port_admin_state(&mut self, port: PortId, up: bool) -> Result<(), SaiError> {
        self.switch_oid()?;
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_ADMIN_STATE);
        attr.value.booldata = up;
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check("set_port_attribute(ADMIN_STATE)", set(port.0, &attr))
        }
    }

    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>> {
        self.events_rx.take()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Broadcom's SAI calls profile_get_next_value with NULL pointers to
    /// restart enumeration; that must never be a write through NULL.
    #[test]
    fn profile_iteration_survives_null_restart() {
        let _ = PROFILE.set(vec![(
            CString::new("SAI_INIT_CONFIG_FILE").unwrap(),
            CString::new("/hemlock/platform/config.bcm").unwrap(),
        )]);

        // NULL value = restart request: must not crash, must return 0.
        let restart =
            unsafe { profile_get_next_value(0, std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(restart, 0);

        // Full enumeration afterwards yields the entry, then end-of-list.
        let mut var: *const c_char = std::ptr::null();
        let mut val: *const c_char = std::ptr::null();
        assert_eq!(unsafe { profile_get_next_value(0, &mut var, &mut val) }, 0);
        assert!(!var.is_null() && !val.is_null());
        assert_eq!(unsafe { profile_get_next_value(0, &mut var, &mut val) }, -1);

        // NULL variable with non-NULL value is refused, not written through.
        let mut val2: *const c_char = std::ptr::null();
        assert_eq!(
            unsafe { profile_get_next_value(0, std::ptr::null_mut(), &mut val2) },
            -1
        );
    }
}

impl Drop for VendorSai {
    fn drop(&mut self) {
        // Best-effort orderly shutdown; the process is usually going down.
        // SAFETY: symbols resolved from the still-loaded library.
        unsafe {
            if let Some(oid) = self.switch_oid {
                if let Some(remove) = (*self.switch_api).remove_switch {
                    let _ = remove(oid);
                }
            }
            if let Ok(uninit) = self
                .library
                .get::<unsafe extern "C" fn() -> ffi::sai_status_t>(b"sai_api_uninitialize\0")
            {
                let _ = uninit();
            }
        }
        INSTANCE_LIVE.store(false, Ordering::SeqCst);
    }
}
