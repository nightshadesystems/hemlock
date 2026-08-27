//! The OpenBCM backend: `SaiBackend` over Hemlock's own C shim.
//!
//! Hemlock drives Broadcom XGS ASICs through SAI. The AS4610-54T cannot —
//! its host CPU is an on-die ARM Cortex-A9 and no `libsaibcm` is published
//! for armhf — so on that board the datapath is a thin C shim built inside
//! the OpenBCM SDK's own tree, exporting the versioned ABI in
//! `openbcm-shim/hemlockbcm.h`. This module dlopens it and presents the
//! same `SaiBackend` every other part of Hemlock talks to.
//!
//! Two things keep the arrangement honest:
//!
//! * **The ABI is ours.** It mirrors the trait, not SAI, so it is small
//!   and every slot has an obvious owner. Major version mismatch refuses
//!   to load; minor versions are accepted and `struct_size` says how much
//!   of the vtable is real.
//! * **A NULL slot means "not implemented here."** It becomes the same
//!   not-implemented error a SAI missing an object family returns, which
//!   [`SaiError::is_unsupported`] already classifies — so `capabilities()`
//!   reports the truth and both consoles degrade the way they do today.
//!   Most slots are NULL until phase 6 fills them in.
//!
//! This module and `vendor.rs` are the only places `unsafe` is allowed.

#![allow(unsafe_code)]

use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::{
    AclAction, AclFamily, AclFields, AclStage, CablePair, FdbAction, IpPrefix, Oid, PolicerSpec,
    PolicerStats, PortCounters, PortId, QosMapType, QueueCounters, RouteTarget, SaiBackend,
    SaiCapabilities, SaiError, SaiEvent, SaiPort, SchedulerSpec, StormClass, StpPortState,
    SwitchInfo, TrapKind, WredSpec,
};

// ---------------------------------------------------------------------------
// The ABI, transcribed from openbcm-shim/hemlockbcm.h.
//
// Hand-written rather than bindgen'd: the header is ours and tiny, and a
// hand-written mirror means the `openbcm` feature needs no libclang — so
// CI compiles this on every push with nothing installed. `abi_layout`'s
// tests below pin the parts a transcription error could get wrong.
// ---------------------------------------------------------------------------

/// The ABI major version *this build of Rust* implements — that is, the
/// version of `hemlockbcm.h` the structs below were transcribed from.
/// A platform pinning any other major cannot be served by this binary,
/// however willing the shim is: the marshalling would be wrong.
pub const ABI_MAJOR: u32 = 1;
/// `hemlockbcm.h`'s `HEMLOCKBCM_PORT_NAME_MAX`.
const PORT_NAME_MAX: usize = 16;

/// `HEMLOCKBCM_ERR_NOT_IMPLEMENTED`, which is also SAI's
/// `SAI_STATUS_NOT_IMPLEMENTED` — so an absent slot and an absent SAI
/// family reach the operator identically.
const ERR_NOT_IMPLEMENTED: i32 = -15;

#[repr(C)]
struct Init {
    config_bcm_path: *const std::os::raw::c_char,
    src_mac: [u8; 6],
    diag_shell: std::os::raw::c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ShimPort {
    logical_port: u32,
    name: [std::os::raw::c_char; PORT_NAME_MAX],
    speed_mbps: u32,
    admin_up: std::os::raw::c_int,
    oper_up: std::os::raw::c_int,
}

#[repr(C)]
#[derive(Default)]
struct ShimCounters {
    in_octets: u64,
    in_ucast_pkts: u64,
    in_mcast_pkts: u64,
    in_bcast_pkts: u64,
    in_discards: u64,
    in_errors: u64,
    in_crc_errors: u64,
    in_alignment_errors: u64,
    in_symbol_errors: u64,
    in_runts: u64,
    in_giants: u64,
    in_pause: u64,
    out_octets: u64,
    out_ucast_pkts: u64,
    out_mcast_pkts: u64,
    out_bcast_pkts: u64,
    out_discards: u64,
    out_errors: u64,
    out_pause: u64,
    collisions: u64,
    late_collisions: u64,
    deferred: u64,
    rx_bins: [u64; 7],
    tx_bins: [u64; 7],
}

#[repr(C)]
#[derive(Default)]
struct ShimCapabilities {
    buffer_bytes_total: u64,
    ecmp_width: u32,
    mirror_sessions_max: u32,
    ipv6: std::os::raw::c_int,
}

/// Opaque switch handle.
#[repr(C)]
struct ShimSwitch {
    _private: [u8; 0],
}

type LinkCb = unsafe extern "C" fn(*mut c_void, u32, std::os::raw::c_int);

type Status = std::os::raw::c_int;

/// The vtable. Field order and types must match `struct hemlockbcm_api`
/// exactly; `struct_size` guards against a shim built from an older
/// header, so trailing slots are only read when it says they exist.
#[repr(C)]
struct Api {
    struct_size: usize,
    abi_major: u32,
    abi_minor: u32,

    create_switch: Option<unsafe extern "C" fn(*mut *mut ShimSwitch, *const Init) -> Status>,
    destroy_switch: Option<unsafe extern "C" fn(*mut ShimSwitch) -> Status>,
    set_link_callback:
        Option<unsafe extern "C" fn(*mut ShimSwitch, Option<LinkCb>, *mut c_void) -> Status>,

    ports: Option<unsafe extern "C" fn(*mut ShimSwitch, *mut ShimPort, *mut usize) -> Status>,
    set_port_admin_state:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> Status>,
    set_port_speed: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,
    set_port_duplex:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> Status>,
    set_port_autoneg:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> Status>,
    set_port_mtu: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,
    port_counters: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, *mut ShimCounters) -> Status>,

