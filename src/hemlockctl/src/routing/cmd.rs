//! Parsing and dispatch for the routing-suite operational commands:
//! `show ip route [...]` and `show ipv6 route [...]`.

use std::net::IpAddr;

use hemlock_common::ipc::IpcEndpoint;

use crate::cli::{fmt_err, resolve};
use crate::interfaces::cmd::take_json;

use super::model::{RouteEntry, RouteTable};
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

/// `show ip ...` / `show ipv6 ...`: `route` for both families, plus
/// `neighbors` (the v6 ARP twin).
pub async fn show_family(
    endpoints: &crate::cli::Endpoints,
    v6: bool,
    args: &[&str],
) -> Result<(), String> {
    let family = if v6 { "ipv6" } else { "ip" };
    let topics: &[&str] = if v6 {
        &["route", "neighbors"]
    } else {
        &["route"]
    };
    let Some(first) = args.first() else {
        return Err(format!(
            "% Incomplete command: show {family} <{}>",
            topics.join("|")
        ));
    };
    match resolve(first, topics)? {
        "route" => show_route(endpoints, v6, &args[1..]).await,
        "neighbors" => show_neighbors(&endpoints.orch, true, &args[1..]).await,
        _ => unreachable!(),
    }
}

/// `show ip route [summary | <prefix|address>] [| json]` and its
/// `show ipv6 route` twin (`v6` flips the family).
async fn show_route(
    endpoints: &crate::cli::Endpoints,
    v6: bool,
    args: &[&str],
) -> Result<(), String> {
    let family = if v6 { "ipv6" } else { "ip" };
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;

    let table = fetch::route_table(&endpoints.mgmtd, &endpoints.syncd, &endpoints.orch, v6)
        .await
        .map_err(fmt_err)?;
    match words.split_first() {
        None => {
            if json {
                return page_json(&format!("{family}_route"), &table);
            }
            crate::pager::page(&render::route_table(&table));
            Ok(())
        }
        // An address or prefix always carries a separator; anything
        // else resolves against the keyword.
        Some((word, more)) if word.contains('.') || word.contains(':') => {
            no_more(more)?;
            let route = find_route(&table, word)?;
            if json {
                return page_json("route", route);
            }
            crate::pager::page(&render::route_entry(route));
            Ok(())
        }
        Some((word, more)) => {
            resolve(word, &["summary"])?;
            no_more(more)?;
            let summary = table.summarize(fetch::next_hop_groups(&endpoints.syncd).await);
            if json {
                return page_json(&format!("{family}_route_summary"), &summary);
            }
            crate::pager::page(&render::route_summary(&summary));
            Ok(())
        }
    }
}

/// `show arp [| json]` / `show ipv6 neighbors [| json]`.
pub async fn show_neighbors(orch: &IpcEndpoint, v6: bool, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    no_more(&words)?;
    let table = fetch::neighbors(orch, v6).await.map_err(fmt_err)?;
    if json {
        return page_json(if v6 { "ipv6_neighbors" } else { "arp" }, &table);
    }
    crate::pager::page(&render::neighbor_table(&table));
    Ok(())
}

/// `clear arp [<ip>]`: flush dynamic kernel neighbors via orch; the
/// change flows back through the RIB pipeline.
pub async fn clear_arp(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let ip = match args {
        [] => String::new(),
        [ip] => ip
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.to_string())
            .map_err(|_| format!("% bad IP address {ip:?}"))?,
        _ => return Err("% Usage: clear arp [<ip>]".into()),
    };
    let channel = orch
        .connect()
        .await
        .map_err(|e| fmt_err(anyhow::anyhow!(e)))?;
    let flushed = hemlock_common::proto::v1::orch_client::OrchClient::new(channel)
        .clear_neighbors(hemlock_common::proto::v1::ClearNeighborsRequest { ip })
        .await
        .map_err(|e| format!("% {}", e.message()))?
        .into_inner()
        .flushed;
    if !flushed {
        return Err("% arp flush unavailable (no kernel neighbor table on this host)".into());
    }
    Ok(())
}

/// `show routing <ospf|bgp> ...` — live FRR protocol detail via orch.
pub async fn show_routing(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "show routing <ospf [neighbor|interface] | bgp [summary | neighbors <ip>]>";
    let Some(first) = args.first() else {
        return Err(format!("% Incomplete command: {USAGE}"));
    };
    match resolve(first, &["ospf", "bgp"])? {
        "ospf" => show_ospf(orch, &args[1..]).await,
        "bgp" => show_bgp(orch, &args[1..]).await,
        _ => unreachable!(),
    }
}

