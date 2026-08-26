//! Data sources for the system-suite show family: mgmtd's running
//! config (configured users) and its session registry (who is on the
//! box), plus the OS account database for the accounts neither owns.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;
use hemlock_config::ConfigTree;

use super::model::{ActiveSession, ConfiguredUser, LogEntry, LoggingState, UsersState};

async fn mgmt_client(
    mgmtd: &IpcEndpoint,
) -> Result<pb::mgmt_client::MgmtClient<tonic::transport::Channel>> {
    let channel = mgmtd.connect().await.context("connecting to mgmtd")?;
    Ok(pb::mgmt_client::MgmtClient::new(channel))
}

/// `show system users`.
pub async fn users_state(mgmtd: &IpcEndpoint) -> Result<UsersState> {
    let mut client = mgmt_client(mgmtd).await?;
    let text = client
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Running as i32,
        })
        .await?
        .into_inner()
        .text;
    let tree = hemlock_config::parse(&text)
        .map_err(|e| anyhow::anyhow!("running config unparsable: {e}"))?;
    let configured = configured_users(&tree);

    let sessions = client
        .list_sessions(pb::ListSessionsRequest {})
        .await?
        .into_inner()
        .sessions
        .into_iter()
        .map(|session| ActiveSession {
            user: session.user,
            from: session.from,
            client: session.client,
            role: session.role,
            idle_secs: session.idle_secs,
            login_time: unix_of(&session.login_time),
        })
        .collect();

    let managed: Vec<&str> = configured.iter().map(|u| u.name.as_str()).collect();
    let unmanaged = os_login_users()
        .into_iter()
        .filter(|name| !managed.contains(&name.as_str()))
        .collect();

    Ok(UsersState {
        configured,
        sessions,
        unmanaged,
    })
}

/// The `system { login { user <name> { ... } } }` accounts, in name
/// order (the config tree is already sorted by the serializer, but the
/// display must not depend on that).
fn configured_users(tree: &ConfigTree) -> Vec<ConfiguredUser> {
    let Some((_, system)) = tree.block("system") else {
        return Vec::new();
    };
    let Some((_, login)) = ConfigTree::blocks_named(system, "login").next() else {
        return Vec::new();
    };
    let mut users: Vec<ConfiguredUser> = ConfigTree::blocks_named(login, "user")
        .filter_map(|(keys, children)| {
            let name = keys.first()?.clone();
            let ssh_keys = children
                .iter()
                .filter(|item| item.name() == "ssh-key")
                .count();
            let has_password = ConfigTree::leaf_value(children, "password-hash").is_some();
            let auth = match (has_password, ssh_keys) {
                (true, _) => "password",
                (false, 0) => "none",
                (false, _) => "ssh-key",
            };
            Some(ConfiguredUser {
                // Least privilege: an omitted role is `operator`.
                role: ConfigTree::leaf_value(children, "role")
                    .unwrap_or("operator")
                    .to_string(),
                name,
                auth: auth.to_string(),
                ssh_keys,
            })
        })
        .collect();
    users.sort_by(|a, b| a.name.cmp(&b.name));
    users
}

/// An RFC-3339 stamp as unix seconds; 0 when it will not parse.
fn unix_of(rfc3339: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|time| u64::try_from(time.timestamp()).unwrap_or(0))
        .unwrap_or(0)
}

/// Human login accounts from the OS: `/etc/passwd` entries in the
/// regular-user uid range with a real shell. Empty off-switch (no
/// /etc/passwd), which is also what a development host wants.
pub fn os_login_users() -> Vec<String> {
    match std::fs::read_to_string("/etc/passwd") {
        Ok(passwd) => os_login_users_in(&passwd),
        Err(_) => Vec::new(),
    }
}

/// [`os_login_users`] over a `/etc/passwd` text.
pub fn os_login_users_in(passwd: &str) -> Vec<String> {
    let mut users: Vec<String> = passwd
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let [name, _, uid, _, _, _, shell] = fields.as_slice() else {
                return None;
            };
            let uid: u32 = uid.trim().parse().ok()?;
            let real_shell = !shell.ends_with("nologin") && !shell.ends_with("false");
            ((1000..60000).contains(&uid) && real_shell).then(|| (*name).to_string())
        })
        .collect();
    users.sort();
    users
}

// ------------------------------------------------- logging

/// The forwarding defaults, mirrored from mgmtd so the display of an
/// unset leaf matches what the applier would do.
const DEFAULT_LOG_LEVEL: &str = "informational";
const DEFAULT_LOG_PORT: &str = "514";
const DEFAULT_LOG_PROTOCOL: &str = "udp";

