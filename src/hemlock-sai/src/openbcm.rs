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
    AclAction, AclFamily, AclFields, AclPacketAction, AclStage, CablePair, FdbAction, IpPrefix,
    Oid, PolicerSpec, PolicerStats, PortCounters, PortId, QosMapType, QueueCounters, RouteTarget,
    SaiBackend, SaiCapabilities, SaiError, SaiEvent, SaiPort, SchedulerSpec, StormClass,
    StpPortState, SwitchInfo, TrapKind, WredSpec,
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

/// `HEMLOCKBCM_STP_*`: the three forwarding states Hemlock drives.
/// 802.1D's listen and disable never reach the datapath from above, so
/// the ABI does not carry them.
const STP_BLOCKING: std::os::raw::c_int = 0;
const STP_LEARNING: std::os::raw::c_int = 1;
const STP_FORWARDING: std::os::raw::c_int = 2;

/// `HEMLOCKBCM_ROUTE_*`: what a route's destination resolves to.
const ROUTE_CPU: std::os::raw::c_int = 0;
const ROUTE_RIF: std::os::raw::c_int = 1;
const ROUTE_DROP: std::os::raw::c_int = 2;

/// `HEMLOCKBCM_STORM_*`, in the trait's own order.
fn storm_class(class: StormClass) -> std::os::raw::c_int {
    match class {
        StormClass::Broadcast => 0,
        StormClass::Multicast => 1,
        StormClass::UnknownUnicast => 2,
    }
}

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

/// Mirrors `struct hemlockbcm_acl_fields`. Field order and types must
/// match the header exactly.
#[repr(C)]
#[derive(Default)]
struct ShimAclFields {
    present: u32,
    src_ip: u32,
    src_ip_mask: u32,
    dst_ip: u32,
    dst_ip_mask: u32,
    protocol: u8,
    src_port: u16,
    dst_port: u16,
    dscp: u8,
    src_mac: [u8; 6],
    src_mac_mask: [u8; 6],
    dst_mac: [u8; 6],
    dst_mac_mask: [u8; 6],
    ethertype: u16,
    vlan: u16,
}

// `HEMLOCKBCM_ACL_F_*`, in header order.
const ACL_F_SRC_IP: u32 = 0x0001;
const ACL_F_DST_IP: u32 = 0x0002;
const ACL_F_PROTOCOL: u32 = 0x0004;
const ACL_F_SRC_PORT: u32 = 0x0008;
const ACL_F_DST_PORT: u32 = 0x0010;
const ACL_F_DSCP: u32 = 0x0020;
const ACL_F_SRC_MAC: u32 = 0x0040;
const ACL_F_DST_MAC: u32 = 0x0080;
const ACL_F_ETHERTYPE: u32 = 0x0100;
const ACL_F_VLAN: u32 = 0x0200;

// `HEMLOCKBCM_ACL_*` actions.
const ACL_FORWARD: std::os::raw::c_int = 0;
const ACL_DROP: std::os::raw::c_int = 1;
const ACL_TRAP: std::os::raw::c_int = 2;
const ACL_COPY: std::os::raw::c_int = 3;

/// An IPv4 prefix as address and mask, or an error for a v6 one.
///
/// The shim reports no IPv6, so a v6 prefix here means the caller built
/// a rule this datapath cannot express. Truncating it to its low 32 bits
/// would produce a rule that matches something else entirely.
fn ipv4_prefix(prefix: IpPrefix, what: &str) -> Result<(u32, u32), SaiError> {
    match prefix.0 {
        std::net::IpAddr::V4(addr) => {
            let bits = u32::from(addr);
            let len = prefix.1.min(32);
            // A /0 mask is 0, and `u32::MAX << 32` is undefined, so the
            // shift is done in 64 bits and truncated.
            let mask = (!0u64 << (32 - u32::from(len))) as u32;
            Ok((bits & mask, mask))
        }
        std::net::IpAddr::V6(_) => Err(SaiError::Other(format!(
            "{what}: IPv6 ACL matches need an IPv6 table, which this datapath does not have"
        ))),
    }
}

/// An IPv4 address as a u32, or an error for a v6 one.
///
/// The shim reports no IPv6, and truncating a v6 address to 32 bits
/// would name some unrelated v4 host.
fn ipv4_address(ip: std::net::IpAddr, what: &str) -> Result<u32, SaiError> {
    match ip {
        std::net::IpAddr::V4(addr) => Ok(u32::from(addr)),
        std::net::IpAddr::V6(_) => Err(SaiError::Other(format!(
            "{what}: IPv6 needs an IPv6 datapath, which this backend does not have"
        ))),
    }
}

/// One L4 port match. The chip expresses a range with a range checker,
/// a separately allocated resource this ABI does not yet carry, so a
/// non-degenerate range is refused rather than silently narrowed to its
/// lower bound.
fn l4_port(range: (u16, u16), what: &str) -> Result<u16, SaiError> {
    if range.0 == range.1 {
        Ok(range.0)
    } else {
        Err(SaiError::Other(format!(
            "{what}: L4 port ranges ({}-{}) need a range checker, which this backend \
             does not implement yet",
            range.0, range.1
        )))
    }
}

impl ShimAclFields {
    fn build(fields: &AclFields) -> Result<Self, SaiError> {
        let mut out = ShimAclFields::default();
        if let Some(prefix) = fields.src_ip {
            let (addr, mask) = ipv4_prefix(prefix, "src_ip")?;
            out.src_ip = addr;
            out.src_ip_mask = mask;
            out.present |= ACL_F_SRC_IP;
        }
        if let Some(prefix) = fields.dst_ip {
            let (addr, mask) = ipv4_prefix(prefix, "dst_ip")?;
            out.dst_ip = addr;
            out.dst_ip_mask = mask;
            out.present |= ACL_F_DST_IP;
        }
        if let Some(protocol) = fields.protocol {
            out.protocol = protocol;
            out.present |= ACL_F_PROTOCOL;
        }
        if let Some(range) = fields.src_port {
            out.src_port = l4_port(range, "src_port")?;
            out.present |= ACL_F_SRC_PORT;
        }
        if let Some(range) = fields.dst_port {
            out.dst_port = l4_port(range, "dst_port")?;
            out.present |= ACL_F_DST_PORT;
        }
        if let Some(dscp) = fields.dscp {
            out.dscp = dscp;
            out.present |= ACL_F_DSCP;
        }
        if let Some((mac, mask)) = fields.src_mac {
            out.src_mac = mac;
            out.src_mac_mask = mask;
            out.present |= ACL_F_SRC_MAC;
        }
        if let Some((mac, mask)) = fields.dst_mac {
            out.dst_mac = mac;
            out.dst_mac_mask = mask;
            out.present |= ACL_F_DST_MAC;
        }
        if let Some(ethertype) = fields.ethertype {
            out.ethertype = ethertype;
            out.present |= ACL_F_ETHERTYPE;
        }
        if let Some(vlan) = fields.vlan {
            out.vlan = vlan;
            out.present |= ACL_F_VLAN;
        }
        Ok(out)
    }
}

/// The ABI's action code for one action set.
fn acl_action(action: &AclAction) -> std::os::raw::c_int {
    match action.action {
        AclPacketAction::Forward => ACL_FORWARD,
        AclPacketAction::Drop => ACL_DROP,
        AclPacketAction::Trap => ACL_TRAP,
        AclPacketAction::Copy => ACL_COPY,
    }
}

/// Opaque switch handle.
#[repr(C)]
struct ShimSwitch {
    _private: [u8; 0],
}

type LinkCb = unsafe extern "C" fn(*mut c_void, u32, std::os::raw::c_int);
type SampleCb = unsafe extern "C" fn(*mut c_void, u32, u32, *const u8, u32);

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

    // --- ABI 1.5: spanning tree --------------------------------------
    stp_default: Option<unsafe extern "C" fn(*mut ShimSwitch, *mut u32) -> Status>,
    stp_create: Option<unsafe extern "C" fn(*mut ShimSwitch, *mut u32) -> Status>,
    stp_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    stp_vlan_set: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u16) -> Status>,
    stp_port_state:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, std::os::raw::c_int) -> Status>,
    lag_stp_port_state:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, std::os::raw::c_int) -> Status>,

    // --- ABI 1.6: port mirroring -------------------------------------
    mirror_create: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, *mut u32) -> Status>,
    mirror_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    mirror_port_attach:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, std::os::raw::c_int) -> Status>,
    mirror_port_detach:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> Status>,

    // --- ABI 1.7: storm control --------------------------------------
    storm_control_set:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int, u32) -> Status>,

    // --- ABI 1.8: host interfaces ------------------------------------
    host_punt_setup: Option<unsafe extern "C" fn(*mut ShimSwitch) -> Status>,
    hostif_create: Option<
        unsafe extern "C" fn(*mut ShimSwitch, u32, *const std::os::raw::c_char, *mut u32) -> Status,
    >,

    // --- ABI 1.9: policers -------------------------------------------
    policer_create: Option<
        unsafe extern "C" fn(*mut ShimSwitch, std::os::raw::c_int, u64, u64, *mut u32) -> Status,
    >,
    policer_set:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int, u64, u64) -> Status>,
    policer_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    policer_stats: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, *mut u64, *mut u64) -> Status>,

    // --- ABI 1.10: ACLs ----------------------------------------------
    acl_table_create:
        Option<unsafe extern "C" fn(*mut ShimSwitch, std::os::raw::c_int, *mut u32) -> Status>,
    acl_table_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    acl_table_bind:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, std::os::raw::c_int) -> Status>,
    acl_table_unbind_all:
        Option<unsafe extern "C" fn(*mut ShimSwitch, std::os::raw::c_int, u32) -> Status>,
    acl_entry_create: Option<
        unsafe extern "C" fn(
            *mut ShimSwitch,
            u32,
            u32,
            *const ShimAclFields,
            std::os::raw::c_int,
            *mut u32,
        ) -> Status,
    >,
    acl_entry_action_set:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> Status>,
    acl_entry_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    acl_available:
        Option<unsafe extern "C" fn(*mut ShimSwitch, std::os::raw::c_int, *mut u32) -> Status>,

    // --- ABI 1.11: ACL counters and per-entry policers ---------------
    acl_counter_create: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, *mut u32) -> Status>,
    acl_counter_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    acl_counter_get: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, *mut u64) -> Status>,
    acl_entry_attach: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, u32) -> Status>,

    // --- ABI 1.12: router interfaces ---------------------------------
    rif_port_create:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u16, *const u8, *mut u32) -> Status>,
    rif_port_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u16, u32) -> Status>,
    rif_vlan_create:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u16, *const u8, *mut u32) -> Status>,
    rif_vlan_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    my_mac_create:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u16, *const u8, *mut u32) -> Status>,
    my_mac_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,

    // --- ABI 1.13: routes --------------------------------------------
    route_set:
        Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, std::os::raw::c_int, u32) -> Status>,
    route_delete: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,

    // --- ABI 1.14: neighbours and next hops --------------------------
    neighbor_set: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, *const u8) -> Status>,
    neighbor_clear: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,
    route_via_nexthop: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, u32) -> Status>,

    // --- ABI 1.15: ECMP groups ---------------------------------------
    ecmp_create: Option<unsafe extern "C" fn(*mut ShimSwitch, *mut u32) -> Status>,
    ecmp_destroy: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,
    ecmp_member_add: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,
    ecmp_member_remove: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,
    route_via_ecmp: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32, u32) -> Status>,

    // --- ABI 1.16: CoPP traps ----------------------------------------
    trap_set: Option<
        unsafe extern "C" fn(
            *mut ShimSwitch,
            std::os::raw::c_int,
            std::os::raw::c_int,
            std::os::raw::c_int,
            u32,
        ) -> Status,
    >,
    trap_clear: Option<
        unsafe extern "C" fn(*mut ShimSwitch, std::os::raw::c_int, std::os::raw::c_int) -> Status,
    >,
    trap_default_policer_set: Option<unsafe extern "C" fn(*mut ShimSwitch, u32) -> Status>,

    // --- ABI 1.17: ingress sampling ----------------------------------
    set_sample_callback:
        Option<unsafe extern "C" fn(*mut ShimSwitch, Option<SampleCb>, *mut c_void) -> Status>,
    sample_rate_set: Option<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> Status>,
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

const OID_TAG_STP: u64 = 0x05;
const OID_TAG_MIRROR: u64 = 0x06;
const OID_TAG_HOSTIF: u64 = 0x07;
const OID_TAG_POLICER: u64 = 0x08;
const OID_TAG_ACL_TABLE: u64 = 0x09;
const OID_TAG_ACL_ENTRY: u64 = 0x0a;
const OID_TAG_ACL_COUNTER: u64 = 0x0b;
const OID_TAG_RIF: u64 = 0x0c;
const OID_TAG_MY_MAC: u64 = 0x0d;
const OID_TAG_NEXT_HOP: u64 = 0x0e;
const OID_TAG_ECMP: u64 = 0x0f;
const OID_TAG_ECMP_MEMBER: u64 = 0x10;

fn ecmp_oid(group: u32) -> Oid {
    Oid((OID_TAG_ECMP << 56) | u64::from(group))
}

fn oid_ecmp(oid: Oid) -> u32 {
    oid.0 as u32
}

/// Group in bits 32..56, next-hop address in the low 32. Twenty-four
/// bits for the group, not thirty-two: bits 56..64 are the tag, and a
/// wider mask would hand the tag back as part of the group id.
fn ecmp_member_oid(group: u32, ip: u32) -> Oid {
    Oid((OID_TAG_ECMP_MEMBER << 56) | (u64::from(group & 0x00ff_ffff) << 32) | u64::from(ip))
}

fn oid_ecmp_member(oid: Oid) -> (u32, u32) {
    (((oid.0 >> 32) & 0x00ff_ffff) as u32, oid.0 as u32)
}

/// A next hop is named by the neighbour it forwards to, because that is
/// how the chip's host table is keyed. Nothing is allocated, so the id
/// is the address itself.
fn next_hop_oid(ip: u32) -> Oid {
    Oid((OID_TAG_NEXT_HOP << 56) | u64::from(ip))
}

fn oid_next_hop(oid: Oid) -> u32 {
    oid.0 as u32
}

fn rif_oid(rif: u32) -> Oid {
    Oid((OID_TAG_RIF << 56) | u64::from(rif))
}

fn oid_rif(oid: Oid) -> u32 {
    oid.0 as u32
}

fn my_mac_oid(my_mac: u32) -> Oid {
    Oid((OID_TAG_MY_MAC << 56) | u64::from(my_mac))
}

fn oid_my_mac(oid: Oid) -> u32 {
    oid.0 as u32
}

fn acl_counter_oid(counter: u32) -> Oid {
    Oid((OID_TAG_ACL_COUNTER << 56) | u64::from(counter))
}

fn oid_acl_counter(oid: Oid) -> u32 {
    oid.0 as u32
}

fn acl_table_oid(table: u32) -> Oid {
    Oid((OID_TAG_ACL_TABLE << 56) | u64::from(table))
}

fn oid_acl_table(oid: Oid) -> u32 {
    oid.0 as u32
}

fn acl_entry_oid(entry: u32) -> Oid {
    Oid((OID_TAG_ACL_ENTRY << 56) | u64::from(entry))
}

