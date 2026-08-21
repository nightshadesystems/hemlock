//! `hemlockctl motd` — the live status block for the dynamic MOTD.
//!
//! Runs on every login via `/etc/update-motd.d/10-hemlock-status`, so two
//! rules trump everything: never exit nonzero, never slow a login down.
//! Every data source is optional — a dead daemon or a missing platform
//! directory drops its fields instead of printing an error — and every
//! daemon call is capped by [`DAEMON_TIMEOUT`]. Rendering is a pure
//! function over [`Motd`] so the layout is unit-testable byte-for-byte.

use std::path::Path;
use std::time::Duration;

use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;
use hemlock_platform::Platform;

/// Per-daemon budget. Both daemons are queried concurrently, so the whole
/// block stays well inside the ~200ms login-latency budget even when both
/// endpoints are unreachable.
const DAEMON_TIMEOUT: Duration = Duration::from_millis(75);

/// Everything the MOTD can show; `None` fields are omitted from output.
#[derive(Debug, Default)]
struct Motd {
    version: String,
    codename: Option<String>,
    hostname: Option<String>,
    /// Pre-formatted, e.g. "Celestica E1031 (Haliburton) (BCM Helix4)".
    platform: Option<String>,
    /// "14d 3h 22m"
    uptime: Option<String>,
    /// 1-minute load average, verbatim from /proc/loadavg.
    load: Option<String>,
    /// "2.1G/16G" (used/total).
    mem: Option<String>,
    /// "46/54" (oper-up / front-panel total).
    ports: Option<String>,
    /// "41C" (hottest sensor).
    temp: Option<String>,
    /// "2/2 OK" (present-and-ok / total).
    psu: Option<String>,
}

pub async fn run(syncd: IpcEndpoint, pmon: IpcEndpoint, platform_dir: &str) -> anyhow::Result<()> {
    let (ports, (temp, psu)) = tokio::join!(port_summary(syncd), environment_summary(pmon));
    let motd = Motd {
        version: hemlock_common::VERSION.to_string(),
        codename: os_release_codename(Path::new("/etc/os-release")),
        hostname: hostname(),
        platform: platform_summary(Path::new(platform_dir)),
        uptime: uptime(),
        load: load_average(),
        mem: memory(),
        ports,
        temp,
        psu,
    };
    print!("{}", render(&motd));
    Ok(())
}

// --- rendering -------------------------------------------------------------

/// Label column ("Platform" is the widest label).
const LABEL_WIDTH: usize = 8;
/// Inline column widths, chosen so "14d 3h 22m"/"46/54 up" and
/// "Load: 0.31"/"Temp: 41C" line up across the two data rows.
const CELL_WIDTHS: [usize; 3] = [13, 14, 0];

fn render(m: &Motd) -> String {
    let mut out = format!("Hemlock NOS v{}", m.version);
    if let Some(codename) = &m.codename {
        out.push_str(&format!(" ({codename})"));
    }
    if let Some(hostname) = &m.hostname {
        out.push_str(&format!(" | {hostname}"));
    }
    out.push('\n');
    if let Some(platform) = &m.platform {
        out.push_str(&format!("{:<LABEL_WIDTH$} : {platform}\n", "Platform"));
    }
    push_row(
        &mut out,
        "Uptime",
        [
            m.uptime.clone(),
            m.load.as_ref().map(|v| format!("Load: {v}")),
            m.mem.as_ref().map(|v| format!("Mem: {v}")),
        ],
    );
    push_row(
        &mut out,
        "Ports",
        [
            m.ports.as_ref().map(|v| format!("{v} up")),
            m.temp.as_ref().map(|v| format!("Temp: {v}")),
            m.psu.as_ref().map(|v| format!("PSU: {v}")),
        ],
    );
    out
}

/// One aligned data row. Missing cells keep their column width so the
/// surviving cells stay vertically aligned; a fully empty row is omitted.
fn push_row(out: &mut String, label: &str, cells: [Option<String>; 3]) {
    if cells.iter().all(Option::is_none) {
        return;
    }
    let mut row = format!("{label:<LABEL_WIDTH$} : ");
    for (cell, width) in cells.iter().zip(CELL_WIDTHS) {
        let text = cell.as_deref().unwrap_or("");
        row.push_str(&format!("{text:<width$}"));
    }
    out.push_str(row.trim_end());
    out.push('\n');
}

