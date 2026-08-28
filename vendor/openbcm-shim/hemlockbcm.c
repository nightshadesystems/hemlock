/*
 * hemlockbcm.c — Hemlock's OpenBCM shim.
 *
 * SPDX-License-Identifier: MIT
 * Copyright (c) Nightshade Systems.
 *
 * Implements the ABI in src/hemlock-sai/openbcm-shim/hemlockbcm.h over
 * Broadcom's OpenBCM SDK, so `OpenBcmBackend` can drive a Helix4 on a
 * board where no SAI exists for the CPU architecture. Built *inside* an
 * OpenBCM tree by build-shim.sh; see docs/as4610-54-port.md.
 *
 * This is Hemlock's own code written against Broadcom's published `bcm_*`
 * API. Nothing here is copied from edgenos, ONL or the SDK's own
 * applications: those were read for *facts about this board* — that the
 * 54282 PHYs need software linkscan, that `init all` then `init bcm` is
 * the working bring-up order — not for their expression.
 *
 * Licensing: this file is MIT. It compiles against Broadcom's
 * Switch-APIs-licensed headers and links against their libraries on the
 * operator's machine, so the resulting libhemlockbcm.so is a derived
 * binary that ships only inside an image the operator built. Neither the
 * SDK nor the .so is ever committed or redistributed here.
 *
 * ---------------------------------------------------------------------
 * NOT COMPILED BY CI. The SDK is not fetchable in CI and the target is
 * ARM; this file is built only by build-shim.sh in the cross container.
 * It IS compiled locally, though: check-shim.sh (next to this file)
 * runs `gcc -fsyntax-only -Wall -Wextra` against a headers-only sparse
 * fetch of sdk-6.5.16, so every signature, struct field, enum member
 * and macro here has been through a real compiler against the real
 * headers. Run it after touching this file. What it cannot prove is
 * the link (the SDK libraries are absent) and ARM type widths.
 */

#include "hemlockbcm.h"

#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* OpenBCM. sal/types.h must come first: it defines the integer types and
 * the COMPILER_64_* family everything else is written in terms of. */
#include <sal/core/libc.h>
#include <sal/types.h>

#include <bcm/cosq.h>
#include <bcm/error.h>
#include <bcm/field.h>
#include <bcm/init.h>
#include <bcm/knet.h>
#include <bcm/l3.h>
#include <bcm/l2.h>
#include <bcm/pkt.h>
#include <bcm/link.h>
#include <bcm/mirror.h>
#include <bcm/policer.h>
#include <bcm/port.h>
#include <bcm/rx.h>
#include <bcm/rate.h>
#include <bcm/stack.h>
#include <bcm/stat.h>
#include <bcm/stg.h>
#include <bcm/trunk.h>
#include <bcm/types.h>
#include <bcm/vlan.h>

#include <soc/drv.h>

/*
 * The SDK's own bring-up entry points. These live in the diag/sysconf
 * layer rather than in a public bcm/ header, so they are declared here
 * the way the SDK's own applications declare them.
 */
extern int sysconf_init(void);
extern int sysconf_probe(void);
extern int sysconf_attach(int unit);
extern int sh_process_command(int unit, char *cmd);

/* Hemlock drives one ASIC; a second would be a different platform. */
#define HB_UNIT 0

/*
 * The widest ECMP group. The SDK has no call that reports the chip's
 * limit, and the width has to be committed at create time -- a group
 * created narrow cannot be widened in place, so every route pointing at
 * it would have to be rewritten to grow it. Reserving the same width for
 * every group trades table space for never having to do that.
 *
 * Confirm against the hardware: too high fails the create, too low
 * refuses a member the chip would have taken.
 */
#define HB_ECMP_MAX_PATHS 32

struct hemlockbcm_switch {
    int unit;
    /* The switch MAC, kept from create_switch: KNET netdevs need one and
     * there is nowhere else to get it at hostif_create time. */
    uint8_t src_mac[6];
    /* The local module id, which qualifies every L2 destination. Read
     * from the SDK on first use and cached; -1 means "not read yet". */
    int modid;
    hemlockbcm_link_cb link_cb;
    void *link_ctx;
    hemlockbcm_sample_cb sample_cb;
    void *sample_ctx;
};

/* An all-zero MAC, for the match structures where the MAC is not part of
 * the match. bcm_mac_t is an array type, so this cannot be a literal. */
static const bcm_mac_t hb_zero_mac = { 0, 0, 0, 0, 0, 0 };

/*
 * The SDK's linkscan callback carries no context pointer, so the one
 * switch we own is reachable through this file-static. Legitimate here
 * precisely because the shim is single-instance by construction; a
 * multi-unit shim would need a unit-indexed table.
 */
static struct hemlockbcm_switch *hb_switch;

static int hb_status(int rv)
{
    if (rv == BCM_E_NONE) {
        return HEMLOCKBCM_OK;
    }
    switch (rv) {
    case BCM_E_UNAVAIL:
        return HEMLOCKBCM_ERR_NOT_SUPPORTED;
    case BCM_E_MEMORY:
        return HEMLOCKBCM_ERR_NO_MEMORY;
    case BCM_E_PARAM:
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    case BCM_E_PORT:
        /* Distinct from NOT_FOUND on purpose. The caller treats
         * "not found" from a VLAN membership call as "already not a
         * member" and reports success, because several of its operations
         * are idempotent -- so an invalid port must not arrive wearing
         * that code, or a typo becomes a silent no-op. */
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    case BCM_E_NOT_FOUND:
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    case BCM_E_EXISTS:
        return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
    default:
        return HEMLOCKBCM_ERR_FAILURE;
    }
}

/* Log and translate in one step: a bare status code reaching Rust loses
 * which SDK call produced it, and that is what bring-up needs to know. */
#define HB_CALL(expr)                                                      \
    ({                                                                     \
        int _rv = (expr);                                                  \
        if (_rv != BCM_E_NONE) {                                           \
            printf("[hemlockbcm] %s = %d (%s)\n", #expr, _rv,              \
                   bcm_errmsg(_rv));                                       \
        }                                                                  \
        hb_status(_rv);                                                    \
    })

/*
 * uint64 is a struct on toolchains without long long, so the SDK's own
 * accessors are the only portable way to read one. armhf gcc has long
 * long, but going through the macros costs nothing and cannot be wrong.
 */
static uint64_t hb_u64(uint64 value)
{
    uint32 hi = 0, lo = 0;
    COMPILER_64_TO_32_HI(hi, value);
    COMPILER_64_TO_32_LO(lo, value);
    return ((uint64_t)hi << 32) | (uint64_t)lo;
}

/* ------------------------------------------------------------------ */
/* Lifecycle                                                           */
/* ------------------------------------------------------------------ */

static void hb_linkscan_handler(int unit, bcm_port_t port, bcm_port_info_t *info)
{
    struct hemlockbcm_switch *sw = hb_switch;

    if (sw == NULL || unit != sw->unit || info == NULL || sw->link_cb == NULL) {
        return;
    }
    sw->link_cb(sw->link_ctx, (uint32_t)port, info->linkstatus ? 1 : 0);
}

static int hb_create_switch(struct hemlockbcm_switch **out,
                            const struct hemlockbcm_init *init)
{
    struct hemlockbcm_switch *sw;
    bcm_port_config_t config;
    bcm_port_t port;
    int rv;

    if (out == NULL || init == NULL || init->config_bcm_path == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (hb_switch != NULL) {
        return HEMLOCKBCM_ERR_FAILURE;  /* one switch per process */
    }

    /*
     * The SDK reads config.bcm from the process working directory. Rather
     * than depend on how syncd was started, chdir to the file's directory
     * — the platform overlay — before init.
     */
    {
        char dir[512];
        char *slash;

        snprintf(dir, sizeof(dir), "%s", init->config_bcm_path);
        slash = strrchr(dir, '/');
        if (slash != NULL) {
            *slash = '\0';
            if (chdir(dir) != 0) {
                printf("[hemlockbcm] chdir(%s) failed\n", dir);
                return HEMLOCKBCM_ERR_FAILURE;
            }
        }
    }

    if (sysconf_init() < 0 || sysconf_probe() < 0) {
        printf("[hemlockbcm] sysconf init/probe failed\n");
        return HEMLOCKBCM_ERR_FAILURE;
    }
    if (!soc_attached(HB_UNIT) && sysconf_attach(HB_UNIT) < 0) {
        printf("[hemlockbcm] sysconf_attach failed\n");
        return HEMLOCKBCM_ERR_FAILURE;
    }

    /*
     * `init all` then `init bcm`, in that order: the first reads
     * config.bcm and brings up the SOC, the PHY firmware and the QSGMII
     * GE ports; the second initialises the BCM API layer on top. This is
     * the sequence proven on this board.
     */
    if (sh_process_command(HB_UNIT, "init all") != 0) {
        printf("[hemlockbcm] 'init all' failed\n");
        return HEMLOCKBCM_ERR_FAILURE;
    }
    if (sh_process_command(HB_UNIT, "init bcm") != 0) {
        printf("[hemlockbcm] 'init bcm' failed\n");
        return HEMLOCKBCM_ERR_FAILURE;
    }

    sw = (struct hemlockbcm_switch *)calloc(1, sizeof(*sw));
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    sw->unit = HB_UNIT;
    sw->modid = -1;  /* read lazily; calloc would make it a valid modid 0 */
    memcpy(sw->src_mac, init->src_mac, sizeof(sw->src_mac));
    hb_switch = sw;

    /*
     * Software linkscan on every front-panel port. The copper ports sit
     * behind external BCM54282 PHYs, whose link state the MAC does not
     * see on its own: without SW linkscan the chip's idea of link never
     * changes and `show interfaces` lies. 250 ms is the SDK's usual
     * polling interval and is far below any human-visible delay.
     */
    rv = bcm_port_config_get(HB_UNIT, &config);
    if (rv != BCM_E_NONE) {
        printf("[hemlockbcm] bcm_port_config_get = %d (%s)\n", rv, bcm_errmsg(rv));
        free(sw);
        hb_switch = NULL;
        return hb_status(rv);
    }
    BCM_PBMP_ITER(config.e, port) {
        rv = bcm_linkscan_mode_set(HB_UNIT, port, BCM_LINKSCAN_MODE_SW);
        if (rv != BCM_E_NONE) {
            printf("[hemlockbcm] linkscan mode port %d = %d (%s)\n", port, rv,
                   bcm_errmsg(rv));
        }
    }
    rv = bcm_linkscan_register(HB_UNIT, hb_linkscan_handler);
    if (rv != BCM_E_NONE) {
        printf("[hemlockbcm] bcm_linkscan_register = %d (%s)\n", rv, bcm_errmsg(rv));
    }
    rv = bcm_linkscan_enable_set(HB_UNIT, 250000);
    if (rv != BCM_E_NONE) {
        printf("[hemlockbcm] bcm_linkscan_enable_set = %d (%s)\n", rv, bcm_errmsg(rv));
    }

    *out = sw;
    printf("[hemlockbcm] switch attached and initialised\n");
    return HEMLOCKBCM_OK;
}

static int hb_destroy_switch(struct hemlockbcm_switch *sw)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    bcm_linkscan_enable_set(sw->unit, 0);
    bcm_linkscan_unregister(sw->unit, hb_linkscan_handler);
    if (hb_switch == sw) {
        hb_switch = NULL;
    }
    free(sw);
    return HEMLOCKBCM_OK;
}

static int hb_set_link_callback(struct hemlockbcm_switch *sw,
                                hemlockbcm_link_cb cb, void *context)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->link_ctx = context;
    sw->link_cb = cb;  /* last, so the context is in place when it fires */
    return HEMLOCKBCM_OK;
}

/* ------------------------------------------------------------------ */
/* Ports                                                               */
/* ------------------------------------------------------------------ */

static int hb_ports(struct hemlockbcm_switch *sw, struct hemlockbcm_port *ports,
                    size_t *count)
{
    bcm_port_config_t config;
    bcm_port_t port;
    size_t found = 0;
    size_t capacity;
    int rv;

    if (sw == NULL || count == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    capacity = (ports == NULL) ? 0 : *count;

    rv = bcm_port_config_get(sw->unit, &config);
    if (rv != BCM_E_NONE) {
        return hb_status(rv);
    }

    /* config.e is every ethernet port, which is what the manifest's port
     * table selects from; ports it does not claim (the internal ge48)
     * syncd logs and leaves alone. */
    BCM_PBMP_ITER(config.e, port) {
        found++;
    }
    if (capacity < found) {
        *count = found;
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }

    found = 0;
    BCM_PBMP_ITER(config.e, port) {
        struct hemlockbcm_port *out = &ports[found++];
        const char *name;
        int value = 0;

        memset(out, 0, sizeof(*out));
        out->logical_port = (uint32_t)port;

        /* The SDK's own name for the port ("ge25", "xe0"). syncd asserts
         * it against the manifest's sdk_names, which is what catches a
         * mistranscribed faceplate map before it mis-cables a rack. */
        name = SOC_PORT_NAME(sw->unit, port);
        if (name != NULL) {
            snprintf(out->name, HEMLOCKBCM_PORT_NAME_MAX, "%s", name);
        }

        if (bcm_port_speed_get(sw->unit, port, &value) == BCM_E_NONE) {
            out->speed_mbps = (uint32_t)value;
        }
        value = 0;
        if (bcm_port_enable_get(sw->unit, port, &value) == BCM_E_NONE) {
            out->admin_up = value ? 1 : 0;
        }
        value = 0;
        if (bcm_port_link_status_get(sw->unit, port, &value) == BCM_E_NONE) {
            out->oper_up = value ? 1 : 0;
        }
    }
    *count = found;
    return HEMLOCKBCM_OK;
}

static int hb_set_port_admin_state(struct hemlockbcm_switch *sw,
                                   uint32_t logical_port, int up)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_enable_set(sw->unit, (bcm_port_t)logical_port, up ? 1 : 0));
}

static int hb_set_port_speed(struct hemlockbcm_switch *sw, uint32_t logical_port,
                             uint32_t speed_mbps)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* 0 means "stop forcing" in Hemlock's link-parameter model; the SDK
     * spells that the same way. */
    return HB_CALL(bcm_port_speed_set(sw->unit, (bcm_port_t)logical_port,
                                      (int)speed_mbps));
}

