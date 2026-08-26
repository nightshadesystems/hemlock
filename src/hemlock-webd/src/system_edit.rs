//! Config edits for the system-suite pages: identity (hostname, time
//! zone, resolvers, domain, login banner).
//!
//! Same discipline as `edit.rs` and `services_edit.rs`: the builders
//! write exactly the leaves hemlockctl writes, based on the running
//! config, and the result goes through mgmtd's normal SetCandidate +
//! Commit path — so validation, the rollback ring and
//! `show configuration` behave as if the change came from the CLI.

use hemlock_common::role::Role;
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

// ---------------------------------------------------------------- users

/// The most keys one account carries, and the account-name rule —
/// mirrored from the CLI; mgmtd re-validates every one of them.
const MAX_SSH_KEYS: usize = 8;

const SSH_KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
    "rsa-sha2-256",
    "rsa-sha2-512",
];

fn valid_user_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 32
        && (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

fn valid_ssh_key(key: &str) -> bool {
    let mut fields = key.split_whitespace();
    let Some(kind) = fields.next() else {
        return false;
    };
    if !SSH_KEY_TYPES.contains(&kind) {
        return false;
    }
    let Some(body) = fields.next() else {
        return false;
    };
    body.len() >= 16
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
}

/// Every configured account and its role — what the console needs to
/// refresh live session roles after a commit.
pub fn configured_roles(tree: &ConfigTree) -> Vec<(String, Role)> {
    let Some((_, system)) = tree.block("system") else {
        return Vec::new();
    };
    let Some((_, login)) = ConfigTree::blocks_named(system, "login").next() else {
        return Vec::new();
    };
    ConfigTree::blocks_named(login, "user")
        .filter_map(|(keys, children)| {
            let name = keys.first()?.clone();
            let role = ConfigTree::leaf_value(children, "role")
                .and_then(Role::parse)
                .unwrap_or_default();
            Some((name, role))
        })
        .collect()
}

/// One user create-or-update, or a removal.
///
/// The password arrives as plaintext exactly once and is hashed here,
/// so the candidate never holds it — the same contract the CLI
/// `set ... password` spelling has.
#[derive(Debug, Default, Deserialize)]
pub struct UserEdit {
    pub name: String,
    /// True removes the account instead of writing it.
    #[serde(default)]
    pub remove: bool,
    /// "admin" | "operator"; absent leaves the role alone.
    #[serde(default)]
    pub role: Option<String>,
    /// Write-only: a new password. Absent leaves the stored hash alone;
    /// an empty string clears it (a key-only account).
    #[serde(default)]
    pub password: Option<String>,
    /// When present, replaces the whole key list.
    #[serde(default)]
    pub ssh_keys: Option<Vec<String>>,
}

pub fn apply_user_edit(tree: &mut ConfigTree, edit: &UserEdit) -> Result<(), String> {
    if !valid_user_name(&edit.name) {
        return Err(format!(
            "bad user name {:?} (a-z, 0-9, _ and -, starting with a letter or _, max 32)",
            edit.name
        ));
    }
    // Validate before touching the tree, so a rejected edit leaves the
    // candidate exactly as it was.
    let role = match edit.role.as_deref() {
        None => None,
        Some(role) => {
            Some(Role::parse(role).ok_or_else(|| format!("bad role {role:?}"))?)
        }
    };
    let hash = match edit.password.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(plaintext) => {
            hemlock_common::passwd::check_plaintext(plaintext)?;
            Some(Some(hemlock_common::passwd::hash(plaintext)?))
        }
    };
    if let Some(keys) = &edit.ssh_keys {
        let live: Vec<&String> = keys.iter().filter(|k| !k.trim().is_empty()).collect();
        if live.len() > MAX_SSH_KEYS {
            return Err(format!(
                "at most {MAX_SSH_KEYS} ssh-keys ({} given)",
                live.len()
            ));
        }
        for key in &live {
            if !valid_ssh_key(key.trim()) {
                return Err(
                    "ssh-key must be `<type> <base64> [comment]` with a known key type".into(),
                );
            }
        }
    }

    // The lockout guard, mirrored from mgmtd: a config that manages
    // users must keep one administrator who can log in. Checked here so
    // the console can say what is wrong before the commit does.
    {
        let before = configured_users_with_password(tree);
        let mut after = before.clone();
        after.retain(|(name, _, _)| name != &edit.name);
        if !edit.remove {
            let existing = before.iter().find(|(name, _, _)| name == &edit.name);
            let final_role = role
                .or_else(|| existing.map(|(_, role, _)| *role))
                .unwrap_or_default();
            let has_password = match &hash {
                Some(hash) => hash.is_some(),
                None => existing.map(|(_, _, has)| *has).unwrap_or(false),
            };
            after.push((edit.name.clone(), final_role, has_password));
        }
        // Once the config manages users it has to keep an administrator
        // who can log in — including through the last removal, which is
        // the web equivalent of the CLI refusing `delete system login`.
        // The only edit exempt is a no-op on a config that manages
        // nobody.
        let usable_admin = after
            .iter()
            .any(|(_, role, has_password)| role.is_admin() && *has_password);
        if !(usable_admin || before.is_empty() && after.is_empty()) {
            return Err("system login: at least one admin user with a password is required".into());
        }
    }

    let system = tree.block_mut("system");
    if edit.remove {
        if let Some(login) = block_children_mut(system, "login") {
            ConfigTree::remove_block(login, "user", &[&edit.name]);
            if login.is_empty() {
                ConfigTree::remove_block(system, "login", &[]);
            }
        }
        remove_block_if_empty(tree, "system");
        return Ok(());
    }

    let login = ConfigTree::ensure_block(system, "login", &[]);
    let user = ConfigTree::ensure_block(login, "user", &[&edit.name]);
    if let Some(role) = role {
        ConfigTree::set_leaf(user, "role", vec![role.as_str().to_string()]);
    }
    match hash {
        None => {}
        Some(None) => ConfigTree::remove_leaf(user, "password-hash"),
        Some(Some(hash)) => ConfigTree::set_leaf(user, "password-hash", vec![hash]),
    }
    if let Some(keys) = &edit.ssh_keys {
        ConfigTree::remove_leaf(user, "ssh-key");
        let mut written: Vec<String> = Vec::new();
        for key in keys.iter().map(|k| k.trim()).filter(|k| !k.is_empty()) {
            if written.iter().any(|existing| existing == key) {
                continue;
            }
            written.push(key.to_string());
            user.push(Item::Leaf {
                name: "ssh-key".into(),
                values: vec![key.to_string()],
            });
        }
    }
    Ok(())
}

