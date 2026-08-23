//! Parsing and dispatch for the switching-suite operational commands:
//! `show vlan`, `show mac address-table`, `show storm-control`,
//! `show mirror` / `show monitor session`, and `clear`.

use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

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

/// `show vlan [id <set> | summary] [| json]`.
pub async fn show_vlan(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let mut rows = fetch::vlans(syncd).await.map_err(fmt_err)?;

    let mut summary = false;
    match words.split_first() {
        None => {}
        Some((first, rest)) => match resolve(first, &["id", "summary"])? {
            "summary" => {
                no_more(rest)?;
                summary = true;
            }
            "id" => {
                let Some(list) = rest.first() else {
                    return Err("% Usage: show vlan id <list>".into());
                };
                let wanted = parse_vlan_set(list)?;
                no_more(&rest[1..])?;
                rows.retain(|row| wanted.contains(&row.id));
            }
            _ => unreachable!(),
        },
    }
    if json {
        return page_json("vlans", &rows);
    }
    let text = if summary {
        render::vlan_summary(&rows)
    } else {
        render::vlan(&rows)
    };
    crate::pager::page(&text);
    Ok(())
}

/// A VLAN id set (`10,20,30-32`), expanded.
fn parse_vlan_set(text: &str) -> Result<Vec<u32>, String> {
    let one = |t: &str| {
        t.parse::<u32>()
            .ok()
            .filter(|id| (1..=4094).contains(id))
            .ok_or_else(|| format!("% bad VLAN id {t:?} (1..4094)"))
    };
    let mut out = Vec::new();
    for part in text.split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((from, to)) => {
                let (from, to) = (one(from)?, one(to)?);
                if from > to {
                    return Err(format!("% bad VLAN range {part:?}"));
                }
                out.extend(from..=to);
            }
            None => out.push(one(part)?),
        }
    }
    if out.is_empty() {
        return Err("% empty VLAN list".into());
    }
    Ok(out)
}

/// `show mac address-table [count|aging-time] [vlan <id>] [interface
/// <port>] [address <mac>] [static|dynamic] [| json]`.
pub async fn show_mac(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str =
        "show mac address-table [count|aging-time] [vlan <id>] [interface <port>] [address <mac>] [static|dynamic]";
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some(first) = words.first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    resolve(first, &["address-table"])?;

    enum View {
        Table,
        Count,
        AgingTime,
    }
    let mut view = View::Table;
    let mut vlan = 0u32;
    let mut port: Option<String> = None;
    let mut mac: Option<String> = None;
    let mut kind = pb::FdbEntryKind::Unspecified;
    let mut rest = &words[1..];
    while let Some(word) = rest.first() {
        match resolve(
            word,
            &[
                "count",
                "aging-time",
                "vlan",
                "interface",
                "address",
                "static",
                "dynamic",
            ],
        )? {
            "count" => view = View::Count,
            "aging-time" => view = View::AgingTime,
            "static" => kind = pb::FdbEntryKind::Static,
            "dynamic" => kind = pb::FdbEntryKind::Dynamic,
            "vlan" => {
                let Some(raw) = rest.get(1) else {
                    return Err(format!("% Incomplete command: {USAGE}"));
                };
                vlan = raw
                    .parse::<u32>()
                    .ok()
                    .filter(|id| (1..=4094).contains(id))
                    .ok_or_else(|| format!("% bad VLAN id {raw:?} (1..4094)"))?;
                rest = &rest[1..];
            }
            "interface" => {
                let Some(raw) = rest.get(1) else {
                    return Err(format!("% Incomplete command: {USAGE}"));
                };
                let id =
                    name::parse_one(raw).ok_or_else(|| format!("% No such interface {raw:?}"))?;
                port = Some(id.full_name());
                rest = &rest[1..];
            }
            "address" => {
                let Some(raw) = rest.get(1) else {
                    return Err(format!("% Incomplete command: {USAGE}"));
                };
                mac = Some(hemlock_common::net::parse_mac(raw).map_err(|e| format!("% {e}"))?);
                rest = &rest[1..];
            }
            _ => unreachable!(),
        }
        rest = &rest[1..];
    }

    let table = fetch::mac_table(syncd, vlan, port.as_deref(), mac.as_deref(), kind)
        .await
        .map_err(fmt_err)?;
    if json {
        return page_json("mac_address_table", &table);
    }
    let text = match view {
        View::Table => render::mac_address_table(&table),
        View::Count => render::mac_count(&table),
        View::AgingTime => render::mac_aging_time(&table),
    };
    crate::pager::page(&text);
    Ok(())
}