static int hb_set_port_duplex(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              int full)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_duplex_set(sw->unit, (bcm_port_t)logical_port,
                                       full ? BCM_PORT_DUPLEX_FULL
                                            : BCM_PORT_DUPLEX_HALF));
}

static int hb_set_port_autoneg(struct hemlockbcm_switch *sw, uint32_t logical_port,
                               int on)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_autoneg_set(sw->unit, (bcm_port_t)logical_port,
                                        on ? 1 : 0));
}

static int hb_set_port_mtu(struct hemlockbcm_switch *sw, uint32_t logical_port,
                           uint32_t mtu)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Hemlock's MTU excludes the FCS; bcm_port_frame_max_set is the whole
     * frame the MAC accepts, so add the 4-byte FCS and the 18 bytes of
     * L2 header the SDK counts in its maximum. */
    return HB_CALL(bcm_port_frame_max_set(sw->unit, (bcm_port_t)logical_port,
                                          (int)mtu + 18));
}

/*
 * Counters. Read in one bcm_stat_multi_get so the whole set is a single
 * pass over the MIB rather than 40-odd round trips per port per sweep —
 * syncd polls every front-panel port every 5 s.
 */
static int hb_port_counters(struct hemlockbcm_switch *sw, uint32_t logical_port,
                            struct hemlockbcm_port_counters *out)
{
    /* Order must match the reads below. */
    static bcm_stat_val_t stats[] = {
        snmpIfInOctets, snmpIfInUcastPkts, snmpIfInMulticastPkts,
        snmpIfInBroadcastPkts, snmpIfInDiscards, snmpIfInErrors,
        snmpDot3StatsFCSErrors, snmpDot3StatsAlignmentErrors,
        snmpDot3StatsSymbolErrors, snmpEtherStatsUndersizePkts,
        snmpEtherStatsOversizePkts, snmpDot3InPauseFrames,
        snmpIfOutOctets, snmpIfOutUcastPkts, snmpIfOutMulticastPkts,
        snmpIfOutBroadcastPkts, snmpIfOutDiscards, snmpIfOutErrors,
        snmpDot3OutPauseFrames, snmpDot3StatsSingleCollisionFrames,
        snmpDot3StatsLateCollisions, snmpDot3StatsDeferredTransmissions,
        /* rx size bins */
        snmpBcmReceivedPkts64Octets, snmpBcmReceivedPkts65to127Octets,
        snmpBcmReceivedPkts128to255Octets, snmpBcmReceivedPkts256to511Octets,
        snmpBcmReceivedPkts512to1023Octets, snmpBcmReceivedPkts1024to1518Octets,
        snmpBcmReceivedPkts1519to2047Octets, snmpBcmReceivedPkts2048to4095Octets,
        snmpBcmReceivedPkts4095to9216Octets,
        /* tx size bins */
        snmpBcmTransmittedPkts64Octets, snmpBcmTransmittedPkts65to127Octets,
        snmpBcmTransmittedPkts128to255Octets, snmpBcmTransmittedPkts256to511Octets,
        snmpBcmTransmittedPkts512to1023Octets, snmpBcmTransmittedPkts1024to1518Octets,
        snmpBcmTransmittedPkts1519to2047Octets, snmpBcmTransmittedPkts2048to4095Octets,
        snmpBcmTransmittedPkts4095to9216Octets,
    };
    const int nstat = (int)(sizeof(stats) / sizeof(stats[0]));
    uint64 values[sizeof(stats) / sizeof(stats[0])];
    uint64_t v[sizeof(stats) / sizeof(stats[0])];
    int rv, i;

    if (sw == NULL || out == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    memset(out, 0, sizeof(*out));

    rv = bcm_stat_multi_get(sw->unit, (bcm_port_t)logical_port, nstat, stats, values);
    if (rv != BCM_E_NONE) {
        return hb_status(rv);
    }
    for (i = 0; i < nstat; i++) {
        v[i] = hb_u64(values[i]);
    }

    out->in_octets = v[0];
    out->in_ucast_pkts = v[1];
    out->in_mcast_pkts = v[2];
    out->in_bcast_pkts = v[3];
    out->in_discards = v[4];
    out->in_errors = v[5];
    out->in_crc_errors = v[6];
    out->in_alignment_errors = v[7];
    out->in_symbol_errors = v[8];
    out->in_runts = v[9];
    out->in_giants = v[10];
    out->in_pause = v[11];
    out->out_octets = v[12];
    out->out_ucast_pkts = v[13];
    out->out_mcast_pkts = v[14];
    out->out_bcast_pkts = v[15];
    out->out_discards = v[16];
    out->out_errors = v[17];
    out->out_pause = v[18];
    out->collisions = v[19];
    out->late_collisions = v[20];
    out->deferred = v[21];

    /*
     * Hemlock's top bin is 1523-max where the chip's boundary is 1519, so
     * the three Broadcom bins above 1518 are summed into it. Frames of
     * 1519-1522 bytes therefore land one bin high. The alternative is
     * dropping the tail entirely, which loses jumbo counts altogether;
     * this is the smaller lie and it is confined to these two lines.
     */
    for (i = 0; i < 6; i++) {
        out->rx_bins[i] = v[22 + i];
        out->tx_bins[i] = v[31 + i];
    }
    out->rx_bins[6] = v[28] + v[29] + v[30];
    out->tx_bins[6] = v[37] + v[38] + v[39];

    return HEMLOCKBCM_OK;
}

/* ------------------------------------------------------------------ */
/* Capabilities                                                        */
/* ------------------------------------------------------------------ */

static int hb_capabilities(struct hemlockbcm_switch *sw,
                           struct hemlockbcm_capabilities *out)
{
    if (sw == NULL || out == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    memset(out, 0, sizeof(*out));

    /*
     * Only what this shim can actually back. Rust turns the rest off on
     * the strength of the NULL vtable slots, so nothing here can claim a
     * family phase 6 has not delivered.
     *
     * The Helix4's shared packet buffer is 4 MB. ECMP width and IPv6 stay
     * zero/false until the L3 slots exist — reporting them now would let
     * a commit reference a family that cannot be programmed.
     */
    out->buffer_bytes_total = 4ull * 1024ull * 1024ull;
    /* The width every group reserves; see HB_ECMP_MAX_PATHS. */
    out->ecmp_width = HB_ECMP_MAX_PATHS;
    /*
     * The XGS mirror-to-port table depth. The SDK has no call that
     * reports it, and this number is load-bearing rather than
     * decorative: syncd refuses a session number above it before
     * reaching the datapath. Getting it wrong is not dangerous -- the
     * SDK returns BCM_E_RESOURCE once the table is full either way -- but
     * too low would refuse sessions the chip would accept, so confirm it
     * on the hardware and correct this if it disagrees.
     */
    out->mirror_sessions_max = 4;
    out->ipv6 = 0;
    return HEMLOCKBCM_OK;
}

/* ------------------------------------------------------------------ */
/* Board bring-up (ABI 1.1)                                            */
/* ------------------------------------------------------------------ */

static int hb_load_led_program(struct hemlockbcm_switch *sw, const char *hex)
{
    char command[4096];

    if (sw == NULL || hex == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (strlen(hex) + sizeof("led prog ") > sizeof(command)) {
        printf("[hemlockbcm] LED program too long (%zu bytes)\n", strlen(hex));
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }

    /*
     * `led prog <hex>` loads the program, `led auto on` lets linkscan
     * drive it (down or no-SFP ports off, link on, activity blinking),
     * `led start` runs the M0. Cosmetic throughout: a failure is reported
     * and the datapath carries on.
     */
    snprintf(command, sizeof(command), "led prog %s", hex);
    if (sh_process_command(sw->unit, command) != 0) {
        printf("[hemlockbcm] 'led prog' failed\n");
        return HEMLOCKBCM_ERR_FAILURE;
    }
    sh_process_command(sw->unit, "led auto on");
    sh_process_command(sw->unit, "led start");
    printf("[hemlockbcm] LED program loaded and started\n");
    return HEMLOCKBCM_OK;
}

/* ------------------------------------------------------------------ */

/* --- L2 VLANs (ABI 1.2) ------------------------------------------------- */

/*
 * A VLAN here is its 802.1Q id and a membership is (vlan, port): the
 * opaque object ids SAI hands out are minted and unpacked on the Rust
 * side, so this file keeps no table of its own and nothing to lose
 * across a restart.
 */

static int hb_default_vlan(struct hemlockbcm_switch *sw, uint16_t *out)
{
    bcm_vlan_t vid = 0;
    int status;

    if (sw == NULL || out == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = HB_CALL(bcm_vlan_default_get(sw->unit, &vid));
    if (status == HEMLOCKBCM_OK) {
        *out = (uint16_t)vid;
    }
    return status;
}

static int hb_create_vlan(struct hemlockbcm_switch *sw, uint16_t vlan_id)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_vlan_create(sw->unit, (bcm_vlan_t)vlan_id));
}

static int hb_remove_vlan(struct hemlockbcm_switch *sw, uint16_t vlan_id)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_vlan_destroy(sw->unit, (bcm_vlan_t)vlan_id));
}

static int hb_add_vlan_member(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                              uint32_t logical_port, int tagged)
{
    bcm_pbmp_t pbmp, ubmp;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * Two bitmaps, not one flag: `pbmp` is who is in the VLAN and `ubmp`
     * is who egresses untagged. An untagged member is in both. Passing a
     * port in ubmp but not pbmp is meaningless, so it never happens here.
     */
    BCM_PBMP_CLEAR(pbmp);
    BCM_PBMP_CLEAR(ubmp);
    BCM_PBMP_PORT_ADD(pbmp, (bcm_port_t)logical_port);
    if (!tagged) {
        BCM_PBMP_PORT_ADD(ubmp, (bcm_port_t)logical_port);
    }
    return HB_CALL(bcm_vlan_port_add(sw->unit, (bcm_vlan_t)vlan_id, pbmp, ubmp));
}

static int hb_remove_vlan_member(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                 uint32_t logical_port)
{
    bcm_pbmp_t pbmp;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    BCM_PBMP_CLEAR(pbmp);
    BCM_PBMP_PORT_ADD(pbmp, (bcm_port_t)logical_port);
    /* Removing from pbmp drops the untagged bitmap entry with it. */
    return HB_CALL(bcm_vlan_port_remove(sw->unit, (bcm_vlan_t)vlan_id, pbmp));
}

static int hb_set_port_pvid(struct hemlockbcm_switch *sw, uint32_t logical_port,
                            uint16_t vlan_id)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_untagged_vlan_set(sw->unit, (bcm_port_t)logical_port,
                                              (bcm_vlan_t)vlan_id));
}

static int hb_set_port_tpid(struct hemlockbcm_switch *sw, uint32_t logical_port,
                            uint16_t tpid)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_tpid_set(sw->unit, (bcm_port_t)logical_port, (uint16)tpid));
}

/* --- MAC address table (ABI 1.3) ---------------------------------------- */

/*
 * The local module id, which every L2 entry's destination is qualified
 * by. Constant for the life of the switch on a single-unit board, so it
 * is read once at first use rather than on every FDB call.
 */
static int hb_modid(struct hemlockbcm_switch *sw, int *out)
{
    if (sw->modid < 0) {
        int modid = 0;
        int status = HB_CALL(bcm_stk_my_modid_get(sw->unit, &modid));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
        sw->modid = modid;
    }
    *out = sw->modid;
    return HEMLOCKBCM_OK;
}

static int hb_set_fdb_aging(struct hemlockbcm_switch *sw, uint32_t secs)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* The SDK spells "no aging" as 0 too, so this passes straight through. */
    return HB_CALL(bcm_l2_age_timer_set(sw->unit, (int)secs));
}

