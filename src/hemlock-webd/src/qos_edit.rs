//! Config edits for the QoS-suite pages: the four global maps, the
//! named WRED/ECN profiles, and per-port classification, scheduling and
//! shaping.
//!
//! Same discipline as `security_edit.rs`: the builders write exactly
//! the leaves and phrases hemlockctl writes, based on the running
//! config, and the result goes through mgmtd's normal SetCandidate +
//! Commit path — so validation (including every QoS `IntentError`), the
//! rollback ring, and `show configuration` behave as if the change came
//! from the CLI. Every builder validates its whole edit before touching
//! the tree.

use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

/// The four map tables and the key/value words each takes, with their
/// domains: (table, key word, key max, value word, value max).
const MAP_TABLES: &[(&str, &str, u8, &str, u8)] = &[
    ("dscp-to-tc", "dscp", 63, "tc", 7),
    ("cos-to-tc", "cos", 7, "tc", 7),
    ("tc-to-dscp", "tc", 7, "dscp", 63),
    ("tc-to-cos", "tc", 7, "cos", 7),
];

/// Egress unicast queues per front-panel port (Helix4).
const QUEUE_COUNT: u8 = 8;

fn map_table(name: &str) -> Result<(&'static str, u8, &'static str, u8), String> {
    MAP_TABLES
        .iter()
        .find(|(table, ..)| *table == name)
        .map(|(_, key, key_max, value, value_max)| (*key, *key_max, *value, *value_max))
        .ok_or_else(|| {
            format!("bad map table {name:?} (dscp-to-tc|cos-to-tc|tc-to-dscp|tc-to-cos)")
        })
}

/// WRED profile name syntax (letter first, then letters/digits/_/-, max
/// 32) — mirror of the CLI's prompt check; mgmtd re-validates.
fn valid_wred_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && name.len() <= 32
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bad WRED profile name {name:?} (letter first, then letters/digits/_/-, max 32)"
        ))
    }
}

/// QoS is a front-panel concept: physical ports and Port-Channels only.
fn valid_qos_port(name: &str) -> Result<(), String> {
    if name.starts_with("Ethernet") || name.starts_with("Port-Channel") {
        Ok(())
    } else {
        Err(format!(
            "{name}: QoS is a front-panel concept (Ethernet or Port-Channel)"
        ))
    }
}

fn block_children_mut<'a>(items: &'a mut [Item], name: &str) -> Option<&'a mut Vec<Item>> {
    items.iter_mut().find_map(|item| match item {
        Item::Block {
            name: n, children, ..
        } if n == name => Some(children),
        _ => None,
    })
}

fn remove_block_if_empty(tree: &mut ConfigTree, name: &str) {
    if tree
        .block(name)
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, name, &[]);
    }
}

/// Remove an emptied `qos { map { <table> { } } }` chain bottom-up. A
/// named `wred-profile <name> { }` block stays: an empty profile is
/// still a defined one.
fn prune_qos(tree: &mut ConfigTree) {
    let qos = tree.block_mut("qos");
    for item in qos.iter_mut() {
        if let Item::Block { name, children, .. } = item {
            if name == "map" {
                children.retain(
                    |child| !matches!(child, Item::Block { children, .. } if children.is_empty()),
                );
            }
        }
    }
    qos.retain(
        |item| !matches!(item, Item::Block { name, children, .. } if name == "map" && children.is_empty()),
    );
    remove_block_if_empty(tree, "qos");
}

/// `Some("")` counts as absent everywhere: the pages send empty inputs
/// for fields the operator left blank.
fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|s| !s.is_empty())
}

// ------------------------------------------------------------ global maps

#[derive(Debug, Deserialize)]
pub struct MapEntrySpec {
    /// The map table (`dscp-to-tc`).
    pub table: String,
    /// The key, as typed: a value, a list, or a range (`40-46,48`).
    pub key: String,
    /// The traffic class (or DSCP/CoS, for the rewrite tables).
    pub value: u32,
}

