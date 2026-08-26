//! Login verification and session tracking.
//!
//! Credentials come from the configuration wherever it manages login
//! users (`system { login { user <name> { password-hash ... } } }`) —
//! that is the source of truth, and checking it means the console
//! works the moment a commit lands, without waiting on the OS applier.
//! For any other account the fallback is the switch's own user
//! database: /etc/shadow (yescrypt on Debian trixie, sha-crypt for
//! legacy hashes), gated on `hemlock` group membership, the same gate
//! that grants CLI socket access.
//!
//! Sessions are in-memory bearer tokens carried in an HttpOnly cookie;
//! a webd restart signs everyone out, which is the right failure mode
//! for a switch. Each carries the role it was opened with, so the
//! privileged-endpoint gate never has to re-read the config per
//! request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hemlock_common::role::Role;

/// The idle timeout with nothing configured, in minutes — mirrors
/// mgmtd's `DEFAULT_WEB_SESSION_TIMEOUT`.
pub const DEFAULT_SESSION_TIMEOUT_MINS: u32 = 30;
/// Failed logins stall this long — brute force at 1.25 guesses/second.
const FAILURE_DELAY: Duration = Duration::from_millis(800);

/// One signed-in operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub username: String,
    pub role: Role,
    /// mgmtd's handle for this session, so `show system users` lists
    /// it; 0 when mgmtd could not be reached at login.
    pub mgmtd_session_id: u64,
}

struct Session {
    info: SessionInfo,
    ttl: Duration,
    expires: Instant,
}

pub struct Sessions(Mutex<HashMap<String, Session>>);

impl Sessions {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Open a session with `timeout_mins` of idle life.
    pub fn create(&self, info: SessionInfo, timeout_mins: u32) -> String {
        let token = random_token();
        let ttl = Duration::from_secs(u64::from(timeout_mins.max(1)) * 60);
        let mut sessions = self.lock();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(
            token.clone(),
            Session {
                info,
                ttl,
                expires: now + ttl,
            },
        );
        token
    }

    /// Resolve a token to its session, sliding the expiry forward.
    pub fn touch(&self, token: &str) -> Option<SessionInfo> {
        let mut sessions = self.lock();
        let now = Instant::now();
        match sessions.get_mut(token) {
            Some(session) if session.expires > now => {
                session.expires = now + session.ttl;
                Some(session.info.clone())
            }
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
        }
    }

    /// The mgmtd handle a token carries, for the logout path.
    pub fn mgmtd_session_id(&self, token: &str) -> u64 {
        self.lock()
            .get(token)
            .map(|s| s.info.mgmtd_session_id)
            .unwrap_or(0)
    }

    /// Apply a role change to every session of `username` — a commit
    /// that demotes an account must reach the console already open.
    pub fn set_role(&self, username: &str, role: Role) {
        for session in self.lock().values_mut() {
            if session.info.username == username {
                session.info.role = role;
            }
        }
    }

    pub fn remove(&self, token: &str) {
        self.lock().remove(token);
    }
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    // Failure here means the OS RNG is broken; an unguessable token is
    // non-negotiable, so give up loudly rather than degrade.
    getrandom::fill(&mut bytes).expect("OS random number generator failed");
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid username or password")]
    Invalid,
}

/// What the running config says about one account: its stored hash and
/// role, when the config manages login users at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigAccount {
    pub password_hash: Option<String>,
    pub role: Role,
}

/// The `system { login { user <name> { ... } } }` account, if any.
pub fn config_account(
    tree: &hemlock_config::ConfigTree,
    username: &str,
) -> Option<ConfigAccount> {
    use hemlock_config::ConfigTree;
    let (_, system) = tree.block("system")?;
    let (_, login) = ConfigTree::blocks_named(system, "login").next()?;
    let (_, user) = ConfigTree::blocks_named(login, "user")
        .find(|(keys, _)| keys.first().map(String::as_str) == Some(username))?;
    Some(ConfigAccount {
        password_hash: ConfigTree::leaf_value(user, "password-hash").map(str::to_string),
        // Least privilege: an omitted role is `operator`.
        role: ConfigTree::leaf_value(user, "role")
            .and_then(Role::parse)
            .unwrap_or_default(),
    })
}