async fn show_ospf(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let view = match words.split_first() {
        None => "overview",
        Some((word, more)) => {
            no_more(more)?;
            resolve(word, &["neighbor", "interface"])?
        }
    };
    let state = fetch::ospf_state(orch)
        .await
        .map_err(|e| format!("% {e}"))?;
    if json {
        return page_json("ospf", &state);
    }
    let text = match view {
        "neighbor" => render::ospf_neighbors(&state),
        "interface" => render::ospf_interfaces(&state),
        _ => render::ospf_overview(&state),
    };
    crate::pager::page(&text);
    Ok(())
}

async fn show_bgp(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let (view, neighbor) = match words.split_first() {
        None => ("table", String::new()),
        Some((word, more)) => match resolve(word, &["summary", "neighbors"])? {
            "summary" => {
                no_more(more)?;
                ("summary", String::new())
            }
            "neighbors" => {
                let [ip] = more else {
                    return Err("% Usage: show routing bgp neighbors <ip>".into());
                };
                let ip = ip
                    .parse::<std::net::IpAddr>()
                    .map_err(|_| format!("% bad neighbor address {ip:?}"))?;
                ("neighbors", ip.to_string())
            }
            _ => unreachable!(),
        },
    };
    let state = fetch::bgp_state(orch, &neighbor)
        .await
        .map_err(|e| format!("% {e}"))?;
    if json {
        return page_json("bgp", &state);
    }
    let text = match view {
        "summary" => render::bgp_summary(&state),
        "neighbors" => render::bgp_neighbor_detail(&state),
        _ => render::bgp_table(&state),
    };
    crate::pager::page(&text);
    Ok(())
}

/// `show vrrp [brief] [| json]`.
pub async fn show_vrrp(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let brief = match words.split_first() {
        None => false,
        Some((word, more)) => {
            no_more(more)?;
            resolve(word, &["brief"])?;
            true
        }
    };
    let state = fetch::vrrp_state(orch)
        .await
        .map_err(|e| format!("% {e}"))?;
    if json {
        return page_json("vrrp", &state);
    }
    let text = if brief {
        render::vrrp_brief(&state)
    } else {
        render::vrrp_detail(&state)
    };
    crate::pager::page(&text);
    Ok(())
}

/// `clear routing bgp <neighbor|*>` via orch (vtysh).
pub async fn clear_routing(orch: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    const USAGE: &str = "% Usage: clear routing bgp <neighbor|*>";
    let Some(first) = args.first() else {
        return Err(USAGE.into());
    };
    resolve(first, &["bgp"])?;
    let [target] = &args[1..] else {
        return Err(USAGE.into());
    };
    let target = if *target == "*" {
        "*".to_string()
    } else {
        target
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.to_string())
            .map_err(|_| format!("% bad neighbor address {target:?}"))?
    };
    let channel = orch
        .connect()
        .await
        .map_err(|e| fmt_err(anyhow::anyhow!(e)))?;
    let cleared = hemlock_common::proto::v1::orch_client::OrchClient::new(channel)
        .clear_bgp(hemlock_common::proto::v1::ClearBgpRequest { neighbor: target })
        .await
        .map_err(|e| format!("% {}", e.message()))?
        .into_inner()
        .cleared;
    if !cleared {
        return Err("% bgp is not running".into());
    }
    Ok(())
}

/// The entry for a prefix (exact) or an address (longest match).
fn find_route<'t>(table: &'t RouteTable, arg: &str) -> Result<&'t RouteEntry, String> {
    let found = if arg.contains('/') {
        let canonical = hemlock_common::net::require_canonical_prefix(arg)
            .map_err(|e| format!("% {arg}: {e}"))?;
        table.routes.iter().find(|r| r.prefix == canonical)
    } else {
        let addr: IpAddr = arg.parse().map_err(|_| format!("% bad address {arg:?}"))?;
        table
            .routes
            .iter()
            .filter(|r| contains(&r.prefix, addr))
            .max_by_key(|r| prefix_len(&r.prefix))
    };
    found.ok_or_else(|| format!("% no route matches {arg}"))
}

fn contains(prefix: &str, addr: IpAddr) -> bool {
    match hemlock_common::net::parse_cidr(prefix) {
        Ok((net, len)) if net.is_ipv4() == addr.is_ipv4() => {
            hemlock_common::net::network(addr, len) == net
        }
        _ => false,
    }
}

fn prefix_len(prefix: &str) -> u8 {
    hemlock_common::net::parse_cidr(prefix)
        .map(|(_, len)| len)
        .unwrap_or(0)
}
