//! Operator account listing and creation.
//!
//! Accounts are OS users, exactly as mkimage creates the default
//! `admin`: member of the `hemlock` group (daemon socket + web/CLI
//! access), optionally `sudo`, login shell hemlockctl. Listing parses
//! /etc/passwd + /etc/group + /etc/shadow; creation shells out to
//! `useradd`/`chpasswd` (webd runs as root on the switch). On non-unix
//! development hosts the list shows the --dev-auth account and
//! creation is refused.

use serde::{Deserialize, Serialize};

pub const OPERATOR_SHELL: &str = "/usr/bin/hemlockctl";

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UserJson {
    pub username: String,
    pub uid: Option<u32>,
    /// Member of `sudo` (full administrator).
    pub sudo: bool,
    /// Password field in /etc/shadow is locked or unreadable.
    pub locked: bool,
    pub shell: String,
}

#[derive(Debug, Deserialize)]
pub struct AddUserRequest {
    pub username: String,
    pub password: String,
    /// Grant full sudo like the stock `admin` account.
    #[serde(default)]
    pub sudo: bool,
}

/// Usernames follow useradd's conservative rules; also keeps shell
/// metacharacters out of the useradd argv by construction.
pub fn valid_username(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 32
        && (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

/// Members of `group` in a /etc/group file: the member list plus every
/// passwd account whose primary gid matches.
fn group_members(group_text: &str, passwd_text: &str, group: &str) -> Vec<String> {
    let mut members: Vec<String> = Vec::new();
    let mut gid: Option<String> = None;
    for line in group_text.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(group) {
            continue;
        }
        let _password = fields.next();
        gid = fields.next().map(|g| g.trim().to_string());
        if let Some(list) = fields.next() {
            members.extend(
                list.split(',')
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string),
            );
        }
        break;
    }
    if let Some(gid) = gid {
        for line in passwd_text.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() > 3
                && fields[3].trim() == gid
                && !members.iter().any(|m| m == fields[0])
            {
                members.push(fields[0].to_string());
            }
        }
    }
    members
}

/// Build the operator list from the three account databases' text.
pub fn operators(group_text: &str, passwd_text: &str, shadow_text: &str) -> Vec<UserJson> {
    let mut users: Vec<UserJson> = group_members(group_text, passwd_text, "hemlock")
        .into_iter()
        .map(|username| {
            let (uid, shell) = passwd_text
                .lines()
                .find_map(|line| {
                    let fields: Vec<&str> = line.split(':').collect();
                    (fields.first() == Some(&username.as_str())).then(|| {
                        (
                            fields.get(2).and_then(|u| u.trim().parse().ok()),
                            fields.get(6).unwrap_or(&"").trim().to_string(),
                        )
                    })
                })
                .unwrap_or((None, String::new()));
            let hash = shadow_text.lines().find_map(|line| {
                let mut fields = line.split(':');
                (fields.next() == Some(username.as_str()))
                    .then(|| fields.next().unwrap_or("").to_string())
            });
            let locked = match hash {
                Some(hash) => hash.is_empty() || hash.starts_with('!') || hash.starts_with('*'),
                None => true,
            };
            let sudo = group_members(group_text, passwd_text, "sudo").contains(&username);
            UserJson {
                username,
                uid,
                sudo,
                locked,
                shell,
            }
        })
        .collect();
    users.sort_by(|a, b| a.username.cmp(&b.username));
    users
}

/// The switch's operator accounts.
pub fn list(dev_auth: Option<&(String, String)>) -> Vec<UserJson> {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let group = read("/etc/group");
    if group.lines().any(|l| l.starts_with("hemlock:")) {
        // Shadow needs root; on failure everyone shows as locked rather
        // than the list failing outright.
        return operators(&group, &read("/etc/passwd"), &read("/etc/shadow"));
    }
    // Development host: no hemlock group; surface the dev credential so
    // the page has something real to render.
    dev_auth
        .map(|(username, _)| UserJson {
            username: username.clone(),
            uid: None,
            sudo: true,
            locked: false,
            shell: "(--dev-auth)".to_string(),
        })
        .into_iter()
        .collect()
}

/// Create an operator account (`useradd` + `chpasswd`).
pub async fn add(request: &AddUserRequest) -> Result<(), String> {
    if !valid_username(&request.username) {
        return Err(format!(
            "bad username {:?} (lowercase letters, digits, - and _; max 32)",
            request.username
        ));
    }
    if request.password.len() < 8 {
        return Err("password must be at least 8 characters".to_string());
    }
    if request.password.contains(['\n', ':']) {
        return Err("password must not contain ':' or newlines".to_string());
    }
    if !cfg!(unix) {
        return Err("user management is only available on the switch".to_string());
    }

    let groups = if request.sudo {
        "sudo,hemlock"
    } else {
        "hemlock"
    };
    let output = tokio::process::Command::new("useradd")
        .args(["-m", "-s", OPERATOR_SHELL, "-G", groups, &request.username])
        .output()
        .await
        .map_err(|e| format!("cannot run useradd: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "useradd failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut child = tokio::process::Command::new("chpasswd")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run chpasswd: {e}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        use tokio::io::AsyncWriteExt as _;
        let line = format!("{}:{}\n", request.username, request.password);
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("cannot write to chpasswd: {e}"))?;
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("chpasswd failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "chpasswd failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    tracing::info!(username = %request.username, sudo = request.sudo, "operator account created");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const GROUP: &str = "sudo:x:27:admin\nhemlock:x:990:admin,noc\n";
    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
        admin:x:1000:1000::/home/admin:/usr/bin/hemlockctl\n\
        noc:x:1001:1001::/home/noc:/usr/bin/hemlockctl\n\
        svc:x:1002:990::/home/svc:/usr/sbin/nologin\n";
    const SHADOW: &str = "admin:$y$j9T$abc$def:19000:0:99999:7:::\n\
        noc:!:19000:0:99999:7:::\n";

    #[test]
    fn lists_operators_with_roles_and_lock_state() {
        let users = operators(GROUP, PASSWD, SHADOW);
        let names: Vec<&str> = users.iter().map(|u| u.username.as_str()).collect();
        // svc's primary group is hemlock (gid 990) — counted too.
        assert_eq!(names, ["admin", "noc", "svc"]);
        let admin = &users[0];
        assert!(admin.sudo && !admin.locked);
        assert_eq!(admin.uid, Some(1000));
        assert_eq!(admin.shell, "/usr/bin/hemlockctl");
        // noc's hash is locked; svc has no shadow entry at all.
        assert!(users[1].locked && !users[1].sudo);
        assert!(users[2].locked);
    }

    #[test]
    fn validates_usernames() {
        assert!(valid_username("admin"));
        assert!(valid_username("net-ops_2"));
        assert!(!valid_username(""));
        assert!(!valid_username("Admin"));
        assert!(!valid_username("2fast"));
        assert!(!valid_username("a b"));
        assert!(!valid_username("evil;rm"));
    }

    #[tokio::test]
    async fn add_rejects_bad_requests() {
        let bad_name = AddUserRequest {
            username: "Not Valid".into(),
            password: "longenough".into(),
            sudo: false,
        };
        assert!(add(&bad_name).await.unwrap_err().contains("bad username"));
        let short = AddUserRequest {
            username: "ok".into(),
            password: "short".into(),
            sudo: false,
        };
        assert!(add(&short).await.unwrap_err().contains("at least 8"));
    }
}
