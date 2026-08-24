//! Parsing and dispatch for the security-suite operational commands:
//! `show acl`, `show copp`, `show port-security`, `show dot1x`,
//! `show dhcp snooping`, `show arp inspection`, and their `clear`
//! verbs.

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

/// A trailing `interface <port>` scope, resolved to the full display
/// name; None when absent.
fn take_interface(words: &[&str], usage: &str) -> Result<Option<String>, String> {
    match words {
        [] => Ok(None),
        [keyword] => {
            resolve(keyword, &["interface"])?;
            Err(format!("% Incomplete command: {usage}"))
        }
        [keyword, raw] => {
            resolve(keyword, &["interface"])?;
            let id = name::parse_one(raw).ok_or_else(|| format!("% No such interface {raw:?}"))?;
            Ok(Some(id.full_name()))
        }
        [_, _, word, ..] => Err(format!("% Invalid input: {word:?}")),
    }
}

/// `show acl [<name>|summary] [| json]`.
pub async fn show_acl(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let mut state = fetch::acl_state(syncd).await.map_err(fmt_err)?;

    let mut summary = false;
    match words.split_first() {
        None => {}
        Some((word, more)) => {
            no_more(more)?;
            // `summary` wins as a keyword; anything else names a list.
            if resolve(word, &["summary"]).is_ok() {
                summary = true;
            } else {
                let name = *word;
                state.acls.retain(|acl| acl.name == name);
                if state.acls.is_empty() {
                    return Err(format!("% No ACL named {name:?}"));
                }
            }
        }
    }
    if json {
        return page_json(if summary { "acl_summary" } else { "acl" }, &state);
    }
    let text = if summary {
        render::acl_summary(&state)
    } else {
        render::acl(&state)
    };
    crate::pager::page(&text);
    Ok(())
}

/// `show copp [| json]`.
pub async fn show_copp(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    no_more(&words)?;
    let state = fetch::copp_state(syncd).await.map_err(fmt_err)?;
    if json {
        return page_json("copp", &state);
    }
    crate::pager::page(&render::copp(&state));
    Ok(())
}

/// `show port-security [interface <port>] [| json]` (the interface
/// form renders the per-port detail block).
pub async fn show_port_security(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let port = take_interface(&words, "show port-security [interface <port>]")?;
    let rows = fetch::port_security(syncd, port.as_deref().unwrap_or(""))
        .await
        .map_err(fmt_err)?;
    if let Some(port) = &port {
        if rows.is_empty() {
            return Err(format!("% port-security is not enabled on {port}"));
        }
    }
    if json {
        return page_json("port_security", &rows);
    }
    let text = if port.is_some() {
        render::port_security_detail(&rows)
    } else {
        render::port_security(&rows)
    };
    crate::pager::page(&text);
    Ok(())
}

/// `show dot1x [interface <port>] [| json]`.
pub async fn show_dot1x(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let port = take_interface(&words, "show dot1x [interface <port>]")?;
    let state = fetch::dot1x_state(orch, port.as_deref().unwrap_or(""))
        .await
        .map_err(fmt_err)?;
    if let Some(port) = &port {
        if state.ports.is_empty() {
            return Err(format!("% dot1x is not enabled on {port}"));
        }
    }
    if json {
        return page_json("dot1x", &state);
    }
    crate::pager::page(&render::dot1x(&state));
    Ok(())
}

/// `show dhcp snooping [binding|statistics] [| json]` (the dispatcher
/// hands over the words after `dhcp`).
pub async fn show_dhcp(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "show dhcp snooping [binding|statistics]";
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some(first) = words.first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    resolve(first, &["snooping"])?;
    let view = match words.get(1) {
        None => "overview",
        Some(word) => {
            no_more(&words[2..])?;
            resolve(word, &["binding", "statistics"])?
        }
    };
    let state = fetch::snoop_state(orch).await.map_err(fmt_err)?;
    if json {
        return match view {
            "binding" => page_json("dhcp_snooping_binding", &state.dhcp.bindings),
            "statistics" => page_json("dhcp_snooping_statistics", &state.dhcp.statistics),
            _ => page_json("dhcp_snooping", &state.dhcp),
        };
    }
    let text = match view {
        "binding" => render::dhcp_snooping_binding(&state.dhcp.bindings),
        "statistics" => render::dhcp_snooping_statistics(&state.dhcp.statistics),
        _ => render::dhcp_snooping(&state.dhcp),
    };
    crate::pager::page(&text);
    Ok(())
}

