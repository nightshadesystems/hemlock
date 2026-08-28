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
#define STUB_FDB 8
#define STUB_LAGS 2
#define STUB_STGS 2
#define STUB_MIRRORS 2
#define STUB_POLICERS 4
#define STUB_ACL_TABLES 2
#define STUB_ACL_ENTRIES 8
#define STUB_ACL_COUNTERS 4
#define STUB_RIFS 4
#define STUB_MY_MACS 4
#define STUB_ROUTES 8
#define STUB_NEIGHBORS 4
/* Beyond the ABI's three route kinds: a route through a next hop. */
#define STUB_ROUTE_NEXTHOP 3
#define STUB_ROUTE_ECMP 4
#define STUB_ECMP_GROUPS 2
#define STUB_ECMP_MEMBERS 4
/* The SDK's own default spanning-tree group id. */
#define STUB_DEFAULT_STG 1

/*
 * Enough VLAN state to be worth testing against: which VLANs exist, who
 * is a member, and whether that membership is tagged. A fixed table,
 * because the point is to exercise the marshalling and the caller's
 * idempotency rules, not to be a switch.
 */
struct stub_vlan {
    uint16_t vlan_id;               /* 0 = free slot */
    /* An explicit used flag rather than "port 0 means empty": logical
     * port 0 is a real port on this hardware, and a sentinel here would
     * report the first free slot as a member of it. */
    int member_used[STUB_PORTS];
    uint32_t members[STUB_PORTS];
    int tagged[STUB_PORTS];
    /* Trunk members of the VLAN. Separate from the port members because
     * the hardware reaches a trunk through a different call, and because
     * trunk id 0 is valid and so cannot double as "empty". */
    int lag_used[STUB_LAGS];
    uint32_t lags[STUB_LAGS];
    int lag_tagged[STUB_LAGS];
    /* Exactly one spanning-tree group holds a VLAN at any time. */
    uint32_t stg;
};

/* An ACL table (a field group) and one of its entries. */
struct stub_acl_table {
    int used;
    uint32_t id;
    int egress;
    int bound[STUB_PORTS];
};

struct stub_acl_entry {
    int used;
    uint32_t id;
    uint32_t table;
    uint32_t priority;
    int action;
    uint32_t counter;               /* 0 = none */
    uint32_t policer;               /* 0 = none */
    struct hemlockbcm_acl_fields fields;
};

/* An L3 interface, and a My-MAC station entry. */
struct stub_rif {
    int used;
    uint32_t id;
    uint16_t vlan_id;
    uint32_t port;                  /* 0 for an SVI */
    uint8_t mac[6];
};

struct stub_my_mac {
    int used;
    uint32_t id;
    uint16_t vlan_id;               /* 0 = any VLAN */
    uint8_t mac[6];
};

/* One route on the default virtual router. */
struct stub_route {
    int used;
    uint32_t prefix;
    uint32_t mask;
    int kind;
    uint32_t rif;                   /* 0 unless the kind is RIF */
    uint32_t nexthop_ip;            /* 0 unless the kind is NEXTHOP */
};

/* One queue's WRED curve, as programmed. */
struct stub_wred {
    int enable;
    int ecn;
    uint32_t min_bytes;
    uint32_t max_bytes;
    uint8_t drop_probability;
};

/* One installed protocol trap: (kind, is_default) -> what trap_set put
 * there. */
struct stub_trap {
    int used;
    int trap_only;
    uint32_t policer;
};

/* An ECMP group and the next hops it spreads across. */
struct stub_ecmp {
    int used;
    uint32_t id;
    int member_used[STUB_ECMP_MEMBERS];
    uint32_t members[STUB_ECMP_MEMBERS];
};

/* A resolved neighbour: an IP, the interface it is on, and its MAC. */
struct stub_neighbor {
    int used;
    uint32_t ip;
    uint32_t rif;
    uint8_t mac[6];
};

/* A match counter, which belongs to one table. */
struct stub_acl_counter {
    int used;
    uint32_t id;
    uint32_t table;
    uint64_t packets;
};

/* A single-rate policer. */
struct stub_policer {
    int used;
    uint32_t id;
    int pps;
    uint64_t rate;
    uint64_t burst;
};

/* A port's KNET netdev. */
struct stub_hostif {
    int used;
    uint32_t id;
    char name[16];
};

/* A local (SPAN) mirror session. */
struct stub_mirror {
    int used;
    uint32_t session;
    uint32_t monitor;
};

/* A spanning-tree group other than the default one. */
struct stub_stg {
    int used;
    uint32_t stg;
    int port_state[STUB_PORTS];
};

/*
 * A trunk. Members stay in it whether or not they are forwarding, so
 * that a gated-closed member still picks up the trunk's ingress
 * classification -- which is exactly what `pvid` here exists to test.
 */
struct stub_lag {
    int used;
    uint32_t tid;
    int member_used[STUB_PORTS];
    uint32_t members[STUB_PORTS];
    int member_enabled[STUB_PORTS];
    uint16_t pvid;                  /* 0 = never set */
};

/*
 * One MAC table entry. `is_static` is the distinction a flush turns on:
 * the ABI's flush drops dynamic entries and leaves static ones, and only
 * the stub's `hemlockbcm_stub_learn` hook creates dynamic ones, since on
 * real hardware the chip does that itself.
 */
struct stub_fdb {
    int used;
    int is_static;
    uint16_t vlan_id;
    uint8_t mac[6];
    uint32_t logical_port;
    int discard;
};

struct hemlockbcm_switch {
    struct hemlockbcm_port ports[STUB_PORTS];
    struct stub_vlan vlans[STUB_VLANS];
    struct stub_fdb fdb[STUB_FDB];
    struct stub_lag lags[STUB_LAGS];
    struct stub_stg stgs[STUB_STGS];
    struct stub_mirror mirrors[STUB_MIRRORS];
    /* The session each direction of each port feeds; 0 = none. */
    uint32_t mirror_in[STUB_PORTS];
    uint32_t mirror_out[STUB_PORTS];
    /* Metered rate per port per storm class; 0 = no limit. */
    uint32_t storm_kbps[STUB_PORTS][3];
    struct stub_hostif hostifs[STUB_PORTS];
    int punt_ready;
    struct stub_policer policers[STUB_POLICERS];
    struct stub_acl_table acl_tables[STUB_ACL_TABLES];
    struct stub_acl_entry acl_entries[STUB_ACL_ENTRIES];
    struct stub_acl_counter acl_counters[STUB_ACL_COUNTERS];
    struct stub_rif rifs[STUB_RIFS];
    struct stub_my_mac my_macs[STUB_MY_MACS];
    struct stub_route routes[STUB_ROUTES];
    struct stub_neighbor neighbors[STUB_NEIGHBORS];
    struct stub_ecmp ecmps[STUB_ECMP_GROUPS];
    struct stub_trap traps[HEMLOCKBCM_TRAP_KIND_COUNT][2];
    /* The default group is always there, so its per-port state lives
       here rather than in the table above. */
    int default_stg_state[STUB_PORTS];
    uint16_t pvid[STUB_PORTS];
    uint16_t tpid[STUB_PORTS];
    uint32_t fdb_aging;
    int learning[STUB_PORTS];
    int learn_limit[STUB_PORTS];
    hemlockbcm_link_cb link_cb;
    void *link_ctx;
    hemlockbcm_sample_cb sample_cb;
    void *sample_ctx;
    uint32_t sample_rate[STUB_PORTS];
    uint8_t dscp_tc[STUB_PORTS][64];
    int dscp_trust[STUB_PORTS];
    uint8_t dot1p_tc[STUB_PORTS][8];
    uint8_t default_tc[STUB_PORTS];
    int sched_strict[STUB_PORTS][8];
    uint8_t sched_weight[STUB_PORTS][8];
    uint64_t queue_shaper[STUB_PORTS][8];
    uint64_t port_shaper[STUB_PORTS];
    struct stub_wred wred[STUB_PORTS][8];
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
    sw->vlans[0].stg = STUB_DEFAULT_STG;
    for (i = 0; i < STUB_PORTS; i++) {
        sw->vlans[0].member_used[i] = 1;
        sw->vlans[0].members[i] = sw->ports[i].logical_port;
        sw->vlans[0].tagged[i] = 0;
        sw->pvid[i] = STUB_DEFAULT_VLAN;
        sw->tpid[i] = 0x8100;
        sw->learning[i] = 1;    /* learning on, like a chip out of reset */
        sw->default_stg_state[i] = HEMLOCKBCM_STP_FORWARDING;
        sw->learn_limit[i] = -1;
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
    /* What this stub can actually hold. A test double that claims a
     * width it cannot serve is the kind of lie the tests exist to
     * catch. */
    out->ecmp_width = STUB_ECMP_MEMBERS;
    out->mirror_sessions_max = STUB_MIRRORS;                  /* phase 6 */
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
        if (vlan->member_used[i] && vlan->members[i] == logical) {
            return (int)i;
        }
    }
    return -1;
}

