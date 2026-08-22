//! Platform manifests: the "platform = data, not code" layer.
//!
//! A platform is a directory under `platforms/` holding a `platform.toml`
//! manifest plus vendor data files. This crate defines the manifest schema,
//! loads and validates it, and provides the [`quirks::PlatformQuirks`] escape
//! hatch for boards that need custom CPLD/LED behavior.

pub mod lint;
pub mod quirks;
pub mod schema;
pub mod sysinit;

use std::path::{Path, PathBuf};

pub use schema::Manifest;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("unsupported schema_version {found} (supported: {supported})")]
    SchemaVersion { found: u32, supported: u32 },

    #[error("invalid port table: {0}")]
    Ports(String),

    #[error("platform {id:?} not found under {root}")]
    NotFound { id: String, root: PathBuf },

    #[error("unknown quirks driver {0:?}")]
    UnknownQuirks(String),
}

/// One fully expanded front-panel port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortDef {
    pub name: String,
    /// 1-based front-panel index.
    pub index: u32,
    pub lanes: Vec<u32>,
    pub speed_mbps: u32,
    pub alias: Option<String>,
    pub autoneg: bool,
    pub media: Option<String>,
    pub breakout: Vec<String>,
    /// PHY model for `show interfaces phy`; `None` = direct serdes.
    pub phy_model: Option<String>,
    /// Speed/duplex modes for `show interfaces capabilities`.
    pub supported_modes: Vec<String>,
}

/// A loaded platform: manifest + its directory + the expanded port table.
#[derive(Debug, Clone)]
pub struct Platform {
    pub dir: PathBuf,
    pub manifest: Manifest,
    /// Expanded from `[ports]`, sorted by front-panel index.
    pub ports: Vec<PortDef>,
}

impl Platform {
    /// Load `platform.toml` from a platform directory.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, PlatformError> {
        let dir = dir.as_ref().to_path_buf();
        let path = dir.join("platform.toml");
        let text = std::fs::read_to_string(&path).map_err(|source| PlatformError::Read {
            path: path.clone(),
            source,
        })?;
        let manifest: Manifest = toml::from_str(&text).map_err(|source| PlatformError::Parse {
            path: path.clone(),
            source: Box::new(source),
        })?;
        if manifest.schema_version != schema::SCHEMA_VERSION {
            return Err(PlatformError::SchemaVersion {
                found: manifest.schema_version,
                supported: schema::SCHEMA_VERSION,
            });
        }
        let ports = expand_ports(&manifest)?;
        Ok(Self {
            dir,
            manifest,
            ports,
        })
    }

    /// Resolve a platform by id under a `platforms/` root, or load it
    /// directly if `id_or_path` is a directory path.
    pub fn find(platforms_root: impl AsRef<Path>, id_or_path: &str) -> Result<Self, PlatformError> {
        let as_path = Path::new(id_or_path);
        if as_path.is_dir() {
            return Self::load(as_path);
        }
        let root = platforms_root.as_ref();
        let dir = root.join(id_or_path);
        if dir.is_dir() {
            Self::load(dir)
        } else {
            Err(PlatformError::NotFound {
                id: id_or_path.to_string(),
                root: root.to_path_buf(),
            })
        }
    }

    /// Absolute path of the ASIC init config (`config.bcm`).
    pub fn config_bcm_path(&self) -> PathBuf {
        self.dir.join(&self.manifest.sai.config_bcm)
    }

    /// Absolute paths of the SAI extra files (LED microcode etc.).
    pub fn extra_file_paths(&self) -> Vec<PathBuf> {
        self.manifest
            .sai
            .extra_files
            .iter()
            .map(|f| self.dir.join(f))
            .collect()
    }

    /// The registered quirks implementation named by the manifest.
    pub fn quirks(&self) -> Result<Box<dyn quirks::PlatformQuirks>, PlatformError> {
        quirks::by_name(&self.manifest.hardware.quirks.driver).ok_or_else(|| {
            PlatformError::UnknownQuirks(self.manifest.hardware.quirks.driver.clone())
        })
    }

    pub fn port(&self, name: &str) -> Option<&PortDef> {
        self.ports.iter().find(|p| p.name == name)
    }
}

