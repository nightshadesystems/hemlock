//! Interface intents: the slice of the config tree mgmtd knows how to
//! apply in phase 1 (`interfaces { ethernet <name> { ... } }`).
//!
//! Later phases add more intent families (vlans, lags, routing policy);
//! each stays a pure function from config tree to typed intent, diffed
//! against the running tree and pushed to the owning daemon.

use std::collections::BTreeMap;

use hemlock_config::ConfigTree;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterfaceIntent {
    /// None = leave the daemon default (up) untouched.
    pub admin_up: Option<bool>,
    pub description: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IntentError {
    #[error("interfaces: ethernet block needs exactly one name key")]
    BadEthernetKey,

    #[error("interface {name}: admin-state must be `enabled` or `disabled`, got {value:?}")]
    BadAdminState { name: String, value: String },

    #[error("interface {name}: duplicate ethernet block")]
    Duplicate { name: String },
}

/// Extract per-interface intents from a config tree.
pub fn interfaces(tree: &ConfigTree) -> Result<BTreeMap<String, InterfaceIntent>, IntentError> {
    let mut intents = BTreeMap::new();
    let Some((_, items)) = tree.block("interfaces") else {
        return Ok(intents);
    };

    for (keys, children) in ConfigTree::blocks_named(items, "ethernet") {
        let [name] = keys else {
            return Err(IntentError::BadEthernetKey);
        };
        let mut intent = InterfaceIntent::default();

        if let Some(value) = ConfigTree::leaf_value(children, "admin-state") {
            intent.admin_up = match value {
                "enabled" => Some(true),
                "disabled" => Some(false),
                other => {
                    return Err(IntentError::BadAdminState {
                        name: name.clone(),
                        value: other.to_string(),
                    })
                }
            };
        }
        if let Some(value) = ConfigTree::leaf_value(children, "description") {
            intent.description = Some(value.to_string());
        }

        if intents.insert(name.clone(), intent).is_some() {
            return Err(IntentError::Duplicate { name: name.clone() });
        }
    }
    Ok(intents)
}

/// One change to push to syncd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortChange {
    pub name: String,
    pub admin_up: Option<bool>,
    pub description: Option<String>,
}

impl PortChange {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(up) = self.admin_up {
            parts.push(format!(
                "admin-state {}",
                if up { "enabled" } else { "disabled" }
            ));
        }
        if let Some(desc) = &self.description {
            parts.push(format!("description {desc:?}"));
        }
        format!("{}: {}", self.name, parts.join(", "))
    }
}

/// Diff candidate intents against running intents.
///
/// An interface that disappears from the config reverts to defaults
/// (admin up, empty description).
pub fn diff(
    running: &BTreeMap<String, InterfaceIntent>,
    candidate: &BTreeMap<String, InterfaceIntent>,
) -> Vec<PortChange> {
    let mut changes = Vec::new();

    for (name, wanted) in candidate {
        let current = running.get(name);
        let admin_now = current.and_then(|c| c.admin_up);
        let desc_now = current.and_then(|c| c.description.clone());

        let admin_up = match (wanted.admin_up, admin_now) {
            (Some(w), Some(n)) if w == n => None,
            (Some(w), Some(_)) => Some(w),
            (Some(w), None) => Some(w),
            // Intent removed -> back to default (up).
            (None, Some(false)) => Some(true),
            (None, _) => None,
        };
        let description = match (&wanted.description, &desc_now) {
            (Some(w), Some(n)) if w == n => None,
            (Some(w), _) => Some(w.clone()),
            (None, Some(n)) if !n.is_empty() => Some(String::new()),
            (None, _) => None,
        };

        if admin_up.is_some() || description.is_some() {
            changes.push(PortChange {
                name: name.clone(),
                admin_up,
                description,
            });
        }
    }

    // Interfaces configured before but absent now: revert to defaults.
    for (name, had) in running {
        if candidate.contains_key(name) {
            continue;
        }
        let admin_up = matches!(had.admin_up, Some(false)).then_some(true);
        let description = had
            .description
            .as_ref()
            .filter(|d| !d.is_empty())
            .map(|_| String::new());
        if admin_up.is_some() || description.is_some() {
            changes.push(PortChange {
                name: name.clone(),
                admin_up,
                description,
            });
        }
    }

    changes
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use hemlock_config::parse;

    fn intents_of(text: &str) -> BTreeMap<String, InterfaceIntent> {
        interfaces(&parse(text).unwrap()).unwrap()
    }

    #[test]
    fn extracts_interface_intents() {
        let intents = intents_of(
            r#"
interfaces {
    ethernet Ethernet0 {
        description "uplink";
        admin-state disabled;
    }
    ethernet Ethernet1 {
        admin-state enabled;
    }
}
"#,
        );
        assert_eq!(intents.len(), 2);
        assert_eq!(
            intents["Ethernet0"],
            InterfaceIntent {
                admin_up: Some(false),
                description: Some("uplink".into())
            }
        );
        assert_eq!(intents["Ethernet1"].admin_up, Some(true));
    }

    #[test]
    fn rejects_bad_admin_state() {
        let tree = parse("interfaces { ethernet Ethernet0 { admin-state banana; } }").unwrap();
        assert!(matches!(
            interfaces(&tree),
            Err(IntentError::BadAdminState { .. })
        ));
    }

    #[test]
    fn diff_only_reports_changes() {
        let running = intents_of(
            "interfaces { ethernet Ethernet0 { admin-state disabled; description \"a\"; } }",
        );
        let unchanged = diff(&running, &running);
        assert!(unchanged.is_empty());

        let candidate = intents_of(
            "interfaces { ethernet Ethernet0 { admin-state enabled; description \"a\"; } }",
        );
        let changes = diff(&running, &candidate);
        assert_eq!(
            changes,
            vec![PortChange {
                name: "Ethernet0".into(),
                admin_up: Some(true),
                description: None,
            }]
        );
    }

    #[test]
    fn removed_interface_reverts_to_defaults() {
        let running = intents_of(
            "interfaces { ethernet Ethernet5 { admin-state disabled; description \"x\"; } }",
        );
        let candidate = intents_of("");
        let changes = diff(&running, &candidate);
        assert_eq!(
            changes,
            vec![PortChange {
                name: "Ethernet5".into(),
                admin_up: Some(true),
                description: Some(String::new()),
            }]
        );
    }
}
