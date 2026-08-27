/*
 * hemlockbcm.h — the libhemlockbcm ABI.
 *
 * SPDX-License-Identifier: MIT
 * Copyright (c) Nightshade Systems. Part of Hemlock; MIT like the rest of
 * Hemlock's own code. The shim that implements this header is built inside
 * a Broadcom OpenBCM tree on the operator's machine and is never shipped
 * from this repository.
 *
 * ---------------------------------------------------------------------
 *
 * This file is the whole contract between Hemlock and the OpenBCM SDK.
 *
 * Hemlock drives Broadcom XGS ASICs through SAI. The AS4610-54T cannot:
 * its host CPU is an on-die ARM Cortex-A9 and no libsaibcm is published
 * for armhf. So for that board — and only where the same is true — the
 * datapath is a thin C shim built inside the SDK's own tree, exporting
 * the small, versioned ABI below. `OpenBcmBackend` in
 * src/hemlock-sai/src/openbcm.rs dlopens it and implements `SaiBackend`
 * over it, so nothing above hemlock-sai can tell the two apart.
 *
 * The slots mirror the `SaiBackend` trait, in trait order. Deliberately
 * *not* SAI: this is the surface Hemlock actually uses, which is a small
 * fraction of SAI's, and keeping it that way is the point.
 *
 * Versioning
 * ----------
 *   MAJOR  bumps on any incompatible change: a slot's signature or
 *          semantics changes, or a slot is removed or reordered. Rust
 *          refuses to load a shim whose major differs from the one the
 *          platform manifest pins (`[sai] abi_major`).
 *   MINOR  bumps when slots are appended to the end of the struct. Rust
 *          accepts any minor and uses `struct_size` to know how much of
 *          the vtable is really there.
 *
 * A NULL slot is legal and means "not implemented on this platform".
 * Rust turns it into the same not-implemented error a SAI missing an
 * object family returns, so `capabilities()` stays truthful and both
 * consoles degrade the way they already do. Phase 6 fills slots in one
 * at a time; until then most of them are NULL by design.
 *
 * Ownership and threading
 * -----------------------
 * Every call is synchronous and blocking, made from Hemlock's single SAI
 * actor thread — the same single-threaded access pattern the vendor SAI
 * gets. The exception is the event callback, which the shim may invoke
 * from its own thread; Rust forwards those into a channel.
 *
 * Strings the shim returns point into shim-owned storage that must stay
 * valid until the next call on the same switch. Rust copies immediately.
 */

#ifndef HEMLOCKBCM_H
#define HEMLOCKBCM_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define HEMLOCKBCM_ABI_MAJOR 1
#define HEMLOCKBCM_ABI_MINOR 5

/*
 * Symbol visibility. The real shim is an ELF .so, where the entry point
 * is exported by default; this exists so the test stub also builds with
 * MSVC on a developer's machine, which exports nothing unless told.
 *
 * Always "export", never "import": nothing links against this header —
 * the Rust side transcribes the ABI by hand and resolves the symbol with
 * dlopen — so there is no consumer side to get wrong.
 */
#if defined(_MSC_VER)
#define HEMLOCKBCM_EXPORT __declspec(dllexport)
#elif defined(__GNUC__)
#define HEMLOCKBCM_EXPORT __attribute__((visibility("default")))
#else
#define HEMLOCKBCM_EXPORT
#endif

/*
 * Status codes. Zero is success; negative values mirror the SAI status
 * codes Hemlock already classifies, so a shim failure and a vendor-SAI
 * failure reach the operator as the same kind of error.
 */
#define HEMLOCKBCM_OK               0
#define HEMLOCKBCM_ERR_FAILURE     (-1)
#define HEMLOCKBCM_ERR_NOT_SUPPORTED (-2)
#define HEMLOCKBCM_ERR_NO_MEMORY   (-3)
#define HEMLOCKBCM_ERR_INVALID_PARAM (-5)
#define HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS (-6)
#define HEMLOCKBCM_ERR_ITEM_NOT_FOUND (-7)
#define HEMLOCKBCM_ERR_NOT_IMPLEMENTED (-15)

