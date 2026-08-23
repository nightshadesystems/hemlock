//! Config edits for the routing pages: static routes (ECMP-aware).
//!
//! Same discipline as `edit.rs` / `switching_edit.rs`: the builders
//! write exactly the leaves hemlockctl writes — one `routing { static {
//! <prefix> <next-hop> [distance <n>] } }` leaf per next hop, or a
//! single `<prefix> drop` leaf — and the result goes through mgmtd's
//! normal SetCandidate + Commit path, so validation, the rollback ring,
//! and `show configuration` behave as if the change came from the CLI.

use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct StaticRouteSet {
    pub prefix: String,
    /// The whole wanted next-hop set for the prefix — existing lines
    /// are replaced wholesale, so an edit modal round-trips cleanly.
    #[serde(default)]
    pub next_hops: Vec<String>,
    /// Null route; mutually exclusive with next hops.
    #[serde(default)]
    pub drop: bool,
    /// Administrative distance; absent = 1.
    #[serde(default)]
    pub distance: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct StaticRouteDelete {
    pub prefix: String,
    /// Delete just this next hop; absent = the whole prefix.
    #[serde(default)]
    pub next_hop: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct StaticRouteEdit {
    #[serde(default)]
    pub set: Vec<StaticRouteSet>,
    #[serde(default)]
    pub delete: Vec<StaticRouteDelete>,
}

pub fn apply_static_route_edit(
    tree: &mut ConfigTree,
    edit: &StaticRouteEdit,
) -> Result<(), String> {
    if edit.set.is_empty() && edit.delete.is_empty() {
        return Err("nothing to change".into());
    }

    // Validate everything first; the tree stays untouched on rejection.
    let mut sets: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for set in &edit.set {
        let prefix = hemlock_common::net::require_canonical_prefix(&set.prefix)
            .map_err(|e| format!("{}: {e}", set.prefix))?;
        if set.drop && !set.next_hops.is_empty() {
            return Err(format!(
                "{prefix}: drop and next hops are mutually exclusive"
            ));
        }
        if !set.drop && set.next_hops.is_empty() {
            return Err(format!(
                "{prefix}: at least one next hop (or drop) is required"
            ));
        }
        let distance = match set.distance {
            None => 1,
            Some(d) if (1..=255).contains(&d) => d,
            Some(d) => return Err(format!("{prefix}: bad distance {d} (1..255)")),
        };
        let lines = if set.drop {
            vec![vec!["drop".to_string()]]
        } else {
            let mut lines = Vec::new();
            for next_hop in &set.next_hops {
                hemlock_common::net::validate_next_hop(&prefix, next_hop)
                    .map_err(|e| format!("{prefix}: {e}"))?;
                let mut values = vec![next_hop.clone()];
                if distance != 1 {
                    values.extend(["distance".to_string(), distance.to_string()]);
                }
                if !lines.contains(&values) {
                    lines.push(values);
                }
            }
            lines
        };
        sets.push((prefix, lines));
    }
    let mut deletes: Vec<(String, Option<String>)> = Vec::new();
    for delete in &edit.delete {
        let prefix = hemlock_common::net::require_canonical_prefix(&delete.prefix)
            .map_err(|e| format!("{}: {e}", delete.prefix))?;
        deletes.push((prefix, delete.next_hop.clone()));
    }

    // Mutate.
    {
        let routing = tree.block_mut("routing");
        let routes = ConfigTree::ensure_block(routing, "static", &[]);
        for (prefix, lines) in sets {
            routes.retain(|item| !matches!(item, Item::Leaf { name, .. } if *name == prefix));
            for values in lines {
                routes.push(Item::Leaf {
                    name: prefix.clone(),
                    values,
                });
            }
        }
        for (prefix, next_hop) in deletes {
            routes.retain(|item| match item {
                Item::Leaf { name, values } if *name == prefix => match &next_hop {
                    Some(hop) => values.first() != Some(hop),
                    None => false,
                },
                _ => true,
            });
        }
        if routes.is_empty() {
            ConfigTree::remove_block(routing, "static", &[]);
        }
    }
    if tree
        .block("routing")
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, "routing", &[]);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tree(text: &str) -> ConfigTree {
        let mut tree = hemlock_config::parse(text).unwrap();
        tree.normalize_interfaces();
        tree
    }

    #[test]
    fn static_route_create_update_delete() {
        let mut t = tree("");

        // ECMP set with default distance.
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![StaticRouteSet {
                    prefix: "10.99.0.0/16".into(),
                    next_hops: vec!["10.9.9.0".into(), "10.42.10.7".into()],
                    drop: false,
                    distance: None,
                }],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("10.99.0.0/16 10.9.9.0"));
        assert!(text.contains("10.99.0.0/16 10.42.10.7"));
        assert_eq!(hemlock_config::parse(&text).unwrap(), t);

        // Replace wholesale with a drop route; add a distance route.
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![
                    StaticRouteSet {
                        prefix: "10.99.0.0/16".into(),
                        next_hops: vec![],
                        drop: true,
                        distance: None,
                    },
                    StaticRouteSet {
                        prefix: "172.16.0.0/12".into(),
                        next_hops: vec!["10.42.10.1".into()],
                        drop: false,
                        distance: Some(250),
                    },
                ],
                delete: vec![],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(text.contains("10.99.0.0/16 drop"));
        assert!(!text.contains("10.9.9.0"));
        assert!(text.contains("172.16.0.0/12 10.42.10.1 distance 250"));

        // Delete one next hop of an ECMP set, then a whole prefix; the
        // emptied blocks disappear.
        let mut t = tree(
            "routing {\n    static {\n        10.99.0.0/16 10.9.9.0\n        10.99.0.0/16 10.42.10.7\n        192.0.2.0/24 drop\n    }\n}\n",
        );
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![],
                delete: vec![StaticRouteDelete {
                    prefix: "10.99.0.0/16".into(),
                    next_hop: Some("10.9.9.0".into()),
                }],
            },
        )
        .unwrap();
        let text = t.to_text();
        assert!(!text.contains("10.9.9.0"));
        assert!(text.contains("10.99.0.0/16 10.42.10.7"));
        apply_static_route_edit(
            &mut t,
            &StaticRouteEdit {
                set: vec![],
                delete: vec![
                    StaticRouteDelete {
                        prefix: "10.99.0.0/16".into(),
                        next_hop: None,
                    },
                    StaticRouteDelete {
                        prefix: "192.0.2.0/24".into(),
                        next_hop: None,
                    },
                ],
            },
        )
        .unwrap();
        assert_eq!(t.to_text(), "");
    }

    #[test]
    fn rejects_bad_static_route_edits() {
        let set =
            |prefix: &str, hops: &[&str], drop: bool, distance: Option<u16>| StaticRouteEdit {
                set: vec![StaticRouteSet {
                    prefix: prefix.into(),
                    next_hops: hops.iter().map(|h| h.to_string()).collect(),
                    drop,
                    distance,
                }],
                delete: vec![],
            };
        let mut t = tree("");
        let err = apply_static_route_edit(&mut t, &StaticRouteEdit::default()).unwrap_err();
        assert!(err.contains("nothing to change"));
        let err = apply_static_route_edit(&mut t, &set("10.99.1.0/16", &["10.0.0.1"], false, None))
            .unwrap_err();
        assert!(err.contains("host bits set; did you mean 10.99.0.0/16?"));
        let err =
            apply_static_route_edit(&mut t, &set("10.99.0.0/16", &[], false, None)).unwrap_err();
        assert!(err.contains("at least one next hop"));
        let err = apply_static_route_edit(&mut t, &set("10.99.0.0/16", &["10.0.0.1"], true, None))
            .unwrap_err();
        assert!(err.contains("mutually exclusive"));
        let err =
            apply_static_route_edit(&mut t, &set("10.99.0.0/16", &["2001:db8::1"], false, None))
                .unwrap_err();
        assert!(err.contains("does not match the address family"));
        let err =
            apply_static_route_edit(&mut t, &set("10.99.0.0/16", &["10.0.0.1"], false, Some(0)))
                .unwrap_err();
        assert!(err.contains("bad distance 0 (1..255)"));
        // Nothing landed in the tree.
        assert_eq!(t.to_text(), "");
    }
}