/// Verify a login attempt. Failures pay `FAILURE_DELAY` before the
/// answer, and the error never says which part was wrong.
pub async fn verify(
    dev_auth: Option<&(String, String)>,
    account: Option<&ConfigAccount>,
    username: &str,
    password: &str,
) -> Result<(), AuthError> {
    if check(dev_auth, account, username, password) {
        Ok(())
    } else {
        tokio::time::sleep(FAILURE_DELAY).await;
        Err(AuthError::Invalid)
    }
}

fn check(
    dev_auth: Option<&(String, String)>,
    account: Option<&ConfigAccount>,
    username: &str,
    password: &str,
) -> bool {
    if let Some((user, pass)) = dev_auth {
        if user == username && pass == password {
            return true;
        }
    }
    // A config-managed account answers from its stored hash: the
    // configuration is the source of truth, so a just-committed
    // password works before the OS applier has run. An account the
    // config manages *without* a password (ssh-key only) can never log
    // in to the console, and must not fall through to /etc/shadow.
    if let Some(account) = account {
        return match &account.password_hash {
            Some(hash) => hemlock_common::passwd::verify(password, hash),
            None => false,
        };
    }
    system_check(username, password)
}

/// Verify against the on-box user database. Only meaningful on the
/// switch (root can read /etc/shadow); on dev hosts it simply finds no
/// account and fails, leaving --dev-auth as the way in.
fn system_check(username: &str, password: &str) -> bool {
    if username.is_empty() || username.contains(':') {
        return false;
    }
    if !operator_account(username) {
        tracing::info!(username, "login refused: not in the hemlock group");
        return false;
    }
    let Some(hash) = shadow_hash(username) else {
        return false;
    };
    // A locked or empty password field never matches.
    if hash.is_empty() || hash.starts_with('!') || hash.starts_with('*') {
        return false;
    }
    if hash.starts_with("$y$") {
        use yescrypt::PasswordVerifier as _;
        yescrypt::Yescrypt::default()
            .verify_password(password.as_bytes(), hash.as_str())
            .is_ok()
    } else if hash.starts_with("$6$") || hash.starts_with("$5$") {
        use sha_crypt::PasswordVerifier as _;
        sha_crypt::ShaCrypt::default()
            .verify_password(password.as_bytes(), hash.as_str())
            .is_ok()
    } else {
        tracing::warn!(username, "unsupported password hash scheme in /etc/shadow");
        false
    }
}

/// The account's password hash from /etc/shadow.
fn shadow_hash(username: &str) -> Option<String> {
    let shadow = std::fs::read_to_string("/etc/shadow").ok()?;
    shadow.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == username).then(|| fields.next().unwrap_or("").to_string())
    })
}

