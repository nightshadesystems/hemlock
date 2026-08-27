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
 * Every SDK symbol it uses was checked against sdk-6.5.16's headers
 * (include/bcm/{port,link,stat,error,vlan,l2,stack}.h, include/soc/drv.h)
 * — but
 * "checked against the header" is not "compiled", so treat the first
 * build as a review step, not a formality.
 */

#include "hemlockbcm.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* OpenBCM. sal/types.h must come first: it defines the integer types and
 * the COMPILER_64_* family everything else is written in terms of. */
#include <sal/core/libc.h>
#include <sal/types.h>

#include <bcm/error.h>
#include <bcm/init.h>
#include <bcm/l2.h>
#include <bcm/link.h>
#include <bcm/port.h>
#include <bcm/stack.h>
#include <bcm/stat.h>
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

struct hemlockbcm_switch {
    int unit;
    /* The local module id, which qualifies every L2 destination. Read
     * from the SDK on first use and cached; -1 means "not read yet". */
    int modid;
    hemlockbcm_link_cb link_cb;
    void *link_ctx;
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
    out->ecmp_width = 0;
    out->mirror_sessions_max = 0;
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
    /* Remaining phase 6 slots are appended below this line. */
};

HEMLOCKBCM_EXPORT const struct hemlockbcm_api *hemlockbcm_get_api(uint32_t want_major)
{
    if (want_major != HEMLOCKBCM_ABI_MAJOR) {
        return NULL;
    }
    return &HB_API;
}
