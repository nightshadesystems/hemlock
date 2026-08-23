//! Self-signed TLS material for `set system https`.
//!
//! On first https startup a certificate is generated (rcgen, pure Rust)
//! and persisted in the state directory, so the browser fingerprint
//! stays stable across reboots. Replacing the files with a real
//! certificate and key works too — they are plain PEM.

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
