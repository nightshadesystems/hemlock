//! The config tree: what a parsed configuration *is*.

use std::fmt::Write as _;

/// One statement in a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// `name value1 value2;`
    Leaf { name: String, values: Vec<String> },
    /// `name key1 key2 { children }` — keys distinguish instances
    /// (`ethernet Ethernet0 { ... }`).
    Block {
        name: String,
        keys: Vec<String>,
        children: Vec<Item>,
    },
}

impl Item {
    pub fn name(&self) -> &str {
        match self {
            Item::Leaf { name, .. } | Item::Block { name, .. } => name,
        }
    }
}

/// A whole configuration (the anonymous top-level block).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigTree {
    pub items: Vec<Item>,
}

impl ConfigTree {
    /// First top-level block with this name.
    pub fn block(&self, name: &str) -> Option<(&[String], &[Item])> {
        block_in(&self.items, name)
    }

    /// All blocks named `name` among `items`, e.g. every
    /// `ethernet <key> { ... }` under `interfaces`.
    pub fn blocks_named<'a>(
        items: &'a [Item],
        name: &str,
    ) -> impl Iterator<Item = (&'a [String], &'a [Item])> {
        let name = name.to_string();
        items.iter().filter_map(move |item| match item {
            Item::Block {
                name: n,
                keys,
                children,
            } if *n == name => Some((keys.as_slice(), children.as_slice())),
            _ => None,
        })
    }

    /// Value of a single-valued leaf among `items`.
    pub fn leaf_value<'a>(items: &'a [Item], name: &str) -> Option<&'a str> {
        items.iter().find_map(|item| match item {
            Item::Leaf { name: n, values } if n == name => values.first().map(String::as_str),
            _ => None,
        })
    }

    /// All values of a leaf among `items` (`trunk-vlans 10 20 30`).
    pub fn leaf_values<'a>(items: &'a [Item], name: &str) -> Option<&'a [String]> {
        items.iter().find_map(|item| match item {
            Item::Leaf { name: n, values } if n == name => Some(values.as_slice()),
            _ => None,
        })
    }

    /// Is a leaf named `name` present among `items`? (For value-less
    /// marker leaves like `shutdown`.)
    pub fn has_leaf(items: &[Item], name: &str) -> bool {
        items
            .iter()
            .any(|item| matches!(item, Item::Leaf { name: n, .. } if n == name))
    }

    /// Render canonical text (4-space indent, quoted where needed,
    /// newline-terminated statements — no trailing semicolons).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            render(&mut out, item, 0);
        }
        out
    }
}

impl ConfigTree {
    /// Mutable access to a top-level block's children, creating the block
    /// if absent (appended at the end, keys empty when created).
    pub fn block_mut(&mut self, name: &str) -> &mut Vec<Item> {
        Self::ensure_block(&mut self.items, name, &[])
    }

