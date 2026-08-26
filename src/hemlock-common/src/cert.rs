//! Self-signed TLS material for `set system https`.
//!
//! On first https startup a certificate is generated (rcgen, pure Rust)
//! and persisted in the state directory, so the browser fingerprint
//! stays stable across reboots. Replacing the files with a real
//! certificate and key works too — they are plain PEM.
//!
//! It lives here rather than in webd because two daemons need it:
//! webd generates and serves the pair, and mgmtd regenerates it for
//! `request certificate regenerate` — the CLI cannot reach webd, and
//! two implementations of "what certificate is this box using" would
//! be one too many.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

/// Ten years: the cert identifies a box on a management network, not a
/// public site; nobody wants a switch to drop off the network because a
/// self-signed cert expired.
const VALIDITY_DAYS: i64 = 3650;

pub fn ensure_cert(state_dir: &Path, hostname: &str) -> Result<(PathBuf, PathBuf)> {
    let cert_path = state_dir.join("cert.pem");
    let key_path = state_dir.join("key.pem");
    if cert_path.exists() && key_path.exists() {
        return Ok((cert_path, key_path));
    }
    generate(state_dir, hostname)
}

/// Replace the certificate and key with a fresh pair.
///
/// `request certificate regenerate` exists for the case where the
/// stored key should no longer be trusted, so this always writes new
/// material — unlike [`ensure_cert`], which keeps what is there.
/// Sessions are unaffected: they live in webd's memory, not in the TLS
/// material, so an operator regenerating from the console is not signed
/// out. The listener picks the new pair up on the next restart, and the
/// caller is told so.
pub fn regenerate(state_dir: &Path, hostname: &str) -> Result<(PathBuf, PathBuf)> {
    generate(state_dir, hostname)
}

fn generate(state_dir: &Path, hostname: &str) -> Result<(PathBuf, PathBuf)> {
    let cert_path = state_dir.join("cert.pem");
    let key_path = state_dir.join("key.pem");

    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;

    let mut names = vec![hostname.to_string()];
    if hostname != "hemlock" {
        names.push("hemlock".to_string());
    }
    let mut params =
        rcgen::CertificateParams::new(names).context("building certificate parameters")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, hostname);
    let now = std::time::SystemTime::now();
    params.not_before = now.into();
    params.not_after = (now + std::time::Duration::from_secs(VALIDITY_DAYS as u64 * 86400)).into();

    let key_pair = rcgen::KeyPair::generate().context("generating TLS key")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-signing certificate")?;

    std::fs::write(&cert_path, cert.pem())
        .with_context(|| format!("writing {}", cert_path.display()))?;
    write_private(&key_path, key_pair.serialize_pem().as_bytes())?;
    info!(cert = %cert_path.display(), hostname, "generated self-signed TLS certificate");
    Ok((cert_path, key_path))
}

/// The SHA-256 fingerprint of a PEM certificate, in the colon-separated
/// uppercase hex every browser and `openssl x509 -fingerprint` shows —
/// so an operator can compare what the console reports with what their
/// browser warns about.
pub fn fingerprint(cert_pem: &str) -> Result<String> {
    let der = pem_body(cert_pem).context("certificate is not PEM")?;
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(&der);
    Ok(digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":"))
}

/// The DER bytes inside a PEM block.
fn pem_body(pem: &str) -> Option<Vec<u8>> {
    let base64: String = pem
        .lines()
        .skip_while(|line| !line.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END"))
        .flat_map(|line| line.trim().chars())
        .collect();
    if base64.is_empty() {
        return None;
    }
    decode_base64(&base64)
}

/// Minimal base64 decode: PEM only, so no whitespace handling beyond
/// what the caller already stripped, and no alternate alphabet.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|c| *c == byte)? as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// Write the key readable by owner only (the daemon runs as root).
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(contents)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
    }
}

/// The webd state directory, where the pair lives on a switch. Named
/// here so mgmtd and webd cannot disagree about the path.
pub const WEBD_STATE_DIR: &str = "/var/lib/hemlock/web";

/// The fingerprint of the pair currently on disk, if there is one.
pub fn current_fingerprint(state_dir: &Path) -> Option<String> {
    let pem = std::fs::read_to_string(state_dir.join("cert.pem")).ok()?;
    fingerprint(&pem).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Regenerating replaces the pair; the fingerprint moves with it,
    /// which is the whole point of the verb.
    #[test]
    fn regeneration_replaces_the_pair() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = ensure_cert(dir.path(), "hemlock-a1").unwrap();
        let first = std::fs::read_to_string(&cert).unwrap();
        let first_key = std::fs::read_to_string(&key).unwrap();
        // ensure_cert keeps what is there.
        ensure_cert(dir.path(), "hemlock-a1").unwrap();
        assert_eq!(std::fs::read_to_string(&cert).unwrap(), first);

        regenerate(dir.path(), "hemlock-a1").unwrap();
        let second = std::fs::read_to_string(&cert).unwrap();
        assert_ne!(second, first, "regenerate must write a new certificate");
        assert_ne!(
            std::fs::read_to_string(&key).unwrap(),
            first_key,
            "regenerate must write a new key"
        );
        assert_ne!(
            fingerprint(&first).unwrap(),
            fingerprint(&second).unwrap(),
            "a new certificate has a new fingerprint"
        );
    }

    /// The fingerprint is the colon-separated uppercase SHA-256 a
    /// browser shows, so the two can be compared by eye.
    #[test]
    fn fingerprints_read_like_a_browser_shows_them() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = ensure_cert(dir.path(), "hemlock-a1").unwrap();
        let print = fingerprint(&std::fs::read_to_string(&cert).unwrap()).unwrap();
        // 32 bytes as `AA:BB:...`.
        assert_eq!(print.len(), 32 * 3 - 1);
        assert_eq!(print.matches(':').count(), 31);
        assert!(print
            .split(':')
            .all(|byte| byte.len() == 2 && byte.chars().all(|c| c.is_ascii_hexdigit())));
        assert_eq!(print, print.to_uppercase());
        // Anything that is not a PEM certificate is refused.
        assert!(fingerprint("not a certificate").is_err());
    }

    #[test]
    fn generates_and_reuses_cert() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = ensure_cert(dir.path(), "sw1").unwrap();
        let pem = std::fs::read_to_string(&cert).unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE"));
        let key_pem = std::fs::read_to_string(&key).unwrap();
        assert!(key_pem.contains("PRIVATE KEY"));
        // Second call must not regenerate (stable fingerprint).
        let before = std::fs::read(&cert).unwrap();
        let (cert2, _) = ensure_cert(dir.path(), "sw1").unwrap();
        assert_eq!(cert, cert2);
        assert_eq!(before, std::fs::read(&cert2).unwrap());
    }
}
