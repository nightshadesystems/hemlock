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

use std::collections::HashMap;

use crate::{
    ffi, AclAction, AclFamily, AclFields, AclPacketAction, AclStage, FdbAction, FdbEventKind,
    IpPrefix, Oid, PolicerSpec, PolicerStats, PortCounters, PortId, QosMapType, QueueCounters,
    RouteTarget, SaiBackend, SaiCapabilities, SaiError, SaiEvent, SaiPort, SchedulerSpec,
    StormClass, StpPortState, SwitchInfo, SwitchInit, TrapKind, WredSpec,
};

/// SAI profile key/value store handed to the vendor library. Static because
/// the profile callbacks are plain C function pointers with no user data.
static PROFILE: OnceLock<Vec<(CString, CString)>> = OnceLock::new();
static PROFILE_ITER: AtomicUsize = AtomicUsize::new(0);

/// Destination for vendor notification callbacks (same constraint).
static EVENT_TX: OnceLock<mpsc::UnboundedSender<SaiEvent>> = OnceLock::new();

/// Bridge-port -> port index for the FDB event callback: notifications
/// carry bridge-port OIDs, and API calls are off-limits inside vendor
/// callbacks, so the mapping is maintained here (populated at
/// create_switch, updated when RIF churn recreates bridge ports).
static BRIDGE_PORTS: OnceLock<std::sync::RwLock<HashMap<u64, u64>>> = OnceLock::new();

fn bridge_ports() -> &'static std::sync::RwLock<HashMap<u64, u64>> {
    BRIDGE_PORTS.get_or_init(Default::default)
}

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