/// `show logging [<count>]`: the configured forwarding beside the tail
/// of the local journal, both from mgmtd (which is root, so it can read
/// a journal an operator account may not).
pub async fn logging_state(mgmtd: &IpcEndpoint, count: u32) -> Result<LoggingState> {
    let mut client = mgmt_client(mgmtd).await?;
    let text = client
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Running as i32,
        })
        .await?
        .into_inner()
        .text;
    let tree = hemlock_config::parse(&text)
        .map_err(|e| anyhow::anyhow!("running config unparsable: {e}"))?;
    let (level, hosts) = logging_config(&tree);

    let log = client
        .get_log(pb::GetLogRequest { count })
        .await?
        .into_inner();

    Ok(LoggingState {
        level,
        hosts,
        entries: log
            .entries
            .into_iter()
            .map(|entry| LogEntry {
                time: entry.time_unix,
                host: entry.host,
                tag: entry.tag,
                pid: entry.pid,
                message: entry.message,
                severity: entry.severity,
            })
            .collect(),
        requested: count,
        journal_available: log.available,
    })
}

/// `system { logging { ... } }` as the level and the display forms of
/// its collectors.
fn logging_config(tree: &ConfigTree) -> (String, Vec<String>) {
    let Some((_, system)) = tree.block("system") else {
        return (DEFAULT_LOG_LEVEL.to_string(), Vec::new());
    };
    let Some((_, logging)) = ConfigTree::blocks_named(system, "logging").next() else {
        return (DEFAULT_LOG_LEVEL.to_string(), Vec::new());
    };
    let level = ConfigTree::leaf_value(logging, "level")
        .unwrap_or(DEFAULT_LOG_LEVEL)
        .to_string();
    let hosts = logging
        .iter()
        .filter_map(|item| match item {
            hemlock_config::Item::Leaf { name, values } if name == "host" => {
                let address = values.first()?;
                let setting = |wanted: &str, fallback: &str| {
                    values[1..]
                        .chunks(2)
                        .find(|pair| pair.first().map(String::as_str) == Some(wanted))
                        .and_then(|pair| pair.get(1).map(String::as_str))
                        .unwrap_or(fallback)
                        .to_string()
                };
                let port = setting("port", DEFAULT_LOG_PORT);
                let protocol = setting("protocol", DEFAULT_LOG_PROTOCOL);
                // A v6 literal needs brackets to keep the port readable.
                Some(if address.contains(':') {
                    format!("[{address}]:{port} ({protocol})")
                } else {
                    format!("{address}:{port} ({protocol})")
                })
            }
            _ => None,
        })
        .collect();
    (level, hosts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_configured_users_from_the_config() {
        let tree = hemlock_config::parse(
            "system { login {\n\
             user cody { role admin\npassword-hash \"$6$a$b\"\n\
             ssh-key \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3 cody@mars\" }\n\
             user noc { password-hash \"$6$c$d\" }\n\
             user keys { ssh-key \"ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQ jo@luna\" }\n\
             } }",
        )
        .unwrap_or_default();
        let users = configured_users(&tree);
        let names: Vec<&str> = users.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["cody", "keys", "noc"]);
        assert_eq!(users[0].role, "admin");
        assert_eq!(users[0].auth, "password");
        assert_eq!(users[0].ssh_keys, 1);
        // An omitted role is the least-privilege default.
        assert_eq!(users[2].role, "operator");
        assert_eq!(users[1].auth, "ssh-key");
        // No login block at all: nothing is config-managed.
        assert!(configured_users(&ConfigTree::default()).is_empty());
    }

    #[test]
    fn reads_os_accounts_for_the_unmanaged_note() {
        const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
            admin:x:1000:1000::/home/admin:/usr/bin/hemlockctl\n\
            noc:x:1001:1001::/home/noc:/usr/bin/hemlockctl\n\
            svc:x:1002:990::/home/svc:/usr/sbin/nologin\n\
            nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";
        // root is out of range, svc and nobody have no real shell.
        assert_eq!(os_login_users_in(PASSWD), ["admin", "noc"]);
        assert!(os_login_users_in("").is_empty());
    }

    #[test]
    fn reads_logging_config_with_its_defaults() {
        let tree = hemlock_config::parse(
            "system { logging {
             host 10.42.0.30
             host 10.42.0.31 port 6514 protocol tcp
             host 2001:db8::30 protocol tcp
             level informational
             } }",
        )
        .unwrap_or_default();
        let (level, hosts) = logging_config(&tree);
        assert_eq!(level, "informational");
        assert_eq!(
            hosts,
            [
                "10.42.0.30:514 (udp)",
                "10.42.0.31:6514 (tcp)",
                "[2001:db8::30]:514 (tcp)",
            ]
        );
        // No block at all: the default level, nothing forwarded.
        let (level, hosts) = logging_config(&ConfigTree::default());
        assert_eq!(level, "informational");
        assert!(hosts.is_empty());
    }

    #[test]
    fn parses_session_timestamps() {
        assert_eq!(unix_of("2026-08-25T09:12:44Z"), 1_787_649_164);
        assert_eq!(unix_of("nonsense"), 0);
    }
}