#[derive(Debug, Deserialize)]
pub struct MapDeleteSpec {
    pub table: String,
    /// The keys to drop, as typed; empty = the whole table.
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct MapEdit {
    #[serde(default)]
    pub set: Vec<MapEntrySpec>,
    #[serde(default)]
    pub delete: Vec<MapDeleteSpec>,
}

/// Replace (or insert) one map entry — a per-value phrase leaf, so a
/// single mapping deletes on its own.
fn set_map_entry(items: &mut Vec<Item>, key_word: &str, key: u8, value_word: &str, value: u8) {
    let key_text = key.to_string();
    for item in items.iter_mut() {
        if let Item::Leaf { name, values } = item {
            if name == key_word && values.first().map(String::as_str) == Some(key_text.as_str()) {
                *values = vec![key_text, value_word.to_string(), value.to_string()];
                return;
            }
        }
    }
    items.push(Item::Leaf {
        name: key_word.to_string(),
        values: vec![key_text, value_word.to_string(), value.to_string()],
    });
}

/// Drop the map entries for `keys` (empty = the whole table).
fn remove_map_entries(items: &mut Vec<Item>, key_word: &str, keys: &[u8]) {
    let wanted: Vec<String> = keys.iter().map(u8::to_string).collect();
    items.retain(|item| {
        !matches!(item, Item::Leaf { name, values }
            if name == key_word
                && (wanted.is_empty()
                    || values
                        .first()
                        .is_some_and(|v| wanted.iter().any(|w| w == v))))
    });
}

pub fn apply_map_edit(tree: &mut ConfigTree, edit: &MapEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    // Validate the whole edit before touching the tree.
    let mut sets = Vec::new();
    for set in &edit.set {
        let (key_word, key_max, value_word, value_max) = map_table(&set.table)?;
        let keys = hemlock_common::net::parse_value_list(&set.key, key_max, key_word)?;
        let value = u8::try_from(set.value)
            .ok()
            .filter(|v| *v <= value_max)
            .ok_or_else(|| format!("bad {value_word} {} (0..{value_max})", set.value))?;
        sets.push((set.table.clone(), key_word, keys, value_word, value));
    }
    let mut deletes = Vec::new();
    for delete in &edit.delete {
        let (key_word, key_max, ..) = map_table(&delete.table)?;
        let keys = if delete.key.is_empty() {
            Vec::new()
        } else {
            hemlock_common::net::parse_value_list(&delete.key, key_max, key_word)?
        };
        deletes.push((delete.table.clone(), key_word, keys));
    }

    for (table, key_word, keys, value_word, value) in sets {
        let qos = tree.block_mut("qos");
        let map = ConfigTree::ensure_block(qos, "map", &[]);
        let entries = ConfigTree::ensure_block(map, &table, &[]);
        for key in keys {
            set_map_entry(entries, key_word, key, value_word, value);
        }
    }
    for (table, key_word, keys) in deletes {
        let qos = tree.block_mut("qos");
        let map = ConfigTree::ensure_block(qos, "map", &[]);
        let entries = ConfigTree::ensure_block(map, &table, &[]);
        remove_map_entries(entries, key_word, &keys);
    }
    prune_qos(tree);
    Ok(())
}

// -------------------------------------------------------- WRED profiles

#[derive(Debug, Deserialize)]
pub struct WredSet {
    pub name: String,
    /// KB, 1..4096; absent leaves the leaf alone, "" removes it.
    #[serde(default)]
    pub min_threshold: Option<String>,
    #[serde(default)]
    pub max_threshold: Option<String>,
    /// Percent, 1..100.
    #[serde(default)]
    pub drop_probability: Option<String>,
    /// Mark instead of drop for ECT traffic.
    #[serde(default)]
    pub ecn: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WredEdit {
    #[serde(default)]
    pub set: Vec<WredSet>,
    /// Profile names to remove. mgmtd refuses one a queue still binds.
    #[serde(default)]
    pub delete: Vec<String>,
}

fn threshold(value: &str, what: &str) -> Result<String, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=4096).contains(n))
        .map(|n| n.to_string())
        .ok_or_else(|| format!("bad {what} {value:?} (1..4096 KB)"))
}

