//! Config edits for the system-suite pages: identity (hostname, time
//! zone, resolvers, domain, login banner).
//!
//! Same discipline as `edit.rs` and `services_edit.rs`: the builders
//! write exactly the leaves hemlockctl writes, based on the running
//! config, and the result goes through mgmtd's normal SetCandidate +
//! Commit path — so validation, the rollback ring and
//! `show configuration` behave as if the change came from the CLI.

use hemlock_config::{ConfigTree, Item};
use serde::Deserialize;

/// The most resolvers the config accepts — mirrored from the CLI and
/// re-checked by mgmtd.
const MAX_NAME_SERVERS: usize = 3;

fn remove_block_if_empty(tree: &mut ConfigTree, name: &str) {
    if tree
        .block(name)
        .is_some_and(|(_, children)| children.is_empty())
    {
        ConfigTree::remove_block(&mut tree.items, name, &[]);
    }
}

/// One RFC-1123 label.
fn valid_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_domain_name(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty() && name.len() <= 253 && name.split('.').all(valid_hostname_label)
}

/// One identity edit. Absent fields stay untouched; an empty string or
/// an empty list clears the leaf, which is what the page sends when a
/// field is blanked.
#[derive(Debug, Default, Deserialize)]
pub struct IdentityEdit {
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub name_servers: Option<Vec<String>>,
    #[serde(default)]
    pub domain_name: Option<String>,
    #[serde(default)]
    pub banner_login: Option<String>,
}

pub fn apply_identity_edit(tree: &mut ConfigTree, edit: &IdentityEdit) -> Result<(), String> {
    // Validate everything before touching the tree, so a rejected edit
    // leaves the candidate exactly as it was.
    if let Some(hostname) = &edit.hostname {
        if !hostname.is_empty() && !valid_hostname_label(hostname) {
            return Err(format!(
                "bad hostname {hostname:?} (letters, digits and hyphens, max 63)"
            ));
        }
    }
    if let Some(domain) = &edit.domain_name {
        if !domain.is_empty() && !valid_domain_name(domain) {
            return Err(format!("bad domain-name {domain:?}"));
        }
    }
    if let Some(tz) = &edit.timezone {
        if !tz.is_empty() && !hemlock_common::tz::exists(tz) {
            return Err(format!("unknown timezone {tz:?}"));
        }
    }
    let name_servers = match &edit.name_servers {
        Some(servers) => {
            let mut canonical: Vec<String> = Vec::new();
            for server in servers.iter().filter(|s| !s.trim().is_empty()) {
                let address: std::net::IpAddr = server
                    .trim()
                    .parse()
                    .map_err(|_| format!("bad name-server {server:?}"))?;
                let address = address.to_string();
                if !canonical.contains(&address) {
                    canonical.push(address);
                }
            }
            if canonical.len() > MAX_NAME_SERVERS {
                return Err(format!(
                    "at most {MAX_NAME_SERVERS} name-servers ({} given)",
                    canonical.len()
                ));
            }
            Some(canonical)
        }
        None => None,
    };

    let system = tree.block_mut("system");
    for (field, leaf) in [
        (&edit.hostname, "hostname"),
        (&edit.timezone, "timezone"),
        (&edit.domain_name, "domain-name"),
    ] {
        match field.as_deref().map(str::trim) {
            None => {}
            Some("") => ConfigTree::remove_leaf(system, leaf),
            Some(value) => ConfigTree::set_leaf(system, leaf, vec![value.to_string()]),
        }
    }
    if let Some(servers) = name_servers {
        ConfigTree::remove_leaf(system, "name-server");
        for address in servers {
            system.push(Item::Leaf {
                name: "name-server".into(),
                values: vec![address],
            });
        }
    }
    match edit.banner_login.as_deref() {
        None => {}
        Some("") => ConfigTree::remove_leaf(system, "banner"),
        Some(text) => ConfigTree::set_phrase(system, "banner", "login", vec![text.to_string()]),
    }
    remove_block_if_empty(tree, "system");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tree_of(text: &str) -> ConfigTree {
        hemlock_config::parse(text).unwrap()
    }

    #[test]
    fn writes_the_identity_leaves_the_cli_writes() {
        let mut tree = ConfigTree::default();
        apply_identity_edit(
            &mut tree,
            &IdentityEdit {
                hostname: Some("hemlock-a1".into()),
                timezone: Some("America/Detroit".into()),
                name_servers: Some(vec!["10.42.0.5".into(), "10.42.0.6".into()]),
                domain_name: Some("nightshade.systems".into()),
                banner_login: Some("Authorized access only.".into()),
            },
        )
        .unwrap();
        assert_eq!(
            tree.to_text(),
            "system {\n    \
             hostname hemlock-a1\n    \
             timezone America/Detroit\n    \
             domain-name nightshade.systems\n    \
             name-server 10.42.0.5\n    \
             name-server 10.42.0.6\n    \
             banner login \"Authorized access only.\"\n}\n"
        );
    }

    /// Blanking a field clears its leaf; an emptied `system` block goes
    /// away rather than persisting as a husk.
    #[test]
    fn blank_fields_clear_and_prune() {
        let mut tree = tree_of(
            "system {\n    hostname sw1\n    name-server 10.0.0.1\n    banner login \"hi\"\n}\n",
        );
        apply_identity_edit(
            &mut tree,
            &IdentityEdit {
                hostname: Some(String::new()),
                name_servers: Some(vec![]),
                banner_login: Some(String::new()),
                ..IdentityEdit::default()
            },
        )
        .unwrap();
        assert_eq!(tree.to_text(), "");

        // An absent field is left alone.
        let mut tree = tree_of("system {\n    hostname sw1\n}\n");
        apply_identity_edit(&mut tree, &IdentityEdit::default()).unwrap();
        assert_eq!(tree.to_text(), "system {\n    hostname sw1\n}\n");
    }

    #[test]
    fn rejects_bad_values_without_touching_the_tree() {
        let before = "system {\n    hostname sw1\n}\n";
        for edit in [
            IdentityEdit {
                hostname: Some("bad hostname".into()),
                ..IdentityEdit::default()
            },
            IdentityEdit {
                domain_name: Some("bad..domain".into()),
                ..IdentityEdit::default()
            },
            IdentityEdit {
                name_servers: Some(vec!["not-an-ip".into()]),
                ..IdentityEdit::default()
            },
            IdentityEdit {
                name_servers: Some(vec![
                    "10.0.0.1".into(),
                    "10.0.0.2".into(),
                    "10.0.0.3".into(),
                    "10.0.0.4".into(),
                ]),
                ..IdentityEdit::default()
            },
        ] {
            let mut tree = tree_of(before);
            assert!(apply_identity_edit(&mut tree, &edit).is_err());
            assert_eq!(tree.to_text(), before, "a rejected edit changed the tree");
        }
    }

    /// Resolvers canonicalize and deduplicate, exactly as the CLI does.
    #[test]
    fn resolvers_canonicalize() {
        let mut tree = ConfigTree::default();
        apply_identity_edit(
            &mut tree,
            &IdentityEdit {
                name_servers: Some(vec![
                    " 2001:0db8:0000::1 ".into(),
                    "10.0.0.1".into(),
                    "10.0.0.1".into(),
                    String::new(),
                ]),
                ..IdentityEdit::default()
            },
        )
        .unwrap();
        assert_eq!(
            tree.to_text(),
            "system {\n    name-server 2001:db8::1\n    name-server 10.0.0.1\n}\n"
        );
    }
}
