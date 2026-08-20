//! On-disk config state: running, candidate, and the rollback ring.
//!
//! Layout under the state directory:
//! ```text
//! running.conf
//! candidate.conf
//! rollback/1.conf   newest rollback point (the previous running config)
//! rollback/1.json   { committed_at, comment }
//! rollback/2.conf   ...up to RING_SIZE
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const RING_SIZE: u32 = 50;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RollbackMeta {
    pub committed_at: String, // RFC 3339
    pub comment: String,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(dir.join("rollback"))
            .with_context(|| format!("creating state dir {}", dir.display()))?;
        Ok(Self { dir })
    }

    fn running_path(&self) -> PathBuf {
        self.dir.join("running.conf")
    }

    fn candidate_path(&self) -> PathBuf {
        self.dir.join("candidate.conf")
    }

    fn rollback_conf(&self, n: u32) -> PathBuf {
        self.dir.join("rollback").join(format!("{n}.conf"))
    }

    fn rollback_meta(&self, n: u32) -> PathBuf {
        self.dir.join("rollback").join(format!("{n}.json"))
    }

    fn read_or_empty(path: &Path) -> Result<String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn write(path: &Path, text: &str) -> Result<()> {
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    pub fn running(&self) -> Result<String> {
        Self::read_or_empty(&self.running_path())
    }

    /// Candidate text; a fresh store's candidate equals running.
    pub fn candidate(&self) -> Result<String> {
        let path = self.candidate_path();
        if path.exists() {
            Self::read_or_empty(&path)
        } else {
            self.running()
        }
    }

    pub fn set_candidate(&self, text: &str) -> Result<()> {
        Self::write(&self.candidate_path(), text)
    }

    pub fn discard_candidate(&self) -> Result<()> {
        let running = self.running()?;
        self.set_candidate(&running)
    }

    /// Commit: current running becomes rollback 1 (ring shifted), the new
    /// text becomes both running and candidate.
    pub fn commit(&self, new_running: &str, meta: &RollbackMeta) -> Result<()> {
        // Shift the ring: N-1 -> N, ..., 1 -> 2.
        for n in (1..RING_SIZE).rev() {
            let (from_conf, to_conf) = (self.rollback_conf(n), self.rollback_conf(n + 1));
            if from_conf.exists() {
                std::fs::rename(&from_conf, &to_conf)
                    .with_context(|| format!("rotating rollback {n}"))?;
                let (from_meta, to_meta) = (self.rollback_meta(n), self.rollback_meta(n + 1));
                if from_meta.exists() {
                    std::fs::rename(&from_meta, &to_meta)
                        .with_context(|| format!("rotating rollback meta {n}"))?;
                }
            }
        }
        Self::write(&self.rollback_conf(1), &self.running()?)?;
        Self::write(
            &self.rollback_meta(1),
            &serde_json::to_string_pretty(meta).context("serializing rollback meta")?,
        )?;
        Self::write(&self.running_path(), new_running)?;
        self.set_candidate(new_running)
    }

    /// Text of rollback point `n` (1 = previous running).
    pub fn rollback(&self, n: u32) -> Result<Option<String>> {
        let path = self.rollback_conf(n);
        if path.exists() {
            Ok(Some(Self::read_or_empty(&path)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_rollbacks(&self) -> Vec<(u32, RollbackMeta)> {
        (1..=RING_SIZE)
            .filter(|n| self.rollback_conf(*n).exists())
            .map(|n| {
                let meta = std::fs::read_to_string(self.rollback_meta(n))
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or_default();
                (n, meta)
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn meta(comment: &str) -> RollbackMeta {
        RollbackMeta {
            committed_at: "2026-01-01T00:00:00Z".into(),
            comment: comment.into(),
        }
    }

    #[test]
    fn fresh_store_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.running().unwrap(), "");
        assert_eq!(store.candidate().unwrap(), "");
        assert!(store.list_rollbacks().is_empty());
    }

    #[test]
    fn commit_rotates_the_ring() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        store.commit("v1;\n", &meta("first")).unwrap();
        store.commit("v2;\n", &meta("second")).unwrap();
        store.commit("v3;\n", &meta("third")).unwrap();

        assert_eq!(store.running().unwrap(), "v3;\n");
        // rollback 1 = what was running before the last commit.
        assert_eq!(store.rollback(1).unwrap().unwrap(), "v2;\n");
        assert_eq!(store.rollback(2).unwrap().unwrap(), "v1;\n");
        assert_eq!(store.rollback(3).unwrap().unwrap(), "");
        assert!(store.rollback(4).unwrap().is_none());

        let listed = store.list_rollbacks();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].1.comment, "third");
    }

    #[test]
    fn candidate_tracks_running_after_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.set_candidate("draft;\n").unwrap();
        assert_eq!(store.candidate().unwrap(), "draft;\n");
        store.commit("draft;\n", &meta("go")).unwrap();
        assert_eq!(store.candidate().unwrap(), "draft;\n");
        store.discard_candidate().unwrap();
        assert_eq!(store.candidate().unwrap(), "draft;\n");
    }
}
