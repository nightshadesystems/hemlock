//! `hemlockctl platform ...` subcommands.

use anyhow::{bail, Context, Result};
use hemlock_platform::{lint::Severity, Platform};

pub fn lint(platforms_dir: &str, id_or_path: &str) -> Result<()> {
    let platform = Platform::find(platforms_dir, id_or_path)
        .with_context(|| format!("loading platform {id_or_path:?}"))?;

    let report = hemlock_platform::lint::lint(&platform);
    for diag in &report.diagnostics {
        println!("{diag}");
    }

    let errors = report
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = report.diagnostics.len() - errors;

    if report.passed() {
        println!(
            "{}: ok ({} ports, {} warning{})",
            platform.manifest.platform.id,
            platform.ports.len(),
            warnings,
            if warnings == 1 { "" } else { "s" }
        );
        Ok(())
    } else {
        bail!(
            "{}: {errors} error{} found",
            platform.manifest.platform.id,
            if errors == 1 { "" } else { "s" }
        );
    }
}