/// (name, role, has a password) for every configured account.
fn configured_users_with_password(tree: &ConfigTree) -> Vec<(String, Role, bool)> {
    let Some((_, system)) = tree.block("system") else {
        return Vec::new();
    };
    let Some((_, login)) = ConfigTree::blocks_named(system, "login").next() else {
        return Vec::new();
    };
    ConfigTree::blocks_named(login, "user")
        .filter_map(|(keys, children)| {
            let name = keys.first()?.clone();
            Some((
                name,
                ConfigTree::leaf_value(children, "role")
                    .and_then(Role::parse)
                    .unwrap_or_default(),
                ConfigTree::leaf_value(children, "password-hash").is_some(),
            ))
        })
        .collect()
}

/// `system { web { session-timeout <minutes> } }`.
#[derive(Debug, Default, Deserialize)]
pub struct WebEdit {
    /// 0 clears the leaf (back to the default).
    pub session_timeout: u32,
}

pub fn apply_web_edit(tree: &mut ConfigTree, edit: &WebEdit) -> Result<(), String> {
    if edit.session_timeout != 0 && !(5..=1440).contains(&edit.session_timeout) {
        return Err(format!(
            "bad session-timeout {} (5..1440 minutes)",
            edit.session_timeout
        ));
    }
    let system = tree.block_mut("system");
    if edit.session_timeout == 0 {
        ConfigTree::remove_block(system, "web", &[]);
    } else {
        let web = ConfigTree::ensure_block(system, "web", &[]);
        ConfigTree::set_leaf(
            web,
            "session-timeout",
            vec![edit.session_timeout.to_string()],
        );
    }
    remove_block_if_empty(tree, "system");
    Ok(())
}

