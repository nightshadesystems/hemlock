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
#define HEMLOCKBCM_ABI_MINOR 16

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

/* Traffic classes a storm-control policer can meter. */
#define HEMLOCKBCM_STORM_BROADCAST       0
#define HEMLOCKBCM_STORM_MULTICAST       1
#define HEMLOCKBCM_STORM_UNKNOWN_UNICAST 2

/* Which fields narrow a flush_fdb call; see that slot. */
#define HEMLOCKBCM_FLUSH_VLAN 0x1u
#define HEMLOCKBCM_FLUSH_PORT 0x2u

/* Which fields of a hemlockbcm_acl_fields are set. */
#define HEMLOCKBCM_ACL_F_SRC_IP    0x0001u
#define HEMLOCKBCM_ACL_F_DST_IP    0x0002u
#define HEMLOCKBCM_ACL_F_PROTOCOL  0x0004u
#define HEMLOCKBCM_ACL_F_SRC_PORT  0x0008u
#define HEMLOCKBCM_ACL_F_DST_PORT  0x0010u
#define HEMLOCKBCM_ACL_F_DSCP      0x0020u
#define HEMLOCKBCM_ACL_F_SRC_MAC   0x0040u
#define HEMLOCKBCM_ACL_F_DST_MAC   0x0080u
#define HEMLOCKBCM_ACL_F_ETHERTYPE 0x0100u
#define HEMLOCKBCM_ACL_F_VLAN      0x0200u

/* What an ACL entry does with a matching packet. */
#define HEMLOCKBCM_ACL_FORWARD 0
#define HEMLOCKBCM_ACL_DROP    1
/* CPU only: punted and dropped in the forwarding plane. */
#define HEMLOCKBCM_ACL_TRAP    2
/* Forwarded and copied to the CPU. */
#define HEMLOCKBCM_ACL_COPY    3

/*
 * One ACL entry's match. Every field is IPv4-shaped: the caller refuses
 * IPv6 tables on a shim that reports no IPv6, so nothing here needs a
 * v6 form.
 *
 * L4 port matches are exact, not ranges. The `_port` fields are a single
 * value each, because expressing a range needs the chip's range checkers
 * (`bcm_field_range_create`) -- a small, separately allocated resource
 * with its own lifecycle -- and the caller rejects a non-degenerate
 * range rather than the shim quietly matching only its lower bound.
 */
