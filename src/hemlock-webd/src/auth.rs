//! Login verification and session tracking.
//!
//! Credentials are the switch's own operator accounts: the password is
//! verified against /etc/shadow (yescrypt on Debian trixie, sha-crypt
//! for legacy hashes — both pure Rust), and the account must belong to
//! the `hemlock` group, the same gate that grants CLI socket access.
//! Sessions are in-memory bearer tokens carried in an HttpOnly cookie;
//! a webd restart signs everyone out, which is the right failure mode
//! for a switch.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Idle timeout: a session ends this long after its last request.
const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);
/// Failed logins stall this long — brute force at 1.25 guesses/second.
const FAILURE_DELAY: Duration = Duration::from_millis(800);

struct Session {
    username: String,
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

    pub fn create(&self, username: &str) -> String {
        let token = random_token();
        let mut sessions = self.lock();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(
            token.clone(),
            Session {
                username: username.to_string(),
                expires: now + SESSION_TTL,
            },
        );
        token
    }

    /// Resolve a token to its user, sliding the expiry forward.
    pub fn touch(&self, token: &str) -> Option<String> {
        let mut sessions = self.lock();
        let now = Instant::now();
        match sessions.get_mut(token) {
            Some(session) if session.expires > now => {
                session.expires = now + SESSION_TTL;
                Some(session.username.clone())
            }
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
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

/// Verify a login attempt. Failures pay `FAILURE_DELAY` before the
/// answer, and the error never says which part was wrong.
pub async fn verify(
    dev_auth: Option<&(String, String)>,
    username: &str,
    password: &str,
) -> Result<(), AuthError> {
    if check(dev_auth, username, password) {
        Ok(())
    } else {
        tokio::time::sleep(FAILURE_DELAY).await;
        Err(AuthError::Invalid)
    }
}

fn check(dev_auth: Option<&(String, String)>, username: &str, password: &str) -> bool {
    if let Some((user, pass)) = dev_auth {
        if user == username && pass == password {
            return true;
        }
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

    #[test]
    fn sessions_round_trip() {
        let sessions = Sessions::new();
        let token = sessions.create("admin");
        assert_eq!(token.len(), 64);
        assert_eq!(sessions.touch(&token).as_deref(), Some("admin"));
        sessions.remove(&token);
        assert_eq!(sessions.touch(&token), None);
        // Unknown tokens resolve to nothing.
        assert_eq!(sessions.touch("nonsense"), None);
    }

    #[test]
    fn tokens_are_unique() {
        let sessions = Sessions::new();
        assert_ne!(sessions.create("a"), sessions.create("a"));
    }

    #[tokio::test]
    async fn dev_auth_verifies() {
        let dev = ("admin".to_string(), "secret".to_string());
        assert!(verify(Some(&dev), "admin", "secret").await.is_ok());
        assert!(verify(Some(&dev), "admin", "wrong").await.is_err());
        assert!(verify(Some(&dev), "other", "secret").await.is_err());
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
