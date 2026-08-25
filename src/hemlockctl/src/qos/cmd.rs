//! Parsing and dispatch for the QoS-suite operational commands:
//! `show qos maps`, `show qos wred`, `show qos interface <port>`, and
//! `show qos interfaces`.

use hemlock_common::ipc::IpcEndpoint;

use crate::cli::{fmt_err, resolve};
use crate::interfaces::cmd::take_json;
use crate::interfaces::name;

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

const USAGE: &str = "show qos <maps|wred|interface <port>|interfaces>";

/// `show qos <maps|wred|interface <port>|interfaces> [| json]`.
pub async fn show(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some(first) = words.first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    // Deferred by the QoS suite: buffer management and PFC.
    if let Some(message) = crate::cli::qos_deferred(first, /* port = */ false) {
        return Err(message);
    }
    match resolve(first, &["maps", "wred", "interface", "interfaces"])? {
        "maps" => {
            no_more(&words[1..])?;
            let state = fetch::maps(syncd).await.map_err(fmt_err)?;
            if json {
                return page_json("qos_maps", &state);
            }
            crate::pager::page(&render::maps(&state));
            Ok(())
        }
        "wred" => {
            no_more(&words[1..])?;
            let state = fetch::wred(syncd).await.map_err(fmt_err)?;
            if json {
                return page_json("qos_wred", &state);
            }
            crate::pager::page(&render::wred(&state));
            Ok(())
        }
        "interface" => {
            let Some(raw) = words.get(1) else {
                return Err("% Incomplete command: show qos interface <port>".into());
            };
            no_more(&words[2..])?;
            let id = name::parse_one(raw).ok_or_else(|| format!("% No such interface {raw:?}"))?;
            let port = id.full_name();
            let state = fetch::ports(syncd, &port).await.map_err(fmt_err)?;
            if state.ports.is_empty() {
                return Err(format!("% No QoS state for {port}"));
            }
            if json {
                return page_json("qos_interface", &state);
            }
            crate::pager::page(&render::interface(&state));
            Ok(())
        }
        "interfaces" => {
            no_more(&words[1..])?;
            let state = fetch::ports(syncd, "").await.map_err(fmt_err)?;
            if json {
                return page_json("qos_interfaces", &state);
            }
            crate::pager::page(&render::interfaces(&state));
            Ok(())
        }
        _ => unreachable!(),
    }
}