unsafe extern "C" fn on_fdb_event(count: u32, data: *const ffi::sai_fdb_event_notification_data_t) {
    let Some(tx) = EVENT_TX.get() else { return };
    if data.is_null() {
        return;
    }
    let items = unsafe { std::slice::from_raw_parts(data, count as usize) };
    for item in items {
        let kind = match item.event_type {
            ffi::_sai_fdb_event_t::SAI_FDB_EVENT_LEARNED => FdbEventKind::Learned,
            ffi::_sai_fdb_event_t::SAI_FDB_EVENT_AGED => FdbEventKind::Aged,
            ffi::_sai_fdb_event_t::SAI_FDB_EVENT_MOVE => FdbEventKind::Moved,
            ffi::_sai_fdb_event_t::SAI_FDB_EVENT_FLUSHED => FdbEventKind::Flushed,
            _ => continue,
        };
        // The entry's bridge port rides in the attr list; map it back to
        // its port through the maintained index.
        let mut port = None;
        if !item.attr.is_null() {
            let attrs = unsafe { std::slice::from_raw_parts(item.attr, item.attr_count as usize) };
            for attr in attrs {
                if attr.id == ffi::_sai_fdb_entry_attr_t::SAI_FDB_ENTRY_ATTR_BRIDGE_PORT_ID {
                    // SAFETY: union read matches the attr id.
                    let bridge_port = unsafe { attr.value.oid };
                    port = bridge_ports()
                        .read()
                        .ok()
                        .and_then(|map| map.get(&bridge_port).copied())
                        .map(PortId);
                }
            }
        }
        let _ = tx.send(SaiEvent::Fdb {
            kind,
            bv_id: item.fdb_entry.bv_id,
            mac: item.fdb_entry.mac_address,
            port,
        });
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
    /// The always-present default STP instance (0 when the switch
    /// cannot report one).
    stp: ffi::sai_object_id_t,
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
    fdb_api: *mut ffi::sai_fdb_api_t,
    policer_api: *mut ffi::sai_policer_api_t,
    mirror_api: *mut ffi::sai_mirror_api_t,
    lag_api: *mut ffi::sai_lag_api_t,
    stp_api: *mut ffi::sai_stp_api_t,
    l2mc_api: *mut ffi::sai_l2mc_api_t,
    l2mc_group_api: *mut ffi::sai_l2mc_group_api_t,
    neighbor_api: *mut ffi::sai_neighbor_api_t,
    next_hop_api: *mut ffi::sai_next_hop_api_t,
    next_hop_group_api: *mut ffi::sai_next_hop_group_api_t,
    my_mac_api: *mut ffi::sai_my_mac_api_t,
    /// Soft-probed like MY_MAC: a vendor blob refusing the ACL api
    /// leaves this null and the ACL capabilities off.
    acl_api: *mut ffi::sai_acl_api_t,
    /// QoS suite api tables, soft-probed the same way.
    qos_map_api: *mut ffi::sai_qos_map_api_t,
    scheduler_api: *mut ffi::sai_scheduler_api_t,
    wred_api: *mut ffi::sai_wred_api_t,
    /// `sai_query_attribute_capability`, when the library exports it
    /// (optional in the SAI spec; absent = assume supported and let the
    /// call fail with a real status).
    query_capability: Option<
        unsafe extern "C" fn(
            ffi::sai_object_id_t,
            ffi::sai_object_type_t,
            ffi::sai_attr_id_t,
            *mut ffi::sai_attr_capability_t,
        ) -> ffi::sai_status_t,
    >,
    /// Storm-control policers this backend created, per (port, class).
    storm_policers: HashMap<(u64, StormClass), ffi::sai_object_id_t>,
    /// QoS map objects this backend created, by type: SAI's map type is
    /// create-only, and a later value-list rewrite needs it to lay the
    /// entries out.
    qos_map_kinds: HashMap<u64, QosMapType>,
    /// Port-level shaper profiles this backend created, per port: the
    /// port shaper has no object of its own in SAI, it is a scheduler
    /// profile hung on the port.
    port_shaper_profiles: HashMap<u64, ffi::sai_object_id_t>,
    /// ACL tables this backend created, and each entry's ACL-range
    /// objects (L4 port ranges live in their own SAI objects; they are
    /// removed with the entry).
    acl_tables: HashMap<u64, AclStage>,
    acl_entry_ranges: HashMap<u64, Vec<ffi::sai_object_id_t>>,
    /// Hostif user-defined traps ([`TrapKind::AclLog`]) — removed
    /// through their own api call, unlike protocol traps.
    user_traps: std::collections::HashSet<u64>,
    /// LAG object ids this backend created (PVID dispatches to the LAG
    /// attribute for these).
    lags: std::collections::HashSet<u64>,
    /// stp-port objects this backend created, per (stp, port).
    stp_ports: HashMap<(u64, u64), ffi::sai_object_id_t>,
    switch_oid: Option<ffi::sai_object_id_t>,
    /// The first port the switch created, for capability probes that
    /// need a live port object (queue stat support).
    first_port: Option<ffi::sai_object_id_t>,
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
            let mut fdb_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(FDB)",
                api_query(ffi::_sai_api_t::SAI_API_FDB, &mut fdb_api),
            )?;
            let mut policer_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(POLICER)",
                api_query(ffi::_sai_api_t::SAI_API_POLICER, &mut policer_api),
            )?;
            let mut mirror_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(MIRROR)",
                api_query(ffi::_sai_api_t::SAI_API_MIRROR, &mut mirror_api),
            )?;
            let mut lag_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(LAG)",
                api_query(ffi::_sai_api_t::SAI_API_LAG, &mut lag_api),
            )?;
            let mut stp_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(STP)",
                api_query(ffi::_sai_api_t::SAI_API_STP, &mut stp_api),
            )?;
            let mut l2mc_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(L2MC)",
                api_query(ffi::_sai_api_t::SAI_API_L2MC, &mut l2mc_api),
            )?;
            let mut l2mc_group_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(L2MC_GROUP)",
                api_query(ffi::_sai_api_t::SAI_API_L2MC_GROUP, &mut l2mc_group_api),
            )?;
            let mut neighbor_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(NEIGHBOR)",
                api_query(ffi::_sai_api_t::SAI_API_NEIGHBOR, &mut neighbor_api),
            )?;
            let mut next_hop_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(NEXT_HOP)",
                api_query(ffi::_sai_api_t::SAI_API_NEXT_HOP, &mut next_hop_api),
            )?;
            let mut next_hop_group_api: *mut c_void = std::ptr::null_mut();
            check(
                "sai_api_query(NEXT_HOP_GROUP)",
                api_query(
                    ffi::_sai_api_t::SAI_API_NEXT_HOP_GROUP,
                    &mut next_hop_group_api,
                ),
            )?;
            // MY_MAC is late-spec (v1.9); a vendor library may not serve
            // the table at all — probe softly, a null table = capability
            // off rather than a failed boot.
            let mut my_mac_api: *mut c_void = std::ptr::null_mut();
            if api_query(ffi::_sai_api_t::SAI_API_MY_MAC, &mut my_mac_api) != 0 {
                my_mac_api = std::ptr::null_mut();
            }
            // ACL likewise: a refused table turns the ACL capabilities
            // off instead of failing boot.
            let mut acl_api: *mut c_void = std::ptr::null_mut();
            if api_query(ffi::_sai_api_t::SAI_API_ACL, &mut acl_api) != 0 {
                acl_api = std::ptr::null_mut();
            }
            // The QoS families are optional too: an absent table turns
            // the matching capabilities off rather than failing boot.
            let mut qos_map_api: *mut c_void = std::ptr::null_mut();
            if api_query(ffi::_sai_api_t::SAI_API_QOS_MAP, &mut qos_map_api) != 0 {
                qos_map_api = std::ptr::null_mut();
            }
            let mut scheduler_api: *mut c_void = std::ptr::null_mut();
            if api_query(ffi::_sai_api_t::SAI_API_SCHEDULER, &mut scheduler_api) != 0 {
                scheduler_api = std::ptr::null_mut();
            }
            let mut wred_api: *mut c_void = std::ptr::null_mut();
            if api_query(ffi::_sai_api_t::SAI_API_WRED, &mut wred_api) != 0 {
                wred_api = std::ptr::null_mut();
            }
            (
                switch_api as *mut ffi::sai_switch_api_t,
                port_api as *mut ffi::sai_port_api_t,
                queue_api as *mut ffi::sai_queue_api_t,
                hostif_api as *mut ffi::sai_hostif_api_t,
                rif_api as *mut ffi::sai_router_interface_api_t,
                route_api as *mut ffi::sai_route_api_t,
                bridge_api as *mut ffi::sai_bridge_api_t,
                vlan_api as *mut ffi::sai_vlan_api_t,
                fdb_api as *mut ffi::sai_fdb_api_t,
                policer_api as *mut ffi::sai_policer_api_t,
                mirror_api as *mut ffi::sai_mirror_api_t,
                lag_api as *mut ffi::sai_lag_api_t,
                stp_api as *mut ffi::sai_stp_api_t,
                l2mc_api as *mut ffi::sai_l2mc_api_t,
                l2mc_group_api as *mut ffi::sai_l2mc_group_api_t,
                neighbor_api as *mut ffi::sai_neighbor_api_t,
                next_hop_api as *mut ffi::sai_next_hop_api_t,
                next_hop_group_api as *mut ffi::sai_next_hop_group_api_t,
                my_mac_api as *mut ffi::sai_my_mac_api_t,
                acl_api as *mut ffi::sai_acl_api_t,
                qos_map_api as *mut ffi::sai_qos_map_api_t,
                scheduler_api as *mut ffi::sai_scheduler_api_t,
                wred_api as *mut ffi::sai_wred_api_t,
            )
        };
        let (
            switch_api,
            port_api,
            queue_api,
            hostif_api,
            rif_api,
            route_api,
            bridge_api,
            vlan_api,
            fdb_api,
            policer_api,
            mirror_api,
            lag_api,
            stp_api,
            l2mc_api,
            l2mc_group_api,
            neighbor_api,
            next_hop_api,
            next_hop_group_api,
            my_mac_api,
            acl_api,
            qos_map_api,
            scheduler_api,
            wred_api,
        ) = apis;
        // SAFETY: symbol lookup only; the fn pointer is copied out and
        // the library stays mapped for our whole lifetime.
        let query_capability = unsafe {
            library
                .get::<unsafe extern "C" fn(
                    ffi::sai_object_id_t,
                    ffi::sai_object_type_t,
                    ffi::sai_attr_id_t,
                    *mut ffi::sai_attr_capability_t,
                ) -> ffi::sai_status_t>(b"sai_query_attribute_capability\0")
                .ok()
                .map(|symbol| *symbol)
        };
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
            fdb_api,
            policer_api,
            mirror_api,
            lag_api,
            stp_api,
            l2mc_api,
            l2mc_group_api,
            neighbor_api,
            next_hop_api,
            next_hop_group_api,
            my_mac_api,
            acl_api,
            qos_map_api,
            scheduler_api,
            wred_api,
            query_capability,
            storm_policers: HashMap::new(),
            qos_map_kinds: HashMap::new(),
            port_shaper_profiles: HashMap::new(),
            acl_tables: HashMap::new(),
            acl_entry_ranges: HashMap::new(),
            user_traps: std::collections::HashSet::new(),
            stp_ports: HashMap::new(),
            lags: std::collections::HashSet::new(),
            switch_oid: None,
            first_port: None,
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

    /// One `set_port_attribute` call with the switch-created guard and a
    /// named error, the shape every scalar port attribute wants.
    fn set_one_port_attr(
        &mut self,
        what: &'static str,
        port: PortId,
        attr: &ffi::sai_attribute_t,
    ) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check(what, set(port.0, attr))
        }
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
        // Optional: platforms without STP support report no default
        // instance; STP calls then fail with a clear error.
        let stp = self
            .switch_attr_oid(
                "get(DEFAULT_STP_INST_ID)",
                sw::SAI_SWITCH_ATTR_DEFAULT_STP_INST_ID,
                switch,
            )
            .unwrap_or(0);

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
            stp,
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

    /// Encode a host address (neighbor / next-hop targets).
    fn ip_address(ip: std::net::IpAddr) -> ffi::sai_ip_address_t {
        // SAFETY: sai_ip_address_t is POD; all-zero is a valid start.
        let mut address: ffi::sai_ip_address_t = unsafe { std::mem::zeroed() };
        match ip {
            std::net::IpAddr::V4(v4) => {
                address.addr_family = ffi::_sai_ip_addr_family_t::SAI_IP_ADDR_FAMILY_IPV4;
                address.addr.ip4 = u32::from_ne_bytes(v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                address.addr_family = ffi::_sai_ip_addr_family_t::SAI_IP_ADDR_FAMILY_IPV6;
                address.addr.ip6 = v6.octets();
            }
        }
        address
    }

    fn neighbor_entry(
        &self,
        rif: Oid,
        ip: std::net::IpAddr,
    ) -> Result<ffi::sai_neighbor_entry_t, SaiError> {
        Ok(ffi::sai_neighbor_entry_t {
            switch_id: self.switch_oid()?,
            rif_id: rif.0,
            ip_address: Self::ip_address(ip),
        })
    }

    /// Create a (tagged|untagged) member of `vlan` fronting `bridge_port`.
    fn create_vlan_member_on(
        &self,
        vlan: ffi::sai_object_id_t,
        bridge_port: ffi::sai_object_id_t,
        tagged: bool,
    ) -> Result<ffi::sai_object_id_t, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid vlan api table; attr array outlives the call.
        let create = unsafe {
            (*self.vlan_api)
                .create_vlan_member
                .ok_or(SaiError::Other("vlan api lacks create_vlan_member".into()))?
        };
        use ffi::_sai_vlan_member_attr_t as attr;
        let mut vlan_attr = Self::zeroed_attr(attr::SAI_VLAN_MEMBER_ATTR_VLAN_ID);
        vlan_attr.value.oid = vlan;
        let mut bp_attr = Self::zeroed_attr(attr::SAI_VLAN_MEMBER_ATTR_BRIDGE_PORT_ID);
        bp_attr.value.oid = bridge_port;
        let mut mode_attr = Self::zeroed_attr(attr::SAI_VLAN_MEMBER_ATTR_VLAN_TAGGING_MODE);
        mode_attr.value.s32 = if tagged {
            ffi::_sai_vlan_tagging_mode_t::SAI_VLAN_TAGGING_MODE_TAGGED as i32
        } else {
            ffi::_sai_vlan_tagging_mode_t::SAI_VLAN_TAGGING_MODE_UNTAGGED as i32
        };
        let attrs = [vlan_attr, bp_attr, mode_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_vlan_member",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(oid)
    }

    /// Ingress VLAN classification of untagged frames. LAG ids dispatch
    /// to the LAG's own PVID attribute.
    fn set_pvid(&self, port: PortId, vlan_number: u16) -> Result<(), SaiError> {
        if self.lags.contains(&port.0) {
            let mut attr = Self::zeroed_attr(ffi::_sai_lag_attr_t::SAI_LAG_ATTR_PORT_VLAN_ID);
            attr.value.u16_ = vlan_number;
            // SAFETY: valid lag api table; attr outlives the call.
            return unsafe {
                let set = (*self.lag_api)
                    .set_lag_attribute
                    .ok_or(SaiError::Other("lag api lacks set_lag_attribute".into()))?;
                check("set_lag_attribute(PORT_VLAN_ID)", set(port.0, &attr))
            };
        }
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_PORT_VLAN_ID);
        attr.value.u16_ = vlan_number;
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check("set_port_attribute(PORT_VLAN_ID)", set(port.0, &attr))
        }
    }

    /// Detach a (port-like) object from the 802.1Q bridge: its
    /// default-VLAN membership, then its bridge port. Idempotent.
    fn detach_from_bridge(&self, port: PortId) -> Result<(), SaiError> {
        let Some(bridge_port) = self.find_bridge_port(port)? else {
            return Ok(());
        };
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
        if let Ok(mut index) = bridge_ports().write() {
            index.remove(&bridge_port);
        }
        Ok(())
    }

    /// Front a (port-like) object with a fresh 1Q bridge port and put
    /// it back in the default VLAN untagged with a matching PVID.
    fn attach_to_bridge(&self, port: PortId) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        let defaults = self.defaults()?;
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
        if let Ok(mut index) = bridge_ports().write() {
            index.insert(bridge_port, port.0);
        }
        self.create_vlan_member_on(defaults.vlan, bridge_port, false)?;
        self.set_pvid(port, defaults.vlan_number)
    }

    /// The bridge port fronting `port`, as an error when absent (routed
    /// ports have none).
    fn bridge_port_of(&self, port: PortId) -> Result<ffi::sai_object_id_t, SaiError> {
        self.find_bridge_port(port)?.ok_or(SaiError::Other(format!(
            "port {port} has no bridge port (routed?)"
        )))
    }

    /// Enumerate every 1Q bridge port and record its fronting port in
    /// the FDB-callback index.
    fn index_bridge_ports(&self) -> Result<(), SaiError> {
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
        let Ok(mut index) = bridge_ports().write() else {
            return Ok(());
        };
        for bridge_port in members {
            let mut attr =
                Self::zeroed_attr(ffi::_sai_bridge_port_attr_t::SAI_BRIDGE_PORT_ATTR_PORT_ID);
            // SAFETY: single-attr get; non-PORT bridge ports may reject it.
            let port = unsafe {
                (get_bridge_port(bridge_port, 1, &mut attr) == 0).then_some(attr.value.oid)
            };
            if let Some(port) = port {
                index.insert(bridge_port, port);
            }
        }
        Ok(())
    }

    /// One probed attr capability; an unavailable probe reads as
    /// supported (the real call then fails with a real status).
    fn attr_supported(&self, object_type: ffi::sai_object_type_t, attr: u32, set: bool) -> bool {
        let Some(query) = self.query_capability else {
            return true;
        };
        let Ok(switch) = self.switch_oid() else {
            return true;
        };
        // SAFETY: sai_attr_capability_t is POD; query fills it.
        let mut caps: ffi::sai_attr_capability_t = unsafe { std::mem::zeroed() };
        let status = unsafe { query(switch, object_type, attr, &mut caps) };
        if status != 0 {
            return true;
        }
        if set {
            caps.set_implemented || caps.create_implemented
        } else {
            caps.create_implemented
        }
    }

    /// Whether the ASIC serves the WRED-drop / ECN-marked queue stats:
    /// read them once on the first port's first queue. A refused read
    /// means those two counter columns render zero forever after.
    fn probe_wred_queue_stats(&self) -> bool {
        // SAFETY per block: valid api tables; buffers outlive the calls.
        unsafe {
            let Some(get_port_attr) = (*self.port_api).get_port_attribute else {
                return false;
            };
            let Some(get_queue_stats) = (*self.queue_api).get_queue_stats else {
                return false;
            };
            let Some(port) = self.first_port else {
                return false;
            };
            let mut oids = [0 as ffi::sai_object_id_t; 1];
            let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_QUEUE_LIST);
            attr.value.objlist.count = 1;
            attr.value.objlist.list = oids.as_mut_ptr();
            // A count-too-small answer still fills the first slot.
            let status = get_port_attr(port, 1, &mut attr);
            if oids[0] == 0 && status != 0 {
                return false;
            }
            const STAT_IDS: [ffi::sai_stat_id_t; 2] = [
                ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_WRED_DROPPED_PACKETS as ffi::sai_stat_id_t,
                ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_WRED_ECN_MARKED_PACKETS
                    as ffi::sai_stat_id_t,
            ];
            let mut stats = [0u64; 2];
            get_queue_stats(
                oids[0],
                STAT_IDS.len() as u32,
                STAT_IDS.as_ptr(),
                stats.as_mut_ptr(),
            ) == 0
        }
    }

    fn fdb_entry(&self, vlan: Option<Oid>, mac: [u8; 6]) -> Result<ffi::sai_fdb_entry_t, SaiError> {
        Ok(ffi::sai_fdb_entry_t {
            switch_id: self.switch_oid()?,
            mac_address: mac,
            bv_id: match vlan {
                Some(vlan) => vlan.0,
                None => self.defaults()?.vlan,
            },
        })
    }

    /// Attach (or detach, with `SAI_NULL_OBJECT_ID`) a storm-control
    /// policer on one port attribute.
    fn set_storm_policer_attr(
        &self,
        port: PortId,
        class: StormClass,
        policer: ffi::sai_object_id_t,
    ) -> Result<(), SaiError> {
        use ffi::_sai_port_attr_t as attr;
        let id = match class {
            StormClass::Broadcast => attr::SAI_PORT_ATTR_BROADCAST_STORM_CONTROL_POLICER_ID,
            StormClass::Multicast => attr::SAI_PORT_ATTR_MULTICAST_STORM_CONTROL_POLICER_ID,
            StormClass::UnknownUnicast => attr::SAI_PORT_ATTR_FLOOD_STORM_CONTROL_POLICER_ID,
        };
        let mut a = Self::zeroed_attr(id);
        a.value.oid = policer;
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check(
                "set_port_attribute(STORM_CONTROL_POLICER_ID)",
                set(port.0, &a),
            )
        }
    }

    /// The QoS-map api table, or a clear error when the vendor blob
    /// refused to serve it (the capability probe then reads
    /// unsupported, so a commit never reaches this).
    fn qos_map_api(&self) -> Result<*mut ffi::sai_qos_map_api_t, SaiError> {
        if self.qos_map_api.is_null() {
            return Err(SaiError::Other("SAI serves no QoS map api table".into()));
        }
        Ok(self.qos_map_api)
    }

    fn sai_qos_map_type(kind: QosMapType) -> u32 {
        use ffi::_sai_qos_map_type_t as t;
        match kind {
            QosMapType::DscpToTc => t::SAI_QOS_MAP_TYPE_DSCP_TO_TC,
            QosMapType::Dot1pToTc => t::SAI_QOS_MAP_TYPE_DOT1P_TO_TC,
            QosMapType::TcToDscp => t::SAI_QOS_MAP_TYPE_TC_AND_COLOR_TO_DSCP,
            QosMapType::TcToDot1p => t::SAI_QOS_MAP_TYPE_TC_AND_COLOR_TO_DOT1P,
        }
    }

    /// The port attribute a map type binds through.
    fn qos_map_port_attr(kind: QosMapType) -> u32 {
        use ffi::_sai_port_attr_t as attr;
        match kind {
            QosMapType::DscpToTc => attr::SAI_PORT_ATTR_QOS_DSCP_TO_TC_MAP,
            QosMapType::Dot1pToTc => attr::SAI_PORT_ATTR_QOS_DOT1P_TO_TC_MAP,
            QosMapType::TcToDscp => attr::SAI_PORT_ATTR_QOS_TC_AND_COLOR_TO_DSCP_MAP,
            QosMapType::TcToDot1p => attr::SAI_PORT_ATTR_QOS_TC_AND_COLOR_TO_DOT1P_MAP,
        }
    }

    /// Hemlock's `(key, value)` pairs as SAI map entries. Which fields
    /// carry the key and the value differ per map type; unused ones
    /// stay 0, and the egress rewrite maps key on (tc, GREEN) because
    /// SAI's TC->DSCP and TC->Dot1p maps are colour-qualified.
    fn qos_map_entries(kind: QosMapType, entries: &[(u8, u8)]) -> Vec<ffi::sai_qos_map_t> {
        entries
            .iter()
            .map(|(key, value)| {
                // SAFETY: sai_qos_map_t is POD; every field is written
                // or deliberately left zero.
                let mut entry: ffi::sai_qos_map_t = unsafe { std::mem::zeroed() };
                match kind {
                    QosMapType::DscpToTc => {
                        entry.key.dscp = *key;
                        entry.value.tc = *value;
                    }
                    QosMapType::Dot1pToTc => {
                        entry.key.dot1p = *key;
                        entry.value.tc = *value;
                    }
                    QosMapType::TcToDscp => {
                        entry.key.tc = *key;
                        entry.key.color = ffi::_sai_packet_color_t::SAI_PACKET_COLOR_GREEN;
                        entry.value.dscp = *value;
                    }
                    QosMapType::TcToDot1p => {
                        entry.key.tc = *key;
                        entry.key.color = ffi::_sai_packet_color_t::SAI_PACKET_COLOR_GREEN;
                        entry.value.dot1p = *value;
                    }
                }
                entry
            })
            .collect()
    }

    /// The SAI queue object for one of a port's unicast egress queues.
    ///
    /// # The Broadcom scheduler topology
    ///
    /// Helix4 hangs a three-level scheduler-group tree off every port:
    /// a root group per port, one child group per traffic class, and
    /// the queue objects at the leaves. Hemlock's config language is
    /// flat (`queue <0-7>` on a port), so the whole tree stays an
    /// implementation detail of this backend.
    ///
    /// Scheduler profiles and WRED profiles bind on the *queue* object
    /// (`SAI_QUEUE_ATTR_SCHEDULER_PROFILE_ID` /
    /// `SAI_QUEUE_ATTR_WRED_PROFILE_ID`), not on the intermediate
    /// groups, so the only walk needed is port -> `QOS_QUEUE_LIST` ->
    /// the entry whose `INDEX` matches and whose `TYPE` is unicast (or
    /// the type-agnostic `ALL`). Ports report multicast queues in the
    /// same list, hence the type filter.
    fn queue_oid(&self, port: PortId, index: u32) -> Result<ffi::sai_object_id_t, SaiError> {
        // SAFETY per block below: valid api tables; buffers outlive the calls.
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
        let mut oids: Vec<ffi::sai_object_id_t> = vec![0; count as usize];
        {
            let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_QUEUE_LIST);
            attr.value.objlist.count = count;
            attr.value.objlist.list = oids.as_mut_ptr();
            // SAFETY: list buffer sized to `count`, alive across the call.
            unsafe {
                check("get(QOS_QUEUE_LIST)", get_port_attr(port.0, 1, &mut attr))?;
                oids.truncate(attr.value.objlist.count as usize);
            }
        }
        for oid in oids {
            let type_attr = Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_TYPE);
            let index_attr = Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_INDEX);
            let mut attrs = [type_attr, index_attr];
            // SAFETY: attr array valid across the call; union reads
            // match the attr ids just fetched.
            let (kind, queue_index) = unsafe {
                check(
                    "get_queue_attribute",
                    get_queue_attr(oid, attrs.len() as u32, attrs.as_mut_ptr()),
                )?;
                (attrs[0].value.s32, u32::from(attrs[1].value.u8_))
            };
            let unicast = kind != ffi::_sai_queue_type_t::SAI_QUEUE_TYPE_MULTICAST as i32;
            if unicast && queue_index == index {
                return Ok(oid);
            }
        }
        Err(SaiError::Other(format!(
            "port {port} has no unicast egress queue {index}"
        )))
    }

    fn scheduler_api(&self) -> Result<*mut ffi::sai_scheduler_api_t, SaiError> {
        if self.scheduler_api.is_null() {
            return Err(SaiError::Other("SAI serves no scheduler api table".into()));
        }
        Ok(self.scheduler_api)
    }

    fn wred_api(&self) -> Result<*mut ffi::sai_wred_api_t, SaiError> {
        if self.wred_api.is_null() {
            return Err(SaiError::Other("SAI serves no WRED api table".into()));
        }
        Ok(self.wred_api)
    }

    /// The attribute set of one scheduler profile. SAI meters bytes,
    /// Hemlock's config language bits, so shaper rates divide by 8.
    fn scheduler_attrs(spec: SchedulerSpec) -> Vec<ffi::sai_attribute_t> {
        use ffi::_sai_scheduler_attr_t as attr;
        let mut type_attr = Self::zeroed_attr(attr::SAI_SCHEDULER_ATTR_SCHEDULING_TYPE);
        type_attr.value.s32 = if spec.strict {
            ffi::_sai_scheduling_type_t::SAI_SCHEDULING_TYPE_STRICT
        } else {
            ffi::_sai_scheduling_type_t::SAI_SCHEDULING_TYPE_DWRR
        } as i32;
        let mut attrs = vec![type_attr];
        if !spec.strict {
            let mut weight_attr = Self::zeroed_attr(attr::SAI_SCHEDULER_ATTR_SCHEDULING_WEIGHT);
            weight_attr.value.u8_ = spec.weight;
            attrs.push(weight_attr);
        }
        let mut meter_attr = Self::zeroed_attr(attr::SAI_SCHEDULER_ATTR_METER_TYPE);
        meter_attr.value.s32 = ffi::_sai_meter_type_t::SAI_METER_TYPE_BYTES as i32;
        attrs.push(meter_attr);
        let mut rate_attr = Self::zeroed_attr(attr::SAI_SCHEDULER_ATTR_MAX_BANDWIDTH_RATE);
        // 0 = no limit, which is exactly the unshaped case.
        rate_attr.value.u64_ = spec.max_rate_bps.unwrap_or(0) / 8;
        attrs.push(rate_attr);
        attrs
    }

    /// The attribute set of one WRED profile. Hemlock exposes a single
    /// curve, so it is programmed on all three colours; ECN marking
    /// swaps drops for marks across the board.
    fn wred_attrs(spec: WredSpec) -> Vec<ffi::sai_attribute_t> {
        use ffi::_sai_wred_attr_t as attr;
        let mut attrs = Vec::new();
        for (enable, min, max, probability) in [
            (
                attr::SAI_WRED_ATTR_GREEN_ENABLE,
                attr::SAI_WRED_ATTR_GREEN_MIN_THRESHOLD,
                attr::SAI_WRED_ATTR_GREEN_MAX_THRESHOLD,
                attr::SAI_WRED_ATTR_GREEN_DROP_PROBABILITY,
            ),
            (
                attr::SAI_WRED_ATTR_YELLOW_ENABLE,
                attr::SAI_WRED_ATTR_YELLOW_MIN_THRESHOLD,
                attr::SAI_WRED_ATTR_YELLOW_MAX_THRESHOLD,
                attr::SAI_WRED_ATTR_YELLOW_DROP_PROBABILITY,
            ),
            (
                attr::SAI_WRED_ATTR_RED_ENABLE,
                attr::SAI_WRED_ATTR_RED_MIN_THRESHOLD,
                attr::SAI_WRED_ATTR_RED_MAX_THRESHOLD,
                attr::SAI_WRED_ATTR_RED_DROP_PROBABILITY,
            ),
        ] {
            let mut enable_attr = Self::zeroed_attr(enable);
            enable_attr.value.booldata = true;
            let mut min_attr = Self::zeroed_attr(min);
            min_attr.value.u32_ = spec.min_threshold_bytes;
            let mut max_attr = Self::zeroed_attr(max);
            max_attr.value.u32_ = spec.max_threshold_bytes;
            let mut probability_attr = Self::zeroed_attr(probability);
            probability_attr.value.u32_ = u32::from(spec.drop_probability);
            attrs.extend([enable_attr, min_attr, max_attr, probability_attr]);
        }
        let mut ecn_attr = Self::zeroed_attr(attr::SAI_WRED_ATTR_ECN_MARK_MODE);
        ecn_attr.value.s32 = if spec.ecn {
            ffi::_sai_ecn_mark_mode_t::SAI_ECN_MARK_MODE_ALL
        } else {
            ffi::_sai_ecn_mark_mode_t::SAI_ECN_MARK_MODE_NONE
        } as i32;
        attrs.push(ecn_attr);
        attrs
    }

    /// The ACL api table, or a clear error when the vendor blob refused
    /// to serve it (the capability probe then reads unsupported, so a
    /// commit never reaches this).
    fn acl_api(&self) -> Result<*mut ffi::sai_acl_api_t, SaiError> {
        if self.acl_api.is_null() {
            return Err(SaiError::Other("SAI serves no ACL api table".into()));
        }
        Ok(self.acl_api)
    }

    fn sai_packet_action(action: AclPacketAction) -> i32 {
        use ffi::_sai_packet_action_t as pa;
        (match action {
            AclPacketAction::Forward => pa::SAI_PACKET_ACTION_FORWARD,
            AclPacketAction::Drop => pa::SAI_PACKET_ACTION_DROP,
            AclPacketAction::Trap => pa::SAI_PACKET_ACTION_TRAP,
            AclPacketAction::Copy => pa::SAI_PACKET_ACTION_COPY,
        }) as i32
    }

    /// The saihostif.h trap type for a protocol trap; None for the
    /// user-defined ACL-log trap, which rides its own object family.
    fn sai_trap_type(kind: TrapKind) -> Option<u32> {
        use ffi::_sai_hostif_trap_type_t as trap;
        Some(match kind {
            TrapKind::ArpRequest => trap::SAI_HOSTIF_TRAP_TYPE_ARP_REQUEST,
            TrapKind::ArpResponse => trap::SAI_HOSTIF_TRAP_TYPE_ARP_RESPONSE,
            TrapKind::Ip2me => trap::SAI_HOSTIF_TRAP_TYPE_IP2ME,
            TrapKind::Stp => trap::SAI_HOSTIF_TRAP_TYPE_STP,
            TrapKind::Lacp => trap::SAI_HOSTIF_TRAP_TYPE_LACP,
            TrapKind::Lldp => trap::SAI_HOSTIF_TRAP_TYPE_LLDP,
            TrapKind::Eapol => trap::SAI_HOSTIF_TRAP_TYPE_EAPOL,
            TrapKind::IgmpQuery => trap::SAI_HOSTIF_TRAP_TYPE_IGMP_TYPE_QUERY,
            TrapKind::IgmpLeave => trap::SAI_HOSTIF_TRAP_TYPE_IGMP_TYPE_LEAVE,
            TrapKind::IgmpV1Report => trap::SAI_HOSTIF_TRAP_TYPE_IGMP_TYPE_V1_REPORT,
            TrapKind::IgmpV2Report => trap::SAI_HOSTIF_TRAP_TYPE_IGMP_TYPE_V2_REPORT,
            TrapKind::IgmpV3Report => trap::SAI_HOSTIF_TRAP_TYPE_IGMP_TYPE_V3_REPORT,
            TrapKind::MldV1V2 => trap::SAI_HOSTIF_TRAP_TYPE_IPV6_MLD_V1_V2,
            TrapKind::MldV1Report => trap::SAI_HOSTIF_TRAP_TYPE_IPV6_MLD_V1_REPORT,
            TrapKind::MldV1Done => trap::SAI_HOSTIF_TRAP_TYPE_IPV6_MLD_V1_DONE,
            TrapKind::MldV2Report => trap::SAI_HOSTIF_TRAP_TYPE_MLD_V2_REPORT,
            TrapKind::Dhcp => trap::SAI_HOSTIF_TRAP_TYPE_DHCP,
            TrapKind::Ospf => trap::SAI_HOSTIF_TRAP_TYPE_OSPF,
            TrapKind::Bgp => trap::SAI_HOSTIF_TRAP_TYPE_BGP,
            TrapKind::Vrrp => trap::SAI_HOSTIF_TRAP_TYPE_VRRP,
            TrapKind::AclLog => return None,
        })
    }

    /// Update one policer's CIR/CBS in place.
    fn set_policer_rate_attrs(
        &self,
        policer: ffi::sai_object_id_t,
        spec: PolicerSpec,
    ) -> Result<(), SaiError> {
        // SAFETY: valid policer api table; attrs outlive the calls.
        let set = unsafe {
            (*self.policer_api)
                .set_policer_attribute
                .ok_or(SaiError::Other(
                    "policer api lacks set_policer_attribute".into(),
                ))?
        };
        use ffi::_sai_policer_attr_t as attr;
        let mut cir = Self::zeroed_attr(attr::SAI_POLICER_ATTR_CIR);
        cir.value.u64_ = if spec.pps { spec.rate } else { spec.rate / 8 };
        let mut cbs = Self::zeroed_attr(attr::SAI_POLICER_ATTR_CBS);
        cbs.value.u64_ = spec.burst;
        // SAFETY: attrs outlive the calls.
        unsafe {
            check("set_policer_attribute(CIR)", set(policer, &cir))?;
            check("set_policer_attribute(CBS)", set(policer, &cbs))
        }
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

        let mut fdb_notify_attr =
            Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_FDB_EVENT_NOTIFY);
        let fdb_cb: unsafe extern "C" fn(u32, *const ffi::sai_fdb_event_notification_data_t) =
            on_fdb_event;
        fdb_notify_attr.value.ptr = fdb_cb as *mut c_void;

        let mut attrs = vec![init_attr, profile_attr, notify_attr, fdb_notify_attr];
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

        // Seed the bridge-port index the FDB event callback maps
        // through; best-effort (no bridge ports = no port names on FDB
        // events, not a failed boot).
        if let Err(err) = self.index_bridge_ports() {
            tracing::warn!(%err, "cannot index bridge ports; FDB events lose port identity");
        }

        if self.diag_shell {
            self.spawn_diag_shell(oid)?;
        }
        Ok(SwitchInfo {
            oid,
            default_vlan_oid: self.defaults.map(|d| d.vlan).unwrap_or(0),
        })
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
        self.first_port = ports.first().map(|p| p.id.0);
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

    fn set_port_speed(&mut self, port: PortId, speed_mbps: u32) -> Result<(), SaiError> {
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_SPEED);
        attr.value.u32_ = speed_mbps;
        self.set_one_port_attr("set_port_attribute(SPEED)", port, &attr)
    }

    fn set_port_duplex(&mut self, port: PortId, full: bool) -> Result<(), SaiError> {
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_FULL_DUPLEX_MODE);
        attr.value.booldata = full;
        self.set_one_port_attr("set_port_attribute(FULL_DUPLEX_MODE)", port, &attr)
    }

    fn set_port_autoneg(&mut self, port: PortId, on: bool) -> Result<(), SaiError> {
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_AUTO_NEG_MODE);
        attr.value.booldata = on;
        self.set_one_port_attr("set_port_attribute(AUTO_NEG_MODE)", port, &attr)
    }

    fn set_port_mtu(&mut self, port: PortId, mtu: u32) -> Result<(), SaiError> {
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_MTU);
        attr.value.u32_ = mtu;
        self.set_one_port_attr("set_port_attribute(MTU)", port, &attr)
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
        // The WRED pair is optional (probed once into
        // `wred_queue_stats`); a refused read leaves both columns 0
        // rather than failing the whole sweep.
        const WRED_STAT_IDS: [ffi::sai_stat_id_t; 2] = [
            ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_WRED_DROPPED_PACKETS as ffi::sai_stat_id_t,
            ffi::_sai_queue_stat_t::SAI_QUEUE_STAT_WRED_ECN_MARKED_PACKETS as ffi::sai_stat_id_t,
        ];

        let mut queues = Vec::with_capacity(queue_oids.len());
        for oid in queue_oids {
            let type_attr = Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_TYPE);
            let index_attr = Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_INDEX);
            let mut attrs = [type_attr, index_attr];
            let mut stats = [0u64; STAT_IDS.len()];
            let mut wred_stats = [0u64; WRED_STAT_IDS.len()];
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
                if get_queue_stats(
                    oid,
                    WRED_STAT_IDS.len() as u32,
                    WRED_STAT_IDS.as_ptr(),
                    wred_stats.as_mut_ptr(),
                ) != 0
                {
                    wred_stats = [0; WRED_STAT_IDS.len()];
                }
                queues.push(QueueCounters {
                    unicast: attrs[0].value.s32
                        != ffi::_sai_queue_type_t::SAI_QUEUE_TYPE_MULTICAST as i32,
                    index: u32::from(attrs[1].value.u8_),
                    pkts: stats[0],
                    bytes: stats[1],
                    dropped_pkts: stats[2],
                    dropped_bytes: stats[3],
                    wred_dropped: wred_stats[0],
                    ecn_marked: wred_stats[1],
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

        // The protocol traps themselves (ARP, IP2ME, and the rest of
        // the CoPP class table) are created by syncd's CoPP program
        // after boot; this call installs only the delivery path.

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
        tracing::info!("CPU punt delivery path installed (wildcard netdev delivery)");
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
        self.detach_from_bridge(port)?;

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

    fn create_vlan_router_interface(&mut self, vlan: Option<Oid>) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let defaults = self.defaults()?;
        let vlan_oid = vlan.map(|v| v.0).unwrap_or(defaults.vlan);

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
            ffi::_sai_router_interface_type_t::SAI_ROUTER_INTERFACE_TYPE_VLAN as i32;
        let mut vlan_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_VLAN_ID);
        vlan_attr.value.oid = vlan_oid;
        let mut mtu_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_MTU);
        mtu_attr.value.u32_ = 9214;

        let mut attrs = vec![vr_attr, type_attr, vlan_attr, mtu_attr];
        if let Some(mac) = self.src_mac {
            let mut mac_attr = Self::zeroed_attr(attr::SAI_ROUTER_INTERFACE_ATTR_SRC_MAC_ADDRESS);
            mac_attr.value.mac = mac;
            attrs.push(mac_attr);
        }
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_router_interface(VLAN)",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_vlan_router_interface(&mut self, rif: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid rif api table.
        unsafe {
            let remove = (*self.rif_api)
                .remove_router_interface
                .ok_or(SaiError::Other(
                    "router interface api lacks remove_router_interface".into(),
                ))?;
            check("remove_router_interface(VLAN)", remove(rif.0))
        }
    }

    fn remove_router_interface(&mut self, port: PortId, rif: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;

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
        self.attach_to_bridge(port)
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
        // A drop route carries a packet action instead of a next hop.
        let attr = match target {
            RouteTarget::Drop => {
                let mut action = Self::zeroed_attr(
                    ffi::_sai_route_entry_attr_t::SAI_ROUTE_ENTRY_ATTR_PACKET_ACTION,
                );
                action.value.s32 = ffi::_sai_packet_action_t::SAI_PACKET_ACTION_DROP as i32;
                action
            }
            _ => {
                let mut nh_attr = Self::zeroed_attr(
                    ffi::_sai_route_entry_attr_t::SAI_ROUTE_ENTRY_ATTR_NEXT_HOP_ID,
                );
                nh_attr.value.oid = match target {
                    RouteTarget::Cpu => defaults.cpu_port,
                    RouteTarget::Rif(rif) => rif.0,
                    RouteTarget::NextHop(next_hop) => next_hop.0,
                    RouteTarget::Group(group) => group.0,
                    RouteTarget::Drop => unreachable!("handled above"),
                };
                nh_attr
            }
        };
        // SAFETY: entry and attr outlive the call.
        unsafe { check("create_route_entry", create(&entry, 1, &attr)) }
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

    fn create_neighbor(
        &mut self,
        rif: Oid,
        ip: std::net::IpAddr,
        mac: [u8; 6],
    ) -> Result<(), SaiError> {
        let entry = self.neighbor_entry(rif, ip)?;
        // SAFETY: valid neighbor api table; entry + attr outlive the call.
        let create = unsafe {
            (*self.neighbor_api)
                .create_neighbor_entry
                .ok_or(SaiError::Other(
                    "neighbor api lacks create_neighbor_entry".into(),
                ))?
        };
        let mut mac_attr = Self::zeroed_attr(
            ffi::_sai_neighbor_entry_attr_t::SAI_NEIGHBOR_ENTRY_ATTR_DST_MAC_ADDRESS,
        );
        mac_attr.value.mac = mac;
        // SAFETY: entry and attr outlive the call. A pre-existing entry
        // for the same (rif, ip) is replaced by remove + create: SAI's
        // create fails with ITEM_ALREADY_EXISTS, so try the set path on
        // that status.
        unsafe {
            let status = create(&entry, 1, &mac_attr);
            if status != 0 {
                if let Some(set) = (*self.neighbor_api).set_neighbor_entry_attribute {
                    return check(
                        "set_neighbor_entry_attribute(DST_MAC)",
                        set(&entry, &mac_attr),
                    );
                }
            }
            check("create_neighbor_entry", status)
        }
    }

    fn remove_neighbor(&mut self, rif: Oid, ip: std::net::IpAddr) -> Result<(), SaiError> {
        let entry = self.neighbor_entry(rif, ip)?;
        // SAFETY: valid neighbor api table; entry outlives the call.
        unsafe {
            let remove = (*self.neighbor_api)
                .remove_neighbor_entry
                .ok_or(SaiError::Other(
                    "neighbor api lacks remove_neighbor_entry".into(),
                ))?;
            check("remove_neighbor_entry", remove(&entry))
        }
    }

    fn create_next_hop(&mut self, rif: Oid, ip: std::net::IpAddr) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid next-hop api table; attrs outlive the call.
        let create = unsafe {
            (*self.next_hop_api)
                .create_next_hop
                .ok_or(SaiError::Other("next-hop api lacks create_next_hop".into()))?
        };
        use ffi::_sai_next_hop_attr_t as attr;
        let mut type_attr = Self::zeroed_attr(attr::SAI_NEXT_HOP_ATTR_TYPE);
        type_attr.value.s32 = ffi::_sai_next_hop_type_t::SAI_NEXT_HOP_TYPE_IP as i32;
        let mut ip_attr = Self::zeroed_attr(attr::SAI_NEXT_HOP_ATTR_IP);
        ip_attr.value.ipaddr = Self::ip_address(ip);
        let mut rif_attr = Self::zeroed_attr(attr::SAI_NEXT_HOP_ATTR_ROUTER_INTERFACE_ID);
        rif_attr.value.oid = rif.0;
        let attrs = [type_attr, ip_attr, rif_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_next_hop",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_next_hop(&mut self, next_hop: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid next-hop api table.
        unsafe {
            let remove = (*self.next_hop_api)
                .remove_next_hop
                .ok_or(SaiError::Other("next-hop api lacks remove_next_hop".into()))?;
            check("remove_next_hop", remove(next_hop.0))
        }
    }

    fn create_next_hop_group(&mut self) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid next-hop-group api table; attr outlives the call.
        let create = unsafe {
            (*self.next_hop_group_api)
                .create_next_hop_group
                .ok_or(SaiError::Other(
                    "next-hop-group api lacks create_next_hop_group".into(),
                ))?
        };
        let mut type_attr =
            Self::zeroed_attr(ffi::_sai_next_hop_group_attr_t::SAI_NEXT_HOP_GROUP_ATTR_TYPE);
        type_attr.value.s32 = ffi::_sai_next_hop_group_type_t::SAI_NEXT_HOP_GROUP_TYPE_ECMP as i32;
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr outlives the call.
        unsafe {
            check(
                "create_next_hop_group",
                create(&mut oid, switch, 1, &type_attr),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_next_hop_group(&mut self, group: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid next-hop-group api table.
        unsafe {
            let remove =
                (*self.next_hop_group_api)
                    .remove_next_hop_group
                    .ok_or(SaiError::Other(
                        "next-hop-group api lacks remove_next_hop_group".into(),
                    ))?;
            check("remove_next_hop_group", remove(group.0))
        }
    }

    fn add_next_hop_group_member(&mut self, group: Oid, next_hop: Oid) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid next-hop-group api table; attrs outlive the call.
        let create = unsafe {
            (*self.next_hop_group_api)
                .create_next_hop_group_member
                .ok_or(SaiError::Other(
                    "next-hop-group api lacks create_next_hop_group_member".into(),
                ))?
        };
        use ffi::_sai_next_hop_group_member_attr_t as attr;
        let mut group_attr =
            Self::zeroed_attr(attr::SAI_NEXT_HOP_GROUP_MEMBER_ATTR_NEXT_HOP_GROUP_ID);
        group_attr.value.oid = group.0;
        let mut nh_attr = Self::zeroed_attr(attr::SAI_NEXT_HOP_GROUP_MEMBER_ATTR_NEXT_HOP_ID);
        nh_attr.value.oid = next_hop.0;
        let attrs = [group_attr, nh_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_next_hop_group_member",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_next_hop_group_member(&mut self, member: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid next-hop-group api table.
        unsafe {
            let remove = (*self.next_hop_group_api)
                .remove_next_hop_group_member
                .ok_or(SaiError::Other(
                    "next-hop-group api lacks remove_next_hop_group_member".into(),
                ))?;
            check("remove_next_hop_group_member", remove(member.0))
        }
    }

    fn create_my_mac(&mut self, vlan_id: Option<u16>, mac: [u8; 6]) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        if self.my_mac_api.is_null() {
            return Err(SaiError::Other(
                "this SAI does not serve the MY_MAC api".into(),
            ));
        }
        // SAFETY: non-null my-mac api table; attrs outlive the call.
        let create = unsafe {
            (*self.my_mac_api)
                .create_my_mac
                .ok_or(SaiError::Other("my-mac api lacks create_my_mac".into()))?
        };
        use ffi::_sai_my_mac_attr_t as attr;
        let mut attrs = Vec::with_capacity(2);
        let mut mac_attr = Self::zeroed_attr(attr::SAI_MY_MAC_ATTR_MAC_ADDRESS);
        mac_attr.value.mac = mac;
        attrs.push(mac_attr);
        if let Some(vlan) = vlan_id {
            let mut vlan_attr = Self::zeroed_attr(attr::SAI_MY_MAC_ATTR_VLAN_ID);
            vlan_attr.value.u16_ = vlan;
            attrs.push(vlan_attr);
        }
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_my_mac",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_my_mac(&mut self, my_mac: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        if self.my_mac_api.is_null() {
            return Err(SaiError::Other(
                "this SAI does not serve the MY_MAC api".into(),
            ));
        }
        // SAFETY: non-null my-mac api table.
        unsafe {
            let remove = (*self.my_mac_api)
                .remove_my_mac
                .ok_or(SaiError::Other("my-mac api lacks remove_my_mac".into()))?;
            check("remove_my_mac", remove(my_mac.0))
        }
    }

    fn create_vlan(&mut self, vlan_id: u16) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid vlan api table; attr outlives the call.
        let create = unsafe {
            (*self.vlan_api)
                .create_vlan
                .ok_or(SaiError::Other("vlan api lacks create_vlan".into()))?
        };
        let mut attr = Self::zeroed_attr(ffi::_sai_vlan_attr_t::SAI_VLAN_ATTR_VLAN_ID);
        attr.value.u16_ = vlan_id;
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr outlives the call.
        unsafe {
            check("create_vlan", create(&mut oid, switch, 1, &attr))?;
        }
        Ok(Oid(oid))
    }

    fn remove_vlan(&mut self, vlan: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid vlan api table.
        unsafe {
            let remove = (*self.vlan_api)
                .remove_vlan
                .ok_or(SaiError::Other("vlan api lacks remove_vlan".into()))?;
            check("remove_vlan", remove(vlan.0))
        }
    }

    fn add_vlan_member(&mut self, vlan: Oid, port: PortId, tagged: bool) -> Result<Oid, SaiError> {
        let bridge_port = self.bridge_port_of(port)?;
        self.create_vlan_member_on(vlan.0, bridge_port, tagged)
            .map(Oid)
    }

    fn remove_vlan_member(&mut self, member: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid vlan api table.
        unsafe {
            let remove = (*self.vlan_api)
                .remove_vlan_member
                .ok_or(SaiError::Other("vlan api lacks remove_vlan_member".into()))?;
            check("remove_vlan_member", remove(member.0))
        }
    }

    fn set_port_pvid(&mut self, port: PortId, vlan_number: u16) -> Result<(), SaiError> {
        self.switch_oid()?;
        self.set_pvid(port, vlan_number)
    }

    fn remove_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError> {
        self.switch_oid()?;
        let bridge_port = self.bridge_port_of(port)?;
        if let Some(member) = self.find_default_vlan_member(bridge_port)? {
            // SAFETY: valid vlan api table.
            unsafe {
                let remove = (*self.vlan_api)
                    .remove_vlan_member
                    .ok_or(SaiError::Other("vlan api lacks remove_vlan_member".into()))?;
                check("remove_vlan_member(default)", remove(member))?;
            }
        }
        Ok(())
    }

    fn restore_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError> {
        let defaults = self.defaults()?;
        let bridge_port = self.bridge_port_of(port)?;
        if self.find_default_vlan_member(bridge_port)?.is_none() {
            self.create_vlan_member_on(defaults.vlan, bridge_port, false)?;
        }
        self.set_pvid(port, defaults.vlan_number)
    }

    fn capabilities(&mut self) -> Result<SaiCapabilities, SaiError> {
        self.switch_oid()?;
        use ffi::_sai_object_type_t as object;

        // SAFETY per block below: valid api tables (fn-pointer presence
        // reads only).
        let fdb_flush = unsafe { (*self.fdb_api).flush_fdb_entries.is_some() };
        let policer_fns = unsafe { (*self.policer_api).create_policer.is_some() };
        let mirror_fns = unsafe { (*self.mirror_api).create_mirror_session.is_some() };

        let mirror = mirror_fns
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_MIRROR_SESSION,
                ffi::_sai_mirror_session_attr_t::SAI_MIRROR_SESSION_ATTR_MONITOR_PORT,
                false,
            );
        // Session capacity, when the switch reports it; 4 is the Helix4
        // default otherwise.
        let mirror_sessions_max = if !mirror {
            0
        } else {
            let mut max = 4u32;
            // SAFETY: valid switch api table; attr outlives the call;
            // union read matches the u32 attr.
            unsafe {
                if let Some(get) = (*self.switch_api).get_switch_attribute {
                    let mut attr = Self::zeroed_attr(
                        ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_MAX_MIRROR_SESSION,
                    );
                    if get(self.switch_oid()?, 1, &mut attr) == 0 && attr.value.u32_ > 0 {
                        max = attr.value.u32_;
                    }
                }
            }
            max
        };

        // ECMP: next-hop groups exist iff the api serves creates; the
        // width comes from SAI_SWITCH_ATTR_ECMP_MEMBERS when the switch
        // answers, else the manifest-profiled Helix4 default (64).
        let nhg_fns = unsafe { (*self.next_hop_group_api).create_next_hop_group.is_some() };
        let ecmp_width = if !nhg_fns {
            0
        } else {
            let mut width = 64u32;
            // SAFETY: valid switch api table; attr outlives the call;
            // union read matches the u32 attr.
            unsafe {
                if let Some(get) = (*self.switch_api).get_switch_attribute {
                    let mut attr =
                        Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_ECMP_MEMBERS);
                    if get(self.switch_oid()?, 1, &mut attr) == 0 && attr.value.u32_ > 0 {
                        width = attr.value.u32_;
                    }
                }
            }
            width
        };
        // IPv6: assumed present unless the switch reports a zero-entry
        // v6 route table (attribute optional; a failed get proves
        // nothing, so it does not clear the bit).
        let mut ipv6 = true;
        // SAFETY: valid switch api table; attr outlives the call.
        unsafe {
            if let Some(get) = (*self.switch_api).get_switch_attribute {
                let mut attr = Self::zeroed_attr(
                    ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_AVAILABLE_IPV6_ROUTE_ENTRY,
                );
                if get(self.switch_oid()?, 1, &mut attr) == 0 && attr.value.u32_ == 0 {
                    ipv6 = false;
                }
            }
        }
        // MY_MAC: the api table may be absent outright (pre-v1.9 blobs)
        // and, even served, the Broadcom blob may refuse the object —
        // the attribute-capability query is the closest static probe.
        // SAFETY: null check before the fn-pointer presence read.
        let my_mac = !self.my_mac_api.is_null()
            && unsafe { (*self.my_mac_api).create_my_mac.is_some() }
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_MY_MAC,
                ffi::_sai_my_mac_attr_t::SAI_MY_MAC_ATTR_MAC_ADDRESS,
                false,
            );

        // ACLs: the api table may have been refused outright; stages
        // probe individually (Helix4's egress TCAM is optional in some
        // SAI builds).
        // SAFETY: null check before the fn-pointer presence read.
        let acl_fns =
            !self.acl_api.is_null() && unsafe { (*self.acl_api).create_acl_table.is_some() };
        let acl_ingress = acl_fns
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_PORT,
                ffi::_sai_port_attr_t::SAI_PORT_ATTR_INGRESS_ACL,
                true,
            );
        let acl_egress = acl_fns
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_PORT,
                ffi::_sai_port_attr_t::SAI_PORT_ATTR_EGRESS_ACL,
                true,
            );
        let acl_entry_policer = acl_fns
            && policer_fns
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_ACL_ENTRY,
                ffi::_sai_acl_entry_attr_t::SAI_ACL_ENTRY_ATTR_ACTION_SET_POLICER,
                false,
            );
        let port_learn_limit = self.attr_supported(
            object::SAI_OBJECT_TYPE_BRIDGE_PORT,
            ffi::_sai_bridge_port_attr_t::SAI_BRIDGE_PORT_ATTR_MAX_LEARNED_ADDRESSES,
            true,
        );
        // SAFETY: valid hostif api table (fn-pointer presence read).
        let copp = policer_fns && unsafe { (*self.hostif_api).create_hostif_trap_group.is_some() };

        // --- QoS suite -----------------------------------------------
        // The shared packet buffer, for the WRED threshold cap. Helix4
        // carries 4 MB; a switch that will not answer reports 0 and the
        // cap check is skipped rather than guessed.
        let mut buffer_bytes_total = 0u64;
        // SAFETY: valid switch api table; attr outlives the call.
        unsafe {
            if let Some(get) = (*self.switch_api).get_switch_attribute {
                let mut attr =
                    Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_TOTAL_BUFFER_SIZE);
                if get(self.switch_oid()?, 1, &mut attr) == 0 {
                    // SAI reports the pool in KB.
                    buffer_bytes_total = u64::from(attr.value.u32_) * 1024;
                }
            }
        }
        // qos maps: the api table may have been refused outright; each
        // direction then probes its port binding attribute.
        // SAFETY: null check before the fn-pointer presence read.
        let qos_map_fns =
            !self.qos_map_api.is_null() && unsafe { (*self.qos_map_api).create_qos_map.is_some() };
        let qos_map_ingress = qos_map_fns
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_PORT,
                ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_DSCP_TO_TC_MAP,
                true,
            );
        let qos_map_egress = qos_map_fns
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_PORT,
                ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_TC_AND_COLOR_TO_DSCP_MAP,
                true,
            );
        let wred = self.attr_supported(
            object::SAI_OBJECT_TYPE_WRED,
            ffi::_sai_wred_attr_t::SAI_WRED_ATTR_GREEN_ENABLE,
            false,
        ) && self.attr_supported(
            object::SAI_OBJECT_TYPE_QUEUE,
            ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_WRED_PROFILE_ID,
            true,
        );
        let ecn = wred
            && self.attr_supported(
                object::SAI_OBJECT_TYPE_WRED,
                ffi::_sai_wred_attr_t::SAI_WRED_ATTR_ECN_MARK_MODE,
                false,
            );
        // Queue shapers ride a per-queue scheduler profile; the port
        // shaper rides the port's own, which exists wherever schedulers
        // do, so only the queue binding needs a probe.
        let queue_shaper = self.attr_supported(
            object::SAI_OBJECT_TYPE_QUEUE,
            ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_SCHEDULER_PROFILE_ID,
            true,
        ) && self.attr_supported(
            object::SAI_OBJECT_TYPE_SCHEDULER,
            ffi::_sai_scheduler_attr_t::SAI_SCHEDULER_ATTR_MAX_BANDWIDTH_RATE,
            false,
        );
        // The WRED-drop / ECN-marked queue stats are read once at boot
        // on the first port; a refused read turns the two columns off.
        let wred_queue_stats = wred && self.probe_wred_queue_stats();

        Ok(SaiCapabilities {
            lag: self.attr_supported(
                object::SAI_OBJECT_TYPE_LAG,
                ffi::_sai_lag_attr_t::SAI_LAG_ATTR_PORT_LIST,
                false,
            ),
            stp: self.attr_supported(
                object::SAI_OBJECT_TYPE_STP,
                ffi::_sai_stp_attr_t::SAI_STP_ATTR_VLAN_LIST,
                false,
            ),
            fdb_flush,
            fdb_aging: self.attr_supported(
                object::SAI_OBJECT_TYPE_SWITCH,
                ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_FDB_AGING_TIME,
                true,
            ),
            l2mc: self.attr_supported(
                object::SAI_OBJECT_TYPE_L2MC_GROUP,
                ffi::_sai_l2mc_group_attr_t::SAI_L2MC_GROUP_ATTR_L2MC_OUTPUT_COUNT,
                false,
            ),
            storm_control: policer_fns
                && self.attr_supported(
                    object::SAI_OBJECT_TYPE_PORT,
                    ffi::_sai_port_attr_t::SAI_PORT_ATTR_BROADCAST_STORM_CONTROL_POLICER_ID,
                    true,
                ),
            mirror,
            mirror_sessions_max,
            port_tpid: self.attr_supported(
                object::SAI_OBJECT_TYPE_PORT,
                ffi::_sai_port_attr_t::SAI_PORT_ATTR_TPID,
                true,
            ),
            ecmp_width,
            ipv6,
            my_mac,
            acl_ingress,
            acl_egress,
            acl_entry_policer,
            port_learn_limit,
            copp,
            buffer_bytes_total,
            qos_map_ingress,
            qos_map_egress,
            wred,
            ecn,
            queue_shaper,
            wred_queue_stats,
        })
    }

    fn set_fdb_aging(&mut self, secs: u32) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        let mut attr = Self::zeroed_attr(ffi::_sai_switch_attr_t::SAI_SWITCH_ATTR_FDB_AGING_TIME);
        attr.value.u32_ = secs;
        // SAFETY: valid switch api table; attr outlives the call.
        unsafe {
            let set = (*self.switch_api)
                .set_switch_attribute
                .ok_or(SaiError::Other(
                    "switch api lacks set_switch_attribute".into(),
                ))?;
            check("set_switch_attribute(FDB_AGING_TIME)", set(switch, &attr))
        }
    }

    fn add_fdb_entry(
        &mut self,
        vlan: Option<Oid>,
        mac: [u8; 6],
        action: FdbAction,
    ) -> Result<(), SaiError> {
        let entry = self.fdb_entry(vlan, mac)?;
        // SAFETY per block below: valid fdb api table; buffers outlive
        // the calls.
        let create = unsafe {
            (*self.fdb_api)
                .create_fdb_entry
                .ok_or(SaiError::Other("fdb api lacks create_fdb_entry".into()))?
        };
        let remove = unsafe { (*self.fdb_api).remove_fdb_entry };

        use ffi::_sai_fdb_entry_attr_t as attr;
        let mut type_attr = Self::zeroed_attr(attr::SAI_FDB_ENTRY_ATTR_TYPE);
        type_attr.value.s32 = ffi::_sai_fdb_entry_type_t::SAI_FDB_ENTRY_TYPE_STATIC as i32;
        let mut action_attr = Self::zeroed_attr(attr::SAI_FDB_ENTRY_ATTR_PACKET_ACTION);
        let mut attrs = vec![type_attr];
        match action {
            FdbAction::Forward(port) => {
                let bridge_port = self.bridge_port_of(port)?;
                action_attr.value.s32 = ffi::_sai_packet_action_t::SAI_PACKET_ACTION_FORWARD as i32;
                let mut bp_attr = Self::zeroed_attr(attr::SAI_FDB_ENTRY_ATTR_BRIDGE_PORT_ID);
                bp_attr.value.oid = bridge_port;
                attrs.push(action_attr);
                attrs.push(bp_attr);
            }
            FdbAction::Drop => {
                action_attr.value.s32 = ffi::_sai_packet_action_t::SAI_PACKET_ACTION_DROP as i32;
                attrs.push(action_attr);
            }
        }
        // Replace semantics: clear any previous entry for this key first
        // (a dynamic learn or an earlier static).
        if let Some(remove) = remove {
            // SAFETY: entry outlives the call; absence is fine.
            let _ = unsafe { remove(&entry) };
        }
        // SAFETY: entry + attr array outlive the call.
        unsafe {
            check(
                "create_fdb_entry",
                create(&entry, attrs.len() as u32, attrs.as_ptr()),
            )
        }
    }

    fn remove_fdb_entry(&mut self, vlan: Option<Oid>, mac: [u8; 6]) -> Result<(), SaiError> {
        let entry = self.fdb_entry(vlan, mac)?;
        // SAFETY: valid fdb api table; entry outlives the call.
        unsafe {
            let remove = (*self.fdb_api)
                .remove_fdb_entry
                .ok_or(SaiError::Other("fdb api lacks remove_fdb_entry".into()))?;
            check("remove_fdb_entry", remove(&entry))
        }
    }

    fn flush_fdb(&mut self, vlan: Option<Oid>, port: Option<PortId>) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid fdb api table; attr array outlives the call.
        let flush = unsafe {
            (*self.fdb_api)
                .flush_fdb_entries
                .ok_or(SaiError::Other("fdb api lacks flush_fdb_entries".into()))?
        };
        use ffi::_sai_fdb_flush_attr_t as attr;
        let mut type_attr = Self::zeroed_attr(attr::SAI_FDB_FLUSH_ATTR_ENTRY_TYPE);
        type_attr.value.s32 =
            ffi::_sai_fdb_flush_entry_type_t::SAI_FDB_FLUSH_ENTRY_TYPE_DYNAMIC as i32;
        let mut attrs = vec![type_attr];
        if let Some(vlan) = vlan {
            let mut bv_attr = Self::zeroed_attr(attr::SAI_FDB_FLUSH_ATTR_BV_ID);
            bv_attr.value.oid = vlan.0;
            attrs.push(bv_attr);
        }
        if let Some(port) = port {
            let bridge_port = self.bridge_port_of(port)?;
            let mut bp_attr = Self::zeroed_attr(attr::SAI_FDB_FLUSH_ATTR_BRIDGE_PORT_ID);
            bp_attr.value.oid = bridge_port;
            attrs.push(bp_attr);
        }
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "flush_fdb_entries",
                flush(switch, attrs.len() as u32, attrs.as_ptr()),
            )
        }
    }

    fn set_port_storm_control(
        &mut self,
        port: PortId,
        class: StormClass,
        kbps: Option<u64>,
    ) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        let existing = self.storm_policers.get(&(port.0, class)).copied();
        match (kbps, existing) {
            (None, None) => Ok(()),
            (None, Some(policer)) => {
                // Detach, then drop the policer object.
                self.set_storm_policer_attr(port, class, 0)?;
                // SAFETY: valid policer api table.
                unsafe {
                    let remove = (*self.policer_api)
                        .remove_policer
                        .ok_or(SaiError::Other("policer api lacks remove_policer".into()))?;
                    check("remove_policer", remove(policer))?;
                }
                self.storm_policers.remove(&(port.0, class));
                Ok(())
            }
            (Some(kbps), Some(policer)) => {
                // Rate change: update CIR in place.
                let mut attr = Self::zeroed_attr(ffi::_sai_policer_attr_t::SAI_POLICER_ATTR_CIR);
                attr.value.u64_ = kbps * 1000 / 8; // bytes/sec
                                                   // SAFETY: valid policer api table; attr outlives the call.
                unsafe {
                    let set = (*self.policer_api)
                        .set_policer_attribute
                        .ok_or(SaiError::Other(
                            "policer api lacks set_policer_attribute".into(),
                        ))?;
                    check("set_policer_attribute(CIR)", set(policer, &attr))
                }
            }
            (Some(kbps), None) => {
                // SAFETY: valid policer api table; attr array outlives
                // the call.
                let create = unsafe {
                    (*self.policer_api)
                        .create_policer
                        .ok_or(SaiError::Other("policer api lacks create_policer".into()))?
                };
                use ffi::_sai_policer_attr_t as attr;
                let mut meter_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_METER_TYPE);
                meter_attr.value.s32 = ffi::_sai_meter_type_t::SAI_METER_TYPE_BYTES as i32;
                let mut mode_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_MODE);
                mode_attr.value.s32 =
                    ffi::_sai_policer_mode_t::SAI_POLICER_MODE_STORM_CONTROL as i32;
                let mut cir_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_CIR);
                cir_attr.value.u64_ = kbps * 1000 / 8; // bytes/sec
                let mut red_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_RED_PACKET_ACTION);
                red_attr.value.s32 = ffi::_sai_packet_action_t::SAI_PACKET_ACTION_DROP as i32;
                let attrs = [meter_attr, mode_attr, cir_attr, red_attr];
                let mut oid: ffi::sai_object_id_t = 0;
                // SAFETY: attr array outlives the call.
                unsafe {
                    check(
                        "create_policer(STORM_CONTROL)",
                        create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
                    )?;
                }
                if let Err(err) = self.set_storm_policer_attr(port, class, oid) {
                    // Roll the orphan policer back before surfacing.
                    // SAFETY: valid policer api table.
                    unsafe {
                        if let Some(remove) = (*self.policer_api).remove_policer {
                            let _ = remove(oid);
                        }
                    }
                    return Err(err);
                }
                self.storm_policers.insert((port.0, class), oid);
                Ok(())
            }
        }
    }

    fn port_storm_drops(&mut self, port: PortId, class: StormClass) -> Result<u64, SaiError> {
        self.switch_oid()?;
        let Some(policer) = self.storm_policers.get(&(port.0, class)).copied() else {
            return Ok(0);
        };
        // SAFETY: valid policer api table; buffers outlive the call.
        let get_stats = unsafe {
            (*self.policer_api)
                .get_policer_stats
                .ok_or(SaiError::Other(
                    "policer api lacks get_policer_stats".into(),
                ))?
        };
        let ids = [ffi::_sai_policer_stat_t::SAI_POLICER_STAT_RED_PACKETS as ffi::sai_stat_id_t];
        let mut values = [0u64; 1];
        // SAFETY: id and value buffers sized identically.
        unsafe {
            check(
                "get_policer_stats(RED_PACKETS)",
                get_stats(policer, ids.len() as u32, ids.as_ptr(), values.as_mut_ptr()),
            )?;
        }
        Ok(values[0])
    }

    fn create_mirror_session(&mut self, monitor: PortId) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid mirror api table; attr array outlives the call.
        let create = unsafe {
            (*self.mirror_api)
                .create_mirror_session
                .ok_or(SaiError::Other(
                    "mirror api lacks create_mirror_session".into(),
                ))?
        };
        use ffi::_sai_mirror_session_attr_t as attr;
        let mut type_attr = Self::zeroed_attr(attr::SAI_MIRROR_SESSION_ATTR_TYPE);
        type_attr.value.s32 = ffi::_sai_mirror_session_type_t::SAI_MIRROR_SESSION_TYPE_LOCAL as i32;
        let mut monitor_attr = Self::zeroed_attr(attr::SAI_MIRROR_SESSION_ATTR_MONITOR_PORT);
        monitor_attr.value.oid = monitor.0;
        let attrs = [type_attr, monitor_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_mirror_session(LOCAL)",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_mirror_session(&mut self, session: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid mirror api table.
        unsafe {
            let remove = (*self.mirror_api)
                .remove_mirror_session
                .ok_or(SaiError::Other(
                    "mirror api lacks remove_mirror_session".into(),
                ))?;
            check("remove_mirror_session", remove(session.0))
        }
    }

    fn set_port_mirror(
        &mut self,
        port: PortId,
        ingress: Option<Oid>,
        egress: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid port api table; attr + list outlive each call.
        let set = unsafe {
            (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?
        };
        for (id, call, session) in [
            (
                ffi::_sai_port_attr_t::SAI_PORT_ATTR_INGRESS_MIRROR_SESSION,
                "set_port_attribute(INGRESS_MIRROR_SESSION)",
                ingress,
            ),
            (
                ffi::_sai_port_attr_t::SAI_PORT_ATTR_EGRESS_MIRROR_SESSION,
                "set_port_attribute(EGRESS_MIRROR_SESSION)",
                egress,
            ),
        ] {
            let mut list: Vec<ffi::sai_object_id_t> = session.map(|s| s.0).into_iter().collect();
            let mut attr = Self::zeroed_attr(id);
            attr.value.objlist.count = list.len() as u32;
            attr.value.objlist.list = list.as_mut_ptr();
            // SAFETY: list buffer alive across the call.
            unsafe {
                check(call, set(port.0, &attr))?;
            }
        }
        Ok(())
    }

    fn set_port_tpid(&mut self, port: PortId, tpid: u16) -> Result<(), SaiError> {
        self.switch_oid()?;
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_TPID);
        attr.value.u16_ = tpid;
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check("set_port_attribute(TPID)", set(port.0, &attr))
        }
    }

    fn create_lag(&mut self) -> Result<PortId, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid lag api table.
        let create = unsafe {
            (*self.lag_api)
                .create_lag
                .ok_or(SaiError::Other("lag api lacks create_lag".into()))?
        };
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: no attrs; oid written by the call.
        unsafe {
            check("create_lag", create(&mut oid, switch, 0, std::ptr::null()))?;
        }
        self.lags.insert(oid);
        let lag = PortId(oid);
        // Front the LAG with a bridge port + default-VLAN membership so
        // it behaves like a boot-time port for the L2 program.
        if let Err(err) = self.attach_to_bridge(lag) {
            // Roll back the orphan LAG before surfacing.
            // SAFETY: valid lag api table.
            unsafe {
                if let Some(remove) = (*self.lag_api).remove_lag {
                    let _ = remove(oid);
                }
            }
            self.lags.remove(&oid);
            return Err(err);
        }
        Ok(lag)
    }

    fn remove_lag(&mut self, lag: PortId) -> Result<(), SaiError> {
        self.switch_oid()?;
        self.detach_from_bridge(lag)?;
        // SAFETY: valid lag api table.
        unsafe {
            let remove = (*self.lag_api)
                .remove_lag
                .ok_or(SaiError::Other("lag api lacks remove_lag".into()))?;
            check("remove_lag", remove(lag.0))?;
        }
        self.lags.remove(&lag.0);
        Ok(())
    }

    fn add_lag_member(&mut self, lag: PortId, port: PortId) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // The member's traffic rides the LAG's bridge port from now on.
        self.detach_from_bridge(port)?;
        // SAFETY: valid lag api table; attr array outlives the call.
        let create = unsafe {
            (*self.lag_api)
                .create_lag_member
                .ok_or(SaiError::Other("lag api lacks create_lag_member".into()))?
        };
        use ffi::_sai_lag_member_attr_t as attr;
        let mut lag_attr = Self::zeroed_attr(attr::SAI_LAG_MEMBER_ATTR_LAG_ID);
        lag_attr.value.oid = lag.0;
        let mut port_attr = Self::zeroed_attr(attr::SAI_LAG_MEMBER_ATTR_PORT_ID);
        port_attr.value.oid = port.0;
        // Members start gated closed: LACP opens them when the partner
        // agrees to collect/distribute.
        let mut egress_attr = Self::zeroed_attr(attr::SAI_LAG_MEMBER_ATTR_EGRESS_DISABLE);
        egress_attr.value.booldata = true;
        let mut ingress_attr = Self::zeroed_attr(attr::SAI_LAG_MEMBER_ATTR_INGRESS_DISABLE);
        ingress_attr.value.booldata = true;
        let attrs = [lag_attr, port_attr, egress_attr, ingress_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_lag_member",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_lag_member(&mut self, member: Oid, port: PortId) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid lag api table.
        unsafe {
            let remove = (*self.lag_api)
                .remove_lag_member
                .ok_or(SaiError::Other("lag api lacks remove_lag_member".into()))?;
            check("remove_lag_member", remove(member.0))?;
        }
        // Standalone default L2 again.
        self.attach_to_bridge(port)
    }

    fn set_lag_member_state(&mut self, member: Oid, enabled: bool) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid lag api table; attrs outlive the calls.
        let set = unsafe {
            (*self.lag_api)
                .set_lag_member_attribute
                .ok_or(SaiError::Other(
                    "lag api lacks set_lag_member_attribute".into(),
                ))?
        };
        use ffi::_sai_lag_member_attr_t as attr;
        for id in [
            attr::SAI_LAG_MEMBER_ATTR_EGRESS_DISABLE,
            attr::SAI_LAG_MEMBER_ATTR_INGRESS_DISABLE,
        ] {
            let mut a = Self::zeroed_attr(id);
            a.value.booldata = !enabled;
            // SAFETY: attr outlives the call.
            unsafe {
                check("set_lag_member_attribute(DISABLE)", set(member.0, &a))?;
            }
        }
        Ok(())
    }

    fn create_stp_instance(&mut self) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid stp api table.
        let create = unsafe {
            (*self.stp_api)
                .create_stp
                .ok_or(SaiError::Other("stp api lacks create_stp".into()))?
        };
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: no attrs; oid written by the call.
        unsafe {
            check("create_stp", create(&mut oid, switch, 0, std::ptr::null()))?;
        }
        Ok(Oid(oid))
    }

    fn remove_stp_instance(&mut self, stp: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // Drop its stp-port objects first.
        let ports: Vec<((u64, u64), ffi::sai_object_id_t)> = self
            .stp_ports
            .iter()
            .filter(|((instance, _), _)| *instance == stp.0)
            .map(|(key, oid)| (*key, *oid))
            .collect();
        // SAFETY per block below: valid stp api table.
        let remove_port = unsafe { (*self.stp_api).remove_stp_port };
        for (key, oid) in ports {
            if let Some(remove_port) = remove_port {
                // SAFETY: oid from our own map.
                let _ = unsafe { remove_port(oid) };
            }
            self.stp_ports.remove(&key);
        }
        // SAFETY: valid stp api table.
        unsafe {
            let remove = (*self.stp_api)
                .remove_stp
                .ok_or(SaiError::Other("stp api lacks remove_stp".into()))?;
            check("remove_stp", remove(stp.0))
        }
    }

    fn set_vlan_stp_instance(
        &mut self,
        vlan: Option<Oid>,
        stp: Option<Oid>,
    ) -> Result<(), SaiError> {
        let defaults = self.defaults()?;
        let vlan_oid = vlan.map(|v| v.0).unwrap_or(defaults.vlan);
        let stp_oid = match stp {
            Some(stp) => stp.0,
            None if defaults.stp != 0 => defaults.stp,
            None => {
                return Err(SaiError::Other(
                    "switch reports no default STP instance".into(),
                ));
            }
        };
        let mut attr = Self::zeroed_attr(ffi::_sai_vlan_attr_t::SAI_VLAN_ATTR_STP_INSTANCE);
        attr.value.oid = stp_oid;
        // SAFETY: valid vlan api table; attr outlives the call.
        unsafe {
            let set = (*self.vlan_api)
                .set_vlan_attribute
                .ok_or(SaiError::Other("vlan api lacks set_vlan_attribute".into()))?;
            check("set_vlan_attribute(STP_INSTANCE)", set(vlan_oid, &attr))
        }
    }

    fn set_stp_port_state(
        &mut self,
        stp: Option<Oid>,
        port: PortId,
        state: StpPortState,
    ) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        let defaults = self.defaults()?;
        let stp_oid = match stp {
            Some(stp) => stp.0,
            None if defaults.stp != 0 => defaults.stp,
            None => {
                return Err(SaiError::Other(
                    "switch reports no default STP instance".into(),
                ));
            }
        };
        let state_value = match state {
            StpPortState::Blocking => ffi::_sai_stp_port_state_t::SAI_STP_PORT_STATE_BLOCKING,
            StpPortState::Learning => ffi::_sai_stp_port_state_t::SAI_STP_PORT_STATE_LEARNING,
            StpPortState::Forwarding => ffi::_sai_stp_port_state_t::SAI_STP_PORT_STATE_FORWARDING,
        } as i32;

        if let Some(existing) = self.stp_ports.get(&(stp_oid, port.0)).copied() {
            let mut attr = Self::zeroed_attr(ffi::_sai_stp_port_attr_t::SAI_STP_PORT_ATTR_STATE);
            attr.value.s32 = state_value;
            // SAFETY: valid stp api table; attr outlives the call.
            return unsafe {
                let set = (*self.stp_api)
                    .set_stp_port_attribute
                    .ok_or(SaiError::Other(
                        "stp api lacks set_stp_port_attribute".into(),
                    ))?;
                check("set_stp_port_attribute(STATE)", set(existing, &attr))
            };
        }

        let bridge_port = self.bridge_port_of(port)?;
        // SAFETY: valid stp api table; attr array outlives the call.
        let create = unsafe {
            (*self.stp_api)
                .create_stp_port
                .ok_or(SaiError::Other("stp api lacks create_stp_port".into()))?
        };
        use ffi::_sai_stp_port_attr_t as attr;
        let mut stp_attr = Self::zeroed_attr(attr::SAI_STP_PORT_ATTR_STP);
        stp_attr.value.oid = stp_oid;
        let mut bp_attr = Self::zeroed_attr(attr::SAI_STP_PORT_ATTR_BRIDGE_PORT);
        bp_attr.value.oid = bridge_port;
        let mut state_attr = Self::zeroed_attr(attr::SAI_STP_PORT_ATTR_STATE);
        state_attr.value.s32 = state_value;
        let attrs = [stp_attr, bp_attr, state_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_stp_port",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        self.stp_ports.insert((stp_oid, port.0), oid);
        Ok(())
    }

    fn create_l2mc_group(&mut self) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid l2mc-group api table.
        let create = unsafe {
            (*self.l2mc_group_api)
                .create_l2mc_group
                .ok_or(SaiError::Other(
                    "l2mc-group api lacks create_l2mc_group".into(),
                ))?
        };
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: no attrs; oid written by the call.
        unsafe {
            check(
                "create_l2mc_group",
                create(&mut oid, switch, 0, std::ptr::null()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_l2mc_group(&mut self, group: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid l2mc-group api table.
        unsafe {
            let remove = (*self.l2mc_group_api)
                .remove_l2mc_group
                .ok_or(SaiError::Other(
                    "l2mc-group api lacks remove_l2mc_group".into(),
                ))?;
            check("remove_l2mc_group", remove(group.0))
        }
    }

    fn add_l2mc_member(&mut self, group: Oid, port: PortId) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let bridge_port = self.bridge_port_of(port)?;
        // SAFETY: valid l2mc-group api table; attr array outlives the
        // call.
        let create = unsafe {
            (*self.l2mc_group_api)
                .create_l2mc_group_member
                .ok_or(SaiError::Other(
                    "l2mc-group api lacks create_l2mc_group_member".into(),
                ))?
        };
        use ffi::_sai_l2mc_group_member_attr_t as attr;
        let mut group_attr = Self::zeroed_attr(attr::SAI_L2MC_GROUP_MEMBER_ATTR_L2MC_GROUP_ID);
        group_attr.value.oid = group.0;
        let mut output_attr = Self::zeroed_attr(attr::SAI_L2MC_GROUP_MEMBER_ATTR_L2MC_OUTPUT_ID);
        output_attr.value.oid = bridge_port;
        let attrs = [group_attr, output_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_l2mc_group_member",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_l2mc_member(&mut self, member: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid l2mc-group api table.
        unsafe {
            let remove = (*self.l2mc_group_api)
                .remove_l2mc_group_member
                .ok_or(SaiError::Other(
                    "l2mc-group api lacks remove_l2mc_group_member".into(),
                ))?;
            check("remove_l2mc_group_member", remove(member.0))
        }
    }

    fn set_l2mc_entry(
        &mut self,
        vlan: Option<Oid>,
        group_ip: std::net::IpAddr,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        let switch = self.switch_oid()?;
        let defaults = self.defaults()?;
        // SAFETY: sai_l2mc_entry_t is POD; all-zero is a valid start.
        let mut entry: ffi::sai_l2mc_entry_t = unsafe { std::mem::zeroed() };
        entry.switch_id = switch;
        entry.bv_id = vlan.map(|v| v.0).unwrap_or(defaults.vlan);
        entry.type_ = ffi::_sai_l2mc_entry_type_t::SAI_L2MC_ENTRY_TYPE_XG;
        match group_ip {
            std::net::IpAddr::V4(v4) => {
                entry.destination.addr_family = ffi::_sai_ip_addr_family_t::SAI_IP_ADDR_FAMILY_IPV4;
                entry.destination.addr.ip4 = u32::from_ne_bytes(v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                entry.destination.addr_family = ffi::_sai_ip_addr_family_t::SAI_IP_ADDR_FAMILY_IPV6;
                entry.destination.addr.ip6 = v6.octets();
            }
        }
        match l2mc {
            Some(group) => {
                // SAFETY: valid l2mc api table; entry + attrs outlive
                // the call.
                let create = unsafe {
                    (*self.l2mc_api)
                        .create_l2mc_entry
                        .ok_or(SaiError::Other("l2mc api lacks create_l2mc_entry".into()))?
                };
                let remove = unsafe { (*self.l2mc_api).remove_l2mc_entry };
                use ffi::_sai_l2mc_entry_attr_t as attr;
                let mut action_attr = Self::zeroed_attr(attr::SAI_L2MC_ENTRY_ATTR_PACKET_ACTION);
                action_attr.value.s32 = ffi::_sai_packet_action_t::SAI_PACKET_ACTION_FORWARD as i32;
                let mut group_attr = Self::zeroed_attr(attr::SAI_L2MC_ENTRY_ATTR_OUTPUT_GROUP_ID);
                group_attr.value.oid = group.0;
                let attrs = [action_attr, group_attr];
                // Replace semantics: drop any previous entry for the key.
                if let Some(remove) = remove {
                    // SAFETY: entry outlives the call; absence is fine.
                    let _ = unsafe { remove(&entry) };
                }
                // SAFETY: entry + attr array outlive the call.
                unsafe {
                    check(
                        "create_l2mc_entry",
                        create(&entry, attrs.len() as u32, attrs.as_ptr()),
                    )
                }
            }
            None => {
                // SAFETY: valid l2mc api table; entry outlives the call.
                unsafe {
                    let remove = (*self.l2mc_api)
                        .remove_l2mc_entry
                        .ok_or(SaiError::Other("l2mc api lacks remove_l2mc_entry".into()))?;
                    check("remove_l2mc_entry", remove(&entry))
                }
            }
        }
    }

    fn set_vlan_unknown_mcast_group(
        &mut self,
        vlan: Option<Oid>,
        l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        let defaults = self.defaults()?;
        let vlan_oid = vlan.map(|v| v.0).unwrap_or(defaults.vlan);
        // SAFETY: valid vlan api table; attrs outlive the calls.
        let set = unsafe {
            (*self.vlan_api)
                .set_vlan_attribute
                .ok_or(SaiError::Other("vlan api lacks set_vlan_attribute".into()))?
        };
        use ffi::_sai_vlan_attr_t as attr;
        let mut type_attr =
            Self::zeroed_attr(attr::SAI_VLAN_ATTR_UNKNOWN_MULTICAST_FLOOD_CONTROL_TYPE);
        type_attr.value.s32 = match l2mc {
            Some(_) => ffi::_sai_vlan_flood_control_type_t::SAI_VLAN_FLOOD_CONTROL_TYPE_L2MC_GROUP,
            None => ffi::_sai_vlan_flood_control_type_t::SAI_VLAN_FLOOD_CONTROL_TYPE_ALL,
        } as i32;
        if let Some(group) = l2mc {
            let mut group_attr =
                Self::zeroed_attr(attr::SAI_VLAN_ATTR_UNKNOWN_MULTICAST_FLOOD_GROUP);
            group_attr.value.oid = group.0;
            // The group first, then the control type that references it.
            // SAFETY: attrs outlive the calls.
            unsafe {
                check(
                    "set_vlan_attribute(UNKNOWN_MULTICAST_FLOOD_GROUP)",
                    set(vlan_oid, &group_attr),
                )?;
                check(
                    "set_vlan_attribute(UNKNOWN_MULTICAST_FLOOD_CONTROL_TYPE)",
                    set(vlan_oid, &type_attr),
                )
            }
        } else {
            // SAFETY: attr outlives the call.
            unsafe {
                check(
                    "set_vlan_attribute(UNKNOWN_MULTICAST_FLOOD_CONTROL_TYPE)",
                    set(vlan_oid, &type_attr),
                )
            }
        }
    }

    fn create_acl_table(&mut self, stage: AclStage, family: AclFamily) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let api = self.acl_api()?;
        // SAFETY: valid acl api table; buffers outlive the call.
        let create = unsafe {
            (*api)
                .create_acl_table
                .ok_or(SaiError::Other("acl api lacks create_acl_table".into()))?
        };
        use ffi::_sai_acl_table_attr_t as attr;
        let mut attrs: Vec<ffi::sai_attribute_t> = Vec::new();

        let mut stage_attr = Self::zeroed_attr(attr::SAI_ACL_TABLE_ATTR_ACL_STAGE);
        stage_attr.value.s32 = match stage {
            AclStage::Ingress => ffi::_sai_acl_stage_t::SAI_ACL_STAGE_INGRESS,
            AclStage::Egress => ffi::_sai_acl_stage_t::SAI_ACL_STAGE_EGRESS,
        } as i32;
        attrs.push(stage_attr);

        let mut bind_points =
            [ffi::_sai_acl_bind_point_type_t::SAI_ACL_BIND_POINT_TYPE_PORT as i32];
        let mut bind_attr = Self::zeroed_attr(attr::SAI_ACL_TABLE_ATTR_ACL_BIND_POINT_TYPE_LIST);
        bind_attr.value.s32list.count = bind_points.len() as u32;
        bind_attr.value.s32list.list = bind_points.as_mut_ptr();
        attrs.push(bind_attr);

        // The family's match-field set. Every family carries the outer
        // VLAN id (internal snooping/DAI entries scope by VLAN).
        let field_ids: &[u32] = match family {
            AclFamily::Ipv4 => &[
                attr::SAI_ACL_TABLE_ATTR_FIELD_SRC_IP,
                attr::SAI_ACL_TABLE_ATTR_FIELD_DST_IP,
                attr::SAI_ACL_TABLE_ATTR_FIELD_IP_PROTOCOL,
                attr::SAI_ACL_TABLE_ATTR_FIELD_L4_SRC_PORT,
                attr::SAI_ACL_TABLE_ATTR_FIELD_L4_DST_PORT,
                attr::SAI_ACL_TABLE_ATTR_FIELD_DSCP,
                attr::SAI_ACL_TABLE_ATTR_FIELD_OUTER_VLAN_ID,
                attr::SAI_ACL_TABLE_ATTR_FIELD_ETHER_TYPE,
            ],
            AclFamily::Ipv6 => &[
                attr::SAI_ACL_TABLE_ATTR_FIELD_SRC_IPV6,
                attr::SAI_ACL_TABLE_ATTR_FIELD_DST_IPV6,
                attr::SAI_ACL_TABLE_ATTR_FIELD_IP_PROTOCOL,
                attr::SAI_ACL_TABLE_ATTR_FIELD_L4_SRC_PORT,
                attr::SAI_ACL_TABLE_ATTR_FIELD_L4_DST_PORT,
                attr::SAI_ACL_TABLE_ATTR_FIELD_DSCP,
                attr::SAI_ACL_TABLE_ATTR_FIELD_OUTER_VLAN_ID,
            ],
            AclFamily::Mac => &[
                attr::SAI_ACL_TABLE_ATTR_FIELD_SRC_MAC,
                attr::SAI_ACL_TABLE_ATTR_FIELD_DST_MAC,
                attr::SAI_ACL_TABLE_ATTR_FIELD_ETHER_TYPE,
                attr::SAI_ACL_TABLE_ATTR_FIELD_OUTER_VLAN_ID,
            ],
        };
        for id in field_ids {
            let mut field = Self::zeroed_attr(*id);
            field.value.booldata = true;
            attrs.push(field);
        }

        // L4 port ranges ride ACL range objects; the table declares the
        // range types it accepts.
        let mut range_types = [
            ffi::_sai_acl_range_type_t::SAI_ACL_RANGE_TYPE_L4_SRC_PORT_RANGE as i32,
            ffi::_sai_acl_range_type_t::SAI_ACL_RANGE_TYPE_L4_DST_PORT_RANGE as i32,
        ];
        if family != AclFamily::Mac {
            let mut range_attr = Self::zeroed_attr(attr::SAI_ACL_TABLE_ATTR_FIELD_ACL_RANGE_TYPE);
            range_attr.value.s32list.count = range_types.len() as u32;
            range_attr.value.s32list.list = range_types.as_mut_ptr();
            attrs.push(range_attr);
        }

        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array and its list buffers outlive the call.
        unsafe {
            check(
                "create_acl_table",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        self.acl_tables.insert(oid, stage);
        Ok(Oid(oid))
    }

    fn remove_acl_table(&mut self, table: Oid) -> Result<(), SaiError> {
        let api = self.acl_api()?;
        // SAFETY: valid acl api table.
        unsafe {
            let remove = (*api)
                .remove_acl_table
                .ok_or(SaiError::Other("acl api lacks remove_acl_table".into()))?;
            check("remove_acl_table", remove(table.0))?;
        }
        self.acl_tables.remove(&table.0);
        Ok(())
    }

    fn create_acl_entry(
        &mut self,
        table: Oid,
        priority: u32,
        fields: &AclFields,
        action: &AclAction,
    ) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let api = self.acl_api()?;
        // SAFETY per block below: valid acl api table; buffers outlive
        // the calls.
        let create = unsafe {
            (*api)
                .create_acl_entry
                .ok_or(SaiError::Other("acl api lacks create_acl_entry".into()))?
        };
        use ffi::_sai_acl_entry_attr_t as attr;
        let mut attrs: Vec<ffi::sai_attribute_t> = Vec::new();

        let mut table_attr = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_TABLE_ID);
        table_attr.value.oid = table.0;
        attrs.push(table_attr);
        let mut prio_attr = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_PRIORITY);
        prio_attr.value.u32_ = priority;
        attrs.push(prio_attr);

        let ip_field = |id: u32, ip: IpPrefix| {
            let mut a = Self::zeroed_attr(id);
            a.value.aclfield.enable = true;
            match ip.0 {
                std::net::IpAddr::V4(v4) => {
                    let mask: u32 = if ip.1 == 0 {
                        0
                    } else {
                        u32::MAX << (32 - ip.1)
                    };
                    a.value.aclfield.data.ip4 = u32::from_ne_bytes(v4.octets());
                    a.value.aclfield.mask.ip4 = mask.to_be();
                }
                std::net::IpAddr::V6(v6) => {
                    let mask: u128 = if ip.1 == 0 {
                        0
                    } else {
                        u128::MAX << (128 - ip.1)
                    };
                    a.value.aclfield.data.ip6 = v6.octets();
                    a.value.aclfield.mask.ip6 = mask.to_be_bytes();
                }
            }
            a
        };
        if let Some(src) = fields.src_ip {
            let id = if src.0.is_ipv4() {
                attr::SAI_ACL_ENTRY_ATTR_FIELD_SRC_IP
            } else {
                attr::SAI_ACL_ENTRY_ATTR_FIELD_SRC_IPV6
            };
            attrs.push(ip_field(id, src));
        }
        if let Some(dst) = fields.dst_ip {
            let id = if dst.0.is_ipv4() {
                attr::SAI_ACL_ENTRY_ATTR_FIELD_DST_IP
            } else {
                attr::SAI_ACL_ENTRY_ATTR_FIELD_DST_IPV6
            };
            attrs.push(ip_field(id, dst));
        }
        if let Some(protocol) = fields.protocol {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_FIELD_IP_PROTOCOL);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.u8_ = protocol;
            a.value.aclfield.mask.u8_ = 0xff;
            attrs.push(a);
        }
        if let Some(dscp) = fields.dscp {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_FIELD_DSCP);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.u8_ = dscp;
            a.value.aclfield.mask.u8_ = 0x3f;
            attrs.push(a);
        }
        if let Some((mac, mask)) = fields.src_mac {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_FIELD_SRC_MAC);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.mac = mac;
            a.value.aclfield.mask.mac = mask;
            attrs.push(a);
        }
        if let Some((mac, mask)) = fields.dst_mac {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_FIELD_DST_MAC);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.mac = mac;
            a.value.aclfield.mask.mac = mask;
            attrs.push(a);
        }
        if let Some(ethertype) = fields.ethertype {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_FIELD_ETHER_TYPE);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.u16_ = ethertype;
            a.value.aclfield.mask.u16_ = 0xffff;
            attrs.push(a);
        }
        if let Some(vlan) = fields.vlan {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_FIELD_OUTER_VLAN_ID);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.u16_ = vlan;
            a.value.aclfield.mask.u16_ = 0xfff;
            attrs.push(a);
        }

        // Exact L4 ports use the u16 field; real ranges become ACL
        // range objects referenced from the entry (removed with it).
        let mut range_oids: Vec<ffi::sai_object_id_t> = Vec::new();
        let exact_port = |attrs: &mut Vec<ffi::sai_attribute_t>, id: u32, port: u16| {
            let mut a = Self::zeroed_attr(id);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.u16_ = port;
            a.value.aclfield.mask.u16_ = 0xffff;
            attrs.push(a);
        };
        for (ports, exact_id, range_type) in [
            (
                fields.src_port,
                attr::SAI_ACL_ENTRY_ATTR_FIELD_L4_SRC_PORT,
                ffi::_sai_acl_range_type_t::SAI_ACL_RANGE_TYPE_L4_SRC_PORT_RANGE,
            ),
            (
                fields.dst_port,
                attr::SAI_ACL_ENTRY_ATTR_FIELD_L4_DST_PORT,
                ffi::_sai_acl_range_type_t::SAI_ACL_RANGE_TYPE_L4_DST_PORT_RANGE,
            ),
        ] {
            let Some((low, high)) = ports else { continue };
            if low == high {
                exact_port(&mut attrs, exact_id, low);
                continue;
            }
            // SAFETY: valid acl api table; attrs outlive the call.
            let create_range = unsafe {
                (*api)
                    .create_acl_range
                    .ok_or(SaiError::Other("acl api lacks create_acl_range".into()))?
            };
            use ffi::_sai_acl_range_attr_t as range_attr;
            let mut type_attr = Self::zeroed_attr(range_attr::SAI_ACL_RANGE_ATTR_TYPE);
            type_attr.value.s32 = range_type as i32;
            let mut limit_attr = Self::zeroed_attr(range_attr::SAI_ACL_RANGE_ATTR_LIMIT);
            limit_attr.value.u32range.min = low as u32;
            limit_attr.value.u32range.max = high as u32;
            let range_attrs = [type_attr, limit_attr];
            let mut range_oid: ffi::sai_object_id_t = 0;
            // SAFETY: attr array outlives the call.
            unsafe {
                check(
                    "create_acl_range",
                    create_range(
                        &mut range_oid,
                        switch,
                        range_attrs.len() as u32,
                        range_attrs.as_ptr(),
                    ),
                )?;
            }
            range_oids.push(range_oid);
        }
        if !range_oids.is_empty() {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_FIELD_ACL_RANGE_TYPE);
            a.value.aclfield.enable = true;
            a.value.aclfield.data.objlist.count = range_oids.len() as u32;
            a.value.aclfield.data.objlist.list = range_oids.as_mut_ptr();
            attrs.push(a);
        }

        let mut action_attr = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_ACTION_PACKET_ACTION);
        action_attr.value.aclaction.enable = true;
        action_attr.value.aclaction.parameter.s32 = Self::sai_packet_action(action.action);
        attrs.push(action_attr);
        if let Some(counter) = action.counter {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_ACTION_COUNTER);
            a.value.aclaction.enable = true;
            a.value.aclaction.parameter.oid = counter.0;
            attrs.push(a);
        }
        if let Some(policer) = action.policer {
            let mut a = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_ACTION_SET_POLICER);
            a.value.aclaction.enable = true;
            a.value.aclaction.parameter.oid = policer.0;
            attrs.push(a);
        }

        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array and its list buffers outlive the call.
        let created = unsafe {
            check(
                "create_acl_entry",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )
        };
        if let Err(err) = created {
            // Roll orphan range objects back before surfacing.
            // SAFETY: valid acl api table.
            unsafe {
                if let Some(remove_range) = (*api).remove_acl_range {
                    for range in range_oids {
                        let _ = remove_range(range);
                    }
                }
            }
            return Err(err);
        }
        if !range_oids.is_empty() {
            self.acl_entry_ranges.insert(oid, range_oids);
        }
        Ok(Oid(oid))
    }

    fn set_acl_entry_action(&mut self, entry: Oid, action: &AclAction) -> Result<(), SaiError> {
        let api = self.acl_api()?;
        // SAFETY: valid acl api table; attrs outlive the calls.
        let set = unsafe {
            (*api).set_acl_entry_attribute.ok_or(SaiError::Other(
                "acl api lacks set_acl_entry_attribute".into(),
            ))?
        };
        use ffi::_sai_acl_entry_attr_t as attr;
        let mut action_attr = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_ACTION_PACKET_ACTION);
        action_attr.value.aclaction.enable = true;
        action_attr.value.aclaction.parameter.s32 = Self::sai_packet_action(action.action);
        let mut counter_attr = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_ACTION_COUNTER);
        counter_attr.value.aclaction.enable = action.counter.is_some();
        counter_attr.value.aclaction.parameter.oid = action.counter.map(|c| c.0).unwrap_or(0);
        let mut policer_attr = Self::zeroed_attr(attr::SAI_ACL_ENTRY_ATTR_ACTION_SET_POLICER);
        policer_attr.value.aclaction.enable = action.policer.is_some();
        policer_attr.value.aclaction.parameter.oid = action.policer.map(|p| p.0).unwrap_or(0);
        // SAFETY: attrs outlive the calls.
        unsafe {
            check(
                "set_acl_entry_attribute(ACTION_PACKET_ACTION)",
                set(entry.0, &action_attr),
            )?;
            check(
                "set_acl_entry_attribute(ACTION_COUNTER)",
                set(entry.0, &counter_attr),
            )?;
            check(
                "set_acl_entry_attribute(ACTION_SET_POLICER)",
                set(entry.0, &policer_attr),
            )
        }
    }

    fn remove_acl_entry(&mut self, entry: Oid) -> Result<(), SaiError> {
        let api = self.acl_api()?;
        // SAFETY: valid acl api table.
        unsafe {
            let remove = (*api)
                .remove_acl_entry
                .ok_or(SaiError::Other("acl api lacks remove_acl_entry".into()))?;
            check("remove_acl_entry", remove(entry.0))?;
            // The entry's range objects go with it.
            if let Some(ranges) = self.acl_entry_ranges.remove(&entry.0) {
                if let Some(remove_range) = (*api).remove_acl_range {
                    for range in ranges {
                        let _ = remove_range(range);
                    }
                }
            }
        }
        Ok(())
    }

    fn create_acl_counter(&mut self, table: Oid) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let api = self.acl_api()?;
        // SAFETY: valid acl api table; attr array outlives the call.
        let create = unsafe {
            (*api)
                .create_acl_counter
                .ok_or(SaiError::Other("acl api lacks create_acl_counter".into()))?
        };
        use ffi::_sai_acl_counter_attr_t as attr;
        let mut table_attr = Self::zeroed_attr(attr::SAI_ACL_COUNTER_ATTR_TABLE_ID);
        table_attr.value.oid = table.0;
        let mut packets_attr = Self::zeroed_attr(attr::SAI_ACL_COUNTER_ATTR_ENABLE_PACKET_COUNT);
        packets_attr.value.booldata = true;
        let attrs = [table_attr, packets_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_acl_counter",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_acl_counter(&mut self, counter: Oid) -> Result<(), SaiError> {
        let api = self.acl_api()?;
        // SAFETY: valid acl api table.
        unsafe {
            let remove = (*api)
                .remove_acl_counter
                .ok_or(SaiError::Other("acl api lacks remove_acl_counter".into()))?;
            check("remove_acl_counter", remove(counter.0))
        }
    }

    fn get_acl_counter(&mut self, counter: Oid) -> Result<u64, SaiError> {
        let api = self.acl_api()?;
        // SAFETY: valid acl api table; attr outlives the call; union
        // read matches the u64 attr.
        unsafe {
            let get = (*api).get_acl_counter_attribute.ok_or(SaiError::Other(
                "acl api lacks get_acl_counter_attribute".into(),
            ))?;
            let mut attr =
                Self::zeroed_attr(ffi::_sai_acl_counter_attr_t::SAI_ACL_COUNTER_ATTR_PACKETS);
            check(
                "get_acl_counter_attribute(PACKETS)",
                get(counter.0, 1, &mut attr),
            )?;
            Ok(attr.value.u64_)
        }
    }

    fn bind_port_acl(
        &mut self,
        port: PortId,
        stage: AclStage,
        table: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.switch_oid()?;
        let id = match stage {
            AclStage::Ingress => ffi::_sai_port_attr_t::SAI_PORT_ATTR_INGRESS_ACL,
            AclStage::Egress => ffi::_sai_port_attr_t::SAI_PORT_ATTR_EGRESS_ACL,
        };
        let mut attr = Self::zeroed_attr(id);
        attr.value.oid = table.map(|t| t.0).unwrap_or(0);
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check("set_port_attribute(INGRESS/EGRESS_ACL)", set(port.0, &attr))
        }
    }

    fn acl_available_entries(&mut self, stage: AclStage) -> Result<u32, SaiError> {
        self.switch_oid()?;
        let api = self.acl_api()?;
        // Free entries are a per-table attribute; without a live table
        // at the stage the honest answer is unknown, reported as 0.
        let Some((&table, _)) = self.acl_tables.iter().find(|(_, s)| **s == stage) else {
            return Ok(0);
        };
        // SAFETY: valid acl api table; attr outlives the call; union
        // read matches the u32 attr.
        unsafe {
            let get = (*api).get_acl_table_attribute.ok_or(SaiError::Other(
                "acl api lacks get_acl_table_attribute".into(),
            ))?;
            let mut attr = Self::zeroed_attr(
                ffi::_sai_acl_table_attr_t::SAI_ACL_TABLE_ATTR_AVAILABLE_ACL_ENTRY,
            );
            if get(table, 1, &mut attr) != 0 {
                return Ok(0);
            }
            Ok(attr.value.u32_)
        }
    }

    fn create_policer(&mut self, spec: PolicerSpec) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid policer api table; attr array outlives the call.
        let create = unsafe {
            (*self.policer_api)
                .create_policer
                .ok_or(SaiError::Other("policer api lacks create_policer".into()))?
        };
        use ffi::_sai_policer_attr_t as attr;
        let mut meter_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_METER_TYPE);
        meter_attr.value.s32 = if spec.pps {
            ffi::_sai_meter_type_t::SAI_METER_TYPE_PACKETS
        } else {
            ffi::_sai_meter_type_t::SAI_METER_TYPE_BYTES
        } as i32;
        let mut mode_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_MODE);
        mode_attr.value.s32 = ffi::_sai_policer_mode_t::SAI_POLICER_MODE_SR_TCM as i32;
        let mut cir_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_CIR);
        cir_attr.value.u64_ = if spec.pps { spec.rate } else { spec.rate / 8 };
        let mut cbs_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_CBS);
        cbs_attr.value.u64_ = spec.burst;
        let mut red_attr = Self::zeroed_attr(attr::SAI_POLICER_ATTR_RED_PACKET_ACTION);
        red_attr.value.s32 = ffi::_sai_packet_action_t::SAI_PACKET_ACTION_DROP as i32;
        let attrs = [meter_attr, mode_attr, cir_attr, cbs_attr, red_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_policer(SR_TCM)",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn set_policer(&mut self, policer: Oid, spec: PolicerSpec) -> Result<(), SaiError> {
        self.switch_oid()?;
        self.set_policer_rate_attrs(policer.0, spec)
    }

    fn remove_policer(&mut self, policer: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid policer api table.
        unsafe {
            let remove = (*self.policer_api)
                .remove_policer
                .ok_or(SaiError::Other("policer api lacks remove_policer".into()))?;
            check("remove_policer", remove(policer.0))
        }
    }

    fn policer_stats(&mut self, policer: Oid) -> Result<PolicerStats, SaiError> {
        self.switch_oid()?;
        // SAFETY: valid policer api table; buffers outlive the call.
        let get_stats = unsafe {
            (*self.policer_api)
                .get_policer_stats
                .ok_or(SaiError::Other(
                    "policer api lacks get_policer_stats".into(),
                ))?
        };
        let ids = [
            ffi::_sai_policer_stat_t::SAI_POLICER_STAT_GREEN_PACKETS as ffi::sai_stat_id_t,
            ffi::_sai_policer_stat_t::SAI_POLICER_STAT_RED_PACKETS as ffi::sai_stat_id_t,
        ];
        let mut values = [0u64; 2];
        // SAFETY: id and value buffers sized identically.
        unsafe {
            check(
                "get_policer_stats(GREEN/RED_PACKETS)",
                get_stats(
                    policer.0,
                    ids.len() as u32,
                    ids.as_ptr(),
                    values.as_mut_ptr(),
                ),
            )?;
        }
        Ok(PolicerStats {
            conforming: values[0],
            dropped: values[1],
        })
    }

    fn create_hostif_trap_group(&mut self, policer: Option<Oid>) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        // SAFETY: valid hostif api table; attr array outlives the call.
        let create = unsafe {
            (*self.hostif_api)
                .create_hostif_trap_group
                .ok_or(SaiError::Other(
                    "hostif api lacks create_hostif_trap_group".into(),
                ))?
        };
        let mut attrs: Vec<ffi::sai_attribute_t> = Vec::new();
        if let Some(policer) = policer {
            let mut a = Self::zeroed_attr(
                ffi::_sai_hostif_trap_group_attr_t::SAI_HOSTIF_TRAP_GROUP_ATTR_POLICER,
            );
            a.value.oid = policer.0;
            attrs.push(a);
        }
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_hostif_trap_group",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_hostif_trap_group(&mut self, group: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        // SAFETY: valid hostif api table.
        unsafe {
            let remove = (*self.hostif_api)
                .remove_hostif_trap_group
                .ok_or(SaiError::Other(
                    "hostif api lacks remove_hostif_trap_group".into(),
                ))?;
            check("remove_hostif_trap_group", remove(group.0))
        }
    }

    fn create_hostif_trap(
        &mut self,
        kind: TrapKind,
        trap_only: bool,
        group: Oid,
    ) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let Some(trap_type) = Self::sai_trap_type(kind) else {
            // The ACL-log trap is a user-defined trap object.
            // SAFETY: valid hostif api table; attr array outlives the
            // call.
            let create = unsafe {
                (*self.hostif_api)
                    .create_hostif_user_defined_trap
                    .ok_or(SaiError::Other(
                        "hostif api lacks create_hostif_user_defined_trap".into(),
                    ))?
            };
            use ffi::_sai_hostif_user_defined_trap_attr_t as attr;
            let mut type_attr = Self::zeroed_attr(attr::SAI_HOSTIF_USER_DEFINED_TRAP_ATTR_TYPE);
            type_attr.value.s32 =
                ffi::_sai_hostif_user_defined_trap_type_t::SAI_HOSTIF_USER_DEFINED_TRAP_TYPE_ACL
                    as i32;
            let mut group_attr =
                Self::zeroed_attr(attr::SAI_HOSTIF_USER_DEFINED_TRAP_ATTR_TRAP_GROUP);
            group_attr.value.oid = if group.0 == 0 {
                self.defaults()?.trap_group
            } else {
                group.0
            };
            let attrs = [type_attr, group_attr];
            let mut oid: ffi::sai_object_id_t = 0;
            // SAFETY: attr array outlives the call.
            unsafe {
                check(
                    "create_hostif_user_defined_trap(ACL)",
                    create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
                )?;
            }
            self.user_traps.insert(oid);
            return Ok(Oid(oid));
        };
        // SAFETY: valid hostif api table; attr array outlives the call.
        let create = unsafe {
            (*self.hostif_api)
                .create_hostif_trap
                .ok_or(SaiError::Other(
                    "hostif api lacks create_hostif_trap".into(),
                ))?
        };
        use ffi::_sai_hostif_trap_attr_t as attr;
        let mut type_attr = Self::zeroed_attr(attr::SAI_HOSTIF_TRAP_ATTR_TRAP_TYPE);
        type_attr.value.s32 = trap_type as i32;
        let mut action_attr = Self::zeroed_attr(attr::SAI_HOSTIF_TRAP_ATTR_PACKET_ACTION);
        action_attr.value.s32 = if trap_only {
            ffi::_sai_packet_action_t::SAI_PACKET_ACTION_TRAP
        } else {
            ffi::_sai_packet_action_t::SAI_PACKET_ACTION_COPY
        } as i32;
        let mut group_attr = Self::zeroed_attr(attr::SAI_HOSTIF_TRAP_ATTR_TRAP_GROUP);
        // Oid(0) = the switch default trap group.
        group_attr.value.oid = if group.0 == 0 {
            self.defaults()?.trap_group
        } else {
            group.0
        };
        let attrs = [type_attr, action_attr, group_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_hostif_trap",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_hostif_trap(&mut self, trap: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        if self.user_traps.remove(&trap.0) {
            // SAFETY: valid hostif api table.
            return unsafe {
                let remove =
                    (*self.hostif_api)
                        .remove_hostif_user_defined_trap
                        .ok_or(SaiError::Other(
                            "hostif api lacks remove_hostif_user_defined_trap".into(),
                        ))?;
                check("remove_hostif_user_defined_trap", remove(trap.0))
            };
        }
        // SAFETY: valid hostif api table.
        unsafe {
            let remove = (*self.hostif_api)
                .remove_hostif_trap
                .ok_or(SaiError::Other(
                    "hostif api lacks remove_hostif_trap".into(),
                ))?;
            check("remove_hostif_trap", remove(trap.0))
        }
    }

    fn set_default_trap_group_policer(&mut self, policer: Option<Oid>) -> Result<(), SaiError> {
        let defaults = self.defaults()?;
        // SAFETY: valid hostif api table; attr outlives the call.
        unsafe {
            let set = (*self.hostif_api)
                .set_hostif_trap_group_attribute
                .ok_or(SaiError::Other(
                    "hostif api lacks set_hostif_trap_group_attribute".into(),
                ))?;
            let mut attr = Self::zeroed_attr(
                ffi::_sai_hostif_trap_group_attr_t::SAI_HOSTIF_TRAP_GROUP_ATTR_POLICER,
            );
            attr.value.oid = policer.map(|p| p.0).unwrap_or(0);
            check(
                "set_hostif_trap_group_attribute(POLICER)",
                set(defaults.trap_group, &attr),
            )
        }
    }

    fn create_qos_map(&mut self, kind: QosMapType, entries: &[(u8, u8)]) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let api = self.qos_map_api()?;
        // SAFETY: valid qos-map api table.
        let create = unsafe {
            (*api)
                .create_qos_map
                .ok_or(SaiError::Other("qos map api lacks create_qos_map".into()))?
        };
        let mut list = Self::qos_map_entries(kind, entries);
        let mut type_attr = Self::zeroed_attr(ffi::_sai_qos_map_attr_t::SAI_QOS_MAP_ATTR_TYPE);
        type_attr.value.s32 = Self::sai_qos_map_type(kind) as i32;
        let mut list_attr =
            Self::zeroed_attr(ffi::_sai_qos_map_attr_t::SAI_QOS_MAP_ATTR_MAP_TO_VALUE_LIST);
        list_attr.value.qosmap.count = list.len() as u32;
        list_attr.value.qosmap.list = list.as_mut_ptr();
        let attrs = [type_attr, list_attr];
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array and the entry list outlive the call.
        unsafe {
            check(
                "create_qos_map",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        self.qos_map_kinds.insert(oid, kind);
        Ok(Oid(oid))
    }

    fn set_qos_map(&mut self, map: Oid, entries: &[(u8, u8)]) -> Result<(), SaiError> {
        self.switch_oid()?;
        let api = self.qos_map_api()?;
        // SAFETY: valid qos-map api table.
        let set = unsafe {
            (*api).set_qos_map_attribute.ok_or(SaiError::Other(
                "qos map api lacks set_qos_map_attribute".into(),
            ))?
        };
        // The map's type is create-only, so the entry layout comes from
        // what this backend created it as.
        let kind = *self
            .qos_map_kinds
            .get(&map.0)
            .ok_or_else(|| SaiError::Other(format!("no such QoS map {map}")))?;
        let mut list = Self::qos_map_entries(kind, entries);
        let mut attr =
            Self::zeroed_attr(ffi::_sai_qos_map_attr_t::SAI_QOS_MAP_ATTR_MAP_TO_VALUE_LIST);
        attr.value.qosmap.count = list.len() as u32;
        attr.value.qosmap.list = list.as_mut_ptr();
        // SAFETY: attr and the entry list outlive the call.
        unsafe {
            check(
                "set_qos_map_attribute(MAP_TO_VALUE_LIST)",
                set(map.0, &attr),
            )
        }
    }

    fn remove_qos_map(&mut self, map: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        let api = self.qos_map_api()?;
        // SAFETY: valid qos-map api table.
        unsafe {
            let remove = (*api)
                .remove_qos_map
                .ok_or(SaiError::Other("qos map api lacks remove_qos_map".into()))?;
            check("remove_qos_map", remove(map.0))?;
        }
        self.qos_map_kinds.remove(&map.0);
        Ok(())
    }

    fn set_port_qos_map_binding(
        &mut self,
        port: PortId,
        kind: QosMapType,
        map: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.switch_oid()?;
        let mut attr = Self::zeroed_attr(Self::qos_map_port_attr(kind));
        attr.value.oid = map.map(|m| m.0).unwrap_or(0);
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check("set_port_attribute(QOS map)", set(port.0, &attr))
        }
    }

    fn set_port_default_tc(&mut self, port: PortId, tc: u8) -> Result<(), SaiError> {
        self.switch_oid()?;
        let mut attr = Self::zeroed_attr(ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_DEFAULT_TC);
        attr.value.u8_ = tc;
        // SAFETY: valid port api table; attr outlives the call.
        unsafe {
            let set = (*self.port_api)
                .set_port_attribute
                .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
            check("set_port_attribute(QOS_DEFAULT_TC)", set(port.0, &attr))
        }
    }

    fn create_scheduler(&mut self, spec: SchedulerSpec) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let api = self.scheduler_api()?;
        // SAFETY: valid scheduler api table.
        let create = unsafe {
            (*api).create_scheduler.ok_or(SaiError::Other(
                "scheduler api lacks create_scheduler".into(),
            ))?
        };
        let attrs = Self::scheduler_attrs(spec);
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_scheduler",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn remove_scheduler(&mut self, scheduler: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        let api = self.scheduler_api()?;
        // SAFETY: valid scheduler api table.
        unsafe {
            let remove = (*api).remove_scheduler.ok_or(SaiError::Other(
                "scheduler api lacks remove_scheduler".into(),
            ))?;
            check("remove_scheduler", remove(scheduler.0))
        }
    }

    fn bind_queue_scheduler(
        &mut self,
        port: PortId,
        queue: u32,
        scheduler: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.switch_oid()?;
        let queue_oid = self.queue_oid(port, queue)?;
        let mut attr =
            Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_SCHEDULER_PROFILE_ID);
        attr.value.oid = scheduler.map(|s| s.0).unwrap_or(0);
        // SAFETY: valid queue api table; attr outlives the call.
        unsafe {
            let set = (*self.queue_api)
                .set_queue_attribute
                .ok_or(SaiError::Other(
                    "queue api lacks set_queue_attribute".into(),
                ))?;
            check(
                "set_queue_attribute(SCHEDULER_PROFILE_ID)",
                set(queue_oid, &attr),
            )
        }
    }

    fn set_port_shaper(&mut self, port: PortId, rate_bps: Option<u64>) -> Result<(), SaiError> {
        self.switch_oid()?;
        // The port shaper is a scheduler profile hung on the port, so
        // this owns that object's lifecycle: create on first set,
        // update in place while it lives, drop it when the shaper goes.
        let existing = self.port_shaper_profiles.get(&port.0).copied();
        match (rate_bps, existing) {
            (Some(rate), Some(profile)) => {
                let mut attr = Self::zeroed_attr(
                    ffi::_sai_scheduler_attr_t::SAI_SCHEDULER_ATTR_MAX_BANDWIDTH_RATE,
                );
                attr.value.u64_ = rate / 8;
                let api = self.scheduler_api()?;
                // SAFETY: valid scheduler api table; attr outlives the call.
                unsafe {
                    let set = (*api).set_scheduler_attribute.ok_or(SaiError::Other(
                        "scheduler api lacks set_scheduler_attribute".into(),
                    ))?;
                    check(
                        "set_scheduler_attribute(MAX_BANDWIDTH_RATE)",
                        set(profile, &attr),
                    )?;
                }
            }
            (Some(rate), None) => {
                let profile = self.create_scheduler(SchedulerSpec {
                    strict: false,
                    weight: 1,
                    max_rate_bps: Some(rate),
                })?;
                let mut attr = Self::zeroed_attr(
                    ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_SCHEDULER_PROFILE_ID,
                );
                attr.value.oid = profile.0;
                // SAFETY: valid port api table; attr outlives the call.
                unsafe {
                    let set = (*self.port_api)
                        .set_port_attribute
                        .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
                    check(
                        "set_port_attribute(QOS_SCHEDULER_PROFILE_ID)",
                        set(port.0, &attr),
                    )?;
                }
                self.port_shaper_profiles.insert(port.0, profile.0);
            }
            (None, Some(profile)) => {
                let mut attr = Self::zeroed_attr(
                    ffi::_sai_port_attr_t::SAI_PORT_ATTR_QOS_SCHEDULER_PROFILE_ID,
                );
                attr.value.oid = 0;
                // SAFETY: valid port api table; attr outlives the call.
                unsafe {
                    let set = (*self.port_api)
                        .set_port_attribute
                        .ok_or(SaiError::Other("port api lacks set_port_attribute".into()))?;
                    check(
                        "set_port_attribute(QOS_SCHEDULER_PROFILE_ID)",
                        set(port.0, &attr),
                    )?;
                }
                self.port_shaper_profiles.remove(&port.0);
                self.remove_scheduler(Oid(profile))?;
            }
            (None, None) => {}
        }
        Ok(())
    }

    fn create_wred(&mut self, spec: WredSpec) -> Result<Oid, SaiError> {
        let switch = self.switch_oid()?;
        let api = self.wred_api()?;
        // SAFETY: valid WRED api table.
        let create = unsafe {
            (*api)
                .create_wred
                .ok_or(SaiError::Other("wred api lacks create_wred".into()))?
        };
        let attrs = Self::wred_attrs(spec);
        let mut oid: ffi::sai_object_id_t = 0;
        // SAFETY: attr array outlives the call.
        unsafe {
            check(
                "create_wred",
                create(&mut oid, switch, attrs.len() as u32, attrs.as_ptr()),
            )?;
        }
        Ok(Oid(oid))
    }

    fn set_wred(&mut self, wred: Oid, spec: WredSpec) -> Result<(), SaiError> {
        self.switch_oid()?;
        let api = self.wred_api()?;
        // SAFETY: valid WRED api table.
        let set = unsafe {
            (*api)
                .set_wred_attribute
                .ok_or(SaiError::Other("wred api lacks set_wred_attribute".into()))?
        };
        // WRED attributes are all CREATE_AND_SET, so the profile
        // updates in place and bound queues keep their binding.
        for attr in Self::wred_attrs(spec) {
            // SAFETY: attr outlives the call.
            unsafe { check("set_wred_attribute", set(wred.0, &attr))? };
        }
        Ok(())
    }

    fn remove_wred(&mut self, wred: Oid) -> Result<(), SaiError> {
        self.switch_oid()?;
        let api = self.wred_api()?;
        // SAFETY: valid WRED api table.
        unsafe {
            let remove = (*api)
                .remove_wred
                .ok_or(SaiError::Other("wred api lacks remove_wred".into()))?;
            check("remove_wred", remove(wred.0))
        }
    }

    fn bind_queue_wred(
        &mut self,
        port: PortId,
        queue: u32,
        wred: Option<Oid>,
    ) -> Result<(), SaiError> {
        self.switch_oid()?;
        let queue_oid = self.queue_oid(port, queue)?;
        let mut attr = Self::zeroed_attr(ffi::_sai_queue_attr_t::SAI_QUEUE_ATTR_WRED_PROFILE_ID);
        attr.value.oid = wred.map(|w| w.0).unwrap_or(0);
        // SAFETY: valid queue api table; attr outlives the call.
        unsafe {
            let set = (*self.queue_api)
                .set_queue_attribute
                .ok_or(SaiError::Other(
                    "queue api lacks set_queue_attribute".into(),
                ))?;
            check(
                "set_queue_attribute(WRED_PROFILE_ID)",
                set(queue_oid, &attr),
            )
        }
    }

    fn set_port_learn_limit(&mut self, port: PortId, limit: Option<u32>) -> Result<(), SaiError> {
        let bridge_port = self.bridge_port_of(port)?;
        // SAFETY: valid bridge api table; attr outlives the call.
        unsafe {
            let set = (*self.bridge_api)
                .set_bridge_port_attribute
                .ok_or(SaiError::Other(
                    "bridge api lacks set_bridge_port_attribute".into(),
                ))?;
            let mut attr = Self::zeroed_attr(
                ffi::_sai_bridge_port_attr_t::SAI_BRIDGE_PORT_ATTR_MAX_LEARNED_ADDRESSES,
            );
            // 0 = no limit, per saibridge.h.
            attr.value.u32_ = limit.unwrap_or(0);
            check(
                "set_bridge_port_attribute(MAX_LEARNED_ADDRESSES)",
                set(bridge_port, &attr),
            )
        }
    }

    fn set_port_learning(&mut self, port: PortId, learn: bool) -> Result<(), SaiError> {
        let bridge_port = self.bridge_port_of(port)?;
        // SAFETY: valid bridge api table; attr outlives the call.
        unsafe {
            let set = (*self.bridge_api)
                .set_bridge_port_attribute
                .ok_or(SaiError::Other(
                    "bridge api lacks set_bridge_port_attribute".into(),
                ))?;
            let mut attr = Self::zeroed_attr(
                ffi::_sai_bridge_port_attr_t::SAI_BRIDGE_PORT_ATTR_FDB_LEARNING_MODE,
            );
            attr.value.s32 = if learn {
                ffi::_sai_bridge_port_fdb_learning_mode_t::SAI_BRIDGE_PORT_FDB_LEARNING_MODE_HW
            } else {
                ffi::_sai_bridge_port_fdb_learning_mode_t::SAI_BRIDGE_PORT_FDB_LEARNING_MODE_DISABLE
            } as i32;
            check(
                "set_bridge_port_attribute(FDB_LEARNING_MODE)",
                set(bridge_port, &attr),
            )
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