// --- local data sources ----------------------------------------------------

fn os_release_codename(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("VERSION_CODENAME="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

fn hostname() -> Option<String> {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

fn platform_summary(dir: &Path) -> Option<String> {
    let platform = Platform::load(dir).ok()?;
    let id = &platform.manifest.platform;
    Some(format!(
        "{} {} ({})",
        id.vendor,
        id.model,
        asic_label(&id.asic_family, &id.asic)
    ))
}

/// "broadcom-xgs"/"helix4" → "BCM Helix4"; unknown families pass the
/// prettified ASIC name through unprefixed.
fn asic_label(family: &str, asic: &str) -> String {
    let pretty = asic
        .split('-')
        .map(capitalize)
        .collect::<Vec<_>>()
        .join("-");
    if family.starts_with("broadcom") {
        format!("BCM {pretty}")
    } else {
        pretty
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn uptime() -> Option<String> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    let mins = (secs / 60.0) as u64;
    Some(format!(
        "{}d {}h {}m",
        mins / 1440,
        (mins % 1440) / 60,
        mins % 60
    ))
}

fn load_average() -> Option<String> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    text.split_whitespace().next().map(str::to_string)
}

fn memory() -> Option<String> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    Some(format!(
        "{}/{}",
        fmt_gib(total.saturating_sub(available)),
        fmt_gib(total)
    ))
}

/// KiB → "2.1G" / "16G" (one decimal, trailing .0 dropped).
fn fmt_gib(kib: u64) -> String {
    let gib = format!("{:.1}", kib as f64 / (1024.0 * 1024.0));
    format!("{}G", gib.strip_suffix(".0").unwrap_or(&gib))
}

// --- daemon data sources ---------------------------------------------------

async fn port_summary(endpoint: IpcEndpoint) -> Option<String> {
    let fetch = async {
        let channel = endpoint.connect().await.ok()?;
        let mut client = pb::syncd_client::SyncdClient::new(channel);
        let ports = client
            .list_ports(pb::ListPortsRequest {})
            .await
            .ok()?
            .into_inner()
            .ports;
        if ports.is_empty() {
            return None;
        }
        let up = ports
            .iter()
            .filter(|p| p.oper_status == pb::OperStatus::Up as i32)
            .count();
        Some(format!("{up}/{}", ports.len()))
    };
    tokio::time::timeout(DAEMON_TIMEOUT, fetch).await.ok()?
}

async fn environment_summary(endpoint: IpcEndpoint) -> (Option<String>, Option<String>) {
    let fetch = async {
        let channel = endpoint.connect().await.ok()?;
        let mut client = pb::pmon_client::PmonClient::new(channel);
        client
            .get_environment(pb::GetEnvironmentRequest {})
            .await
            .ok()
            .map(tonic::Response::into_inner)
    };
    let Some(env) = tokio::time::timeout(DAEMON_TIMEOUT, fetch)
        .await
        .ok()
        .flatten()
    else {
        return (None, None);
    };
    let temp = env
        .temperatures
        .iter()
        .map(|t| t.celsius)
        .fold(f64::NAN, f64::max);
    let temp = temp.is_finite().then(|| format!("{temp:.0}C"));
    let psu = (!env.psus.is_empty()).then(|| {
        let ok = env.psus.iter().filter(|p| p.present && p.ok).count();
        format!("{ok}/{} OK", env.psus.len())
    });
    (temp, psu)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The worked example from the MOTD spec.
    fn full() -> Motd {
        Motd {
            version: "1.0.0".into(),
            codename: Some("bookworm".into()),
            hostname: Some("qs-hq-leaf3".into()),
            platform: Some("Celestica Questone 2A (BCM Trident3-X5)".into()),
            uptime: Some("14d 3h 22m".into()),
            load: Some("0.31".into()),
            mem: Some("2.1G/16G".into()),
            ports: Some("46/54".into()),
            temp: Some("41C".into()),
            psu: Some("2/2 OK".into()),
        }
    }

    #[test]
    fn renders_spec_example_byte_for_byte() {
        let expected = "\
Hemlock NOS v1.0.0 (bookworm) | qs-hq-leaf3
Platform : Celestica Questone 2A (BCM Trident3-X5)
Uptime   : 14d 3h 22m   Load: 0.31    Mem: 2.1G/16G
Ports    : 46/54 up     Temp: 41C     PSU: 2/2 OK
";
        assert_eq!(render(&full()), expected);
    }

    #[test]
    fn everything_missing_still_prints_the_version_line() {
        let m = Motd {
            version: "1.0.0".into(),
            ..Motd::default()
        };
        assert_eq!(render(&m), "Hemlock NOS v1.0.0\n");
    }

    #[test]
    fn missing_cells_keep_surviving_columns_aligned() {
        // pmon down: Temp/PSU gone, Ports keeps its column; syncd down on
        // the row above would similarly leave Load/Mem in place.
        let m = Motd {
            temp: None,
            psu: None,
            ..full()
        };
        let out = render(&m);
        assert!(out.contains("Ports    : 46/54 up\n"));

        let m = Motd {
            ports: None,
            ..full()
        };
        let out = render(&m);
        // Temp stays in the same column as Load on the row above.
        assert!(out.contains("Uptime   : 14d 3h 22m   Load: 0.31    Mem: 2.1G/16G\n"));
        assert!(out.contains("Ports    :              Temp: 41C     PSU: 2/2 OK\n"));
    }

    #[test]
    fn rows_with_no_data_are_omitted() {
        let m = Motd {
            ports: None,
            temp: None,
            psu: None,
            ..full()
        };
        let out = render(&m);
        assert!(!out.contains("Ports"));
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn version_line_degrades_without_codename_or_hostname() {
        let m = Motd {
            codename: None,
            ..full()
        };
        assert!(render(&m).starts_with("Hemlock NOS v1.0.0 | qs-hq-leaf3\n"));
        let m = Motd {
            hostname: None,
            ..full()
        };
        assert!(render(&m).starts_with("Hemlock NOS v1.0.0 (bookworm)\n"));
    }

    #[test]
    fn fmt_gib_drops_trailing_zero() {
        assert_eq!(fmt_gib(16 * 1024 * 1024), "16G");
        assert_eq!(fmt_gib(2_202_009), "2.1G"); // 2.1 GiB in KiB
        assert_eq!(fmt_gib(0), "0G");
    }

    #[test]
    fn asic_labels() {
        assert_eq!(asic_label("broadcom-xgs", "helix4"), "BCM Helix4");
        assert_eq!(asic_label("broadcom-xgs", "trident3-x5"), "BCM Trident3-X5");
        assert_eq!(asic_label("marvell", "prestera"), "Prestera");
    }

    #[test]
    fn codename_parses_quoted_and_bare_values() {
        let dir = tempfile_dir();
        let path = dir.path().join("os-release");
        std::fs::write(&path, "ID=hemlock\nVERSION_CODENAME=trixie\n").unwrap();
        assert_eq!(os_release_codename(&path).as_deref(), Some("trixie"));
        std::fs::write(&path, "VERSION_CODENAME=\"trixie\"\n").unwrap();
        assert_eq!(os_release_codename(&path).as_deref(), Some("trixie"));
        std::fs::write(&path, "ID=hemlock\n").unwrap();
        assert_eq!(os_release_codename(&path), None);
        assert_eq!(os_release_codename(&dir.path().join("missing")), None);
    }

    #[test]
    fn platform_summary_reads_the_e1031_manifest() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../platforms/cel-e1031");
        assert_eq!(
            platform_summary(&dir).as_deref(),
            Some("Celestica E1031 (Haliburton) (BCM Helix4)")
        );
        assert_eq!(platform_summary(Path::new("/nonexistent")), None);
    }

    fn tempfile_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
}