/*
 * Port forwarding states within a spanning-tree group. Deliberately the
 * three Hemlock actually drives: 802.1D's listen and disable never reach
 * the datapath from above.
 */
#define HEMLOCKBCM_STP_BLOCKING   0
#define HEMLOCKBCM_STP_LEARNING   1
#define HEMLOCKBCM_STP_FORWARDING 2

/* Which fields narrow a flush_fdb call; see that slot. */
#define HEMLOCKBCM_FLUSH_VLAN 0x1u
#define HEMLOCKBCM_FLUSH_PORT 0x2u

/* Opaque per-switch handle, created by create_switch. */
struct hemlockbcm_switch;

/* Longest SDK port name the shim will report, including the NUL. */
#define HEMLOCKBCM_PORT_NAME_MAX 16

/* What create_switch needs. Mirrors hemlock_sai::SwitchInit. */
struct hemlockbcm_init {
    /* Absolute path to the board's config.bcm (the SDK reads it from the
     * process working directory otherwise, which Hemlock does not rely
     * on). NUL-terminated. */
    const char *config_bcm_path;
    /* Switch source MAC, resolved by syncd from the ONIE syseeprom or
     * the management netdev. All-zero means "the shim picks". */
    uint8_t src_mac[6];
    /* Non-zero enables the SDK's diagnostic shell on the process's
     * stdin/stdout. Bench bring-up only. */
    int diag_shell;
};

/*
 * One front-panel port as the SDK sees it.
 *
 * `logical_port` is the SDK's own logical port number and is the join key
 * Hemlock's manifest `lanes` list carries. `name` is the SDK's name for
 * the same port ("ge25", "xe0"); syncd asserts it against the manifest's
 * `sdk_names`, so a mistranscribed port map fails at startup instead of
 * quietly mis-cabling a rack. Report both — they are two independent
 * facts and the check is only worth anything while they stay that way.
 */
struct hemlockbcm_port {
    uint32_t logical_port;
    char name[HEMLOCKBCM_PORT_NAME_MAX];
    uint32_t speed_mbps;
    int admin_up;
    int oper_up;
};

/* Cumulative port counters. Mirrors hemlock_sai::PortCounters; a counter
 * the chip does not keep is reported as 0. */
struct hemlockbcm_port_counters {
    uint64_t in_octets, in_ucast_pkts, in_mcast_pkts, in_bcast_pkts;
    uint64_t in_discards, in_errors, in_crc_errors, in_alignment_errors;
    uint64_t in_symbol_errors, in_runts, in_giants, in_pause;
    uint64_t out_octets, out_ucast_pkts, out_mcast_pkts, out_bcast_pkts;
    uint64_t out_discards, out_errors, out_pause;
    uint64_t collisions, late_collisions, deferred;
    /* RMON frame-size bins:
     * 64 / 65-127 / 128-255 / 256-511 / 512-1023 / 1024-1522 / 1523-max */
    uint64_t rx_bins[7];
    uint64_t tx_bins[7];
};

/* What this platform's datapath actually supports. Mirrors the fields of
 * hemlock_sai::SaiCapabilities that a non-SAI backend can answer; the
 * rest Rust derives from which vtable slots are non-NULL. */
struct hemlockbcm_capabilities {
    uint64_t buffer_bytes_total;   /* shared packet buffer, 0 = unknown */
    uint32_t ecmp_width;           /* widest next-hop group, 0 = none */
    uint32_t mirror_sessions_max;  /* 0 = no mirroring */
    int ipv6;
};

/* Port oper-status change, delivered from the shim's own thread. */
typedef void (*hemlockbcm_link_cb)(void *context, uint32_t logical_port, int up);

/*
 * The vtable. Append-only: new slots go at the end and bump the minor.
 *
 * Every function returns a HEMLOCKBCM_* status unless noted. Out
 * parameters are written only on HEMLOCKBCM_OK.
 */