/// `show arp inspection [statistics] [| json]`. `show arp` belongs to
/// the routing suite; the dispatcher resolves `inspection` and hands
/// over the words after it.
pub async fn show_arp_inspection(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let statistics = match words.split_first() {
        None => false,
        Some((word, more)) => {
            no_more(more)?;
            resolve(word, &["statistics"])?;
            true
        }
    };
    let state = fetch::snoop_state(orch).await.map_err(fmt_err)?;
    if json {
        return if statistics {
            page_json("arp_inspection_statistics", &state.arp.statistics)
        } else {
            page_json("arp_inspection", &state.arp)
        };
    }
    let text = if statistics {
        render::arp_inspection_statistics(&state.arp.statistics)
    } else {
        render::arp_inspection(&state.arp)
    };
    crate::pager::page(&text);
    Ok(())
}

/// `clear acl counters [<name>]`: zero hardware match counters,
/// optionally scoped to one list.
pub async fn clear_acl_counters(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "% Usage: clear acl counters [<name>]";
    let Some(first) = args.first() else {
        return Err(USAGE.into());
    };
    resolve(first, &["counters"])?;
    let name = match &args[1..] {
        [] => String::new(),
        [name] => (*name).to_string(),
        _ => return Err(USAGE.into()),
    };
    let cleared = fetch::clear_acl_counters(syncd, name)
        .await
        .map_err(fmt_err)?;
    println!("counters cleared on {cleared} acl(s)");
    Ok(())
}

/// `clear copp counters`.
pub async fn clear_copp_counters(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let Some(first) = args.first() else {
        return Err("% Usage: clear copp counters".into());
    };
    resolve(first, &["counters"])?;
    no_more(&args[1..])?;
    fetch::clear_copp_counters(syncd).await.map_err(fmt_err)?;
    println!("copp counters cleared");
    Ok(())
}

/// `clear port-security [interface <port>]`: reset learned MACs and
/// errdisable state (the config stays).
pub async fn clear_port_security(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let port = take_interface(args, "clear port-security [interface <port>]")?;
    let cleared = fetch::reset_port_security(syncd, port.unwrap_or_default())
        .await
        .map_err(fmt_err)?;
    println!("port-security reset on {cleared} port(s)");
    Ok(())
}

/// `clear dhcp snooping binding [<mac>]`: flush dynamic bindings (the
/// dispatcher hands over the words after `dhcp`).
pub async fn clear_dhcp_binding(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "% Usage: clear dhcp snooping binding [<mac>]";
    let Some(first) = args.first() else {
        return Err(USAGE.into());
    };
    resolve(first, &["snooping"])?;
    let Some(second) = args.get(1) else {
        return Err(USAGE.into());
    };
    resolve(second, &["binding"])?;
    let mac = match &args[2..] {
        [] => String::new(),
        [raw] => hemlock_common::net::parse_mac(raw).map_err(|e| format!("% {e}"))?,
        _ => return Err(USAGE.into()),
    };
    let cleared = fetch::clear_snoop_binding(orch, mac)
        .await
        .map_err(fmt_err)?;
    println!("{cleared} dhcp snooping binding(s) cleared");
    Ok(())
}

/// `clear dot1x interface <port>`: force reauthentication via the
/// authenticator.
pub async fn clear_dot1x(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "clear dot1x interface <port>";
    let Some(port) = take_interface(args, USAGE)? else {
        return Err(format!("% Usage: {USAGE}"));
    };
    let triggered = fetch::dot1x_reauth(orch, port.clone())
        .await
        .map_err(fmt_err)?;
    if !triggered {
        return Err(format!("% dot1x is not enabled on {port}"));
    }
    println!("reauthentication triggered on {port}");
    Ok(())
}