static int hb_add_fdb_entry(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                            const uint8_t mac[6], uint32_t logical_port, int discard)
{
    bcm_l2_addr_t entry;
    int modid = 0;
    int status;

    if (sw == NULL || mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_modid(sw, &modid);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    /* bcm_l2_addr_t has three dozen fields, most of them meaningless
     * here; the SDK's own initializer is the only safe way to fill it. */
    bcm_l2_addr_t_init(&entry, (const uint8 *)mac, (bcm_vlan_t)vlan_id);
    entry.flags = BCM_L2_STATIC;
    if (discard) {
        /* A black hole, not a forwarding entry: drop frames both to and
         * from this MAC. The port is meaningless and stays as init left
         * it. */
        entry.flags |= BCM_L2_DISCARD_SRC | BCM_L2_DISCARD_DST;
    } else {
        entry.port = (bcm_port_t)logical_port;
        entry.modid = modid;
    }
    /* bcm_l2_addr_add overwrites an existing entry for the same
     * (mac, vid), which is the "replaces" the caller documents. */
    return HB_CALL(bcm_l2_addr_add(sw->unit, &entry));
}

static int hb_remove_fdb_entry(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                               const uint8_t mac[6])
{
    bcm_mac_t address;

    if (sw == NULL || mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sal_memcpy(address, mac, sizeof(address));
    return HB_CALL(bcm_l2_addr_delete(sw->unit, address, (bcm_vlan_t)vlan_id));
}

static int hb_flush_fdb(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                        uint32_t logical_port, uint32_t flags)
{
    bcm_l2_addr_t match;
    uint32 replace_flags = BCM_L2_REPLACE_DELETE;
    int modid = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * One call covers all four scopes. bcm_l2_replace matches on the
     * fields its MATCH_* flags name and ignores the rest, so "everything"
     * is simply no MATCH flags at all. Crucially BCM_L2_REPLACE_
     * MATCH_STATIC is never set, which is what makes static entries
     * survive a flush -- the caller's documented rule -- in every scope
     * rather than only in the ones the by_vlan/by_port helpers cover.
     */
    bcm_l2_addr_t_init(&match, hb_zero_mac, BCM_VLAN_INVALID);
    if (flags & HEMLOCKBCM_FLUSH_VLAN) {
        match.vid = (bcm_vlan_t)vlan_id;
        replace_flags |= BCM_L2_REPLACE_MATCH_VLAN;
    }
    if (flags & HEMLOCKBCM_FLUSH_PORT) {
        status = hb_modid(sw, &modid);
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
        match.port = (bcm_port_t)logical_port;
        match.modid = modid;
        replace_flags |= BCM_L2_REPLACE_MATCH_DEST;
    }
    return HB_CALL(bcm_l2_replace(sw->unit, replace_flags, &match, 0, 0, 0));
}

static int hb_set_port_learning(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                int learn)
{
    /*
     * ARL is the hardware learning itself; FWD lets a frame whose source
     * is not yet in the table still be forwarded. Dropping FWD along with
     * ARL would black-hole traffic on a port whose MACs are configured
     * statically, which is not what "learning off" means, so FWD stays on
     * either way.
     */
    uint32 flags = learn ? (BCM_PORT_LEARN_ARL | BCM_PORT_LEARN_FWD)
                         : BCM_PORT_LEARN_FWD;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_learn_set(sw->unit, (bcm_port_t)logical_port, flags));
}

static int hb_set_port_learn_limit(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   int limit)
{
    bcm_l2_learn_limit_t config;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    bcm_l2_learn_limit_t_init(&config);
    config.flags = BCM_L2_LEARN_LIMIT_PORT;
    config.port = (bcm_port_t)logical_port;
    if (limit < 0) {
        /* The SDK's own spelling of "no cap". */
        config.limit = -1;
        return HB_CALL(bcm_l2_learn_limit_set(sw->unit, &config));
    }
    /*
     * ACTION_CPU rather than ACTION_DROP: the caller raises a
     * port-security violation event carrying the offending source MAC,
     * and it can only do that if the chip hands the frame to the CPU.
     * Learning still stops at the limit either way.
     */
    config.flags |= BCM_L2_LEARN_LIMIT_ACTION_CPU;
    config.limit = limit;
    return HB_CALL(bcm_l2_learn_limit_set(sw->unit, &config));
}

/* --- Link aggregation (ABI 1.4) ----------------------------------------- */

/*
 * Every member operation is read-modify-write of the whole member array
 * via bcm_trunk_get/bcm_trunk_set, rather than bcm_trunk_member_add and
 * its siblings. Those incremental calls are newer than this SDK's ESW
 * devices and return BCM_E_UNAVAIL on some of them; get/set is the path
 * every XGS device has always had. It also preserves bcm_trunk_info_t
 * (the port-selection criteria) instead of guessing at it, since the
 * struct comes back from the read.
 */
#define HB_TRUNK_MAX_MEMBERS 8

/* A member that is in the trunk but forwarding nothing, in either
 * direction: the 802.3ad collect/distribute gate held closed. */
#define HB_TRUNK_GATED (BCM_TRUNK_MEMBER_INGRESS_DISABLE | BCM_TRUNK_MEMBER_EGRESS_DISABLE)

/* Read a trunk's info and members. */
static int hb_trunk_read(struct hemlockbcm_switch *sw, uint32_t tid,
                         bcm_trunk_info_t *info, bcm_trunk_member_t *members,
                         int *count)
{
    return HB_CALL(bcm_trunk_get(sw->unit, (bcm_trunk_t)tid, info,
                                 HB_TRUNK_MAX_MEMBERS, members, count));
}

/* Index of `logical_port` in a member array, or -1. */
static int hb_trunk_index(const bcm_trunk_member_t *members, int count,
                          uint32_t logical_port)
{
    bcm_gport_t want;
    int i;

    BCM_GPORT_LOCAL_SET(want, (bcm_port_t)logical_port);
    for (i = 0; i < count; i++) {
        if (members[i].gport == want) {
            return i;
        }
    }
    return -1;
}

static int hb_lag_create(struct hemlockbcm_switch *sw, uint32_t *tid)
{
    bcm_trunk_t created = 0;
    int status;

    if (sw == NULL || tid == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = HB_CALL(bcm_trunk_create(sw->unit, 0, &created));
    if (status == HEMLOCKBCM_OK) {
        *tid = (uint32_t)created;
    }
    return status;
}

static int hb_lag_destroy(struct hemlockbcm_switch *sw, uint32_t tid)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_trunk_destroy(sw->unit, (bcm_trunk_t)tid));
}

static int hb_lag_member_add(struct hemlockbcm_switch *sw, uint32_t tid,
                             uint32_t logical_port, int enabled)
{
    bcm_trunk_info_t info;
    bcm_trunk_member_t members[HB_TRUNK_MAX_MEMBERS];
    int count = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_trunk_read(sw, tid, &info, members, &count);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    if (hb_trunk_index(members, count, logical_port) >= 0) {
        return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
    }
    if (count >= HB_TRUNK_MAX_MEMBERS) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    /*
     * A joining member has to pick up the trunk's ingress
     * classification, or a port added after lag_set_pvid would classify
     * into whatever VLAN it last had. There is no trunk-wide PVID to
     * read back -- it only exists as the per-member setting -- so take it
     * from a member already in the trunk. Inductively that keeps every
     * member in step, and an empty trunk has nothing to inherit because
     * the caller has not set a PVID on it yet.
     */
    if (count > 0 && BCM_GPORT_IS_LOCAL(members[0].gport)) {
        bcm_vlan_t pvid = 0;
        bcm_port_t first = BCM_GPORT_LOCAL_GET(members[0].gport);

        if (bcm_port_untagged_vlan_get(sw->unit, first, &pvid) == BCM_E_NONE) {
            status = HB_CALL(bcm_port_untagged_vlan_set(
                sw->unit, (bcm_port_t)logical_port, pvid));
            if (status != HEMLOCKBCM_OK) {
                return status;
            }
        }
    }
    bcm_trunk_member_t_init(&members[count]);
    BCM_GPORT_LOCAL_SET(members[count].gport, (bcm_port_t)logical_port);
    members[count].flags = enabled ? 0 : HB_TRUNK_GATED;
    count++;
    return HB_CALL(bcm_trunk_set(sw->unit, (bcm_trunk_t)tid, &info, count, members));
}

static int hb_lag_member_remove(struct hemlockbcm_switch *sw, uint32_t tid,
                                uint32_t logical_port)
{
    bcm_trunk_info_t info;
    bcm_trunk_member_t members[HB_TRUNK_MAX_MEMBERS];
    int count = 0;
    int index;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_trunk_read(sw, tid, &info, members, &count);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    index = hb_trunk_index(members, count, logical_port);
    if (index < 0) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* Close the gap; the array's order carries no meaning. */
    members[index] = members[count - 1];
    count--;
    return HB_CALL(bcm_trunk_set(sw->unit, (bcm_trunk_t)tid, &info, count, members));
}

static int hb_lag_member_state(struct hemlockbcm_switch *sw, uint32_t tid,
                               uint32_t logical_port, int enabled)
{
    bcm_trunk_info_t info;
    bcm_trunk_member_t members[HB_TRUNK_MAX_MEMBERS];
    int count = 0;
    int index;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_trunk_read(sw, tid, &info, members, &count);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    index = hb_trunk_index(members, count, logical_port);
    if (index < 0) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    members[index].flags = enabled ? 0 : HB_TRUNK_GATED;
    return HB_CALL(bcm_trunk_set(sw->unit, (bcm_trunk_t)tid, &info, count, members));
}

static int hb_lag_vlan_member_add(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                  uint32_t tid, int tagged)
{
    bcm_gport_t gport;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    BCM_GPORT_TRUNK_SET(gport, (bcm_trunk_t)tid);
    /* The port form of this uses two bitmaps; the gport form takes the
     * untagged decision as a flag on the one call. */
    return HB_CALL(bcm_vlan_gport_add(sw->unit, (bcm_vlan_t)vlan_id, gport,
                                      tagged ? 0 : BCM_VLAN_GPORT_ADD_UNTAGGED));
}

static int hb_lag_vlan_member_remove(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                     uint32_t tid)
{
    bcm_gport_t gport;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    BCM_GPORT_TRUNK_SET(gport, (bcm_trunk_t)tid);
    return HB_CALL(bcm_vlan_gport_delete(sw->unit, (bcm_vlan_t)vlan_id, gport));
}

static int hb_lag_set_pvid(struct hemlockbcm_switch *sw, uint32_t tid, uint16_t vlan_id)
{
    bcm_trunk_info_t info;
    bcm_trunk_member_t members[HB_TRUNK_MAX_MEMBERS];
    int count = 0;
    int status;
    int i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_trunk_read(sw, tid, &info, members, &count);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    /*
     * Ingress classification belongs to the receiving port, so there is
     * no trunk-wide form of it: apply it to every member. Gated-closed
     * members are in the array too, which is why they stay in the trunk
     * rather than being removed from it.
     */
    for (i = 0; i < count; i++) {
        bcm_port_t port;

        if (!BCM_GPORT_IS_LOCAL(members[i].gport)) {
            continue;  /* not a local port; nothing to classify here */
        }
        port = BCM_GPORT_LOCAL_GET(members[i].gport);
        status = HB_CALL(bcm_port_untagged_vlan_set(sw->unit, port,
                                                    (bcm_vlan_t)vlan_id));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
    }
    return HEMLOCKBCM_OK;
}

/* --- Spanning tree (ABI 1.5) -------------------------------------------- */

static int hb_stp_state(int state, int *out)
{
    switch (state) {
    case HEMLOCKBCM_STP_BLOCKING:
        *out = BCM_STG_STP_BLOCK;
        return HEMLOCKBCM_OK;
    case HEMLOCKBCM_STP_LEARNING:
        *out = BCM_STG_STP_LEARN;
        return HEMLOCKBCM_OK;
    case HEMLOCKBCM_STP_FORWARDING:
        *out = BCM_STG_STP_FORWARD;
        return HEMLOCKBCM_OK;
    default:
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
}

static int hb_stp_default(struct hemlockbcm_switch *sw, uint32_t *stg)
{
    bcm_stg_t value = 0;
    int status;

    if (sw == NULL || stg == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = HB_CALL(bcm_stg_default_get(sw->unit, &value));
    if (status == HEMLOCKBCM_OK) {
        *stg = (uint32_t)value;
    }
    return status;
}

static int hb_stp_create(struct hemlockbcm_switch *sw, uint32_t *stg)
{
    bcm_stg_t created = 0;
    int status;

    if (sw == NULL || stg == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = HB_CALL(bcm_stg_create(sw->unit, &created));
    if (status == HEMLOCKBCM_OK) {
        *stg = (uint32_t)created;
    }
    return status;
}

static int hb_stp_destroy(struct hemlockbcm_switch *sw, uint32_t stg)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_stg_destroy(sw->unit, (bcm_stg_t)stg));
}

/*
 * The group currently holding `vlan_id`, or BCM_STG_INVALID if none does.
 *
 * There is no "which group is this VLAN in" call, so this walks the
 * groups and asks each for its VLANs. Both lists are SDK-allocated and
 * have their own destructors; the loop frees every list it takes,
 * including on the early exit.
 */
static int hb_stp_group_of(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                           bcm_stg_t *out)
{
    bcm_stg_t *groups = NULL;
    int group_count = 0;
    int status;
    int i;

    *out = BCM_STG_INVALID;
    status = HB_CALL(bcm_stg_list(sw->unit, &groups, &group_count));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    for (i = 0; i < group_count; i++) {
        bcm_vlan_t *vlans = NULL;
        int vlan_count = 0;
        int j;

        if (bcm_stg_vlan_list(sw->unit, groups[i], &vlans, &vlan_count) != BCM_E_NONE) {
            continue;
        }
        for (j = 0; j < vlan_count; j++) {
            if (vlans[j] == (bcm_vlan_t)vlan_id) {
                *out = groups[i];
                break;
            }
        }
        bcm_stg_vlan_list_destroy(sw->unit, vlans, vlan_count);
        if (*out != BCM_STG_INVALID) {
            break;
        }
    }
    bcm_stg_list_destroy(sw->unit, groups, group_count);
    return HEMLOCKBCM_OK;
}

static int hb_stp_vlan_set(struct hemlockbcm_switch *sw, uint32_t stg, uint16_t vlan_id)
{
    bcm_stg_t current = BCM_STG_INVALID;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_stp_group_of(sw, vlan_id, &current);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    if (current == (bcm_stg_t)stg) {
        return HEMLOCKBCM_OK;  /* already there */
    }
    /*
     * A VLAN belongs to exactly one group. Whether bcm_stg_vlan_add
     * relocates it is not documented, so do the move explicitly rather
     * than leave the VLAN in two groups on a device that does not.
     */
    if (current != BCM_STG_INVALID) {
        status = HB_CALL(bcm_stg_vlan_remove(sw->unit, current, (bcm_vlan_t)vlan_id));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
    }
    return HB_CALL(bcm_stg_vlan_add(sw->unit, (bcm_stg_t)stg, (bcm_vlan_t)vlan_id));
}

static int hb_stp_port_state(struct hemlockbcm_switch *sw, uint32_t stg,
                             uint32_t logical_port, int state)
{
    int bcm_state = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_stp_state(state, &bcm_state);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    return HB_CALL(bcm_stg_stp_set(sw->unit, (bcm_stg_t)stg,
                                   (bcm_port_t)logical_port, bcm_state));
}

static int hb_lag_stp_port_state(struct hemlockbcm_switch *sw, uint32_t stg,
                                 uint32_t tid, int state)
{
    bcm_trunk_info_t info;
    bcm_trunk_member_t members[HB_TRUNK_MAX_MEMBERS];
    int count = 0;
    int bcm_state = 0;
    int status;
    int i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_stp_state(state, &bcm_state);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    status = hb_trunk_read(sw, tid, &info, members, &count);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    /* Forwarding state is per port, like ingress classification: every
     * member, gated-closed ones included. */
    for (i = 0; i < count; i++) {
        bcm_port_t port;

        if (!BCM_GPORT_IS_LOCAL(members[i].gport)) {
            continue;
        }
        port = BCM_GPORT_LOCAL_GET(members[i].gport);
        status = HB_CALL(bcm_stg_stp_set(sw->unit, (bcm_stg_t)stg, port, bcm_state));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
    }
    return HEMLOCKBCM_OK;
}

/* --- Port mirroring (ABI 1.6) -------------------------------------------- */

static uint32 hb_mirror_flags(int egress)
{
    return egress ? BCM_MIRROR_PORT_EGRESS : BCM_MIRROR_PORT_INGRESS;
}

static int hb_mirror_create(struct hemlockbcm_switch *sw, uint32_t monitor_port,
                            uint32_t *session)
{
    bcm_mirror_destination_t dest;
    int status;

    if (sw == NULL || session == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * Mirroring is off until the mode is set, and setting it again is
     * harmless -- cheaper than tracking whether it has been done, and it
     * keeps the shim stateless.
     */
    status = HB_CALL(bcm_mirror_mode_set(sw->unit, BCM_MIRROR_L2));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    bcm_mirror_destination_t_init(&dest);
    /* Local SPAN: no flags, destination is a port on this device. */
    BCM_GPORT_LOCAL_SET(dest.gport, (bcm_port_t)monitor_port);
    status = HB_CALL(bcm_mirror_destination_create(sw->unit, &dest));
    if (status == HEMLOCKBCM_OK) {
        *session = (uint32_t)dest.mirror_dest_id;
    }
    return status;
}

static int hb_mirror_destroy(struct hemlockbcm_switch *sw, uint32_t session)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_mirror_destination_destroy(sw->unit, (bcm_gport_t)session));
}

