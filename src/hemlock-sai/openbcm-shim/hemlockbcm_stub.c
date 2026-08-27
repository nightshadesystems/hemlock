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
#define STUB_VLANS 8
#define STUB_DEFAULT_VLAN 1

/*
 * Enough VLAN state to be worth testing against: which VLANs exist, who
 * is a member, and whether that membership is tagged. A fixed table,
 * because the point is to exercise the marshalling and the caller's
 * idempotency rules, not to be a switch.
 */
struct stub_vlan {
    uint16_t vlan_id;               /* 0 = free slot */
    uint32_t members[STUB_PORTS];   /* logical port, 0 = free */
    int tagged[STUB_PORTS];
};

struct hemlockbcm_switch {
    struct hemlockbcm_port ports[STUB_PORTS];
    struct stub_vlan vlans[STUB_VLANS];
    uint16_t pvid[STUB_PORTS];
    uint16_t tpid[STUB_PORTS];
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
    /* Every port starts an untagged member of the default VLAN with a
     * matching PVID, which is where a real chip comes up too. */
    sw->vlans[0].vlan_id = STUB_DEFAULT_VLAN;
    for (i = 0; i < STUB_PORTS; i++) {
        sw->vlans[0].members[i] = sw->ports[i].logical_port;
        sw->vlans[0].tagged[i] = 0;
        sw->pvid[i] = STUB_DEFAULT_VLAN;
        sw->tpid[i] = 0x8100;
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

/* ABI 1.1. Records the program so the Rust tests can prove it arrived. */
static char stub_led_program[512];

static int stub_load_led_program(struct hemlockbcm_switch *sw, const char *hex)
{
    if (sw == NULL || hex == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    snprintf(stub_led_program, sizeof(stub_led_program), "%s", hex);
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI. */
HEMLOCKBCM_EXPORT const char *hemlockbcm_stub_led_program(void)
{
    return stub_led_program;
}

/* --- L2 VLANs (ABI 1.2) ------------------------------------------------- */

static struct stub_vlan *find_vlan(struct hemlockbcm_switch *sw, uint16_t vlan_id)
{
    size_t i;
    for (i = 0; i < STUB_VLANS; i++) {
        if (sw->vlans[i].vlan_id == vlan_id) {
            return &sw->vlans[i];
        }
    }
    return NULL;
}

/* Index of `logical` in a VLAN's member list, or -1. */
static int vlan_member_index(const struct stub_vlan *vlan, uint32_t logical)
{
    size_t i;
    for (i = 0; i < STUB_PORTS; i++) {
        if (vlan->members[i] == logical) {
            return (int)i;
        }
    }
    return -1;
}

static int vlan_has_members(const struct stub_vlan *vlan)
{
    size_t i;
    for (i = 0; i < STUB_PORTS; i++) {
        if (vlan->members[i] != 0) {
            return 1;
        }
    }
    return 0;
}

static int stub_default_vlan(struct hemlockbcm_switch *sw, uint16_t *out)
{
    if (sw == NULL || out == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    *out = STUB_DEFAULT_VLAN;
    return HEMLOCKBCM_OK;
}

static int stub_create_vlan(struct hemlockbcm_switch *sw, uint16_t vlan_id)
{
    size_t i;

    if (sw == NULL || vlan_id == 0 || vlan_id > 4094) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (find_vlan(sw, vlan_id) != NULL) {
        return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
    }
    for (i = 0; i < STUB_VLANS; i++) {
        if (sw->vlans[i].vlan_id == 0) {
            memset(&sw->vlans[i], 0, sizeof(sw->vlans[i]));
            sw->vlans[i].vlan_id = vlan_id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_remove_vlan(struct hemlockbcm_switch *sw, uint16_t vlan_id)
{
    struct stub_vlan *vlan;

    if (sw == NULL || vlan_id == STUB_DEFAULT_VLAN) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* The SDK refuses to destroy a VLAN that still has members. */
    if (vlan_has_members(vlan)) {
        return HEMLOCKBCM_ERR_FAILURE;
    }
    memset(vlan, 0, sizeof(*vlan));
    return HEMLOCKBCM_OK;
}

static int stub_add_vlan_member(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                uint32_t logical_port, int tagged)
{
    struct stub_vlan *vlan;
    size_t i;

    if (sw == NULL || find_port(sw, logical_port) == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    if (vlan_member_index(vlan, logical_port) >= 0) {
        return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
    }
    for (i = 0; i < STUB_PORTS; i++) {
        if (vlan->members[i] == 0) {
            vlan->members[i] = logical_port;
            vlan->tagged[i] = tagged ? 1 : 0;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_remove_vlan_member(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                   uint32_t logical_port)
{
    struct stub_vlan *vlan;
    int index;

    /* Validate the port, like the add path does: the caller reads
     * ITEM_NOT_FOUND as "already not a member" and reports success, so a
     * port that does not exist must not answer with it. */
    if (sw == NULL || find_port(sw, logical_port) == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    index = vlan_member_index(vlan, logical_port);
    if (index < 0) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    vlan->members[index] = 0;
    vlan->tagged[index] = 0;
    return HEMLOCKBCM_OK;
}

static int stub_set_port_pvid(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              uint16_t vlan_id)
{
    struct hemlockbcm_port *port;

    if (sw == NULL || vlan_id == 0 || vlan_id > 4094) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->pvid[port - sw->ports] = vlan_id;
    return HEMLOCKBCM_OK;
}

static int stub_set_port_tpid(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              uint16_t tpid)
{
    struct hemlockbcm_port *port;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->tpid[port - sw->ports] = tpid;
    return HEMLOCKBCM_OK;
}

/*
 * Test hooks, not part of the ABI: they let the Rust tests assert on what
 * a call actually did rather than only on its status code.
 */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_vlan_member(struct hemlockbcm_switch *sw,
                                                  uint16_t vlan_id,
                                                  uint32_t logical_port)
{
    struct stub_vlan *vlan;
    int index;

    if (sw == NULL) {
        return -1;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return -1;
    }
    index = vlan_member_index(vlan, logical_port);
    if (index < 0) {
        return -1;  /* not a member */
    }
    return vlan->tagged[index];  /* 0 = untagged member, 1 = tagged */
}

HEMLOCKBCM_EXPORT uint16_t hemlockbcm_stub_pvid(struct hemlockbcm_switch *sw,
                                                uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);
    return port == NULL ? 0 : sw->pvid[port - sw->ports];
}

HEMLOCKBCM_EXPORT uint16_t hemlockbcm_stub_tpid(struct hemlockbcm_switch *sw,
                                                uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);
    return port == NULL ? 0 : sw->tpid[port - sw->ports];
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
    stub_load_led_program,
    stub_default_vlan,
    stub_create_vlan,
    stub_remove_vlan,
    stub_add_vlan_member,
    stub_remove_vlan_member,
    stub_set_port_pvid,
    stub_set_port_tpid,
};

HEMLOCKBCM_EXPORT const struct hemlockbcm_api *hemlockbcm_get_api(uint32_t want_major)
{
    if (want_major != HEMLOCKBCM_ABI_MAJOR) {
        return NULL;
    }
    return &STUB_API;
}
