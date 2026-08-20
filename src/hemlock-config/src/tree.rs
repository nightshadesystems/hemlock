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
            Item::Leaf { name: n, values } if n == name => {
                values.first().map(String::as_str)
            }
            _ => None,
        })
    }

    /// Render canonical text (4-space indent, quoted where needed).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            render(&mut out, item, 0);
        }
        out
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
            out.push_str(";\n");
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
            "system {\n    hostname sw1;\n}\ninterfaces {\n    ethernet Ethernet0 {\n        description \"uplink to core\";\n    }\n}\n"
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
