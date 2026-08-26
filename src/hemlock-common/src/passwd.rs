//! SHA-512-crypt hashing for config-managed login users.
//!
//! `set system login user <name> password <plaintext>` hashes at the
//! prompt: only the crypt string ever reaches the candidate, so the
//! plaintext is never written to `candidate.conf`, never rendered by
//! `show configuration`, and never lands in a rollback ring entry or a
//! tech-support bundle. The OS applier feeds the same string straight
//! to `/etc/shadow`.
//!
//! SHA-512 (`$6$`) rather than Debian trixie's default yescrypt: the
//! hash is stored in the *configuration*, which is portable across
//! images and restorable onto older ones, and `$6$` is the format every
//! Linux libcrypt has understood for over a decade. Verification
//! accepts both, so an imported yescrypt hash still logs in.

/// The shortest plaintext accepted at `set` time.
pub const MIN_PASSWORD_LEN: usize = 8;

/// Hash `plaintext` as SHA-512-crypt with a fresh random salt.
pub fn hash(plaintext: &str) -> Result<String, String> {
    use sha_crypt::PasswordHasher as _;
    sha_crypt::ShaCrypt::default()
        .hash_password(plaintext.as_bytes())
        .map(|hash| hash.as_str().to_string())
        .map_err(|e| format!("cannot hash password: {e}"))
}

/// Does `password` clear the prompt-time rules? Length and the two
/// characters that would corrupt `/etc/shadow`; a switch's local
/// accounts are not the place for a dictionary check, and the message
/// has to say exactly what to fix.
pub fn check_plaintext(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    if password.contains(['\n', ':']) {
        return Err("password must not contain ':' or newlines".into());
    }
    Ok(())
}

/// Is this a crypt string `/etc/shadow` would accept? Deliberately
/// permissive about the scheme so a config restored from another box
/// (yescrypt `$y$`, SHA-256 `$5$`) still loads; the shape is what is
/// checked.
pub fn valid_hash(hash: &str) -> bool {
    if hash.contains(char::is_whitespace) {
        return false;
    }
    let mut parts = hash.split('$');
    // A crypt string starts with `$`, so the first split part is empty.
    if parts.next() != Some("") {
        return false;
    }
    let Some(scheme) = parts.next() else {
        return false;
    };
    if !matches!(scheme, "6" | "5" | "y" | "7" | "2b" | "2y") {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    // salt + hash at minimum; `$6$rounds=N$salt$hash` adds one.
    rest.len() >= 2 && rest.iter().all(|part| !part.is_empty())
}

/// Verify `plaintext` against a stored crypt string.
pub fn verify(plaintext: &str, hash: &str) -> bool {
    if hash.is_empty() || hash.starts_with('!') || hash.starts_with('*') {
        return false;
    }
    if hash.starts_with("$y$") {
        use yescrypt::PasswordVerifier as _;
        yescrypt::Yescrypt::default()
            .verify_password(plaintext.as_bytes(), hash)
            .is_ok()
    } else if hash.starts_with("$6$") || hash.starts_with("$5$") {
        use sha_crypt::PasswordVerifier as _;
        sha_crypt::ShaCrypt::default()
            .verify_password(plaintext.as_bytes(), hash)
            .is_ok()
    } else {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hashes_round_trip_and_salt_afresh() {
        let first = hash("correct horse").unwrap();
        let second = hash("correct horse").unwrap();
        assert!(first.starts_with("$6$") || first.starts_with("$5$"));
        assert_ne!(first, second, "each hash gets its own salt");
        assert!(valid_hash(&first));
        assert!(verify("correct horse", &first));
        assert!(!verify("wrong horse", &first));
    }

    #[test]
    fn plaintext_rules() {
        assert!(check_plaintext("longenough").is_ok());
        assert_eq!(
            check_plaintext("short").unwrap_err(),
            "password must be at least 8 characters"
        );
        assert!(check_plaintext("has:colon1").is_err());
    }

    #[test]
    fn hash_shape() {
        assert!(valid_hash("$6$rounds=656000$abcdefgh$ijklmnop"));
        assert!(valid_hash("$6$abcdefgh$ijklmnop"));
        assert!(valid_hash("$y$j9T$abc$def"));
        assert!(!valid_hash("plaintext"));
        assert!(!valid_hash("$1$md5$hash")); // MD5 crypt is not accepted
        assert!(!valid_hash("$6$onlysalt"));
        assert!(!valid_hash("$6$has space$hash"));
        assert!(!valid_hash(""));
    }

    /// A locked or empty shadow field never matches.
    #[test]
    fn locked_hashes_never_verify() {
        assert!(!verify("anything", ""));
        assert!(!verify("anything", "!"));
        assert!(!verify("anything", "*"));
    }
}
