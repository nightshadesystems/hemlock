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

    /// Migrate the legacy `interfaces { ethernet <name> { ... } }` (and
    /// `management <name>`) form to the current name-as-block form
    /// (`interfaces { Ethernet1 { ... } }`), in place. Keeps configs
    /// persisted before the format change loading cleanly.
    pub fn normalize_interfaces(&mut self) {
        for item in &mut self.items {
            let Item::Block { name, children, .. } = item else {
                continue;
            };
            if name != "interfaces" {
                continue;
            }
            for child in children {
                if let Item::Block { name, keys, .. } = child {
                    if matches!(name.as_str(), "ethernet" | "management") && keys.len() == 1 {
                        *name = keys.remove(0);
                    }
                }
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
        assert_eq!(
            ConfigTree::leaf_value(children, "admin-state"),
            Some("disabled")
        );
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