static int hb_mirror_port_attach(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 uint32_t session, int egress)
{
    uint32 flags = hb_mirror_flags(egress);
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * The SDK's add is genuinely additive -- a port can feed several
     * destinations at once -- but the ABI here is a set, so clear the
     * direction first. Deleting when nothing is attached is not an
     * error.
     */
    status = HB_CALL(bcm_mirror_port_dest_delete_all(sw->unit,
                                                     (bcm_port_t)logical_port, flags));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    return HB_CALL(bcm_mirror_port_dest_add(sw->unit, (bcm_port_t)logical_port, flags,
                                            (bcm_gport_t)session));
}

static int hb_mirror_port_detach(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 int egress)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_mirror_port_dest_delete_all(sw->unit, (bcm_port_t)logical_port,
                                                   hb_mirror_flags(egress)));
}

/* --- Storm control (ABI 1.7) --------------------------------------------- */

static int hb_storm_flags(int storm_class, int *out)
{
    switch (storm_class) {
    case HEMLOCKBCM_STORM_BROADCAST:
        *out = BCM_RATE_BCAST;
        return HEMLOCKBCM_OK;
    case HEMLOCKBCM_STORM_MULTICAST:
        *out = BCM_RATE_MCAST;
        return HEMLOCKBCM_OK;
    case HEMLOCKBCM_STORM_UNKNOWN_UNICAST:
        /* Destination lookup failure: unicast with no FDB entry. */
        *out = BCM_RATE_DLF;
        return HEMLOCKBCM_OK;
    default:
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
}

static int hb_storm_control_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                int storm_class, uint32_t kbps)
{
    int flags = 0;
    uint32 burst;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_storm_flags(storm_class, &flags);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    /*
     * The burst allowance is the one number here the caller does not
     * supply, and the SDK has no "pick a sensible default" value: zero
     * is a rate, not an absence. An eighth of the rate is 125 ms of
     * traffic, which is long enough that a normal burst of broadcast
     * (an ARP storm from a rebooting rack) is metered rather than
     * shredded, and short enough that the cap still means something.
     * Worth revisiting against real traffic; it changes smoothness, not
     * the enforced rate.
     */
    burst = kbps / 8;
    if (kbps != 0 && burst == 0) {
        burst = 1;
    }
    /* BCM_RATE_DISABLE is 0, so "no limit" is the SDK's own encoding
     * rather than a sentinel invented here. */
    return HB_CALL(bcm_rate_bandwidth_set(sw->unit, (bcm_port_t)logical_port, flags,
                                          kbps, burst));
}

/* --- Host interfaces (ABI 1.8) ------------------------------------------- */

/* Per-port filters sit above the priority a future catch-all would use;
 * 0 is the highest and is left free deliberately. */
#define HB_KNET_PRIORITY_PORT 1

static int hb_host_punt_setup(struct hemlockbcm_switch *sw)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * Clears any netifs and filters a previous run left in the kernel
     * module, which is what makes creating them again idempotent across
     * a syncd restart. The KNET modules outlive the process.
     */
    return HB_CALL(bcm_knet_init(sw->unit));
}

static int hb_hostif_create(struct hemlockbcm_switch *sw, uint32_t logical_port,
                            const char *name, uint32_t *hostif)
{
    bcm_knet_netif_t netif;
    bcm_knet_filter_t filter;
    int status;

    if (sw == NULL || name == NULL || hostif == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }

    bcm_knet_netif_t_init(&netif);
    /* TX_LOCAL_PORT: what the kernel writes to this netdev leaves on the
     * physical port, bypassing lookup -- which is what a protocol stack
     * sending its own BPDUs and LACPDUs needs. */
    netif.type = BCM_KNET_NETIF_T_TX_LOCAL_PORT;
    netif.port = (bcm_port_t)logical_port;
    sal_memcpy(netif.mac_addr, sw->src_mac, sizeof(netif.mac_addr));
    /* The SDK's buffer is 16 bytes and the caller promises at most 15
     * characters, so this cannot truncate; the bound is belt and braces. */
    snprintf(netif.name, sizeof(netif.name), "%s", name);
    status = HB_CALL(bcm_knet_netif_create(sw->unit, &netif));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }

    bcm_knet_filter_t_init(&filter);
    filter.type = BCM_KNET_FILTER_T_RX_PKT;
    filter.priority = HB_KNET_PRIORITY_PORT;
    filter.dest_type = BCM_KNET_DEST_T_NETIF;
    filter.dest_id = netif.id;
    filter.match_flags = BCM_KNET_FILTER_M_INGPORT;
    filter.m_ingport = (bcm_port_t)logical_port;
    /*
     * Strip the VLAN tag on the way up. The chip presents punted frames
     * with the internal tag it classified them into, which for an access
     * port is a tag that was never on the wire -- so leaving it on would
     * hand the stack a frame the peer never sent. Visible immediately at
     * bring-up: tcpdump on the netdev shows a tag that should not be
     * there, or a missing one if this is wrong in the other direction.
     */
    filter.flags = BCM_KNET_FILTER_F_STRIP_TAG;
    snprintf(filter.desc, sizeof(filter.desc), "%s", name);
    status = HB_CALL(bcm_knet_filter_create(sw->unit, &filter));
    if (status != HEMLOCKBCM_OK) {
        /* Do not leave a netdev with nothing feeding it. */
        (void)bcm_knet_netif_destroy(sw->unit, netif.id);
        return status;
    }
    *hostif = (uint32_t)netif.id;
    return HEMLOCKBCM_OK;
}

/* --- Policers (ABI 1.9) --------------------------------------------------- */

/*
 * The SDK carries rates and bursts as thousands plus a 0..999 remainder,
 * so an exact value needs both halves. Splitting here rather than
 * rounding to the nearest thousand matters at the small end: a CoPP
 * class metered at 100 packets/s would otherwise become 0.
 */
static void hb_policer_split(uint64_t value, uint32 *thousands, uint32 *remainder)
{
    *thousands = (uint32)(value / 1000u);
    *remainder = (uint32)(value % 1000u);
}

static void hb_policer_config(bcm_policer_config_t *cfg, int pps, uint64_t rate,
                              uint64_t burst)
{
    bcm_policer_config_t_init(cfg);
    /*
     * Committed mode is a single rate with a single bucket, which is
     * what the caller's spec describes. Colour-blind because nothing
     * upstream marks packets before they reach the policer, so treating
     * an arriving packet as anything but green would meter on a colour
     * no one set.
     */
    cfg->mode = bcmPolicerModeCommitted;
    cfg->flags = BCM_POLICER_COLOR_BLIND;
    cfg->flags |= pps ? BCM_POLICER_MODE_PACKETS : BCM_POLICER_MODE_BYTES;
    /* In byte mode the SDK counts bits, and the caller's burst is in
     * bytes; in packet mode both are packets. */
    hb_policer_split(rate, &cfg->ckbits_sec, &cfg->cbits_sec_lower);
    hb_policer_split(pps ? burst : burst * 8u, &cfg->ckbits_burst,
                     &cfg->cbits_burst_lower);
}

static int hb_policer_create(struct hemlockbcm_switch *sw, int pps, uint64_t rate,
                             uint64_t burst, uint32_t *policer)
{
    bcm_policer_config_t cfg;
    bcm_policer_t created = 0;
    int status;

    if (sw == NULL || policer == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    hb_policer_config(&cfg, pps, rate, burst);
    status = HB_CALL(bcm_policer_create(sw->unit, &cfg, &created));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    /* Counting is off by default, and the caller can ask for the counts
     * at any time, so turn it on with the policer rather than lazily. */
    status = HB_CALL(bcm_policer_stat_enable_set(sw->unit, created, 1));
    if (status != HEMLOCKBCM_OK) {
        (void)bcm_policer_destroy(sw->unit, created);
        return status;
    }
    *policer = (uint32_t)created;
    return HEMLOCKBCM_OK;
}

static int hb_policer_set(struct hemlockbcm_switch *sw, uint32_t policer, int pps,
                          uint64_t rate, uint64_t burst)
{
    bcm_policer_config_t cfg;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    hb_policer_config(&cfg, pps, rate, burst);
    return HB_CALL(bcm_policer_set(sw->unit, (bcm_policer_t)policer, &cfg));
}

static int hb_policer_destroy(struct hemlockbcm_switch *sw, uint32_t policer)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_policer_destroy(sw->unit, (bcm_policer_t)policer));
}

static int hb_policer_stats(struct hemlockbcm_switch *sw, uint32_t policer,
                            uint64_t *conforming, uint64_t *dropped)
{
    /*
     * The chip counts colour *transitions*, not conformance, so these
     * two numbers are a reading of that matrix rather than a lookup.
     * Colour-blind committed mode admits every packet as green, so:
     * green-to-green is what conformed, and green-to-red plus
     * green-to-drop is what did not. Nothing arrives yellow or red, so
     * the rest of the matrix stays zero and is not summed.
     *
     * This mapping is the one thing in this family that no header
     * states; if a policer ever reports conforming counts while
     * visibly dropping, this is the place to look.
     */
    static const bcm_policer_stat_t drop_stats[] = {
        bcmPolicerStatGreenToRedPackets,
        bcmPolicerStatGreenToDropPackets,
    };
    uint64 value;
    uint64_t total = 0;
    int status;
    size_t i;

    if (sw == NULL || conforming == NULL || dropped == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    COMPILER_64_ZERO(value);
    status = HB_CALL(bcm_policer_stat_get(sw->unit, (bcm_policer_t)policer, 0,
                                          bcmPolicerStatGreenToGreenPackets, &value));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    *conforming = hb_u64(value);

    for (i = 0; i < sizeof(drop_stats) / sizeof(drop_stats[0]); i++) {
        COMPILER_64_ZERO(value);
        status = HB_CALL(bcm_policer_stat_get(sw->unit, (bcm_policer_t)policer, 0,
                                              drop_stats[i], &value));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
        total += hb_u64(value);
    }
    *dropped = total;
    return HEMLOCKBCM_OK;
}

/* --- ACLs (ABI 1.10) ----------------------------------------------------- */

/*
 * Field groups are created with the full qualifier set this ABI can
 * express, rather than one tailored per table. The chip allocates TCAM
 * by slice and a wider qset can cost a wider slice, but a group whose
 * qset is decided at create time cannot gain a qualifier later -- and
 * the caller does not tell us, at create time, which fields its entries
 * will use. Trading slice width for that is the right way round: an
 * entry that cannot be expressed is a failure, a slice that is wider
 * than it needed to be is an inefficiency.
 */
static void hb_acl_qset(bcm_field_qset_t *qset, int egress)
{
    BCM_FIELD_QSET_INIT(*qset);
    BCM_FIELD_QSET_ADD(*qset, egress ? bcmFieldQualifyStageEgress
                                     : bcmFieldQualifyStageIngress);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyInPorts);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifySrcIp);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyDstIp);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyIpProtocol);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyL4SrcPort);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyL4DstPort);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyDSCP);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifySrcMac);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyDstMac);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyEtherType);
    BCM_FIELD_QSET_ADD(*qset, bcmFieldQualifyOuterVlanId);
}

static int hb_acl_table_create(struct hemlockbcm_switch *sw, int egress, uint32_t *table)
{
    bcm_field_qset_t qset;
    bcm_field_group_t group = 0;
    int status;

    if (sw == NULL || table == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    hb_acl_qset(&qset, egress);
    /*
     * Group priority is the order between *groups*, not between entries,
     * and Hemlock has one group per table with no defined precedence
     * among tables -- a port binds one table per stage. BCM_FIELD_GROUP_
     * PRIO_ANY lets the SDK place it.
     */
    status = HB_CALL(bcm_field_group_create(sw->unit, qset, BCM_FIELD_GROUP_PRIO_ANY,
                                            &group));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    *table = (uint32_t)group;
    return HEMLOCKBCM_OK;
}

static int hb_acl_table_destroy(struct hemlockbcm_switch *sw, uint32_t table)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_field_group_destroy(sw->unit, (bcm_field_group_t)table));
}

static int hb_acl_table_bind(struct hemlockbcm_switch *sw, uint32_t table,
                             uint32_t logical_port, int bind)
{
    bcm_pbmp_t pbmp;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    BCM_PBMP_CLEAR(pbmp);
    BCM_PBMP_PORT_ADD(pbmp, (bcm_port_t)logical_port);
    if (bind) {
        return HB_CALL(bcm_field_group_ports_add(sw->unit, (bcm_field_group_t)table,
                                                 pbmp));
    }
    return HB_CALL(bcm_field_group_ports_remove(sw->unit, (bcm_field_group_t)table,
                                                pbmp));
}

struct hb_unbind_ctx {
    int unit;
    int egress;
    bcm_port_t port;
    int status;
};

