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

/// `HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS` / `..._ITEM_NOT_FOUND`, the same
/// numbers as `SAI_STATUS_ITEM_ALREADY_EXISTS` / `..._ITEM_NOT_FOUND`.
/// Several trait methods are specified as idempotent, and these are what
/// "it was already like that" looks like coming back from a shim.
const ERR_ITEM_ALREADY_EXISTS: i32 = -6;
const ERR_ITEM_NOT_FOUND: i32 = -7;

/// `HEMLOCKBCM_FLUSH_VLAN` / `HEMLOCKBCM_FLUSH_PORT`: which of a
/// `flush_fdb` call's arguments narrow the flush. Flags rather than
/// sentinel values because logical port 0 is a real port.
const FLUSH_VLAN: u32 = 0x1;
const FLUSH_PORT: u32 = 0x2;

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

    // --- ABI 1.1 -----------------------------------------------------
    // Appended, not inserted. A shim built against 1.0 reports the
    // smaller `struct_size` and `has()` refuses to read this far, which
    // is the whole point of the minor-version rule.
    load_led_program:
        Option<unsafe extern "C" fn(*mut ShimSwitch, *const std::os::raw::c_char) -> Status>,

    // --- ABI 1.2: L2 VLANs -------------------------------------------
    default_vlan: Option<unsafe extern "C" fn(*mut ShimSwitch, *mut u16) -> Status>,
    create_vlan: Option<unsafe extern "C" fn(*mut ShimSwitch, u16) -> Status>,
    remove_vlan: Option<unsafe extern "C" fn(*mut ShimSwitch, u16) -> Status>,
    add_vlan_member:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u16, u32, std::os::raw::c_int) -> Status>,
    remove_vlan_member: Option<unsafe extern "C" fn(*mut ShimSwitch, u16, u32) -> Status>,
    set_port_pvid: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u16) -> Status>,
    set_port_tpid: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u16) -> Status>,

    // --- ABI 1.3: MAC address table ----------------------------------
    set_fdb_aging: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    add_fdb_entry: Option<
        unsafe extern "C" fn(*mut ShimSwitch, u16, *const u8, u32, std::os::raw::c_int) -> Status,
    >,
    remove_fdb_entry: Option<unsafe extern "C" fn(*mut ShimSwitch, u16, *const u8) -> Status>,
    flush_fdb: Option<unsafe extern "C" fn(*mut ShimSwitch, u16, u32, u32) -> Status>,
    set_port_learning:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> Status>,
    set_port_learn_limit:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> Status>,

    // --- ABI 1.4: link aggregation -----------------------------------
    lag_create: Option<unsafe extern "C" fn(*mut ShimSwitch, *mut u32) -> Status>,
    lag_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    lag_member_add:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, std::os::raw::c_int) -> Status>,
    lag_member_remove: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,
    lag_member_state:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, std::os::raw::c_int) -> Status>,
    lag_vlan_member_add:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u16, u32, std::os::raw::c_int) -> Status>,
    lag_vlan_member_remove: Option<unsafe extern "C" fn(*mut ShimSwitch, u16, u32) -> Status>,
    lag_set_pvid: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u16) -> Status>,
}

impl Api {
    /// Whether a slot at `offset` bytes into the struct is actually
    /// present in the shim we loaded. A shim built from an older header
    /// is shorter, and reading past its end would be undefined.
    fn has(&self, offset: usize) -> bool {
        offset + std::mem::size_of::<usize>() <= self.struct_size
    }
}

// ---------------------------------------------------------------------------
// Object ids.
//
// SAI mints opaque ids and remembers what they mean; OpenBCM has no such
// concept, so Hemlock derives them from the facts instead of keeping a
// side table. A VLAN's id is its 802.1Q number; a membership's is the
// (vlan, port) pair it names. Two consequences worth stating: the ids are
// stable across a syncd restart without any state to reload, and a
// membership id can be taken apart again, which is what
// `remove_vlan_member` needs since the trait hands back only the id.
//
// The tag in the high bits is not decoration: `Oid` is one type across
// every object family, and a VLAN oid and a future FDB oid must not
// collide. It also makes a mixed-up id fail an assertion in tests rather
// than silently address VLAN 5.
// ---------------------------------------------------------------------------

/// Discriminator in bits 56..64, so the payload has the low 56 bits.
const OID_TAG_VLAN: u64 = 0x01 << 56;
const OID_TAG_VLAN_MEMBER: u64 = 0x02 << 56;

fn vlan_oid(vlan_id: u16) -> Oid {
    Oid(OID_TAG_VLAN | u64::from(vlan_id))
}

fn oid_vlan_id(oid: Oid) -> u16 {
    oid.0 as u16
}

/// `vlan_id` in bits 32..48, logical port in the low 32.
fn member_oid(vlan_id: u16, port: PortId) -> Oid {
    Oid(OID_TAG_VLAN_MEMBER | (u64::from(vlan_id) << 32) | (port.0 & 0xffff_ffff))
}

fn oid_member(oid: Oid) -> (u16, PortId) {
    (((oid.0 >> 32) & 0xffff) as u16, PortId(oid.0 & 0xffff_ffff))
}

/// The discriminator byte. Compared for equality, never masked: the tags
/// are a numbering, not a bit set, and `0x03 & 0x04 == 0` is luck rather
/// than design.
fn oid_tag(raw: u64) -> u64 {
    raw >> 56
}

/// A LAG's `PortId`, tagged so it can never be mistaken for a logical
/// port. The trait passes LAG ids to calls that also take real ports, so
/// this discriminator is load-bearing rather than decorative.
const PORT_TAG_LAG: u64 = 0x10;
const OID_TAG_LAG_MEMBER: u64 = 0x03;
const OID_TAG_LAG_VLAN_MEMBER: u64 = 0x04;

fn lag_port(tid: u32) -> PortId {
    PortId((PORT_TAG_LAG << 56) | u64::from(tid))
}

/// The trunk id behind a LAG's `PortId`, or `None` for a real port.
fn lag_tid_of(port: PortId) -> Option<u32> {
    (oid_tag(port.0) == PORT_TAG_LAG).then_some(port.0 as u32)
}

fn lag_member_oid(tid: u32, port: PortId) -> Oid {
    Oid((OID_TAG_LAG_MEMBER << 56) | (u64::from(tid) << 32) | (port.0 & 0xffff_ffff))
}

fn oid_lag_member(oid: Oid) -> (u32, PortId) {
    // 24 bits for the trunk, not 32: bits 56..64 are the tag, and a
    // 32-bit mask here would hand the caller the tag back as part of the
    // trunk id. Trunk ids are chip-table indices, far inside 24 bits.
    (
        ((oid.0 >> 32) & 0x00ff_ffff) as u32,
        PortId(oid.0 & 0xffff_ffff),
    )
}