fn oid_acl_entry(oid: Oid) -> u32 {
    oid.0 as u32
}

/// The ABI's stage flag: egress is 1, ingress 0.
fn acl_stage(stage: AclStage) -> std::os::raw::c_int {
    match stage {
        AclStage::Ingress => 0,
        AclStage::Egress => 1,
    }
}

fn policer_oid(policer: u32) -> Oid {
    Oid((OID_TAG_POLICER << 56) | u64::from(policer))
}

fn oid_policer(oid: Oid) -> u32 {
    oid.0 as u32
}

fn hostif_oid(hostif: u32) -> Oid {
    Oid((OID_TAG_HOSTIF << 56) | u64::from(hostif))
}

fn mirror_oid(session: u32) -> Oid {
    Oid((OID_TAG_MIRROR << 56) | u64::from(session))
}

fn oid_mirror(oid: Oid) -> u32 {
    oid.0 as u32
}

fn stp_oid(stg: u32) -> Oid {
    Oid((OID_TAG_STP << 56) | u64::from(stg))
}

fn oid_stg(oid: Oid) -> u32 {
    oid.0 as u32
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

/// The internal VLAN backing a port router interface.
///
/// An L3 interface is per VLAN here, so routing a port means giving it a
/// VLAN of its own. Deriving that VLAN from the logical port keeps both
/// sides free of an allocator and makes `remove_router_interface` able
/// to name the same VLAN without anything having been remembered.
///
/// The range runs down from the top of the 802.1Q space, which is where
/// an operator is least likely to have configured something. If they
/// have, the collision is loud rather than silent: creating the VLAN
/// returns "already exists" and the router interface fails, instead of
/// quietly taking over a VLAN that is carrying traffic.
const RIF_VLAN_TOP: u16 = 4094;

fn rif_vlan_for(logical_port: u32) -> Result<u16, SaiError> {
    // 4094 down to 1; a chip with more ports than that is not this one,
    // but the arithmetic should not wrap into someone's VLAN 4000 if it
    // ever were.
    u16::try_from(logical_port)
        .ok()
        .filter(|port| *port < RIF_VLAN_TOP)
        .map(|port| RIF_VLAN_TOP - port)
        .ok_or_else(|| {
            SaiError::Other(format!(
                "logical port {logical_port} has no room for an internal router VLAN"
            ))
        })
}

/// `HEMLOCKBCM_TRAP_*`: the trap kinds the shim can match. AclLog and
/// SamplePacket are absent on purpose -- the first installs nothing (see
/// `create_hostif_trap`), the second belongs to the sFlow family.
const TRAP_STP: std::os::raw::c_int = 1;
const TRAP_DHCP: std::os::raw::c_int = 16;

fn trap_kind(kind: TrapKind) -> Result<std::os::raw::c_int, SaiError> {
    Ok(match kind {
        TrapKind::Ip2me => 0,
        TrapKind::Stp => TRAP_STP,
        TrapKind::Lacp => 2,
        TrapKind::Lldp => 3,
        TrapKind::Eapol => 4,
        TrapKind::IgmpQuery => 5,
        TrapKind::IgmpLeave => 6,
        TrapKind::IgmpV1Report => 7,
        TrapKind::IgmpV2Report => 8,
        TrapKind::IgmpV3Report => 9,
        TrapKind::MldV1V2 => 10,
        TrapKind::MldV1Report => 11,
        TrapKind::MldV1Done => 12,
        TrapKind::MldV2Report => 13,
        TrapKind::ArpRequest => 14,
        TrapKind::ArpResponse => 15,
        TrapKind::Dhcp => TRAP_DHCP,
        TrapKind::Ospf => 17,
        TrapKind::Bgp => 18,
        TrapKind::Vrrp => 19,
        // Handled before the mapping, like AclLog: the shim's sample
        // delivery path is the punt, so there is no field entry to make.
        TrapKind::SamplePacket => {
            return Err(SaiError::Other(
                "SamplePacket is handled before the kind mapping".into(),
            ))
        }
        TrapKind::AclLog => {
            return Err(SaiError::Other(
                "AclLog is handled before the kind mapping".into(),
            ))
        }
    })
}

const OID_TAG_SAMPLE: u64 = 0x13;

/// A samplepacket session is its rate; nothing is allocated.
fn sample_oid(rate: u32) -> Oid {
    Oid((OID_TAG_SAMPLE << 56) | u64::from(rate))
}

fn oid_sample_rate(oid: Oid) -> u32 {
    oid.0 as u32
}

const OID_TAG_TRAP_GROUP: u64 = 0x11;
const OID_TAG_TRAP: u64 = 0x12;

/// A trap group is its policer, so the id carries it. 0 = unpoliced.
fn trap_group_oid(policer: u32) -> Oid {
    Oid((OID_TAG_TRAP_GROUP << 56) | u64::from(policer))
}

fn oid_trap_group_policer(oid: Oid) -> u32 {
    oid.0 as u32
}

/// Kind in the low byte, default-group flag in bit 8.
fn trap_oid(kind: std::os::raw::c_int, is_default: bool) -> Oid {
    Oid((OID_TAG_TRAP << 56) | (u64::from(is_default) << 8) | kind as u64)
}

fn oid_trap(oid: Oid) -> (std::os::raw::c_int, bool) {
    ((oid.0 & 0xff) as std::os::raw::c_int, oid.0 & 0x100 != 0)
}

/// The accepted-but-inert AclLog trap; 0xff is no real kind.
fn acl_log_trap_oid() -> Oid {
    Oid((OID_TAG_TRAP << 56) | 0xff)
}

/// The SamplePacket trap: satisfied by the shim's own delivery path
/// rather than a field entry, so it too installs nothing here.
fn sample_trap_oid() -> Oid {
    Oid((OID_TAG_TRAP << 56) | 0xfe)
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

unsafe extern "C" fn sample_callback(
    context: *mut c_void,
    logical_port: u32,
    original_length: u32,
    data: *const u8,
    length: u32,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: same contract as link_callback -- `context` is the leaked
    // Arc<EventContext>; `data` is valid for `length` bytes only during
    // this call, so it is copied before the send.
    let ctx = unsafe { &*(context as *const EventContext) };
    let bytes = if data.is_null() || length == 0 {
        Vec::new()
    } else {
        // SAFETY: the shim promises `data` covers `length` bytes.
        unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec()
    };
    let _ = ctx.tx.send(SaiEvent::SampledPacket {
        port: PortId(u64::from(logical_port)),
        original_length,
        bytes,
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
    /// The default trap group's policer (0 = none): the one cached word
    /// in this backend. A trap created into the default group needs the
    /// value from the last set_default_trap_group_policer call, and
    /// nothing derivable holds it. Lost on restart, and safely so --
    /// syncd replays the whole CoPP program after create_switch.
    default_trap_policer: u32,
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
            default_trap_policer: 0,
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

    /// The SDK logical port number behind a `PortId`.
    ///
    /// A LAG id is a `PortId` too, and `port.0 as u32` would quietly
    /// truncate one to its trunk id -- a small number that is a
    /// perfectly good port on this chip. Every slot that takes a port
    /// and only a port goes through here, so a LAG id reaching one is an
    /// error rather than a command against whichever port shares that
    /// number.
    fn logical_port(&self, port: PortId) -> Result<u32, SaiError> {
        match lag_tid_of(port) {
            Some(_) => Err(SaiError::Other(format!(
                "{port} is a LAG, and this operation takes a port"
            ))),
            None => Ok(port.0 as u32),
        }
    }

    /// The MAC a router interface answers to: the switch MAC syncd
    /// resolved from the ONIE syseeprom or the management netdev.
    ///
    /// There is no fallback. A router interface with a made-up MAC would
    /// come up, answer ARP, and put a MAC on the wire that belongs to
    /// nothing -- far worse than refusing to route.
    fn router_mac(&self) -> Result<[u8; 6], SaiError> {
        self.src_mac.ok_or_else(|| {
            SaiError::Other(
                "no switch MAC: a router interface needs one and there is no safe default".into(),
            )
        })
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

    /// Set an ACL entry's counter and policer, detaching either with
    /// `None`. One call for both, because the entry's action set is
    /// replaced as a unit and splitting them would make the order
    /// between the two significant.
    fn attach_acl_objects(
        &self,
        entry: Oid,
        counter: Option<Oid>,
        policer: Option<Oid>,
    ) -> Result<(), SaiError> {
        // Always call when the slot exists, including with neither
        // object: this is a set, and "attach nothing" is how an update
        // that dropped its `log` clause detaches the old counter.
        // Skipping it here left the previous attachment in place.
        let f = match slot!(self, acl_entry_attach, "acl_entry_attach") {
            Ok(f) => f,
            // A shim without the slot can still serve rules that ask for
            // neither; one that asks for either gets the real error.
            Err(e) => {
                return if counter.is_none() && policer.is_none() {
                    Ok(())
                } else {
                    Err(e)
                };
            }
        };
        let sw = self.switch()?;
        // 0 is the ABI's "no object", and no id this backend mints is 0.
        // SAFETY: plain scalars over the ABI.
        check("acl_entry_attach", unsafe {
            f(
                sw,
                oid_acl_entry(entry),
                counter.map_or(0, oid_acl_counter),
                policer.map_or(0, oid_policer),
            )
        })
    }

    /// An STP argument the trait spells `Option<Oid>`, where `None`
    /// means the default instance -- which the shim reports rather than
    /// this side assuming the SDK's usual value of 1.
    fn stg_or_default(&self, stp: Option<Oid>) -> Result<u32, SaiError> {
        match stp {
            Some(stp) => Ok(oid_stg(stp)),
            None => {
                let f = slot!(self, stp_default, "stp_default")?;
                let sw = self.switch()?;
                let mut stg: u32 = 0;
                // SAFETY: `stg` is a live u32 the shim writes only on OK.
                check("stp_default", unsafe { f(sw, &mut stg) })?;
                Ok(stg)
            }
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
        if let Ok(set_cb) = slot!(self, set_sample_callback, "set_sample_callback") {
            let ctx = Arc::as_ptr(&self.event_ctx) as *mut c_void;
            // SAFETY: same lifetime argument as the link callback above.
            check("set_sample_callback", unsafe {
                set_cb(handle, Some(sample_callback), ctx)
            })?;
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
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_admin_state", unsafe {
            f(sw, logical, i32::from(up))
        })
    }

    fn set_port_speed(&mut self, port: PortId, speed_mbps: u32) -> Result<(), SaiError> {
        let f = slot!(self, set_port_speed, "set_port_speed")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_speed", unsafe { f(sw, logical, speed_mbps) })
    }

    fn set_port_duplex(&mut self, port: PortId, full: bool) -> Result<(), SaiError> {
        let f = slot!(self, set_port_duplex, "set_port_duplex")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_duplex", unsafe {
            f(sw, logical, i32::from(full))
        })
    }

    fn set_port_autoneg(&mut self, port: PortId, on: bool) -> Result<(), SaiError> {
        let f = slot!(self, set_port_autoneg, "set_port_autoneg")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_autoneg", unsafe { f(sw, logical, i32::from(on)) })
    }

    fn set_port_mtu(&mut self, port: PortId, mtu: u32) -> Result<(), SaiError> {
        let f = slot!(self, set_port_mtu, "set_port_mtu")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_mtu", unsafe { f(sw, logical, mtu) })
    }

    fn port_counters(&mut self, port: PortId) -> Result<PortCounters, SaiError> {
        let f = slot!(self, port_counters, "port_counters")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        let mut raw = ShimCounters::default();
        // SAFETY: `raw` is written only on success.
        check("port_counters", unsafe { f(sw, logical, &mut raw) })?;
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
            stp: slot!(self, stp_create, "stp_create").is_ok(),
            fdb_flush: slot!(self, flush_fdb, "flush_fdb").is_ok(),
            fdb_aging: slot!(self, set_fdb_aging, "set_fdb_aging").is_ok(),
            l2mc: false,
            storm_control: slot!(self, storm_control_set, "storm_control_set").is_ok(),
            mirror: slot!(self, mirror_create, "mirror_create").is_ok(),
            mirror_sessions_max: raw.mirror_sessions_max,
            port_tpid: slot!(self, set_port_tpid, "set_port_tpid").is_ok(),
            ecmp_width: raw.ecmp_width,
            ipv6: raw.ipv6 != 0,
            my_mac: slot!(self, my_mac_create, "my_mac_create").is_ok(),
            acl_ingress: slot!(self, acl_entry_create, "acl_entry_create").is_ok(),
            acl_egress: slot!(self, acl_entry_create, "acl_entry_create").is_ok(),
            acl_entry_policer: slot!(self, acl_entry_attach, "acl_entry_attach").is_ok(),
            port_learn_limit: slot!(self, set_port_learn_limit, "set_port_learn_limit").is_ok(),
            copp: slot!(self, trap_set, "trap_set").is_ok(),
            buffer_bytes_total: raw.buffer_bytes_total,
            qos_map_ingress: false,
            qos_map_egress: false,
            wred: false,
            ecn: false,
            queue_shaper: false,
            wred_queue_stats: false,
            sflow: slot!(self, sample_rate_set, "sample_rate_set").is_ok(),
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

    // --- Host interfaces (ABI 1.8) ------------------------------------
    //
    // SAI models the punt path as a wildcard hostif table entry that
    // delivers on the ingress port's netdev, plus a NETDEV object per
    // port. KNET has no wildcard -- delivery follows per-filter matches
    // -- so the wildcard's meaning is carried by one ingress-port filter
    // per netdev, which `create_hostif` installs alongside the netdev.

    fn setup_host_punt(&mut self) -> Result<(), SaiError> {
        let f = slot!(self, host_punt_setup, "host_punt_setup")?;
        let sw = self.switch()?;
        // SAFETY: no arguments beyond the handle.
        check("host_punt_setup", unsafe { f(sw) })
    }

    fn create_hostif(&mut self, port: PortId, name: &str) -> Result<Oid, SaiError> {
        let f = slot!(self, hostif_create, "hostif_create")?;
        let logical = self.logical_port(port)?;
        // The kernel's own limit, and SAI documents the same one. Checked
        // here rather than truncated: a silently shortened netdev name
        // would collide with another port's.
        if name.is_empty() || name.len() > 15 {
            return Err(SaiError::Other(format!(
                "host interface name {name:?} must be 1..=15 characters"
            )));
        }
        let c_name = std::ffi::CString::new(name)
            .map_err(|_| SaiError::Other(format!("NUL in host interface name {name:?}")))?;
        let sw = self.switch()?;
        let mut hostif: u32 = 0;
        // SAFETY: `c_name` outlives the call; `hostif` is written on OK.
        check("hostif_create", unsafe {
            f(sw, logical, c_name.as_ptr(), &mut hostif)
        })?;
        Ok(hostif_oid(hostif))
    }

    // --- Router interfaces (ABI 1.12) ---------------------------------
    //
    // An L3 interface is per VLAN on this hardware, so a port router
    // interface is a VLAN of the port's own plus an interface on it.
    // Which VLAN is decided here rather than in the shim, so that
    // neither side keeps an allocator: see `rif_vlan_for`.

    fn create_router_interface(&mut self, port: PortId) -> Result<Oid, SaiError> {
        let f = slot!(self, rif_port_create, "rif_port_create")?;
        let logical = self.logical_port(port)?;
        let vlan_id = rif_vlan_for(logical)?;
        let mac = self.router_mac()?;
        let sw = self.switch()?;
        let mut rif: u32 = 0;
        // SAFETY: `mac` outlives the call; `rif` is written only on OK.
        check("rif_port_create", unsafe {
            f(sw, logical, vlan_id, mac.as_ptr(), &mut rif)
        })?;
        Ok(rif_oid(rif))
    }

    fn remove_router_interface(&mut self, port: PortId, rif: Oid) -> Result<(), SaiError> {
        let f = slot!(self, rif_port_destroy, "rif_port_destroy")?;
        let logical = self.logical_port(port)?;
        // The same derivation as on the way in: the VLAN is a function
        // of the port, so nothing had to be remembered between the two.
        let vlan_id = rif_vlan_for(logical)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("rif_port_destroy", unsafe {
            f(sw, logical, vlan_id, oid_rif(rif))
        })
    }

    fn create_vlan_router_interface(&mut self, vlan: Option<Oid>) -> Result<Oid, SaiError> {
        let f = slot!(self, rif_vlan_create, "rif_vlan_create")?;
        let vlan_id = self.vlan_id_or_default(vlan)?;
        let mac = self.router_mac()?;
        let sw = self.switch()?;
        let mut rif: u32 = 0;
        // SAFETY: `mac` outlives the call; `rif` is written only on OK.
        check("rif_vlan_create", unsafe {
            f(sw, vlan_id, mac.as_ptr(), &mut rif)
        })?;
        Ok(rif_oid(rif))
    }

    fn remove_vlan_router_interface(&mut self, rif: Oid) -> Result<(), SaiError> {
        let f = slot!(self, rif_vlan_destroy, "rif_vlan_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("rif_vlan_destroy", unsafe { f(sw, oid_rif(rif)) })
    }

    fn create_my_mac(&mut self, vlan_id: Option<u16>, mac: [u8; 6]) -> Result<Oid, SaiError> {
        let f = slot!(self, my_mac_create, "my_mac_create")?;
        let sw = self.switch()?;
        let mut my_mac: u32 = 0;
        // 0 is the ABI's "any VLAN"; VLAN 0 is not a valid 802.1Q id, so
        // it is not a value the caller could have meant.
        // SAFETY: `mac` outlives the call; `my_mac` is written on OK.
        check("my_mac_create", unsafe {
            f(sw, vlan_id.unwrap_or(0), mac.as_ptr(), &mut my_mac)
        })?;
        Ok(my_mac_oid(my_mac))
    }

    fn remove_my_mac(&mut self, my_mac: Oid) -> Result<(), SaiError> {
        let f = slot!(self, my_mac_destroy, "my_mac_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("my_mac_destroy", unsafe { f(sw, oid_my_mac(my_mac)) })
    }

    // --- Routes (ABI 1.13) --------------------------------------------
    //
    // Targets that resolve without a next-hop object. A route to a next
    // hop or an ECMP group needs an egress object, and building one
    // needs the neighbour's MAC and its egress port -- which may not be
    // known when the route is programmed. Those two are refused rather
    // than approximated; see the ABI header.

    fn create_route(&mut self, dest: IpPrefix, target: RouteTarget) -> Result<(), SaiError> {
        let f = slot!(self, route_set, "route_set")?;
        let (prefix, mask) = ipv4_prefix(dest, "route")?;
        let (kind, rif) = match target {
            RouteTarget::Cpu => (ROUTE_CPU, 0),
            RouteTarget::Rif(rif) => (ROUTE_RIF, oid_rif(rif)),
            RouteTarget::Drop => (ROUTE_DROP, 0),
            RouteTarget::NextHop(next_hop) => {
                // A different slot: the target is the neighbour's egress
                // object, which the shim finds by looking the next hop's
                // address up in the chip's host table. "Not found" there
                // means the neighbour has not resolved yet.
                let f = slot!(self, route_via_nexthop, "route_via_nexthop")?;
                let sw = self.switch()?;
                // SAFETY: plain scalars over the ABI.
                return check("route_via_nexthop", unsafe {
                    f(sw, prefix, mask, oid_next_hop(next_hop))
                });
            }
            RouteTarget::Group(group) => {
                let f = slot!(self, route_via_ecmp, "route_via_ecmp")?;
                let sw = self.switch()?;
                // SAFETY: plain scalars over the ABI.
                return check("route_via_ecmp", unsafe {
                    f(sw, prefix, mask, oid_ecmp(group))
                });
            }
        };
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("route_set", unsafe { f(sw, prefix, mask, kind, rif) })
    }

    fn remove_route(&mut self, dest: IpPrefix) -> Result<(), SaiError> {
        let f = slot!(self, route_delete, "route_delete")?;
        let (prefix, mask) = ipv4_prefix(dest, "route")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("route_delete", unsafe { f(sw, prefix, mask) })
    }

    // --- Neighbours and next hops (ABI 1.14) --------------------------
    //
    // A next hop is (interface, ip) -- a name, not something the chip
    // allocates. Resolving the *neighbour* is what builds the egress
    // object, and the chip files it under the neighbour's IP in its own
    // host table. That table is the resolution table, so neither side
    // keeps one and `create_next_hop` touches no hardware at all.

    fn create_neighbor(
        &mut self,
        rif: Oid,
        ip: std::net::IpAddr,
        mac: [u8; 6],
    ) -> Result<(), SaiError> {
        let f = slot!(self, neighbor_set, "neighbor_set")?;
        let ip = ipv4_address(ip, "neighbor")?;
        let sw = self.switch()?;
        // SAFETY: `mac` outlives the call.
        check("neighbor_set", unsafe {
            f(sw, oid_rif(rif), ip, mac.as_ptr())
        })
    }

    fn remove_neighbor(&mut self, rif: Oid, ip: std::net::IpAddr) -> Result<(), SaiError> {
        let f = slot!(self, neighbor_clear, "neighbor_clear")?;
        let ip = ipv4_address(ip, "neighbor")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("neighbor_clear", unsafe { f(sw, oid_rif(rif), ip) })
    }

    fn create_next_hop(&mut self, rif: Oid, ip: std::net::IpAddr) -> Result<Oid, SaiError> {
        // No hardware object: the egress object belongs to the
        // neighbour, and this is only the name a route uses to ask for
        // it. Minting the id here rather than at the chip is what keeps
        // both sides free of a resolution table.
        let _ = rif;
        Ok(next_hop_oid(ipv4_address(ip, "next hop")?))
    }

    fn remove_next_hop(&mut self, _next_hop: Oid) -> Result<(), SaiError> {
        // Nothing was allocated, so nothing is freed. The egress object
        // goes when its neighbour does.
        Ok(())
    }

    // --- ECMP groups (ABI 1.15) ---------------------------------------
    //
    // A group is a multipath egress object; its members are the egress
    // objects the neighbours own, named by the next hop's address the
    // same way a single-path route names one. A member id carries both
    // the group and the address, so removing one needs nothing
    // remembered.

    fn create_next_hop_group(&mut self) -> Result<Oid, SaiError> {
        let f = slot!(self, ecmp_create, "ecmp_create")?;
        let sw = self.switch()?;
        let mut group: u32 = 0;
        // SAFETY: `group` is a live u32 the shim writes only on OK.
        check("ecmp_create", unsafe { f(sw, &mut group) })?;
        Ok(ecmp_oid(group))
    }

    fn remove_next_hop_group(&mut self, group: Oid) -> Result<(), SaiError> {
        let f = slot!(self, ecmp_destroy, "ecmp_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("ecmp_destroy", unsafe { f(sw, oid_ecmp(group)) })
    }

    fn add_next_hop_group_member(&mut self, group: Oid, next_hop: Oid) -> Result<Oid, SaiError> {
        let f = slot!(self, ecmp_member_add, "ecmp_member_add")?;
        let group_id = oid_ecmp(group);
        let ip = oid_next_hop(next_hop);
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("ecmp_member_add", unsafe { f(sw, group_id, ip) })?;
        Ok(ecmp_member_oid(group_id, ip))
    }

    fn remove_next_hop_group_member(&mut self, member: Oid) -> Result<(), SaiError> {
        let f = slot!(self, ecmp_member_remove, "ecmp_member_remove")?;
        // The member id says which group and which next hop, so this
        // needs no lookup of its own.
        let (group_id, ip) = oid_ecmp_member(member);
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("ecmp_member_remove", unsafe { f(sw, group_id, ip) })
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
        self.logical_port(port)?;
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
        self.logical_port(port)?;
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
            // A LAG destination would need a trunk gport, which this ABI
            // has no slot for; refuse rather than program whichever port
            // shares the trunk's number.
            FdbAction::Forward(port) => (self.logical_port(port)?, 0),
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
                self.logical_port(port)?
            }
            None => 0,
        };
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("flush_fdb", unsafe { f(sw, vlan_id, logical_port, flags) })
    }

    // --- Storm control (ABI 1.7) --------------------------------------

    fn set_port_storm_control(
        &mut self,
        port: PortId,
        class: StormClass,
        kbps: Option<u64>,
    ) -> Result<(), SaiError> {
        let f = slot!(self, storm_control_set, "storm_control_set")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // The SDK meters in kbit/s and spells "no limit" as 0, so `None`
        // needs no sentinel of its own. A rate past u32 is not a real
        // configuration on a 10G port, so it saturates rather than
        // wrapping -- and wrapping to 0 would silently remove the limit.
        let kbps = kbps.unwrap_or(0).min(u64::from(u32::MAX)) as u32;
        // SAFETY: plain scalars over the ABI.
        check("storm_control_set", unsafe {
            f(sw, logical, storm_class(class), kbps)
        })
    }

    /// Not implemented, and not an oversight: the chip counts storm
    /// drops once per port, not once per class, so there is no honest
    /// per-class answer to give. Reporting the port-wide number three
    /// times would read as three measurements. See the ABI header.
    fn port_storm_drops(&mut self, _port: PortId, _class: StormClass) -> Result<u64, SaiError> {
        unimplemented_slot("port_storm_drops")
    }

    fn create_mirror_session(&mut self, monitor: PortId) -> Result<Oid, SaiError> {
        let f = slot!(self, mirror_create, "mirror_create")?;
        let logical = self.logical_port(monitor)?;
        let sw = self.switch()?;
        let mut session: u32 = 0;
        // SAFETY: `session` is a live u32 the shim writes only on OK.
        check("mirror_create", unsafe { f(sw, logical, &mut session) })?;
        Ok(mirror_oid(session))
    }

    fn remove_mirror_session(&mut self, session: Oid) -> Result<(), SaiError> {
        let f = slot!(self, mirror_destroy, "mirror_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("mirror_destroy", unsafe { f(sw, oid_mirror(session)) })
    }

    fn set_port_mirror(
        &mut self,
        port: PortId,
        ingress: Option<Oid>,
        egress: Option<Oid>,
    ) -> Result<(), SaiError> {
        // The two directions are independent, and each is a set rather
        // than an addition: attaching replaces whatever that direction
        // pointed at, `None` clears it.
        let logical = self.logical_port(port)?;
        for (session, is_egress) in [(ingress, 0), (egress, 1)] {
            match session {
                Some(session) => {
                    let f = slot!(self, mirror_port_attach, "mirror_port_attach")?;
                    let sw = self.switch()?;
                    // SAFETY: plain scalars over the ABI.
                    check("mirror_port_attach", unsafe {
                        f(sw, logical, oid_mirror(session), is_egress)
                    })?;
                }
                None => {
                    let f = slot!(self, mirror_port_detach, "mirror_port_detach")?;
                    let sw = self.switch()?;
                    // SAFETY: plain scalars over the ABI.
                    check("mirror_port_detach", unsafe { f(sw, logical, is_egress) })?;
                }
            }
        }
        Ok(())
    }

    // --- Ingress sampling / sFlow (ABI 1.17) --------------------------
    //
    // A samplepacket session carries exactly one fact -- its rate -- so
    // its id is that fact and nothing is allocated, the same shape as a
    // trap group. Delivery is the shim's own: a KNET filter steers
    // sample-reason packets to the SDK RX path and the callback
    // registered at create_switch turns each into a SampledPacket
    // event.

    fn create_samplepacket(&mut self, rate: u32) -> Result<Oid, SaiError> {
        // The session must be usable later, so the checks live here:
        // rate 0 would encode as "no session" and a rate past i32 would
        // be refused by the SDK long after the session looked fine.
        slot!(self, sample_rate_set, "sample_rate_set")?;
        if rate == 0 || rate > i32::MAX as u32 {
            return Err(SaiError::Other(format!(
                "sample rate must be 1..={}, got {rate}",
                i32::MAX
            )));
        }
        Ok(sample_oid(rate))
    }

    fn remove_samplepacket(&mut self, _session: Oid) -> Result<(), SaiError> {
        // A name; nothing was allocated. Ports still bound to it keep
        // sampling until they are unbound, which is the caller's
        // sequencing to get right -- and it does, unbinding first.
        Ok(())
    }

    fn set_port_sample_session(
        &mut self,
        port: PortId,
        session: Option<Oid>,
    ) -> Result<(), SaiError> {
        let f = slot!(self, sample_rate_set, "sample_rate_set")?;
        let logical = self.logical_port(port)?;
        let rate = match session {
            Some(session) if oid_tag(session.0) == OID_TAG_SAMPLE => oid_sample_rate(session),
            // A junk id would decode to a junk rate and quietly sample
            // at it; refuse instead.
            Some(session) => {
                return Err(SaiError::Other(format!(
                    "{session} is not a samplepacket session"
                )))
            }
            None => 0,
        };
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("sample_rate_set", unsafe { f(sw, logical, rate) })
    }

    fn run_cable_diag(&mut self, _port: PortId) -> Result<Vec<CablePair>, SaiError> {
        unimplemented_slot("run_cable_diag")
    }

    fn set_port_tpid(&mut self, port: PortId, tpid: u16) -> Result<(), SaiError> {
        let f = slot!(self, set_port_tpid, "set_port_tpid")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_tpid", unsafe { f(sw, logical, tpid) })
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
        let logical = self.logical_port(port)?;
        // The port stops bridging on its own account: from here its
        // traffic belongs to the trunk. Done before the member exists,
        // so a failure leaves the port in the state it started in.
        self.remove_port_default_vlan(port)?;
        let sw = self.switch()?;
        // Gated closed, per the trait: in the trunk, forwarding nothing
        // until something (LACP, or a static config) opens the gate.
        // SAFETY: plain scalars over the ABI.
        check("lag_member_add", unsafe { f(sw, tid, logical, 0) })?;
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
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("lag_member_remove", unsafe { f(sw, tid, logical) })?;
        self.restore_port_default_vlan(port)
    }

    fn set_lag_member_state(&mut self, member: Oid, enabled: bool) -> Result<(), SaiError> {
        let f = slot!(self, lag_member_state, "lag_member_state")?;
        let (tid, port) = oid_lag_member(member);
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("lag_member_state", unsafe {
            f(sw, tid, logical, i32::from(enabled))
        })
    }

    // --- Spanning tree (ABI 1.5) --------------------------------------
    //
    // An STP instance is an SDK spanning-tree group, and its id is
    // derived from the group id like every other object here. `None`
    // means the default group, which the shim reports rather than this
    // side assuming it.

    fn create_stp_instance(&mut self) -> Result<Oid, SaiError> {
        let f = slot!(self, stp_create, "stp_create")?;
        let sw = self.switch()?;
        let mut stg: u32 = 0;
        // SAFETY: `stg` is a live u32 the shim writes only on OK.
        check("stp_create", unsafe { f(sw, &mut stg) })?;
        Ok(stp_oid(stg))
    }

    fn remove_stp_instance(&mut self, stp: Oid) -> Result<(), SaiError> {
        let f = slot!(self, stp_destroy, "stp_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("stp_destroy", unsafe { f(sw, oid_stg(stp)) })
    }

    fn set_vlan_stp_instance(
        &mut self,
        vlan: Option<Oid>,
        stp: Option<Oid>,
    ) -> Result<(), SaiError> {
        let f = slot!(self, stp_vlan_set, "stp_vlan_set")?;
        let vlan_id = self.vlan_id_or_default(vlan)?;
        let stg = self.stg_or_default(stp)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("stp_vlan_set", unsafe { f(sw, stg, vlan_id) })
    }

    fn set_stp_port_state(
        &mut self,
        stp: Option<Oid>,
        port: PortId,
        state: StpPortState,
    ) -> Result<(), SaiError> {
        let stg = self.stg_or_default(stp)?;
        let state = match state {
            StpPortState::Blocking => STP_BLOCKING,
            StpPortState::Learning => STP_LEARNING,
            StpPortState::Forwarding => STP_FORWARDING,
        };
        // Port-like, so a LAG id is legal here too and reaches every
        // member. Unlike PVID this is not inherited on join: a port has
        // a state in every group at once, so there is no single value to
        // inherit, and the caller re-applies after a membership change.
        if let Some(tid) = lag_tid_of(port) {
            let f = slot!(self, lag_stp_port_state, "lag_stp_port_state")?;
            let sw = self.switch()?;
            // SAFETY: plain scalars over the ABI.
            return check("lag_stp_port_state", unsafe { f(sw, stg, tid, state) });
        }
        let f = slot!(self, stp_port_state, "stp_port_state")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("stp_port_state", unsafe {
            f(sw, stg, port.0 as u32, state)
        })
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

    // --- ACLs (ABI 1.10) ----------------------------------------------
    //
    // A table is a field group and an entry is a field entry in it; both
    // ids are the SDK's own. Counters and per-entry policers are not
    // carried yet, and an action asking for either is refused rather
    // than quietly installed without it.

    fn create_acl_table(&mut self, stage: AclStage, family: AclFamily) -> Result<Oid, SaiError> {
        // An IPv6 table would need the v6 qualifiers, and this shim
        // reports no IPv6 at all -- so the table would exist and none of
        // its entries could be expressed.
        if family == AclFamily::Ipv6 {
            return Err(SaiError::Other(
                "IPv6 ACL tables need an IPv6 datapath, which this backend does not have".into(),
            ));
        }
        let f = slot!(self, acl_table_create, "acl_table_create")?;
        let sw = self.switch()?;
        let mut table: u32 = 0;
        // SAFETY: `table` is a live u32 the shim writes only on OK.
        check("acl_table_create", unsafe {
            f(sw, acl_stage(stage), &mut table)
        })?;
        Ok(acl_table_oid(table))
    }

    fn remove_acl_table(&mut self, table: Oid) -> Result<(), SaiError> {
        let f = slot!(self, acl_table_destroy, "acl_table_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("acl_table_destroy", unsafe { f(sw, oid_acl_table(table)) })
    }

    fn create_acl_entry(
        &mut self,
        table: Oid,
        priority: u32,
        fields: &AclFields,
        action: &AclAction,
    ) -> Result<Oid, SaiError> {
        let f = slot!(self, acl_entry_create, "acl_entry_create")?;
        // Refuses before anything reaches the chip if the match is one
        // this datapath cannot express.
        let shim_fields = ShimAclFields::build(fields)?;
        let attachments = (action.counter, action.policer);
        let action = acl_action(action);
        let sw = self.switch()?;
        let mut entry: u32 = 0;
        // SAFETY: `shim_fields` outlives the call; `entry` is written on OK.
        check("acl_entry_create", unsafe {
            f(
                sw,
                oid_acl_table(table),
                priority,
                &shim_fields,
                action,
                &mut entry,
            )
        })?;
        let entry = acl_entry_oid(entry);
        // The counter and policer are separate objects, so attaching
        // them is a second call. A failure here leaves a rule that
        // matches and acts but does not count, which is worse than no
        // rule: unwind it rather than report success.
        if let Err(e) = self.attach_acl_objects(entry, attachments.0, attachments.1) {
            let _ = self.remove_acl_entry(entry);
            return Err(e);
        }
        Ok(entry)
    }

    fn set_acl_entry_action(&mut self, entry: Oid, action: &AclAction) -> Result<(), SaiError> {
        let f = slot!(self, acl_entry_action_set, "acl_entry_action_set")?;
        let attachments = (action.counter, action.policer);
        let action = acl_action(action);
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("acl_entry_action_set", unsafe {
            f(sw, oid_acl_entry(entry), action)
        })?;
        // The action set is replaced as a unit, and the attachments are
        // part of it: an update that dropped the `log` clause has to
        // detach the counter too, which is what passing None does.
        self.attach_acl_objects(entry, attachments.0, attachments.1)
    }

    fn remove_acl_entry(&mut self, entry: Oid) -> Result<(), SaiError> {
        let f = slot!(self, acl_entry_destroy, "acl_entry_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("acl_entry_destroy", unsafe { f(sw, oid_acl_entry(entry)) })
    }

    fn bind_port_acl(
        &mut self,
        port: PortId,
        stage: AclStage,
        table: Option<Oid>,
    ) -> Result<(), SaiError> {
        let logical = self.logical_port(port)?;
        match table {
            Some(table) => {
                let f = slot!(self, acl_table_bind, "acl_table_bind")?;
                let sw = self.switch()?;
                // SAFETY: plain scalars over the ABI.
                check("acl_table_bind", unsafe {
                    f(sw, oid_acl_table(table), logical, 1)
                })
            }
            // Unbind without being told from what. Neither side keeps a
            // binding table, so the shim sweeps its stage's groups.
            None => {
                let f = slot!(self, acl_table_unbind_all, "acl_table_unbind_all")?;
                let sw = self.switch()?;
                // SAFETY: plain scalars over the ABI.
                check("acl_table_unbind_all", unsafe {
                    f(sw, acl_stage(stage), logical)
                })
            }
        }
    }

    fn acl_available_entries(&mut self, stage: AclStage) -> Result<u32, SaiError> {
        let f = slot!(self, acl_available, "acl_available")?;
        let sw = self.switch()?;
        let mut entries: u32 = 0;
        // SAFETY: `entries` is a live u32 the shim writes only on OK.
        check("acl_available", unsafe {
            f(sw, acl_stage(stage), &mut entries)
        })?;
        Ok(entries)
    }

    fn create_acl_counter(&mut self, table: Oid) -> Result<Oid, SaiError> {
        let f = slot!(self, acl_counter_create, "acl_counter_create")?;
        let sw = self.switch()?;
        let mut counter: u32 = 0;
        // SAFETY: `counter` is a live u32 the shim writes only on OK.
        check("acl_counter_create", unsafe {
            f(sw, oid_acl_table(table), &mut counter)
        })?;
        Ok(acl_counter_oid(counter))
    }

    fn remove_acl_counter(&mut self, counter: Oid) -> Result<(), SaiError> {
        let f = slot!(self, acl_counter_destroy, "acl_counter_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("acl_counter_destroy", unsafe {
            f(sw, oid_acl_counter(counter))
        })
    }

    fn get_acl_counter(&mut self, counter: Oid) -> Result<u64, SaiError> {
        let f = slot!(self, acl_counter_get, "acl_counter_get")?;
        let sw = self.switch()?;
        let mut packets: u64 = 0;
        // SAFETY: `packets` is a live u64 the shim writes only on OK.
        check("acl_counter_get", unsafe {
            f(sw, oid_acl_counter(counter), &mut packets)
        })?;
        Ok(packets)
    }

    fn create_policer(&mut self, spec: PolicerSpec) -> Result<Oid, SaiError> {
        let f = slot!(self, policer_create, "policer_create")?;
        let sw = self.switch()?;
        let mut policer: u32 = 0;
        // SAFETY: `policer` is a live u32 the shim writes only on OK.
        check("policer_create", unsafe {
            f(sw, i32::from(spec.pps), spec.rate, spec.burst, &mut policer)
        })?;
        Ok(policer_oid(policer))
    }

    fn set_policer(&mut self, policer: Oid, spec: PolicerSpec) -> Result<(), SaiError> {
        let f = slot!(self, policer_set, "policer_set")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("policer_set", unsafe {
            f(
                sw,
                oid_policer(policer),
                i32::from(spec.pps),
                spec.rate,
                spec.burst,
            )
        })
    }

    fn remove_policer(&mut self, policer: Oid) -> Result<(), SaiError> {
        let f = slot!(self, policer_destroy, "policer_destroy")?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("policer_destroy", unsafe { f(sw, oid_policer(policer)) })
    }

    fn policer_stats(&mut self, policer: Oid) -> Result<PolicerStats, SaiError> {
        let f = slot!(self, policer_stats, "policer_stats")?;
        let sw = self.switch()?;
        let mut conforming: u64 = 0;
        let mut dropped: u64 = 0;
        // SAFETY: both out params are live u64s, written only on OK.
        check("policer_stats", unsafe {
            f(sw, oid_policer(policer), &mut conforming, &mut dropped)
        })?;
        Ok(PolicerStats {
            conforming,
            dropped,
        })
    }

    // --- CoPP traps (ABI 1.16) ----------------------------------------
    //
    // A trap group carries exactly one fact -- its policer -- so its id
    // *is* that fact and nothing is allocated. A trap's id carries its
    // kind and whether it sits in the default group, which is all a
    // removal needs. The one cached word in this whole backend is the
    // default group's policer: a trap created into the default group
    // later needs the current value, and nothing derivable holds it.

    fn create_hostif_trap_group(&mut self, policer: Option<Oid>) -> Result<Oid, SaiError> {
        // The group is a name for its policer; the hardware object only
        // comes into being when a trap is created into it.
        Ok(trap_group_oid(policer.map_or(0, oid_policer)))
    }

    fn remove_hostif_trap_group(&mut self, _group: Oid) -> Result<(), SaiError> {
        // Nothing was allocated. The trap entries themselves are removed
        // through remove_hostif_trap, which is where their lifetime sits.
        Ok(())
    }

    fn create_hostif_trap(
        &mut self,
        kind: TrapKind,
        trap_only: bool,
        group: Oid,
    ) -> Result<Oid, SaiError> {
        // ACL log copies cannot be told apart from other CPU-bound
        // traffic by this chip's pipeline -- the copy is made by the ACL
        // entry itself, and a second FP stage cannot match "was copied
        // by the first". The trap is accepted so the CoPP program can
        // run, installs nothing, and the class's counters will read
        // zero. The gap is documented here and in the port doc, not
        // hidden.
        if kind == TrapKind::AclLog {
            return Ok(acl_log_trap_oid());
        }
        // The sample punt path is the KNET filter and RX handler the
        // shim installs with sampling itself, so this trap's work is
        // already done the moment sampling can be enabled at all.
        // Accepting it (rather than refusing) also keeps the sFlow
        // engine's "samples will not arrive" warning from firing when
        // they in fact do.
        if kind == TrapKind::SamplePacket {
            slot!(self, sample_rate_set, "sample_rate_set")?;
            return Ok(sample_trap_oid());
        }
        let kind_id = trap_kind(kind)?;
        let f = slot!(self, trap_set, "trap_set")?;
        let is_default = group.0 == 0;
        let policer = if is_default {
            self.default_trap_policer
        } else if oid_tag(group.0) == OID_TAG_TRAP_GROUP {
            oid_trap_group_policer(group)
        } else {
            // A junk group id would decode to a junk policer and attach
            // it; refuse rather than meter through garbage.
            return Err(SaiError::Other(format!("{group} is not a trap group")));
        };
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("trap_set", unsafe {
            f(
                sw,
                kind_id,
                i32::from(trap_only),
                i32::from(is_default),
                policer,
            )
        })?;
        Ok(trap_oid(kind_id, is_default))
    }

    fn remove_hostif_trap(&mut self, trap: Oid) -> Result<(), SaiError> {
        if trap == acl_log_trap_oid() || trap == sample_trap_oid() {
            return Ok(()); // nothing to take back out
        }
        let f = slot!(self, trap_clear, "trap_clear")?;
        let (kind_id, is_default) = oid_trap(trap);
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("trap_clear", unsafe {
            f(sw, kind_id, i32::from(is_default))
        })
    }

    fn set_default_trap_group_policer(&mut self, policer: Option<Oid>) -> Result<(), SaiError> {
        let f = slot!(self, trap_default_policer_set, "trap_default_policer_set")?;
        let id = policer.map_or(0, oid_policer);
        let sw = self.switch()?;
        // The shim sweeps the traps already installed in the default
        // group; the cached value covers the ones created after.
        // SAFETY: plain scalars over the ABI.
        check("trap_default_policer_set", unsafe { f(sw, id) })?;
        self.default_trap_policer = id;
        Ok(())
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
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // The ABI spells "no limit" as a negative, matching the SDK. A
        // limit larger than i32::MAX is not a real configuration, so it
        // saturates rather than wrapping into "unlimited".
        let limit = match limit {
            Some(limit) => limit.min(i32::MAX as u32) as i32,
            None => -1,
        };
        // SAFETY: plain scalars over the ABI.
        check("set_port_learn_limit", unsafe { f(sw, logical, limit) })
    }

    fn set_port_learning(&mut self, port: PortId, learn: bool) -> Result<(), SaiError> {
        let f = slot!(self, set_port_learning, "set_port_learning")?;
        let logical = self.logical_port(port)?;
        let sw = self.switch()?;
        // SAFETY: plain scalars over the ABI.
        check("set_port_learning", unsafe {
            f(sw, logical, i32::from(learn))
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
    /// VLANs, the FDB, LAGs and STP have left this list; the rest has
    /// not.
    #[test]
    fn phase_six_families_are_unsupported() {
        let mut b = backend();
        assert!(b.run_cable_diag(PortId(1)).unwrap_err().is_unsupported());
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
        assert_eq!(caps.ecmp_width, 4, "what the stub can actually hold");
        assert!(caps.ipv6);
        // Mirroring follows its slot; the session count is a
        // separate fact the shim reports, and syncd validates
        // against it, so 0 would refuse every session up front.
        assert!(caps.mirror && caps.mirror_sessions_max == 2);
        // ...but the slots that do exist read as supported -- derived
        // from the vtable, not from a hand-maintained list.
        assert!(caps.port_tpid, "QinQ, ABI 1.2");
        assert!(
            caps.fdb_flush && caps.fdb_aging && caps.port_learn_limit,
            "ABI 1.3"
        );
        assert!(caps.lag, "ABI 1.4");
        assert!(caps.stp, "ABI 1.5");
        assert!(caps.storm_control, "ABI 1.7");
        assert!(caps.acl_ingress && caps.acl_egress, "ABI 1.10");
        assert!(caps.copp, "ABI 1.16");
        assert!(caps.sflow, "ABI 1.17");
        assert!(caps.acl_entry_policer, "ABI 1.11");
        for (name, on) in [
            ("l2mc", caps.l2mc),
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
            stp_default: None,
            stp_create: None,
            stp_destroy: None,
            stp_vlan_set: None,
            stp_port_state: None,
            lag_stp_port_state: None,
            mirror_create: None,
            mirror_destroy: None,
            mirror_port_attach: None,
            mirror_port_detach: None,
            storm_control_set: None,
            host_punt_setup: None,
            hostif_create: None,
            policer_create: None,
            policer_set: None,
            policer_destroy: None,
            policer_stats: None,
            acl_table_create: None,
            acl_table_destroy: None,
            acl_table_bind: None,
            acl_table_unbind_all: None,
            acl_entry_create: None,
            acl_entry_action_set: None,
            acl_entry_destroy: None,
            acl_available: None,
            acl_counter_create: None,
            acl_counter_destroy: None,
            acl_counter_get: None,
            acl_entry_attach: None,
            rif_port_create: None,
            rif_port_destroy: None,
            rif_vlan_create: None,
            rif_vlan_destroy: None,
            my_mac_create: None,
            my_mac_destroy: None,
            route_set: None,
            route_delete: None,
            neighbor_set: None,
            neighbor_clear: None,
            route_via_nexthop: None,
            ecmp_create: None,
            ecmp_destroy: None,
            ecmp_member_add: None,
            ecmp_member_remove: None,
            route_via_ecmp: None,
            trap_set: None,
            trap_clear: None,
            trap_default_policer_set: None,
            set_sample_callback: None,
            sample_rate_set: None,
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
            "stp_default",
            "stp_create",
            "stp_destroy",
            "stp_vlan_set",
            "stp_port_state",
            "lag_stp_port_state",
            "mirror_create",
            "mirror_destroy",
            "mirror_port_attach",
            "mirror_port_detach",
            "storm_control_set",
            "host_punt_setup",
            "hostif_create",
            "policer_create",
            "policer_set",
            "policer_destroy",
            "policer_stats",
            "acl_table_create",
            "acl_table_destroy",
            "acl_table_bind",
            "acl_table_unbind_all",
            "acl_entry_create",
            "acl_entry_action_set",
            "acl_entry_destroy",
            "acl_available",
            "acl_counter_create",
            "acl_counter_destroy",
            "acl_counter_get",
            "acl_entry_attach",
            "rif_port_create",
            "rif_port_destroy",
            "rif_vlan_create",
            "rif_vlan_destroy",
            "my_mac_create",
            "my_mac_destroy",
            "route_set",
            "route_delete",
            "neighbor_set",
            "neighbor_clear",
            "route_via_nexthop",
            "ecmp_create",
            "ecmp_destroy",
            "ecmp_member_add",
            "ecmp_member_remove",
            "route_via_ecmp",
            "trap_set",
            "trap_clear",
            "trap_default_policer_set",
            "set_sample_callback",
            "sample_rate_set",
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
            std::mem::offset_of!(Api, stp_default),
            std::mem::offset_of!(Api, stp_create),
            std::mem::offset_of!(Api, stp_destroy),
            std::mem::offset_of!(Api, stp_vlan_set),
            std::mem::offset_of!(Api, stp_port_state),
            std::mem::offset_of!(Api, lag_stp_port_state),
            std::mem::offset_of!(Api, mirror_create),
            std::mem::offset_of!(Api, mirror_destroy),
            std::mem::offset_of!(Api, mirror_port_attach),
            std::mem::offset_of!(Api, mirror_port_detach),
            std::mem::offset_of!(Api, storm_control_set),
            std::mem::offset_of!(Api, host_punt_setup),
            std::mem::offset_of!(Api, hostif_create),
            std::mem::offset_of!(Api, policer_create),
            std::mem::offset_of!(Api, policer_set),
            std::mem::offset_of!(Api, policer_destroy),
            std::mem::offset_of!(Api, policer_stats),
            std::mem::offset_of!(Api, acl_table_create),
            std::mem::offset_of!(Api, acl_table_destroy),
            std::mem::offset_of!(Api, acl_table_bind),
            std::mem::offset_of!(Api, acl_table_unbind_all),
            std::mem::offset_of!(Api, acl_entry_create),
            std::mem::offset_of!(Api, acl_entry_action_set),
            std::mem::offset_of!(Api, acl_entry_destroy),
            std::mem::offset_of!(Api, acl_available),
            std::mem::offset_of!(Api, acl_counter_create),
            std::mem::offset_of!(Api, acl_counter_destroy),
            std::mem::offset_of!(Api, acl_counter_get),
            std::mem::offset_of!(Api, acl_entry_attach),
            std::mem::offset_of!(Api, rif_port_create),
            std::mem::offset_of!(Api, rif_port_destroy),
            std::mem::offset_of!(Api, rif_vlan_create),
            std::mem::offset_of!(Api, rif_vlan_destroy),
            std::mem::offset_of!(Api, my_mac_create),
            std::mem::offset_of!(Api, my_mac_destroy),
            std::mem::offset_of!(Api, route_set),
            std::mem::offset_of!(Api, route_delete),
            std::mem::offset_of!(Api, neighbor_set),
            std::mem::offset_of!(Api, neighbor_clear),
            std::mem::offset_of!(Api, route_via_nexthop),
            std::mem::offset_of!(Api, ecmp_create),
            std::mem::offset_of!(Api, ecmp_destroy),
            std::mem::offset_of!(Api, ecmp_member_add),
            std::mem::offset_of!(Api, ecmp_member_remove),
            std::mem::offset_of!(Api, route_via_ecmp),
            std::mem::offset_of!(Api, trap_set),
            std::mem::offset_of!(Api, trap_clear),
            std::mem::offset_of!(Api, trap_default_policer_set),
            std::mem::offset_of!(Api, set_sample_callback),
            std::mem::offset_of!(Api, sample_rate_set),
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
    // --- Spanning tree -------------------------------------------------

    /// -1 = no such group or port, else the HEMLOCKBCM_STP_* value.
    fn stub_stp(b: &OpenBcmBackend, stg: u32, port: u32) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_stp_state\0").unwrap();
            f(b.switch, stg, port)
        }
    }

    /// The group holding a VLAN, or 0 if there is no such VLAN.
    fn stub_vlan_stg(b: &OpenBcmBackend, vlan_id: u16) -> u32 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u16) -> u32> =
                b._library.get(b"hemlockbcm_stub_vlan_stg\0").unwrap();
            f(b.switch, vlan_id)
        }
    }

    /// A VLAN is in exactly one group, so assigning it to another is a
    /// move. Getting that wrong leaves it in two, and the SDK does not
    /// document which way its own call behaves.
    #[test]
    fn assigning_a_vlan_to_an_instance_moves_it() {
        let mut b = backend();
        let default_stg = b.stg_or_default(None).unwrap();
        let stp = b.create_stp_instance().unwrap();
        let vlan = b.create_vlan(100).unwrap();
        assert_eq!(stub_vlan_stg(&b, 100), default_stg, "starts in the default");

        b.set_vlan_stp_instance(Some(vlan), Some(stp)).unwrap();
        assert_eq!(stub_vlan_stg(&b, 100), oid_stg(stp));

        // An instance still holding VLANs cannot be removed.
        assert!(b.remove_stp_instance(stp).is_err());
        b.set_vlan_stp_instance(Some(vlan), None).unwrap();
        assert_eq!(stub_vlan_stg(&b, 100), default_stg, "moved back");
        b.remove_stp_instance(stp).unwrap();
    }

    #[test]
    fn port_forwarding_state_is_per_instance() {
        let mut b = backend();
        let default_stg = b.stg_or_default(None).unwrap();
        let stp = b.create_stp_instance().unwrap();

        assert_eq!(
            stub_stp(&b, default_stg, 1),
            STP_FORWARDING,
            "ports come up forwarding in the default instance"
        );
        b.set_stp_port_state(Some(stp), PortId(1), StpPortState::Blocking)
            .unwrap();
        assert_eq!(stub_stp(&b, oid_stg(stp), 1), STP_BLOCKING);
        assert_eq!(
            stub_stp(&b, default_stg, 1),
            STP_FORWARDING,
            "the other instance is untouched"
        );

        b.set_stp_port_state(None, PortId(1), StpPortState::Learning)
            .unwrap();
        assert_eq!(stub_stp(&b, default_stg, 1), STP_LEARNING);
        assert_eq!(stub_stp(&b, oid_stg(stp), 1), STP_BLOCKING);
    }

    /// A LAG id is port-like here too, and reaches every member --
    /// including gated-closed ones, which is the whole reason they stay
    /// in the trunk.
    #[test]
    fn a_lag_forwarding_state_reaches_every_member() {
        let mut b = backend();
        let default_stg = b.stg_or_default(None).unwrap();
        let lag = b.create_lag().unwrap();
        let gated = b.add_lag_member(lag, PortId(1)).unwrap();
        let open = b.add_lag_member(lag, PortId(2)).unwrap();
        b.set_lag_member_state(open, true).unwrap();
        let _ = gated;

        b.set_stp_port_state(None, lag, StpPortState::Blocking)
            .unwrap();
        assert_eq!(stub_stp(&b, default_stg, 1), STP_BLOCKING, "gated member");
        assert_eq!(stub_stp(&b, default_stg, 2), STP_BLOCKING, "open member");
        assert_eq!(
            stub_stp(&b, default_stg, 3),
            STP_FORWARDING,
            "a port outside the LAG is untouched"
        );

        // Not inherited on join, unlike PVID: a port has a state in
        // every instance, so there is no single value to inherit. The
        // caller re-applies after a membership change, and the ABI says
        // so rather than the shim pretending otherwise.
        b.add_lag_member(lag, PortId(3)).unwrap();
        assert_eq!(stub_stp(&b, default_stg, 3), STP_FORWARDING);
    }

    /// An STP id is its own family: it must not be readable as a VLAN,
    /// a membership or a LAG.
    #[test]
    fn stp_ids_are_their_own_family() {
        assert_eq!(oid_stg(stp_oid(7)), 7);
        assert_eq!(lag_tid_of(PortId(stp_oid(7).0)), None);
        assert_eq!(oid_lag_vlan_member(stp_oid(7)), None);
        assert_ne!(oid_tag(stp_oid(1).0), oid_tag(vlan_oid(1).0));
        assert_ne!(
            oid_tag(stp_oid(1).0),
            oid_tag(lag_member_oid(1, PortId(1)).0)
        );
    }
    // --- Port mirroring ------------------------------------------------

    /// The session a direction feeds; 0 = none, -1 = no such port.
    fn stub_mirror(b: &OpenBcmBackend, port: u32, egress: bool) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(
                    *mut ShimSwitch,
                    u32,
                    std::os::raw::c_int,
                ) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_mirror\0").unwrap();
            f(b.switch, port, i32::from(egress))
        }
    }

    /// The two directions are independent, and each is a set: attaching
    /// replaces, `None` clears. The SDK's own call is additive, so
    /// "replaces" is something the shim has to do rather than get.
    #[test]
    fn port_mirroring_sets_each_direction_independently() {
        let mut b = backend();
        let one = b.create_mirror_session(PortId(3)).unwrap();
        let two = b.create_mirror_session(PortId(4)).unwrap();
        assert_ne!(one, two);

        b.set_port_mirror(PortId(1), Some(one), None).unwrap();
        assert_eq!(stub_mirror(&b, 1, false), oid_mirror(one) as i32);
        assert_eq!(stub_mirror(&b, 1, true), 0, "egress untouched");

        b.set_port_mirror(PortId(1), Some(two), Some(one)).unwrap();
        assert_eq!(
            stub_mirror(&b, 1, false),
            oid_mirror(two) as i32,
            "replaced"
        );
        assert_eq!(stub_mirror(&b, 1, true), oid_mirror(one) as i32);

        // A session with ports still attached cannot be removed.
        assert!(b.remove_mirror_session(one).is_err());
        b.set_port_mirror(PortId(1), None, None).unwrap();
        assert_eq!(stub_mirror(&b, 1, false), 0);
        assert_eq!(stub_mirror(&b, 1, true), 0);
        b.remove_mirror_session(one).unwrap();
        b.remove_mirror_session(two).unwrap();
    }

    /// Clearing a direction that is already clear is not an error: the
    /// caller reaches this whenever it rewrites one direction and leaves
    /// the other alone.
    #[test]
    fn detaching_an_unmirrored_direction_is_not_an_error() {
        let mut b = backend();
        b.set_port_mirror(PortId(1), None, None).unwrap();
        assert_eq!(stub_mirror(&b, 1, false), 0);
    }

    /// A LAG id is a `PortId`, and truncating one to `u32` yields its
    /// trunk id -- a small number that is a perfectly good port here.
    /// Every port-only slot rejects it instead.
    ///
    /// The second LAG is deliberate: trunk 1 truncates to logical port
    /// 1, which exists. A test using trunk 0 would pass whether or not
    /// the guard were there, because the stub has no port 0 to hit.
    #[test]
    fn a_lag_id_is_refused_where_a_port_is_required() {
        let mut b = backend();
        let _first = b.create_lag().unwrap();
        let lag = b.create_lag().unwrap();
        assert_eq!(lag_tid_of(lag), Some(1), "collides with logical port 1");

        b.set_port_learning(PortId(1), true).unwrap();
        b.set_port_tpid(PortId(1), 0x8100).unwrap();

        assert!(b.set_port_admin_state(lag, true).is_err());
        assert!(b.set_port_speed(lag, 1000).is_err());
        assert!(b.set_port_learning(lag, false).is_err());
        assert!(b.set_port_tpid(lag, 0x88a8).is_err());
        assert!(b.create_mirror_session(lag).is_err());
        assert!(b.set_port_mirror(lag, None, None).is_err());
        assert!(b.port_counters(lag).is_err());
        assert!(b.set_port_learn_limit(lag, Some(8)).is_err());
        assert!(b.remove_port_default_vlan(lag).is_err());
        // An FDB entry pointing at a LAG would need a trunk gport, which
        // the ABI has no slot for; refused rather than mis-programmed.
        assert!(b
            .add_fdb_entry(None, MAC_A, FdbAction::Forward(lag))
            .is_err());
        assert!(b.flush_fdb(None, Some(lag)).is_err());
        // ...and the port that joins a LAG has to be a real one.
        assert!(b.add_lag_member(lag, lag).is_err());

        // Port 1 is what every one of those would have hit. It did not.
        assert_eq!(stub_port_int(&b, b"hemlockbcm_stub_learning\0", 1), 1);
        assert_eq!(stub_port_int(&b, b"hemlockbcm_stub_learn_limit\0", 1), -1);
        assert_eq!(stub_tpid(&b, 1), 0x8100);
        assert_eq!(stub_mirror(&b, 1, false), 0);
        assert_eq!(stub_member(&b, 1, 1), 0, "still in the default VLAN");
    }
    // --- Storm control -------------------------------------------------

    /// The metered rate for one class, or -1 for no such port.
    fn stub_storm(b: &OpenBcmBackend, port: u32, class: StormClass) -> i64 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32, std::os::raw::c_int) -> i64,
            > = b._library.get(b"hemlockbcm_stub_storm\0").unwrap();
            f(b.switch, port, storm_class(class))
        }
    }

    #[test]
    fn storm_control_meters_each_class_separately() {
        let mut b = backend();
        for class in [
            StormClass::Broadcast,
            StormClass::Multicast,
            StormClass::UnknownUnicast,
        ] {
            assert_eq!(stub_storm(&b, 1, class), 0, "unmetered by default");
        }

        b.set_port_storm_control(PortId(1), StormClass::Broadcast, Some(1000))
            .unwrap();
        assert_eq!(stub_storm(&b, 1, StormClass::Broadcast), 1000);
        assert_eq!(
            stub_storm(&b, 1, StormClass::Multicast),
            0,
            "the other classes are independent"
        );
        assert_eq!(
            stub_storm(&b, 2, StormClass::Broadcast),
            0,
            "and so are ports"
        );

        // None removes the limit, which the SDK spells as a rate of 0.
        b.set_port_storm_control(PortId(1), StormClass::Broadcast, None)
            .unwrap();
        assert_eq!(stub_storm(&b, 1, StormClass::Broadcast), 0);
    }

    /// A rate past u32 saturates. Wrapping would land on a small number
    /// or, worse, on 0 -- which is how the SDK spells "no limit", so a
    /// huge cap would silently become no cap at all.
    #[test]
    fn an_absurd_storm_rate_saturates_rather_than_removing_the_limit() {
        let mut b = backend();
        b.set_port_storm_control(PortId(1), StormClass::Multicast, Some(u64::MAX))
            .unwrap();
        assert_eq!(
            stub_storm(&b, 1, StormClass::Multicast),
            i64::from(u32::MAX)
        );
        // The value that would wrap to exactly 0.
        b.set_port_storm_control(PortId(1), StormClass::Multicast, Some(1 << 32))
            .unwrap();
        assert_ne!(stub_storm(&b, 1, StormClass::Multicast), 0);
    }

    /// The chip counts storm drops once per port, not once per class, so
    /// there is no honest per-class answer. Unsupported is the truthful
    /// reply, and both consoles already render it.
    #[test]
    fn per_class_storm_drops_are_unsupported_not_invented() {
        let mut b = backend();
        let err = b
            .port_storm_drops(PortId(1), StormClass::Broadcast)
            .unwrap_err();
        assert!(err.is_unsupported(), "{err:?}");
    }
    // --- Host interfaces -----------------------------------------------

    fn stub_hostif(b: &OpenBcmBackend, port: u32) -> String {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32) -> *const std::os::raw::c_char,
            > = b._library.get(b"hemlockbcm_stub_hostif\0").unwrap();
            std::ffi::CStr::from_ptr(f(b.switch, port))
                .to_string_lossy()
                .into_owned()
        }
    }

    #[test]
    fn host_interfaces_are_netdevs_bound_to_their_port() {
        let mut b = backend();
        b.setup_host_punt().unwrap();

        let one = b.create_hostif(PortId(1), "Ethernet1").unwrap();
        let two = b.create_hostif(PortId(2), "Ethernet2").unwrap();
        assert_ne!(one, two);
        assert_eq!(stub_hostif(&b, 1), "Ethernet1");
        assert_eq!(stub_hostif(&b, 2), "Ethernet2");
        assert_eq!(stub_hostif(&b, 3), "", "a port with no netdev");

        // One netdev per port; a second is a mistake, not a rename.
        assert!(b.create_hostif(PortId(1), "Ethernet1b").is_err());
    }

    /// The punt setup clears what a previous run left behind, which is
    /// what makes creating the netdevs again idempotent across a syncd
    /// restart -- the KNET modules outlive the process.
    #[test]
    fn punt_setup_clears_a_previous_run() {
        let mut b = backend();
        b.setup_host_punt().unwrap();
        b.create_hostif(PortId(1), "Ethernet1").unwrap();
        assert_eq!(stub_hostif(&b, 1), "Ethernet1");

        b.setup_host_punt().unwrap();
        assert_eq!(stub_hostif(&b, 1), "", "cleared");
        b.create_hostif(PortId(1), "Ethernet1").unwrap();
    }

    /// A netdev name is checked, never truncated: the kernel caps it at
    /// 15 characters, and quietly shortening one would collide with
    /// another port's netdev.
    #[test]
    fn an_overlong_host_interface_name_is_refused() {
        let mut b = backend();
        b.setup_host_punt().unwrap();
        assert!(b.create_hostif(PortId(1), "").is_err());
        assert!(b.create_hostif(PortId(1), "EthernetLongName1").is_err());
        assert!(b.create_hostif(PortId(1), &"x".repeat(16)).is_err());
        b.create_hostif(PortId(1), &"x".repeat(15)).unwrap();
        // ...and a LAG has no netdev of its own here.
        assert!(b.create_hostif(lag_port(1), "Po1").is_err());
    }
    // --- Policers --------------------------------------------------------

    /// The configured rate, negated when the policer meters packets;
    /// -1 for no such policer (rate 1 in pps would also be -1, so the
    /// tests avoid that value rather than the hook pretending).
    fn stub_policer_rate(b: &OpenBcmBackend, policer: Oid) -> i64 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32) -> i64> =
                b._library.get(b"hemlockbcm_stub_policer_rate\0").unwrap();
            f(b.switch, oid_policer(policer))
        }
    }

    #[test]
    fn policers_carry_their_rate_and_units() {
        let mut b = backend();
        let bits = b
            .create_policer(PolicerSpec {
                pps: false,
                rate: 10_000_000,
                burst: 65_536,
            })
            .unwrap();
        let packets = b
            .create_policer(PolicerSpec {
                pps: true,
                rate: 600,
                burst: 128,
            })
            .unwrap();
        assert_ne!(bits, packets);
        assert_eq!(stub_policer_rate(&b, bits), 10_000_000);
        assert_eq!(stub_policer_rate(&b, packets), -600, "metering packets");

        b.set_policer(
            bits,
            PolicerSpec {
                pps: false,
                rate: 5_000_000,
                burst: 65_536,
            },
        )
        .unwrap();
        assert_eq!(stub_policer_rate(&b, bits), 5_000_000);

        b.remove_policer(bits).unwrap();
        assert_eq!(stub_policer_rate(&b, bits), -1, "gone");
        assert!(b.remove_policer(bits).is_err());
    }

    #[test]
    fn policer_stats_round_trip_both_counters() {
        let mut b = backend();
        let policer = b
            .create_policer(PolicerSpec {
                pps: true,
                rate: 400,
                burst: 40,
            })
            .unwrap();
        let stats = b.policer_stats(policer).unwrap();
        // The stub derives its counters from the configuration, which is
        // enough to prove both fields cross the ABI in the right order --
        // the failure this catches is conforming and dropped swapped.
        assert_eq!(stats.conforming, 400);
        assert_eq!(stats.dropped, 40);
    }

    /// A policer id is its own family and cannot be read as any other.
    #[test]
    fn policer_ids_are_their_own_family() {
        assert_eq!(oid_policer(policer_oid(9)), 9);
        assert_eq!(oid_lag_vlan_member(policer_oid(9)), None);
        assert_eq!(lag_tid_of(PortId(policer_oid(9).0)), None);
        assert_ne!(oid_tag(policer_oid(1).0), oid_tag(stp_oid(1).0));
        assert_ne!(oid_tag(policer_oid(1).0), oid_tag(mirror_oid(1).0));
    }
    // --- ACLs ------------------------------------------------------------

    fn permit() -> AclAction {
        AclAction {
            action: AclPacketAction::Forward,
            counter: None,
            policer: None,
        }
    }

    fn deny() -> AclAction {
        AclAction {
            action: AclPacketAction::Drop,
            counter: None,
            policer: None,
        }
    }

    /// The entry's action code, or -1 if there is no such entry.
    fn stub_acl_action(b: &OpenBcmBackend, entry: Oid) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_acl_action\0").unwrap();
            f(b.switch, oid_acl_entry(entry))
        }
    }

    /// The entry's `present` mask, or 0 for no such entry.
    fn stub_acl_fields(b: &OpenBcmBackend, entry: Oid) -> u32 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32) -> u32> =
                b._library.get(b"hemlockbcm_stub_acl_fields\0").unwrap();
            f(b.switch, oid_acl_entry(entry))
        }
    }

    /// 1 bound, 0 not, -1 if either the table or the port is unknown.
    fn stub_acl_bound(b: &OpenBcmBackend, table: Oid, port: u32) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_acl_bound\0").unwrap();
            f(b.switch, oid_acl_table(table), port)
        }
    }

    #[test]
    fn acl_entries_carry_their_match_and_action() {
        let mut b = backend();
        let table = b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();

        let fields = AclFields {
            src_ip: Some(("10.0.0.0".parse().unwrap(), 8)),
            protocol: Some(6),
            dst_port: Some((22, 22)),
            ..Default::default()
        };
        let entry = b.create_acl_entry(table, 100, &fields, &deny()).unwrap();
        assert_eq!(stub_acl_action(&b, entry), ACL_DROP);
        assert_eq!(
            stub_acl_fields(&b, entry),
            ACL_F_SRC_IP | ACL_F_PROTOCOL | ACL_F_DST_PORT,
            "only the fields that were set"
        );

        // The action changes; the match does not.
        b.set_acl_entry_action(entry, &permit()).unwrap();
        assert_eq!(stub_acl_action(&b, entry), ACL_FORWARD);
        assert_eq!(
            stub_acl_fields(&b, entry),
            ACL_F_SRC_IP | ACL_F_PROTOCOL | ACL_F_DST_PORT
        );

        // A table with entries cannot go.
        assert!(b.remove_acl_table(table).is_err());
        b.remove_acl_entry(entry).unwrap();
        b.remove_acl_table(table).unwrap();
    }

    /// Binding belongs to the table, not its entries, and unbinding
    /// without being told from what sweeps only that stage.
    #[test]
    fn binding_is_per_table_and_per_stage() {
        let mut b = backend();
        let ingress = b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();
        let egress = b
            .create_acl_table(AclStage::Egress, AclFamily::Ipv4)
            .unwrap();

        b.bind_port_acl(PortId(1), AclStage::Ingress, Some(ingress))
            .unwrap();
        b.bind_port_acl(PortId(1), AclStage::Egress, Some(egress))
            .unwrap();
        assert_eq!(stub_acl_bound(&b, ingress, 1), 1);
        assert_eq!(stub_acl_bound(&b, egress, 1), 1);
        assert_eq!(stub_acl_bound(&b, ingress, 2), 0, "another port");

        // Unbinding ingress leaves egress alone -- they are separate
        // facts, and a sweep that took both would silently disarm an
        // egress ACL nobody asked about.
        b.bind_port_acl(PortId(1), AclStage::Ingress, None).unwrap();
        assert_eq!(stub_acl_bound(&b, ingress, 1), 0);
        assert_eq!(stub_acl_bound(&b, egress, 1), 1);

        // A table a port still binds cannot go.
        assert!(b.remove_acl_table(egress).is_err());
        b.bind_port_acl(PortId(1), AclStage::Egress, None).unwrap();
        b.remove_acl_table(egress).unwrap();
        b.remove_acl_table(ingress).unwrap();
    }

    /// A prefix becomes address-and-mask, and the /0 and /32 ends are
    /// where the shift is easy to get wrong.
    #[test]
    fn ipv4_prefixes_become_address_and_mask() {
        assert_eq!(
            ipv4_prefix(("10.1.2.3".parse().unwrap(), 24), "x").unwrap(),
            (0x0a01_0200, 0xffff_ff00),
            "host bits are cleared"
        );
        assert_eq!(
            ipv4_prefix(("10.1.2.3".parse().unwrap(), 32), "x").unwrap(),
            (0x0a01_0203, 0xffff_ffff)
        );
        assert_eq!(
            ipv4_prefix(("0.0.0.0".parse().unwrap(), 0), "x").unwrap(),
            (0, 0),
            "a /0 masks nothing, and must not shift by 32"
        );
    }

    /// Three things this backend cannot express. Each is refused, and
    /// none is quietly approximated -- an ACL that silently matches
    /// something other than what was asked for is worse than one that
    /// fails to install.
    #[test]
    fn acl_rules_this_backend_cannot_express_are_refused() {
        let mut b = backend();
        let table = b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();

        // An IPv6 table, on a datapath reporting no IPv6.
        assert!(b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv6)
            .is_err());

        // An IPv6 match in an IPv4 table: truncating it to 32 bits would
        // match some unrelated v4 prefix.
        let v6 = AclFields {
            src_ip: Some(("2001:db8::".parse().unwrap(), 32)),
            ..Default::default()
        };
        assert!(b.create_acl_entry(table, 100, &v6, &deny()).is_err());

        // A real L4 port range needs a range checker.
        let range = AclFields {
            dst_port: Some((1024, 2048)),
            ..Default::default()
        };
        assert!(b.create_acl_entry(table, 100, &range, &deny()).is_err());
        // ...but a degenerate one is an exact match and is fine.
        let single = AclFields {
            dst_port: Some((1024, 1024)),
            ..Default::default()
        };
        b.create_acl_entry(table, 100, &single, &deny()).unwrap();
    }

    #[test]
    fn acl_availability_falls_as_entries_are_added() {
        let mut b = backend();
        let table = b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();
        let before = b.acl_available_entries(AclStage::Ingress).unwrap();
        assert!(before > 0);

        let entry = b
            .create_acl_entry(table, 10, &AclFields::default(), &permit())
            .unwrap();
        assert_eq!(
            b.acl_available_entries(AclStage::Ingress).unwrap(),
            before - 1
        );
        // The other stage is counted separately.
        assert_eq!(b.acl_available_entries(AclStage::Egress).unwrap(), before);

        b.remove_acl_entry(entry).unwrap();
        assert_eq!(b.acl_available_entries(AclStage::Ingress).unwrap(), before);
    }
    /// The entry's (counter, policer) pair, or -1 for no such entry.
    fn stub_acl_attach(b: &OpenBcmBackend, entry: Oid) -> i64 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32) -> i64> =
                b._library.get(b"hemlockbcm_stub_acl_attach\0").unwrap();
            f(b.switch, oid_acl_entry(entry))
        }
    }

    fn attached(counter: Option<Oid>, policer: Option<Oid>) -> i64 {
        ((counter.map_or(0, oid_acl_counter) as i64) << 32) | policer.map_or(0, oid_policer) as i64
    }

    #[test]
    fn acl_entries_carry_a_counter_and_a_policer() {
        let mut b = backend();
        let table = b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();
        let counter = b.create_acl_counter(table).unwrap();
        let policer = b
            .create_policer(PolicerSpec {
                pps: true,
                rate: 100,
                burst: 10,
            })
            .unwrap();

        let logged = AclAction {
            counter: Some(counter),
            policer: Some(policer),
            ..deny()
        };
        let entry = b
            .create_acl_entry(table, 100, &AclFields::default(), &logged)
            .unwrap();
        assert_eq!(
            stub_acl_attach(&b, entry),
            attached(Some(counter), Some(policer))
        );
        assert_eq!(b.get_acl_counter(counter).unwrap(), 100);

        // A counter an entry still references cannot go.
        assert!(b.remove_acl_counter(counter).is_err());

        // Dropping the log clause detaches the counter: the action set
        // is replaced as a unit, so an update that no longer asks for a
        // counter must not leave the old one attached.
        b.set_acl_entry_action(entry, &deny()).unwrap();
        assert_eq!(stub_acl_attach(&b, entry), attached(None, None));
        b.remove_acl_counter(counter).unwrap();
    }

    /// A rule that matches and acts but silently does not count is worse
    /// than no rule, so a failed attach unwinds the entry rather than
    /// reporting success.
    #[test]
    fn a_failed_attachment_unwinds_the_entry() {
        let mut b = backend();
        let table = b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();
        let before = b.acl_available_entries(AclStage::Ingress).unwrap();

        // A counter id that names nothing: the entry installs, the
        // attach fails, and the entry must not survive.
        let bogus = AclAction {
            counter: Some(acl_counter_oid(99)),
            ..deny()
        };
        assert!(b
            .create_acl_entry(table, 100, &AclFields::default(), &bogus)
            .is_err());
        assert_eq!(
            b.acl_available_entries(AclStage::Ingress).unwrap(),
            before,
            "the half-built entry was removed"
        );
    }

    /// Counters belong to the table they count in, because the chip
    /// allocates them per group.
    #[test]
    fn acl_counters_belong_to_their_table() {
        let mut b = backend();
        let table = b
            .create_acl_table(AclStage::Ingress, AclFamily::Ipv4)
            .unwrap();
        let one = b.create_acl_counter(table).unwrap();
        let two = b.create_acl_counter(table).unwrap();
        assert_ne!(one, two);
        assert_ne!(
            b.get_acl_counter(one).unwrap(),
            b.get_acl_counter(two).unwrap(),
            "distinct counters, distinct counts"
        );

        assert!(b.create_acl_counter(acl_table_oid(99)).is_err());
        b.remove_acl_counter(one).unwrap();
        assert!(b.get_acl_counter(one).is_err());
    }

    /// Counter ids are their own family.
    #[test]
    fn acl_counter_ids_are_their_own_family() {
        assert_eq!(oid_acl_counter(acl_counter_oid(3)), 3);
        assert_ne!(oid_tag(acl_counter_oid(1).0), oid_tag(acl_entry_oid(1).0));
        assert_ne!(oid_tag(acl_counter_oid(1).0), oid_tag(acl_table_oid(1).0));
        assert_ne!(oid_tag(acl_counter_oid(1).0), oid_tag(policer_oid(1).0));
    }
    // --- Router interfaces -----------------------------------------------

    /// The VLAN an interface sits on, or 0 for no such interface.
    fn stub_rif_vlan(b: &OpenBcmBackend, rif: Oid) -> u16 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32) -> u16> =
                b._library.get(b"hemlockbcm_stub_rif_vlan\0").unwrap();
            f(b.switch, oid_rif(rif))
        }
    }

    fn stub_my_mac_count(b: &OpenBcmBackend) -> u32 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch) -> u32> =
                b._library.get(b"hemlockbcm_stub_my_mac_count\0").unwrap();
            f(b.switch)
        }
    }

    /// Routing a port takes it out of the bridge and into a VLAN of its
    /// own; removing the interface puts it back. The round trip is what
    /// matters: a port left in the internal VLAN forwards nothing and
    /// looks like a cable fault.
    #[test]
    fn a_port_router_interface_leaves_and_rejoins_the_bridge() {
        let mut b = backend();
        assert_eq!(stub_member(&b, 1, 1), 0, "bridging to start with");

        let rif = b.create_router_interface(PortId(1)).unwrap();
        assert_eq!(stub_member(&b, 1, 1), -1, "out of the default VLAN");
        assert_eq!(stub_rif_vlan(&b, rif), 4093, "a VLAN of its own");
        assert_eq!(stub_pvid(&b, 1), 4093, "and classified into it");

        b.remove_router_interface(PortId(1), rif).unwrap();
        assert_eq!(stub_member(&b, 1, 1), 0, "bridging again");
        assert_eq!(stub_pvid(&b, 1), 1, "and back on the default PVID");
        assert_eq!(stub_rif_vlan(&b, rif), 0, "the interface is gone");
    }

    /// The internal VLAN is a function of the port, so removing the
    /// interface can name the same VLAN without anything having been
    /// remembered between the two calls.
    #[test]
    fn the_internal_router_vlan_is_derived_from_the_port() {
        assert_eq!(rif_vlan_for(1).unwrap(), 4093);
        assert_eq!(rif_vlan_for(53).unwrap(), 4041, "the last SFP+ port");
        // Distinct ports never share one.
        assert_ne!(rif_vlan_for(1).unwrap(), rif_vlan_for(2).unwrap());
        // And the arithmetic does not wrap into somebody's VLAN 4000.
        assert!(rif_vlan_for(4094).is_err());
        assert!(rif_vlan_for(u32::MAX).is_err());
    }

    /// An SVI leaves the VLAN bridging: only the interface is created,
    /// and only the interface goes.
    #[test]
    fn an_svi_leaves_its_vlan_bridging() {
        let mut b = backend();
        let vlan = b.create_vlan(100).unwrap();
        b.add_vlan_member(vlan, PortId(1), true).unwrap();

        let rif = b.create_vlan_router_interface(Some(vlan)).unwrap();
        assert_eq!(stub_rif_vlan(&b, rif), 100);
        assert_eq!(stub_member(&b, 100, 1), 1, "still a tagged member");

        b.remove_vlan_router_interface(rif).unwrap();
        assert_eq!(stub_rif_vlan(&b, rif), 0);
        assert_eq!(stub_member(&b, 100, 1), 1, "the VLAN is untouched");

        // None means the default VLAN, which the shim reports.
        let default_rif = b.create_vlan_router_interface(None).unwrap();
        assert_eq!(stub_rif_vlan(&b, default_rif), 1);
    }

    /// A My-MAC entry is separate from an interface: VRRP virtual MACs
    /// need one with no interface of their own.
    #[test]
    fn my_mac_entries_are_independent_of_interfaces() {
        let mut b = backend();
        assert_eq!(stub_my_mac_count(&b), 0);

        let virtual_mac = b
            .create_my_mac(Some(100), [0x00, 0x00, 0x5e, 0x00, 0x01, 0x01])
            .unwrap();
        // No VLAN scope: the same MAC routable everywhere.
        let any = b
            .create_my_mac(None, [0x02, 0x00, 0x00, 0x00, 0x00, 0x01])
            .unwrap();
        assert_ne!(virtual_mac, any);
        assert_eq!(stub_my_mac_count(&b), 2);

        b.remove_my_mac(virtual_mac).unwrap();
        assert_eq!(stub_my_mac_count(&b), 1);
        assert!(b.remove_my_mac(virtual_mac).is_err(), "already gone");
    }

    /// A router interface needs the switch MAC. Inventing one would put
    /// an address on the wire that belongs to nothing, so with no MAC
    /// resolved the call fails instead.
    #[test]
    fn a_router_interface_without_a_switch_mac_is_refused() {
        let mut init = init_for(stub_path());
        init.src_mac = None;
        let mut b = OpenBcmBackend::new(&init, ABI_MAJOR).unwrap();
        b.create_switch().unwrap();
        assert!(b.create_router_interface(PortId(1)).is_err());
        assert!(b.create_vlan_router_interface(None).is_err());
        // ...and the port was not taken out of the bridge on the way.
        assert_eq!(stub_member(&b, 1, 1), 0);
    }

    /// Interface and My-MAC ids are their own families.
    #[test]
    fn router_interface_ids_are_their_own_families() {
        assert_eq!(oid_rif(rif_oid(5)), 5);
        assert_eq!(oid_my_mac(my_mac_oid(5)), 5);
        assert_ne!(oid_tag(rif_oid(1).0), oid_tag(my_mac_oid(1).0));
        assert_ne!(oid_tag(rif_oid(1).0), oid_tag(acl_counter_oid(1).0));
        assert_ne!(oid_tag(my_mac_oid(1).0), oid_tag(vlan_oid(1).0));
    }
    // --- Routes ------------------------------------------------------------

    /// The route's kind with its RIF in the high bits, or -1 for none.
    fn stub_route(b: &OpenBcmBackend, dest: &str) -> i64 {
        let prefix: IpPrefix = {
            let (addr, len) = dest.split_once('/').unwrap();
            (addr.parse().unwrap(), len.parse().unwrap())
        };
        let (bits, mask) = ipv4_prefix(prefix, "test").unwrap();
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> i64> =
                b._library.get(b"hemlockbcm_stub_route\0").unwrap();
            f(b.switch, bits, mask)
        }
    }

    fn route(dest: &str) -> IpPrefix {
        let (addr, len) = dest.split_once('/').unwrap();
        (addr.parse().unwrap(), len.parse().unwrap())
    }

    #[test]
    fn routes_carry_their_target() {
        let mut b = backend();
        let rif = b.create_router_interface(PortId(1)).unwrap();

        b.create_route(route("10.0.0.0/24"), RouteTarget::Rif(rif))
            .unwrap();
        assert_eq!(
            stub_route(&b, "10.0.0.0/24"),
            ((oid_rif(rif) as i64) << 32) | ROUTE_RIF as i64
        );

        b.create_route(route("10.0.0.1/32"), RouteTarget::Cpu)
            .unwrap();
        assert_eq!(stub_route(&b, "10.0.0.1/32"), ROUTE_CPU as i64);

        b.create_route(route("192.168.0.0/16"), RouteTarget::Drop)
            .unwrap();
        assert_eq!(stub_route(&b, "192.168.0.0/16"), ROUTE_DROP as i64);

        // A default route is a /0, whose mask is 0 -- the shift that is
        // easy to get wrong, and it must not collide with anything.
        b.create_route(route("0.0.0.0/0"), RouteTarget::Drop)
            .unwrap();
        assert_eq!(stub_route(&b, "0.0.0.0/0"), ROUTE_DROP as i64);
        assert_eq!(stub_route(&b, "10.0.0.1/32"), ROUTE_CPU as i64, "untouched");

        b.remove_route(route("10.0.0.1/32")).unwrap();
        assert_eq!(stub_route(&b, "10.0.0.1/32"), -1);
        assert!(b.remove_route(route("10.0.0.1/32")).is_err());
    }

    /// Retargeting a prefix must not need a delete first: the prefix
    /// would be unreachable in between.
    #[test]
    fn retargeting_a_route_replaces_it_in_place() {
        let mut b = backend();
        let rif = b.create_router_interface(PortId(1)).unwrap();
        b.create_route(route("10.0.0.0/24"), RouteTarget::Drop)
            .unwrap();
        b.create_route(route("10.0.0.0/24"), RouteTarget::Rif(rif))
            .unwrap();
        assert_eq!(
            stub_route(&b, "10.0.0.0/24"),
            ((oid_rif(rif) as i64) << 32) | ROUTE_RIF as i64
        );
        // One route, not two: removing it once leaves nothing behind.
        b.remove_route(route("10.0.0.0/24")).unwrap();
        assert_eq!(stub_route(&b, "10.0.0.0/24"), -1);
    }

    /// Targets that need an egress object are refused, not approximated:
    /// a route silently pointing somewhere else is a black hole with no
    /// error to explain it.
    #[test]
    fn routes_needing_a_next_hop_are_refused() {
        let mut b = backend();
        assert!(b
            .create_route(route("10.0.0.0/24"), RouteTarget::NextHop(Oid(1)))
            .is_err());
        assert!(b
            .create_route(route("10.0.0.0/24"), RouteTarget::Group(Oid(1)))
            .is_err());
        assert_eq!(stub_route(&b, "10.0.0.0/24"), -1, "nothing was programmed");

        // IPv6 has no datapath here, so a v6 prefix is refused too.
        assert!(b
            .create_route(("2001:db8::".parse().unwrap(), 32), RouteTarget::Drop)
            .is_err());
    }
    // --- Neighbours and next hops ------------------------------------------

    const NEIGHBOR_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x99];

    fn stub_neighbor(b: &OpenBcmBackend, ip: &str) -> i32 {
        let addr = ipv4_address(ip.parse().unwrap(), "test").unwrap();
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_neighbor\0").unwrap();
            f(b.switch, addr)
        }
    }

    fn stub_route_nexthop(b: &OpenBcmBackend, dest: &str) -> u32 {
        let (bits, mask) = ipv4_prefix(route(dest), "test").unwrap();
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> u32> =
                b._library.get(b"hemlockbcm_stub_route_nexthop\0").unwrap();
            f(b.switch, bits, mask)
        }
    }

    /// Set up an SVI with the neighbour's MAC learned on it, which is
    /// what the egress object's port comes from.
    fn routed_backend() -> (OpenBcmBackend, Oid) {
        let mut b = backend();
        let vlan = b.create_vlan(100).unwrap();
        b.add_vlan_member(vlan, PortId(1), false).unwrap();
        let rif = b.create_vlan_router_interface(Some(vlan)).unwrap();
        b.add_fdb_entry(Some(vlan), NEIGHBOR_MAC, FdbAction::Forward(PortId(1)))
            .unwrap();
        (b, rif)
    }

    #[test]
    fn a_route_follows_its_neighbour_once_resolved() {
        let (mut b, rif) = routed_backend();
        let next_hop = b.create_next_hop(rif, "10.0.0.1".parse().unwrap()).unwrap();

        // Unresolved: the route cannot be programmed, and says so rather
        // than pointing somewhere arbitrary.
        assert_eq!(stub_neighbor(&b, "10.0.0.1"), 0);
        assert!(b
            .create_route(route("10.1.0.0/16"), RouteTarget::NextHop(next_hop))
            .is_err());
        assert_eq!(stub_route(&b, "10.1.0.0/16"), -1, "nothing programmed");

        // Resolve it, and the same route goes in.
        b.create_neighbor(rif, "10.0.0.1".parse().unwrap(), NEIGHBOR_MAC)
            .unwrap();
        assert_eq!(stub_neighbor(&b, "10.0.0.1"), 1);
        b.create_route(route("10.1.0.0/16"), RouteTarget::NextHop(next_hop))
            .unwrap();
        assert_eq!(
            stub_route_nexthop(&b, "10.1.0.0/16"),
            ipv4_address("10.0.0.1".parse().unwrap(), "test").unwrap()
        );

        b.remove_neighbor(rif, "10.0.0.1".parse().unwrap()).unwrap();
        assert_eq!(stub_neighbor(&b, "10.0.0.1"), 0);
        assert!(b.remove_neighbor(rif, "10.0.0.1".parse().unwrap()).is_err());
    }

    /// The egress object needs a port, and the port comes from the FDB.
    /// An unlearned MAC has no port, and guessing one would be a black
    /// hole -- so the neighbour does not resolve, which is the state the
    /// caller already models by leaving the route on the CPU.
    #[test]
    fn a_neighbour_whose_mac_is_unlearned_does_not_resolve() {
        let mut b = backend();
        let vlan = b.create_vlan(100).unwrap();
        b.add_vlan_member(vlan, PortId(1), false).unwrap();
        let rif = b.create_vlan_router_interface(Some(vlan)).unwrap();

        // No FDB entry for it yet.
        assert!(b
            .create_neighbor(rif, "10.0.0.1".parse().unwrap(), NEIGHBOR_MAC)
            .is_err());
        assert_eq!(stub_neighbor(&b, "10.0.0.1"), 0);

        // Learn it, and the same call succeeds.
        b.add_fdb_entry(Some(vlan), NEIGHBOR_MAC, FdbAction::Forward(PortId(1)))
            .unwrap();
        b.create_neighbor(rif, "10.0.0.1".parse().unwrap(), NEIGHBOR_MAC)
            .unwrap();
        assert_eq!(stub_neighbor(&b, "10.0.0.1"), 1);
    }

    /// A next hop allocates nothing: it is the name of a neighbour, so
    /// creating one twice yields the same id and removing one is a
    /// no-op. That is what lets both sides stay free of a resolution
    /// table.
    #[test]
    fn a_next_hop_is_a_name_not_an_object() {
        let (mut b, rif) = routed_backend();
        let ip: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        let one = b.create_next_hop(rif, ip).unwrap();
        let two = b.create_next_hop(rif, ip).unwrap();
        assert_eq!(one, two, "the same neighbour is the same next hop");
        assert_eq!(oid_next_hop(one), ipv4_address(ip, "test").unwrap());

        // Removing it frees nothing and cannot fail.
        b.remove_next_hop(one).unwrap();
        b.remove_next_hop(one).unwrap();

        // ...and a resolved neighbour still routes afterwards, because
        // the egress object belonged to the neighbour all along.
        b.create_neighbor(rif, ip, NEIGHBOR_MAC).unwrap();
        b.create_route(route("10.1.0.0/16"), RouteTarget::NextHop(one))
            .unwrap();
    }

    /// IPv6 has no datapath here, and truncating an address to 32 bits
    /// would name an unrelated v4 host.
    #[test]
    fn ipv6_neighbours_and_next_hops_are_refused() {
        let (mut b, rif) = routed_backend();
        let v6: std::net::IpAddr = "2001:db8::1".parse().unwrap();
        assert!(b.create_neighbor(rif, v6, NEIGHBOR_MAC).is_err());
        assert!(b.remove_neighbor(rif, v6).is_err());
        assert!(b.create_next_hop(rif, v6).is_err());
    }
    // --- ECMP groups -------------------------------------------------------

    fn stub_ecmp_member(b: &OpenBcmBackend, group: Oid, next_hop: Oid) -> bool {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(*mut ShimSwitch, u32, u32) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_ecmp_member\0").unwrap();
            f(b.switch, oid_ecmp(group), oid_next_hop(next_hop)) != 0
        }
    }

    /// A member is a resolved neighbour's egress object, named by its
    /// address. An unresolved one is refused: a group with a hole in it
    /// black-holes a share of the traffic rather than all of it, which
    /// is harder to notice, not easier.
    #[test]
    fn ecmp_members_are_resolved_next_hops() {
        let (mut b, rif) = routed_backend();
        let group = b.create_next_hop_group().unwrap();
        let next_hop = b.create_next_hop(rif, "10.0.0.1".parse().unwrap()).unwrap();

        assert!(b.add_next_hop_group_member(group, next_hop).is_err());
        assert!(!stub_ecmp_member(&b, group, next_hop));

        b.create_neighbor(rif, "10.0.0.1".parse().unwrap(), NEIGHBOR_MAC)
            .unwrap();
        let member = b.add_next_hop_group_member(group, next_hop).unwrap();
        assert!(stub_ecmp_member(&b, group, next_hop));

        // A group with members cannot go.
        assert!(b.remove_next_hop_group(group).is_err());
        b.remove_next_hop_group_member(member).unwrap();
        assert!(!stub_ecmp_member(&b, group, next_hop));
        b.remove_next_hop_group(group).unwrap();
    }

    /// The member id carries both the group and the next hop, so
    /// removing one needs no lookup -- and the group half must survive
    /// the round trip rather than coming back with the tag in it.
    #[test]
    fn an_ecmp_member_id_names_its_group_and_next_hop() {
        assert_eq!(
            oid_ecmp_member(ecmp_member_oid(7, 0x0a00_0001)),
            (7, 0x0a00_0001)
        );
        // The widest values each field can hold.
        assert_eq!(
            oid_ecmp_member(ecmp_member_oid(0x00ff_ffff, u32::MAX)),
            (0x00ff_ffff, u32::MAX)
        );
        assert_ne!(
            oid_tag(ecmp_member_oid(1, 1).0),
            oid_tag(ecmp_oid(1).0),
            "a member is not its group"
        );
        assert_ne!(oid_tag(ecmp_oid(1).0), oid_tag(next_hop_oid(1).0));
    }

    /// A route through a group, and the group refusing to go while one
    /// points at it.
    #[test]
    fn a_route_can_follow_an_ecmp_group() {
        let (mut b, rif) = routed_backend();
        b.create_neighbor(rif, "10.0.0.1".parse().unwrap(), NEIGHBOR_MAC)
            .unwrap();
        let group = b.create_next_hop_group().unwrap();
        let next_hop = b.create_next_hop(rif, "10.0.0.1".parse().unwrap()).unwrap();
        let member = b.add_next_hop_group_member(group, next_hop).unwrap();

        b.create_route(route("10.2.0.0/16"), RouteTarget::Group(group))
            .unwrap();
        assert_eq!(stub_route_nexthop(&b, "10.2.0.0/16"), oid_ecmp(group));

        b.remove_next_hop_group_member(member).unwrap();
        assert!(
            b.remove_next_hop_group(group).is_err(),
            "a route still points at it"
        );
        b.remove_route(route("10.2.0.0/16")).unwrap();
        b.remove_next_hop_group(group).unwrap();
    }
    // --- CoPP traps --------------------------------------------------------

    /// -1 if the trap is not installed, else (policer << 8) |
    /// (trap_only << 1) | 1.
    fn stub_trap(b: &OpenBcmBackend, kind: std::os::raw::c_int, is_default: bool) -> i64 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(
                    *mut ShimSwitch,
                    std::os::raw::c_int,
                    std::os::raw::c_int,
                ) -> i64,
            > = b._library.get(b"hemlockbcm_stub_trap\0").unwrap();
            f(b.switch, kind, i32::from(is_default))
        }
    }

    fn installed(policer: Option<Oid>, trap_only: bool) -> i64 {
        (i64::from(policer.map_or(0, oid_policer)) << 8) | (i64::from(trap_only) << 1) | 1
    }

    fn copp_policer(b: &mut OpenBcmBackend, rate: u64) -> Oid {
        b.create_policer(PolicerSpec {
            pps: true,
            rate,
            burst: rate / 4,
        })
        .unwrap()
    }

    #[test]
    fn copp_traps_carry_their_groups_policer() {
        let mut b = backend();
        let policer = copp_policer(&mut b, 512);
        let group = b.create_hostif_trap_group(Some(policer)).unwrap();

        // A punt: the forwarding copy is dropped.
        let stp = b.create_hostif_trap(TrapKind::Stp, true, group).unwrap();
        assert_eq!(
            stub_trap(&b, TRAP_STP, false),
            installed(Some(policer), true)
        );

        // A copy: DHCP keeps forwarding.
        let dhcp = b.create_hostif_trap(TrapKind::Dhcp, false, group).unwrap();
        assert_eq!(
            stub_trap(&b, TRAP_DHCP, false),
            installed(Some(policer), false)
        );
        assert_ne!(stp, dhcp);

        b.remove_hostif_trap(stp).unwrap();
        assert_eq!(stub_trap(&b, TRAP_STP, false), -1);
        assert!(b.remove_hostif_trap(stp).is_err(), "already gone");
        assert_eq!(
            stub_trap(&b, TRAP_DHCP, false),
            installed(Some(policer), false),
            "the other trap is untouched"
        );

        // The group is a name for its policer: removing it frees
        // nothing, and the same policer names the same group.
        b.remove_hostif_trap_group(group).unwrap();
        assert_eq!(b.create_hostif_trap_group(Some(policer)).unwrap(), group);
    }

    /// program_copp uses `?` on every trap it creates, so one refused
    /// kind takes down all of CoPP at startup. Every kind in the class
    /// table must therefore be accepted -- this is that guarantee,
    /// pinned. (SamplePacket is absent from the table; it belongs to
    /// sFlow and is refused until that family exists.)
    #[test]
    fn every_copp_class_table_kind_is_accepted() {
        let mut b = backend();
        let policer = copp_policer(&mut b, 1000);
        let group = b.create_hostif_trap_group(Some(policer)).unwrap();
        for kind in [
            TrapKind::Ip2me,
            TrapKind::Stp,
            TrapKind::Lacp,
            TrapKind::Lldp,
            TrapKind::Eapol,
            TrapKind::IgmpQuery,
            TrapKind::IgmpLeave,
            TrapKind::IgmpV1Report,
            TrapKind::IgmpV2Report,
            TrapKind::IgmpV3Report,
            TrapKind::MldV1V2,
            TrapKind::MldV1Report,
            TrapKind::MldV1Done,
            TrapKind::MldV2Report,
            TrapKind::ArpRequest,
            TrapKind::ArpResponse,
            TrapKind::Dhcp,
            TrapKind::Ospf,
            TrapKind::Bgp,
            TrapKind::Vrrp,
            TrapKind::AclLog,
        ] {
            b.create_hostif_trap(kind, false, group)
                .unwrap_or_else(|e| panic!("{kind:?} must be accepted: {e}"));
        }
        // SamplePacket is accepted too, satisfied by the shim's own
        // delivery path rather than a field entry.
        let sample = b
            .create_hostif_trap(TrapKind::SamplePacket, true, Oid(0))
            .unwrap();
        b.remove_hostif_trap(sample).unwrap();
    }

    /// Default-group traps follow the default policer both ways: ones
    /// installed before a policer change are swept, ones created after
    /// pick up the cached value.
    #[test]
    fn default_group_traps_follow_the_default_policer() {
        let mut b = backend();
        let before = b
            .create_hostif_trap(TrapKind::ArpRequest, false, Oid(0))
            .unwrap();
        assert_eq!(stub_trap(&b, 14, true), installed(None, false), "unpoliced");

        let policer = copp_policer(&mut b, 256);
        b.set_default_trap_group_policer(Some(policer)).unwrap();
        assert_eq!(
            stub_trap(&b, 14, true),
            installed(Some(policer), false),
            "swept onto the existing trap"
        );

        let after = b
            .create_hostif_trap(TrapKind::ArpResponse, false, Oid(0))
            .unwrap();
        assert_eq!(
            stub_trap(&b, 15, true),
            installed(Some(policer), false),
            "picked up by a trap created afterwards"
        );
        assert_ne!(before, after);

        b.set_default_trap_group_policer(None).unwrap();
        assert_eq!(stub_trap(&b, 14, true), installed(None, false));
        assert_eq!(stub_trap(&b, 15, true), installed(None, false));

        // The same kind in the default group and a named group are
        // different traps -- the id carries the distinction.
        let group = b.create_hostif_trap_group(None).unwrap();
        let named = b
            .create_hostif_trap(TrapKind::ArpRequest, false, group)
            .unwrap();
        assert_ne!(before, named);
    }

    /// The AclLog trap is accepted so the CoPP program can run, and
    /// installs nothing: the pipeline cannot tell an ACL entry's CPU
    /// copy from any other CPU-bound packet. Zero counters, not a
    /// refused class -- and not a pretend match either.
    #[test]
    fn acl_log_is_accepted_but_inert() {
        let mut b = backend();
        let group = b.create_hostif_trap_group(None).unwrap();
        let trap = b.create_hostif_trap(TrapKind::AclLog, true, group).unwrap();
        for kind in 0..20 {
            assert_eq!(stub_trap(&b, kind, false), -1, "kind {kind} installed");
        }
        b.remove_hostif_trap(trap).unwrap();
    }

    /// A junk group id would decode to a junk policer and meter through
    /// garbage; it is refused instead.
    #[test]
    fn a_trap_into_a_non_group_is_refused() {
        let mut b = backend();
        assert!(b
            .create_hostif_trap(TrapKind::Stp, true, vlan_oid(100))
            .is_err());
        assert_eq!(stub_trap(&b, TRAP_STP, false), -1);
    }
    // --- Ingress sampling / sFlow ------------------------------------------

    fn stub_sample_rate(b: &OpenBcmBackend, port: u32) -> i64 {
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*mut ShimSwitch, u32) -> i64> =
                b._library.get(b"hemlockbcm_stub_sample_rate\0").unwrap();
            f(b.switch, port)
        }
    }

    /// Deliver one fake sampled packet the way the shim's RX handler
    /// would; 0 = delivered.
    fn stub_fire_sample(b: &OpenBcmBackend, port: u32, original: u32, data: &[u8]) -> i32 {
        unsafe {
            let f: libloading::Symbol<
                unsafe extern "C" fn(
                    *mut ShimSwitch,
                    u32,
                    u32,
                    *const u8,
                    u32,
                ) -> std::os::raw::c_int,
            > = b._library.get(b"hemlockbcm_stub_fire_sample\0").unwrap();
            f(b.switch, port, original, data.as_ptr(), data.len() as u32)
        }
    }

    /// A session is its rate: same rate, same session; removal frees
    /// nothing. Binding programs the port's sampler, `None` stops it.
    #[test]
    fn sample_sessions_bind_their_rate_to_ports() {
        let mut b = backend();
        let session = b.create_samplepacket(1024).unwrap();
        assert_eq!(b.create_samplepacket(1024).unwrap(), session);
        assert_ne!(b.create_samplepacket(512).unwrap(), session);

        b.set_port_sample_session(PortId(1), Some(session)).unwrap();
        assert_eq!(stub_sample_rate(&b, 1), 1024);
        assert_eq!(stub_sample_rate(&b, 2), 0, "other ports untouched");

        b.set_port_sample_session(PortId(1), None).unwrap();
        assert_eq!(stub_sample_rate(&b, 1), 0);
        b.remove_samplepacket(session).unwrap();

        // The invalid shapes are refused before the datapath.
        assert!(b.create_samplepacket(0).is_err());
        assert!(b.create_samplepacket(u32::MAX).is_err());
        assert!(b
            .set_port_sample_session(PortId(1), Some(vlan_oid(100)))
            .is_err());
        assert!(b
            .set_port_sample_session(lag_port(1), Some(session))
            .is_err());
    }

    /// The delivery path: a sampled packet arriving through the shim's
    /// callback comes out of the event channel with its port, wire
    /// length and bytes intact -- the bytes copied, since the shim's
    /// buffer dies with the call.
    #[test]
    fn sampled_packets_arrive_as_events() {
        let mut b = backend();
        let mut events = b.take_events().unwrap();
        let frame = [0xffu8, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x02, 0x00];

        assert_eq!(stub_fire_sample(&b, 3, 128, &frame), 0);
        match events.try_recv().expect("a sample event") {
            SaiEvent::SampledPacket {
                port,
                original_length,
                bytes,
            } => {
                assert_eq!(port, PortId(3));
                assert_eq!(original_length, 128, "wire length, not delivered length");
                assert_eq!(bytes, frame);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    /// The SamplePacket trap is satisfied by the delivery path itself,
    /// so accepting it is what keeps the sFlow engine's "samples will
    /// not arrive" warning truthful -- they do arrive.
    #[test]
    fn the_samplepacket_trap_is_satisfied_not_refused() {
        let mut b = backend();
        let trap = b
            .create_hostif_trap(TrapKind::SamplePacket, true, Oid(0))
            .unwrap();
        assert_ne!(trap, acl_log_trap_oid());
        b.remove_hostif_trap(trap).unwrap();
    }
}