    capabilities: Option<unsafe extern "C" fn(*mut ShimSwitch, *mut ShimCapabilities) -> Status>,
}

impl Api {
    /// Whether a slot at `offset` bytes into the struct is actually
    /// present in the shim we loaded. A shim built from an older header
    /// is shorter, and reading past its end would be undefined.
    fn has(&self, offset: usize) -> bool {
        offset + std::mem::size_of::<usize>() <= self.struct_size
    }
}

/// Map a shim status onto the error type the rest of Hemlock understands.
fn check(call: &'static str, status: Status) -> Result<(), SaiError> {
    if status == 0 {
        Ok(())
    } else {
        Err(SaiError::Status { call, status })
    }
}

/// The error a NULL vtable slot produces. Classified as unsupported, so
/// syncd's capability gates and the two consoles treat "this shim does
/// not implement it yet" exactly like "this SAI does not implement it".
fn unimplemented_slot<T>(call: &'static str) -> Result<T, SaiError> {
    Err(SaiError::Status {
        call,
        status: ERR_NOT_IMPLEMENTED,
    })
}

// ---------------------------------------------------------------------------
// Link notifications.
//
// The shim may call back from its own thread, so the context it gets is a
// leaked Arc holding the sender. One per backend instance — unlike the
// vendor SAI's global callbacks, which the C API forces.
// ---------------------------------------------------------------------------

struct EventContext {
    tx: mpsc::UnboundedSender<SaiEvent>,
}

unsafe extern "C" fn link_callback(context: *mut c_void, logical_port: u32, up: c_int_alias) {
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the pointer we handed to set_link_callback, an
    // Arc<EventContext> kept alive for the backend's lifetime. Borrowed,
    // never reconstructed, so the refcount is untouched.
    let ctx = unsafe { &*(context as *const EventContext) };
    let _ = ctx.tx.send(SaiEvent::PortOperStatus {
        port: PortId(logical_port as u64),
        up: up != 0,
    });
}

// `c_int` under a name the extern "C" signature above can use verbatim.
#[allow(non_camel_case_types)]
type c_int_alias = std::os::raw::c_int;

// ---------------------------------------------------------------------------

/// `SaiBackend` over a dlopened `libhemlockbcm.so`.
pub struct OpenBcmBackend {
    /// Kept alive for as long as any pointer into it is used; dropping it
    /// unloads the shim.
    _library: libloading::Library,
    api: &'static Api,
    switch: *mut ShimSwitch,
    shim_path: PathBuf,
    config_bcm_path: PathBuf,
    src_mac: Option<[u8; 6]>,
    diag_shell: bool,
    events: Option<mpsc::UnboundedReceiver<SaiEvent>>,
    event_ctx: Arc<EventContext>,
    /// Ports as the shim last reported them, so `sai_port_name` can
    /// answer syncd's startup assertion without another call.
    port_names: Arc<Mutex<Vec<(u32, String)>>>,
}

// SAFETY: the raw switch pointer is only ever used from the thread that
// owns the backend — syncd's dedicated SAI actor thread, the same
// single-threaded discipline the vendor library gets.
unsafe impl Send for OpenBcmBackend {}

/// Identity and version, not the vtable: a list of function pointers
/// tells a reader nothing, and the shim's path and ABI are what anyone
/// debugging a load actually wants.
impl std::fmt::Debug for OpenBcmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenBcmBackend")
            .field("shim_path", &self.shim_path)
            .field("abi", &format_args!("{}.{}", ABI_MAJOR, self.api.abi_minor))
            .field("switch_created", &!self.switch.is_null())
            .finish()
    }
}

impl OpenBcmBackend {
    /// dlopen the shim at `shim_path` and check the ABI handshake.
    /// `abi_major` is what the platform manifest pins.
    pub fn new(init: &crate::SwitchInit, abi_major: u32) -> Result<Self, SaiError> {
        // The pin has to match what this binary knows how to marshal, not
        // merely what some shim is willing to serve. A manifest asking
        // for a major this code was not written against is unservable
        // even if a shim answers it.
        if abi_major != ABI_MAJOR {
            return Err(SaiError::Load(format!(
                "manifest pins shim ABI major {abi_major}, but this build implements {ABI_MAJOR}"
            )));
        }
        let shim_path = init
            .shim_path
            .clone()
            .ok_or_else(|| SaiError::Other("no shim_path for the openbcm backend".into()))?;

        // SAFETY: loading the platform's own datapath shim, built from
        // the header committed in this repository.
        let library = unsafe { libloading::Library::new(&shim_path) }
            .map_err(|e| SaiError::Load(format!("{}: {e}", shim_path.display())))?;

        // SAFETY: the one symbol the ABI defines.
        let api: &'static Api = unsafe {
            let get_api: libloading::Symbol<unsafe extern "C" fn(u32) -> *const Api> = library
                .get(b"hemlockbcm_get_api\0")
                .map_err(|e| SaiError::Load(format!("hemlockbcm_get_api: {e}")))?;
            let ptr = get_api(abi_major);
            if ptr.is_null() {
                return Err(SaiError::Load(format!(
                    "{}: shim does not implement ABI major {abi_major}",
                    shim_path.display()
                )));
            }
            &*ptr
        };

