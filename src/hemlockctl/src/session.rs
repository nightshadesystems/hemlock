//! Who is running the CLI, and what they are allowed to do.
//!
//! At startup the CLI asks mgmtd (`WhoAmI`) for the caller's role and
//! registers the session, so `show system users` can list it; it slides
//! the idle timer before each command and closes on the way out.
//!
//! Enforcement uses the table in `hemlock_common::role`, shared with
//! the web console — one list, so a verb cannot be gated in one console
//! and forgotten in the other.
//!
//! When mgmtd cannot be reached the CLI assumes `admin`: every gated
//! verb needs mgmtd anyway, so refusing here would replace the real
//! "cannot reach mgmtd" message with a misleading permission error.

use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;
use hemlock_common::role::Role;

/// The caller's identity for the life of one CLI process.
#[derive(Debug, Clone)]
pub struct Identity {
    pub user: String,
    pub role: Role,
    /// mgmtd's session handle; 0 = nothing registered (an unreachable
    /// mgmtd, or the one-shot subcommand path).
    pub session_id: u64,
}

impl Identity {
    /// The fallback when mgmtd cannot say: see the module doc.
    fn assumed(user: String) -> Self {
        Self {
            user,
            role: Role::Admin,
            session_id: 0,
        }
    }

    /// Refuse `verb` when the caller is an operator.
    pub fn check(&self, verb: &str) -> Result<(), String> {
        if self.role.is_admin() || !hemlock_common::role::cli_requires_admin(verb) {
            Ok(())
        } else {
            Err(hemlock_common::role::PERMISSION_DENIED.to_string())
        }
    }
}

/// The account name this process runs as.
pub fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".into())
}

/// Where this login came from: the ssh peer address, or the console.
pub fn login_source() -> String {
    for var in ["SSH_CONNECTION", "SSH_CLIENT"] {
        if let Ok(value) = std::env::var(var) {
            if let Some(address) = value.split_whitespace().next() {
                if !address.is_empty() {
                    return address.to_string();
                }
            }
        }
    }
    "console".into()
}

/// Ask mgmtd who we are. `client` empty = do not register a session
/// (the one-shot `hemlockctl <subcommand>` path).
pub async fn who_am_i(mgmtd: &IpcEndpoint, client: &str) -> Identity {
    let user = current_user();
    let Ok(channel) = mgmtd.connect().await else {
        return Identity::assumed(user);
    };
    let mut client_rpc = pb::mgmt_client::MgmtClient::new(channel);
    let response = client_rpc
        .who_am_i(pb::WhoAmIRequest {
            user: user.clone(),
            client: client.to_string(),
            from: login_source(),
        })
        .await;
    match response {
        Ok(response) => {
            let response = response.into_inner();
            Identity {
                role: Role::parse(&response.role).unwrap_or(Role::Admin),
                session_id: response.session_id,
                user,
            }
        }
        Err(_) => Identity::assumed(user),
    }
}

/// Slide the session's idle timer. Best-effort: a failure here must
/// never keep a command from running.
pub async fn touch(mgmtd: &IpcEndpoint, session_id: u64) {
    if session_id == 0 {
        return;
    }
    if let Ok(channel) = mgmtd.connect().await {
        let mut client = pb::mgmt_client::MgmtClient::new(channel);
        let _ = client
            .touch_session(pb::TouchSessionRequest { session_id })
            .await;
    }
}

/// Drop the session on the way out.
pub async fn close(mgmtd: &IpcEndpoint, session_id: u64) {
    if session_id == 0 {
        return;
    }
    if let Ok(channel) = mgmtd.connect().await {
        let mut client = pb::mgmt_client::MgmtClient::new(channel);
        let _ = client
            .close_session(pb::CloseSessionRequest { session_id })
            .await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The gate is the shared table, and an admin passes everything.
    #[test]
    fn operators_are_refused_the_privileged_verbs() {
        let operator = Identity {
            user: "noc".into(),
            role: Role::Operator,
            session_id: 1,
        };
        for verb in hemlock_common::role::ADMIN_CLI_VERBS {
            assert_eq!(
                operator.check(verb).unwrap_err(),
                "% permission denied (operator role)",
                "{verb} was allowed"
            );
        }
        // Looking is always allowed.
        for verb in ["show", "bash", "exit", "help"] {
            assert!(operator.check(verb).is_ok(), "{verb} was refused");
        }

        let admin = Identity {
            role: Role::Admin,
            ..operator
        };
        for verb in hemlock_common::role::ADMIN_CLI_VERBS {
            assert!(admin.check(verb).is_ok(), "{verb} was refused for admin");
        }
    }

    /// A peer address comes from the ssh environment; a local login
    /// reads as the console.
    #[test]
    fn login_source_reads_the_ssh_environment() {
        // The environment is process-global; this is the only test that
        // touches these two variables.
        std::env::remove_var("SSH_CLIENT");
        std::env::set_var("SSH_CONNECTION", "10.42.0.100 51234 10.42.0.9 22");
        assert_eq!(login_source(), "10.42.0.100");
        std::env::remove_var("SSH_CONNECTION");
        std::env::set_var("SSH_CLIENT", "10.42.0.101 51234 22");
        assert_eq!(login_source(), "10.42.0.101");
        std::env::remove_var("SSH_CLIENT");
        assert_eq!(login_source(), "console");
    }
}