/// `show storm-control [| json]`.
pub async fn show_storm_control(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    no_more(&words)?;
    let rows = fetch::storm_control(syncd).await.map_err(fmt_err)?;
    if json {
        return page_json("storm_control", &rows);
    }
    crate::pager::page(&render::storm_control(&rows));
    Ok(())
}

/// `show mirror [| json]` (and `show monitor session`, which routes
/// here after its extra keyword).
pub async fn show_mirror(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    no_more(&words)?;
    let sessions = fetch::mirror(syncd).await.map_err(fmt_err)?;
    if json {
        return page_json("mirror_sessions", &sessions);
    }
    crate::pager::page(&render::mirror(&sessions));
    Ok(())
}

/// `show port-channel [<n>] [summary|detail] [| json]`.
pub async fn show_port_channel(
    syncd: &IpcEndpoint,
    orch: &IpcEndpoint,
    args: &[&str],
) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let mut lags = fetch::port_channels(syncd, orch).await.map_err(fmt_err)?;

    let mut rest: &[&str] = &words;
    if let Some(first) = rest.first() {
        if let Ok(group) = first.parse::<u32>() {
            if !(1..=64).contains(&group) {
                return Err(format!("% bad port-channel number {first:?} (1..64)"));
            }
            lags.retain(|lag| lag.group == group);
            rest = &rest[1..];
        }
    }
    let mut detail = false;
    match rest.first() {
        None => {}
        Some(word) => {
            match resolve(word, &["summary", "detail"])? {
                "detail" => detail = true,
                "summary" => {}
                _ => unreachable!(),
            }
            no_more(&rest[1..])?;
        }
    }
    if json {
        return page_json("port_channels", &lags);
    }
    let text = if detail {
        render::port_channel_detail(&lags)
    } else {
        render::port_channel_summary(&lags)
    };
    crate::pager::page(&text);
    Ok(())
}

/// `show lacp <neighbor [detail]|counters|sys-id> [| json]`.
pub async fn show_lacp(
    syncd: &IpcEndpoint,
    orch: &IpcEndpoint,
    args: &[&str],
) -> Result<(), String> {
    const USAGE: &str = "show lacp <neighbor [detail]|counters|sys-id>";
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some(first) = words.first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    match resolve(first, &["neighbor", "counters", "sys-id"])? {
        "sys-id" => {
            no_more(&words[1..])?;
            let system = fetch::lacp_system(orch).await.map_err(fmt_err)?;
            if json {
                return page_json("lacp", &system);
            }
            crate::pager::page(&render::lacp_sys_id(&system));
            Ok(())
        }
        keyword @ ("neighbor" | "counters") => {
            let mut detail = false;
            match words.get(1) {
                None => {}
                Some(word) if keyword == "neighbor" => {
                    resolve(word, &["detail"])?;
                    detail = true;
                    no_more(&words[2..])?;
                }
                Some(word) => return Err(format!("% Invalid input: {word:?}")),
            }
            let lags = fetch::port_channels(syncd, orch).await.map_err(fmt_err)?;
            if json {
                return page_json("port_channels", &lags);
            }
            let text = match (keyword, detail) {
                ("neighbor", false) => render::lacp_neighbor(&lags),
                ("neighbor", true) => render::lacp_neighbor_detail(&lags),
                _ => render::lacp_counters(&lags),
            };
            crate::pager::page(&text);
            Ok(())
        }
        _ => unreachable!(),
    }
}