struct hemlockbcm_api {
    /* sizeof(struct hemlockbcm_api) as the shim was compiled. Rust uses
     * it to know which trailing slots exist. MUST be first. */
    size_t struct_size;
    uint32_t abi_major;
    uint32_t abi_minor;

    /* --- lifecycle ------------------------------------------------- */

    /* Attach the device, run the SDK's init, and return a handle.
     * Called exactly once. */
    int (*create_switch)(struct hemlockbcm_switch **out,
                         const struct hemlockbcm_init *init);
    /* Detach and free. The handle is invalid afterwards. */
    int (*destroy_switch)(struct hemlockbcm_switch *sw);
    /* Register the oper-status callback. `context` is passed back
     * verbatim. Passing NULL unregisters. */
    int (*set_link_callback)(struct hemlockbcm_switch *sw,
                             hemlockbcm_link_cb cb, void *context);

    /* --- ports ----------------------------------------------------- */

    /* Enumerate front-panel ports. On entry *count is the capacity of
     * `ports`; on return it is the number written. If the capacity is
     * too small, write nothing, set *count to the number required and
     * return HEMLOCKBCM_ERR_NO_MEMORY. Calling with ports=NULL and
     * *count=0 is the way to ask for the count. */
    int (*ports)(struct hemlockbcm_switch *sw,
                 struct hemlockbcm_port *ports, size_t *count);
    int (*set_port_admin_state)(struct hemlockbcm_switch *sw,
                                uint32_t logical_port, int up);
    /* Line rate in Mb/s. Only meaningful with autoneg off; the caller
     * orders autoneg, then speed, then duplex. */
    int (*set_port_speed)(struct hemlockbcm_switch *sw,
                          uint32_t logical_port, uint32_t speed_mbps);
    /* Forced duplex; 0 = half. */
    int (*set_port_duplex)(struct hemlockbcm_switch *sw,
                           uint32_t logical_port, int full);
    int (*set_port_autoneg)(struct hemlockbcm_switch *sw,
                            uint32_t logical_port, int on);
    /* L2 MTU in frame bytes, excluding the FCS. */
    int (*set_port_mtu)(struct hemlockbcm_switch *sw,
                        uint32_t logical_port, uint32_t mtu);
    int (*port_counters)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                         struct hemlockbcm_port_counters *out);

    /* --- capabilities ---------------------------------------------- */

    int (*capabilities)(struct hemlockbcm_switch *sw,
                        struct hemlockbcm_capabilities *out);

    /* --- board bring-up (ABI 1.1) ----------------------------------- */

    /*
     * Load the chip's LED-processor program and start it.
     *
     * `hex` is the program as an ASCII hex string, exactly the argument
     * the SDK's `led prog` diag command takes. The shim loads it, enables
     * linkscan-driven auto updates and starts the M0.
     *
     * Cosmetic: without it the LED latches power up driving every port
     * LED solid on, which is ugly but harmless, so the caller logs a
     * failure and carries on. Appended in ABI 1.1 — a shim built against
     * 1.0 simply reports the smaller `struct_size` and the caller skips
     * this slot.
     */
    int (*load_led_program)(struct hemlockbcm_switch *sw, const char *hex);

    /* --- L2 VLANs (ABI 1.2) ----------------------------------------- */

    /*
     * These slots are deliberately primitive: a VLAN is its 802.1Q id and
     * a membership is (vlan, port). SAI hands out opaque object ids for
     * both, and the caller packs and unpacks those ids on the Rust side
     * rather than making the shim keep a table it would have to keep
     * consistent across a warm restart. The shim stays stateless.
     *
     * VLAN ids are 1..=4094. A port is a logical port number, the same
     * one `ports` reports.
     */

    /* The chip's default VLAN, which always exists and cannot be
     * created or destroyed. The caller needs it to move a port in and
     * out of default bridging. */
    int (*default_vlan)(struct hemlockbcm_switch *sw, uint16_t *out);

    int (*create_vlan)(struct hemlockbcm_switch *sw, uint16_t vlan_id);
    /* Members must already be gone; the SDK enforces it. */
    int (*remove_vlan)(struct hemlockbcm_switch *sw, uint16_t vlan_id);

    /* Add `logical_port` to `vlan_id`. `tagged` = 0 makes it an untagged
     * (access) member, which is a separate egress bitmap on this
     * hardware rather than a property of the membership.
     *
     * A shim MAY return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS for a port
     * that is already a member, and HEMLOCKBCM_ERR_ITEM_NOT_FOUND for
     * removing one that is not, or it MAY report success for both --
     * the underlying SDK sets and clears bits in a bitmap and does not
     * document which it does. The caller's idempotent operations accept
     * either, so no shim has to find out. What a shim must NOT do is
     * report some third failure for those cases. */
    int (*add_vlan_member)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                           uint32_t logical_port, int tagged);
    int (*remove_vlan_member)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                              uint32_t logical_port);

    /* Ingress classification of untagged frames (PVID). Independent of
     * membership: setting it does not join the VLAN. */
    int (*set_port_pvid)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                         uint16_t vlan_id);

    /* The port's outer TPID; 0x8100 is the default and 0x88a8 makes it a
     * provider-bridge (dot1q-tunnel) port. */
    int (*set_port_tpid)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                         uint16_t tpid);

    /* --- MAC address table (ABI 1.3) --------------------------------- */

    /*
     * Aging time for dynamic entries, in seconds. 0 disables aging.
     */
    int (*set_fdb_aging)(struct hemlockbcm_switch *sw, uint32_t secs);

    /*
     * Install a static entry for (`vlan_id`, `mac`), replacing any
     * existing entry for that pair. `discard` non-zero installs a black
     * hole -- frames to or from that MAC are dropped -- and
     * `logical_port` is then ignored. Otherwise the entry forwards to
     * `logical_port`.
     */
    int (*add_fdb_entry)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                         const uint8_t mac[6], uint32_t logical_port, int discard);
    int (*remove_fdb_entry)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                            const uint8_t mac[6]);

    /*
     * Flush *dynamic* entries; static ones survive. `flags` says which of
     * `vlan_id` and `logical_port` narrow the flush -- neither is
     * optional-by-sentinel, because logical port 0 is a real port and
     * VLAN 0 is not obviously invalid either.
     */
    int (*flush_fdb)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                     uint32_t logical_port, uint32_t flags);

    /* Hardware source-MAC learning on a port. */
    int (*set_port_learning)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                             int learn);

    /*
     * Cap dynamic learning on a port; `limit` < 0 removes the cap. At the
     * limit the chip stops learning new source MACs on that port, which
     * is the enforcement the caller asked for.
     *
     * The shim should prefer an over-limit action that punts the
     * offending frame to the CPU rather than dropping it outright, so
     * that a later ABI minor can add the FDB-notification slot and turn
     * those punts into port-security violation events. Until that slot
     * exists there is no notification path at all: enforcement works,
     * `SaiEvent::LearnLimitViolation` and `SaiEvent::Fdb` never fire on
     * this backend, and nothing above should be told otherwise.
     */
    int (*set_port_learn_limit)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                int limit);

    /* --- Link aggregation (ABI 1.4) ---------------------------------- */

    /*
     * A LAG is a hardware trunk, identified by the SDK's trunk id. As
     * with VLANs, the caller derives its object ids from the trunk id
     * and the member port, so nothing is remembered on this side.
     *
     * Members stay in the trunk whether or not they are forwarding: the
     * collect/distribute gate is a per-member attribute, not membership.
     * That matters because a gated-closed member must still pick up the
     * LAG's VLAN configuration, and it can only do so if the shim can
     * still see it.
     */

    int (*lag_create)(struct hemlockbcm_switch *sw, uint32_t *tid);
    /* Members must already be gone; the SDK enforces it. */
    int (*lag_destroy)(struct hemlockbcm_switch *sw, uint32_t tid);

    /* Add `logical_port` to the trunk. `enabled` = 0 adds it gated
     * closed: in the trunk, carrying its configuration, forwarding
     * nothing in either direction. */
    int (*lag_member_add)(struct hemlockbcm_switch *sw, uint32_t tid,
                          uint32_t logical_port, int enabled);
    int (*lag_member_remove)(struct hemlockbcm_switch *sw, uint32_t tid,
                             uint32_t logical_port);
    /* The collect/distribute gate on an existing member. */
    int (*lag_member_state)(struct hemlockbcm_switch *sw, uint32_t tid,
                            uint32_t logical_port, int enabled);

    /*
     * VLAN membership for a trunk rather than a port. Separate slots
     * rather than a flag on the port ones, because the hardware reaches
     * a trunk through a different call entirely and because widening an
     * existing slot's meaning would be a major-version change.
     */
    int (*lag_vlan_member_add)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                               uint32_t tid, int tagged);
    int (*lag_vlan_member_remove)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                  uint32_t tid);

    /*
     * PVID for a trunk. Ingress classification is a property of the
     * receiving port, so unlike membership this has no trunk-wide form
     * in the hardware: the shim applies it to every member, including
     * gated-closed ones, and to members added later via `lag_member_add`.
     */
    int (*lag_set_pvid)(struct hemlockbcm_switch *sw, uint32_t tid, uint16_t vlan_id);

    /* --- Spanning tree (ABI 1.5) ------------------------------------- */

    /*
     * An STP instance is an SDK spanning-tree group (STG). As with VLANs
     * and trunks, the caller derives its object id from the group id.
     *
     * A VLAN belongs to exactly one group, so assigning it to a new one
     * is a move, not an addition.
     */

    /* The group every VLAN starts in. Always exists; cannot be created
     * or destroyed. */
    int (*stp_default)(struct hemlockbcm_switch *sw, uint32_t *stg);
    int (*stp_create)(struct hemlockbcm_switch *sw, uint32_t *stg);
    /* Its VLANs must have moved elsewhere first. */
    int (*stp_destroy)(struct hemlockbcm_switch *sw, uint32_t stg);

    /* Move `vlan_id` into `stg`, out of whichever group holds it now. */
    int (*stp_vlan_set)(struct hemlockbcm_switch *sw, uint32_t stg, uint16_t vlan_id);

    /*
     * A port's forwarding state within one group. `state` is one of the
     * HEMLOCKBCM_STP_* values below, which are a smaller set than the
     * SDK's: Hemlock never drives listen or disable, so a shim that maps
     * them onto its hardware's nearest equivalent is not making a
     * decision anyone above depends on.
     */
    int (*stp_port_state)(struct hemlockbcm_switch *sw, uint32_t stg,
                          uint32_t logical_port, int state);
    /*
     * The same for a trunk. Like PVID this has no trunk-wide form in the
     * hardware -- forwarding state is per port -- so the shim applies it
     * to every member, gated-closed ones included.
     *
     * Unlike PVID it is NOT inherited by a member that joins later, and
     * the caller must re-apply it after a membership change. The reason
     * is structural rather than laziness: a port has a forwarding state
     * in every group at once, so there is no single value for a joining
     * member to take, whereas a port has exactly one PVID.
     */
    int (*lag_stp_port_state)(struct hemlockbcm_switch *sw, uint32_t stg,
                              uint32_t tid, int state);

    /*
     * Everything below is the rest of phase 6: mirroring, storm control,
     * ACLs/policers/CoPP, sFlow and QoS. Each lands as one slot appended
     * here plus the matching Rust method, with the minor bumped. Until
     * then Rust reports those families unsupported, which is the truth
     * and which both consoles already handle.
     */
};

/*
 * The only exported symbol.
 *
 * Returns NULL if the shim cannot satisfy `want_major` — the shim, not
 * Rust, decides whether it can, so a future shim may serve more than one
 * major from one binary.
 */
HEMLOCKBCM_EXPORT const struct hemlockbcm_api *hemlockbcm_get_api(uint32_t want_major);

#ifdef __cplusplus
}
#endif

#endif /* HEMLOCKBCM_H */