fn block_children_mut<'a>(items: &'a mut [Item], name: &str) -> Option<&'a mut Vec<Item>> {
    items.iter_mut().find_map(|item| match item {
        Item::Block {
            name: n, children, ..
        } if n == name => Some(children),
        _ => None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tree_of(text: &str) -> ConfigTree {
        hemlock_config::parse(text).unwrap()
    }

    /// One admin with a password, so the lockout guard is satisfied.
    const SEED: &str = "system {\n    login {\n        user cody {\n            \
        role admin\n            password-hash \"$6$abcdefgh$ijklmnop\"\n        }\n    }\n}\n";

    #[test]
    fn creates_and_updates_users() {
        let mut tree = tree_of(SEED);
        apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "noc".into(),
                role: Some("operator".into()),
                password: Some("hunter2hunter2".into()),
                ssh_keys: Some(vec![
                    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3 noc@mars".into(),
                    // Blank and duplicate entries are dropped.
                    "  ".into(),
                    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3 noc@mars".into(),
                ]),
                ..UserEdit::default()
            },
        )
        .unwrap();
        let text = tree.to_text();
        assert!(text.contains("user noc {"), "{text}");
        assert!(text.contains("role operator"), "{text}");
        // The plaintext is never stored; the hash verifies.
        assert!(!text.contains("hunter2hunter2"), "{text}");
        let (_, system) = tree.block("system").unwrap();
        let (_, login) = ConfigTree::blocks_named(system, "login").next().unwrap();
        let (_, noc) = ConfigTree::blocks_named(login, "user")
            .find(|(keys, _)| keys[0] == "noc")
            .unwrap();
        let hash = ConfigTree::leaf_value(noc, "password-hash").unwrap();
        assert!(hemlock_common::passwd::verify("hunter2hunter2", hash));
        assert_eq!(
            noc.iter().filter(|item| item.name() == "ssh-key").count(),
            1
        );
    }

    /// The lockout guard is mirrored client-side: the console refuses
    /// the edit before the commit has to.
    #[test]
    fn refuses_to_strand_the_last_admin() {
        const MESSAGE: &str = "system login: at least one admin user with a password is required";

        // Removing the only administrator.
        let mut tree = tree_of(SEED);
        assert_eq!(
            apply_user_edit(
                &mut tree,
                &UserEdit {
                    name: "cody".into(),
                    remove: true,
                    ..UserEdit::default()
                }
            )
            .unwrap_err(),
            MESSAGE
        );
        assert_eq!(tree.to_text(), SEED, "a rejected edit changed the tree");

        // Demoting them.
        assert_eq!(
            apply_user_edit(
                &mut tree,
                &UserEdit {
                    name: "cody".into(),
                    role: Some("operator".into()),
                    ..UserEdit::default()
                }
            )
            .unwrap_err(),
            MESSAGE
        );

        // Clearing their password (leaving a key-only admin).
        assert_eq!(
            apply_user_edit(
                &mut tree,
                &UserEdit {
                    name: "cody".into(),
                    password: Some(String::new()),
                    ..UserEdit::default()
                }
            )
            .unwrap_err(),
            MESSAGE
        );

        // With a second administrator, all three are allowed.
        let mut tree = tree_of(SEED);
        apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "jo".into(),
                role: Some("admin".into()),
                password: Some("hunter2hunter2".into()),
                ..UserEdit::default()
            },
        )
        .unwrap();
        apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "cody".into(),
                remove: true,
                ..UserEdit::default()
            },
        )
        .unwrap();
        assert!(!tree.to_text().contains("user cody"), "{}", tree.to_text());
    }

    #[test]
    fn rejects_bad_user_edits() {
        let mut tree = tree_of(SEED);
        for edit in [
            UserEdit {
                name: "Not Valid".into(),
                ..UserEdit::default()
            },
            UserEdit {
                name: "noc".into(),
                role: Some("superuser".into()),
                ..UserEdit::default()
            },
            UserEdit {
                name: "noc".into(),
                password: Some("short".into()),
                ..UserEdit::default()
            },
            UserEdit {
                name: "noc".into(),
                ssh_keys: Some(vec!["ssh-dss AAAAB3NzaC1kc3MAAACBnope key".into()]),
                ..UserEdit::default()
            },
        ] {
            assert!(apply_user_edit(&mut tree, &edit).is_err());
            assert_eq!(tree.to_text(), SEED, "a rejected edit changed the tree");
        }
    }

    /// The first configured user has to be a usable administrator, and
    /// the last one can never be removed — the console equivalent of
    /// the CLI refusing `delete system login`.
    #[test]
    fn the_first_user_must_be_an_admin_and_the_last_cannot_go() {
        let mut tree = ConfigTree::default();
        assert!(apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "noc".into(),
                role: Some("operator".into()),
                password: Some("hunter2hunter2".into()),
                ..UserEdit::default()
            }
        )
        .is_err());
        apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "cody".into(),
                role: Some("admin".into()),
                password: Some("hunter2hunter2".into()),
                ..UserEdit::default()
            },
        )
        .unwrap();
        // Now an operator can join, and leave again.
        apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "noc".into(),
                role: Some("operator".into()),
                password: Some("hunter2hunter2".into()),
                ..UserEdit::default()
            },
        )
        .unwrap();
        apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "noc".into(),
                remove: true,
                ..UserEdit::default()
            },
        )
        .unwrap();
        assert!(!tree.to_text().contains("user noc"), "{}", tree.to_text());
        // The remaining administrator cannot.
        assert!(apply_user_edit(
            &mut tree,
            &UserEdit {
                name: "cody".into(),
                remove: true,
                ..UserEdit::default()
            }
        )
        .is_err());
        assert!(tree.to_text().contains("user cody"), "{}", tree.to_text());
    }

    #[test]
    fn reads_configured_roles() {
        let tree = tree_of(
            "system { login { user cody { role admin\npassword-hash \"$6$a$b\" }\n\
             user noc { password-hash \"$6$c$d\" } } }",
        );
        let mut roles = configured_roles(&tree);
        roles.sort();
        assert_eq!(
            roles,
            vec![
                ("cody".to_string(), Role::Admin),
                ("noc".to_string(), Role::Operator)
            ]
        );
        assert!(configured_roles(&ConfigTree::default()).is_empty());
    }

    #[test]
    fn writes_the_web_session_timeout() {
        let mut tree = ConfigTree::default();
        apply_web_edit(
            &mut tree,
            &WebEdit {
                session_timeout: 60,
            },
        )
        .unwrap();
        assert_eq!(
            tree.to_text(),
            "system {\n    web {\n        session-timeout 60\n    }\n}\n"
        );
        // 0 clears the block; an emptied system block goes too.
        apply_web_edit(&mut tree, &WebEdit { session_timeout: 0 }).unwrap();
        assert_eq!(tree.to_text(), "");
        assert!(apply_web_edit(
            &mut tree,
            &WebEdit {
                session_timeout: 4
            }
        )
        .is_err());
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
