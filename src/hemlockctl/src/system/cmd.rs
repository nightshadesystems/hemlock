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

/// `show system <users> [| json]`.
pub async fn show(mgmtd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "% Usage: show system <users>";
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some((topic, rest)) = words.split_first() else {
        return Err(USAGE.into());
    };
    match resolve(topic, &["users"])? {
        "users" => {
            no_more(rest)?;
            let state = fetch::users_state(mgmtd).await.map_err(fmt_err)?;
            if json {
                return page_json("system_users", &state);
            }
            crate::pager::page(&render::users(&state));
            Ok(())
        }
        _ => unreachable!(),
    }
}