struct hemlockbcm_acl_fields {
    uint32_t present;       /* HEMLOCKBCM_ACL_F_* */
    uint32_t src_ip;        /* host byte order */
    uint32_t src_ip_mask;
    uint32_t dst_ip;
    uint32_t dst_ip_mask;
    uint8_t protocol;
    uint16_t src_port;
    uint16_t dst_port;
    uint8_t dscp;
    uint8_t src_mac[6];
    uint8_t src_mac_mask[6];
    uint8_t dst_mac[6];
    uint8_t dst_mac_mask[6];
    uint16_t ethertype;
    uint16_t vlan;
};

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

    /* --- Port mirroring (ABI 1.6) ------------------------------------- */

    /*
     * Local (SPAN) mirroring. A session is an SDK mirror destination and
     * its id is the destination's own, so as with everything else here
     * the caller derives its object id and the shim keeps no table.
     *
     * `monitor` and the mirrored ports are logical ports. Trunks are not
     * accepted: the caller rejects a LAG id before it reaches these
     * slots rather than letting one arrive looking like a port number.
     */

    int (*mirror_create)(struct hemlockbcm_switch *sw, uint32_t monitor_port,
                         uint32_t *session);
    /* Ports must be detached first. */
    int (*mirror_destroy)(struct hemlockbcm_switch *sw, uint32_t session);

    /*
     * Point one direction of `logical_port` at `session`, replacing
     * whatever that direction pointed at before. `egress` selects the
     * direction; the two are independent.
     */
    int (*mirror_port_attach)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              uint32_t session, int egress);
    /* Stop mirroring that direction, whatever it was pointed at. */
    int (*mirror_port_detach)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              int egress);

    /* --- Storm control (ABI 1.7) -------------------------------------- */

    /*
     * Meter one traffic class on a port. `kbps` of 0 removes the limit,
     * which is the SDK's own encoding of "no rate" rather than a
     * sentinel invented here.
     *
     * There is deliberately no companion slot for per-class drop
     * counts. The chip has one storm-control drop trigger per port
     * (`bcmDbgCntRxStormControlDrop`), not one per class, so a
     * `port_storm_drops(port, class)` answer built on it would be the
     * same port-wide number reported three times as if it were three
     * facts. The caller reports the family as unsupported for reads,
     * which is true, instead.
     */
    int (*storm_control_set)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                             int storm_class, uint32_t kbps);

    /* --- Host interfaces (ABI 1.8) ------------------------------------ */

    /*
     * The CPU punt path. SAI models this as one wildcard hostif table
     * entry that delivers every trapped packet on its ingress port's
     * netdev, plus a NETDEV hostif object per port. KNET has no such
     * wildcard: delivery is decided by per-filter matches, so "the
     * ingress port's netdev" is expressed as one ingress-port filter per
     * netdev, installed by `hostif_create`.
     *
     * `host_punt_setup` therefore initialises the KNET subsystem and
     * nothing else. It is not a no-op dressed up as work: the init is
     * what clears netifs and filters left behind by a previous run, so
     * calling it before any `hostif_create` is what makes a syncd
     * restart idempotent.
     */
    int (*host_punt_setup)(struct hemlockbcm_switch *sw);

    /*
     * A netdev for `logical_port`, plus the filter that delivers that
     * port's punted traffic to it. `name` is NUL-terminated and at most
     * 15 characters, which is both what the kernel accepts and what SAI
     * documents. Returns an opaque handle.
     *
     * There is no destroy: the caller creates these once per port at
     * start-up and never removes one, and `host_punt_setup` clears the
     * previous run's.
     */
    int (*hostif_create)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                         const char *name, uint32_t *hostif);

    /* --- Policers (ABI 1.9) ------------------------------------------- */

    /*
     * A single-rate policer: one rate, one burst, everything over it
     * dropped. `pps` selects packets or bits per second; `rate` and
     * `burst` are then packets/packets or bits/bytes respectively,
     * matching what the caller was given rather than what the SDK's
     * fields are named.
     *
     * Conversion to the SDK's kilo-denominated fields happens in the
     * shim, which is where the exactness matters: it splits each value
     * into thousands plus a remainder so nothing is rounded away.
     */
    int (*policer_create)(struct hemlockbcm_switch *sw, int pps, uint64_t rate,
                          uint64_t burst, uint32_t *policer);
    int (*policer_set)(struct hemlockbcm_switch *sw, uint32_t policer, int pps,
                       uint64_t rate, uint64_t burst);
    int (*policer_destroy)(struct hemlockbcm_switch *sw, uint32_t policer);

    /*
     * Conforming and dropped packet counts. Both are read from the
     * chip's colour-transition counters, and the mapping from those to
     * "conforming" and "dropped" is the shim's -- see the comment there,
     * because it is the one part of this family that the header cannot
     * make self-evident.
     */
    int (*policer_stats)(struct hemlockbcm_switch *sw, uint32_t policer,
                         uint64_t *conforming, uint64_t *dropped);

    /* --- ACLs (ABI 1.10) ---------------------------------------------- */

    /*
     * A table is a field group; an entry is a field entry in it. Both
     * ids are the SDK's own, so as everywhere else the caller derives
     * its object ids and the shim keeps no table.
     *
     * `egress` selects the pipeline stage. There is no family argument:
     * the caller refuses IPv6 tables while this shim reports no IPv6,
     * and a MAC-only table is an IPv4 group whose entries happen to
     * qualify on L2 fields, which costs nothing on this hardware.
     */
    int (*acl_table_create)(struct hemlockbcm_switch *sw, int egress, uint32_t *table);
    /* Entries must be gone and no port may still bind it. */
    int (*acl_table_destroy)(struct hemlockbcm_switch *sw, uint32_t table);

    /* Bind or unbind one port. Binding is a property of the group, not
     * of its entries, so this does not touch them. */
    int (*acl_table_bind)(struct hemlockbcm_switch *sw, uint32_t table,
                          uint32_t logical_port, int bind);
    /*
     * Unbind `logical_port` from every table at `egress`'s stage. The
     * caller reaches this when told to unbind without being told from
     * what, which it cannot answer itself without keeping the binding
     * table the shim deliberately does not keep either.
     */
    int (*acl_table_unbind_all)(struct hemlockbcm_switch *sw, int egress,
                                uint32_t logical_port);

    /* Higher `priority` wins, matching the caller's ordering. */
    int (*acl_entry_create)(struct hemlockbcm_switch *sw, uint32_t table,
                            uint32_t priority,
                            const struct hemlockbcm_acl_fields *fields,
                            int action, uint32_t *entry);
    /* Replace the entry's action, leaving its match alone. */
    int (*acl_entry_action_set)(struct hemlockbcm_switch *sw, uint32_t entry, int action);
    int (*acl_entry_destroy)(struct hemlockbcm_switch *sw, uint32_t entry);

    /* Free TCAM entries at a stage, for utilisation reporting. */
    int (*acl_available)(struct hemlockbcm_switch *sw, int egress, uint32_t *entries);

    /* --- ACL counters and per-entry policers (ABI 1.11) --------------- */

    /*
     * A counter belongs to the table it counts in: the chip allocates
     * counters per group, so one cannot be moved between tables and the
     * table has to be named at create time.
     *
     * Counting is packets, not bytes. The caller's `show acl` reports
     * matches, and asking the chip for both would spend two counter
     * slots to answer one question.
     */
    int (*acl_counter_create)(struct hemlockbcm_switch *sw, uint32_t table,
                              uint32_t *counter);
    /* No entry may still reference it. */
    int (*acl_counter_destroy)(struct hemlockbcm_switch *sw, uint32_t counter);
    int (*acl_counter_get)(struct hemlockbcm_switch *sw, uint32_t counter,
                           uint64_t *packets);

    /*
     * Attach or replace an entry's counter and policer. Passing 0 for
     * either detaches it, which is safe because neither a counter id nor
     * a policer id of 0 is one this ABI ever hands out.
     *
     * Both together, in one call, because an entry's action set is
     * replaced as a unit: `acl_entry_action_set` clears the actions and
     * rebuilds them, and leaving the attachments to a separate call
     * would make the order between them significant.
     */
    int (*acl_entry_attach)(struct hemlockbcm_switch *sw, uint32_t entry,
                            uint32_t counter, uint32_t policer);

    /* --- Router interfaces (ABI 1.12) --------------------------------- */

    /*
     * An L3 interface on this hardware is per *VLAN*, not per port:
     * `bcm_l3_intf_t` carries a VLAN id and no port. A SAI port router
     * interface therefore has no direct counterpart, and is built as a
     * VLAN of the port's own plus an L3 interface on it.
     *
     * Which VLAN is the caller's choice, not the shim's, so that the
     * shim keeps no allocator and a restart cannot forget what it
     * handed out. The caller passes the VLAN it has reserved; the shim
     * creates it, moves the port into it untagged, sets the PVID, and
     * puts the interface on it. `rif_port_destroy` undoes all four in
     * reverse, leaving the port bridging again.
     */
    int (*rif_port_create)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                           uint16_t vlan_id, const uint8_t mac[6], uint32_t *rif);
    int (*rif_port_destroy)(struct hemlockbcm_switch *sw, uint32_t logical_port,
                            uint16_t vlan_id, uint32_t rif);

    /*
     * An SVI: an L3 interface on a VLAN that keeps bridging. The VLAN
     * already exists and is left alone on destroy -- only the interface
     * goes.
     */
    int (*rif_vlan_create)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                           const uint8_t mac[6], uint32_t *rif);
    int (*rif_vlan_destroy)(struct hemlockbcm_switch *sw, uint32_t rif);

    /*
     * A My-MAC entry: frames whose destination MAC matches enter L3
     * instead of being bridged. `vlan_id` of 0 matches any VLAN.
     *
     * Creating a router interface does not imply one of these. The two
     * are separate on this hardware -- the interface says what L3 looks
     * like on a VLAN, the station entry says which frames get there --
     * and SAI's VRRP virtual MACs need station entries with no
     * interface of their own.
     */
    int (*my_mac_create)(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                         const uint8_t mac[6], uint32_t *my_mac);
    int (*my_mac_destroy)(struct hemlockbcm_switch *sw, uint32_t my_mac);

    /* --- Routes (ABI 1.13) -------------------------------------------- */

    /*
     * A route on the default virtual router. `kind` says what the
     * destination resolves to and which of the remaining arguments
     * carries it.
     *
     * Only the targets that need no next-hop object are here. A route to
     * a next hop or an ECMP group needs an egress object, and building
     * one needs the neighbour's MAC and egress port -- which the caller
     * may not have yet when the route is programmed. That sequencing is
     * a design in its own right and is not folded in here silently: the
     * caller refuses those two targets until the slot for them exists.
     */
