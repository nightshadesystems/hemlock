/*
 * hemlockbcm_stub.c — an in-memory shim implementing hemlockbcm.h.
 *
 * SPDX-License-Identifier: MIT
 * Copyright (c) Nightshade Systems.
 *
 * The real shim is built inside a Broadcom OpenBCM tree and needs an ARM
 * cross toolchain, the SDK, and ultimately the hardware — so CI can never
 * exercise it. This stub exists so the *Rust* side is still tested: it
 * implements the same ABI over a fake 4-port switch, and
 * `OpenBcmBackend`'s tests dlopen it and round-trip every method.
 *
 * What it is for: proving the dlopen, the version handshake, the vtable
 * marshalling, the out-parameter conventions and the NULL-slot path.
 * What it is not: a simulator. It models nothing about a Helix4.
 *
 * Two slots are deliberately left NULL (`set_port_mtu` and
 * `destroy_switch`) so the tests cover a shim that does not implement
 * everything — the case every real shim is in until phase 6 finishes.
 *
 * Built by hemlock-sai's build.rs into OUT_DIR when the `openbcm`
 * feature is on; never shipped in an image.
 */

#include "hemlockbcm.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define STUB_PORTS 4

struct hemlockbcm_switch {
    struct hemlockbcm_port ports[STUB_PORTS];
    hemlockbcm_link_cb link_cb;
    void *link_ctx;
    int created;
};

static struct hemlockbcm_port *find_port(struct hemlockbcm_switch *sw, uint32_t logical)
{
    size_t i;
    if (sw == NULL) {
        return NULL;
    }
    for (i = 0; i < STUB_PORTS; i++) {
        if (sw->ports[i].logical_port == logical) {
            return &sw->ports[i];
        }
    }
    return NULL;
}

static int stub_create_switch(struct hemlockbcm_switch **out,
                              const struct hemlockbcm_init *init)
{
    struct hemlockbcm_switch *sw;
    size_t i;

    if (out == NULL || init == NULL || init->config_bcm_path == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw = (struct hemlockbcm_switch *)calloc(1, sizeof(*sw));
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    /* Two copper, two SFP+ — the shape of a real board, so the Rust
     * side's name/lane handling is exercised on both kinds. */
    for (i = 0; i < STUB_PORTS; i++) {
        sw->ports[i].logical_port = (uint32_t)(i + 1);
        sw->ports[i].speed_mbps = (i < 2) ? 1000u : 10000u;
        sw->ports[i].admin_up = 0;
        sw->ports[i].oper_up = 0;
        if (i < 2) {
            snprintf(sw->ports[i].name, HEMLOCKBCM_PORT_NAME_MAX, "ge%u", (unsigned)i);
        } else {
            snprintf(sw->ports[i].name, HEMLOCKBCM_PORT_NAME_MAX, "xe%u", (unsigned)(i - 2));
        }
    }
    sw->created = 1;
    *out = sw;
    return HEMLOCKBCM_OK;
}

static int stub_set_link_callback(struct hemlockbcm_switch *sw,
                                  hemlockbcm_link_cb cb, void *context)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->link_cb = cb;
    sw->link_ctx = context;
    return HEMLOCKBCM_OK;
}

static int stub_ports(struct hemlockbcm_switch *sw,
                      struct hemlockbcm_port *ports, size_t *count)
{
    if (sw == NULL || count == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* The documented two-call convention: too small a buffer writes
     * nothing, reports the requirement, and fails. */
    if (ports == NULL || *count < STUB_PORTS) {
        *count = STUB_PORTS;
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    memcpy(ports, sw->ports, sizeof(sw->ports));
    *count = STUB_PORTS;
    return HEMLOCKBCM_OK;
}

static int stub_set_port_admin_state(struct hemlockbcm_switch *sw,
                                     uint32_t logical_port, int up)
{
    struct hemlockbcm_port *p = find_port(sw, logical_port);
    if (p == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    p->admin_up = up ? 1 : 0;
    /* Link follows admin state, as the pure-Rust mock backend does, and
     * report it the way the real shim will — from the callback. */
    p->oper_up = p->admin_up;
    if (sw->link_cb != NULL) {
        sw->link_cb(sw->link_ctx, logical_port, p->oper_up);
    }
    return HEMLOCKBCM_OK;
}

static int stub_set_port_speed(struct hemlockbcm_switch *sw,
                               uint32_t logical_port, uint32_t speed_mbps)
{
    struct hemlockbcm_port *p = find_port(sw, logical_port);
    if (p == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    p->speed_mbps = speed_mbps;
    return HEMLOCKBCM_OK;
}

static int stub_set_port_duplex(struct hemlockbcm_switch *sw,
                                uint32_t logical_port, int full)
{
    (void)full;
    return find_port(sw, logical_port) ? HEMLOCKBCM_OK : HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
}

static int stub_set_port_autoneg(struct hemlockbcm_switch *sw,
                                 uint32_t logical_port, int on)
{
    (void)on;
    return find_port(sw, logical_port) ? HEMLOCKBCM_OK : HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
}

static int stub_port_counters(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              struct hemlockbcm_port_counters *out)
{
    struct hemlockbcm_port *p = find_port(sw, logical_port);
    if (p == NULL || out == NULL) {
        return p == NULL ? HEMLOCKBCM_ERR_ITEM_NOT_FOUND : HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    memset(out, 0, sizeof(*out));
    /* Distinct, port-derived values so the tests can tell the fields
     * apart and catch a mis-ordered struct. */
    out->in_octets = 1000u + logical_port;
    out->in_ucast_pkts = 10u + logical_port;
    out->out_octets = 2000u + logical_port;
    out->out_ucast_pkts = 20u + logical_port;
    out->in_crc_errors = logical_port;
    out->rx_bins[0] = 64u + logical_port;
    out->tx_bins[6] = 1523u + logical_port;
    return HEMLOCKBCM_OK;
}

static int stub_capabilities(struct hemlockbcm_switch *sw,
                             struct hemlockbcm_capabilities *out)
{
    if (sw == NULL || out == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    memset(out, 0, sizeof(*out));
    out->buffer_bytes_total = 4u * 1024u * 1024u;  /* Helix4's 4 MB */
    out->ecmp_width = 64;
    out->mirror_sessions_max = 0;                  /* phase 6 */
    out->ipv6 = 1;
    return HEMLOCKBCM_OK;
}

static const struct hemlockbcm_api STUB_API = {
    sizeof(struct hemlockbcm_api),
    HEMLOCKBCM_ABI_MAJOR,
    HEMLOCKBCM_ABI_MINOR,
    stub_create_switch,
    NULL,  /* destroy_switch: deliberately absent (NULL-slot coverage) */
    stub_set_link_callback,
    stub_ports,
    stub_set_port_admin_state,
    stub_set_port_speed,
    stub_set_port_duplex,
    stub_set_port_autoneg,
    NULL,  /* set_port_mtu: deliberately absent (NULL-slot coverage) */
    stub_port_counters,
    stub_capabilities,
};

HEMLOCKBCM_EXPORT const struct hemlockbcm_api *hemlockbcm_get_api(uint32_t want_major)
{
    if (want_major != HEMLOCKBCM_ABI_MAJOR) {
        return NULL;
    }
    return &STUB_API;
}