    /// Mutable children of the block `name key...` among `items`, creating
    /// it (appended) if absent.
    pub fn ensure_block<'a>(
        items: &'a mut Vec<Item>,
        name: &str,
        keys: &[&str],
    ) -> &'a mut Vec<Item> {
        let position = items.iter().position(|item| match item {
            Item::Block {
                name: n, keys: k, ..
            } => n == name && k.iter().map(String::as_str).eq(keys.iter().copied()),
            _ => false,
        });
        let index = match position {
            Some(index) => index,
            None => {
                items.push(Item::Block {
                    name: name.to_string(),
                    keys: keys.iter().map(|k| k.to_string()).collect(),
                    children: Vec::new(),
                });
                items.len() - 1
            }
        };
        match &mut items[index] {
            Item::Block { children, .. } => children,
            Item::Leaf { .. } => unreachable!("position matched a Block"),
        }
    }

    /// Set (replace or insert) a leaf `name values...;` among `items`.
    pub fn set_leaf(items: &mut Vec<Item>, name: &str, values: Vec<String>) {
        for item in items.iter_mut() {
            if let Item::Leaf { name: n, values: v } = item {
                if n == name {
                    *v = values;
                    return;
                }
            }
        }
        items.push(Item::Leaf {
            name: name.to_string(),
            values,
        });
    }

    /// Remove every leaf named `name` among `items`.
    pub fn remove_leaf(items: &mut Vec<Item>, name: &str) {
        items.retain(|item| !matches!(item, Item::Leaf { name: n, .. } if n == name));
    }

    /// Remove the block `name key...` (and its whole subtree) among `items`.
    pub fn remove_block(items: &mut Vec<Item>, name: &str, keys: &[&str]) {
        items.retain(|item| {
            !matches!(item, Item::Block { name: n, keys: k, .. }
                if n == name && k.iter().map(String::as_str).eq(keys.iter().copied()))
        });
    }

    /// Migrate legacy interface config forms in place, so configs
    /// persisted before a format change keep loading cleanly:
    ///
    /// - `interfaces { ethernet <name> { ... } }` (and `management
    ///   <name>`) become the name-as-block form (`Ethernet1 { ... }`);
    /// - hyphenated keywords become their spelled-out phrases
    ///   (`no-shutdown` -> `no shutdown`, `admin-state enabled|disabled`
    ///   -> `no shutdown`/`shutdown`, and in `switchport` blocks
    ///   `access-vlan`/`trunk-vlans`/`native-vlan` ->
    ///   `access vlan`/`trunk vlans`/`native vlan`).
    pub fn normalize_interfaces(&mut self) {
        for item in &mut self.items {
            let Item::Block { name, children, .. } = item else {
                continue;
            };
            if name != "interfaces" {
                continue;
            }
            for child in children {
                let Item::Block {
                    name,
                    keys,
                    children,
                } = child
                else {
                    continue;
                };
                if matches!(name.as_str(), "ethernet" | "management") && keys.len() == 1 {
                    *name = keys.remove(0);
                }
                normalize_admin_leaves(children);
                for sub in children {
                    if let Item::Block {
                        name, children: sp, ..
                    } = sub
                    {
                        if name == "switchport" {
                            normalize_switchport_leaves(sp);
                        }
                    }
                }
            }
        }
    }

    /// Marker phrase present? (`no shutdown` = leaf `no` whose first
    /// value is `shutdown`.)
    pub fn has_phrase(items: &[Item], first: &str, second: &str) -> bool {
        Self::phrase_values(items, first, second).is_some()
    }

    /// The values after a two-word phrase leaf: for `access vlan 10`,
    /// `phrase_values(items, "access", "vlan")` is `["10"]`.
    pub fn phrase_values<'a>(items: &'a [Item], first: &str, second: &str) -> Option<&'a [String]> {
        items.iter().find_map(|item| match item {
            Item::Leaf { name, values }
                if name == first && values.first().map(String::as_str) == Some(second) =>
            {
                Some(&values[1..])
            }
            _ => None,
        })
    }

    /// Set (replace or insert) a phrase leaf `first second rest...`.
    pub fn set_phrase(items: &mut Vec<Item>, first: &str, second: &str, rest: Vec<String>) {
        let mut values = vec![second.to_string()];
        values.extend(rest);
        Self::set_leaf(items, first, values);
    }
}

/// `shutdown` / `no shutdown` from the legacy `no-shutdown` and
/// `admin-state enabled|disabled` leaves. Invalid admin-state values are
/// left alone for the intent parser to report.
fn normalize_admin_leaves(children: &mut [Item]) {
    for item in children.iter_mut() {
        let Item::Leaf { name, values } = item else {
            continue;
        };
        match name.as_str() {
            "no-shutdown" => {
                *name = "no".into();
                *values = vec!["shutdown".into()];
            }
            "admin-state" => match values.first().map(String::as_str) {
                Some("enabled") => {
                    *name = "no".into();
                    *values = vec!["shutdown".into()];
                }
                Some("disabled") => {
                    *name = "shutdown".into();
                    values.clear();
                }
                _ => {}
            },
            _ => {}
        }
    }
}