static int hb_unbind_one(int unit, bcm_field_group_t group, void *user_data)
{
    struct hb_unbind_ctx *ctx = (struct hb_unbind_ctx *)user_data;
    bcm_field_qset_t qset;
    bcm_pbmp_t pbmp;
    int rv;

    /* Only groups at the stage we were asked about. The stage is a
     * qualifier in the group's own qset, which is how it is read back. */
    if (bcm_field_group_get(unit, group, &qset) != BCM_E_NONE) {
        return BCM_E_NONE;  /* skip; traversal continues */
    }
    if (BCM_FIELD_QSET_TEST(qset, ctx->egress ? bcmFieldQualifyStageEgress
                                              : bcmFieldQualifyStageIngress) == 0) {
        return BCM_E_NONE;
    }
    BCM_PBMP_CLEAR(pbmp);
    BCM_PBMP_PORT_ADD(pbmp, ctx->port);
    rv = bcm_field_group_ports_remove(unit, group, pbmp);
    /* A group the port was never bound to is not a failure here: this is
     * "unbind from whatever holds it", and most groups will not. */
    if (rv != BCM_E_NONE && rv != BCM_E_NOT_FOUND) {
        ctx->status = hb_status(rv);
    }
    return BCM_E_NONE;
}

static int hb_acl_table_unbind_all(struct hemlockbcm_switch *sw, int egress,
                                   uint32_t logical_port)
{
    struct hb_unbind_ctx ctx;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    ctx.unit = sw->unit;
    ctx.egress = egress;
    ctx.port = (bcm_port_t)logical_port;
    ctx.status = HEMLOCKBCM_OK;
    status = HB_CALL(bcm_field_group_traverse(sw->unit, hb_unbind_one, &ctx));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    return ctx.status;
}

static int hb_acl_action(struct hemlockbcm_switch *sw, bcm_field_entry_t entry,
                         int action)
{
    switch (action) {
    case HEMLOCKBCM_ACL_FORWARD:
        /* Nothing to add: an entry with no action matches and lets the
         * packet take its normal path, which is what permit means. */
        return HEMLOCKBCM_OK;
    case HEMLOCKBCM_ACL_DROP:
        return HB_CALL(bcm_field_action_add(sw->unit, entry, bcmFieldActionDrop, 0, 0));
    case HEMLOCKBCM_ACL_TRAP: {
        /* Punted *and* dropped: the copy reaches the CPU, the original
         * does not forward. Two actions, not one. */
        int status = HB_CALL(bcm_field_action_add(sw->unit, entry,
                                                  bcmFieldActionCopyToCpu, 0, 0));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
        return HB_CALL(bcm_field_action_add(sw->unit, entry, bcmFieldActionDrop, 0, 0));
    }
    case HEMLOCKBCM_ACL_COPY:
        return HB_CALL(bcm_field_action_add(sw->unit, entry, bcmFieldActionCopyToCpu,
                                            0, 0));
    default:
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
}

static int hb_acl_qualify(struct hemlockbcm_switch *sw, bcm_field_entry_t entry,
                          const struct hemlockbcm_acl_fields *f)
{
    int status;

#define HB_QUALIFY(expr)                    \
    do {                                    \
        status = HB_CALL(expr);             \
        if (status != HEMLOCKBCM_OK) {      \
            return status;                  \
        }                                   \
    } while (0)

    if (f->present & HEMLOCKBCM_ACL_F_SRC_IP) {
        HB_QUALIFY(bcm_field_qualify_SrcIp(sw->unit, entry, f->src_ip, f->src_ip_mask));
    }
    if (f->present & HEMLOCKBCM_ACL_F_DST_IP) {
        HB_QUALIFY(bcm_field_qualify_DstIp(sw->unit, entry, f->dst_ip, f->dst_ip_mask));
    }
    if (f->present & HEMLOCKBCM_ACL_F_PROTOCOL) {
        HB_QUALIFY(bcm_field_qualify_IpProtocol(sw->unit, entry, f->protocol, 0xff));
    }
    if (f->present & HEMLOCKBCM_ACL_F_SRC_PORT) {
        HB_QUALIFY(bcm_field_qualify_L4SrcPort(sw->unit, entry, f->src_port, 0xffff));
    }
    if (f->present & HEMLOCKBCM_ACL_F_DST_PORT) {
        HB_QUALIFY(bcm_field_qualify_L4DstPort(sw->unit, entry, f->dst_port, 0xffff));
    }
    if (f->present & HEMLOCKBCM_ACL_F_DSCP) {
        HB_QUALIFY(bcm_field_qualify_DSCP(sw->unit, entry, f->dscp, 0x3f));
    }
    if (f->present & HEMLOCKBCM_ACL_F_SRC_MAC) {
        bcm_mac_t mac, mask;
        sal_memcpy(mac, f->src_mac, sizeof(mac));
        sal_memcpy(mask, f->src_mac_mask, sizeof(mask));
        HB_QUALIFY(bcm_field_qualify_SrcMac(sw->unit, entry, mac, mask));
    }
    if (f->present & HEMLOCKBCM_ACL_F_DST_MAC) {
        bcm_mac_t mac, mask;
        sal_memcpy(mac, f->dst_mac, sizeof(mac));
        sal_memcpy(mask, f->dst_mac_mask, sizeof(mask));
        HB_QUALIFY(bcm_field_qualify_DstMac(sw->unit, entry, mac, mask));
    }
    if (f->present & HEMLOCKBCM_ACL_F_ETHERTYPE) {
        HB_QUALIFY(bcm_field_qualify_EtherType(sw->unit, entry, f->ethertype, 0xffff));
    }
    if (f->present & HEMLOCKBCM_ACL_F_VLAN) {
        HB_QUALIFY(bcm_field_qualify_OuterVlanId(sw->unit, entry, f->vlan, 0xfff));
    }
#undef HB_QUALIFY
    return HEMLOCKBCM_OK;
}

static int hb_acl_entry_create(struct hemlockbcm_switch *sw, uint32_t table,
                               uint32_t priority,
                               const struct hemlockbcm_acl_fields *fields,
                               int action, uint32_t *entry)
{
    bcm_field_entry_t created = 0;
    int status;

    if (sw == NULL || fields == NULL || entry == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = HB_CALL(bcm_field_entry_create(sw->unit, (bcm_field_group_t)table,
                                            &created));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    status = HB_CALL(bcm_field_entry_prio_set(sw->unit, created, (int)priority));
    if (status == HEMLOCKBCM_OK) {
        status = hb_acl_qualify(sw, created, fields);
    }
    if (status == HEMLOCKBCM_OK) {
        status = hb_acl_action(sw, created, action);
    }
    if (status == HEMLOCKBCM_OK) {
        /* Nothing above touches the TCAM until this. */
        status = HB_CALL(bcm_field_entry_install(sw->unit, created));
    }
    if (status != HEMLOCKBCM_OK) {
        /* A half-built entry is worse than none: it occupies a slot and
         * matches on whichever qualifiers did land. */
        (void)bcm_field_entry_destroy(sw->unit, created);
        return status;
    }
    *entry = (uint32_t)created;
    return HEMLOCKBCM_OK;
}

static int hb_acl_entry_action_set(struct hemlockbcm_switch *sw, uint32_t entry,
                                   int action)
{
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Replace rather than accumulate: adding drop to an entry that
     * already copies would leave both in place. */
    status = HB_CALL(bcm_field_action_remove_all(sw->unit, (bcm_field_entry_t)entry));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    status = hb_acl_action(sw, (bcm_field_entry_t)entry, action);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    return HB_CALL(bcm_field_entry_install(sw->unit, (bcm_field_entry_t)entry));
}

static int hb_acl_entry_destroy(struct hemlockbcm_switch *sw, uint32_t entry)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_field_entry_destroy(sw->unit, (bcm_field_entry_t)entry));
}

struct hb_avail_ctx {
    int unit;
    int egress;
    uint32_t free_entries;
};

static int hb_avail_one(int unit, bcm_field_group_t group, void *user_data)
{
    struct hb_avail_ctx *ctx = (struct hb_avail_ctx *)user_data;
    bcm_field_group_status_t status;
    bcm_field_qset_t qset;

    if (bcm_field_group_get(unit, group, &qset) != BCM_E_NONE) {
        return BCM_E_NONE;
    }
    if (BCM_FIELD_QSET_TEST(qset, ctx->egress ? bcmFieldQualifyStageEgress
                                              : bcmFieldQualifyStageIngress) == 0) {
        return BCM_E_NONE;
    }
    if (bcm_field_group_status_get(unit, group, &status) != BCM_E_NONE) {
        return BCM_E_NONE;
    }
    if (status.entries_free > 0) {
        ctx->free_entries += (uint32_t)status.entries_free;
    }
    return BCM_E_NONE;
}

static int hb_acl_available(struct hemlockbcm_switch *sw, int egress, uint32_t *entries)
{
    struct hb_avail_ctx ctx;
    int status;

    if (sw == NULL || entries == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * Free TCAM is reported per group, and a group holds a slice, so the
     * honest total at a stage is the sum over that stage's groups. It is
     * not the whole free TCAM: space in slices no group has claimed yet
     * is not counted, because the SDK will not say how much of it this
     * stage could still take. Under-reporting is the safe direction for
     * a utilisation figure.
     */
    ctx.unit = sw->unit;
    ctx.egress = egress;
    ctx.free_entries = 0;
    status = HB_CALL(bcm_field_group_traverse(sw->unit, hb_avail_one, &ctx));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    *entries = ctx.free_entries;
    return HEMLOCKBCM_OK;
}

/* --- ACL counters and per-entry policers (ABI 1.11) ----------------------- */

static int hb_acl_counter_create(struct hemlockbcm_switch *sw, uint32_t table,
                                 uint32_t *counter)
{
    bcm_field_stat_t stats[1];
    int stat_id = 0;
    int status;

    if (sw == NULL || counter == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Packets only; see the header for why not both. */
    stats[0] = bcmFieldStatPackets;
    status = HB_CALL(bcm_field_stat_create(sw->unit, (bcm_field_group_t)table, 1, stats,
                                           &stat_id));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    *counter = (uint32_t)stat_id;
    return HEMLOCKBCM_OK;
}

static int hb_acl_counter_destroy(struct hemlockbcm_switch *sw, uint32_t counter)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_field_stat_destroy(sw->unit, (int)counter));
}