/// `show spanning-tree [detail|blockedports|mst configuration] [| json]`.
pub async fn show_spanning_tree(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let bridge = fetch::stp_bridge(orch).await.map_err(fmt_err)?;
    if json {
        return page_json("spanning_tree", &bridge);
    }
    let text = match words.split_first() {
        None => render::spanning_tree(&bridge),
        Some((first, rest)) => match resolve(first, &["detail", "blockedports", "mst"])? {
            "detail" => {
                no_more(rest)?;
                render::spanning_tree_detail(&bridge)
            }
            "blockedports" => {
                no_more(rest)?;
                render::spanning_tree_blockedports(&bridge)
            }
            "mst" => {
                let Some(keyword) = rest.first() else {
                    return Err("% Usage: show spanning-tree mst configuration".into());
                };
                resolve(keyword, &["configuration"])?;
                no_more(&rest[1..])?;
                render::spanning_tree_mst_configuration(&bridge)
            }
            _ => unreachable!(),
        },
    };
    crate::pager::page(&text);
    Ok(())
}

/// `show igmp|mld snooping [groups|querier] [| json]`.
pub async fn show_snooping(orch: &IpcEndpoint, family: &str, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some(first) = words.first() else {
        return Err(format!(
            "% Incomplete command: show {family} snooping [groups|querier]"
        ));
    };
    resolve(first, &["snooping"])?;
    let view = fetch::snooping(orch, family).await.map_err(fmt_err)?;
    if json {
        return page_json("snooping", &view);
    }
    let text = match words.get(1) {
        None => render::snooping(&view),
        Some(word) => match resolve(word, &["groups", "querier"])? {
            "groups" => {
                no_more(&words[2..])?;
                render::snooping_groups(&view)
            }
            "querier" => {
                no_more(&words[2..])?;
                render::snooping_querier(&view)
            }
            _ => unreachable!(),
        },
    };
    crate::pager::page(&text);
    Ok(())
}

/// `clear <counters [<interface>] | mac-table [vlan <id>] [interface <port>]>`.
pub async fn clear(syncd: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "clear <counters [<interface>] | mac-table [vlan <id>] [interface <port>]>";
    let Some(first) = args.first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    match resolve(first, &["counters", "mac-table"])? {
        "counters" => {
            let names = match args.get(1) {
                None => vec![],
                Some(selector) => {
                    let ids = name::parse_selector(selector)
                        .ok_or_else(|| format!("% No such interface {selector:?}"))?;
                    ids.iter().map(name::InterfaceId::full_name).collect()
                }
            };
            no_more(&args[2.min(args.len())..])?;
            let cleared = fetch::clear_counters(syncd, names).await.map_err(fmt_err)?;
            println!("counters cleared on {cleared} interface(s)");
            Ok(())
        }
        "mac-table" => {
            let mut vlan = 0u32;
            let mut port: Option<String> = None;
            let mut rest = &args[1..];
            while let Some(word) = rest.first() {
                match resolve(word, &["vlan", "interface"])? {
                    "vlan" => {
                        let Some(raw) = rest.get(1) else {
                            return Err(format!("% Incomplete command: {USAGE}"));
                        };
                        vlan = raw
                            .parse::<u32>()
                            .ok()
                            .filter(|id| (1..=4094).contains(id))
                            .ok_or_else(|| format!("% bad VLAN id {raw:?} (1..4094)"))?;
                        rest = &rest[1..];
                    }
                    "interface" => {
                        let Some(raw) = rest.get(1) else {
                            return Err(format!("% Incomplete command: {USAGE}"));
                        };
                        let known = fetch::port_names(syncd).await.map_err(fmt_err)?;
                        port = Some(match crate::complete::match_port(raw, &known) {
                            crate::complete::PortMatch::One(name) => name,
                            crate::complete::PortMatch::NoMatch => {
                                return Err(format!("% No such interface {raw:?}"));
                            }
                            crate::complete::PortMatch::Ambiguous(hits) => {
                                return Err(format!(
                                    "% Ambiguous interface {raw:?}: {}",
                                    hits.join(", ")
                                ));
                            }
                        });
                        rest = &rest[1..];
                    }
                    _ => unreachable!(),
                }
                rest = &rest[1..];
            }
            let flushed = fetch::clear_mac_table(syncd, vlan, port)
                .await
                .map_err(fmt_err)?;
            println!("{flushed} dynamic mac address(es) flushed");
            Ok(())
        }
        _ => unreachable!(),
    }
}
