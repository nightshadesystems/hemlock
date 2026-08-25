//! Parsing and dispatch for the services-suite operational commands:
//! `show lldp [neighbors [detail]]` and `clear lldp counters`.

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

/// `show lldp [neighbors [detail]] [| json]`.
pub async fn show_lldp(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;

    // Deferred by this suite: LLDP-MED has no TLVs here at all.
    if let Some(first) = words.first() {
        if resolve(first, &["med"]).is_ok() {
            return Err("% LLDP-MED is not supported".into());
        }
    }
    let (neighbors, detail) = match words.split_first() {
        None => (false, false),
        Some((word, rest)) => {
            resolve(word, &["neighbors"])?;
            match rest.split_first() {
                None => (true, false),
                Some((word, rest)) => {
                    no_more(rest)?;
                    resolve(word, &["detail"])?;
                    (true, true)
                }
            }
        }
    };
    let state = fetch::lldp_state(orch, "").await.map_err(fmt_err)?;
    if json {
        return page_json(if neighbors { "lldp_neighbors" } else { "lldp" }, &state);
    }
    let text = match (neighbors, detail) {
        (false, _) => render::lldp(&state),
        (true, false) => render::lldp_neighbors(&state),
        (true, true) => render::lldp_neighbors_detail(&state),
    };
    crate::pager::page(&text);
    Ok(())
}

/// `clear lldp counters`: zero the per-port frame counters.
pub async fn clear_lldp(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "% Usage: clear lldp counters";
    let Some(first) = args.first() else {
        return Err(USAGE.into());
    };
    resolve(first, &["counters"])?;
    no_more(&args[1..])?;
    let cleared = fetch::clear_lldp_counters(orch).await.map_err(fmt_err)?;
    println!("lldp counters cleared on {cleared} port(s)");
    Ok(())
}