static int hb_acl_counter_get(struct hemlockbcm_switch *sw, uint32_t counter,
                              uint64_t *packets)
{
    uint64 value;
    int status;

    if (sw == NULL || packets == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    COMPILER_64_ZERO(value);
    status = HB_CALL(bcm_field_stat_get(sw->unit, (int)counter, bcmFieldStatPackets,
                                        &value));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    *packets = hb_u64(value);
    return HEMLOCKBCM_OK;
}

/*
 * The policer level. The chip supports a hierarchy of meters per entry;
 * Hemlock's model is one policer per rule, so everything sits at the
 * first level.
 */
#define HB_ACL_POLICER_LEVEL 0

static int hb_acl_entry_attach(struct hemlockbcm_switch *sw, uint32_t entry,
                               uint32_t counter, uint32_t policer)
{
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * Detach first in both cases, so this is a set rather than an
     * accumulation. Detaching what is not attached is not an error here:
     * the caller reaches this on every action update, most of which
     * change neither attachment.
     */
    (void)bcm_field_entry_policer_detach(sw->unit, (bcm_field_entry_t)entry,
                                         HB_ACL_POLICER_LEVEL);
    if (policer != 0) {
        status = HB_CALL(bcm_field_entry_policer_attach(sw->unit,
                                                        (bcm_field_entry_t)entry,
                                                        HB_ACL_POLICER_LEVEL,
                                                        (bcm_policer_t)policer));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
    }

    /*
     * Counters detach by id, not wholesale, so the current one has to be
     * read back before it can be replaced -- there is no detach_all for
     * statistics the way there is for policers.
     */
    {
        int current = 0;

        if (bcm_field_entry_stat_get(sw->unit, (bcm_field_entry_t)entry, &current)
                == BCM_E_NONE
            && current != (int)counter) {
            (void)bcm_field_entry_stat_detach(sw->unit, (bcm_field_entry_t)entry,
                                              current);
        }
    }
    if (counter != 0) {
        status = HB_CALL(bcm_field_entry_stat_attach(sw->unit, (bcm_field_entry_t)entry,
                                                     (int)counter));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
    }
    /* Attachments are TCAM-visible state like the actions are. */
    return HB_CALL(bcm_field_entry_install(sw->unit, (bcm_field_entry_t)entry));
}

/* --- Router interfaces (ABI 1.12) ----------------------------------------- */

static int hb_l3_intf_create(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                             const uint8_t mac[6], uint32_t *rif)
{
    bcm_l3_intf_t intf;
    int status;

    bcm_l3_intf_t_init(&intf);
    intf.l3a_vid = (bcm_vlan_t)vlan_id;
    sal_memcpy(intf.l3a_mac_addr, mac, sizeof(intf.l3a_mac_addr));
    status = HB_CALL(bcm_l3_intf_create(sw->unit, &intf));
    if (status == HEMLOCKBCM_OK) {
        *rif = (uint32_t)intf.l3a_intf_id;
    }
    return status;
}

static int hb_l3_intf_destroy(struct hemlockbcm_switch *sw, uint32_t rif)
{
    bcm_l3_intf_t intf;

    /* Delete takes the whole struct, but only the id is read. */
    bcm_l3_intf_t_init(&intf);
    intf.l3a_intf_id = (bcm_if_t)rif;
    return HB_CALL(bcm_l3_intf_delete(sw->unit, &intf));
}

static int hb_rif_port_create(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              uint16_t vlan_id, const uint8_t mac[6], uint32_t *rif)
{
    bcm_pbmp_t pbmp, ubmp;
    bcm_vlan_t default_vlan = 0;
    int status;

    if (sw == NULL || mac == NULL || rif == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }

    /*
     * Four steps, and the order matters: the port has to leave the
     * bridge before it joins the routed VLAN, or it briefly forwards
     * between the two. Each failure below unwinds what came before it,
     * because a port left half-routed forwards nothing and looks like a
     * cable fault.
     */
    status = HB_CALL(bcm_vlan_default_get(sw->unit, &default_vlan));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    BCM_PBMP_CLEAR(pbmp);
    BCM_PBMP_PORT_ADD(pbmp, (bcm_port_t)logical_port);
    /* Not an error if it was not a member; the caller may have moved it. */
    (void)bcm_vlan_port_remove(sw->unit, default_vlan, pbmp);

    status = HB_CALL(bcm_vlan_create(sw->unit, (bcm_vlan_t)vlan_id));
    if (status != HEMLOCKBCM_OK) {
        goto restore_bridge;
    }
    BCM_PBMP_CLEAR(ubmp);
    BCM_PBMP_PORT_ADD(ubmp, (bcm_port_t)logical_port);
    status = HB_CALL(bcm_vlan_port_add(sw->unit, (bcm_vlan_t)vlan_id, pbmp, ubmp));
    if (status != HEMLOCKBCM_OK) {
        goto destroy_vlan;
    }
    status = HB_CALL(bcm_port_untagged_vlan_set(sw->unit, (bcm_port_t)logical_port,
                                                (bcm_vlan_t)vlan_id));
    if (status != HEMLOCKBCM_OK) {
        goto remove_member;
    }
    status = hb_l3_intf_create(sw, vlan_id, mac, rif);
    if (status != HEMLOCKBCM_OK) {
        goto restore_pvid;
    }
    return HEMLOCKBCM_OK;

restore_pvid:
    (void)bcm_port_untagged_vlan_set(sw->unit, (bcm_port_t)logical_port, default_vlan);
remove_member:
    (void)bcm_vlan_port_remove(sw->unit, (bcm_vlan_t)vlan_id, pbmp);
destroy_vlan:
    (void)bcm_vlan_destroy(sw->unit, (bcm_vlan_t)vlan_id);
restore_bridge:
    BCM_PBMP_CLEAR(ubmp);
    BCM_PBMP_PORT_ADD(ubmp, (bcm_port_t)logical_port);
    (void)bcm_vlan_port_add(sw->unit, default_vlan, pbmp, ubmp);
    (void)bcm_port_untagged_vlan_set(sw->unit, (bcm_port_t)logical_port, default_vlan);
    return status;
}

static int hb_rif_port_destroy(struct hemlockbcm_switch *sw, uint32_t logical_port,
                               uint16_t vlan_id, uint32_t rif)
{
    bcm_pbmp_t pbmp, ubmp;
    bcm_vlan_t default_vlan = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = HB_CALL(bcm_vlan_default_get(sw->unit, &default_vlan));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    /*
     * Undo in reverse, and keep going past a failure: this runs when the
     * operator has already said the interface is gone, and stopping
     * half way would leave the port in the routed VLAN with no
     * interface on it -- a port that forwards nothing at all. The first
     * failure is what gets reported.
     */
    status = hb_l3_intf_destroy(sw, rif);

    BCM_PBMP_CLEAR(pbmp);
    BCM_PBMP_PORT_ADD(pbmp, (bcm_port_t)logical_port);
    BCM_PBMP_CLEAR(ubmp);
    BCM_PBMP_PORT_ADD(ubmp, (bcm_port_t)logical_port);

    {
        int rv = bcm_vlan_port_remove(sw->unit, (bcm_vlan_t)vlan_id, pbmp);
        if (rv != BCM_E_NONE && rv != BCM_E_NOT_FOUND && status == HEMLOCKBCM_OK) {
            status = hb_status(rv);
        }
    }
    {
        int rv = bcm_vlan_destroy(sw->unit, (bcm_vlan_t)vlan_id);
        if (rv != BCM_E_NONE && rv != BCM_E_NOT_FOUND && status == HEMLOCKBCM_OK) {
            status = hb_status(rv);
        }
    }
    {
        int rv = bcm_vlan_port_add(sw->unit, default_vlan, pbmp, ubmp);
        if (rv != BCM_E_NONE && rv != BCM_E_EXISTS && status == HEMLOCKBCM_OK) {
            status = hb_status(rv);
        }
    }
    {
        int rv = bcm_port_untagged_vlan_set(sw->unit, (bcm_port_t)logical_port,
                                            default_vlan);
        if (rv != BCM_E_NONE && status == HEMLOCKBCM_OK) {
            status = hb_status(rv);
        }
    }
    return status;
}

static int hb_rif_vlan_create(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                              const uint8_t mac[6], uint32_t *rif)
{
    if (sw == NULL || mac == NULL || rif == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* The VLAN exists and keeps bridging; only the interface is new. */
    return hb_l3_intf_create(sw, vlan_id, mac, rif);
}

static int hb_rif_vlan_destroy(struct hemlockbcm_switch *sw, uint32_t rif)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return hb_l3_intf_destroy(sw, rif);
}

static int hb_my_mac_create(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                            const uint8_t mac[6], uint32_t *my_mac)
{
    bcm_l2_station_t station;
    int station_id = 0;
    int status;
    static const bcm_mac_t exact = { 0xff, 0xff, 0xff, 0xff, 0xff, 0xff };

    if (sw == NULL || mac == NULL || my_mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    bcm_l2_station_t_init(&station);
    sal_memcpy(station.dst_mac, mac, sizeof(station.dst_mac));
    sal_memcpy(station.dst_mac_mask, exact, sizeof(station.dst_mac_mask));
    if (vlan_id != 0) {
        station.vlan = (bcm_vlan_t)vlan_id;
        station.vlan_mask = 0xfff;
    }
    /*
     * Which protocols this MAC makes routable. IPv4 and ARP together:
     * without ARP the box answers no resolution for an address it
     * routes, which looks like a dead next hop rather than a missing
     * flag. IPv6 is left out while the datapath reports none.
     */
    station.flags = BCM_L2_STATION_IPV4 | BCM_L2_STATION_ARP_RARP;
    status = HB_CALL(bcm_l2_station_add(sw->unit, &station_id, &station));
    if (status == HEMLOCKBCM_OK) {
        *my_mac = (uint32_t)station_id;
    }
    return status;
}

static int hb_my_mac_destroy(struct hemlockbcm_switch *sw, uint32_t my_mac)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_l2_station_delete(sw->unit, (int)my_mac));
}

/* --- Routes (ABI 1.13) ----------------------------------------------------- */

static void hb_route_key(bcm_l3_route_t *route, uint32_t prefix, uint32_t mask)
{
    bcm_l3_route_t_init(route);
    route->l3a_subnet = (bcm_ip_t)prefix;
    route->l3a_ip_mask = (bcm_ip_t)mask;
}

static int hb_route_set(struct hemlockbcm_switch *sw, uint32_t prefix, uint32_t mask,
                        int kind, uint32_t rif)
{
    bcm_l3_route_t route;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    hb_route_key(&route, prefix, mask);
    switch (kind) {
    case HEMLOCKBCM_ROUTE_CPU:
        /*
         * L2TOCPU, not a copy: the packet goes to the CPU *unrouted*.
         * These are the switch's own addresses, and routing one would
         * decrement its TTL and rewrite its MACs on the way to a stack
         * that expects neither.
         */
        route.l3a_flags |= BCM_L3_L2TOCPU;
        break;
    case HEMLOCKBCM_ROUTE_RIF:
        route.l3a_intf = (bcm_if_t)rif;
        break;
    case HEMLOCKBCM_ROUTE_DROP:
        route.l3a_flags |= BCM_L3_DST_DISCARD;
        break;
    default:
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * Add is a replace when the prefix is already there, which is what
     * the caller wants: a route whose target changed must not need a
     * delete first, or the prefix would be unreachable in between.
     */
    route.l3a_flags |= BCM_L3_REPLACE;
    return HB_CALL(bcm_l3_route_add(sw->unit, &route));
}

static int hb_route_delete(struct hemlockbcm_switch *sw, uint32_t prefix, uint32_t mask)
{
    bcm_l3_route_t route;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    hb_route_key(&route, prefix, mask);
    return HB_CALL(bcm_l3_route_delete(sw->unit, &route));
}

/* --- Neighbours and next hops (ABI 1.14) ----------------------------------- */

/* The VLAN an L3 interface sits on. Needed to look a neighbour's MAC up
 * in the FDB, and read back rather than remembered. */
static int hb_rif_vlan(struct hemlockbcm_switch *sw, uint32_t rif, bcm_vlan_t *vlan)
{
    bcm_l3_intf_t intf;
    int status;

    bcm_l3_intf_t_init(&intf);
    intf.l3a_intf_id = (bcm_if_t)rif;
    status = HB_CALL(bcm_l3_intf_get(sw->unit, &intf));
    if (status == HEMLOCKBCM_OK) {
        *vlan = intf.l3a_vid;
    }
    return status;
}

/* The egress object filed under a neighbour's IP, or an error. */
static int hb_host_egress(struct hemlockbcm_switch *sw, uint32_t ip, bcm_if_t *egress)
{
    bcm_l3_host_t host;
    int status;

    bcm_l3_host_t_init(&host);
    host.l3a_ip_addr = (bcm_ip_t)ip;
    status = HB_CALL(bcm_l3_host_find(sw->unit, &host));
    if (status == HEMLOCKBCM_OK) {
        *egress = host.l3a_intf;
    }
    return status;
}

static int hb_neighbor_set(struct hemlockbcm_switch *sw, uint32_t rif, uint32_t ip,
                           const uint8_t mac[6])
{
    bcm_l3_egress_t egress;
    bcm_l3_host_t host;
    bcm_l2_addr_t l2addr;
    bcm_vlan_t vlan = 0;
    bcm_mac_t address;
    bcm_if_t egress_id = 0;
    int status;

    if (sw == NULL || mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_rif_vlan(sw, rif, &vlan);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }

    /*
     * Where the packet actually leaves. The chip's egress object carries
     * a port, and nothing the caller passes says which one -- so ask the
     * FDB where this MAC lives. An unlearned MAC has no answer, and a
     * guess would be a black hole: BCM_E_NOT_FOUND becomes "not found"
     * for the caller, which keeps the route pointed at the CPU until
     * the neighbour answers an ARP.
     */
    sal_memcpy(address, mac, sizeof(address));
    status = HB_CALL(bcm_l2_addr_get(sw->unit, address, vlan, &l2addr));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }

    bcm_l3_egress_t_init(&egress);
    egress.intf = (bcm_if_t)rif;
    sal_memcpy(egress.mac_addr, mac, sizeof(egress.mac_addr));
    egress.vlan = vlan;
    egress.module = l2addr.modid;
    if (l2addr.tgid != BCM_TRUNK_INVALID) {
        /* The neighbour is behind a LAG; the egress follows the trunk so
         * that hashing still spreads across its members. */
        egress.flags |= BCM_L3_TGID;
        egress.trunk = l2addr.tgid;
    } else {
        egress.port = l2addr.port;
    }
    status = HB_CALL(bcm_l3_egress_create(sw->unit, 0, &egress, &egress_id));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }

    /*
     * The host entry does two jobs: it makes the neighbour itself
     * reachable, and it is where the egress object is filed under the
     * neighbour's IP so a route can find it later. REPLACE so that a
     * neighbour whose MAC changed does not need deleting first.
     */
    bcm_l3_host_t_init(&host);
    host.l3a_ip_addr = (bcm_ip_t)ip;
    host.l3a_intf = egress_id;
    host.l3a_flags |= BCM_L3_REPLACE;
    sal_memcpy(host.l3a_nexthop_mac, mac, sizeof(host.l3a_nexthop_mac));
    status = HB_CALL(bcm_l3_host_add(sw->unit, &host));
    if (status != HEMLOCKBCM_OK) {
        /* No entry points at it, so it would leak silently. */
        (void)bcm_l3_egress_destroy(sw->unit, egress_id);
        return status;
    }
    return HEMLOCKBCM_OK;
}

static int hb_neighbor_clear(struct hemlockbcm_switch *sw, uint32_t rif, uint32_t ip)
{
    bcm_l3_host_t host;
    bcm_if_t egress_id = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    (void)rif;  /* the host table is keyed by IP alone */

    /* Read the egress object out before the entry that names it goes. */
    status = hb_host_egress(sw, ip, &egress_id);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    bcm_l3_host_t_init(&host);
    host.l3a_ip_addr = (bcm_ip_t)ip;
    status = HB_CALL(bcm_l3_host_delete(sw->unit, &host));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    return HB_CALL(bcm_l3_egress_destroy(sw->unit, egress_id));
}

static int hb_route_via_nexthop(struct hemlockbcm_switch *sw, uint32_t prefix,
                                uint32_t mask, uint32_t nexthop_ip)
{
    bcm_l3_route_t route;
    bcm_if_t egress_id = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Unresolved neighbour: nothing to forward through yet. */
    status = hb_host_egress(sw, nexthop_ip, &egress_id);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    hb_route_key(&route, prefix, mask);
    route.l3a_intf = egress_id;
    route.l3a_flags |= BCM_L3_REPLACE;
    return HB_CALL(bcm_l3_route_add(sw->unit, &route));
}

/* --- ECMP groups (ABI 1.15) ------------------------------------------------ */


static int hb_ecmp_create(struct hemlockbcm_switch *sw, uint32_t *group)
{
    bcm_if_t mpintf = 0;
    int status;

    if (sw == NULL || group == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Empty, with the width reserved: intf_count 0 and no array. */
    status = HB_CALL(bcm_l3_egress_multipath_max_create(sw->unit, 0, HB_ECMP_MAX_PATHS,
                                                        0, NULL, &mpintf));
    if (status == HEMLOCKBCM_OK) {
        *group = (uint32_t)mpintf;
    }
    return status;
}

static int hb_ecmp_destroy(struct hemlockbcm_switch *sw, uint32_t group)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_l3_egress_multipath_destroy(sw->unit, (bcm_if_t)group));
}

static int hb_ecmp_member_add(struct hemlockbcm_switch *sw, uint32_t group,
                              uint32_t nexthop_ip)
{
    bcm_if_t egress_id = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Unresolved neighbour: no object to add, and a group with a hole
     * in it would black-hole a share of the traffic. */
    status = hb_host_egress(sw, nexthop_ip, &egress_id);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    return HB_CALL(bcm_l3_egress_multipath_add(sw->unit, (bcm_if_t)group, egress_id));
}

static int hb_ecmp_member_remove(struct hemlockbcm_switch *sw, uint32_t group,
                                 uint32_t nexthop_ip)
{
    bcm_if_t egress_id = 0;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_host_egress(sw, nexthop_ip, &egress_id);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    return HB_CALL(bcm_l3_egress_multipath_delete(sw->unit, (bcm_if_t)group, egress_id));
}