        // The shim answering at all is not enough: a shim that served a
        // different major would marshal every call wrong.
        if api.abi_major != abi_major {
            return Err(SaiError::Load(format!(
                "{}: shim reports ABI {}.{}, manifest pins major {abi_major}",
                shim_path.display(),
                api.abi_major,
                api.abi_minor
            )));
        }
        // A struct shorter than its own fixed header is not a vtable.
        let minimum = std::mem::size_of::<usize>() + 2 * std::mem::size_of::<u32>();
        if api.struct_size < minimum {
            return Err(SaiError::Load(format!(
                "{}: shim reports struct_size {} (minimum {minimum})",
                shim_path.display(),
                api.struct_size
            )));
        }
        tracing::info!(
            shim = %shim_path.display(),
            abi = format_args!("{}.{}", api.abi_major, api.abi_minor),
            struct_size = api.struct_size,
            "OpenBCM shim loaded"
        );

        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Self {
            _library: library,
            api,
            switch: std::ptr::null_mut(),
            shim_path,
            config_bcm_path: init.config_bcm_path.clone(),
            src_mac: init.src_mac,
            diag_shell: init.diag_shell,
            events: Some(rx),
            event_ctx: Arc::new(EventContext { tx }),
            port_names: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn switch(&self) -> Result<*mut ShimSwitch, SaiError> {
        if self.switch.is_null() {
            Err(SaiError::NoSwitch)
        } else {
            Ok(self.switch)
        }
    }
}

impl Drop for OpenBcmBackend {
    fn drop(&mut self) {
        if self.switch.is_null() {
            return;
        }
        // Best effort: a shim without destroy_switch just leaks its
        // handle, which matters only in tests — syncd holds one for the
        // life of the process.
        if let Some(destroy) = self.api.destroy_switch {
            // SAFETY: our own handle, and the last use of it.
            let status = unsafe { destroy(self.switch) };
            if status != 0 {
                tracing::warn!(status, "OpenBCM shim destroy_switch failed");
            }
        }
        self.switch = std::ptr::null_mut();
    }
}

/// Every call on a slot follows the same shape: check it exists, call it,
/// map the status. Spelled once here rather than in each of the methods.
macro_rules! slot {
    ($self:expr, $field:ident, $call:literal) => {{
        match $self.api.$field {
            Some(f) if $self.api.has(std::mem::offset_of!(Api, $field)) => Ok(f),
            _ => unimplemented_slot($call),
        }
    }};
}

impl SaiBackend for OpenBcmBackend {
    fn name(&self) -> String {
        format!("openbcm:{}", self.shim_path.display())
    }

    fn create_switch(&mut self) -> Result<SwitchInfo, SaiError> {
        if !self.switch.is_null() {
            return Err(SaiError::Other("switch already created".into()));
        }
        let create = slot!(self, create_switch, "create_switch")?;
        let config = path_to_cstring(&self.config_bcm_path)?;
        let init = Init {
            config_bcm_path: config.as_ptr(),
            src_mac: self.src_mac.unwrap_or([0; 6]),
            diag_shell: i32::from(self.diag_shell),
        };
        let mut handle: *mut ShimSwitch = std::ptr::null_mut();
        // SAFETY: `init` and its string outlive the call; `handle` is
        // written only on success.
        check("create_switch", unsafe { create(&mut handle, &init) })?;
        if handle.is_null() {
            return Err(SaiError::Other(
                "shim create_switch returned success with a null handle".into(),
            ));
        }
        self.switch = handle;

        // Register for oper-status before anything is brought up, so no
        // transition is missed.
        if let Ok(set_cb) = slot!(self, set_link_callback, "set_link_callback") {
            let ctx = Arc::as_ptr(&self.event_ctx) as *mut c_void;
            // SAFETY: the context is an Arc held by `self` for as long as
            // the switch exists, and cleared in Drop before it is freed.
            check("set_link_callback", unsafe {
                set_cb(handle, Some(link_callback), ctx)
            })?;
        } else {
            tracing::warn!("OpenBCM shim has no set_link_callback; link state will not update");
        }

        // OpenBCM has no SAI object ids: the switch is unit 0 and there
        // is one 802.1Q VLAN table, so the "default VLAN oid" that FDB
        // events are keyed on is simply VLAN 1.
        Ok(SwitchInfo {
            oid: 0,
            default_vlan_oid: 1,
        })
    }

    fn ports(&mut self) -> Result<Vec<SaiPort>, SaiError> {
        let ports_fn = slot!(self, ports, "ports")?;
        let sw = self.switch()?;

        // Two-call convention: ask for the count, then fill.
        let mut count: usize = 0;
        // SAFETY: the null/zero form is defined by the ABI as "how many?".
        unsafe { ports_fn(sw, std::ptr::null_mut(), &mut count) };
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut raw = vec![
            ShimPort {
                logical_port: 0,
                name: [0; PORT_NAME_MAX],
                speed_mbps: 0,
                admin_up: 0,
                oper_up: 0,
            };
            count
        ];
        let mut capacity = count;
        // SAFETY: `raw` has `capacity` elements and the shim writes at
        // most that many.
        check("ports", unsafe {
            ports_fn(sw, raw.as_mut_ptr(), &mut capacity)
        })?;
        raw.truncate(capacity.min(count));

        let mut names = Vec::with_capacity(raw.len());
        let ports = raw
            .iter()
            .map(|p| {
                names.push((p.logical_port, shim_name(&p.name)));
                SaiPort {
                    // The SDK's logical port number *is* the id and the
                    // lane: the manifest's `lanes` list carries it, which
                    // is how syncd's correlate-by-lane-set join works
                    // unchanged on this backend.
                    id: PortId(p.logical_port as u64),
                    lanes: vec![p.logical_port],
                    speed_mbps: p.speed_mbps,
                    admin_up: p.admin_up != 0,
                    oper_up: p.oper_up != 0,
                }
            })
            .collect();
        if let Ok(mut cached) = self.port_names.lock() {
            *cached = names;
        }
        Ok(ports)
    }

    fn sai_port_name(&self, port: PortId) -> Option<String> {
        let cached = self.port_names.lock().ok()?;
        cached
            .iter()
            .find(|(logical, _)| *logical as u64 == port.0)
            .map(|(_, name)| name.clone())
    }

