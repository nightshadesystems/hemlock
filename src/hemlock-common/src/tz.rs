//! The installed IANA time-zone database: existence checks and the
//! name list the CLI and web console complete from.
//!
//! `set system timezone <tz>` is validated against the tzdata actually
//! installed on the box (`/usr/share/zoneinfo`), not a compiled-in
//! list — a name the box cannot resolve would leave `timedatectl`
//! failing at commit time with nothing to show the operator. Both
//! hemlockctl (prompt-time feedback) and mgmtd (commit-time
//! re-validation) go through here, so the two can never disagree about
//! what a valid zone is.
//!
//! Development hosts without a tzdata tree (Windows, a bare container)
//! have nothing to check against; there [`exists`] accepts any
//! syntactically sound name rather than rejecting every one of them.

use std::path::{Path, PathBuf};

/// Where the zone files live. Overridable so tests can point at a
/// fixture tree.
pub fn zoneinfo_dir() -> PathBuf {
    match std::env::var("HEMLOCK_ZONEINFO_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/usr/share/zoneinfo"),
    }
}

/// Sub-trees the tz database ships that are not zone names operators
/// pick: the two alternate compilations and the legacy SysV aliases.
const SKIPPED_DIRS: &[&str] = &["posix", "right", "SystemV"];

/// Syntax every zone name obeys: letters, digits, `_`, `+`, `-` and
/// `/` separators, no empty or dot components. Checked before any
/// filesystem lookup so a name can never walk out of the zone tree.
pub fn valid_name(tz: &str) -> bool {
    if tz.is_empty() || tz.len() > 64 || tz.starts_with('/') || tz.ends_with('/') {
        return false;
    }
    tz.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
    })
}

/// Does the installed database carry this zone? Without a database
/// (development hosts) any syntactically valid name is accepted.
pub fn exists(tz: &str) -> bool {
    if !valid_name(tz) {
        return false;
    }
    let dir = zoneinfo_dir();
    if !dir.is_dir() {
        return true;
    }
    dir.join(tz).is_file()
}

/// Every zone name the installed database offers, sorted. Empty
/// without a database.
pub fn names() -> Vec<String> {
    let dir = zoneinfo_dir();
    let mut out = Vec::new();
    collect(&dir, "", &mut out, 0);
    out.sort();
    out.dedup();
    out
}

/// The tz tree is at most three levels deep (`America/Indiana/Knox`);
/// the depth cap keeps a symlink loop from walking forever.
fn collect(dir: &Path, prefix: &str, out: &mut Vec<String>, depth: usize) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            if depth == 0 && SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            let nested = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            collect(&path, &nested, out, depth + 1);
            continue;
        }
        // Top-level files are the database's own metadata
        // (`zone.tab`, `leapseconds`, ...) plus a handful of real
        // zones (`UTC`, `GMT`); the metadata all carries an extension
        // or a lowercase name, real zones never do.
        let zone = if prefix.is_empty() {
            if name.contains('.') || name.chars().next().is_some_and(|c| c.is_lowercase()) {
                continue;
            }
            name
        } else {
            format!("{prefix}/{name}")
        };
        if valid_name(&zone) {
            out.push(zone);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The zone directory is process-global state, so the tests that
    /// point it at a fixture take turns.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `body` with `HEMLOCK_ZONEINFO_DIR` set to `dir`.
    fn with_zoneinfo<T>(dir: &str, body: impl FnOnce() -> T) -> T {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("HEMLOCK_ZONEINFO_DIR", dir);
        let out = body();
        std::env::remove_var("HEMLOCK_ZONEINFO_DIR");
        out
    }

    /// A miniature zone tree: two regions, one nested zone, the
    /// alternate compilations, and the database metadata.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for zone in [
            "America/Detroit",
            "America/Indiana/Knox",
            "Europe/Berlin",
            "posix/America/Detroit",
            "right/America/Detroit",
        ] {
            let path = root.join(zone);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "TZif").unwrap();
        }
        std::fs::write(root.join("UTC"), "TZif").unwrap();
        std::fs::write(root.join("zone.tab"), "# metadata").unwrap();
        std::fs::write(root.join("leapseconds"), "# metadata").unwrap();
        dir
    }

    #[test]
    fn lists_zones_and_skips_the_alternate_trees() {
        let dir = fixture();
        let names = with_zoneinfo(&dir.path().display().to_string(), names);
        assert_eq!(
            names,
            [
                "America/Detroit",
                "America/Indiana/Knox",
                "Europe/Berlin",
                "UTC",
            ]
        );
    }

    #[test]
    fn existence_follows_the_installed_database() {
        let dir = fixture();
        with_zoneinfo(&dir.path().display().to_string(), || {
            assert!(exists("America/Detroit"));
            assert!(exists("America/Indiana/Knox"));
            assert!(!exists("America/Marquette"));
            // Never escapes the tree, database or not.
            assert!(!exists("../etc/passwd"));
            assert!(!exists("/etc/passwd"));
        });
    }

    /// Without a database every syntactically valid name is accepted:
    /// a development host must not reject the whole world.
    #[test]
    fn no_database_accepts_valid_names() {
        with_zoneinfo("/nonexistent/zoneinfo", || {
            assert!(exists("America/Marquette"));
            assert!(!exists("no spaces/here"));
            assert!(names().is_empty());
        });
    }

    #[test]
    fn name_syntax() {
        assert!(valid_name("UTC"));
        assert!(valid_name("Etc/GMT+5"));
        assert!(valid_name("America/Argentina/Buenos_Aires"));
        assert!(!valid_name(""));
        assert!(!valid_name("America/"));
        assert!(!valid_name("America//Detroit"));
        assert!(!valid_name("America/../etc"));
    }
}