/// Operator accounts belong to the `hemlock` group (as supplementary or
/// primary group) — the same membership that grants CLI socket access.
fn operator_account(username: &str) -> bool {
    let Ok(groups) = std::fs::read_to_string("/etc/group") else {
        return false;
    };
    let mut hemlock_gid: Option<&str> = None;
    for line in groups.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some("hemlock") {
            continue;
        }
        let _password = fields.next();
        hemlock_gid = fields.next();
        if let Some(members) = fields.next() {
            if members.split(',').any(|m| m.trim() == username) {
                return true;
            }
        }
        break;
    }
    let Some(gid) = hemlock_gid else {
        return false;
    };
    // Primary-group membership: the passwd entry's gid field.
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else {
        return false;
    };
    passwd.lines().any(|line| {
        let mut fields = line.split(':');
        fields.next() == Some(username) && fields.nth(2).map(str::trim) == Some(gid.trim())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn info(username: &str, role: Role) -> SessionInfo {
        SessionInfo {
            username: username.to_string(),
            role,
            mgmtd_session_id: 7,
        }
    }

    #[test]
    fn sessions_round_trip() {
        let sessions = Sessions::new();
        let token = sessions.create(info("admin", Role::Admin), 30);
        assert_eq!(token.len(), 64);
        let live = sessions.touch(&token).unwrap();
        assert_eq!(live.username, "admin");
        assert_eq!(live.role, Role::Admin);
        assert_eq!(sessions.mgmtd_session_id(&token), 7);
        sessions.remove(&token);
        assert_eq!(sessions.touch(&token), None);
        // Unknown tokens resolve to nothing.
        assert_eq!(sessions.touch("nonsense"), None);
        assert_eq!(sessions.mgmtd_session_id("nonsense"), 0);
    }

    /// A commit that demotes an account reaches the console session
    /// already open, without a re-login.
    #[test]
    fn role_changes_reach_live_sessions() {
        let sessions = Sessions::new();
        let token = sessions.create(info("noc", Role::Admin), 30);
        let other = sessions.create(info("cody", Role::Admin), 30);
        sessions.set_role("noc", Role::Operator);
        assert_eq!(sessions.touch(&token).unwrap().role, Role::Operator);
        assert_eq!(sessions.touch(&other).unwrap().role, Role::Admin);
    }

    #[test]
    fn tokens_are_unique() {
        let sessions = Sessions::new();
        assert_ne!(
            sessions.create(info("a", Role::Admin), 30),
            sessions.create(info("a", Role::Admin), 30)
        );
    }

    #[tokio::test]
    async fn dev_auth_verifies() {
        let dev = ("admin".to_string(), "secret".to_string());
        assert!(verify(Some(&dev), None, "admin", "secret").await.is_ok());
        assert!(verify(Some(&dev), None, "admin", "wrong").await.is_err());
        assert!(verify(Some(&dev), None, "other", "secret").await.is_err());
    }

    /// A config-managed account answers from its stored hash, and a
    /// key-only one can never log in to the console.
    #[tokio::test]
    async fn config_accounts_verify_against_their_stored_hash() {
        let hash = hemlock_common::passwd::hash("hunter2hunter2").unwrap();
        let account = ConfigAccount {
            password_hash: Some(hash),
            role: Role::Admin,
        };
        assert!(verify(None, Some(&account), "cody", "hunter2hunter2")
            .await
            .is_ok());
        assert!(verify(None, Some(&account), "cody", "wrong").await.is_err());

        let key_only = ConfigAccount {
            password_hash: None,
            role: Role::Operator,
        };
        assert!(verify(None, Some(&key_only), "jo", "anything")
            .await
            .is_err());
    }

    #[test]
    fn reads_config_accounts_out_of_the_tree() {
        let tree = hemlock_config::parse(
            "system { login {\n\
             user cody { role admin\npassword-hash \"$6$a$bcdefgh\" }\n\
             user jo { ssh-key \"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIN0ex4mpl3 jo@luna\" }\n\
             } }",
        )
        .unwrap_or_default();
        let cody = config_account(&tree, "cody").unwrap();
        assert_eq!(cody.role, Role::Admin);
        assert_eq!(cody.password_hash.as_deref(), Some("$6$a$bcdefgh"));
        let jo = config_account(&tree, "jo").unwrap();
        assert_eq!(jo.role, Role::Operator);
        assert!(jo.password_hash.is_none());
        // Not config-managed at all.
        assert!(config_account(&tree, "nobody").is_none());
        assert!(config_account(&hemlock_config::ConfigTree::default(), "cody").is_none());
    }

    // Round-trip through each hasher: what chpasswd would put in
    // /etc/shadow (same MCF strings) must verify, wrong passwords not.
    #[test]
    fn yescrypt_hashes_verify() {
        use yescrypt::{PasswordHasher as _, PasswordVerifier as _};
        let yescrypt = yescrypt::Yescrypt::default();
        let hash = yescrypt.hash_password(b"password").unwrap();
        let hash = hash.as_str();
        assert!(hash.starts_with("$y$"));
        assert!(yescrypt.verify_password(b"password", hash).is_ok());
        assert!(yescrypt.verify_password(b"not-the-password", hash).is_err());
    }

    #[test]
    fn sha512_crypt_hashes_verify() {
        use sha_crypt::{PasswordHasher as _, PasswordVerifier as _};
        let sha = sha_crypt::ShaCrypt::default();
        let hash = sha.hash_password(b"password").unwrap();
        let hash = hash.as_str();
        assert!(hash.starts_with("$6$") || hash.starts_with("$5$"));
        assert!(sha.verify_password(b"password", hash).is_ok());
        assert!(sha.verify_password(b"not-the-password", hash).is_err());
    }
}