#define HEMLOCKBCM_ROUTE_CPU  0   /* punt: one of the switch's own addresses */
#define HEMLOCKBCM_ROUTE_RIF  1   /* a connected subnet, via `rif` */
#define HEMLOCKBCM_ROUTE_DROP 2   /* a null route, dropped in hardware */

    int (*route_set)(struct hemlockbcm_switch *sw, uint32_t prefix, uint32_t mask,
                     int kind, uint32_t rif);
    int (*route_delete)(struct hemlockbcm_switch *sw, uint32_t prefix, uint32_t mask);

    /* --- Neighbours and next hops (ABI 1.14) -------------------------- */

    /*
     * Resolving a neighbour builds the egress object that a next hop
     * forwards through, and files it in the chip's host table under the
     * neighbour's IP. That table is the resolution table: neither side
     * keeps one, and `route_via_nexthop` finds the egress object by
     * looking the next hop's IP up in it.
     *
     * An egress object needs a destination port, which is not in
     * anything the caller passes: it comes from the FDB, by looking up
     * the neighbour's MAC in the interface's VLAN. Until the MAC is
     * learned there is no port, and this returns "not found" rather than
     * guessing one. That is not a failure state -- it is the
     * resolve-via-punt case the caller already models by pointing the
     * route at the CPU until the neighbour answers.
     */
    int (*neighbor_set)(struct hemlockbcm_switch *sw, uint32_t rif, uint32_t ip,
                        const uint8_t mac[6]);
    int (*neighbor_clear)(struct hemlockbcm_switch *sw, uint32_t rif, uint32_t ip);

    /*
     * A route through a resolved next hop. `nexthop_ip` names the
     * neighbour; the shim finds its egress object in the host table.
     * "Not found" means the neighbour has not resolved yet.
     *
     * There is no next-hop *object* slot: a next hop is (interface, ip),
     * which is a name rather than something the chip allocates, so the
     * caller mints its own id and nothing needs creating or destroying.
     */
    int (*route_via_nexthop)(struct hemlockbcm_switch *sw, uint32_t prefix,
                             uint32_t mask, uint32_t nexthop_ip);

    /* --- ECMP groups (ABI 1.15) --------------------------------------- */

    /*
     * A group is a multipath egress object, and its members are the
     * egress objects the neighbours own -- so a member is added by
     * naming the next hop's address and letting the shim find its
     * object in the host table, exactly as a single-path route does. An
     * unresolved neighbour has no object, and adding it returns "not
     * found" rather than a group with a hole in it.
     *
     * The group is created empty with its width reserved up front:
     * widening one later would move it, and every route pointing at it
     * would have to be rewritten.
     */
    int (*ecmp_create)(struct hemlockbcm_switch *sw, uint32_t *group);
    /* Members must be gone, and no route may still point at it. */
    int (*ecmp_destroy)(struct hemlockbcm_switch *sw, uint32_t group);
    int (*ecmp_member_add)(struct hemlockbcm_switch *sw, uint32_t group,
                           uint32_t nexthop_ip);
    int (*ecmp_member_remove)(struct hemlockbcm_switch *sw, uint32_t group,
                              uint32_t nexthop_ip);
    int (*route_via_ecmp)(struct hemlockbcm_switch *sw, uint32_t prefix, uint32_t mask,
                          uint32_t group);

    /* --- CoPP traps (ABI 1.16) ---------------------------------------- */

    /*
     * A protocol trap is a field entry with copy-to-CPU (plus drop, for
     * a punt rather than a copy) and a policer attached. The shim owns
     * each kind's match, because several need qualifiers the generic
     * ACL fields cannot carry (the ARP opcode, "destination IP is
     * local").
     *
     * Everything is keyed by (kind, is_default): the shim derives its
     * field group and entry ids from them, so the chip's own entry
     * table is the state and a trap_set for an existing pair replaces
     * it. `is_default` marks traps in the switch's default trap group,
     * whose policer can be swept later by trap_default_policer_set;
     * named-group traps carry their group's policer at create time.
     *
     * Two kinds are deliberately less precise than their names, because
     * this chip's parser cannot do better, and pretending otherwise
     * would be worse:
     *   - The IGMP kinds all install the same match (IP protocol 2).
     *     The message-type byte is only reachable through a qualifier
     *     the SDK marks internal-only. They share a policer in every
     *     real class table, so the imprecision costs nothing there; put
     *     them in *different* groups and the highest kind wins.
     *   - The MLD kinds install "IPv6 multicast" (EtherType 0x86dd,
     *     MAC 33:33::/16) -- a superset that genuinely contains MLD on
     *     a box with no IPv6 datapath to classify deeper.
     */