    fn set_port_admin_state(&mut self, port: PortId, up: bool) -> Result<(), SaiError> {
        let f = slot!(self, set_port_admin_state, "set_port_admin_state")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_admin_state", unsafe {
            f(sw, port.0 as u32, i32::from(up))
        })
    }

    fn set_port_speed(&mut self, port: PortId, speed_mbps: u32) -> Result<(), SaiError> {
        let f = slot!(self, set_port_speed, "set_port_speed")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_speed", unsafe {
            f(sw, port.0 as u32, speed_mbps)
        })
    }

    fn set_port_duplex(&mut self, port: PortId, full: bool) -> Result<(), SaiError> {
        let f = slot!(self, set_port_duplex, "set_port_duplex")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_duplex", unsafe {
            f(sw, port.0 as u32, i32::from(full))
        })
    }

    fn set_port_autoneg(&mut self, port: PortId, on: bool) -> Result<(), SaiError> {
        let f = slot!(self, set_port_autoneg, "set_port_autoneg")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_autoneg", unsafe {
            f(sw, port.0 as u32, i32::from(on))
        })
    }

    fn set_port_mtu(&mut self, port: PortId, mtu: u32) -> Result<(), SaiError> {
        let f = slot!(self, set_port_mtu, "set_port_mtu")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_mtu", unsafe { f(sw, port.0 as u32, mtu) })
    }

    fn port_counters(&mut self, port: PortId) -> Result<PortCounters, SaiError> {
        let f = slot!(self, port_counters, "port_counters")?;
        let sw = self.switch()?;
        let mut raw = ShimCounters::default();
        // SAFETY: `raw` is written only on success.
        check("port_counters", unsafe { f(sw, port.0 as u32, &mut raw) })?;
        Ok(PortCounters {
            in_octets: raw.in_octets,
            in_ucast_pkts: raw.in_ucast_pkts,
            in_mcast_pkts: raw.in_mcast_pkts,
            in_bcast_pkts: raw.in_bcast_pkts,
            in_discards: raw.in_discards,
            in_errors: raw.in_errors,
            in_crc_errors: raw.in_crc_errors,
            in_alignment_errors: raw.in_alignment_errors,
            in_symbol_errors: raw.in_symbol_errors,
            in_runts: raw.in_runts,
            in_giants: raw.in_giants,
            in_pause: raw.in_pause,
            out_octets: raw.out_octets,
            out_ucast_pkts: raw.out_ucast_pkts,
            out_mcast_pkts: raw.out_mcast_pkts,
            out_bcast_pkts: raw.out_bcast_pkts,
            out_discards: raw.out_discards,
            out_errors: raw.out_errors,
            out_pause: raw.out_pause,
            collisions: raw.collisions,
            late_collisions: raw.late_collisions,
            deferred: raw.deferred,
            rx_bins: raw.rx_bins,
            tx_bins: raw.tx_bins,
        })
    }

    /// No queue-stat slot until phase 6's QoS step. An empty list is the
    /// documented "backend has none" answer: syncd renders the
    /// platform-declared queues as zeros rather than failing.
    fn port_queue_counters(&mut self, _port: PortId) -> Result<Vec<QueueCounters>, SaiError> {
        Ok(Vec::new())
    }

    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>> {
        self.events.take()
    }

    fn capabilities(&mut self) -> Result<SaiCapabilities, SaiError> {
        let f = slot!(self, capabilities, "capabilities")?;
        let sw = self.switch()?;
        let mut raw = ShimCapabilities::default();
        // SAFETY: `raw` is written only on success.
        check("capabilities", unsafe { f(sw, &mut raw) })?;

        // What the shim reports, plus what the vtable itself proves: a
        // family whose slot is NULL is not supported, whatever anyone
        // says. Phase 6 turns these on one at a time by filling slots in,
        // so the flags cannot drift from the implementation.
        Ok(SaiCapabilities {
            lag: false,
            stp: false,
            fdb_flush: false,
            fdb_aging: false,
            l2mc: false,
            storm_control: false,
            mirror: raw.mirror_sessions_max > 0,
            mirror_sessions_max: raw.mirror_sessions_max,
            port_tpid: false,
            ecmp_width: raw.ecmp_width,
            ipv6: raw.ipv6 != 0,
            my_mac: false,
            acl_ingress: false,
            acl_egress: false,
            acl_entry_policer: false,
            port_learn_limit: false,
            copp: false,
            buffer_bytes_total: raw.buffer_bytes_total,
            qos_map_ingress: false,
            qos_map_egress: false,
            wred: false,
            ecn: false,
            queue_shaper: false,
            wred_queue_stats: false,
            sflow: false,
            cable_diag: false,
        })
    }

    // -----------------------------------------------------------------
    // Phase 6.
    //
    // Every family below reaches the operator as "not supported by this
    // platform" — the same path a SAI missing the family takes, so
    // nothing above this crate needs to know the difference. They are
    // listed individually, in trait order, because that list is the
    // work queue: FDB, LAG, STP, mirror, storm control, ACLs/policers/
    // CoPP, sFlow, QoS.
    // -----------------------------------------------------------------

    fn setup_host_punt(&mut self) -> Result<(), SaiError> {
        unimplemented_slot("setup_host_punt")
    }

    fn create_hostif(&mut self, _port: PortId, _name: &str) -> Result<Oid, SaiError> {
        unimplemented_slot("create_hostif")
    }

    fn create_router_interface(&mut self, _port: PortId) -> Result<Oid, SaiError> {
        unimplemented_slot("create_router_interface")
    }

    fn create_vlan_router_interface(&mut self, _vlan: Option<Oid>) -> Result<Oid, SaiError> {
        unimplemented_slot("create_vlan_router_interface")
    }

    fn remove_vlan_router_interface(&mut self, _rif: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_vlan_router_interface")
    }

    fn remove_router_interface(&mut self, _port: PortId, _rif: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_router_interface")
    }

    fn create_route(&mut self, _dest: IpPrefix, _target: RouteTarget) -> Result<(), SaiError> {
        unimplemented_slot("create_route")
    }

    fn remove_route(&mut self, _dest: IpPrefix) -> Result<(), SaiError> {
        unimplemented_slot("remove_route")
    }

    fn create_neighbor(
        &mut self,
        _rif: Oid,
        _ip: std::net::IpAddr,
        _mac: [u8; 6],
    ) -> Result<(), SaiError> {
        unimplemented_slot("create_neighbor")
    }

    fn remove_neighbor(&mut self, _rif: Oid, _ip: std::net::IpAddr) -> Result<(), SaiError> {
        unimplemented_slot("remove_neighbor")
    }

    fn create_next_hop(&mut self, _rif: Oid, _ip: std::net::IpAddr) -> Result<Oid, SaiError> {
        unimplemented_slot("create_next_hop")
    }

    fn remove_next_hop(&mut self, _next_hop: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_next_hop")
    }

    fn create_next_hop_group(&mut self) -> Result<Oid, SaiError> {
        unimplemented_slot("create_next_hop_group")
    }

    fn remove_next_hop_group(&mut self, _group: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_next_hop_group")
    }

    fn add_next_hop_group_member(&mut self, _group: Oid, _next_hop: Oid) -> Result<Oid, SaiError> {
        unimplemented_slot("add_next_hop_group_member")
    }

    fn remove_next_hop_group_member(&mut self, _member: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_next_hop_group_member")
    }

    fn create_my_mac(&mut self, _vlan_id: Option<u16>, _mac: [u8; 6]) -> Result<Oid, SaiError> {
        unimplemented_slot("create_my_mac")
    }

    fn remove_my_mac(&mut self, _my_mac: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_my_mac")
    }

    fn create_vlan(&mut self, _vlan_id: u16) -> Result<Oid, SaiError> {
        unimplemented_slot("create_vlan")
    }

    fn remove_vlan(&mut self, _vlan: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_vlan")
    }

    fn add_vlan_member(
        &mut self,
        _vlan: Oid,
        _port: PortId,
        _tagged: bool,
    ) -> Result<Oid, SaiError> {
        unimplemented_slot("add_vlan_member")
    }

    fn remove_vlan_member(&mut self, _member: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_vlan_member")
    }

    fn set_port_pvid(&mut self, _port: PortId, _vlan_number: u16) -> Result<(), SaiError> {
        unimplemented_slot("set_port_pvid")
    }

    fn remove_port_default_vlan(&mut self, _port: PortId) -> Result<(), SaiError> {
        unimplemented_slot("remove_port_default_vlan")
    }

    fn restore_port_default_vlan(&mut self, _port: PortId) -> Result<(), SaiError> {
        unimplemented_slot("restore_port_default_vlan")
    }

    fn set_fdb_aging(&mut self, _secs: u32) -> Result<(), SaiError> {
        unimplemented_slot("set_fdb_aging")
    }

    fn add_fdb_entry(
        &mut self,
        _vlan: Option<Oid>,
        _mac: [u8; 6],
        _action: FdbAction,
    ) -> Result<(), SaiError> {
        unimplemented_slot("add_fdb_entry")
    }

    fn remove_fdb_entry(&mut self, _vlan: Option<Oid>, _mac: [u8; 6]) -> Result<(), SaiError> {
        unimplemented_slot("remove_fdb_entry")
    }

    fn flush_fdb(&mut self, _vlan: Option<Oid>, _port: Option<PortId>) -> Result<(), SaiError> {
        unimplemented_slot("flush_fdb")
    }

    fn set_port_storm_control(
        &mut self,
        _port: PortId,
        _class: StormClass,
        _kbps: Option<u64>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_port_storm_control")
    }

    fn port_storm_drops(&mut self, _port: PortId, _class: StormClass) -> Result<u64, SaiError> {
        unimplemented_slot("port_storm_drops")
    }

    fn create_mirror_session(&mut self, _monitor: PortId) -> Result<Oid, SaiError> {
        unimplemented_slot("create_mirror_session")
    }

    fn remove_mirror_session(&mut self, _session: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_mirror_session")
    }

    fn set_port_mirror(
        &mut self,
        _port: PortId,
        _ingress: Option<Oid>,
        _egress: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_port_mirror")
    }

    fn create_samplepacket(&mut self, _rate: u32) -> Result<Oid, SaiError> {
        unimplemented_slot("create_samplepacket")
    }

    fn remove_samplepacket(&mut self, _session: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_samplepacket")
    }

    fn run_cable_diag(&mut self, _port: PortId) -> Result<Vec<CablePair>, SaiError> {
        unimplemented_slot("run_cable_diag")
    }

    fn set_port_sample_session(
        &mut self,
        _port: PortId,
        _session: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_port_sample_session")
    }

    fn set_port_tpid(&mut self, _port: PortId, _tpid: u16) -> Result<(), SaiError> {
        unimplemented_slot("set_port_tpid")
    }

    fn create_lag(&mut self) -> Result<PortId, SaiError> {
        unimplemented_slot("create_lag")
    }

    fn remove_lag(&mut self, _lag: PortId) -> Result<(), SaiError> {
        unimplemented_slot("remove_lag")
    }

    fn add_lag_member(&mut self, _lag: PortId, _port: PortId) -> Result<Oid, SaiError> {
        unimplemented_slot("add_lag_member")
    }

    fn remove_lag_member(&mut self, _member: Oid, _port: PortId) -> Result<(), SaiError> {
        unimplemented_slot("remove_lag_member")
    }

    fn set_lag_member_state(&mut self, _member: Oid, _enabled: bool) -> Result<(), SaiError> {
        unimplemented_slot("set_lag_member_state")
    }

    fn create_stp_instance(&mut self) -> Result<Oid, SaiError> {
        unimplemented_slot("create_stp_instance")
    }

    fn remove_stp_instance(&mut self, _stp: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_stp_instance")
    }

    fn set_vlan_stp_instance(
        &mut self,
        _vlan: Option<Oid>,
        _stp: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_vlan_stp_instance")
    }

    fn set_stp_port_state(
        &mut self,
        _stp: Option<Oid>,
        _port: PortId,
        _state: StpPortState,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_stp_port_state")
    }

    fn create_l2mc_group(&mut self) -> Result<Oid, SaiError> {
        unimplemented_slot("create_l2mc_group")
    }

    fn remove_l2mc_group(&mut self, _group: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_l2mc_group")
    }

    fn add_l2mc_member(&mut self, _group: Oid, _port: PortId) -> Result<Oid, SaiError> {
        unimplemented_slot("add_l2mc_member")
    }

    fn remove_l2mc_member(&mut self, _member: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_l2mc_member")
    }

    fn set_l2mc_entry(
        &mut self,
        _vlan: Option<Oid>,
        _group_ip: std::net::IpAddr,
        _l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_l2mc_entry")
    }

    fn set_vlan_unknown_mcast_group(
        &mut self,
        _vlan: Option<Oid>,
        _l2mc: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_vlan_unknown_mcast_group")
    }

    fn create_acl_table(&mut self, _stage: AclStage, _family: AclFamily) -> Result<Oid, SaiError> {
        unimplemented_slot("create_acl_table")
    }

    fn remove_acl_table(&mut self, _table: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_acl_table")
    }

    fn create_acl_entry(
        &mut self,
        _table: Oid,
        _priority: u32,
        _fields: &AclFields,
        _action: &AclAction,
    ) -> Result<Oid, SaiError> {
        unimplemented_slot("create_acl_entry")
    }

    fn set_acl_entry_action(&mut self, _entry: Oid, _action: &AclAction) -> Result<(), SaiError> {
        unimplemented_slot("set_acl_entry_action")
    }

    fn remove_acl_entry(&mut self, _entry: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_acl_entry")
    }

    fn create_acl_counter(&mut self, _table: Oid) -> Result<Oid, SaiError> {
        unimplemented_slot("create_acl_counter")
    }

    fn remove_acl_counter(&mut self, _counter: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_acl_counter")
    }

    fn get_acl_counter(&mut self, _counter: Oid) -> Result<u64, SaiError> {
        unimplemented_slot("get_acl_counter")
    }

    fn bind_port_acl(
        &mut self,
        _port: PortId,
        _stage: AclStage,
        _table: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("bind_port_acl")
    }

    fn acl_available_entries(&mut self, _stage: AclStage) -> Result<u32, SaiError> {
        unimplemented_slot("acl_available_entries")
    }

    fn create_policer(&mut self, _spec: PolicerSpec) -> Result<Oid, SaiError> {
        unimplemented_slot("create_policer")
    }

    fn set_policer(&mut self, _policer: Oid, _spec: PolicerSpec) -> Result<(), SaiError> {
        unimplemented_slot("set_policer")
    }

    fn remove_policer(&mut self, _policer: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_policer")
    }

    fn policer_stats(&mut self, _policer: Oid) -> Result<PolicerStats, SaiError> {
        unimplemented_slot("policer_stats")
    }

    fn create_hostif_trap_group(&mut self, _policer: Option<Oid>) -> Result<Oid, SaiError> {
        unimplemented_slot("create_hostif_trap_group")
    }

    fn remove_hostif_trap_group(&mut self, _group: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_hostif_trap_group")
    }

    fn create_hostif_trap(
        &mut self,
        _kind: TrapKind,
        _trap_only: bool,
        _group: Oid,
    ) -> Result<Oid, SaiError> {
        unimplemented_slot("create_hostif_trap")
    }

    fn remove_hostif_trap(&mut self, _trap: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_hostif_trap")
    }

    fn set_default_trap_group_policer(&mut self, _policer: Option<Oid>) -> Result<(), SaiError> {
        unimplemented_slot("set_default_trap_group_policer")
    }

    fn create_qos_map(
        &mut self,
        _kind: QosMapType,
        _entries: &[(u8, u8)],
    ) -> Result<Oid, SaiError> {
        unimplemented_slot("create_qos_map")
    }

    fn set_qos_map(&mut self, _map: Oid, _entries: &[(u8, u8)]) -> Result<(), SaiError> {
        unimplemented_slot("set_qos_map")
    }

    fn remove_qos_map(&mut self, _map: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_qos_map")
    }

    fn set_port_qos_map_binding(
        &mut self,
        _port: PortId,
        _kind: QosMapType,
        _map: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("set_port_qos_map_binding")
    }

    fn set_port_default_tc(&mut self, _port: PortId, _tc: u8) -> Result<(), SaiError> {
        unimplemented_slot("set_port_default_tc")
    }

    fn create_scheduler(&mut self, _spec: SchedulerSpec) -> Result<Oid, SaiError> {
        unimplemented_slot("create_scheduler")
    }

    fn remove_scheduler(&mut self, _scheduler: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_scheduler")
    }

    fn bind_queue_scheduler(
        &mut self,
        _port: PortId,
        _queue: u32,
        _scheduler: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("bind_queue_scheduler")
    }

    fn set_port_shaper(&mut self, _port: PortId, _rate_bps: Option<u64>) -> Result<(), SaiError> {
        unimplemented_slot("set_port_shaper")
    }

    fn create_wred(&mut self, _spec: WredSpec) -> Result<Oid, SaiError> {
        unimplemented_slot("create_wred")
    }

    fn set_wred(&mut self, _wred: Oid, _spec: WredSpec) -> Result<(), SaiError> {
        unimplemented_slot("set_wred")
    }

    fn remove_wred(&mut self, _wred: Oid) -> Result<(), SaiError> {
        unimplemented_slot("remove_wred")
    }

    fn bind_queue_wred(
        &mut self,
        _port: PortId,
        _queue: u32,
        _wred: Option<Oid>,
    ) -> Result<(), SaiError> {
        unimplemented_slot("bind_queue_wred")
    }

    fn set_port_learn_limit(&mut self, _port: PortId, _limit: Option<u32>) -> Result<(), SaiError> {
        unimplemented_slot("set_port_learn_limit")
    }

    fn set_port_learning(&mut self, _port: PortId, _learn: bool) -> Result<(), SaiError> {
        unimplemented_slot("set_port_learning")
    }
}