static int hb_route_via_ecmp(struct hemlockbcm_switch *sw, uint32_t prefix,
                             uint32_t mask, uint32_t group)
{
    bcm_l3_route_t route;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    hb_route_key(&route, prefix, mask);
    /* MULTIPATH says the interface is a group rather than one egress
     * object; without it the route would follow whatever single object
     * happened to share that id. */
    route.l3a_flags |= BCM_L3_MULTIPATH | BCM_L3_REPLACE;
    route.l3a_intf = (bcm_if_t)group;
    return HB_CALL(bcm_l3_route_add(sw->unit, &route));
}

/* --- CoPP traps (ABI 1.16) ------------------------------------------------- */

/*
 * The trap entries live in a field group of their own, with a fixed id
 * so it can be found again without anything remembering it -- the same
 * derived-id idiom the trap entries themselves use. User ACL groups get
 * SDK-assigned ids counting up from zero, and entry ids likewise, so
 * the fixed ids sit far above both. Confirm on hardware that they are
 * inside the chip's valid ranges; a wrong base fails the first
 * trap_set loudly.
 */
#define HB_TRAP_GROUP_ID        100
#define HB_TRAP_GROUP_PRIO      1
#define HB_TRAP_ENTRY_BASE      0x8000
#define HB_TRAP_DEFAULT_OFFSET  0x100
#define HB_TRAP_PRIO_BASE       100
#define HB_TRAP_MAX_SUB         2

static bcm_field_entry_t hb_trap_entry_id(int kind, int is_default, int sub)
{
    return HB_TRAP_ENTRY_BASE + (is_default ? HB_TRAP_DEFAULT_OFFSET : 0)
           + kind * HB_TRAP_MAX_SUB + sub;
}

/* How many entries a kind needs: DHCP has two server ports, BGP two
 * directions. Everything else is one match. */
static int hb_trap_subentries(int kind)
{
    return (kind == HEMLOCKBCM_TRAP_DHCP || kind == HEMLOCKBCM_TRAP_BGP) ? 2 : 1;
}

/* Ensure the trap group exists; racing a previous creation is fine. */
static int hb_trap_group_ensure(struct hemlockbcm_switch *sw)
{
    bcm_field_qset_t qset;
    int rv;

    BCM_FIELD_QSET_INIT(qset);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyStageIngress);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyDstMac);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyEtherType);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyIpProtocol);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyL4SrcPort);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyL4DstPort);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyArpOpcode);
    BCM_FIELD_QSET_ADD(qset, bcmFieldQualifyDstIpLocal);
    rv = bcm_field_group_create_id(sw->unit, qset, HB_TRAP_GROUP_PRIO,
                                   HB_TRAP_GROUP_ID);
    if (rv == BCM_E_EXISTS) {
        return HEMLOCKBCM_OK;
    }
    return HB_CALL(rv);
}

/* One kind's match, on one of its entries. Where a MAC is matched it is
 * exact unless stated; every IPv4 protocol match also pins the
 * EtherType, so a parser that reuses the protocol field for IPv6
 * next-headers cannot alias it. */
static int hb_trap_qualify(struct hemlockbcm_switch *sw, bcm_field_entry_t entry,
                           int kind, int sub)
{
    static const bcm_mac_t exact = { 0xff, 0xff, 0xff, 0xff, 0xff, 0xff };
    static const bcm_mac_t stp_mac = { 0x01, 0x80, 0xc2, 0x00, 0x00, 0x00 };
    static const bcm_mac_t lacp_mac = { 0x01, 0x80, 0xc2, 0x00, 0x00, 0x02 };
    static const bcm_mac_t lldp_mac = { 0x01, 0x80, 0xc2, 0x00, 0x00, 0x0e };
    static const bcm_mac_t v6_mcast = { 0x33, 0x33, 0x00, 0x00, 0x00, 0x00 };
    static const bcm_mac_t v6_mcast_mask = { 0xff, 0xff, 0x00, 0x00, 0x00, 0x00 };
    bcm_mac_t mac, mask;
    int status;

#define HB_TQ(expr)                        \
    do {                                   \
        status = HB_CALL(expr);            \
        if (status != HEMLOCKBCM_OK) {     \
            return status;                 \
        }                                  \
    } while (0)

    switch (kind) {
    case HEMLOCKBCM_TRAP_IP2ME:
        /* Frames whose destination IP resolves to this switch. */
        HB_TQ(bcm_field_qualify_DstIpLocal(sw->unit, entry, 1, 1));
        break;
    case HEMLOCKBCM_TRAP_STP:
        sal_memcpy(mac, stp_mac, sizeof(mac));
        sal_memcpy(mask, exact, sizeof(mask));
        HB_TQ(bcm_field_qualify_DstMac(sw->unit, entry, mac, mask));
        break;
    case HEMLOCKBCM_TRAP_LACP:
        sal_memcpy(mac, lacp_mac, sizeof(mac));
        sal_memcpy(mask, exact, sizeof(mask));
        HB_TQ(bcm_field_qualify_DstMac(sw->unit, entry, mac, mask));
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x8809, 0xffff));
        break;
    case HEMLOCKBCM_TRAP_LLDP:
        sal_memcpy(mac, lldp_mac, sizeof(mac));
        sal_memcpy(mask, exact, sizeof(mask));
        HB_TQ(bcm_field_qualify_DstMac(sw->unit, entry, mac, mask));
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x88cc, 0xffff));
        break;
    case HEMLOCKBCM_TRAP_EAPOL:
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x888e, 0xffff));
        break;
    case HEMLOCKBCM_TRAP_IGMP_QUERY:
    case HEMLOCKBCM_TRAP_IGMP_LEAVE:
    case HEMLOCKBCM_TRAP_IGMP_V1_REPORT:
    case HEMLOCKBCM_TRAP_IGMP_V2_REPORT:
    case HEMLOCKBCM_TRAP_IGMP_V3_REPORT:
        /* All IGMP: the type byte is not honestly reachable. */
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x0800, 0xffff));
        HB_TQ(bcm_field_qualify_IpProtocol(sw->unit, entry, 2, 0xff));
        break;
    case HEMLOCKBCM_TRAP_MLD_V1_V2:
    case HEMLOCKBCM_TRAP_MLD_V1_REPORT:
    case HEMLOCKBCM_TRAP_MLD_V1_DONE:
    case HEMLOCKBCM_TRAP_MLD_V2_REPORT:
        /* IPv6 multicast: a superset that contains MLD. */
        sal_memcpy(mac, v6_mcast, sizeof(mac));
        sal_memcpy(mask, v6_mcast_mask, sizeof(mask));
        HB_TQ(bcm_field_qualify_DstMac(sw->unit, entry, mac, mask));
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x86dd, 0xffff));
        break;
    case HEMLOCKBCM_TRAP_ARP_REQUEST:
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x0806, 0xffff));
        HB_TQ(bcm_field_qualify_ArpOpcode(sw->unit, entry, bcmFieldArpOpcodeRequest));
        break;
    case HEMLOCKBCM_TRAP_ARP_RESPONSE:
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x0806, 0xffff));
        HB_TQ(bcm_field_qualify_ArpOpcode(sw->unit, entry, bcmFieldArpOpcodeReply));
        break;
    case HEMLOCKBCM_TRAP_DHCP:
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x0800, 0xffff));
        HB_TQ(bcm_field_qualify_IpProtocol(sw->unit, entry, 17, 0xff));
        HB_TQ(bcm_field_qualify_L4DstPort(sw->unit, entry, sub == 0 ? 67 : 68,
                                          0xffff));
        break;
    case HEMLOCKBCM_TRAP_OSPF:
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x0800, 0xffff));
        HB_TQ(bcm_field_qualify_IpProtocol(sw->unit, entry, 89, 0xff));
        break;
    case HEMLOCKBCM_TRAP_BGP:
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x0800, 0xffff));
        HB_TQ(bcm_field_qualify_IpProtocol(sw->unit, entry, 6, 0xff));
        if (sub == 0) {
            HB_TQ(bcm_field_qualify_L4DstPort(sw->unit, entry, 179, 0xffff));
        } else {
            HB_TQ(bcm_field_qualify_L4SrcPort(sw->unit, entry, 179, 0xffff));
        }
        break;
    case HEMLOCKBCM_TRAP_VRRP:
        HB_TQ(bcm_field_qualify_EtherType(sw->unit, entry, 0x0800, 0xffff));
        HB_TQ(bcm_field_qualify_IpProtocol(sw->unit, entry, 112, 0xff));
        break;
    default:
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
#undef HB_TQ
    return HEMLOCKBCM_OK;
}

static int hb_trap_set(struct hemlockbcm_switch *sw, int kind, int trap_only,
                       int is_default, uint32_t policer)
{
    int sub;
    int status;

    if (sw == NULL || kind < 0 || kind >= HEMLOCKBCM_TRAP_KIND_COUNT) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    status = hb_trap_group_ensure(sw);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    for (sub = 0; sub < hb_trap_subentries(kind); sub++) {
        bcm_field_entry_t entry = hb_trap_entry_id(kind, is_default, sub);

        /* A replace: whatever was there for this (kind, is_default)
         * goes first, so re-programming a class never duplicates. */
        (void)bcm_field_entry_destroy(sw->unit, entry);
        status = HB_CALL(bcm_field_entry_create_id(sw->unit, HB_TRAP_GROUP_ID, entry));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
        /* Deterministic overlap resolution: the higher kind wins, and
         * IP2ME is kind 0 so every protocol trap beats it. */
        status = HB_CALL(bcm_field_entry_prio_set(sw->unit, entry,
                                                  HB_TRAP_PRIO_BASE + kind));
        if (status == HEMLOCKBCM_OK) {
            status = hb_trap_qualify(sw, entry, kind, sub);
        }
        if (status == HEMLOCKBCM_OK) {
            status = HB_CALL(bcm_field_action_add(sw->unit, entry,
                                                  bcmFieldActionCopyToCpu, 0, 0));
        }
        if (status == HEMLOCKBCM_OK && trap_only) {
            /* A punt: the CPU copy is the only copy. */
            status = HB_CALL(bcm_field_action_add(sw->unit, entry,
                                                  bcmFieldActionDrop, 0, 0));
        }
        if (status == HEMLOCKBCM_OK && policer != 0) {
            status = HB_CALL(bcm_field_entry_policer_attach(
                sw->unit, entry, HB_ACL_POLICER_LEVEL, (bcm_policer_t)policer));
        }
        if (status == HEMLOCKBCM_OK) {
            status = HB_CALL(bcm_field_entry_install(sw->unit, entry));
        }
        if (status != HEMLOCKBCM_OK) {
            /* A half-built trap matches wrongly; take it back out. */
            (void)bcm_field_entry_destroy(sw->unit, entry);
            return status;
        }
    }
    return HEMLOCKBCM_OK;
}

static int hb_trap_clear(struct hemlockbcm_switch *sw, int kind, int is_default)
{
    int sub;
    int status = HEMLOCKBCM_OK;

    if (sw == NULL || kind < 0 || kind >= HEMLOCKBCM_TRAP_KIND_COUNT) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (sub = 0; sub < hb_trap_subentries(kind); sub++) {
        int rv = bcm_field_entry_destroy(
            sw->unit, hb_trap_entry_id(kind, is_default, sub));
        if (rv != BCM_E_NONE && status == HEMLOCKBCM_OK) {
            status = hb_status(rv);
        }
    }
    return status;
}

static int hb_trap_default_policer_set(struct hemlockbcm_switch *sw, uint32_t policer)
{
    int kind;
    int sub;
    int status;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /*
     * Walk the derived ids and probe the chip for which default-group
     * traps exist -- prio_get is the cheapest call that distinguishes
     * "no such entry" from an entry with no policer. The chip's entry
     * table is the membership list; nothing here remembers one.
     */
    for (kind = 0; kind < HEMLOCKBCM_TRAP_KIND_COUNT; kind++) {
        for (sub = 0; sub < hb_trap_subentries(kind); sub++) {
            bcm_field_entry_t entry = hb_trap_entry_id(kind, 1, sub);
            int prio = 0;

            if (bcm_field_entry_prio_get(sw->unit, entry, &prio) != BCM_E_NONE) {
                continue;  /* not installed */
            }
            (void)bcm_field_entry_policer_detach(sw->unit, entry,
                                                 HB_ACL_POLICER_LEVEL);
            if (policer != 0) {
                status = HB_CALL(bcm_field_entry_policer_attach(
                    sw->unit, entry, HB_ACL_POLICER_LEVEL,
                    (bcm_policer_t)policer));
                if (status != HEMLOCKBCM_OK) {
                    return status;
                }
            }
            status = HB_CALL(bcm_field_entry_install(sw->unit, entry));
            if (status != HEMLOCKBCM_OK) {
                return status;
            }
        }
    }
    return HEMLOCKBCM_OK;
}

/* --- Ingress sampling / sFlow (ABI 1.17) ------------------------------------ */

/*
 * Whether the sample delivery path (KNET filter + RX handler + RX
 * thread) is installed. File-static like hb_switch, and lost with the
 * process -- safely, because host_punt_setup's knet init wipes the
 * kernel-side filter on restart too, and the first enabling
 * sample_rate_set of the new run reinstalls both.
 */
static int hb_sample_delivery_up;

static bcm_rx_t hb_sample_rx(int unit, bcm_pkt_t *pkt, void *cookie)
{
    struct hemlockbcm_switch *sw = hb_switch;

    (void)unit;
    (void)cookie;
    if (sw == NULL || sw->sample_cb == NULL || pkt == NULL) {
        return BCM_RX_NOT_HANDLED;
    }
    /* Only sampled packets; everything else keeps its normal path. */
    if (!BCM_RX_REASON_GET(pkt->rx_reasons, bcmRxReasonSampleSource)) {
        return BCM_RX_NOT_HANDLED;
    }
    if (pkt->pkt_data != NULL && pkt->pkt_data[0].data != NULL) {
        uint32_t length = pkt->pkt_len;
        /* The block length is an int in the SDK's struct; clamp the
         * delivered bytes to what the first block really holds. */
        uint32_t block = pkt->pkt_data[0].len > 0 ? (uint32_t)pkt->pkt_data[0].len : 0;

        if (length > block) {
            length = block;
        }
        sw->sample_cb(sw->sample_ctx, (uint32_t)pkt->src_port,
                      (uint32_t)pkt->tot_len, pkt->pkt_data[0].data, length);
    }
    return BCM_RX_HANDLED;
}

