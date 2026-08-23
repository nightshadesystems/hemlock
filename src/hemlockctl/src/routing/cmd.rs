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

/// `show ip route [summary | <prefix|address>] [| json]` and its
/// `show ipv6 route` twin (`v6` flips the family).
pub async fn show_route(
    mgmtd: &IpcEndpoint,
    syncd: &IpcEndpoint,
    v6: bool,
    args: &[&str],
) -> Result<(), String> {
    let family = if v6 { "ipv6" } else { "ip" };
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;
    let Some(first) = words.first() else {
        return Err(format!(
            "% Incomplete command: show {family} route [summary | <prefix>]"
        ));
    };
    resolve(first, &["route"])?;
    let rest = &words[1..];

    let table = fetch::route_table(mgmtd, syncd, v6)
        .await
        .map_err(fmt_err)?;
    match rest.split_first() {
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
            let summary = table.summarize(0);
            if json {
                return page_json(&format!("{family}_route_summary"), &summary);
            }
            crate::pager::page(&render::route_summary(&summary));
            Ok(())
        }
    }
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