/// Expand groups + explicit entries into the flat port table.
fn expand_ports(manifest: &Manifest) -> Result<Vec<PortDef>, PlatformError> {
    let mut ports = Vec::new();

    for group in &manifest.ports.groups {
        let lpp = group.lanes_per_port as usize;
        if lpp == 0 {
            return Err(PlatformError::Ports(format!(
                "group {:?}: lanes_per_port must be >= 1",
                group.prefix
            )));
        }
        if group.lanes.is_empty() || group.lanes.len() % lpp != 0 {
            return Err(PlatformError::Ports(format!(
                "group {:?}: lanes length {} is not a positive multiple of lanes_per_port {}",
                group.prefix,
                group.lanes.len(),
                lpp
            )));
        }
        for (i, chunk) in group.lanes.chunks(lpp).enumerate() {
            let i = i as u32;
            let index = group.index_start + i;
            ports.push(PortDef {
                name: format!("{}{}", group.prefix, group.name_start + i),
                index,
                lanes: chunk.to_vec(),
                speed_mbps: group.speed_mbps,
                alias: group.alias_prefix.as_ref().map(|p| format!("{p}{index}")),
                autoneg: group.autoneg,
                media: group.media.clone(),
                breakout: group.breakout.clone(),
                phy_model: group.phy_model.clone(),
                supported_modes: group.supported_modes.clone(),
            });
        }
    }

    for entry in &manifest.ports.ports {
        ports.push(PortDef {
            name: entry.name.clone(),
            index: entry.index,
            lanes: entry.lanes.clone(),
            speed_mbps: entry.speed_mbps,
            alias: entry.alias.clone(),
            autoneg: entry.autoneg,
            media: entry.media.clone(),
            breakout: entry.breakout.clone(),
            phy_model: entry.phy_model.clone(),
            supported_modes: entry.supported_modes.clone(),
        });
    }

    if ports.is_empty() {
        return Err(PlatformError::Ports(
            "no ports defined (need at least one [[ports.group]] or [[ports.port]])".into(),
        ));
    }

    ports.sort_by_key(|p| p.index);
    Ok(ports)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn minimal_manifest(ports: &str) -> String {
        format!(
            r#"
schema_version = 1

[platform]
id = "test-sw"
onie_machine = "x86_64-test_sw-r0"
vendor = "Test"
model = "TSW-1"
asic_family = "broadcom-xgs"
asic = "helix4"

[sai]
package = "libsaibcm"
version_pin = "3.7.x-helix4"
libsai_path = "/usr/lib/libsai.so.1"
config_bcm = "config.bcm"

{ports}
"#
        )
    }

    fn load_from_str(toml_text: &str) -> Result<Platform, PlatformError> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("platform.toml"), toml_text).unwrap();
        Platform::load(dir.path())
    }

    #[test]
    fn expands_port_groups_with_swapped_lanes() {
        let p = load_from_str(&minimal_manifest(
            r#"
[[ports.group]]
prefix = "Ethernet"
name_start = 0
index_start = 1
speed_mbps = 1000
lanes = [2, 1, 4, 3]
alias_prefix = "etp"
autoneg = true
"#,
        ))
        .unwrap();
        assert_eq!(p.ports.len(), 4);
        assert_eq!(p.ports[0].name, "Ethernet0");
        assert_eq!(p.ports[0].lanes, vec![2]);
        assert_eq!(p.ports[0].alias.as_deref(), Some("etp1"));
        assert_eq!(p.ports[3].name, "Ethernet3");
        assert_eq!(p.ports[3].index, 4);
        assert_eq!(p.ports[3].lanes, vec![3]);
    }

    #[test]
    fn multi_lane_groups_chunk_lanes() {
        let p = load_from_str(&minimal_manifest(
            r#"
[[ports.group]]
prefix = "Ethernet"
name_start = 0
index_start = 1
speed_mbps = 40000
lanes_per_port = 4
lanes = [1, 2, 3, 4, 5, 6, 7, 8]
"#,
        ))
        .unwrap();
        assert_eq!(p.ports.len(), 2);
        assert_eq!(p.ports[0].lanes, vec![1, 2, 3, 4]);
        assert_eq!(p.ports[1].lanes, vec![5, 6, 7, 8]);
    }

    #[test]
    fn rejects_wrong_schema_version() {
        let text = minimal_manifest(
            "[[ports.port]]\nname = \"Ethernet0\"\nindex = 1\nspeed_mbps = 1000\nlanes = [1]",
        )
        .replace("schema_version = 1", "schema_version = 99");
        assert!(matches!(
            load_from_str(&text),
            Err(PlatformError::SchemaVersion { found: 99, .. })
        ));
    }

    #[test]
    fn rejects_ragged_lane_list() {
        let err = load_from_str(&minimal_manifest(
            r#"
[[ports.group]]
prefix = "Ethernet"
name_start = 0
index_start = 1
speed_mbps = 40000
lanes_per_port = 4
lanes = [1, 2, 3]
"#,
        ))
        .unwrap_err();
        assert!(matches!(err, PlatformError::Ports(_)));
    }

    #[test]
    fn rejects_empty_port_table() {
        let err = load_from_str(&minimal_manifest("[ports]")).unwrap_err();
        assert!(matches!(err, PlatformError::Ports(_)));
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = minimal_manifest(
            "[[ports.port]]\nname = \"Ethernet0\"\nindex = 1\nspeed_mbps = 1000\nlanes = [1]",
        )
        .replace("[sai]", "[sai]\nbogus_field = true");
        assert!(matches!(
            load_from_str(&text),
            Err(PlatformError::Parse { .. })
        ));
    }
}