static int hb_sample_delivery_ensure(struct hemlockbcm_switch *sw)
{
    bcm_knet_filter_t filter;
    int status;

    if (hb_sample_delivery_up) {
        return HEMLOCKBCM_OK;
    }
    /*
     * Priority 0: above the per-port punt filters (priority 1), so a
     * sampled copy goes to the RX API rather than up the ingress
     * port's netdev -- where the kernel stack would mistake it for
     * received traffic.
     */
    bcm_knet_filter_t_init(&filter);
    filter.type = BCM_KNET_FILTER_T_RX_PKT;
    filter.priority = 0;
    filter.dest_type = BCM_KNET_DEST_T_BCM_RX_API;
    filter.match_flags = BCM_KNET_FILTER_M_REASON;
    BCM_RX_REASON_SET(filter.m_reason, bcmRxReasonSampleSource);
    snprintf(filter.desc, sizeof(filter.desc), "hemlock-sflow");
    status = HB_CALL(bcm_knet_filter_create(sw->unit, &filter));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    status = HB_CALL(bcm_rx_register(sw->unit, "hemlock-sflow", hb_sample_rx,
                                     100, NULL, BCM_RCO_F_ALL_COS));
    if (status != HEMLOCKBCM_OK) {
        (void)bcm_knet_filter_destroy(sw->unit, filter.id);
        return status;
    }
    if (!bcm_rx_active(sw->unit)) {
        /* NULL config = the SDK's defaults. */
        status = HB_CALL(bcm_rx_start(sw->unit, NULL));
        if (status != HEMLOCKBCM_OK) {
            (void)bcm_rx_unregister(sw->unit, hb_sample_rx, 100);
            (void)bcm_knet_filter_destroy(sw->unit, filter.id);
            return status;
        }
    }
    hb_sample_delivery_up = 1;
    return HEMLOCKBCM_OK;
}

static int hb_set_sample_callback(struct hemlockbcm_switch *sw,
                                  hemlockbcm_sample_cb cb, void *context)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Registration only; the delivery path waits for the first
     * enabling sample_rate_set (see hb_sample_delivery_ensure). */
    sw->sample_cb = cb;
    sw->sample_ctx = context;
    return HEMLOCKBCM_OK;
}

static int hb_sample_rate_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              uint32_t rate)
{
    int status;

    if (sw == NULL || rate > (uint32_t)INT_MAX) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (rate != 0) {
        status = hb_sample_delivery_ensure(sw);
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
    }
    /* Ingress only; the egress rate stays off. */
    return HB_CALL(bcm_port_sample_rate_set(sw->unit, (bcm_port_t)logical_port,
                                            (int)rate, 0));
}

/* --- QoS (ABI 1.18) --------------------------------------------------------- */

static int hb_qos_dscp_tc_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              const uint8_t tc[64])
{
    int codepoint;
    int status;

    if (sw == NULL || tc == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (codepoint = 0; codepoint < 64; codepoint++) {
        /* mapcp = the same codepoint: classification only, no ingress
         * DSCP rewrite. */
        status = HB_CALL(bcm_port_dscp_map_set(sw->unit, (bcm_port_t)logical_port,
                                               codepoint, codepoint, tc[codepoint]));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
    }
    return HEMLOCKBCM_OK;
}

static int hb_qos_dscp_trust_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 int trust)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_dscp_map_mode_set(sw->unit, (bcm_port_t)logical_port,
                                              trust ? BCM_PORT_DSCP_MAP_ALL
                                                    : BCM_PORT_DSCP_MAP_NONE));
}

static int hb_qos_dot1p_tc_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                               const uint8_t tc[8])
{
    int pri;
    int cfi;
    int status;

    if (sw == NULL || tc == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (pri = 0; pri < 8; pri++) {
        for (cfi = 0; cfi < 2; cfi++) {
            /* Green regardless of CFI: drop precedence is WRED's job,
             * not the classifier's. */
            status = HB_CALL(bcm_port_vlan_priority_map_set(
                sw->unit, (bcm_port_t)logical_port, pri, cfi, tc[pri],
                bcmColorGreen));
            if (status != HEMLOCKBCM_OK) {
                return status;
            }
        }
    }
    return HEMLOCKBCM_OK;
}

static int hb_qos_default_tc_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 uint8_t tc)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return HB_CALL(bcm_port_untagged_priority_set(sw->unit, (bcm_port_t)logical_port,
                                                  tc));
}

static int hb_qos_queue_sched_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                  uint32_t queue, int strict, uint8_t weight)
{
    bcm_gport_t gport;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    BCM_GPORT_LOCAL_SET(gport, (bcm_port_t)logical_port);
    return HB_CALL(bcm_cosq_gport_sched_set(
        sw->unit, gport, (bcm_cos_queue_t)queue,
        strict ? BCM_COSQ_STRICT : BCM_COSQ_WEIGHTED_ROUND_ROBIN,
        strict ? 0 : (int)weight));
}

static int hb_qos_queue_shaper_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   uint32_t queue, uint64_t kbps)
{
    bcm_gport_t gport;

    if (sw == NULL || kbps > 0xffffffffull) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    BCM_GPORT_LOCAL_SET(gport, (bcm_port_t)logical_port);
    /* min 0: a ceiling, not a guarantee. max 0 = the SDK's "unshaped". */
    return HB_CALL(bcm_cosq_gport_bandwidth_set(sw->unit, gport,
                                                (bcm_cos_queue_t)queue, 0,
                                                (uint32)kbps, 0));
}

static int hb_qos_port_shaper_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                  uint64_t kbps)
{
    uint32 burst;

    if (sw == NULL || kbps > 0xffffffffull) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* The same burst reasoning as storm control: an eighth of the rate
     * is 125 ms of traffic -- smoothing, not the enforced rate. */
    burst = (uint32)(kbps / 8u);
    if (kbps != 0 && burst == 0) {
        burst = 1;
    }
    return HB_CALL(bcm_port_rate_egress_set(sw->unit, (bcm_port_t)logical_port,
                                            (uint32)kbps, burst));
}

static int hb_qos_queue_wred_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 uint32_t queue, uint32_t min_bytes, uint32_t max_bytes,
                                 uint8_t drop_probability, int ecn, int enable)
{
    bcm_cosq_gport_discard_t discard;
    bcm_gport_t gport;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    BCM_GPORT_LOCAL_SET(gport, (bcm_port_t)logical_port);
    sal_memset(&discard, 0, sizeof(discard));
    if (enable) {
        discard.flags = BCM_COSQ_DISCARD_ENABLE | BCM_COSQ_DISCARD_BYTES
                        | BCM_COSQ_DISCARD_COLOR_ALL;
        if (ecn) {
            discard.flags |= BCM_COSQ_DISCARD_MARK_CONGESTION;
        }
        discard.min_thresh = (int)min_bytes;
        discard.max_thresh = (int)max_bytes;
        discard.drop_probability = (int)drop_probability;
    }
    /* Disabled: an all-zero struct with no ENABLE flag. */
    return HB_CALL(bcm_cosq_gport_discard_set(sw->unit, gport,
                                              (bcm_cos_queue_t)queue, &discard));
}

static int hb_queue_counters_get(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 uint32_t queue, uint64_t *pkts, uint64_t *bytes,
                                 uint64_t *dropped_pkts, uint64_t *dropped_bytes)
{
    static const bcm_cosq_stat_t stats[4] = {
        bcmCosqStatOutPackets,
        bcmCosqStatOutBytes,
        bcmCosqStatDroppedPackets,
        bcmCosqStatDroppedBytes,
    };
    uint64_t *out[4];
    bcm_gport_t gport;
    int i;

    if (sw == NULL || pkts == NULL || bytes == NULL || dropped_pkts == NULL
        || dropped_bytes == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    out[0] = pkts;
    out[1] = bytes;
    out[2] = dropped_pkts;
    out[3] = dropped_bytes;
    BCM_GPORT_LOCAL_SET(gport, (bcm_port_t)logical_port);
    for (i = 0; i < 4; i++) {
        uint64 value;
        int status;

        COMPILER_64_ZERO(value);
        status = HB_CALL(bcm_cosq_stat_get(sw->unit, gport, (bcm_cos_queue_t)queue,
                                           stats[i], &value));
        if (status != HEMLOCKBCM_OK) {
            return status;
        }
        *out[i] = hb_u64(value);
    }
    return HEMLOCKBCM_OK;
}

/* --- Copper cable diagnostics (ABI 1.19) ------------------------------------ */

static int hb_cable_state(_shr_port_cable_state_t state)
{
    switch (state) {
    case _SHR_PORT_CABLE_STATE_OK:
        return HEMLOCKBCM_CABLE_OK;
    case _SHR_PORT_CABLE_STATE_OPEN:
        return HEMLOCKBCM_CABLE_OPEN;
    case _SHR_PORT_CABLE_STATE_SHORT:
    case _SHR_PORT_CABLE_STATE_OPENSHORT:
        /* Open-short is a wiring fault, not an unclassifiable result. */
        return HEMLOCKBCM_CABLE_SHORT;
    case _SHR_PORT_CABLE_STATE_CROSSTALK:
        return HEMLOCKBCM_CABLE_CROSSTALK;
    default:
        return HEMLOCKBCM_CABLE_UNKNOWN;
    }
}

static int hb_cable_diag(struct hemlockbcm_switch *sw, uint32_t logical_port,
                         int pair_state[4], uint32_t pair_length_m[4], uint32_t *pairs)
{
    bcm_port_cable_diag_t result;
    int count;
    int i;
    int status;

    if (sw == NULL || pair_state == NULL || pair_length_m == NULL || pairs == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sal_memset(&result, 0, sizeof(result));
    /* Blocking for as long as the PHY takes; the caller knew. On a
     * fibre port the PHY driver answers "unavailable", which reaches
     * the operator as unsupported -- the truth. */
    status = HB_CALL(bcm_port_cable_diag(sw->unit, (bcm_port_t)logical_port, &result));
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    count = result.npairs;
    if (count > 4) {
        count = 4;
    }
    if (count < 0) {
        count = 0;
    }
    for (i = 0; i < count; i++) {
        pair_state[i] = hb_cable_state(result.pair_state[i]);
        /* A negative length is "not measured". */
        pair_length_m[i] = result.pair_len[i] > 0 ? (uint32_t)result.pair_len[i] : 0;
    }
    *pairs = (uint32_t)count;
    return HEMLOCKBCM_OK;
}

static const struct hemlockbcm_api HB_API = {
    sizeof(struct hemlockbcm_api),
    HEMLOCKBCM_ABI_MAJOR,
    HEMLOCKBCM_ABI_MINOR,
    hb_create_switch,
    hb_destroy_switch,
    hb_set_link_callback,
    hb_ports,
    hb_set_port_admin_state,
    hb_set_port_speed,
    hb_set_port_duplex,
    hb_set_port_autoneg,
    hb_set_port_mtu,
    hb_port_counters,
    hb_capabilities,
    hb_load_led_program,
    hb_default_vlan,
    hb_create_vlan,
    hb_remove_vlan,
    hb_add_vlan_member,
    hb_remove_vlan_member,
    hb_set_port_pvid,
    hb_set_port_tpid,
    hb_set_fdb_aging,
    hb_add_fdb_entry,
    hb_remove_fdb_entry,
    hb_flush_fdb,
    hb_set_port_learning,
    hb_set_port_learn_limit,
    hb_lag_create,
    hb_lag_destroy,
    hb_lag_member_add,
    hb_lag_member_remove,
    hb_lag_member_state,
    hb_lag_vlan_member_add,
    hb_lag_vlan_member_remove,
    hb_lag_set_pvid,
    hb_stp_default,
    hb_stp_create,
    hb_stp_destroy,
    hb_stp_vlan_set,
    hb_stp_port_state,
    hb_lag_stp_port_state,
    hb_mirror_create,
    hb_mirror_destroy,
    hb_mirror_port_attach,
    hb_mirror_port_detach,
    hb_storm_control_set,
    hb_host_punt_setup,
    hb_hostif_create,
    hb_policer_create,
    hb_policer_set,
    hb_policer_destroy,
    hb_policer_stats,
    hb_acl_table_create,
    hb_acl_table_destroy,
    hb_acl_table_bind,
    hb_acl_table_unbind_all,
    hb_acl_entry_create,
    hb_acl_entry_action_set,
    hb_acl_entry_destroy,
    hb_acl_available,
    hb_acl_counter_create,
    hb_acl_counter_destroy,
    hb_acl_counter_get,
    hb_acl_entry_attach,
    hb_rif_port_create,
    hb_rif_port_destroy,
    hb_rif_vlan_create,
    hb_rif_vlan_destroy,
    hb_my_mac_create,
    hb_my_mac_destroy,
    hb_route_set,
    hb_route_delete,
    hb_neighbor_set,
    hb_neighbor_clear,
    hb_route_via_nexthop,
    hb_ecmp_create,
    hb_ecmp_destroy,
    hb_ecmp_member_add,
    hb_ecmp_member_remove,
    hb_route_via_ecmp,
    hb_trap_set,
    hb_trap_clear,
    hb_trap_default_policer_set,
    hb_set_sample_callback,
    hb_sample_rate_set,
    hb_qos_dscp_tc_set,
    hb_qos_dscp_trust_set,
    hb_qos_dot1p_tc_set,
    hb_qos_default_tc_set,
    hb_qos_queue_sched_set,
    hb_qos_queue_shaper_set,
    hb_qos_port_shaper_set,
    hb_qos_queue_wred_set,
    hb_queue_counters_get,
    hb_cable_diag,
    /* New slots are appended below this line, with the minor bumped. */
};

HEMLOCKBCM_EXPORT const struct hemlockbcm_api *hemlockbcm_get_api(uint32_t want_major)
{
    if (want_major != HEMLOCKBCM_ABI_MAJOR) {
        return NULL;
    }
    return &HB_API;
}