/// Hyphenated switchport keywords to their phrase forms.
fn normalize_switchport_leaves(children: &mut [Item]) {
    for item in children.iter_mut() {
        let Item::Leaf { name, values } = item else {
            continue;
        };
        let (new_name, phrase) = match name.as_str() {
            "access-vlan" => ("access", "vlan"),
            "trunk-vlans" => ("trunk", "vlans"),
            "native-vlan" => ("native", "vlan"),
            _ => continue,
        };
        *name = new_name.into();
        values.insert(0, phrase.into());
    }
}

// --------------------------------------------------------- redaction

impl ConfigTree {
    /// Replace every stored secret with `<hidden>`.
    ///
    /// Which leaves are secret is policy, but it is policy every reader
    /// of a configuration needs — `show configuration`, the web
    /// console, and the tech-support bundle all have to agree, or one
    /// of them leaks what the others hide. So the list lives here, with
    /// the language, and there is exactly one of it.
    ///
    /// Today: RADIUS shared keys (`security { dot1x { radius-server
    /// <ip> { key ... } } }`), SNMP v3 passphrases (`services { snmp {
    /// user ... } }`) and login password hashes (`system { login {
    /// user <name> { password-hash ... } } }`).
    pub fn redact_secrets(&mut self) {
        redact_secrets_impl(self);
    }
}

/// The RADIUS half; the other two families have their own passes.
fn redact_secrets_impl(tree: &mut ConfigTree) {
    redact_snmp_users(tree);
    redact_login_hashes(tree);
    let Some(security) = tree.items.iter_mut().find_map(|item| match item {
        Item::Block { name, children, .. } if name == "security" => Some(children),
        _ => None,
    }) else {
        return;
    };
    for item in security.iter_mut() {
        let Item::Block { name, children, .. } = item else {
            continue;
        };
        if name != "dot1x" {
            continue;
        }
        for server in children.iter_mut() {
            let Item::Block { name, children, .. } = server else {
                continue;
            };
            if name != "radius-server" {
                continue;
            }
            for leaf in children.iter_mut() {
                if let Item::Leaf { name, values } = leaf {
                    if name == "key" {
                        *values = vec!["<hidden>".into()];
                    }
                }
            }
        }
    }
}

/// `system { login { user <name> { password-hash "$6$..." } } }`: the
/// crypt string is a secret like any other stored credential, so it
/// follows the established convention and renders as `<hidden>`. The
/// ssh keys beside it are public by construction and stay.
fn redact_login_hashes(tree: &mut ConfigTree) {
    let Some(system) = tree.items.iter_mut().find_map(|item| match item {
        Item::Block { name, children, .. } if name == "system" => Some(children),
        _ => None,
    }) else {
        return;
    };
    for item in system.iter_mut() {
        let Item::Block { name, children, .. } = item else {
            continue;
        };
        if name != "login" {
            continue;
        }
        for user in children.iter_mut() {
            let Item::Block { name, children, .. } = user else {
                continue;
            };
            if name != "user" {
                continue;
            }
            for leaf in children.iter_mut() {
                if let Item::Leaf { name, values } = leaf {
                    if name == "password-hash" {
                        *values = vec!["<hidden>".into()];
                    }
                }
            }
        }
    }
}

/// `services { snmp { user <name> auth sha <pass> priv aes <pass> } }`:
/// both passphrases render as `<hidden>`, the protocol keywords stay
/// so the line still reads as configuration.
fn redact_snmp_users(tree: &mut ConfigTree) {
    let Some(services) = tree.items.iter_mut().find_map(|item| match item {
        Item::Block { name, children, .. } if name == "services" => Some(children),
        _ => None,
    }) else {
        return;
    };
    for item in services.iter_mut() {
        let Item::Block { name, children, .. } = item else {
            continue;
        };
        if name != "snmp" {
            continue;
        }
        for leaf in children.iter_mut() {
            let Item::Leaf { name, values } = leaf else {
                continue;
            };
            // `<user> auth sha <pass> priv aes <pass>`: the two
            // passwords sit at index 3 and 6.
            if name == "user" && values.len() == 7 {
                values[3] = "<hidden>".into();
                values[6] = "<hidden>".into();
            }
        }
    }
}