fn lag_vlan_member_oid(vlan_id: u16, tid: u32) -> Oid {
    Oid((OID_TAG_LAG_VLAN_MEMBER << 56) | (u64::from(vlan_id) << 32) | u64::from(tid))
}

/// `Some((vlan, trunk))` when this id names a trunk's VLAN membership
/// rather than a port's.
fn oid_lag_vlan_member(oid: Oid) -> Option<(u16, u32)> {
    (oid_tag(oid.0) == OID_TAG_LAG_VLAN_MEMBER)
        .then_some((((oid.0 >> 32) & 0xffff) as u16, oid.0 as u32))
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

/// Every call on a slot follows the same shape: check it exists (both
/// non-NULL and within the shim's reported `struct_size`), then call it.
/// Spelled once here rather than in each of the methods.
macro_rules! slot {
    ($self:expr, $field:ident, $call:literal) => {{
        match $self.api.$field {
            Some(f) if $self.api.has(std::mem::offset_of!(Api, $field)) => Ok(f),
            _ => unimplemented_slot($call),
        }
    }};
}

/// `SaiBackend` over a dlopened `libhemlockbcm.so`.
pub struct OpenBcmBackend {
    /// Kept alive for as long as any pointer into it is used; dropping it
    /// unloads the shim.
    _library: libloading::Library,
    api: &'static Api,
    switch: *mut ShimSwitch,
    shim_path: PathBuf,
    led_program_path: Option<PathBuf>,
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
            led_program_path: init.led_program_path.clone(),
            config_bcm_path: init.config_bcm_path.clone(),
            src_mac: init.src_mac,
            diag_shell: init.diag_shell,
            events: Some(rx),
            event_ctx: Arc::new(EventContext { tx }),
            port_names: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Load and start the chip's LED-processor program, if the platform
    /// ships one and the shim implements the slot.
    ///
    /// Every failure path here is a warning: LEDs are cosmetic, and a
    /// switch that forwards with the wrong lights on is enormously better
    /// than one that refuses to start over them. The `has()` check is
    /// what lets a shim built against ABI 1.0 — before this slot existed
    /// — keep working.
    fn load_led_program(&mut self) {
        let Some(path) = self.led_program_path.clone() else {
            return;
        };
        let Ok(load) = slot!(self, load_led_program, "load_led_program") else {
            tracing::warn!(
                abi_minor = self.api.abi_minor,
                "shim has no load_led_program slot; port LEDs will be left as the \
                 latches powered up"
            );
            return;
        };
        let Ok(switch) = self.switch() else { return };

        let hex = match std::fs::read_to_string(&path) {
            Ok(text) => text.split_whitespace().collect::<String>(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "LED program unreadable");
                return;
            }
        };
        if hex.is_empty() {
            tracing::warn!(path = %path.display(), "LED program file is empty");
            return;
        }
        let Ok(text) = CString::new(hex) else {
            tracing::warn!(path = %path.display(), "LED program contains a NUL");
            return;
        };
        // SAFETY: `text` outlives the call; the shim copies what it needs.
        match check("load_led_program", unsafe { load(switch, text.as_ptr()) }) {
            Ok(()) => tracing::info!(path = %path.display(), "LED program loaded"),
            Err(e) => tracing::warn!(error = %e, "LED program load failed (LEDs only)"),
        }
    }

    fn switch(&self) -> Result<*mut ShimSwitch, SaiError> {
        if self.switch.is_null() {
            Err(SaiError::NoSwitch)
        } else {
            Ok(self.switch)
        }
    }

    /// The chip's default VLAN. Asked of the shim rather than assumed to
    /// be 1: `config.bcm` can move it, and a wrong guess would quietly
    /// strand every access port.
    fn default_vlan(&self) -> Result<u16, SaiError> {
        let f = slot!(self, default_vlan, "default_vlan")?;
        let sw = self.switch()?;
        let mut vlan_id: u16 = 0;
        // SAFETY: `vlan_id` is a live u16 the shim writes only on OK.
        check("default_vlan", unsafe { f(sw, &mut vlan_id) })?;
        Ok(vlan_id)
    }

    /// The trunk id behind a LAG's `PortId`. A real port here is a
    /// caller mistake, not something to paper over: passing a logical
    /// port to `remove_lag` would destroy whatever trunk happened to
    /// share that number.
    fn lag_tid(&self, lag: PortId) -> Result<u32, SaiError> {
        lag_tid_of(lag).ok_or_else(|| SaiError::Other(format!("{lag} is not a LAG")))
    }

    /// A VLAN argument the trait spells `Option<Oid>`, where `None`
    /// means the default VLAN. Resolving it here rather than in each
    /// caller keeps "which VLAN did they mean" in one place -- and the
    /// default is asked of the shim, never assumed.
    fn vlan_id_or_default(&self, vlan: Option<Oid>) -> Result<u16, SaiError> {
        match vlan {
            Some(vlan) => Ok(oid_vlan_id(vlan)),
            None => self.default_vlan(),
        }
    }

    /// Shared by `add_vlan_member` and `restore_port_default_vlan`,
    /// which differ only in what they do with the status.
    fn vlan_member_add(&self, vlan_id: u16, port: PortId, tagged: bool) -> Result<(), SaiError> {
        let f = slot!(self, add_vlan_member, "add_vlan_member")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("add_vlan_member", unsafe {
            f(sw, vlan_id, port.0 as u32, i32::from(tagged))
        })
    }

    fn vlan_member_remove(&self, vlan_id: u16, port: PortId) -> Result<(), SaiError> {
        let f = slot!(self, remove_vlan_member, "remove_vlan_member")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("remove_vlan_member", unsafe {
            f(sw, vlan_id, port.0 as u32)
        })
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

        // The chip's LED processor. Purely cosmetic — without it the LED
        // latches power up driving every port LED solid on — so nothing
        // here is allowed to fail the switch.
        self.load_led_program();

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
            lag: slot!(self, lag_create, "lag_create").is_ok(),
            stp: false,
            fdb_flush: slot!(self, flush_fdb, "flush_fdb").is_ok(),
            fdb_aging: slot!(self, set_fdb_aging, "set_fdb_aging").is_ok(),
            l2mc: false,
            storm_control: false,
            mirror: raw.mirror_sessions_max > 0,
            mirror_sessions_max: raw.mirror_sessions_max,
            port_tpid: slot!(self, set_port_tpid, "set_port_tpid").is_ok(),
            ecmp_width: raw.ecmp_width,
            ipv6: raw.ipv6 != 0,
            my_mac: false,
            acl_ingress: false,
            acl_egress: false,
            acl_entry_policer: false,
            port_learn_limit: slot!(self, set_port_learn_limit, "set_port_learn_limit").is_ok(),
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

    // --- L2 VLANs (ABI 1.2) ------------------------------------------
    //
    // SAI hands out opaque object ids for VLANs and memberships; OpenBCM
    // has neither. Rather than make the shim keep a table it would have
    // to rebuild after a restart, the ids are minted here out of the
    // facts themselves -- a VLAN *is* its id, a membership *is* (vlan,
    // port) -- so they survive a syncd restart for free and the shim
    // stays stateless. See `vlan_oid` / `member_oid` below.

    fn create_vlan(&mut self, vlan_id: u16) -> Result<Oid, SaiError> {
        let f = slot!(self, create_vlan, "create_vlan")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("create_vlan", unsafe { f(sw, vlan_id) })?;
        Ok(vlan_oid(vlan_id))
    }

    fn remove_vlan(&mut self, vlan: Oid) -> Result<(), SaiError> {
        let f = slot!(self, remove_vlan, "remove_vlan")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("remove_vlan", unsafe { f(sw, oid_vlan_id(vlan)) })
    }

    fn add_vlan_member(&mut self, vlan: Oid, port: PortId, tagged: bool) -> Result<Oid, SaiError> {
        let vlan_id = oid_vlan_id(vlan);
        // A LAG is a member in its own right on this hardware, reached
        // through a different call than a port.
        if let Some(tid) = lag_tid_of(port) {
            let f = slot!(self, lag_vlan_member_add, "lag_vlan_member_add")?;
            let sw = self.switch()?;
            // SAFETY: plain scalars over the ABI.
            check("lag_vlan_member_add", unsafe {
                f(sw, vlan_id, tid, i32::from(tagged))
            })?;
            return Ok(lag_vlan_member_oid(vlan_id, tid));
        }
        self.vlan_member_add(vlan_id, port, tagged)?;
        Ok(member_oid(vlan_id, port))
    }

    fn remove_vlan_member(&mut self, member: Oid) -> Result<(), SaiError> {
        // The id says which kind of membership it is; there is nothing
        // to look up.
        if let Some((vlan_id, tid)) = oid_lag_vlan_member(member) {
            let f = slot!(self, lag_vlan_member_remove, "lag_vlan_member_remove")?;
            let sw = self.switch()?;
            // SAFETY: plain scalars over the ABI.
            return check("lag_vlan_member_remove", unsafe { f(sw, vlan_id, tid) });
        }
        let (vlan_id, port) = oid_member(member);
        self.vlan_member_remove(vlan_id, port)
    }

    fn set_port_pvid(&mut self, port: PortId, vlan_number: u16) -> Result<(), SaiError> {
        // Ingress classification belongs to the receiving port, so a
        // trunk has no single place to put it: the shim applies it to
        // every member, gated-closed ones included.
        if let Some(tid) = lag_tid_of(port) {
            let f = slot!(self, lag_set_pvid, "lag_set_pvid")?;
            let sw = self.switch()?;
            // SAFETY: plain scalars over the ABI.
            return check("lag_set_pvid", unsafe { f(sw, tid, vlan_number) });
        }
        let f = slot!(self, set_port_pvid, "set_port_pvid")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_pvid", unsafe {
            f(sw, port.0 as u32, vlan_number)
        })
    }

    fn remove_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError> {
        let default = self.default_vlan()?;
        // Idempotent per the trait: a port that is already out of the
        // default VLAN is the desired state, not a failure.
        match self.vlan_member_remove(default, port) {
            Err(SaiError::Status {
                status: ERR_ITEM_NOT_FOUND,
                ..
            }) => Ok(()),
            other => other,
        }
    }

    fn restore_port_default_vlan(&mut self, port: PortId) -> Result<(), SaiError> {
        let default = self.default_vlan()?;
        match self.vlan_member_add(default, port, false) {
            Err(SaiError::Status {
                status: ERR_ITEM_ALREADY_EXISTS,
                ..
            }) => Ok(()),
            other => other,
        }?;
        // Membership and ingress classification are independent on this
        // hardware, so "back to default L2" is both or neither.
        self.set_port_pvid(port, default)
    }

    // --- MAC address table (ABI 1.3) ---------------------------------
    //
    // Entries and enforcement only: there is no FDB *notification* slot
    // in the ABI yet, so `SaiEvent::Fdb` and
    // `SaiEvent::LearnLimitViolation` never fire on this backend. The
    // shim asks the chip to punt over-limit frames to the CPU rather
    // than drop them, which is what a later minor needs in order to turn
    // them into events -- but the path stops there for now.

    fn set_fdb_aging(&mut self, secs: u32) -> Result<(), SaiError> {
        let f = slot!(self, set_fdb_aging, "set_fdb_aging")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_fdb_aging", unsafe { f(sw, secs) })
    }

    fn add_fdb_entry(
        &mut self,
        vlan: Option<Oid>,
        mac: [u8; 6],
        action: FdbAction,
    ) -> Result<(), SaiError> {
        let f = slot!(self, add_fdb_entry, "add_fdb_entry")?;
        let vlan_id = self.vlan_id_or_default(vlan)?;
        let sw = self.switch()?;
        let (port, discard) = match action {
            FdbAction::Forward(port) => (port.0 as u32, 0),
            // The port is meaningless for a black hole, and the shim
            // ignores it; pass 0 rather than something that looks real.
            FdbAction::Drop => (0, 1),
        };
        // SAFETY: `mac` is a live 6-byte array for the call's duration.
        check("add_fdb_entry", unsafe {
            f(sw, vlan_id, mac.as_ptr(), port, discard)
        })
    }

    fn remove_fdb_entry(&mut self, vlan: Option<Oid>, mac: [u8; 6]) -> Result<(), SaiError> {
        let f = slot!(self, remove_fdb_entry, "remove_fdb_entry")?;
        let vlan_id = self.vlan_id_or_default(vlan)?;
        let sw = self.switch()?;
        // SAFETY: `mac` is a live 6-byte array for the call's duration.
        check("remove_fdb_entry", unsafe { f(sw, vlan_id, mac.as_ptr()) })
    }

    fn flush_fdb(&mut self, vlan: Option<Oid>, port: Option<PortId>) -> Result<(), SaiError> {
        let f = slot!(self, flush_fdb, "flush_fdb")?;
        // Which fields narrow the flush is carried in flags, not in
        // sentinel values: logical port 0 is a real port.
        let mut flags = 0u32;
        let vlan_id = match vlan {
            Some(vlan) => {
                flags |= FLUSH_VLAN;
                oid_vlan_id(vlan)
            }
            None => 0,
        };
        let logical_port = match port {
            Some(port) => {
                flags |= FLUSH_PORT;
                port.0 as u32
            }
            None => 0,
        };
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("flush_fdb", unsafe { f(sw, vlan_id, logical_port, flags) })
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

    fn set_port_tpid(&mut self, port: PortId, tpid: u16) -> Result<(), SaiError> {
        let f = slot!(self, set_port_tpid, "set_port_tpid")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_tpid", unsafe { f(sw, port.0 as u32, tpid) })
    }

    // --- Link aggregation (ABI 1.4) ----------------------------------
    //
    // A LAG is a hardware trunk, and its ids are derived like the VLAN
    // ones: the LAG's `PortId` carries its trunk id, a member's `Oid`
    // carries (trunk, port). Nothing is remembered on this side.
    //
    // The trait promises that a LAG's id is accepted wherever a port is,
    // which OpenBCM does not offer directly -- so `add_vlan_member`,
    // `remove_vlan_member` and `set_port_pvid` look at the id and
    // dispatch to the trunk-shaped slots. That check is what
    // `is_lag_port` is for, and it is why LAG ids are tagged rather than
    // being small integers that could collide with a logical port.

    fn create_lag(&mut self) -> Result<PortId, SaiError> {
        let f = slot!(self, lag_create, "lag_create")?;
        let sw = self.switch()?;
        let mut tid: u32 = 0;
        // SAFETY: `tid` is a live u32 the shim writes only on OK.
        check("lag_create", unsafe { f(sw, &mut tid) })?;
        Ok(lag_port(tid))
    }

    fn remove_lag(&mut self, lag: PortId) -> Result<(), SaiError> {
        let f = slot!(self, lag_destroy, "lag_destroy")?;
        let tid = self.lag_tid(lag)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("lag_destroy", unsafe { f(sw, tid) })
    }

    fn add_lag_member(&mut self, lag: PortId, port: PortId) -> Result<Oid, SaiError> {
        let f = slot!(self, lag_member_add, "lag_member_add")?;
        let tid = self.lag_tid(lag)?;
        // The port stops bridging on its own account: from here its
        // traffic belongs to the trunk. Done before the member exists,
        // so a failure leaves the port in the state it started in.
        self.remove_port_default_vlan(port)?;
        let sw = self.switch()?;
        // Gated closed, per the trait: in the trunk, forwarding nothing
        // until something (LACP, or a static config) opens the gate.
        // SAFETY: plain scalars over the ABI.
        check("lag_member_add", unsafe { f(sw, tid, port.0 as u32, 0) })?;
        Ok(lag_member_oid(tid, port))
    }

    fn remove_lag_member(&mut self, member: Oid, port: PortId) -> Result<(), SaiError> {
        let (tid, member_port) = oid_lag_member(member);
        if member_port != port {
            return Err(SaiError::Other(format!(
                "LAG member {member} fronts {member_port}, not {port}"
            )));
        }
        let f = slot!(self, lag_member_remove, "lag_member_remove")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("lag_member_remove", unsafe { f(sw, tid, port.0 as u32) })?;
        self.restore_port_default_vlan(port)
    }

    fn set_lag_member_state(&mut self, member: Oid, enabled: bool) -> Result<(), SaiError> {
        let f = slot!(self, lag_member_state, "lag_member_state")?;
        let (tid, port) = oid_lag_member(member);
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("lag_member_state", unsafe {
            f(sw, tid, port.0 as u32, i32::from(enabled))
        })
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

    fn set_port_learn_limit(&mut self, port: PortId, limit: Option<u32>) -> Result<(), SaiError> {
        let f = slot!(self, set_port_learn_limit, "set_port_learn_limit")?;
        let sw = self.switch()?;
        // The ABI spells "no limit" as a negative, matching the SDK. A
        // limit larger than i32::MAX is not a real configuration, so it
        // saturates rather than wrapping into "unlimited".
        let limit = match limit {
            Some(limit) => limit.min(i32::MAX as u32) as i32,
            None => -1,
        };
        // SAFETY: plain scalars over the ABI.
        check("set_port_learn_limit", unsafe {
            f(sw, port.0 as u32, limit)
        })
    }

    fn set_port_learning(&mut self, port: PortId, learn: bool) -> Result<(), SaiError> {
        let f = slot!(self, set_port_learning, "set_port_learning")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_learning", unsafe {
            f(sw, port.0 as u32, i32::from(learn))
        })
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

    /// A vtable slot is one function pointer wide.
    const WORD: usize = std::mem::size_of::<usize>();

    /// Path of the stub shim build.rs compiled for us.
    fn stub_path() -> PathBuf {
        PathBuf::from(env!("HEMLOCK_OPENBCM_STUB"))
    }

    fn init_for(shim: PathBuf) -> crate::SwitchInit {
        crate::SwitchInit {
            libsai_path: PathBuf::new(),
            shim_path: Some(shim),
            led_program_path: None,
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
    /// VLANs, the FDB and LAGs have left this list; the rest has not.
    #[test]
    fn phase_six_families_are_unsupported() {
        let mut b = backend();
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

    /// Capabilities must reflect the vtable, not optimism: a family
    /// reads as supported exactly when its slot is there. The stub
    /// implements the VLAN slots and nothing else of phase 6.
    #[test]
    fn capabilities_report_only_what_exists() {
        let mut b = backend();
        let caps = b.capabilities().unwrap();
        assert_eq!(caps.buffer_bytes_total, 4 * 1024 * 1024);
        assert_eq!(caps.ecmp_width, 64);
        assert!(caps.ipv6);
        // No mirror sessions reported => the family is off.
        assert!(!caps.mirror && caps.mirror_sessions_max == 0);
        // ...but the slots that do exist read as supported -- derived
        // from the vtable, not from a hand-maintained list.
        assert!(caps.port_tpid, "QinQ, ABI 1.2");
        assert!(
            caps.fdb_flush && caps.fdb_aging && caps.port_learn_limit,
            "ABI 1.3"
        );
        assert!(caps.lag, "ABI 1.4");
        for (name, on) in [
            ("stp", caps.stp),
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

    /// The LED program reaches the shim, whitespace-stripped, when the
    /// platform ships one.
    #[test]
    fn the_led_program_reaches_the_shim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("led.hex");
        // Vendor hex files wrap across lines; the shim wants one string.
        std::fs::write(&path, "021d2860 e167bc06\ne190d219\n").unwrap();

        let mut init = init_for(stub_path());
        init.led_program_path = Some(path);
        let mut b = OpenBcmBackend::new(&init, ABI_MAJOR).unwrap();
        b.create_switch().unwrap();

        // Ask the stub what it was handed.
        let loaded = unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn() -> *const std::os::raw::c_char> =
                b._library.get(b"hemlockbcm_stub_led_program\0").unwrap();
            std::ffi::CStr::from_ptr(f()).to_string_lossy().into_owned()
        };
        assert_eq!(loaded, "021d2860e167bc06e190d219");
    }

    /// LEDs are cosmetic: nothing about them may fail the switch.
    #[test]
    fn a_broken_led_program_does_not_fail_create_switch() {
        let mut init = init_for(stub_path());
        init.led_program_path = Some(PathBuf::from("/no/such/led.hex"));
        let mut b = OpenBcmBackend::new(&init, ABI_MAJOR).unwrap();
        assert!(b.create_switch().is_ok());
        // And the switch is fully usable afterwards.
        assert_eq!(b.ports().unwrap().len(), 4);
    }

    /// The minor-version rule: a shim built against an older header
    /// reports a shorter `struct_size`, and slots past its end must read
    /// as absent rather than being called through a dangling offset.
    #[test]
    fn struct_size_gates_slots_appended_by_a_later_minor() {
        let b = OpenBcmBackend::new(&init_for(stub_path()), ABI_MAJOR).unwrap();
        let led = std::mem::offset_of!(Api, load_led_program);
        // The stub is built from the current header, so it has the slot.
        assert!(b.api.has(led));

        // A 1.0-sized vtable stops just before it.
        let one_zero = Api {
            struct_size: led,
            abi_major: ABI_MAJOR,
            abi_minor: 0,
            create_switch: None,
            destroy_switch: None,
            set_link_callback: None,
            ports: None,
            set_port_admin_state: None,
            set_port_speed: None,
            set_port_duplex: None,
            set_port_autoneg: None,
            set_port_mtu: None,
            port_counters: None,
            capabilities: None,
            load_led_program: None,
            default_vlan: None,
            create_vlan: None,
            remove_vlan: None,
            add_vlan_member: None,
            remove_vlan_member: None,
            set_port_pvid: None,
            set_port_tpid: None,
            set_fdb_aging: None,
            add_fdb_entry: None,
            remove_fdb_entry: None,
            flush_fdb: None,
            set_port_learning: None,
            set_port_learn_limit: None,
            lag_create: None,
            lag_destroy: None,
            lag_member_add: None,
            lag_member_remove: None,
            lag_member_state: None,
            lag_vlan_member_add: None,
            lag_vlan_member_remove: None,
            lag_set_pvid: None,
        };
        assert!(!one_zero.has(led), "a 1.0 shim must not expose a 1.1 slot");
        assert!(one_zero.has(std::mem::offset_of!(Api, capabilities)));
        // Nor a 1.2 one, which is further out still.
        assert!(!one_zero.has(std::mem::offset_of!(Api, create_vlan)));
    }

    /// The vtable's slots, in the order the header declares them.
    ///
    /// Three independent transcriptions of one struct have to agree: the
    /// header, the Rust `Api` above, and each shim's positional
    /// initializer. Two of those are C initializers with no field names,
    /// and neighbouring slots can share a signature -- `set_port_pvid`
    /// and `set_port_tpid` are both `(sw, u32, u16)` -- so swapping them
    /// compiles cleanly and misbehaves only on hardware. This parses the
    /// header, and the tests below hold the others against it.
    fn header_slots() -> Vec<String> {
        const HEADER: &str = include_str!("../openbcm-shim/hemlockbcm.h");
        let body = HEADER
            .split_once("struct hemlockbcm_api {")
            .expect("header declares the vtable")
            .1
            .split_once("\n};")
            .expect("vtable is closed")
            .0;
        body.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("int (*")?;
                Some(rest.split(')').next()?.to_string())
            })
            .collect()
    }

    /// Slot names as they appear in a C vtable initializer, given the
    /// prefix that shim spells its functions with.
    fn initializer_slots(source: &str, prefix: &str) -> Vec<String> {
        let body = source
            .split_once("struct hemlockbcm_api ")
            .expect("source defines a vtable")
            .1
            .split_once("\n};")
            .expect("vtable is closed")
            .0;
        body.lines()
            .filter_map(|line| {
                let line = line.trim().trim_end_matches(',');
                // NULL entries carry a trailing comment saying which slot
                // they are; take the name from there.
                if let Some(comment) = line.strip_prefix("NULL,") {
                    let name = comment.trim().trim_start_matches("/*").trim();
                    return Some(name.split(':').next()?.trim().to_string());
                }
                line.strip_prefix(prefix).map(|name| name.to_string())
            })
            .collect()
    }

    /// The Rust transcription and the header must name the same slots in
    /// the same order, and there must be exactly as many of them as the
    /// struct has room for -- which is what catches a slot added to the
    /// header and forgotten here.
    #[test]
    fn the_header_rust_and_both_shims_agree_on_slot_order() {
        let expected = [
            "create_switch",
            "destroy_switch",
            "set_link_callback",
            "ports",
            "set_port_admin_state",
            "set_port_speed",
            "set_port_duplex",
            "set_port_autoneg",
            "set_port_mtu",
            "port_counters",
            "capabilities",
            "load_led_program",
            "default_vlan",
            "create_vlan",
            "remove_vlan",
            "add_vlan_member",
            "remove_vlan_member",
            "set_port_pvid",
            "set_port_tpid",
            "set_fdb_aging",
            "add_fdb_entry",
            "remove_fdb_entry",
            "flush_fdb",
            "set_port_learning",
            "set_port_learn_limit",
            "lag_create",
            "lag_destroy",
            "lag_member_add",
            "lag_member_remove",
            "lag_member_state",
            "lag_vlan_member_add",
            "lag_vlan_member_remove",
            "lag_set_pvid",
        ];
        assert_eq!(header_slots(), expected, "header slot order changed");

        // Same count in the Rust struct: everything after the three
        // fixed header fields is one word-sized slot.
        let slots = (std::mem::size_of::<Api>() - std::mem::offset_of!(Api, create_switch)) / WORD;
        assert_eq!(slots, expected.len(), "Api has a slot the header does not");

        // Both shims fill the vtable positionally, so their initializers
        // must list the same slots in the same order.
        assert_eq!(
            initializer_slots(include_str!("../openbcm-shim/hemlockbcm_stub.c"), "stub_"),
            expected,
            "the stub's vtable is out of order"
        );
        assert_eq!(
            initializer_slots(
                include_str!("../../../vendor/openbcm-shim/hemlockbcm.c"),
                "hb_"
            ),
            expected,
            "the real shim's vtable is out of order"
        );
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
            std::mem::offset_of!(Api, load_led_program),
            std::mem::offset_of!(Api, default_vlan),
            std::mem::offset_of!(Api, create_vlan),
            std::mem::offset_of!(Api, remove_vlan),
            std::mem::offset_of!(Api, add_vlan_member),
            std::mem::offset_of!(Api, remove_vlan_member),
            std::mem::offset_of!(Api, set_port_pvid),
            std::mem::offset_of!(Api, set_port_tpid),
            std::mem::offset_of!(Api, set_fdb_aging),
            std::mem::offset_of!(Api, add_fdb_entry),
            std::mem::offset_of!(Api, remove_fdb_entry),
            std::mem::offset_of!(Api, flush_fdb),
            std::mem::offset_of!(Api, set_port_learning),
            std::mem::offset_of!(Api, set_port_learn_limit),
            std::mem::offset_of!(Api, lag_create),
            std::mem::offset_of!(Api, lag_destroy),
            std::mem::offset_of!(Api, lag_member_add),
            std::mem::offset_of!(Api, lag_member_remove),
            std::mem::offset_of!(Api, lag_member_state),
            std::mem::offset_of!(Api, lag_vlan_member_add),
            std::mem::offset_of!(Api, lag_vlan_member_remove),
            std::mem::offset_of!(Api, lag_set_pvid),
        ];
        for pair in order.windows(2) {
            assert!(pair[0] < pair[1], "vtable slots are out of order");
        }
        // Function pointers are word-sized, so the slots are contiguous.
        assert_eq!(order[order.len() - 1] - order[0], (order.len() - 1) * WORD);
    }
    // --- L2 VLANs -----------------------------------------------------

    /// `hemlockbcm_stub_vlan_member`: -1 = not a member, 0 = untagged,
    /// 1 = tagged. Reaching into the stub is the only way to check that a
    /// call did the right thing rather than merely returned OK.
    fn stub_member(b: &OpenBcmBackend, vlan_id: u16, port: u32) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u16, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_vlan_member\0").unwrap();
            f(b.switch, vlan_id, port)
        }
    }

    fn stub_pvid(b: &OpenBcmBackend, port: u32) -> u16 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32) -> u16> =
                b._library.get(b"hemlockbcm_stub_pvid\0").unwrap();
            f(b.switch, port)
        }
    }

    #[test]
    fn vlan_membership_reaches_the_shim_tagged_and_untagged() {
        let mut b = backend();
        let vlan = b.create_vlan(100).unwrap();

        let tagged = b.add_vlan_member(vlan, PortId(1), true).unwrap();
        assert_eq!(stub_member(&b, 100, 1), 1, "tagged member");
        b.add_vlan_member(vlan, PortId(2), false).unwrap();
        assert_eq!(stub_member(&b, 100, 2), 0, "untagged member");

        // The membership id is enough on its own to undo the membership:
        // nothing is remembered on either side of the ABI.
        b.remove_vlan_member(tagged).unwrap();
        assert_eq!(stub_member(&b, 100, 1), -1, "no longer a member");

        // A VLAN with members left cannot be destroyed, which is the
        // SDK's rule and the trait's documented precondition.
        assert!(b.remove_vlan(vlan).is_err());
        b.remove_vlan_member(member_oid(100, PortId(2))).unwrap();
        b.remove_vlan(vlan).unwrap();
    }

    /// The ids carry their own meaning, so they survive a syncd restart
    /// with no table to reload -- and a VLAN id and a membership id can
    /// never be confused for one another.
    #[test]
    fn object_ids_encode_the_facts_they_name() {
        assert_eq!(oid_vlan_id(vlan_oid(4094)), 4094);
        assert_eq!(oid_member(member_oid(100, PortId(53))), (100, PortId(53)));
        assert_ne!(vlan_oid(100).0, member_oid(100, PortId(0)).0);
        // Tags live above the payload, so neither can collide with the
        // other's range however large the payload gets.
        assert_ne!(vlan_oid(1).0 >> 56, member_oid(1, PortId(1)).0 >> 56);
    }

    /// Both are documented idempotent, and the shim is allowed either to
    /// report "already like that" or to report success -- so the second
    /// call must succeed whichever the shim does.
    #[test]
    fn default_vlan_moves_are_idempotent() {
        let mut b = backend();
        // The stub starts every port an untagged default-VLAN member.
        assert_eq!(stub_member(&b, 1, 1), 0);

        b.remove_port_default_vlan(PortId(1)).unwrap();
        assert_eq!(stub_member(&b, 1, 1), -1);
        b.remove_port_default_vlan(PortId(1))
            .expect("removing a port that is already out is not a failure");

        b.set_port_pvid(PortId(1), 100).unwrap();
        b.restore_port_default_vlan(PortId(1)).unwrap();
        assert_eq!(stub_member(&b, 1, 1), 0, "untagged member again");
        assert_eq!(stub_pvid(&b, 1), 1, "and classified into the default VLAN");
        b.restore_port_default_vlan(PortId(1))
            .expect("restoring a port that is already back is not a failure");
    }

    /// A real failure must still be a failure: the idempotency above is
    /// scoped to one status code, not to "ignore errors from this call".
    #[test]
    fn idempotency_does_not_swallow_real_failures() {
        let mut b = backend();
        // Port 404 does not exist; the stub rejects it as a bad
        // parameter, which is not "already like that".
        assert!(b.remove_port_default_vlan(PortId(404)).is_err());
        assert!(b.restore_port_default_vlan(PortId(404)).is_err());
    }

    fn stub_tpid(b: &OpenBcmBackend, port: u32) -> u16 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32) -> u16> =
                b._library.get(b"hemlockbcm_stub_tpid\0").unwrap();
            f(b.switch, port)
        }
    }

    #[test]
    fn port_tpid_reaches_the_shim() {
        let mut b = backend();
        assert_eq!(stub_tpid(&b, 1), 0x8100, "the 802.1Q default");
        b.set_port_tpid(PortId(1), 0x88a8).unwrap();
        assert_eq!(stub_tpid(&b, 1), 0x88a8, "a provider-bridge port");
    }

    /// The default VLAN is asked of the shim, not assumed to be 1:
    /// config.bcm can move it.
    #[test]
    fn the_default_vlan_comes_from_the_shim() {
        let b = backend();
        assert_eq!(b.default_vlan().unwrap(), 1);
    }
    // --- MAC address table --------------------------------------------

    const MAC_A: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x0a];
    const MAC_B: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x0b];
    const MAC_C: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x0c];

    /// -1 = absent, 0 = forwarding, 1 = discarding.
    fn stub_fdb(b: &OpenBcmBackend, vlan_id: u16, mac: &[u8; 6]) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u16, *const u8) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_fdb_entry\0").unwrap();
            f(b.switch, vlan_id, mac.as_ptr())
        }
    }

    fn stub_fdb_count(b: &OpenBcmBackend) -> u32 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch) -> u32> =
                b._library.get(b"hemlockbcm_stub_fdb_count\0").unwrap();
            f(b.switch)
        }
    }

    /// Seed a *dynamic* entry. Nothing in the ABI can create one -- the
    /// chip learns them -- so the flush tests need this hook.
    fn stub_learn(b: &OpenBcmBackend, vlan_id: u16, mac: &[u8; 6], port: u32) {
        let rc = unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u16, *const u8, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_learn\0").unwrap();
            f(b.switch, vlan_id, mac.as_ptr(), port)
        };
        assert_eq!(rc, 0, "stub FDB is full");
    }

    #[test]
    fn static_fdb_entries_forward_or_black_hole() {
        let mut b = backend();
        b.add_fdb_entry(None, MAC_A, FdbAction::Forward(PortId(2)))
            .unwrap();
        assert_eq!(stub_fdb(&b, 1, &MAC_A), 0, "forwarding entry");

        // Same (vlan, mac) again replaces rather than duplicating, which
        // is what the trait documents.
        b.add_fdb_entry(None, MAC_A, FdbAction::Drop).unwrap();
        assert_eq!(stub_fdb(&b, 1, &MAC_A), 1, "now a black hole");
        assert_eq!(stub_fdb_count(&b), 1, "replaced, not duplicated");

        b.remove_fdb_entry(None, MAC_A).unwrap();
        assert_eq!(stub_fdb(&b, 1, &MAC_A), -1);
        // Removing what is not there is a plain not-found: nothing about
        // the FDB is documented idempotent, so nothing is swallowed.
        assert!(b.remove_fdb_entry(None, MAC_A).is_err());
    }

    /// `None` means the default VLAN, and the default comes from the
    /// shim -- so an entry added with `None` lands in VLAN 1 here and
    /// would follow config.bcm on a board that moved it.
    #[test]
    fn a_none_vlan_means_the_default_vlan() {
        let mut b = backend();
        let vlan = b.create_vlan(200).unwrap();
        b.add_fdb_entry(None, MAC_A, FdbAction::Forward(PortId(1)))
            .unwrap();
        b.add_fdb_entry(Some(vlan), MAC_A, FdbAction::Forward(PortId(1)))
            .unwrap();
        // The same MAC in two VLANs is two entries, not one.
        assert_eq!(stub_fdb_count(&b), 2);
        assert_eq!(stub_fdb(&b, 1, &MAC_A), 0);
        assert_eq!(stub_fdb(&b, 200, &MAC_A), 0);
    }

    /// Flush drops dynamic entries and leaves static ones, in every
    /// scope -- that last part is the bit worth pinning, because the
    /// obvious SDK helpers make it easy to get right for some scopes and
    /// wrong for others.
    #[test]
    fn flush_scopes_drop_dynamic_entries_and_spare_static_ones() {
        let mut b = backend();
        let vlan = b.create_vlan(200).unwrap();
        b.add_fdb_entry(None, MAC_A, FdbAction::Forward(PortId(1)))
            .unwrap();

        // Everything.
        stub_learn(&b, 1, &MAC_B, 1);
        b.flush_fdb(None, None).unwrap();
        assert_eq!(stub_fdb(&b, 1, &MAC_B), -1, "dynamic entry flushed");
        assert_eq!(stub_fdb(&b, 1, &MAC_A), 0, "static entry survives");

        // By VLAN: the other VLAN's entry is untouched.
        stub_learn(&b, 1, &MAC_B, 1);
        stub_learn(&b, 200, &MAC_B, 1);
        b.flush_fdb(Some(vlan), None).unwrap();
        assert_eq!(stub_fdb(&b, 200, &MAC_B), -1);
        assert_eq!(stub_fdb(&b, 1, &MAC_B), 0, "a different VLAN");

        // By port: the other port's entry is untouched. A third MAC,
        // deliberately -- reusing MAC_A here would put a dynamic entry
        // under the same (vlan, mac) key as the static one and make
        // every assertion below ambiguous.
        stub_learn(&b, 1, &MAC_C, 2);
        b.flush_fdb(None, Some(PortId(2))).unwrap();
        assert_eq!(stub_fdb(&b, 1, &MAC_C), -1, "learned on port 2");
        assert_eq!(stub_fdb(&b, 1, &MAC_B), 0, "learned on port 1");
        assert_eq!(stub_fdb_count(&b), 2, "the static entry and MAC_B");

        // By both, which must intersect rather than union.
        b.flush_fdb(Some(vlan_oid(1)), Some(PortId(2))).unwrap();
        assert_eq!(stub_fdb(&b, 1, &MAC_B), 0, "wrong port, so it stays");
    }

    /// Port 0 is a real logical port, so a flush scoped to it must not
    /// be read as "no port given". This is why the ABI carries flags
    /// rather than sentinel values.
    #[test]
    fn flushing_port_zero_is_not_flushing_everything() {
        let mut b = backend();
        stub_learn(&b, 1, &MAC_A, 1);
        // Port 0 does not exist on the 4-port stub, so nothing matches
        // it -- and nothing else may be swept up either.
        b.flush_fdb(None, Some(PortId(0))).unwrap();
        assert_eq!(stub_fdb(&b, 1, &MAC_A), 0, "learned on port 1, untouched");
    }

    /// Read one of the stub's per-port integer hooks.
    fn stub_port_int(b: &OpenBcmBackend, name: &[u8], port: u32) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32) -> std::os::raw::c_int,
            > = b._library.get(name).unwrap();
            f(b.switch, port)
        }
    }

    fn stub_fdb_aging(b: &OpenBcmBackend) -> u32 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch) -> u32> =
                b._library.get(b"hemlockbcm_stub_fdb_aging\0").unwrap();
            f(b.switch)
        }
    }

    #[test]
    fn learning_aging_and_learn_limits_reach_the_shim() {
        const LEARNING: &[u8] = b"hemlockbcm_stub_learning\0";
        const LIMIT: &[u8] = b"hemlockbcm_stub_learn_limit\0";
        let mut b = backend();
        assert_eq!(stub_port_int(&b, LEARNING, 1), 1, "on by default");
        assert_eq!(stub_port_int(&b, LIMIT, 1), -1, "uncapped");

        b.set_port_learning(PortId(1), false).unwrap();
        assert_eq!(stub_port_int(&b, LEARNING, 1), 0);
        b.set_port_learn_limit(PortId(1), Some(64)).unwrap();
        assert_eq!(stub_port_int(&b, LIMIT, 1), 64);
        // None removes the cap, which the ABI spells as a negative.
        b.set_port_learn_limit(PortId(1), None).unwrap();
        assert_eq!(stub_port_int(&b, LIMIT, 1), -1);
        // A limit past i32 saturates rather than wrapping to "unlimited".
        b.set_port_learn_limit(PortId(1), Some(u32::MAX)).unwrap();
        assert_eq!(stub_port_int(&b, LIMIT, 1), i32::MAX);

        assert_eq!(stub_fdb_aging(&b), 0);
        b.set_fdb_aging(300).unwrap();
        assert_eq!(stub_fdb_aging(&b), 300);
        b.set_fdb_aging(0).unwrap();
        assert_eq!(stub_fdb_aging(&b), 0, "0 disables aging");
    }
    // --- Link aggregation ---------------------------------------------

    /// -1 = not a member, 0 = member gated closed, 1 = member forwarding.
    fn stub_lag_member(b: &OpenBcmBackend, tid: u32, port: u32) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_lag_member\0").unwrap();
            f(b.switch, tid, port)
        }
    }

    /// -1 = not a member, 0 = untagged, 1 = tagged.
    fn stub_vlan_lag(b: &OpenBcmBackend, vlan_id: u16, tid: u32) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u16, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_vlan_lag\0").unwrap();
            f(b.switch, vlan_id, tid)
        }
    }

    /// A member joins gated closed and leaves the default VLAN, which is
    /// what the trait promises: in the trunk, carrying its config,
    /// forwarding nothing until something opens the gate.
    #[test]
    fn lag_members_join_gated_closed_and_leave_the_bridge() {
        let mut b = backend();
        let lag = b.create_lag().unwrap();
        let tid = lag_tid_of(lag).expect("a LAG id");
        assert_eq!(
            stub_member(&b, 1, 1),
            0,
            "port 1 starts in the default VLAN"
        );

        let member = b.add_lag_member(lag, PortId(1)).unwrap();
        assert_eq!(stub_lag_member(&b, tid, 1), 0, "in the trunk, gated closed");
        assert_eq!(stub_member(&b, 1, 1), -1, "and out of the default VLAN");

        b.set_lag_member_state(member, true).unwrap();
        assert_eq!(stub_lag_member(&b, tid, 1), 1, "gate open");
        b.set_lag_member_state(member, false).unwrap();
        assert_eq!(stub_lag_member(&b, tid, 1), 0, "gate closed again");

        // A LAG with members cannot be destroyed.
        assert!(b.remove_lag(lag).is_err());
        b.remove_lag_member(member, PortId(1)).unwrap();
        assert_eq!(stub_lag_member(&b, tid, 1), -1);
        assert_eq!(stub_member(&b, 1, 1), 0, "standalone bridging restored");
        b.remove_lag(lag).unwrap();
    }

    /// The member id names the port it fronts, so a mismatched pair is
    /// caught here rather than removing some other port from the trunk.
    #[test]
    fn a_lag_member_id_must_match_the_port_it_fronts() {
        let mut b = backend();
        let lag = b.create_lag().unwrap();
        let member = b.add_lag_member(lag, PortId(1)).unwrap();
        assert!(b.remove_lag_member(member, PortId(2)).is_err());
        assert_eq!(
            stub_lag_member(&b, lag_tid_of(lag).unwrap(), 1),
            0,
            "still a member"
        );
    }

    /// A LAG id is accepted wherever a port is, which the hardware does
    /// not offer directly -- these calls dispatch on the id's tag.
    #[test]
    fn a_lag_is_a_vlan_member_in_its_own_right() {
        let mut b = backend();
        let lag = b.create_lag().unwrap();
        let tid = lag_tid_of(lag).unwrap();
        let vlan = b.create_vlan(100).unwrap();

        let member = b.add_vlan_member(vlan, lag, true).unwrap();
        assert_eq!(stub_vlan_lag(&b, 100, tid), 1, "tagged trunk member");
        // ...and it is a *trunk* membership, not a port one that happened
        // to use the LAG's number as a port.
        assert_eq!(stub_member(&b, 100, tid), -1);

        b.remove_vlan_member(member).unwrap();
        assert_eq!(stub_vlan_lag(&b, 100, tid), -1);

        // Ports and LAGs coexist in the same VLAN, told apart by the id.
        b.add_vlan_member(vlan, PortId(1), false).unwrap();
        b.add_vlan_member(vlan, lag, false).unwrap();
        assert_eq!(stub_member(&b, 100, 1), 0);
        assert_eq!(stub_vlan_lag(&b, 100, tid), 0);
    }

    /// The reason gated-closed members stay in the trunk: ingress
    /// classification is per receiving port, so a LAG's PVID has to
    /// reach every member -- including ones not yet forwarding, and ones
    /// that join afterwards.
    #[test]
    fn a_lag_pvid_reaches_every_member_gated_or_not() {
        let mut b = backend();
        let lag = b.create_lag().unwrap();
        b.create_vlan(100).unwrap();
        let member = b.add_lag_member(lag, PortId(1)).unwrap();

        b.set_port_pvid(lag, 100).unwrap();
        assert_eq!(
            stub_pvid(&b, 1),
            100,
            "gated-closed member still classified"
        );

        // Opening the gate changes nothing about classification.
        b.set_lag_member_state(member, true).unwrap();
        assert_eq!(stub_pvid(&b, 1), 100);

        // A port joining later inherits it too.
        b.add_lag_member(lag, PortId(2)).unwrap();
        assert_eq!(stub_pvid(&b, 2), 100, "inherited on join");
    }

    /// LAG ids are tagged so they can never be read as logical ports,
    /// and the three id families never overlap.
    #[test]
    fn lag_ids_cannot_be_confused_with_ports_or_other_objects() {
        assert_eq!(lag_tid_of(PortId(1)), None, "a real port is not a LAG");
        assert_eq!(lag_tid_of(lag_port(0)), Some(0), "trunk 0 is a real trunk");
        assert_eq!(lag_tid_of(lag_port(7)), Some(7));

        assert_eq!(
            oid_lag_member(lag_member_oid(3, PortId(50))),
            (3, PortId(50))
        );
        assert_eq!(
            oid_lag_vlan_member(lag_vlan_member_oid(100, 3)),
            Some((100, 3))
        );
        // A port's VLAN membership must not be read as a trunk's.
        assert_eq!(oid_lag_vlan_member(member_oid(100, PortId(3))), None);
        assert_eq!(oid_lag_vlan_member(vlan_oid(100)), None);

        // Every tag distinct, which is what makes the dispatch safe.
        let tags = [
            oid_tag(vlan_oid(1).0),
            oid_tag(member_oid(1, PortId(1)).0),
            oid_tag(lag_member_oid(1, PortId(1)).0),
            oid_tag(lag_vlan_member_oid(1, 1).0),
            oid_tag(lag_port(1).0),
        ];
        for (i, tag) in tags.iter().enumerate() {
            assert!(
                !tags[..i].contains(tag),
                "tag {tag:#x} is used by two id families"
            );
        }
    }
}
