//! ONIE `machine.conf` parsing and platform matching.
//!
//! ONIE exposes the discovered machine as shell-style assignments, e.g.:
//! ```text
//! onie_machine=cel_e1031
//! onie_arch=x86_64
//! onie_machine_rev=0
//! onie_platform=x86_64-cel_e1031-r0
//! ```
//! The Hemlock image is built for exactly one `onie_machine` string (from
//! the platform manifest); installing onto anything else is refused unless
//! the operator forces it.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("machine.conf has no onie_platform (or onie_machine) entry")]
    NoPlatform,
}

#[derive(Debug, Clone)]
pub struct MachineConf {
    pub values: HashMap<String, String>,
}

impl MachineConf {
    pub fn parse(text: &str) -> Self {
        let values = text
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let (key, value) = line.split_once('=')?;
                Some((
                    key.trim().to_string(),
                    value.trim().trim_matches('"').to_string(),
                ))
            })
            .collect();
        Self { values }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, MachineError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| MachineError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Ok(Self::parse(&text))
    }

    /// The full platform string, assembled the way ONIE does if only the
    /// parts are present: `<arch>-<machine>-r<rev>`.
    pub fn platform(&self) -> Result<String, MachineError> {
        if let Some(platform) = self.values.get("onie_platform") {
            return Ok(platform.clone());
        }
        let machine = self
            .values
            .get("onie_machine")
            .ok_or(MachineError::NoPlatform)?;
        let arch = self
            .values
            .get("onie_arch")
            .map(String::as_str)
            .unwrap_or("x86_64");
        let rev = self
            .values
            .get("onie_machine_rev")
            .map(String::as_str)
            .unwrap_or("0");
        Ok(format!("{arch}-{machine}-r{rev}"))
    }

    /// Does this machine match the platform the image was built for?
    pub fn matches(&self, expected_onie_machine: &str) -> bool {
        self.platform()
            .map(|p| p == expected_onie_machine)
            .unwrap_or(false)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const E1031: &str = "\
# ONIE machine discovery
onie_machine=cel_e1031
onie_arch=x86_64
onie_machine_rev=0
onie_platform=x86_64-cel_e1031-r0
onie_version=2019.05
";

    #[test]
    fn parses_and_matches() {
        let conf = MachineConf::parse(E1031);
        assert_eq!(conf.platform().unwrap(), "x86_64-cel_e1031-r0");
        assert!(conf.matches("x86_64-cel_e1031-r0"));
        assert!(!conf.matches("x86_64-cel_questone2a-r0"));
    }

    #[test]
    fn assembles_platform_from_parts() {
        let conf =
            MachineConf::parse("onie_machine=acme_sw48\nonie_arch=x86_64\nonie_machine_rev=2\n");
        assert_eq!(conf.platform().unwrap(), "x86_64-acme_sw48-r2");
    }

    #[test]
    fn missing_platform_is_an_error() {
        let conf = MachineConf::parse("onie_version=2019.05\n");
        assert!(conf.platform().is_err());
        assert!(!conf.matches("anything"));
    }

    #[test]
    fn ignores_comments_and_quotes() {
        let conf = MachineConf::parse("# c\nonie_platform=\"x86_64-cel_e1031-r0\"\n");
        assert!(conf.matches("x86_64-cel_e1031-r0"));
    }
}