static int vlan_has_members(const struct stub_vlan *vlan)
{
    size_t i;
    for (i = 0; i < STUB_PORTS; i++) {
        if (vlan->member_used[i]) {
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
            sw->vlans[i].stg = STUB_DEFAULT_STG;
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
        if (!vlan->member_used[i]) {
            vlan->member_used[i] = 1;
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
    vlan->member_used[index] = 0;
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

/* --- MAC address table (ABI 1.3) ---------------------------------------- */

static struct stub_fdb *find_fdb(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                 const uint8_t mac[6])
{
    size_t i;
    for (i = 0; i < STUB_FDB; i++) {
        if (sw->fdb[i].used && sw->fdb[i].vlan_id == vlan_id &&
            memcmp(sw->fdb[i].mac, mac, 6) == 0) {
            return &sw->fdb[i];
        }
    }
    return NULL;
}

static int stub_set_fdb_aging(struct hemlockbcm_switch *sw, uint32_t secs)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->fdb_aging = secs;
    return HEMLOCKBCM_OK;
}

static int stub_add_fdb_entry(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                              const uint8_t mac[6], uint32_t logical_port, int discard)
{
    struct stub_fdb *entry;
    size_t i;

    if (sw == NULL || mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* A forwarding entry needs a real port; a black hole does not. */
    if (!discard && find_port(sw, logical_port) == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    entry = find_fdb(sw, vlan_id, mac);
    if (entry == NULL) {
        for (i = 0; i < STUB_FDB; i++) {
            if (!sw->fdb[i].used) {
                entry = &sw->fdb[i];
                break;
            }
        }
    }
    if (entry == NULL) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    /* Overwrites in place, which is the ABI's documented "replaces". */
    entry->used = 1;
    entry->is_static = 1;
    entry->vlan_id = vlan_id;
    memcpy(entry->mac, mac, 6);
    entry->logical_port = discard ? 0 : logical_port;
    entry->discard = discard ? 1 : 0;
    return HEMLOCKBCM_OK;
}

static int stub_remove_fdb_entry(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                 const uint8_t mac[6])
{
    struct stub_fdb *entry;

    if (sw == NULL || mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    entry = find_fdb(sw, vlan_id, mac);
    if (entry == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    memset(entry, 0, sizeof(*entry));
    return HEMLOCKBCM_OK;
}

static int stub_flush_fdb(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                          uint32_t logical_port, uint32_t flags)
{
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_FDB; i++) {
        struct stub_fdb *entry = &sw->fdb[i];

        if (!entry->used || entry->is_static) {
            continue;  /* static entries survive a flush */
        }
        if ((flags & HEMLOCKBCM_FLUSH_VLAN) && entry->vlan_id != vlan_id) {
            continue;
        }
        if ((flags & HEMLOCKBCM_FLUSH_PORT) && entry->logical_port != logical_port) {
            continue;
        }
        memset(entry, 0, sizeof(*entry));
    }
    return HEMLOCKBCM_OK;
}

static int stub_set_port_learning(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                  int learn)
{
    struct hemlockbcm_port *port;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->learning[port - sw->ports] = learn ? 1 : 0;
    return HEMLOCKBCM_OK;
}

static int stub_set_port_learn_limit(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                     int limit)
{
    struct hemlockbcm_port *port;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->learn_limit[port - sw->ports] = limit;
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

/* Seed a dynamic (learned) entry, which no ABI call can create -- the
 * chip learns those itself. The flush tests need some to flush. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_learn(struct hemlockbcm_switch *sw,
                                            uint16_t vlan_id, const uint8_t mac[6],
                                            uint32_t logical_port)
{
    size_t i;

    if (sw == NULL || mac == NULL) {
        return -1;
    }
    for (i = 0; i < STUB_FDB; i++) {
        if (!sw->fdb[i].used) {
            sw->fdb[i].used = 1;
            sw->fdb[i].is_static = 0;
            sw->fdb[i].vlan_id = vlan_id;
            memcpy(sw->fdb[i].mac, mac, 6);
            sw->fdb[i].logical_port = logical_port;
            sw->fdb[i].discard = 0;
            return 0;
        }
    }
    return -1;
}

/* -1 = absent, 0 = present and forwarding, 1 = present and discarding. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_fdb_entry(struct hemlockbcm_switch *sw,
                                                uint16_t vlan_id, const uint8_t mac[6])
{
    struct stub_fdb *entry;

    if (sw == NULL || mac == NULL) {
        return -1;
    }
    entry = find_fdb(sw, vlan_id, mac);
    return entry == NULL ? -1 : entry->discard;
}

HEMLOCKBCM_EXPORT uint32_t hemlockbcm_stub_fdb_count(struct hemlockbcm_switch *sw)
{
    uint32_t count = 0;
    size_t i;

    if (sw != NULL) {
        for (i = 0; i < STUB_FDB; i++) {
            count += sw->fdb[i].used ? 1u : 0u;
        }
    }
    return count;
}

HEMLOCKBCM_EXPORT uint32_t hemlockbcm_stub_fdb_aging(struct hemlockbcm_switch *sw)
{
    return sw == NULL ? 0 : sw->fdb_aging;
}

HEMLOCKBCM_EXPORT int hemlockbcm_stub_learning(struct hemlockbcm_switch *sw,
                                               uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);
    return port == NULL ? -1 : sw->learning[port - sw->ports];
}

HEMLOCKBCM_EXPORT int hemlockbcm_stub_learn_limit(struct hemlockbcm_switch *sw,
                                                  uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);
    return port == NULL ? -2 : sw->learn_limit[port - sw->ports];
}

/* --- Link aggregation (ABI 1.4) ----------------------------------------- */

static struct stub_lag *find_lag(struct hemlockbcm_switch *sw, uint32_t tid)
{
    size_t i;
    for (i = 0; i < STUB_LAGS; i++) {
        if (sw->lags[i].used && sw->lags[i].tid == tid) {
            return &sw->lags[i];
        }
    }
    return NULL;
}

static int lag_member_index(const struct stub_lag *lag, uint32_t logical_port)
{
    size_t i;
    for (i = 0; i < STUB_PORTS; i++) {
        if (lag->member_used[i] && lag->members[i] == logical_port) {
            return (int)i;
        }
    }
    return -1;
}

static int stub_lag_create(struct hemlockbcm_switch *sw, uint32_t *tid)
{
    size_t i;

    if (sw == NULL || tid == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_LAGS; i++) {
        if (!sw->lags[i].used) {
            memset(&sw->lags[i], 0, sizeof(sw->lags[i]));
            sw->lags[i].used = 1;
            /* Trunk ids start at 0 on real hardware, so 0 is a valid
             * trunk and must not read as "no trunk" anywhere above. */
            sw->lags[i].tid = (uint32_t)i;
            *tid = sw->lags[i].tid;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_lag_destroy(struct hemlockbcm_switch *sw, uint32_t tid)
{
    struct stub_lag *lag;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    lag = find_lag(sw, tid);
    if (lag == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_PORTS; i++) {
        if (lag->member_used[i]) {
            return HEMLOCKBCM_ERR_FAILURE;  /* members must be gone first */
        }
    }
    memset(lag, 0, sizeof(*lag));
    return HEMLOCKBCM_OK;
}

static int stub_lag_member_add(struct hemlockbcm_switch *sw, uint32_t tid,
                               uint32_t logical_port, int enabled)
{
    struct stub_lag *lag;
    size_t i;

    if (sw == NULL || find_port(sw, logical_port) == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    lag = find_lag(sw, tid);
    if (lag == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    if (lag_member_index(lag, logical_port) >= 0) {
        return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
    }
    for (i = 0; i < STUB_PORTS; i++) {
        if (!lag->member_used[i]) {
            lag->member_used[i] = 1;
            lag->members[i] = logical_port;
            lag->member_enabled[i] = enabled ? 1 : 0;
            /* A member joining picks up the trunk's ingress
             * classification, the same way lag_set_pvid applies it to
             * the members already there. The real shim has no trunk-wide
             * PVID to read, so it takes the same value off a member
             * already in the trunk; the observable result is identical,
             * which is what the ABI specifies. */
            if (lag->pvid != 0) {
                struct hemlockbcm_port *port = find_port(sw, logical_port);
                sw->pvid[port - sw->ports] = lag->pvid;
            }
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_lag_member_remove(struct hemlockbcm_switch *sw, uint32_t tid,
                                  uint32_t logical_port)
{
    struct stub_lag *lag;
    int index;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    lag = find_lag(sw, tid);
    if (lag == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    index = lag_member_index(lag, logical_port);
    if (index < 0) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    lag->member_used[index] = 0;
    lag->members[index] = 0;
    lag->member_enabled[index] = 0;
    return HEMLOCKBCM_OK;
}

static int stub_lag_member_state(struct hemlockbcm_switch *sw, uint32_t tid,
                                 uint32_t logical_port, int enabled)
{
    struct stub_lag *lag;
    int index;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    lag = find_lag(sw, tid);
    if (lag == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    index = lag_member_index(lag, logical_port);
    if (index < 0) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    lag->member_enabled[index] = enabled ? 1 : 0;
    return HEMLOCKBCM_OK;
}

static int stub_lag_vlan_member_add(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                    uint32_t tid, int tagged)
{
    struct stub_vlan *vlan;
    size_t i;

    if (sw == NULL || find_lag(sw, tid) == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_LAGS; i++) {
        if (vlan->lag_used[i] && vlan->lags[i] == tid) {
            return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
        }
    }
    for (i = 0; i < STUB_LAGS; i++) {
        if (!vlan->lag_used[i]) {
            vlan->lag_used[i] = 1;
            vlan->lags[i] = tid;
            vlan->lag_tagged[i] = tagged ? 1 : 0;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_lag_vlan_member_remove(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                       uint32_t tid)
{
    struct stub_vlan *vlan;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_LAGS; i++) {
        if (vlan->lag_used[i] && vlan->lags[i] == tid) {
            vlan->lag_used[i] = 0;
            vlan->lags[i] = 0;
            vlan->lag_tagged[i] = 0;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
}

static int stub_lag_set_pvid(struct hemlockbcm_switch *sw, uint32_t tid, uint16_t vlan_id)
{
    struct stub_lag *lag;
    size_t i;

    if (sw == NULL || vlan_id == 0 || vlan_id > 4094) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    lag = find_lag(sw, tid);
    if (lag == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    lag->pvid = vlan_id;
    /* Applied to every member, gated-closed ones included -- which is
     * why a gated member stays in the trunk rather than leaving it. */
    for (i = 0; i < STUB_PORTS; i++) {
        if (lag->member_used[i]) {
            struct hemlockbcm_port *port = find_port(sw, lag->members[i]);
            if (port != NULL) {
                sw->pvid[port - sw->ports] = vlan_id;
            }
        }
    }
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

/* -1 = not a member, 0 = member gated closed, 1 = member forwarding. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_lag_member(struct hemlockbcm_switch *sw,
                                                 uint32_t tid, uint32_t logical_port)
{
    struct stub_lag *lag;
    int index;

    if (sw == NULL) {
        return -1;
    }
    lag = find_lag(sw, tid);
    if (lag == NULL) {
        return -1;
    }
    index = lag_member_index(lag, logical_port);
    return index < 0 ? -1 : lag->member_enabled[index];
}

/* -1 = not a member, 0 = untagged, 1 = tagged. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_vlan_lag(struct hemlockbcm_switch *sw,
                                               uint16_t vlan_id, uint32_t tid)
{
    struct stub_vlan *vlan;
    size_t i;

    if (sw == NULL) {
        return -1;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return -1;
    }
    for (i = 0; i < STUB_LAGS; i++) {
        if (vlan->lag_used[i] && vlan->lags[i] == tid) {
            return vlan->lag_tagged[i];
        }
    }
    return -1;
}

/* --- Spanning tree (ABI 1.5) -------------------------------------------- */

static struct stub_stg *find_stg(struct hemlockbcm_switch *sw, uint32_t stg)
{
    size_t i;
    for (i = 0; i < STUB_STGS; i++) {
        if (sw->stgs[i].used && sw->stgs[i].stg == stg) {
            return &sw->stgs[i];
        }
    }
    return NULL;
}

static int stub_stp_default(struct hemlockbcm_switch *sw, uint32_t *stg)
{
    if (sw == NULL || stg == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    *stg = STUB_DEFAULT_STG;
    return HEMLOCKBCM_OK;
}

static int stub_stp_create(struct hemlockbcm_switch *sw, uint32_t *stg)
{
    size_t i;

    if (sw == NULL || stg == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_STGS; i++) {
        if (!sw->stgs[i].used) {
            memset(&sw->stgs[i], 0, sizeof(sw->stgs[i]));
            sw->stgs[i].used = 1;
            /* Ids above the default one, which always exists. */
            sw->stgs[i].stg = STUB_DEFAULT_STG + (uint32_t)i + 1;
            *stg = sw->stgs[i].stg;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_stp_destroy(struct hemlockbcm_switch *sw, uint32_t stg)
{
    struct stub_stg *group;
    size_t i;

    if (sw == NULL || stg == STUB_DEFAULT_STG) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    group = find_stg(sw, stg);
    if (group == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_VLANS; i++) {
        if (sw->vlans[i].vlan_id != 0 && sw->vlans[i].stg == stg) {
            return HEMLOCKBCM_ERR_FAILURE;  /* VLANs must move first */
        }
    }
    memset(group, 0, sizeof(*group));
    return HEMLOCKBCM_OK;
}

static int stub_stp_vlan_set(struct hemlockbcm_switch *sw, uint32_t stg, uint16_t vlan_id)
{
    struct stub_vlan *vlan;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (stg != STUB_DEFAULT_STG && find_stg(sw, stg) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    vlan = find_vlan(sw, vlan_id);
    if (vlan == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* One group per VLAN, so this is a move rather than an addition. */
    vlan->stg = stg;
    return HEMLOCKBCM_OK;
}

static int stub_stp_apply(struct hemlockbcm_switch *sw, uint32_t stg,
                          uint32_t logical_port, int state)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);
    struct stub_stg *group;

    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (stg == STUB_DEFAULT_STG) {
        sw->default_stg_state[port - sw->ports] = state;
        return HEMLOCKBCM_OK;
    }
    group = find_stg(sw, stg);
    if (group == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    group->port_state[port - sw->ports] = state;
    return HEMLOCKBCM_OK;
}

static int stub_stp_port_state(struct hemlockbcm_switch *sw, uint32_t stg,
                               uint32_t logical_port, int state)
{
    if (sw == NULL || state < HEMLOCKBCM_STP_BLOCKING ||
        state > HEMLOCKBCM_STP_FORWARDING) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    return stub_stp_apply(sw, stg, logical_port, state);
}

static int stub_lag_stp_port_state(struct hemlockbcm_switch *sw, uint32_t stg,
                                   uint32_t tid, int state)
{
    struct stub_lag *lag;
    size_t i;

    if (sw == NULL || state < HEMLOCKBCM_STP_BLOCKING ||
        state > HEMLOCKBCM_STP_FORWARDING) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    lag = find_lag(sw, tid);
    if (lag == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* Every member, gated-closed ones included. Not inherited on join:
     * a port has a state in every group, so there is nothing single to
     * inherit. */
    for (i = 0; i < STUB_PORTS; i++) {
        if (lag->member_used[i]) {
            int status = stub_stp_apply(sw, stg, lag->members[i], state);
            if (status != HEMLOCKBCM_OK) {
                return status;
            }
        }
    }
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

HEMLOCKBCM_EXPORT int hemlockbcm_stub_stp_state(struct hemlockbcm_switch *sw,
                                                uint32_t stg, uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);
    struct stub_stg *group;

    if (sw == NULL || port == NULL) {
        return -1;
    }
    if (stg == STUB_DEFAULT_STG) {
        return sw->default_stg_state[port - sw->ports];
    }
    group = find_stg(sw, stg);
    return group == NULL ? -1 : group->port_state[port - sw->ports];
}

/* The group holding a VLAN, or 0 if there is no such VLAN. */
HEMLOCKBCM_EXPORT uint32_t hemlockbcm_stub_vlan_stg(struct hemlockbcm_switch *sw,
                                                    uint16_t vlan_id)
{
    struct stub_vlan *vlan;

    if (sw == NULL) {
        return 0;
    }
    vlan = find_vlan(sw, vlan_id);
    return vlan == NULL ? 0 : vlan->stg;
}

/* --- Port mirroring (ABI 1.6) -------------------------------------------- */

static struct stub_mirror *find_mirror(struct hemlockbcm_switch *sw, uint32_t session)
{
    size_t i;
    for (i = 0; i < STUB_MIRRORS; i++) {
        if (sw->mirrors[i].used && sw->mirrors[i].session == session) {
            return &sw->mirrors[i];
        }
    }
    return NULL;
}

static int stub_mirror_create(struct hemlockbcm_switch *sw, uint32_t monitor_port,
                              uint32_t *session)
{
    size_t i;

    if (sw == NULL || session == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (find_port(sw, monitor_port) == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_MIRRORS; i++) {
        if (!sw->mirrors[i].used) {
            memset(&sw->mirrors[i], 0, sizeof(sw->mirrors[i]));
            sw->mirrors[i].used = 1;
            /* Session ids start at 1: a real destination id is an opaque
             * gport and 0 is not special, but nothing here needs 0 and
             * leaving it out makes an unattached direction easy to
             * spell in the hooks below. */
            sw->mirrors[i].session = (uint32_t)i + 1;
            sw->mirrors[i].monitor = monitor_port;
            *session = sw->mirrors[i].session;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_mirror_destroy(struct hemlockbcm_switch *sw, uint32_t session)
{
    struct stub_mirror *mirror;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    mirror = find_mirror(sw, session);
    if (mirror == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_PORTS; i++) {
        if (sw->mirror_in[i] == session || sw->mirror_out[i] == session) {
            return HEMLOCKBCM_ERR_FAILURE;  /* detach the ports first */
        }
    }
    memset(mirror, 0, sizeof(*mirror));
    return HEMLOCKBCM_OK;
}

static int stub_mirror_port_attach(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   uint32_t session, int egress)
{
    struct hemlockbcm_port *port;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (find_mirror(sw, session) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* A set, not an addition: whatever the direction pointed at before
     * is replaced. */
    if (egress) {
        sw->mirror_out[port - sw->ports] = session;
    } else {
        sw->mirror_in[port - sw->ports] = session;
    }
    return HEMLOCKBCM_OK;
}

static int stub_mirror_port_detach(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   int egress)
{
    struct hemlockbcm_port *port;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Detaching a direction that is already clear is not an error. */
    if (egress) {
        sw->mirror_out[port - sw->ports] = 0;
    } else {
        sw->mirror_in[port - sw->ports] = 0;
    }
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI: the session a direction points at, or
 * 0 for none, or -1 for no such port. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_mirror(struct hemlockbcm_switch *sw,
                                             uint32_t logical_port, int egress)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);

    if (sw == NULL || port == NULL) {
        return -1;
    }
    return (int)(egress ? sw->mirror_out[port - sw->ports]
                        : sw->mirror_in[port - sw->ports]);
}

/* --- Storm control (ABI 1.7) --------------------------------------------- */

static int stub_storm_control_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                  int storm_class, uint32_t kbps)
{
    struct hemlockbcm_port *port;

    if (sw == NULL || storm_class < HEMLOCKBCM_STORM_BROADCAST ||
        storm_class > HEMLOCKBCM_STORM_UNKNOWN_UNICAST) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->storm_kbps[port - sw->ports][storm_class] = kbps;
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI. -1 = no such port. */
HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_storm(struct hemlockbcm_switch *sw,
                                                uint32_t logical_port, int storm_class)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);

    if (sw == NULL || port == NULL || storm_class < HEMLOCKBCM_STORM_BROADCAST ||
        storm_class > HEMLOCKBCM_STORM_UNKNOWN_UNICAST) {
        return -1;
    }
    return (int64_t)sw->storm_kbps[port - sw->ports][storm_class];
}

/* --- Host interfaces (ABI 1.8) ------------------------------------------- */

static int stub_host_punt_setup(struct hemlockbcm_switch *sw)
{
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Clears the previous run's, like bcm_knet_init does. */
    for (i = 0; i < STUB_PORTS; i++) {
        memset(&sw->hostifs[i], 0, sizeof(sw->hostifs[i]));
    }
    sw->punt_ready = 1;
    return HEMLOCKBCM_OK;
}

static int stub_hostif_create(struct hemlockbcm_switch *sw, uint32_t logical_port,
                              const char *name, uint32_t *hostif)
{
    struct hemlockbcm_port *port;
    size_t index;

    if (sw == NULL || name == NULL || hostif == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (strlen(name) > 15) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    index = (size_t)(port - sw->ports);
    if (sw->hostifs[index].used) {
        return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
    }
    sw->hostifs[index].used = 1;
    sw->hostifs[index].id = (uint32_t)index + 1;
    snprintf(sw->hostifs[index].name, sizeof(sw->hostifs[index].name), "%s", name);
    *hostif = sw->hostifs[index].id;
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

/* The netdev name for a port, or "" if it has none. */
HEMLOCKBCM_EXPORT const char *hemlockbcm_stub_hostif(struct hemlockbcm_switch *sw,
                                                     uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);

    if (sw == NULL || port == NULL || !sw->hostifs[port - sw->ports].used) {
        return "";
    }
    return sw->hostifs[port - sw->ports].name;
}

HEMLOCKBCM_EXPORT int hemlockbcm_stub_punt_ready(struct hemlockbcm_switch *sw)
{
    return sw == NULL ? 0 : sw->punt_ready;
}

/* --- Policers (ABI 1.9) --------------------------------------------------- */

static struct stub_policer *find_policer(struct hemlockbcm_switch *sw, uint32_t policer)
{
    size_t i;
    for (i = 0; i < STUB_POLICERS; i++) {
        if (sw->policers[i].used && sw->policers[i].id == policer) {
            return &sw->policers[i];
        }
    }
    return NULL;
}

static int stub_policer_create(struct hemlockbcm_switch *sw, int pps, uint64_t rate,
                               uint64_t burst, uint32_t *policer)
{
    size_t i;

    if (sw == NULL || policer == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_POLICERS; i++) {
        if (!sw->policers[i].used) {
            memset(&sw->policers[i], 0, sizeof(sw->policers[i]));
            sw->policers[i].used = 1;
            sw->policers[i].id = (uint32_t)i + 1;
            sw->policers[i].pps = pps ? 1 : 0;
            sw->policers[i].rate = rate;
            sw->policers[i].burst = burst;
            *policer = sw->policers[i].id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_policer_set(struct hemlockbcm_switch *sw, uint32_t policer, int pps,
                            uint64_t rate, uint64_t burst)
{
    struct stub_policer *entry;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    entry = find_policer(sw, policer);
    if (entry == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    entry->pps = pps ? 1 : 0;
    entry->rate = rate;
    entry->burst = burst;
    return HEMLOCKBCM_OK;
}

static int stub_policer_destroy(struct hemlockbcm_switch *sw, uint32_t policer)
{
    struct stub_policer *entry;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    entry = find_policer(sw, policer);
    if (entry == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    memset(entry, 0, sizeof(*entry));
    return HEMLOCKBCM_OK;
}

static int stub_policer_stats(struct hemlockbcm_switch *sw, uint32_t policer,
                              uint64_t *conforming, uint64_t *dropped)
{
    struct stub_policer *entry;

    if (sw == NULL || conforming == NULL || dropped == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    entry = find_policer(sw, policer);
    if (entry == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* Synthetic but not arbitrary: derived from the configured rate so
     * that a test can tell one policer's counters from another's. */
    *conforming = entry->rate;
    *dropped = entry->burst;
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI: the configured rate, or -1 if there is
 * no such policer. Sign of the rate carries `pps`. */
HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_policer_rate(struct hemlockbcm_switch *sw,
                                                       uint32_t policer)
{
    struct stub_policer *entry;

    if (sw == NULL) {
        return -1;
    }
    entry = find_policer(sw, policer);
    if (entry == NULL) {
        return -1;
    }
    return entry->pps ? -(int64_t)entry->rate : (int64_t)entry->rate;
}

/* --- ACLs (ABI 1.10) ------------------------------------------------------ */

static struct stub_acl_table *find_acl_table(struct hemlockbcm_switch *sw, uint32_t table)
{
    size_t i;
    for (i = 0; i < STUB_ACL_TABLES; i++) {
        if (sw->acl_tables[i].used && sw->acl_tables[i].id == table) {
            return &sw->acl_tables[i];
        }
    }
    return NULL;
}

static struct stub_acl_entry *find_acl_entry(struct hemlockbcm_switch *sw, uint32_t entry)
{
    size_t i;
    for (i = 0; i < STUB_ACL_ENTRIES; i++) {
        if (sw->acl_entries[i].used && sw->acl_entries[i].id == entry) {
            return &sw->acl_entries[i];
        }
    }
    return NULL;
}

static int stub_acl_table_create(struct hemlockbcm_switch *sw, int egress, uint32_t *table)
{
    size_t i;

    if (sw == NULL || table == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_ACL_TABLES; i++) {
        if (!sw->acl_tables[i].used) {
            memset(&sw->acl_tables[i], 0, sizeof(sw->acl_tables[i]));
            sw->acl_tables[i].used = 1;
            sw->acl_tables[i].id = (uint32_t)i + 1;
            sw->acl_tables[i].egress = egress ? 1 : 0;
            *table = sw->acl_tables[i].id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_acl_table_destroy(struct hemlockbcm_switch *sw, uint32_t table)
{
    struct stub_acl_table *found;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_acl_table(sw, table);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_ACL_ENTRIES; i++) {
        if (sw->acl_entries[i].used && sw->acl_entries[i].table == table) {
            return HEMLOCKBCM_ERR_FAILURE;  /* entries must go first */
        }
    }
    for (i = 0; i < STUB_PORTS; i++) {
        if (found->bound[i]) {
            return HEMLOCKBCM_ERR_FAILURE;  /* and so must bindings */
        }
    }
    memset(found, 0, sizeof(*found));
    return HEMLOCKBCM_OK;
}

static int stub_acl_table_bind(struct hemlockbcm_switch *sw, uint32_t table,
                               uint32_t logical_port, int bind)
{
    struct stub_acl_table *found;
    struct hemlockbcm_port *port;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_acl_table(sw, table);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    found->bound[port - sw->ports] = bind ? 1 : 0;
    return HEMLOCKBCM_OK;
}

static int stub_acl_table_unbind_all(struct hemlockbcm_switch *sw, int egress,
                                     uint32_t logical_port)
{
    struct hemlockbcm_port *port;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Only this stage's tables: the other stage's binding is a separate
     * fact and unbinding one must not disturb it. */
    for (i = 0; i < STUB_ACL_TABLES; i++) {
        if (sw->acl_tables[i].used && sw->acl_tables[i].egress == (egress ? 1 : 0)) {
            sw->acl_tables[i].bound[port - sw->ports] = 0;
        }
    }
    return HEMLOCKBCM_OK;
}

static int stub_acl_entry_create(struct hemlockbcm_switch *sw, uint32_t table,
                                 uint32_t priority,
                                 const struct hemlockbcm_acl_fields *fields,
                                 int action, uint32_t *entry)
{
    size_t i;

    if (sw == NULL || fields == NULL || entry == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (action < HEMLOCKBCM_ACL_FORWARD || action > HEMLOCKBCM_ACL_COPY) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (find_acl_table(sw, table) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_ACL_ENTRIES; i++) {
        if (!sw->acl_entries[i].used) {
            memset(&sw->acl_entries[i], 0, sizeof(sw->acl_entries[i]));
            sw->acl_entries[i].used = 1;
            sw->acl_entries[i].id = (uint32_t)i + 1;
            sw->acl_entries[i].table = table;
            sw->acl_entries[i].priority = priority;
            sw->acl_entries[i].action = action;
            sw->acl_entries[i].fields = *fields;
            *entry = sw->acl_entries[i].id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_acl_entry_action_set(struct hemlockbcm_switch *sw, uint32_t entry,
                                     int action)
{
    struct stub_acl_entry *found;

    if (sw == NULL || action < HEMLOCKBCM_ACL_FORWARD || action > HEMLOCKBCM_ACL_COPY) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_acl_entry(sw, entry);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* The match is untouched, which is the point of having this at all. */
    found->action = action;
    return HEMLOCKBCM_OK;
}

static int stub_acl_entry_destroy(struct hemlockbcm_switch *sw, uint32_t entry)
{
    struct stub_acl_entry *found;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_acl_entry(sw, entry);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    memset(found, 0, sizeof(*found));
    return HEMLOCKBCM_OK;
}

static int stub_acl_available(struct hemlockbcm_switch *sw, int egress, uint32_t *entries)
{
    uint32_t used = 0;
    size_t i;

    if (sw == NULL || entries == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_ACL_ENTRIES; i++) {
        struct stub_acl_table *table;

        if (!sw->acl_entries[i].used) {
            continue;
        }
        table = find_acl_table(sw, sw->acl_entries[i].table);
        if (table != NULL && table->egress == (egress ? 1 : 0)) {
            used++;
        }
    }
    *entries = (uint32_t)STUB_ACL_ENTRIES - used;
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

/* The entry's action, or -1 if there is no such entry. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_acl_action(struct hemlockbcm_switch *sw,
                                                 uint32_t entry)
{
    struct stub_acl_entry *found = sw == NULL ? NULL : find_acl_entry(sw, entry);
    return found == NULL ? -1 : found->action;
}

/* The entry's `present` mask, or 0 if there is no such entry. */
HEMLOCKBCM_EXPORT uint32_t hemlockbcm_stub_acl_fields(struct hemlockbcm_switch *sw,
                                                      uint32_t entry)
{
    struct stub_acl_entry *found = sw == NULL ? NULL : find_acl_entry(sw, entry);
    return found == NULL ? 0 : found->fields.present;
}

/* 1 if the port binds the table, 0 if not, -1 if either is unknown. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_acl_bound(struct hemlockbcm_switch *sw,
                                                uint32_t table, uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);
    struct stub_acl_table *found = sw == NULL ? NULL : find_acl_table(sw, table);

    if (port == NULL || found == NULL) {
        return -1;
    }
    return found->bound[port - sw->ports];
}

/* --- ACL counters and per-entry policers (ABI 1.11) ----------------------- */

static struct stub_acl_counter *find_acl_counter(struct hemlockbcm_switch *sw,
                                                 uint32_t counter)
{
    size_t i;
    for (i = 0; i < STUB_ACL_COUNTERS; i++) {
        if (sw->acl_counters[i].used && sw->acl_counters[i].id == counter) {
            return &sw->acl_counters[i];
        }
    }
    return NULL;
}

static int stub_acl_counter_create(struct hemlockbcm_switch *sw, uint32_t table,
                                   uint32_t *counter)
{
    size_t i;

    if (sw == NULL || counter == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (find_acl_table(sw, table) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_ACL_COUNTERS; i++) {
        if (!sw->acl_counters[i].used) {
            memset(&sw->acl_counters[i], 0, sizeof(sw->acl_counters[i]));
            sw->acl_counters[i].used = 1;
            sw->acl_counters[i].id = (uint32_t)i + 1;
            sw->acl_counters[i].table = table;
            /* Distinct per counter so a test can tell them apart. */
            sw->acl_counters[i].packets = 100ull * (i + 1);
            *counter = sw->acl_counters[i].id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_acl_counter_destroy(struct hemlockbcm_switch *sw, uint32_t counter)
{
    struct stub_acl_counter *found;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_acl_counter(sw, counter);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_ACL_ENTRIES; i++) {
        if (sw->acl_entries[i].used && sw->acl_entries[i].counter == counter) {
            return HEMLOCKBCM_ERR_FAILURE;  /* still referenced */
        }
    }
    memset(found, 0, sizeof(*found));
    return HEMLOCKBCM_OK;
}

static int stub_acl_counter_get(struct hemlockbcm_switch *sw, uint32_t counter,
                                uint64_t *packets)
{
    struct stub_acl_counter *found;

    if (sw == NULL || packets == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_acl_counter(sw, counter);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    *packets = found->packets;
    return HEMLOCKBCM_OK;
}

static int stub_acl_entry_attach(struct hemlockbcm_switch *sw, uint32_t entry,
                                 uint32_t counter, uint32_t policer)
{
    struct stub_acl_entry *found;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_acl_entry(sw, entry);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    if (counter != 0 && find_acl_counter(sw, counter) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    if (policer != 0 && find_policer(sw, policer) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* A set: 0 detaches, and neither id is ever 0. */
    found->counter = counter;
    found->policer = policer;
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI: the entry's (counter, policer) pair
 * packed into one value, or -1 if there is no such entry. */
HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_acl_attach(struct hemlockbcm_switch *sw,
                                                     uint32_t entry)
{
    struct stub_acl_entry *found = sw == NULL ? NULL : find_acl_entry(sw, entry);

    if (found == NULL) {
        return -1;
    }
    return ((int64_t)found->counter << 32) | (int64_t)found->policer;
}

/* --- Router interfaces (ABI 1.12) ----------------------------------------- */

static struct stub_rif *find_rif(struct hemlockbcm_switch *sw, uint32_t rif)
{
    size_t i;
    for (i = 0; i < STUB_RIFS; i++) {
        if (sw->rifs[i].used && sw->rifs[i].id == rif) {
            return &sw->rifs[i];
        }
    }
    return NULL;
}

static int stub_rif_alloc(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                          const uint8_t mac[6], uint32_t *rif)
{
    size_t i;

    for (i = 0; i < STUB_RIFS; i++) {
        if (!sw->rifs[i].used) {
            memset(&sw->rifs[i], 0, sizeof(sw->rifs[i]));
            sw->rifs[i].used = 1;
            sw->rifs[i].id = (uint32_t)i + 1;
            sw->rifs[i].vlan_id = vlan_id;
            memcpy(sw->rifs[i].mac, mac, 6);
            *rif = sw->rifs[i].id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_rif_port_create(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                uint16_t vlan_id, const uint8_t mac[6], uint32_t *rif)
{
    struct hemlockbcm_port *port;
    int status;

    if (sw == NULL || mac == NULL || rif == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Out of the bridge, into a VLAN of its own, then the interface. */
    (void)stub_remove_vlan_member(sw, STUB_DEFAULT_VLAN, logical_port);
    status = stub_create_vlan(sw, vlan_id);
    if (status != HEMLOCKBCM_OK) {
        return status;
    }
    status = stub_add_vlan_member(sw, vlan_id, logical_port, 0);
    if (status != HEMLOCKBCM_OK) {
        (void)stub_remove_vlan(sw, vlan_id);
        return status;
    }
    sw->pvid[port - sw->ports] = vlan_id;
    status = stub_rif_alloc(sw, vlan_id, mac, rif);
    if (status != HEMLOCKBCM_OK) {
        (void)stub_remove_vlan_member(sw, vlan_id, logical_port);
        (void)stub_remove_vlan(sw, vlan_id);
        sw->pvid[port - sw->ports] = STUB_DEFAULT_VLAN;
        (void)stub_add_vlan_member(sw, STUB_DEFAULT_VLAN, logical_port, 0);
        return status;
    }
    sw->rifs[*rif - 1].port = logical_port;
    return HEMLOCKBCM_OK;
}

static int stub_rif_port_destroy(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 uint16_t vlan_id, uint32_t rif)
{
    struct hemlockbcm_port *port;
    struct stub_rif *found;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    found = find_rif(sw, rif);
    if (port == NULL || found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    memset(found, 0, sizeof(*found));
    (void)stub_remove_vlan_member(sw, vlan_id, logical_port);
    (void)stub_remove_vlan(sw, vlan_id);
    /* Bridging again: member of the default VLAN, PVID to match. */
    (void)stub_add_vlan_member(sw, STUB_DEFAULT_VLAN, logical_port, 0);
    sw->pvid[port - sw->ports] = STUB_DEFAULT_VLAN;
    return HEMLOCKBCM_OK;
}

static int stub_rif_vlan_create(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                                const uint8_t mac[6], uint32_t *rif)
{
    if (sw == NULL || mac == NULL || rif == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (find_vlan(sw, vlan_id) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    return stub_rif_alloc(sw, vlan_id, mac, rif);
}

static int stub_rif_vlan_destroy(struct hemlockbcm_switch *sw, uint32_t rif)
{
    struct stub_rif *found;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_rif(sw, rif);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* The VLAN stays: it was bridging before and still is. */
    memset(found, 0, sizeof(*found));
    return HEMLOCKBCM_OK;
}

static int stub_my_mac_create(struct hemlockbcm_switch *sw, uint16_t vlan_id,
                              const uint8_t mac[6], uint32_t *my_mac)
{
    size_t i;

    if (sw == NULL || mac == NULL || my_mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_MY_MACS; i++) {
        if (!sw->my_macs[i].used) {
            memset(&sw->my_macs[i], 0, sizeof(sw->my_macs[i]));
            sw->my_macs[i].used = 1;
            sw->my_macs[i].id = (uint32_t)i + 1;
            sw->my_macs[i].vlan_id = vlan_id;
            memcpy(sw->my_macs[i].mac, mac, 6);
            *my_mac = sw->my_macs[i].id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_my_mac_destroy(struct hemlockbcm_switch *sw, uint32_t my_mac)
{
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_MY_MACS; i++) {
        if (sw->my_macs[i].used && sw->my_macs[i].id == my_mac) {
            memset(&sw->my_macs[i], 0, sizeof(sw->my_macs[i]));
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
}

/* Test hooks, not part of the ABI. */

/* The VLAN an interface sits on, or 0 if there is no such interface. */
HEMLOCKBCM_EXPORT uint16_t hemlockbcm_stub_rif_vlan(struct hemlockbcm_switch *sw,
                                                    uint32_t rif)
{
    struct stub_rif *found = sw == NULL ? NULL : find_rif(sw, rif);
    return found == NULL ? 0 : found->vlan_id;
}

HEMLOCKBCM_EXPORT uint32_t hemlockbcm_stub_my_mac_count(struct hemlockbcm_switch *sw)
{
    uint32_t count = 0;
    size_t i;

    if (sw != NULL) {
        for (i = 0; i < STUB_MY_MACS; i++) {
            count += sw->my_macs[i].used ? 1u : 0u;
        }
    }
    return count;
}

/* --- Routes (ABI 1.13) ----------------------------------------------------- */

static struct stub_route *find_route(struct hemlockbcm_switch *sw, uint32_t prefix,
                                     uint32_t mask)
{
    size_t i;
    for (i = 0; i < STUB_ROUTES; i++) {
        if (sw->routes[i].used && sw->routes[i].prefix == prefix &&
            sw->routes[i].mask == mask) {
            return &sw->routes[i];
        }
    }
    return NULL;
}

static int stub_route_set(struct hemlockbcm_switch *sw, uint32_t prefix, uint32_t mask,
                          int kind, uint32_t rif)
{
    struct stub_route *found;
    size_t i;

    if (sw == NULL || kind < HEMLOCKBCM_ROUTE_CPU || kind > HEMLOCKBCM_ROUTE_DROP) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (kind == HEMLOCKBCM_ROUTE_RIF && find_rif(sw, rif) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* A replace, not a duplicate: the same prefix keeps its slot. */
    found = find_route(sw, prefix, mask);
    if (found == NULL) {
        for (i = 0; i < STUB_ROUTES; i++) {
            if (!sw->routes[i].used) {
                found = &sw->routes[i];
                break;
            }
        }
    }
    if (found == NULL) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    found->used = 1;
    found->prefix = prefix;
    found->mask = mask;
    found->kind = kind;
    found->rif = kind == HEMLOCKBCM_ROUTE_RIF ? rif : 0;
    return HEMLOCKBCM_OK;
}

static int stub_route_delete(struct hemlockbcm_switch *sw, uint32_t prefix, uint32_t mask)
{
    struct stub_route *found;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_route(sw, prefix, mask);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    memset(found, 0, sizeof(*found));
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI: the route's kind, or -1 for no such
 * route. The RIF it points at is packed into the high bits. */
HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_route(struct hemlockbcm_switch *sw,
                                                uint32_t prefix, uint32_t mask)
{
    struct stub_route *found = sw == NULL ? NULL : find_route(sw, prefix, mask);

    if (found == NULL) {
        return -1;
    }
    return ((int64_t)found->rif << 32) | (int64_t)found->kind;
}

/* --- Neighbours and next hops (ABI 1.14) ----------------------------------- */

static struct stub_neighbor *find_neighbor(struct hemlockbcm_switch *sw, uint32_t ip)
{
    size_t i;
    for (i = 0; i < STUB_NEIGHBORS; i++) {
        if (sw->neighbors[i].used && sw->neighbors[i].ip == ip) {
            return &sw->neighbors[i];
        }
    }
    return NULL;
}

static int stub_neighbor_set(struct hemlockbcm_switch *sw, uint32_t rif, uint32_t ip,
                             const uint8_t mac[6])
{
    struct stub_neighbor *found;
    struct stub_rif *interface;
    size_t i;

    if (sw == NULL || mac == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    interface = find_rif(sw, rif);
    if (interface == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /*
     * The FDB lookup the real shim does. Without a learned MAC there is
     * no egress port, and the caller is expected to leave the route on
     * the CPU until there is.
     */
    {
        int learned = 0;
        for (i = 0; i < STUB_FDB; i++) {
            if (sw->fdb[i].used && sw->fdb[i].vlan_id == interface->vlan_id &&
                memcmp(sw->fdb[i].mac, mac, 6) == 0) {
                learned = 1;
                break;
            }
        }
        if (!learned) {
            return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
        }
    }

    /* Replace in place: a neighbour whose MAC changed keeps its slot. */
    found = find_neighbor(sw, ip);
    if (found == NULL) {
        for (i = 0; i < STUB_NEIGHBORS; i++) {
            if (!sw->neighbors[i].used) {
                found = &sw->neighbors[i];
                break;
            }
        }
    }
    if (found == NULL) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    found->used = 1;
    found->ip = ip;
    found->rif = rif;
    memcpy(found->mac, mac, 6);
    return HEMLOCKBCM_OK;
}

static int stub_neighbor_clear(struct hemlockbcm_switch *sw, uint32_t rif, uint32_t ip)
{
    struct stub_neighbor *found;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    (void)rif;
    found = find_neighbor(sw, ip);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    memset(found, 0, sizeof(*found));
    return HEMLOCKBCM_OK;
}

static int stub_route_via_nexthop(struct hemlockbcm_switch *sw, uint32_t prefix,
                                  uint32_t mask, uint32_t nexthop_ip)
{
    struct stub_neighbor *neighbor;
    struct stub_route *found;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    neighbor = find_neighbor(sw, nexthop_ip);
    if (neighbor == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;  /* unresolved */
    }
    found = find_route(sw, prefix, mask);
    if (found == NULL) {
        for (i = 0; i < STUB_ROUTES; i++) {
            if (!sw->routes[i].used) {
                found = &sw->routes[i];
                break;
            }
        }
    }
    if (found == NULL) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    found->used = 1;
    found->prefix = prefix;
    found->mask = mask;
    found->kind = STUB_ROUTE_NEXTHOP;
    found->rif = neighbor->rif;
    found->nexthop_ip = nexthop_ip;
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

/* 1 if a neighbour for `ip` is resolved, else 0. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_neighbor(struct hemlockbcm_switch *sw, uint32_t ip)
{
    return (sw != NULL && find_neighbor(sw, ip) != NULL) ? 1 : 0;
}

/* The next-hop IP a route forwards through, or 0. */
HEMLOCKBCM_EXPORT uint32_t hemlockbcm_stub_route_nexthop(struct hemlockbcm_switch *sw,
                                                         uint32_t prefix, uint32_t mask)
{
    struct stub_route *found = sw == NULL ? NULL : find_route(sw, prefix, mask);
    return found == NULL ? 0 : found->nexthop_ip;
}

/* --- ECMP groups (ABI 1.15) ------------------------------------------------ */

static struct stub_ecmp *find_ecmp(struct hemlockbcm_switch *sw, uint32_t group)
{
    size_t i;
    for (i = 0; i < STUB_ECMP_GROUPS; i++) {
        if (sw->ecmps[i].used && sw->ecmps[i].id == group) {
            return &sw->ecmps[i];
        }
    }
    return NULL;
}

static int stub_ecmp_create(struct hemlockbcm_switch *sw, uint32_t *group)
{
    size_t i;

    if (sw == NULL || group == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    for (i = 0; i < STUB_ECMP_GROUPS; i++) {
        if (!sw->ecmps[i].used) {
            memset(&sw->ecmps[i], 0, sizeof(sw->ecmps[i]));
            sw->ecmps[i].used = 1;
            sw->ecmps[i].id = (uint32_t)i + 1;
            *group = sw->ecmps[i].id;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_ecmp_destroy(struct hemlockbcm_switch *sw, uint32_t group)
{
    struct stub_ecmp *found;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_ecmp(sw, group);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_ECMP_MEMBERS; i++) {
        if (found->member_used[i]) {
            return HEMLOCKBCM_ERR_FAILURE;  /* members first */
        }
    }
    for (i = 0; i < STUB_ROUTES; i++) {
        if (sw->routes[i].used && sw->routes[i].kind == STUB_ROUTE_ECMP &&
            sw->routes[i].nexthop_ip == group) {
            return HEMLOCKBCM_ERR_FAILURE;  /* still routed through */
        }
    }
    memset(found, 0, sizeof(*found));
    return HEMLOCKBCM_OK;
}

static int stub_ecmp_member_add(struct hemlockbcm_switch *sw, uint32_t group,
                                uint32_t nexthop_ip)
{
    struct stub_ecmp *found;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_ecmp(sw, group);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    /* The neighbour has to have resolved, like a single-path route. */
    if (find_neighbor(sw, nexthop_ip) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_ECMP_MEMBERS; i++) {
        if (found->member_used[i] && found->members[i] == nexthop_ip) {
            return HEMLOCKBCM_ERR_ITEM_ALREADY_EXISTS;
        }
    }
    for (i = 0; i < STUB_ECMP_MEMBERS; i++) {
        if (!found->member_used[i]) {
            found->member_used[i] = 1;
            found->members[i] = nexthop_ip;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_NO_MEMORY;
}

static int stub_ecmp_member_remove(struct hemlockbcm_switch *sw, uint32_t group,
                                   uint32_t nexthop_ip)
{
    struct stub_ecmp *found;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    found = find_ecmp(sw, group);
    if (found == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    for (i = 0; i < STUB_ECMP_MEMBERS; i++) {
        if (found->member_used[i] && found->members[i] == nexthop_ip) {
            found->member_used[i] = 0;
            found->members[i] = 0;
            return HEMLOCKBCM_OK;
        }
    }
    return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
}

static int stub_route_via_ecmp(struct hemlockbcm_switch *sw, uint32_t prefix,
                               uint32_t mask, uint32_t group)
{
    struct stub_route *found;
    size_t i;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    if (find_ecmp(sw, group) == NULL) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    found = find_route(sw, prefix, mask);
    if (found == NULL) {
        for (i = 0; i < STUB_ROUTES; i++) {
            if (!sw->routes[i].used) {
                found = &sw->routes[i];
                break;
            }
        }
    }
    if (found == NULL) {
        return HEMLOCKBCM_ERR_NO_MEMORY;
    }
    found->used = 1;
    found->prefix = prefix;
    found->mask = mask;
    found->kind = STUB_ROUTE_ECMP;
    found->rif = 0;
    /* The group the route follows, in the same field a single-path
     * route keeps its next hop in. */
    found->nexthop_ip = group;
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI: 1 if the group has that member. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_ecmp_member(struct hemlockbcm_switch *sw,
                                                  uint32_t group, uint32_t nexthop_ip)
{
    struct stub_ecmp *found = sw == NULL ? NULL : find_ecmp(sw, group);
    size_t i;

    if (found == NULL) {
        return 0;
    }
    for (i = 0; i < STUB_ECMP_MEMBERS; i++) {
        if (found->member_used[i] && found->members[i] == nexthop_ip) {
            return 1;
        }
    }
    return 0;
}

/* --- CoPP traps (ABI 1.16) ------------------------------------------------- */

static int stub_trap_valid(int kind)
{
    return kind >= 0 && kind < HEMLOCKBCM_TRAP_KIND_COUNT;
}

static int stub_trap_set(struct hemlockbcm_switch *sw, int kind, int trap_only,
                         int is_default, uint32_t policer)
{
    struct stub_trap *slot;

    if (sw == NULL || !stub_trap_valid(kind)) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* A replace, keyed by (kind, is_default), like the real shim's
     * derived entry ids make it. */
    slot = &sw->traps[kind][is_default ? 1 : 0];
    slot->used = 1;
    slot->trap_only = trap_only ? 1 : 0;
    slot->policer = policer;
    return HEMLOCKBCM_OK;
}

static int stub_trap_clear(struct hemlockbcm_switch *sw, int kind, int is_default)
{
    struct stub_trap *slot;

    if (sw == NULL || !stub_trap_valid(kind)) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    slot = &sw->traps[kind][is_default ? 1 : 0];
    if (!slot->used) {
        return HEMLOCKBCM_ERR_ITEM_NOT_FOUND;
    }
    memset(slot, 0, sizeof(*slot));
    return HEMLOCKBCM_OK;
}

static int stub_trap_default_policer_set(struct hemlockbcm_switch *sw, uint32_t policer)
{
    int kind;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Only the installed default-group traps; named groups keep the
     * policer they were created with. */
    for (kind = 0; kind < HEMLOCKBCM_TRAP_KIND_COUNT; kind++) {
        if (sw->traps[kind][1].used) {
            sw->traps[kind][1].policer = policer;
        }
    }
    return HEMLOCKBCM_OK;
}

/* Test hook, not part of the ABI: -1 if the trap is not installed, else
 * (policer << 8) | (trap_only << 1) | 1. */
HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_trap(struct hemlockbcm_switch *sw,
                                               int kind, int is_default)
{
    struct stub_trap *slot;

    if (sw == NULL || !stub_trap_valid(kind)) {
        return -1;
    }
    slot = &sw->traps[kind][is_default ? 1 : 0];
    if (!slot->used) {
        return -1;
    }
    return ((int64_t)slot->policer << 8) | ((int64_t)slot->trap_only << 1) | 1;
}

/* --- Ingress sampling / sFlow (ABI 1.17) ------------------------------------ */

static int stub_set_sample_callback(struct hemlockbcm_switch *sw,
                                    hemlockbcm_sample_cb cb, void *context)
{
    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->sample_cb = cb;
    sw->sample_ctx = context;
    return HEMLOCKBCM_OK;
}

static int stub_sample_rate_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                uint32_t rate)
{
    struct hemlockbcm_port *port;

    if (sw == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    port = find_port(sw, logical_port);
    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->sample_rate[port - sw->ports] = rate;
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_sample_rate(struct hemlockbcm_switch *sw,
                                                      uint32_t logical_port)
{
    struct hemlockbcm_port *port = find_port(sw, logical_port);

    if (sw == NULL || port == NULL) {
        return -1;
    }
    return (int64_t)sw->sample_rate[port - sw->ports];
}

/* Deliver one fake sampled packet through the registered callback, the
 * way the real shim's RX handler would. Returns 0 if delivered. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_fire_sample(struct hemlockbcm_switch *sw,
                                                  uint32_t logical_port,
                                                  uint32_t original_length,
                                                  const uint8_t *data,
                                                  uint32_t length)
{
    if (sw == NULL || sw->sample_cb == NULL) {
        return -1;
    }
    sw->sample_cb(sw->sample_ctx, logical_port, original_length, data, length);
    return 0;
}

/* --- QoS (ABI 1.18) --------------------------------------------------------- */

#define STUB_QUEUES 8

static struct hemlockbcm_port *stub_qos_port(struct hemlockbcm_switch *sw,
                                             uint32_t logical_port)
{
    return sw == NULL ? NULL : find_port(sw, logical_port);
}

static int stub_qos_dscp_tc_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                const uint8_t tc[64])
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || tc == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    memcpy(sw->dscp_tc[port - sw->ports], tc, 64);
    return HEMLOCKBCM_OK;
}

static int stub_qos_dscp_trust_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   int trust)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->dscp_trust[port - sw->ports] = trust ? 1 : 0;
    return HEMLOCKBCM_OK;
}

static int stub_qos_dot1p_tc_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                 const uint8_t tc[8])
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || tc == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    memcpy(sw->dot1p_tc[port - sw->ports], tc, 8);
    return HEMLOCKBCM_OK;
}

static int stub_qos_default_tc_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   uint8_t tc)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->default_tc[port - sw->ports] = tc;
    return HEMLOCKBCM_OK;
}

static int stub_qos_queue_sched_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                    uint32_t queue, int strict, uint8_t weight)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || queue >= STUB_QUEUES) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->sched_strict[port - sw->ports][queue] = strict ? 1 : 0;
    sw->sched_weight[port - sw->ports][queue] = weight;
    return HEMLOCKBCM_OK;
}

static int stub_qos_queue_shaper_set(struct hemlockbcm_switch *sw,
                                     uint32_t logical_port, uint32_t queue,
                                     uint64_t kbps)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || queue >= STUB_QUEUES) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->queue_shaper[port - sw->ports][queue] = kbps;
    return HEMLOCKBCM_OK;
}

static int stub_qos_port_shaper_set(struct hemlockbcm_switch *sw,
                                    uint32_t logical_port, uint64_t kbps)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    sw->port_shaper[port - sw->ports] = kbps;
    return HEMLOCKBCM_OK;
}

static int stub_qos_queue_wred_set(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   uint32_t queue, uint32_t min_bytes,
                                   uint32_t max_bytes, uint8_t drop_probability,
                                   int ecn, int enable)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);
    struct stub_wred *slot;

    if (port == NULL || queue >= STUB_QUEUES) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    slot = &sw->wred[port - sw->ports][queue];
    if (!enable) {
        memset(slot, 0, sizeof(*slot));
        return HEMLOCKBCM_OK;
    }
    slot->enable = 1;
    slot->ecn = ecn ? 1 : 0;
    slot->min_bytes = min_bytes;
    slot->max_bytes = max_bytes;
    slot->drop_probability = drop_probability;
    return HEMLOCKBCM_OK;
}

static int stub_queue_counters_get(struct hemlockbcm_switch *sw, uint32_t logical_port,
                                   uint32_t queue, uint64_t *pkts, uint64_t *bytes,
                                   uint64_t *dropped_pkts, uint64_t *dropped_bytes)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || queue >= STUB_QUEUES || pkts == NULL || bytes == NULL
        || dropped_pkts == NULL || dropped_bytes == NULL) {
        return HEMLOCKBCM_ERR_INVALID_PARAM;
    }
    /* Deterministic per (port, queue), so a test can prove the right
     * queue's numbers land in the right row. */
    *pkts = (uint64_t)logical_port * 1000u + queue * 10u;
    *bytes = *pkts * 64u;
    *dropped_pkts = queue;
    *dropped_bytes = queue * 64u;
    return HEMLOCKBCM_OK;
}

/* Test hooks, not part of the ABI. */

/* tc | (trust << 8), or -1. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_dscp_tc(struct hemlockbcm_switch *sw,
                                              uint32_t logical_port, int codepoint)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || codepoint < 0 || codepoint > 63) {
        return -1;
    }
    return sw->dscp_tc[port - sw->ports][codepoint]
           | (sw->dscp_trust[port - sw->ports] << 8);
}

HEMLOCKBCM_EXPORT int hemlockbcm_stub_dot1p_tc(struct hemlockbcm_switch *sw,
                                               uint32_t logical_port, int pri)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || pri < 0 || pri > 7) {
        return -1;
    }
    return sw->dot1p_tc[port - sw->ports][pri];
}

HEMLOCKBCM_EXPORT int hemlockbcm_stub_default_tc(struct hemlockbcm_switch *sw,
                                                 uint32_t logical_port)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);
    return port == NULL ? -1 : sw->default_tc[port - sw->ports];
}

/* (strict << 16) | weight, or -1. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_sched(struct hemlockbcm_switch *sw,
                                            uint32_t logical_port, uint32_t queue)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || queue >= STUB_QUEUES) {
        return -1;
    }
    return (sw->sched_strict[port - sw->ports][queue] << 16)
           | sw->sched_weight[port - sw->ports][queue];
}

HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_queue_shaper(struct hemlockbcm_switch *sw,
                                                       uint32_t logical_port,
                                                       uint32_t queue)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);

    if (port == NULL || queue >= STUB_QUEUES) {
        return -1;
    }
    return (int64_t)sw->queue_shaper[port - sw->ports][queue];
}

HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_port_shaper(struct hemlockbcm_switch *sw,
                                                      uint32_t logical_port)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);
    return port == NULL ? -1 : (int64_t)sw->port_shaper[port - sw->ports];
}

/* (min << 32) | max, or -1. */
HEMLOCKBCM_EXPORT int64_t hemlockbcm_stub_wred_range(struct hemlockbcm_switch *sw,
                                                     uint32_t logical_port,
                                                     uint32_t queue)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);
    struct stub_wred *slot;

    if (port == NULL || queue >= STUB_QUEUES) {
        return -1;
    }
    slot = &sw->wred[port - sw->ports][queue];
    return ((int64_t)slot->min_bytes << 32) | (int64_t)slot->max_bytes;
}

/* enable | (ecn << 1) | (drop_probability << 8), or -1. */
HEMLOCKBCM_EXPORT int hemlockbcm_stub_wred_mode(struct hemlockbcm_switch *sw,
                                                uint32_t logical_port, uint32_t queue)
{
    struct hemlockbcm_port *port = stub_qos_port(sw, logical_port);
    struct stub_wred *slot;

    if (port == NULL || queue >= STUB_QUEUES) {
        return -1;
    }
    slot = &sw->wred[port - sw->ports][queue];
    return slot->enable | (slot->ecn << 1) | (slot->drop_probability << 8);
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
    stub_set_fdb_aging,
    stub_add_fdb_entry,
    stub_remove_fdb_entry,
    stub_flush_fdb,
    stub_set_port_learning,
    stub_set_port_learn_limit,
    stub_lag_create,
    stub_lag_destroy,
    stub_lag_member_add,
    stub_lag_member_remove,
    stub_lag_member_state,
    stub_lag_vlan_member_add,
    stub_lag_vlan_member_remove,
    stub_lag_set_pvid,
    stub_stp_default,
    stub_stp_create,
    stub_stp_destroy,
    stub_stp_vlan_set,
    stub_stp_port_state,
    stub_lag_stp_port_state,
    stub_mirror_create,
    stub_mirror_destroy,
    stub_mirror_port_attach,
    stub_mirror_port_detach,
    stub_storm_control_set,
    stub_host_punt_setup,
    stub_hostif_create,
    stub_policer_create,
    stub_policer_set,
    stub_policer_destroy,
    stub_policer_stats,
    stub_acl_table_create,
    stub_acl_table_destroy,
    stub_acl_table_bind,
    stub_acl_table_unbind_all,
    stub_acl_entry_create,
    stub_acl_entry_action_set,
    stub_acl_entry_destroy,
    stub_acl_available,
    stub_acl_counter_create,
    stub_acl_counter_destroy,
    stub_acl_counter_get,
    stub_acl_entry_attach,
    stub_rif_port_create,
    stub_rif_port_destroy,
    stub_rif_vlan_create,
    stub_rif_vlan_destroy,
    stub_my_mac_create,
    stub_my_mac_destroy,
    stub_route_set,
    stub_route_delete,
    stub_neighbor_set,
    stub_neighbor_clear,
    stub_route_via_nexthop,
    stub_ecmp_create,
    stub_ecmp_destroy,
    stub_ecmp_member_add,
    stub_ecmp_member_remove,
    stub_route_via_ecmp,
    stub_trap_set,
    stub_trap_clear,
    stub_trap_default_policer_set,
    stub_set_sample_callback,
    stub_sample_rate_set,
    stub_qos_dscp_tc_set,
    stub_qos_dscp_trust_set,
    stub_qos_dot1p_tc_set,
    stub_qos_default_tc_set,
    stub_qos_queue_sched_set,
    stub_qos_queue_shaper_set,
    stub_qos_port_shaper_set,
    stub_qos_queue_wred_set,
    stub_queue_counters_get,
};

HEMLOCKBCM_EXPORT const struct hemlockbcm_api *hemlockbcm_get_api(uint32_t want_major)
{
    if (want_major != HEMLOCKBCM_ABI_MAJOR) {
        return NULL;
    }
    return &STUB_API;
}