fn block_in<'a>(items: &'a [Item], name: &str) -> Option<(&'a [String], &'a [Item])> {
    items.iter().find_map(|item| match item {
        Item::Block {
            name: n,
            keys,
            children,
        } if n == name => Some((keys.as_slice(), children.as_slice())),
        _ => None,
    })
}

/// Quote a word unless it is safe as a bare token.
fn atom(word: &str) -> String {
    let bare = !word.is_empty() && word.chars().all(crate::lexer::is_word_char);
    if bare {
        word.to_string()
    } else {
        let escaped = word.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

fn render(out: &mut String, item: &Item, depth: usize) {
    let indent = "    ".repeat(depth);
    match item {
        Item::Leaf { name, values } => {
            let _ = write!(out, "{indent}{name}");
            for value in values {
                let _ = write!(out, " {}", atom(value));
            }
            out.push('\n');
        }
        Item::Block {
            name,
            keys,
            children,
        } => {
            let _ = write!(out, "{indent}{name}");
            for key in keys {
                let _ = write!(out, " {}", atom(key));
            }
            out.push_str(" {\n");
            for child in children {
                render(out, child, depth + 1);
            }
            let _ = writeln!(out, "{indent}}}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> ConfigTree {
        ConfigTree {
            items: vec![
                Item::Block {
                    name: "system".into(),
                    keys: vec![],
                    children: vec![Item::Leaf {
                        name: "hostname".into(),
                        values: vec!["sw1".into()],
                    }],
                },
                Item::Block {
                    name: "interfaces".into(),
                    keys: vec![],
                    children: vec![Item::Block {
                        name: "ethernet".into(),
                        keys: vec!["Ethernet0".into()],
                        children: vec![Item::Leaf {
                            name: "description".into(),
                            values: vec!["uplink to core".into()],
                        }],
                    }],
                },
            ],
        }
    }

    #[test]
    fn renders_canonical_text() {
        let text = sample().to_text();
        assert_eq!(
            text,
            "system {\n    hostname sw1\n}\ninterfaces {\n    ethernet Ethernet0 {\n        description \"uplink to core\"\n    }\n}\n"
        );
    }

    #[test]
    fn mutation_creates_and_updates() {
        let mut tree = ConfigTree::default();
        {
            let interfaces = tree.block_mut("interfaces");
            let eth = ConfigTree::ensure_block(interfaces, "ethernet", &["Ethernet5"]);
            ConfigTree::set_leaf(eth, "description", vec!["uplink".into()]);
            ConfigTree::set_leaf(eth, "admin-state", vec!["disabled".into()]);
            // Overwrite an existing leaf in place.
            ConfigTree::set_leaf(eth, "description", vec!["core uplink".into()]);
        }
        let (_, interfaces) = tree.block("interfaces").unwrap();
        let (keys, eth) = ConfigTree::blocks_named(interfaces, "ethernet")
            .next()
            .unwrap();
        assert_eq!(keys, ["Ethernet5"]);
        assert_eq!(
            ConfigTree::leaf_value(eth, "description"),
            Some("core uplink")
        );
        assert_eq!(ConfigTree::leaf_value(eth, "admin-state"), Some("disabled"));

        // ensure_block with the same key must not duplicate.
        let interfaces = tree.block_mut("interfaces");
        ConfigTree::ensure_block(interfaces, "ethernet", &["Ethernet5"]);
        assert_eq!(ConfigTree::blocks_named(interfaces, "ethernet").count(), 1);

        // remove_leaf drops it; round-trips through text.
        let eth = ConfigTree::ensure_block(interfaces, "ethernet", &["Ethernet5"]);
        ConfigTree::remove_leaf(eth, "admin-state");
        let text = tree.to_text();
        let reparsed = crate::parse(&text).unwrap();
        assert_eq!(reparsed, tree);
        let (_, interfaces) = reparsed.block("interfaces").unwrap();
        let (_, eth) = ConfigTree::blocks_named(interfaces, "ethernet")
            .next()
            .unwrap();
        assert_eq!(ConfigTree::leaf_value(eth, "admin-state"), None);
    }

    #[test]
    fn normalizes_legacy_interface_blocks() {
        let mut tree = crate::parse(
            "interfaces { ethernet Ethernet1 { admin-state disabled } management Management1 { } Ethernet2 { } }",
        )
        .unwrap();
        tree.normalize_interfaces();
        let (_, interfaces) = tree.block("interfaces").unwrap();
        let names: Vec<&str> = interfaces.iter().map(Item::name).collect();
        assert_eq!(names, ["Ethernet1", "Management1", "Ethernet2"]);
        let (keys, children) = ConfigTree::blocks_named(interfaces, "Ethernet1")
            .next()
            .unwrap();
        assert!(keys.is_empty());
        // Legacy admin-state converts to the marker form.
        assert!(ConfigTree::has_leaf(children, "shutdown"));
        assert_eq!(ConfigTree::leaf_value(children, "admin-state"), None);
    }

    #[test]
    fn normalizes_hyphenated_keywords_to_phrases() {
        let mut tree = crate::parse(
            "interfaces {\n Ethernet1 {\n no-shutdown\n switchport {\n mode access\naccess-vlan 10\n }\n }\n \
             Ethernet2 {\n admin-state enabled\n switchport {\n trunk-vlans 10 20\nnative-vlan 5\n }\n }\n }",
        )
        .unwrap();
        tree.normalize_interfaces();
        let (_, interfaces) = tree.block("interfaces").unwrap();
        let (_, e1) = ConfigTree::blocks_named(interfaces, "Ethernet1")
            .next()
            .unwrap();
        assert!(ConfigTree::has_phrase(e1, "no", "shutdown"));
        let (_, sp1) = ConfigTree::blocks_named(e1, "switchport").next().unwrap();
        assert_eq!(
            ConfigTree::phrase_values(sp1, "access", "vlan"),
            Some(&["10".to_string()][..])
        );
        let (_, e2) = ConfigTree::blocks_named(interfaces, "Ethernet2")
            .next()
            .unwrap();
        assert!(ConfigTree::has_phrase(e2, "no", "shutdown"));
        let (_, sp2) = ConfigTree::blocks_named(e2, "switchport").next().unwrap();
        assert_eq!(
            ConfigTree::phrase_values(sp2, "trunk", "vlans"),
            Some(&["10".to_string(), "20".to_string()][..])
        );
        assert_eq!(
            ConfigTree::phrase_values(sp2, "native", "vlan"),
            Some(&["5".to_string()][..])
        );
        // The rendered text carries the new phrases and round-trips.
        let text = tree.to_text();
        assert!(text.contains("no shutdown"));
        assert!(text.contains("access vlan 10"));
        assert!(text.contains("trunk vlans 10 20"));
        assert!(text.contains("native vlan 5"));
        assert_eq!(crate::parse(&text).unwrap(), tree);
    }

    #[test]
    fn queries_work() {
        let tree = sample();
        let (_, system) = tree.block("system").unwrap();
        assert_eq!(ConfigTree::leaf_value(system, "hostname"), Some("sw1"));

        let (_, interfaces) = tree.block("interfaces").unwrap();
        let (keys, children) = ConfigTree::blocks_named(interfaces, "ethernet")
            .next()
            .unwrap();
        assert_eq!(keys, ["Ethernet0"]);
        assert_eq!(
            ConfigTree::leaf_value(children, "description"),
            Some("uplink to core")
        );
    }
}
