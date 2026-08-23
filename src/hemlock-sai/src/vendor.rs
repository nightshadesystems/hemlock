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

use crate::{
    ffi, IpPrefix, Oid, PortCounters, PortId, QueueCounters, RouteTarget, SaiBackend, SaiError,
    SaiEvent, SaiPort, SwitchInfo, SwitchInit,
};

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

/// Switch-scope objects the L3 family needs, resolved once right after
/// `create_switch` (they exist for the switch's whole lifetime).
#[derive(Debug, Clone, Copy)]
struct SwitchDefaults {
    virtual_router: ffi::sai_object_id_t,
    vlan: ffi::sai_object_id_t,
    /// The default VLAN's 802.1Q number (for restoring a port's PVID).
    vlan_number: u16,
    bridge_1q: ffi::sai_object_id_t,
    cpu_port: ffi::sai_object_id_t,
    trap_group: ffi::sai_object_id_t,
}

pub struct VendorSai {
    /// Keeps the vendor library mapped for the lifetime of the backend.
    library: libloading::Library,
    /// Service table the vendor library may hold a pointer to; boxed so its
    /// address is stable for our whole lifetime.
    _services: Box<ffi::sai_service_method_table_t>,
    switch_api: *mut ffi::sai_switch_api_t,
    port_api: *mut ffi::sai_port_api_t,
    queue_api: *mut ffi::sai_queue_api_t,
    hostif_api: *mut ffi::sai_hostif_api_t,
    rif_api: *mut ffi::sai_router_interface_api_t,
    route_api: *mut ffi::sai_route_api_t,
    bridge_api: *mut ffi::sai_bridge_api_t,
    vlan_api: *mut ffi::sai_vlan_api_t,
    switch_oid: Option<ffi::sai_object_id_t>,
    defaults: Option<SwitchDefaults>,
    events_rx: Option<mpsc::UnboundedReceiver<SaiEvent>>,
    src_mac: Option<[u8; 6]>,
    diag_shell: bool,
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
        let apis = unsafe {
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
            let mut queue_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(QUEUE)",
                api_query(ffi::_sai_api_t::SAI_API_QUEUE, &mut queue_api),
            )?;
            let mut hostif_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(HOSTIF)",
                api_query(ffi::_sai_api_t::SAI_API_HOSTIF, &mut hostif_api),
            )?;
            let mut rif_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(ROUTER_INTERFACE)",
                api_query(ffi::_sai_api_t::SAI_API_ROUTER_INTERFACE, &mut rif_api),
            )?;
            let mut route_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(ROUTE)",
                api_query(ffi::_sai_api_t::SAI_API_ROUTE, &mut route_api),
            )?;
            let mut bridge_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(BRIDGE)",
                api_query(ffi::_sai_api_t::SAI_API_BRIDGE, &mut bridge_api),
            )?;
            let mut vlan_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(VLAN)",
                api_query(ffi::_sai_api_t::SAI_API_VLAN, &mut vlan_api),
            )?;
            (
                switch_api as *mut ffi::sai_switch_api_t,
                port_api as *mut ffi::sai_port_api_t,
                queue_api as *mut ffi::sai_queue_api_t,
                hostif_api as *mut ffi::sai_hostif_api_t,
                rif_api as *mut ffi::sai_router_interface_api_t,
                route_api as *mut ffi::sai_route_api_t,
                bridge_api as *mut ffi::sai_bridge_api_t,
                vlan_api as *mut ffi::sai_vlan_api_t,
            )
        };
        let (switch_api, port_api, queue_api, hostif_api, rif_api, route_api, bridge_api, vlan_api) =
            apis;
        Ok(Self {
            library,
            _services: services,
            switch_api,
            port_api,
            queue_api,
            hostif_api,
            rif_api,
            route_api,
            bridge_api,
            vlan_api,
            switch_oid: None,
            defaults: None,
            events_rx: Some(rx),
            src_mac: init.src_mac,
            diag_shell: init.diag_shell,
            name: format!("vendor:{}", init.libsai_path.display()),
        })
    }

    fn switch_oid(&self) -> Result<ffi::sai_object_id_t, SaiError> {
        self.switch_oid.ok_or(SaiError::NoSwitch)
    }

    /// Bench diag shell. Broadcom's SAI runs its `BCM.0>` shell *inside*
    /// a blocking `set_switch_attribute(SWITCH_SHELL_ENABLE, true)` call
    /// (the attribute is not honored at create_switch) — the set only
    /// returns when the operator types `exit`. Mirror SONiC syncd: a
    /// dedicated thread re-invokes it in a loop so `exit` reopens the
    /// prompt; the vendor library's internal locking lets normal SAI
    /// calls proceed on the actor thread meanwhile.
    fn spawn_diag_shell(&self, switch: ffi::sai_object_id_t) -> Result<(), SaiError> {
        // SAFETY: valid switch api table from sai_api_query; fn pointers
        // are Copy + Send.
        let set = unsafe {
            (*self.switch_api)
                .set_switch_attribute
                .ok_or(SaiError::Other(
                    "switch api lacks set_switch_attribute".into(),
                ))?
        };
        std::thread::Builder::new()
            .name("sai-diag-shell".into())
            .spawn(move || {
                tracing::info!("vendor diag shell on this terminal (`exit` reopens it)");
                loop {
                    let mut attr = Self::zeroed_attr(
                        ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_SWITCH_SHELL_ENABLE,
                    );
                    attr.value.booldata = true;
                    // SAFETY: attr outlives the call; blocks this thread
                    // for the shell session's lifetime.
                    let status = unsafe { set(switch, &attr) };
                    if status != 0 {
                        tracing::warn!(
                            status,
                            "diag shell set_switch_attribute failed; shell unavailable"
                        );
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            })
            .map_err(|e| SaiError::Other(format!("spawning sai-diag-shell: {e}")))?;
        Ok(())
    }

    fn zeroed_attr(id: u32) -> ffi::sai_attribute_t {
        // SAFETY: sai_attribute_t is POD; an all-zero value is a valid
        // starting point before the union field is assigned.
        let mut attr: ffi::sai_attribute_t = unsafe { std::mem::zeroed() };
        attr.id = id;
        attr
    }

    /// Switch-scope objects needed by the L3 family, or a clear error
    /// when their resolution failed at create_switch.
    fn defaults(&self) -> Result<SwitchDefaults, SaiError> {
        self.switch_oid()?;
        self.defaults.ok_or(SaiError::Other(
            "switch L3 defaults unavailable (resolution failed at create_switch)".into(),
        ))
    }

    /// One switch attribute holding a single OID.
    fn switch_attr_oid(
        &self,
        call: &'static str,
        id: u32,
        switch: ffi::sai_object_id_t,
    ) -> Result<ffi::sai_object_id_t, SaiError> {
        // SAFETY: valid switch api table; attr outlives the call.
        let get = unsafe {
            (*self.switch_api)
                .get_switch_attribute
                .ok_or(SaiError::Other(
                    "switch api lacks get_switch_attribute".into(),
                ))?
        };
        let mut attr = Self::zeroed_attr(id);
        // SAFETY: single-attr get; union read matches an oid-valued attr.
        unsafe {
            check(call, get(switch, 1, &mut attr))?;
            Ok(attr.value.oid)
        }
    }

    /// Resolve the default virtual router / VLAN / 1Q bridge / CPU port /
    /// trap group after create_switch.
    fn resolve_defaults(&self, switch: ffi::sai_object_id_t) -> Result<SwitchDefaults, SaiError> {
        use ffi::_sai_switch_attr_t as sw;
        let virtual_router = self.switch_attr_oid(
            "get(DEFAULT_VIRTUAL_ROUTER_ID)",
            sw::SAI_SWITCH_ATTR_DEFAULT_VIRTUAL_ROUTER_ID,
            switch,
        )?;
        let vlan = self.switch_attr_oid(
            "get(DEFAULT_VLAN_ID)",
            sw::SAI_SWITCH_ATTR_DEFAULT_VLAN_ID,
            switch,
        )?;
        let bridge_1q = self.switch_attr_oid(
            "get(DEFAULT_1Q_BRIDGE_ID)",
            sw::SAI_SWITCH_ATTR_DEFAULT_1Q_BRIDGE_ID,
            switch,
        )?;
        let cpu_port =
            self.switch_attr_oid("get(CPU_PORT)", sw::SAI_SWITCH_ATTR_CPU_PORT, switch)?;
        let trap_group = self.switch_attr_oid(
            "get(DEFAULT_TRAP_GROUP)",
            sw::SAI_SWITCH_ATTR_DEFAULT_TRAP_GROUP,
            switch,
        )?;

        // The default VLAN's 802.1Q number, for restoring a port's PVID.
        let vlan_number = {
            // SAFETY: valid vlan api table; attr outlives the call.
            let get = unsafe {
                (*self.vlan_api)
                    .get_vlan_attribute
                    .ok_or(SaiError::Other("vlan api lacks get_vlan_attribute".into()))?
            };
            let mut attr = Self::zeroed_attr(ffi::_sai_vlan_attr_t::SAI_VLAN_ATTR_VLAN_ID);
            // SAFETY: single-attr get; union read matches a u16 attr.
            unsafe {
                check("get_vlan_attribute(VLAN_ID)", get(vlan, 1, &mut attr))?;
                attr.value.u16_
            }
        };

        Ok(SwitchDefaults {
            virtual_router,
            vlan,
            vlan_number,
            bridge_1q,
            cpu_port,
            trap_group,
        })
    }

    /// The 1Q bridge port fronting `port`, if it is currently bridged.
    fn find_bridge_port(&self, port: PortId) -> Result<Option<ffi::sai_object_id_t>, SaiError> {
        let defaults = self.defaults()?;
        // SAFETY per block below: valid api tables, buffers outlive calls.
        let get_bridge = unsafe {
            (*self.bridge_api)
                .get_bridge_attribute
                .ok_or(SaiError::Other(
                    "bridge api lacks get_bridge_attribute".into(),
                ))?
        };
        let get_bridge_port = unsafe {
            (*self.bridge_api)
                .get_bridge_port_attribute
                .ok_or(SaiError::Other(
                    "bridge api lacks get_bridge_port_attribute".into(),
                ))?
        };

        let mut members: Vec<ffi::sai_object_id_t> = vec![0; 256];
        {
            let mut attr = Self::zeroed_attr(ffi::_sai_bridge_attr_t::SAI_BRIDGE_ATTR_PORT_LIST);
            attr.value.objlist.count = members.len() as u32;
            attr.value.objlist.list = members.as_mut_ptr();
            // SAFETY: list buffer alive across the call.
            unsafe {
                check(
                    "get_bridge_attribute(PORT_LIST)",
                    get_bridge(defaults.bridge_1q, 1, &mut attr),
                )?;
                members.truncate(attr.value.objlist.count as usize);
            }
        }
        for bridge_port in members {
            let mut attr =
                Self::zeroed_attr(ffi::_sai_bridge_port_attr_t::SAI_BRIDGE_PORT_ATTR_PORT_ID);
            // SAFETY: single-attr get. Non-PORT bridge ports may reject
            // the read; those simply don't match.
            let matched = unsafe {
                get_bridge_port(bridge_port, 1, &mut attr) == 0 && attr.value.oid == port.0
            };
            if matched {
                return Ok(Some(bridge_port));
            }
        }
        Ok(None)
    }

    /// The default-VLAN member fronting `bridge_port`, if any.
    fn find_default_vlan_member(
        &self,
        bridge_port: ffi::sai_object_id_t,
    ) -> Result<Option<ffi::sai_object_id_t>, SaiError> {
        let defaults = self.defaults()?;
        // SAFETY per block below: valid api tables, buffers outlive calls.
        let get_vlan = unsafe {
            (*self.vlan_api)
                .get_vlan_attribute
                .ok_or(SaiError::Other("vlan api lacks get_vlan_attribute".into()))?
        };
        let get_member = unsafe {
            (*self.vlan_api)
                .get_vlan_member_attribute
                .ok_or(SaiError::Other(
                    "vlan api lacks get_vlan_member_attribute".into(),
                ))?
        };

        let mut members: Vec<ffi::sai_object_id_t> = vec![0; 256];
        {
            let mut attr = Self::zeroed_attr(ffi::_sai_vlan_attr_t::SAI_VLAN_ATTR_MEMBER_LIST);
            attr.value.objlist.count = members.len() as u32;
            attr.value.objlist.list = members.as_mut_ptr();
            // SAFETY: list buffer alive across the call.
            unsafe {
                check(
                    "get_vlan_attribute(MEMBER_LIST)",
                    get_vlan(defaults.vlan, 1, &mut attr),
                )?;
                members.truncate(attr.value.objlist.count as usize);
            }
        }
        for member in members {
            let mut attr = Self::zeroed_attr(
                ffi::_sai_vlan_member_attr_t::SAI_VLAN_MEMBER_ATTR_BRIDGE_PORT_ID,
            );
            // SAFETY: single-attr get.
            let matched =
                unsafe { get_member(member, 1, &mut attr) == 0 && attr.value.oid == bridge_port };
            if matched {
                return Ok(Some(member));
            }
        }
        Ok(None)
    }

    /// Encode a destination prefix; SAI wants address and mask in network
    /// byte order (the in-memory bytes are the address bytes in order).
    fn ip_prefix(dest: IpPrefix) -> ffi::sai_ip_prefix_t {
        // SAFETY: sai_ip_prefix_t is POD; all-zero is a valid start.
        let mut prefix: ffi::sai_ip_prefix_t = unsafe { std::mem::zeroed() };
        match dest.0 {
            std::net::IpAddr::V4(v4) => {
                prefix.addr_family = ffi::_sai_ip_addr_family_t::SAI_IP_ADDR_FAMILY_IPV4;
                prefix.addr.ip4 = u32::from_ne_bytes(v4.octets());
                let mask = if dest.1 == 0 {
                    0u32
                } else {
                    u32::MAX << (32 - u32::from(dest.1))
                };
                prefix.mask.ip4 = u32::from_ne_bytes(mask.to_be_bytes());
            }
            std::net::IpAddr::V6(v6) => {
                prefix.addr_family = ffi::_sai_ip_addr_family_t::SAI_IP_ADDR_FAMILY_IPV6;
                prefix.addr.ip6 = v6.octets();
                let mask = if dest.1 == 0 {
                    0u128
                } else {
                    u128::MAX << (128 - u32::from(dest.1))
                };
                prefix.mask.ip6 = mask.to_be_bytes();
            }
        }
        prefix
    }

    fn route_entry(&self, dest: IpPrefix) -> Result<ffi::sai_route_entry_t, SaiError> {
        Ok(ffi::sai_route_entry_t {
            switch_id: self.switch_oid()?,
            vr_id: self.defaults()?.virtual_router,
            destination: Self::ip_prefix(dest),
        })
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

        // L3 needs the default VR/VLAN/bridge/CPU-port/trap-group; a
        // vendor that can't report them keeps L2 working and fails L3
        // calls with a clear error instead of failing bring-up.
        match self.resolve_defaults(oid) {
            Ok(defaults) => self.defaults = Some(defaults),
            Err(err) => tracing::warn!(%err, "switch L3 defaults unresolved; L3 unavailable"),
        }

        if self.diag_shell {
            self.spawn_diag_shell(oid)?;
        }
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

    fn port_counters(&mut self, port: PortId) -> Result<PortCounters, SaiError> {
        self.switch_oid()?;
        use ffi::_sai_port_stat_t as stat;

        // One batched sai_get_port_stats call; `IDS` order defines the
        // meaning of each slot in `values`. EOS-vs-SAI mapping notes:
        // - runts  <- ETHER_STATS_UNDERSIZE_PKTS
        // - giants <- ETHER_RX_OVERSIZE_PKTS
        // - the EOS 1024-1522 bin maps to SAI's 1024-1518 counter, and
        //   1523-max aggregates the four SAI >=1519 counters (the ASIC
        //   bins on 1518, EOS displays 1522; the 1519-1522 sliver lands
        //   in 1523-max).
        const IDS: [ffi::_sai_port_stat_t::Type; 42] = [
            stat::SAI_PORT_STAT_IF_IN_OCTETS,
            stat::SAI_PORT_STAT_IF_IN_UCAST_PKTS,
            stat::SAI_PORT_STAT_IF_IN_MULTICAST_PKTS,
            stat::SAI_PORT_STAT_IF_IN_BROADCAST_PKTS,
            stat::SAI_PORT_STAT_IF_IN_DISCARDS,
            stat::SAI_PORT_STAT_IF_IN_ERRORS,
            stat::SAI_PORT_STAT_DOT3_STATS_FCS_ERRORS,
            stat::SAI_PORT_STAT_DOT3_STATS_ALIGNMENT_ERRORS,
            stat::SAI_PORT_STAT_DOT3_STATS_SYMBOL_ERRORS,
            stat::SAI_PORT_STAT_ETHER_STATS_UNDERSIZE_PKTS,
            stat::SAI_PORT_STAT_ETHER_RX_OVERSIZE_PKTS,
            stat::SAI_PORT_STAT_PAUSE_RX_PKTS,
            stat::SAI_PORT_STAT_IF_OUT_OCTETS,
            stat::SAI_PORT_STAT_IF_OUT_UCAST_PKTS,
            stat::SAI_PORT_STAT_IF_OUT_MULTICAST_PKTS,
            stat::SAI_PORT_STAT_IF_OUT_BROADCAST_PKTS,
            stat::SAI_PORT_STAT_IF_OUT_DISCARDS,
            stat::SAI_PORT_STAT_IF_OUT_ERRORS,
            stat::SAI_PORT_STAT_PAUSE_TX_PKTS,
            stat::SAI_PORT_STAT_ETHER_STATS_COLLISIONS,
            stat::SAI_PORT_STAT_DOT3_STATS_LATE_COLLISIONS,
            stat::SAI_PORT_STAT_DOT3_STATS_DEFERRED_TRANSMISSIONS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_64_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_65_TO_127_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_128_TO_255_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_256_TO_511_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_512_TO_1023_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_1024_TO_1518_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_1519_TO_2047_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_2048_TO_4095_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_4096_TO_9216_OCTETS,
            stat::SAI_PORT_STAT_ETHER_IN_PKTS_9217_TO_16383_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_64_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_65_TO_127_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_128_TO_255_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_256_TO_511_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_512_TO_1023_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_1024_TO_1518_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_1519_TO_2047_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_2048_TO_4095_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_4096_TO_9216_OCTETS,
            stat::SAI_PORT_STAT_ETHER_OUT_PKTS_9217_TO_16383_OCTETS,
        ];

        // SAFETY: valid port api table from sai_api_query.
        let get_stats = unsafe {
            (*self.port_api)
                .get_port_stats
                .ok_or(SaiError::Other("port api lacks get_port_stats".into()))?
        };
        let ids: Vec<ffi::sai_stat_id_t> = IDS.iter().map(|i| *i as ffi::sai_stat_id_t).collect();
        let mut values = [0u64; IDS.len()];
        // SAFETY: id and value buffers are sized identically and outlive
        // the call.
        let batch =
            unsafe { get_stats(port.0, ids.len() as u32, ids.as_ptr(), values.as_mut_ptr()) };
        if batch != 0 {
            // Some SAI builds reject a batch containing any unsupported
            // counter. Degrade to per-id reads: unsupported counters
            // honestly stay 0.
            tracing::debug!(
                port = %port,
                status = batch,
                "batched get_port_stats failed; falling back to per-id reads"
            );
            for (id, value) in ids.iter().zip(values.iter_mut()) {
                let mut one = 0u64;
                // SAFETY: single-id read into a local.
                let status = unsafe { get_stats(port.0, 1, id, &mut one) };
                *value = if status == 0 { one } else { 0 };
            }
        }

        let rx_1523_max = values[28] + values[29] + values[30] + values[31];
        let tx_1523_max = values[38] + values[39] + values[40] + values[41];
        Ok(PortCounters {
            in_octets: values[0],
            in_ucast_pkts: values[1],
            in_mcast_pkts: values[2],
            in_bcast_pkts: values[3],
            in_discards: values[4],
            in_errors: values[5],
            in_crc_errors: values[6],
            in_alignment_errors: values[7],
            in_symbol_errors: values[8],
            in_runts: values[9],
            in_giants: values[10],
            in_pause: values[11],
            out_octets: values[12],
            out_ucast_pkts: values[13],
            out_mcast_pkts: values[14],
            out_bcast_pkts: values[15],
            out_discards: values[16],
            out_errors: values[17],
            out_pause: values[18],
            collisions: values[19],
            late_collisions: values[20],
            deferred: values[21],
            rx_bins: [
                values[22],
                values[23],
                values[24],
                values[25],
                values[26],
                values[27],
                rx_1523_max,
            ],
            tx_bins: [
                values[32],
                values[33],
                values[34],
                values[35],
                values[36],
                values[37],
                tx_1523_max,
            ],
        })
    }

    fn port_queue_counters(&mut self, port: PortId) -> Result<Vec<QueueCounters>, SaiError> {
        self.switch_oid()?;

        // SAFETY per block below: valid api tables, buffers outlive calls.
        let get_port_attr = unsafe {
            (*self.port_api)
                .get_port_attribute
                .ok_or(SaiError::Other("port api lacks get_port_attribute".into()))?
        };
        let get_queue_attr = unsafe {
            (*self.queue_api)
                .get_queue_attribute
                .ok_or(SaiError::Other(
                    "queue api lacks get_queue_attribute".into(),
                ))?
        };
        let get_queue_stats = unsafe {
            (*self.queue_api)
                .get_queue_stats
                .ok_or(SaiError::Other("queue api lacks get_queue_stats".into()))?
        };

        let count = {
            let mut attr =
                Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_NUMBER_OF_QUEUES);
            // SAFETY: single-attr get.
            unsafe {
                check(
                    "get(QOS_NUMBER_OF_QUEUES)",
                    get_port_attr(port.0, 1, &mut attr),
                )?;
                attr.value.u32_
            }
        };
        let mut queue_oids: Vec<ffi::sai_object_id_t> = vec![0; count as usize];
        {
            let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_QUEUE_LIST);
            attr.value.objlist.count = count;
            attr.value.objlist.list = queue_oids.as_mut_ptr();
            // SAFETY: list buffer sized to `count`, alive across the call.
            unsafe {
                check("get(QOS_QUEUE_LIST)", get_port_attr(port.0, 1, &mut attr))?;
                queue_oids.truncate(attr.value.objlist.count as usize);
            }
        }

        const STAT_IDS: [ffi::sai_stat_id_t; 4] = [
            ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_PACKETS as ffi::sai_stat_id_t,
            ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_BYTES as ffi::sai_stat_id_t,
            ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_DROPPED_PACKETS as ffi::sai_stat_id_t,
            ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_DROPPED_BYTES as ffi::sai_stat_id_t,
        ];

        let mut queues = Vec::with_capacity(queue_oids.len());
        for oid in queue_oids {
            let type_attr = Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_TYPE);
            let index_attr = Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_INDEX);
            let mut attrs = [type_attr, index_attr];
            let mut stats = [0u64; STAT_IDS.len()];
            // SAFETY: attr array + stat buffers valid across the calls;
            // union reads match the attr ids just fetched.
            unsafe {
                check(
                    "get_queue_attribute",
                    get_queue_attr(oid, attrs.len() as u32, attrs.as_mut_ptr()),
                )?;
                check(
                    "get_queue_stats",
                    get_queue_stats(
                        oid,
                        STAT_IDS.len() as u32,
                        STAT_IDS.as_ptr(),
                        stats.as_mut_ptr(),
                    ),
                )?;
                queues.push(QueueCounters {
                    unicast: attrs[0].value.s32
                        != ffi::_sai_queue_type_t::SAI_QUEUE_TYPE_MULTICAST as i32,
                    index: u32::from(attrs[1].value.u8_),
                    pkts: stats[0],
                    bytes: stats[1],
                    dropped_pkts: stats[2],
                    dropped_bytes: stats[3],
                });
            }
        }
        Ok(queues)
    }

    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>> {
        self.events_rx.take()
    }

    fn setup_host_punt(&mut self) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        let defaults = self.defaults()?;

        // SAFETY per block below: valid hostif api table from
        // sai_api_query; attr arrays outlive the calls.
        let create_trap = unsafe {
            (*self.hostif_api)
                .create_hostif_trap
                .ok_or(SaiError::Other(
                    "hostif api lacks create_hostif_trap".into(),
                ))?
        };
        use ffi::_sai_hostif_trap_attr_t as trap_attr;
        use ffi::_sai_hostif_trap_type_t as trap_type;
        use ffi::_sai_packet_action_t as action;
        // ARP is copied (L2 flooding keeps working on switched ports);
        // traffic to the switch's own addresses is trapped outright.
        let traps: [(&'static str, u32, u32); 3] = [
            (
                "create_hostif_trap(ARP_REQUEST)",
                trap_type::SAI_HOSTIF_TRAP_TYPE_ARP_REQUEST,
                action::SAI_PACKET_ACTION_COPY,
            ),
            (
                "create_hostif_trap(ARP_RESPONSE)",
                trap_type::SAI_HOSTIF_TRAP_TYPE_ARP_RESPONSE,
                action::SAI_PACKET_ACTION_COPY,
            ),
            (
                "create_hostif_trap(IP2ME)",
                trap_type::SAI_HOSTIF_TRAP_TYPE_IP2ME,
                action::SAI_PACKET_ACTION_TRAP,
            ),
        ];
        for (call, trap, packet_action) in traps {
            let mut type_attr = Self::zeroed_attr(trap_attr::SAI_HOSTIF_TRAP_ATTR_TRAP_TYPE);
            type_attr.value.s32 = trap as i32;
            let mut action_attr = Self::zeroed_attr(trap_attr::SAI_HOSTIF_TRAP_ATTR_PACKET_ACTION);
            action_attr.value.s32 = packet_action as i32;
            let mut group_attr = Self::zeroed_attr(trap_attr::SAI_HOSTIF_TRAP_ATTR_TRAP_GROUP);
            group_attr.value.oid = defaults.trap_group;
            let attrs = [type_attr, action_attr, group_attr];
            let mut oid: ffi::sai_object_id_t = 0;
            // SAFETY: attr array outlives the call.
            unsafe {
                check(
                    call,
                    create_trap(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
                )?;
            }
        }

        // Wildcard table entry: every trapped packet is delivered on the
        // netdev of its ingress physical port.
        let create_entry = unsafe {
            (*self.hostif_api)
                .create_hostif_table_entry
                .ok_or(SaiError::Other(
                    "hostif api lacks create_hostif_table_entry".into(),
                ))?
        };
        use ffi::_sai_hostif_table_entry_attr_t as entry_attr;
        let mut type_attr = Self::zeroed_attr(entry_attr::SAI_HOSTIF_TABLE_ENTRY_ATTR_TYPE);
        type_attr.value.s32 =
            ffi::_sai_hostif_table_entry_type_t::SAI_HOSTIF_TABLE_ENTRY_TYPE_WILDCARD as i32;
        let mut channel_attr =
            Self::zeroed_attr(entry_attr::SAI_HOSTIF_TABLE_ENTRY_ATTR_CHANNEL_TYPE);
        channel_attr.value.s32 =
            ffi::_sai_hostif_table_entry_channel_type_t::SAI_HOSTIF_TABLE_ENTRY_CHANNEL_TYPE_NETDEV_PHYSICAL_PORT as i32;
        let attrs = [type_attr, channel_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_hostif_table_entry(WILDCARD)",
                create_entry(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        tracing::info!("CPU punt path installed (ARP copy, IP2ME trap, netdev delivery)");
        Ok(())
    }

    fn create_hostif(&mut self, port: PortId, name: &str) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid hostif api table; attr array outlives the call.
        let create = unsafe {
            (*self.hostif_api)
                .create_hostif
                .ok_or(SaiError::Other("hostif api lacks create_hostif".into()))?
        };
        use ffi::_sai_hostif_attr_t as attr;
        let mut type_attr = Self::zeroed_attr(attr::SAI_HOSTIF_ATTR_TYPE);
        type_attr.value.s32 = ffi::_sai_hostif_type_t::SAI_HOSTIF_TYPE_NETDEV as i32;
        let mut obj_attr = Self::zeroed_attr(attr::SAI_HOSTIF_ATTR_OBJ_ID);
        obj_attr.value.oid = port.0;
        let mut name_attr = Self::zeroed_attr(attr::SAI_HOSTIF_ATTR_NAME);
        // SAI_HOSTIF_NAME_SIZE is 16 including the NUL; zeroed_attr left
        // the tail NUL-filled.
        // SAFETY: chardata is the union member NAME reads; the attr was
        // zero-initialized so every byte is initialized.
        unsafe {
            for (dst, src) in name_attr
                .value
                .chardata
                .iter_mut()
                .zip(name.bytes().take(15))
            {
                *dst = src as c_char;
            }
        }
        let mut vlan_attr = Self::zeroed_attr(attr::SAI_HOSTIF_ATTR_VLAN_TAG);
        vlan_attr.value.s32 = ffi::_sai_hostif_vlan_tag_t::SAI_HOSTIF_VLAN_TAG_STRIP as i32;

        let attrs = [type_attr, obj_attr, name_attr, vlan_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_hostif(NETDEV)",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn create_router_interface(&mut self, port: PortId) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let defaults = self.defaults()?;

        // A port RIF needs the port out of the 802.1Q bridge first:
        // default-VLAN membership, then the bridge port itself.
        if let Some(bridge_port) = self.find_bridge_port(port)? {
            if let Some(member) = self.find_default_vlan_member(bridge_port)? {
                // SAFETY: valid vlan api table.
                unsafe {
                    let remove = (*self.vlan_api)
                        .remove_vlan_member
                        .ok_or(SaiError::Other("vlan api lacks remove_vlan_member".into()))?;
                    check("remove_vlan_member(default)", remove(member))?;
                }
            }
            // SAFETY: valid bridge api table.
            unsafe {
                let remove = (*self.bridge_api)
                    .remove_bridge_port
                    .ok_or(SaiError::Other(
                        "bridge api lacks remove_bridge_port".into(),
                    ))?;
                check("remove_bridge_port", remove(bridge_port))?;
            }
        }

        // SAFETY: valid rif api table; attr array outlives the call.
        let create = unsafe {
            (*self.rif_api)
                .create_router_interface
                .ok_or(SaiError::Other(
                    "router interface api lacks create_router_interface".into(),
                ))?
        };
        use ffi::_sai_router_interface_attr_t as attr;
        let mut vr_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_VIRTUAL_ROUTER_ID);
        vr_attr.value.oid = defaults.virtual_router;
        let mut type_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_TYPE);
        type_attr.value.s32 =
            ffi::_sai_router_interface_type_t::SAI_ROUTER_INTERFACE_TYPE_PORT as i32;
        let mut port_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_PORT_ID);
        port_attr.value.oid = port.0;
        let mut mtu_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_MTU);
        mtu_attr.value.u32_ = 9214;

        let mut attrs = vec![vr_attr, type_attr, port_attr, mtu_attr];
        if let Some(mac) = self.src_mac {
            let mut mac_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_SRC_MAC_ADDRESS);
            mac_attr.value.mac = mac;
            attrs.push(mac_attr);
        }
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_router_interface(PORT)",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_router_interface(&mut self, port: PortId, rif: Oid) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        let defaults = self.defaults()?;

        // SAFETY: valid rif api table.
        unsafe {
            let remove = (*self.rif_api)
                .remove_router_interface
                .ok_or(SaiError::Other(
                    "router interface api lacks remove_router_interface".into(),
                ))?;
            check("remove_router_interface", remove(rif.0))?;
        }

        // Restore default L2 bridging: bridge port, untagged default-VLAN
        // membership, PVID.
        let bridge_port = {
            // SAFETY: valid bridge api table; attr array outlives the call.
            let create = unsafe {
                (*self.bridge_api)
                    .create_bridge_port
                    .ok_or(SaiError::Other(
                        "bridge api lacks create_bridge_port".into(),
                    ))?
            };
            use ffi::_sai_bridge_port_attr_t as attr;
            let mut type_attr = Self::zeroed_attr(attr::SAI_BRIDGE_PORT_ATTR_TYPE);
            type_attr.value.s32 = ffi::_sai_bridge_port_type_t::SAI_BRIDGE_PORT_TYPE_PORT as i32;
            let mut port_attr = Self::zeroed_attr(attr::SAI_BRIDGE_PORT_ATTR_PORT_ID);
            port_attr.value.oid = port.0;
            let mut admin_attr = Self::zeroed_attr(attr::SAI_BRIDGE_PORT_ATTR_ADMIN_STATE);
            admin_attr.value.booldata = true;
            let attrs = [type_attr, port_attr, admin_attr];
            let mut oid: ffi::sai_object_id_t = 0;
            // SAFETY: attr array outlives the call.
            unsafe {
                check(
                    "create_bridge_port(PORT)",
                    create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
                )?;
            }
            oid
        };
        {
            // SAFETY: valid vlan api table; attr array outlives the call.
            let create = unsafe {
                (*self.vlan_api)
                    .create_vlan_member
                    .ok_or(SaiError::Other("vlan api lacks create_vlan_member".into()))?
            };
            use ffi::_sai_vlan_member_attr_t as attr;
            let mut vlan_attr = Self::zeroed_attr(attr::SAI_VLAN_MEMBER_ATTR_VLAN_ID);
            vlan_attr.value.oid = defaults.vlan;
            let mut bp_attr = Self::zeroed_attr(attr::SAI_VLAN_MEMBER_ATTR_BRIDGE_PORT_ID);
            bp_attr.value.oid = bridge_port;
            let mut mode_attr = Self::zeroed_attr(attr::SAI_VLAN_MEMBER_ATTR_VLAN_TAGGING_MODE);
            mode_attr.value.s32 =
                ffi::_sai_vlan_tagging_mode_t::SAI_VLAN_TAGGING_MODE_UNTAGGED as i32;
            let attrs = [vlan_attr, bp_attr, mode_attr];
            let mut oid: ffi::sai_object_id_t = 0;
            // SAFETY: attr array outlives the call.
            unsafe {
                check(
                    "create_vlan_member(default)",
                    create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
                )?;
            }
        }
        {
            let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_PORT_VLAN_ID);
            attr.value.u16_ = defaults.vlan_number;
            // SAFETY: valid port api table; attr outlives the call.
            unsafe {
                let set = (*self.port_api)
                    .set_port_attribute
                    .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
                check("set_port_attribute(PORT_VLAN_ID)", set(port.0, &attr))?;
            }
        }
        Ok(())
    }

    fn create_route(&mut self, dest: IpPrefix, target: RouteTarget) -> Result<(), SaiError> {
        let defaults = self.defaults()?;
        let entry = self.route_entry(dest)?;
        // SAFETY: valid route api table; entry + attr outlive the call.
        let create = unsafe {
            (*self.route_api)
                .create_route_entry
                .ok_or(SaiError::Other("route api lacks create_route_entry".into()))?
        };
        let mut nh_attr =
            Self::zeroed_attr(ffi::_sai_route_entry_attr_t::SAI_ROUTE_ENTRY_ATTR_NEXT_HOP_ID);
        nh_attr.value.oid = match target {
            RouteTarget::Cpu => defaults.cpu_port,
            RouteTarget::Rif(rif) => rif.0,
        };
        // SAFETY: entry and attr outlive the call.
        unsafe { check("create_route_entry", create(&entry, 1, &nh_attr)) }
    }

    fn remove_route(&mut self, dest: IpPrefix) -> Result<(), SaiError> {
        let entry = self.route_entry(dest)?;
        // SAFETY: valid route api table; entry outlives the call.
        unsafe {
            let remove = (*self.route_api)
                .remove_route_entry
                .ok_or(SaiError::Other("route api lacks remove_route_entry".into()))?;
            check("remove_route_entry", remove(&entry))
        }
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
