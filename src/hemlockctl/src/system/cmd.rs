//! Parsing and dispatch for the system-suite operational commands:
//! `show system users` and its siblings.

use hemlock_common::ipc::IpcEndpoint;

use crate::cli::{fmt_err, resolve};
use crate::interfaces::cmd::take_json;

use super::{fetch, render};

fn no_more(rest: &[&str]) -> Result<(), String> {
    match rest.first() {
        None => Ok(()),
        Some(word) => Err(format!("% Invalid input: {word:?}")),
    }
}

fn page_json<T: serde::Serialize>(label: &str, value: &T) -> Result<(), String> {
    let root = serde_json::json!({ label: value });
    let rendered = serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".into());
    crate::pager::page(&format!("{rendered}\n"));
    Ok(())
}

/// `show logging [<count>] [| json]` — the forwarding config plus the
/// tail of the local journal.
pub async fn show_logging(mgmtd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "% Usage: show logging [<count>]";
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let count = match words.split_first() {
        None => DEFAULT_LOG_LINES,
        Some((word, rest)) => {
            no_more(rest)?;
            // Deferred by this suite: there is no local log buffer to
            // clear, and the journal is systemd's to rotate.
            if resolve(word, &["buffered", "onboard"]).is_ok() {
                return Err("% only the journal-backed log is supported".into());
            }
            match word.parse::<u32>() {
                Ok(count) if (1..=MAX_LOG_LINES).contains(&count) => count,
                _ => return Err(format!("{USAGE}  (1..{MAX_LOG_LINES})")),
            }
        }
    };
    let state = fetch::logging_state(mgmtd, count).await.map_err(fmt_err)?;
    if json {
        return page_json("logging", &state);
    }
    crate::pager::page(&render::logging(&state));
    Ok(())
}

/// Journal lines `show logging` prints without an argument, and the
/// ceiling one request may ask for — mirrored from mgmtd.
const DEFAULT_LOG_LINES: u32 = 50;
const MAX_LOG_LINES: u32 = 5000;

/// `show version [| json]` — software and platform. Never fails: a
/// switch with syncd down still answers, which is when it is asked.
pub async fn show_version(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    no_more(&words)?;
    let state = fetch::version_state(syncd).await;
    if json {
        return page_json("version", &state);
    }
    crate::pager::page(&render::version(&state));
    Ok(())
}

/// `show switch [| json]` — the ASIC summary.
pub async fn show_switch(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    no_more(&words)?;
    let state = fetch::switch_state(syncd).await.map_err(fmt_err)?;
    if json {
        return page_json("switch", &state);
    }
    crate::pager::page(&render::switch(&state));
    Ok(())
}

/// `show environment [| json]` — temperatures, fans and PSUs.
pub async fn show_environment(pmon: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    no_more(&words)?;
    let state = fetch::environment_state(pmon).await.map_err(fmt_err)?;
    if json {
        return page_json("environment", &state);
    }
    crate::pager::page(&render::environment(&state));
    Ok(())
}

/// `show system <users|commits|image> [| json]`.
pub async fn show(mgmtd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "% Usage: show system <users|commits|image>";
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some((topic, rest)) = words.split_first() else {
        return Err(USAGE.into());
    };
    no_more(rest)?;
    match resolve(topic, &["users", "commits", "image"])? {
        "users" => {
            let state = fetch::users_state(mgmtd).await.map_err(fmt_err)?;
            if json {
                return page_json("system_users", &state);
            }
            crate::pager::page(&render::users(&state));
            Ok(())
        }
        "commits" => {
            let state = fetch::commits_state(mgmtd).await.map_err(fmt_err)?;
            if json {
                return page_json("system_commits", &state);
            }
            crate::pager::page(&render::commits(&state));
            Ok(())
        }
        "image" => {
            let state = fetch::image_state(mgmtd).await.map_err(fmt_err)?;
            if json {
                return page_json("system_image", &state);
            }
            crate::pager::page(&render::image(&state));
            Ok(())
        }
        _ => unreachable!(),
    }
}

/// The line `rollback <n>` prints before the confirm flow, naming what
/// it is about to load. Best-effort: an unreachable mgmtd here means
/// the rollback itself is about to fail with a better message.
pub async fn rollback_target(mgmtd: &IpcEndpoint, index: u32) -> Option<String> {
    let state = fetch::commits_state(mgmtd).await.ok()?;
    let commit = state.find(index)?;
    let time = if commit.time > 0 {
        render::stamp_or_dash(commit.time)
    } else {
        "-".into()
    };
    let field = |text: &str| {
        if text.is_empty() {
            "-".to_string()
        } else {
            text.to_string()
        }
    };
    Some(format!(
        "Rolling back to commit {index} ({time}, {}, {})",
        field(&commit.user),
        field(&commit.client)
    ))
}

/// `show interfaces <port> cable-diagnostics [| json]` — replay the
/// last sweep. The interfaces dispatcher hands the port over already
/// canonicalized.
pub async fn show_cable_diagnostics(
    syncd: &IpcEndpoint,
    port: &str,
    json: bool,
) -> Result<(), String> {
    let state = fetch::cable_diagnostics(syncd, port)
        .await
        .map_err(fmt_err)?;
    if json {
        return page_json("cable_diagnostics", &state);
    }
    if !state.has_result {
        return Err(format!("% no cable diagnostics have been run on {port}"));
    }
    crate::pager::page(&render::cable_diagnostics(&state));
    Ok(())
}

/// `request cable-diagnostics <port>` — run the sweep, then render the
/// same table the `show` form replays.
pub async fn request_cable_diagnostics(syncd: &IpcEndpoint, port: &str) -> Result<(), String> {
    let state = fetch::run_cable_diagnostics(syncd, port)
        .await
        .map_err(fmt_err)?;
    crate::pager::page(&render::cable_diagnostics(&state));
    Ok(())
}

/// `request tech-support` — assemble the bundle and say where it
/// landed. mgmtd does the collecting; this only reports.
pub async fn request_tech_support(mgmtd: &IpcEndpoint) -> Result<(), String> {
    println!("collecting tech-support bundle (this takes a moment)...");
    let (path, size) = fetch::tech_support(mgmtd).await.map_err(fmt_err)?;
    println!("tech-support bundle written to {path} ({})", bytes(size));
    Ok(())
}

/// A byte count in the units an operator reads.
fn bytes(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else {
        format!("{} KiB", n.div_ceil(1024).max(1))
    }
}

/// `request certificate regenerate` — replace the web console's
/// self-signed pair and print the new fingerprint.
pub async fn request_certificate_regenerate(mgmtd: &IpcEndpoint) -> Result<(), String> {
    let fingerprint = fetch::regenerate_certificate(mgmtd)
        .await
        .map_err(fmt_err)?;
    println!("web console certificate regenerated");
    println!("SHA-256 fingerprint: {fingerprint}");
    println!("(the console restarts to serve it; existing sessions are unaffected)");
    Ok(())
}

/// `request reboot [onie-rescue]` — the confirmed reboot verb.
pub async fn request_reboot(mgmtd: &IpcEndpoint, onie_rescue: bool) -> Result<(), String> {
    fetch::reboot(mgmtd, onie_rescue).await.map_err(fmt_err)?;
    if onie_rescue {
        println!("ONIE rescue armed; rebooting into ONIE");
    } else {
        println!("rebooting");
    }
    Ok(())
}