pub fn apply_wred_edit(tree: &mut ConfigTree, edit: &WredEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    for set in &edit.set {
        valid_wred_name(&set.name)?;
        if let Some(value) = nonempty(&set.min_threshold) {
            threshold(value, "min-threshold")?;
        }
        if let Some(value) = nonempty(&set.max_threshold) {
            threshold(value, "max-threshold")?;
        }
        if let Some(value) = nonempty(&set.drop_probability) {
            value
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=100).contains(n))
                .ok_or_else(|| format!("bad drop-probability {value:?} (1..100)"))?;
        }
    }
    for name in &edit.delete {
        valid_wred_name(name)?;
    }

    for set in &edit.set {
        let qos = tree.block_mut("qos");
        let profile = ConfigTree::ensure_block(qos, "wred-profile", &[&set.name]);
        for (field, value) in [
            ("min-threshold", &set.min_threshold),
            ("max-threshold", &set.max_threshold),
            ("drop-probability", &set.drop_probability),
        ] {
            match value {
                None => {}
                Some(text) if text.is_empty() => ConfigTree::remove_leaf(profile, field),
                Some(text) => ConfigTree::set_leaf(profile, field, vec![text.clone()]),
            }
        }
        match set.ecn {
            None => {}
            Some(true) => ConfigTree::set_leaf(profile, "ecn", vec![]),
            Some(false) => ConfigTree::remove_leaf(profile, "ecn"),
        }
    }
    for name in &edit.delete {
        let qos = tree.block_mut("qos");
        ConfigTree::remove_block(qos, "wred-profile", &[name]);
    }
    prune_qos(tree);
    Ok(())
}

// ------------------------------------------------------------- per-port

#[derive(Debug, Deserialize)]
pub struct QueueSpec {
    pub queue: u32,
    /// "strict" | "dwrr".
    #[serde(default)]
    pub mode: Option<String>,
    /// DWRR weight 1..127; "" clears it back to the default.
    #[serde(default)]
    pub weight: Option<String>,
    /// Shaper rate with a k/m/g suffix; "" clears it.
    #[serde(default)]
    pub shape: Option<String>,
    /// WRED profile name; "" clears the binding.
    #[serde(default)]
    pub wred_profile: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PortQosSet {
    /// A physical port or a Port-Channel display name.
    pub name: String,
    /// "dscp" | "cos" | "untrusted"; "" clears the leaf.
    #[serde(default)]
    pub trust: Option<String>,
    /// 0..7; "" clears the leaf.
    #[serde(default)]
    pub default_tc: Option<String>,
    /// Port shaper rate with a k/m/g suffix; "" clears it.
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub queues: Vec<QueueSpec>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PortQosEdit {
    #[serde(default)]
    pub set: Vec<PortQosSet>,
    /// Ports to unconfigure entirely (back to the platform defaults).
    #[serde(default)]
    pub delete: Vec<String>,
}

/// One queue's validated program, ready to write.
struct QueuePlan {
    index: String,
    mode: Option<String>,
    weight: Option<String>,
    shape: Option<String>,
    wred_profile: Option<String>,
}

fn plan_queue(port: &str, spec: &QueueSpec) -> Result<QueuePlan, String> {
    let index = u8::try_from(spec.queue)
        .ok()
        .filter(|q| *q < QUEUE_COUNT)
        .ok_or_else(|| {
            format!(
                "{port}: queue {} is out of range (0..{})",
                spec.queue,
                QUEUE_COUNT - 1
            )
        })?;
    if let Some(mode) = nonempty(&spec.mode) {
        if !matches!(mode, "strict" | "dwrr") {
            return Err(format!(
                "{port} queue {index}: bad mode {mode:?} (strict|dwrr)"
            ));
        }
    }
    // Strict priority and a DWRR weight are mutually exclusive; the
    // page mirrors that rule, and mgmtd enforces it again.
    if nonempty(&spec.mode) == Some("strict") && nonempty(&spec.weight).is_some() {
        return Err(format!(
            "{port} queue {index}: strict and weight are mutually exclusive"
        ));
    }
    if let Some(weight) = nonempty(&spec.weight) {
        weight
            .parse::<u8>()
            .ok()
            .filter(|w| (1..=127).contains(w))
            .ok_or_else(|| format!("{port} queue {index}: bad weight {weight:?} (1..127)"))?;
    }
    let shape = match nonempty(&spec.shape) {
        Some(rate) => Some(hemlock_common::net::format_shape_rate(
            hemlock_common::net::parse_shape_rate(rate)?,
        )),
        None => spec.shape.as_ref().map(|_| String::new()),
    };
    if let Some(profile) = nonempty(&spec.wred_profile) {
        valid_wred_name(profile)?;
    }
    Ok(QueuePlan {
        index: index.to_string(),
        mode: spec.mode.clone(),
        weight: spec.weight.clone(),
        shape,
        wred_profile: spec.wred_profile.clone(),
    })
}

pub fn apply_port_qos_edit(tree: &mut ConfigTree, edit: &PortQosEdit) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }
    let mut plans = Vec::new();
    for set in &edit.set {
        valid_qos_port(&set.name)?;
        if let Some(trust) = nonempty(&set.trust) {
            if !matches!(trust, "dscp" | "cos" | "untrusted") {
                return Err(format!(
                    "{}: bad trust {trust:?} (dscp|cos|untrusted)",
                    set.name
                ));
            }
        }
        if let Some(tc) = nonempty(&set.default_tc) {
            tc.parse::<u8>()
                .ok()
                .filter(|n| *n <= 7)
                .ok_or_else(|| format!("{}: bad default-tc {tc:?} (0..7)", set.name))?;
        }
        let shape = match nonempty(&set.shape) {
            Some(rate) => Some(hemlock_common::net::format_shape_rate(
                hemlock_common::net::parse_shape_rate(rate)?,
            )),
            None => set.shape.as_ref().map(|_| String::new()),
        };
        let queues: Result<Vec<QueuePlan>, String> = set
            .queues
            .iter()
            .map(|spec| plan_queue(&set.name, spec))
            .collect();
        plans.push((
            set.name.clone(),
            set.trust.clone(),
            set.default_tc.clone(),
            shape,
            queues?,
        ));
    }
    for name in &edit.delete {
        valid_qos_port(name)?;
    }

