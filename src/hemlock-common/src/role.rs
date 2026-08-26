//! Login roles and the one privileged-operation table both front-ends
//! enforce.
//!
//! `admin` may do everything; `operator` may look but not touch. The
//! CLI refuses the config-mode verbs and every `request`, the web
//! console answers 403 on every editing, clearing and request
//! endpoint — and both read the *same* table below, so a new
//! privileged endpoint cannot be gated in one front-end and forgotten
//! in the other.
//!
//! This is a guard rail, not a privilege boundary. An operator with
//! shell access (`bash` from the CLI, or ssh) can still run anything
//! their OS account permits; the kernel and the `hemlock` group are
//! the real boundary. What the roles buy is that a read-only operator
//! cannot change the switch by accident through either console.

/// The refusal both front-ends print, verbatim.
pub const PERMISSION_DENIED: &str = "% permission denied (operator role)";

/// A login user's privilege level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Role {
    /// Full access: configuration, commits, and every `request` verb.
    Admin,
    /// Read-only. The least-privilege default for a new user.
    #[default]
    Operator,
}

impl Role {
    /// The config spelling (`role admin;`).
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "admin" => Some(Role::Admin),
            "operator" => Some(Role::Operator),
            _ => None,
        }
    }

    pub fn is_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The role keywords, for completion and validation.
pub const ROLES: &[&str] = &["admin", "operator"];

/// Operational-mode and config-mode CLI verbs an operator may not run.
/// `show` and `bash` are deliberately absent: looking is what the role
/// is for, and the shell is the OS's boundary, not this one's.
pub const ADMIN_CLI_VERBS: &[&str] = &[
    "configure",
    "set",
    "delete",
    "commit",
    "rollback",
    "discard",
    "request",
    "upgrade",
];

/// webd API paths an operator may not POST to. Every editing, clearing
/// and request endpoint belongs here; webd's router test walks its own
/// POST routes against this list, so adding an endpoint without adding
/// it here fails the tests rather than shipping open.
pub const ADMIN_WEB_PATHS: &[&str] = &[
    "/api/acls/bindings/edit",
    "/api/acls/clear",
    "/api/acls/edit",
    "/api/arp/edit",
    "/api/arp/flush",
    "/api/bgp/edit",
    "/api/config/restore",
    "/api/copp/clear",
    "/api/copp/edit",
    "/api/dhcp/leases/clear",
    "/api/dhcp/relay/edit",
    "/api/dhcp/server/edit",
    "/api/dot1x/edit",
    "/api/dot1x/reauth",
    "/api/interfaces/edit",
    "/api/lags/edit",
    "/api/lldp/edit",
    "/api/mac-table/edit",
    "/api/mac-table/flush",
    "/api/mirror/edit",
    "/api/ntp/edit",
    "/api/ospf/edit",
    "/api/port-security/clear",
    "/api/port-security/edit",
    "/api/qos/maps/edit",
    "/api/qos/ports/edit",
    "/api/qos/wred/edit",
    "/api/reboot",
    "/api/reboot/cancel",
    "/api/routes/static/edit",
    "/api/sflow/edit",
    "/api/snmp/edit",
    "/api/snooping-sec/bindings/clear",
    "/api/snooping-sec/edit",
    "/api/snooping/edit",
    "/api/spanning-tree/clear-errdisable",
    "/api/spanning-tree/edit",
    "/api/storm-control/edit",
    "/api/svis/edit",
    "/api/system/certificate/regenerate",
    "/api/system/diag/cable",
    "/api/system/diag/ping",
    "/api/system/diag/traceroute",
    "/api/system/identity/edit",
    "/api/system/logging/edit",
    "/api/system/rollback",
    "/api/system/tech-support",
    "/api/system/users/edit",
    "/api/system/web/edit",
    "/api/upgrade/apply",
    "/api/upgrade/discard",
    "/api/upgrade/upload",
    "/api/users/add",
    "/api/vlans/edit",
    "/api/vrrp/edit",
];

/// Does this CLI verb need `admin`? Verbs arrive already resolved from
/// their prefix (`conf` -> `configure`), so the match is exact.
pub fn cli_requires_admin(verb: &str) -> bool {
    ADMIN_CLI_VERBS.contains(&verb)
}

/// Does this webd path need `admin`?
pub fn web_requires_admin(path: &str) -> bool {
    ADMIN_WEB_PATHS.contains(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip() {
        assert_eq!(Role::parse("admin"), Some(Role::Admin));
        assert_eq!(Role::parse("operator"), Some(Role::Operator));
        assert_eq!(Role::parse("Admin"), None);
        assert_eq!(Role::default(), Role::Operator);
        assert!(Role::Admin.is_admin());
        assert!(!Role::Operator.is_admin());
        assert_eq!(Role::Admin.to_string(), "admin");
    }

    #[test]
    fn the_table_gates_the_verbs_and_paths_it_names() {
        assert!(cli_requires_admin("commit"));
        assert!(cli_requires_admin("request"));
        assert!(!cli_requires_admin("show"));
        assert!(!cli_requires_admin("bash"));
        assert!(web_requires_admin("/api/interfaces/edit"));
        assert!(!web_requires_admin("/api/interfaces"));
    }

    /// Every entry is a distinct absolute API path — a duplicate would
    /// mean two lists were merged carelessly.
    #[test]
    fn the_web_table_is_well_formed() {
        let mut sorted = ADMIN_WEB_PATHS.to_vec();
        sorted.sort_unstable();
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(sorted, deduped, "duplicate entry in ADMIN_WEB_PATHS");
        assert_eq!(sorted, ADMIN_WEB_PATHS, "keep ADMIN_WEB_PATHS sorted");
        assert!(ADMIN_WEB_PATHS.iter().all(|p| p.starts_with("/api/")));
    }
}