#define HEMLOCKBCM_TRAP_IP2ME       0   /* lowest priority: protocol traps
                                         * win overlaps like DHCP-to-me */
#define HEMLOCKBCM_TRAP_STP         1
#define HEMLOCKBCM_TRAP_LACP        2
#define HEMLOCKBCM_TRAP_LLDP        3
#define HEMLOCKBCM_TRAP_EAPOL       4
#define HEMLOCKBCM_TRAP_IGMP_QUERY  5
#define HEMLOCKBCM_TRAP_IGMP_LEAVE  6
#define HEMLOCKBCM_TRAP_IGMP_V1_REPORT 7
#define HEMLOCKBCM_TRAP_IGMP_V2_REPORT 8
#define HEMLOCKBCM_TRAP_IGMP_V3_REPORT 9
#define HEMLOCKBCM_TRAP_MLD_V1_V2   10
#define HEMLOCKBCM_TRAP_MLD_V1_REPORT 11
#define HEMLOCKBCM_TRAP_MLD_V1_DONE 12
#define HEMLOCKBCM_TRAP_MLD_V2_REPORT 13
#define HEMLOCKBCM_TRAP_ARP_REQUEST 14
#define HEMLOCKBCM_TRAP_ARP_RESPONSE 15
#define HEMLOCKBCM_TRAP_DHCP        16
#define HEMLOCKBCM_TRAP_OSPF        17
#define HEMLOCKBCM_TRAP_BGP         18
#define HEMLOCKBCM_TRAP_VRRP        19
#define HEMLOCKBCM_TRAP_KIND_COUNT  20

    /* Install (or replace) the trap for (kind, is_default). `trap_only`
     * punts -- the forwarding copy is dropped; otherwise the packet
     * forwards and the CPU gets a copy. `policer` of 0 = unpoliced. */
    int (*trap_set)(struct hemlockbcm_switch *sw, int kind, int trap_only,
                    int is_default, uint32_t policer);
    int (*trap_clear)(struct hemlockbcm_switch *sw, int kind, int is_default);

    /* Re-policer every installed default-group trap. The sweep walks
     * the derived entry ids and probes the chip, so nothing has to
     * remember which kinds are installed. */
    int (*trap_default_policer_set)(struct hemlockbcm_switch *sw, uint32_t policer);

    /*
     * Everything below is the rest of phase 6: sFlow and QoS. Each lands as one slot appended here plus the
     * matching Rust method, with the minor bumped. Until then Rust
     * reports those families unsupported, which is the truth and which
     * both consoles already handle.
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