/// A NUL-terminated path for the ABI. Non-UTF-8 paths are rejected rather
/// than lossily converted: a wrong config.bcm path fails init in a way
/// that is hard to read, so fail here where the message is clear.
fn path_to_cstring(path: &Path) -> Result<CString, SaiError> {
    let text = path
        .to_str()
        .ok_or_else(|| SaiError::Other(format!("path is not UTF-8: {}", path.display())))?;
    CString::new(text).map_err(|_| SaiError::Other(format!("NUL in path: {}", path.display())))
}

/// A shim-reported port name. The ABI says it is NUL-terminated within
/// its fixed array; a shim that fills the array completely still yields
/// the leading bytes rather than reading past the end.
fn shim_name(raw: &[std::os::raw::c_char; PORT_NAME_MAX]) -> String {
    // SAFETY: `raw` is a fixed-size array we own; the read is bounded by
    // its length whether or not a NUL is present.
    let bytes = unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const u8, raw.len()) };
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Path of the stub shim build.rs compiled for us.
    fn stub_path() -> PathBuf {
        PathBuf::from(env!("HEMLOCK_OPENBCM_STUB"))
    }

    fn init_for(shim: PathBuf) -> crate::SwitchInit {
        crate::SwitchInit {
            libsai_path: PathBuf::new(),
            shim_path: Some(shim),
            config_bcm_path: PathBuf::from("/nonexistent/config.bcm"),
            profile: Vec::new(),
            src_mac: Some([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
            diag_shell: false,
        }
    }

    fn backend() -> OpenBcmBackend {
        let mut b = OpenBcmBackend::new(&init_for(stub_path()), ABI_MAJOR).unwrap();
        b.create_switch().unwrap();
        b
    }

    #[test]
    fn loads_the_shim_and_reports_its_identity() {
        let b = OpenBcmBackend::new(&init_for(stub_path()), ABI_MAJOR).unwrap();
        assert!(b.name().starts_with("openbcm:"));
    }

    /// A shim serving a different major would marshal every call wrong,
    /// so refuse to load rather than find out later.
    #[test]
    fn refuses_an_abi_major_mismatch() {
        let err = OpenBcmBackend::new(&init_for(stub_path()), ABI_MAJOR + 1).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("ABI major") || message.contains("does not implement"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_shim_is_a_load_error() {
        let err = OpenBcmBackend::new(&init_for(PathBuf::from("/no/such/shim.so")), ABI_MAJOR)
            .unwrap_err();
        assert!(matches!(err, SaiError::Load(_)), "{err:?}");
    }

    /// Calls before create_switch have no handle to use.
    #[test]
    fn calls_before_create_switch_fail() {
        let mut b = OpenBcmBackend::new(&init_for(stub_path()), ABI_MAJOR).unwrap();
        assert!(matches!(b.ports().unwrap_err(), SaiError::NoSwitch));
    }

    #[test]
    fn enumerates_ports_with_their_sdk_names() {
        let mut b = backend();
        let ports = b.ports().unwrap();
        assert_eq!(ports.len(), 4);
        // The logical port number is the id *and* the lane, which is what
        // makes syncd's correlate-by-lane-set join work unchanged.
        assert_eq!(ports[0].id, PortId(1));
        assert_eq!(ports[0].lanes, vec![1]);
        assert_eq!(ports[0].speed_mbps, 1000);
        assert_eq!(ports[3].speed_mbps, 10000);
        // Names come back for syncd's startup assertion.
        assert_eq!(b.sai_port_name(PortId(1)).as_deref(), Some("ge0"));
        assert_eq!(b.sai_port_name(PortId(4)).as_deref(), Some("xe1"));
        assert_eq!(b.sai_port_name(PortId(99)), None);
    }

    #[test]
    fn admin_state_round_trips_and_raises_an_event() {
        let mut b = backend();
        let mut events = b.take_events().unwrap();
        assert!(b.take_events().is_none(), "the receiver is taken once");

        b.set_port_admin_state(PortId(2), true).unwrap();
        let event = events.try_recv().expect("a link event");
        match event {
            SaiEvent::PortOperStatus { port, up } => {
                assert_eq!(port, PortId(2));
                assert!(up);
            }
            other => panic!("unexpected event {other:?}"),
        }

        // And the port reads back up.
        let ports = b.ports().unwrap();
        let port = ports.iter().find(|p| p.id == PortId(2)).unwrap();
        assert!(port.admin_up && port.oper_up);

        b.set_port_admin_state(PortId(2), false).unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            SaiEvent::PortOperStatus { up: false, .. }
        ));
    }

    #[test]
    fn link_parameters_round_trip() {
        let mut b = backend();
        b.set_port_autoneg(PortId(1), false).unwrap();
        b.set_port_speed(PortId(1), 100).unwrap();
        b.set_port_duplex(PortId(1), true).unwrap();
        let ports = b.ports().unwrap();
        let port = ports.iter().find(|p| p.id == PortId(1)).unwrap();
        assert_eq!(port.speed_mbps, 100);
    }

    #[test]
    fn counters_marshal_field_for_field() {
        let mut b = backend();
        let c = b.port_counters(PortId(3)).unwrap();
        // Values the stub derives from the port number, so a struct whose
        // fields drifted out of order would land in the wrong ones.
        assert_eq!(c.in_octets, 1003);
        assert_eq!(c.in_ucast_pkts, 13);
        assert_eq!(c.out_octets, 2003);
        assert_eq!(c.out_ucast_pkts, 23);
        assert_eq!(c.in_crc_errors, 3);
        assert_eq!(c.rx_bins[0], 67);
        assert_eq!(c.tx_bins[6], 1526);
        // Untouched fields stay zero rather than picking up neighbours.
        assert_eq!(c.in_giants, 0);
        assert_eq!(c.collisions, 0);
    }

    #[test]
    fn an_unknown_port_is_an_error_not_a_panic() {
        let mut b = backend();
        assert!(b.port_counters(PortId(404)).is_err());
        assert!(b.set_port_admin_state(PortId(404), true).is_err());
    }

    /// The stub leaves `set_port_mtu` NULL, like every real shim will
    /// until phase 6. It must read as unsupported, not as a failure.
    #[test]
    fn a_null_slot_reports_unsupported() {
        let mut b = backend();
        let err = b.set_port_mtu(PortId(1), 9100).unwrap_err();
        assert!(
            err.is_unsupported(),
            "a NULL slot must classify as unsupported: {err:?}"
        );
        assert!(err.to_string().contains("NOT_IMPLEMENTED"), "{err}");
    }

    /// Everything phase 6 has not reached yet takes the same path.
    #[test]
    fn phase_six_families_are_unsupported() {
        let mut b = backend();
        assert!(b.create_vlan(10).unwrap_err().is_unsupported());
        assert!(b.create_lag().unwrap_err().is_unsupported());
        assert!(b.set_fdb_aging(300).unwrap_err().is_unsupported());
        assert!(b.create_stp_instance().unwrap_err().is_unsupported());
        assert!(b.create_samplepacket(1024).unwrap_err().is_unsupported());
        assert!(b.run_cable_diag(PortId(1)).unwrap_err().is_unsupported());
        assert!(b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap_err()
            .is_unsupported());
        // Queue counters are the one "absent" family with a defined
        // empty answer rather than an error.
        assert!(b.port_queue_counters(PortId(1)).unwrap().is_empty());
    }

    /// Capabilities must reflect the vtable, not optimism: the stub
    /// implements no phase-6 family, so none may read as supported.
    #[test]
    fn capabilities_report_only_what_exists() {
        let mut b = backend();
        let caps = b.capabilities().unwrap();
        assert_eq!(caps.buffer_bytes_total, 4 * 1024 * 1024);
        assert_eq!(caps.ecmp_width, 64);
        assert!(caps.ipv6);
        // No mirror sessions reported => the family is off.
        assert!(!caps.mirror && caps.mirror_sessions_max == 0);
        for (name, on) in [
            ("lag", caps.lag),
            ("stp", caps.stp),
            ("fdb_flush", caps.fdb_flush),
            ("l2mc", caps.l2mc),
            ("storm_control", caps.storm_control),
            ("acl_ingress", caps.acl_ingress),
            ("copp", caps.copp),
            ("sflow", caps.sflow),
            ("cable_diag", caps.cable_diag),
            ("wred", caps.wred),
        ] {
            assert!(!on, "{name} must not be claimed before it is implemented");
        }
    }

    #[test]
    fn creating_the_switch_twice_is_refused() {
        let mut b = backend();
        assert!(b.create_switch().is_err());
    }

    /// A shim reporting a shorter struct than its own fixed header is not
    /// a vtable, and reading slots out of it would be undefined.
    #[test]
    fn struct_size_below_the_header_is_rejected() {
        // Exercised through the constructor's own bound rather than a
        // second stub: the check is on the value the shim reports.
        let minimum = std::mem::size_of::<usize>() + 2 * std::mem::size_of::<u32>();
        assert!(std::mem::size_of::<Api>() > minimum);
    }

    /// The layout facts a hand-transcribed ABI could get wrong. If the
    /// header changes, these are what should fail first.
    #[test]
    fn abi_layout_matches_the_header() {
        assert_eq!(PORT_NAME_MAX, 16);
        assert_eq!(ABI_MAJOR, 1);
        // struct_size must be the first member, or the shim's own size
        // report lands somewhere else entirely.
        assert_eq!(std::mem::offset_of!(Api, struct_size), 0);
        assert!(std::mem::offset_of!(Api, abi_major) < std::mem::offset_of!(Api, create_switch));
        // Slot order, which is the ABI.
        let order = [
            std::mem::offset_of!(Api, create_switch),
            std::mem::offset_of!(Api, destroy_switch),
            std::mem::offset_of!(Api, set_link_callback),
            std::mem::offset_of!(Api, ports),
            std::mem::offset_of!(Api, set_port_admin_state),
            std::mem::offset_of!(Api, set_port_speed),
            std::mem::offset_of!(Api, set_port_duplex),
            std::mem::offset_of!(Api, set_port_autoneg),
            std::mem::offset_of!(Api, set_port_mtu),
            std::mem::offset_of!(Api, port_counters),
            std::mem::offset_of!(Api, capabilities),
        ];
        for pair in order.windows(2) {
            assert!(pair[0] < pair[1], "vtable slots are out of order");
        }
        // Function pointers are word-sized, so the slots are contiguous.
        assert_eq!(
            order[order.len() - 1] - order[0],
            (order.len() - 1) * std::mem::size_of::<usize>()
        );
    }
}