    for (name, trust, default_tc, shape, queues) in plans {
        let interfaces = tree.block_mut("interfaces");
        let eth = ConfigTree::ensure_block(interfaces, &name, &[]);
        let qos = ConfigTree::ensure_block(eth, "qos", &[]);
        for (field, value) in [("trust", &trust), ("default-tc", &default_tc)] {
            match value {
                None => {}
                Some(text) if text.is_empty() => ConfigTree::remove_leaf(qos, field),
                Some(text) => ConfigTree::set_leaf(qos, field, vec![text.clone()]),
            }
        }
        match &shape {
            None => {}
            Some(text) if text.is_empty() => ConfigTree::remove_leaf(qos, "shape"),
            Some(text) => ConfigTree::set_phrase(qos, "shape", "rate", vec![text.clone()]),
        }
        for plan in queues {
            let queue = ConfigTree::ensure_block(qos, "queue", &[&plan.index]);
            match plan.mode.as_deref() {
                None => {}
                Some("strict") => {
                    ConfigTree::remove_leaf(queue, "weight");
                    ConfigTree::set_leaf(queue, "priority", vec!["strict".into()]);
                }
                // "dwrr" (or a cleared mode) drops the strict marker;
                // the weight leaf below carries the share.
                Some(_) => ConfigTree::remove_leaf(queue, "priority"),
            }
            match plan.weight.as_deref() {
                None => {}
                Some("") => ConfigTree::remove_leaf(queue, "weight"),
                Some(text) => {
                    ConfigTree::remove_leaf(queue, "priority");
                    ConfigTree::set_leaf(queue, "weight", vec![text.to_string()]);
                }
            }
            match plan.shape.as_deref() {
                None => {}
                Some("") => ConfigTree::remove_leaf(queue, "shape"),
                Some(text) => {
                    ConfigTree::set_phrase(queue, "shape", "rate", vec![text.to_string()])
                }
            }
            match plan.wred_profile.as_deref() {
                None => {}
                Some("") => ConfigTree::remove_leaf(queue, "wred-profile"),
                Some(text) => ConfigTree::set_leaf(queue, "wred-profile", vec![text.to_string()]),
            }
            // A queue left entirely at the defaults carries no block.
            if queue.is_empty() {
                ConfigTree::remove_block(qos, "queue", &[&plan.index]);
            }
        }
    }
    for name in &edit.delete {
        let interfaces = tree.block_mut("interfaces");
        if let Some(eth) = block_children_mut(interfaces, name) {
            ConfigTree::remove_block(eth, "qos", &[]);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tree(text: &str) -> ConfigTree {
        hemlock_config::parse(text).unwrap()
    }

    #[test]
    fn map_edits_write_per_value_leaves() {
        let mut t = ConfigTree::default();
        apply_map_edit(
            &mut t,
            &MapEdit {
                set: vec![MapEntrySpec {
                    table: "dscp-to-tc".into(),
                    key: "40-42,48".into(),
                    value: 5,
                }],
                delete: Vec::new(),
            },
        )
        .unwrap();
        let text = t.to_text();
        for dscp in [40, 41, 42, 48] {
            assert!(text.contains(&format!("dscp {dscp} tc 5")), "{text}");
        }
        // One value out, then the whole table — which collapses the
        // `map` scaffolding with it.
        apply_map_edit(
            &mut t,
            &MapEdit {
                set: Vec::new(),
                delete: vec![MapDeleteSpec {
                    table: "dscp-to-tc".into(),
                    key: "41".into(),
                }],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("dscp 41"));
        apply_map_edit(
            &mut t,
            &MapEdit {
                set: Vec::new(),
                delete: vec![MapDeleteSpec {
                    table: "dscp-to-tc".into(),
                    key: String::new(),
                }],
            },
        )
        .unwrap();
        assert_eq!(t.to_text().trim(), "");
    }

    #[test]
    fn map_edits_validate_before_writing() {
        let mut t = ConfigTree::default();
        let bad = |table: &str, key: &str, value: u32| {
            apply_map_edit(
                &mut ConfigTree::default(),
                &MapEdit {
                    set: vec![MapEntrySpec {
                        table: table.into(),
                        key: key.into(),
                        value,
                    }],
                    delete: Vec::new(),
                },
            )
            .unwrap_err()
        };
        assert!(bad("banana", "1", 1).contains("bad map table"));
        assert!(bad("dscp-to-tc", "64", 5).contains("bad dscp"));
        assert!(bad("dscp-to-tc", "46", 9).contains("bad tc"));
        assert!(bad("tc-to-dscp", "3", 64).contains("bad dscp"));
        // Nothing was written by the failed edits.
        assert!(apply_map_edit(&mut t, &MapEdit::default()).is_err());
        assert_eq!(t.to_text().trim(), "");
    }

    #[test]
    fn wred_edits_round_trip_the_profile() {
        let mut t = ConfigTree::default();
        apply_wred_edit(
            &mut t,
            &WredEdit {
                set: vec![WredSet {
                    name: "BULK".into(),
                    min_threshold: Some("64".into()),
                    max_threshold: Some("256".into()),
                    drop_probability: Some("10".into()),
                    ecn: Some(true),
                }],
                delete: Vec::new(),
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("wred-profile BULK"));
        assert!(text.contains("min-threshold 64"));
        assert!(text.contains("ecn"));
        // Clearing ECN and a threshold drops just those leaves.
        apply_wred_edit(
            &mut t,
            &WredEdit {
                set: vec![WredSet {
                    name: "BULK".into(),
                    min_threshold: None,
                    max_threshold: Some(String::new()),
                    drop_probability: None,
                    ecn: Some(false),
                }],
                delete: Vec::new(),
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("min-threshold 64"));
        assert!(!text.contains("max-threshold"));
        assert!(!text.contains("ecn"));

        apply_wred_edit(
            &mut t,
            &WredEdit {
                set: Vec::new(),
                delete: vec!["BULK".into()],
            },
        )
        .unwrap();
        assert_eq!(t.to_text().trim(), "");

        // Bad names and out-of-range thresholds never reach the tree.
        assert!(apply_wred_edit(
            &mut ConfigTree::default(),
            &WredEdit {
                set: vec![WredSet {
                    name: "9BAD".into(),
                    min_threshold: None,
                    max_threshold: None,
                    drop_probability: None,
                    ecn: None,
                }],
                delete: Vec::new(),
            }
        )
        .unwrap_err()
        .contains("bad WRED profile name"));
    }

    #[test]
    fn port_qos_edits_write_the_cli_form() {
        let mut t = tree("interfaces { Ethernet1 { } }");
        apply_port_qos_edit(
            &mut t,
            &PortQosEdit {
                set: vec![PortQosSet {
                    name: "Ethernet1".into(),
                    trust: Some("dscp".into()),
                    default_tc: Some("1".into()),
                    shape: None,
                    queues: vec![
                        QueueSpec {
                            queue: 7,
                            mode: Some("strict".into()),
                            weight: None,
                            shape: None,
                            wred_profile: None,
                        },
                        QueueSpec {
                            queue: 5,
                            mode: Some("dwrr".into()),
                            weight: Some("40".into()),
                            shape: Some("100m".into()),
                            wred_profile: None,
                        },
                        QueueSpec {
                            queue: 3,
                            mode: Some("dwrr".into()),
                            weight: Some("30".into()),
                            shape: None,
                            wred_profile: Some("BULK".into()),
                        },
                    ],
                }],
                delete: Vec::new(),
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("trust dscp"));
        assert!(text.contains("default-tc 1"));
        assert!(text.contains("priority strict"));
        assert!(text.contains("weight 40"));
        assert!(text.contains("shape rate 100m"));
        assert!(text.contains("wred-profile BULK"));
        // The written form is exactly what the CLI writes, so it
        // re-parses and extracts.
        let parsed = hemlock_config::parse(&text).unwrap();
        assert_eq!(parsed, hemlock_config::parse(&parsed.to_text()).unwrap());

        // Strict and weight together is refused by the page's mirror of
        // the rule, before mgmtd ever sees it.
        assert!(apply_port_qos_edit(
            &mut tree("interfaces { Ethernet1 { } }"),
            &PortQosEdit {
                set: vec![PortQosSet {
                    name: "Ethernet1".into(),
                    trust: None,
                    default_tc: None,
                    shape: None,
                    queues: vec![QueueSpec {
                        queue: 5,
                        mode: Some("strict".into()),
                        weight: Some("40".into()),
                        shape: None,
                        wred_profile: None,
                    }],
                }],
                delete: Vec::new(),
            }
        )
        .unwrap_err()
        .contains("strict and weight are mutually exclusive"));

        // Deleting the port's program drops the whole block.
        apply_port_qos_edit(
            &mut t,
            &PortQosEdit {
                set: Vec::new(),
                delete: vec!["Ethernet1".into()],
            },
        )
        .unwrap();
        assert!(!t.to_text().contains("qos"));
    }

    #[test]
    fn port_qos_edits_reject_non_front_panel_interfaces() {
        for name in ["Vlan10", "Management1", "Loopback0"] {
            let err = apply_port_qos_edit(
                &mut ConfigTree::default(),
                &PortQosEdit {
                    set: vec![PortQosSet {
                        name: name.into(),
                        trust: Some("dscp".into()),
                        default_tc: None,
                        shape: None,
                        queues: Vec::new(),
                    }],
                    delete: Vec::new(),
                },
            )
            .unwrap_err();
            assert!(err.contains("front-panel"), "{err}");
        }
    }

    #[test]
    fn shaper_rates_canonicalize_and_validate() {
        let mut t = tree("interfaces { Port-Channel1 { } }");
        apply_port_qos_edit(
            &mut t,
            &PortQosEdit {
                set: vec![PortQosSet {
                    name: "Port-Channel1".into(),
                    trust: None,
                    default_tc: None,
                    shape: Some("800000000".into()),
                    queues: Vec::new(),
                }],
                delete: Vec::new(),
            },
        )
        .unwrap();
        // The suffixed config form is canonical, whatever the page sent.
        assert!(t.to_text().contains("shape rate 800m"));

        assert!(apply_port_qos_edit(
            &mut ConfigTree::default(),
            &PortQosEdit {
                set: vec![PortQosSet {
                    name: "Ethernet1".into(),
                    trust: None,
                    default_tc: None,
                    shape: Some("32k".into()),
                    queues: Vec::new(),
                }],
                delete: Vec::new(),
            }
        )
        .unwrap_err()
        .contains("64k shaper granularity floor"));
    }
}
