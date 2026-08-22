//! `show interfaces ...` subcommand parsing and dispatch.
//!
//! Grammar (all words accept EOS-style unique prefixes):
//!
//! ```text
//! show interfaces [<name-or-range>] [<subcommand> ...] [| json]
//! ```
//!
//! Interface arguments accept full names, abbreviations, and ranges
//! (`Ethernet1-4`, `Et1-4,49`, `ma1`), case-insensitively. `| json`
//! emits the underlying data model via serde, keyed by full names.

use hemlock_common::ipc::IpcEndpoint;

use super::name::{self, InterfaceId};
use super::render::summary::StatusFilter;
use super::{fetch, render};
use crate::cli::{fmt_err, resolve};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Detail,
    Description,
    Status(StatusFilter),
    Counters,
    CountersErrors,
    CountersDiscards,
    CountersRates,
    CountersQueue,
    CountersBins,
    Transceiver,
    TransceiverDetail,
    TransceiverProperties,
    TransceiverEeprom,
    Capabilities,
    Flowcontrol,
    Negotiation { detail: bool },
    Phy { detail: bool },
    Mac { detail: bool },
    Switchport,
    Trunk,
    Vlans,
}

const SUBCOMMANDS: &[&str] = &[
    "description",
    "status",
    "counters",
    "transceiver",
    "capabilities",
    "flowcontrol",
    "negotiation",
    "phy",
    "mac",
    "switchport",
    "trunk",
    "vlans",
];

fn no_more(rest: &[&str]) -> Result<(), String> {
    match rest.first() {
        None => Ok(()),
        Some(word) => Err(format!("% Invalid input: {word:?}")),
    }
}

/// An optional trailing `detail` word.
fn detail_flag(rest: &[&str]) -> Result<bool, String> {
    match rest.first() {
        None => Ok(false),
        Some(word) => {
            resolve(word, &["detail"])?;
            no_more(&rest[1..])?;
            Ok(true)
        }
    }
}

fn parse(words: &[&str]) -> Result<Cmd, String> {
    let Some(first) = words.first() else {
        return Ok(Cmd::Detail);
    };
    let rest = &words[1..];
    Ok(match resolve(first, SUBCOMMANDS)? {
        "description" => {
            no_more(rest)?;
            Cmd::Description
        }
        "status" => match rest.first() {
            None => Cmd::Status(StatusFilter::All),
            Some(word) => {
                let filter = match resolve(
                    word,
                    &["connected", "notconnect", "errdisabled", "inactive"],
                )? {
                    "connected" => StatusFilter::Connected,
                    "notconnect" => StatusFilter::NotConnect,
                    "errdisabled" => StatusFilter::ErrDisabled,
                    "inactive" => StatusFilter::Inactive,
                    _ => unreachable!(),
                };
                no_more(&rest[1..])?;
                Cmd::Status(filter)
            }
        },
        "counters" => match rest.first() {
            None => Cmd::Counters,
            Some(word) => {
                let cmd = match resolve(word, &["errors", "discards", "rates", "queue", "bins"])? {
                    "errors" => Cmd::CountersErrors,
                    "discards" => Cmd::CountersDiscards,
                    "rates" => Cmd::CountersRates,
                    "queue" => Cmd::CountersQueue,
                    "bins" => Cmd::CountersBins,
                    _ => unreachable!(),
                };
                no_more(&rest[1..])?;
                cmd
            }
        },
        "transceiver" => match rest.first() {
            None => Cmd::Transceiver,
            Some(word) => {
                let cmd = match resolve(word, &["detail", "properties", "eeprom"])? {
                    "detail" => Cmd::TransceiverDetail,
                    "properties" => Cmd::TransceiverProperties,
                    "eeprom" => Cmd::TransceiverEeprom,
                    _ => unreachable!(),
                };
                no_more(&rest[1..])?;
                cmd
            }
        },
        "capabilities" => {
            no_more(rest)?;
            Cmd::Capabilities
        }
        "flowcontrol" => {
            no_more(rest)?;
            Cmd::Flowcontrol
        }
        "negotiation" => Cmd::Negotiation {
            detail: detail_flag(rest)?,
        },
        "phy" => Cmd::Phy {
            detail: detail_flag(rest)?,
        },
        "mac" => Cmd::Mac {
            detail: detail_flag(rest)?,
        },
        "switchport" => {
            no_more(rest)?;
            Cmd::Switchport
        }
        "trunk" => {
            no_more(rest)?;
            Cmd::Trunk
        }
        "vlans" => {
            no_more(rest)?;
            Cmd::Vlans
        }
        _ => unreachable!(),
    })
}

/// Strip a trailing `| json` (or `|json`), EOS pipe style.
fn take_json(words: &mut Vec<&str>) -> Result<bool, String> {
    match words.as_slice() {
        [.., "|"] => Err("% Incomplete command: | json".into()),
        [.., "|", last] => {
            resolve(last, &["json"])?;
            words.truncate(words.len() - 2);
            Ok(true)
        }
        [.., "|json"] => {
            words.truncate(words.len() - 1);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn json_object<T: serde::Serialize>(
    label: &str,
    items: impl Iterator<Item = (InterfaceId, T)>,
) -> String {
    let mut map = serde_json::Map::new();
    for (id, item) in items {
        map.insert(
            id.full_name(),
            serde_json::to_value(item).unwrap_or(serde_json::Value::Null),
        );
    }
    let root = serde_json::json!({ label: map });
    serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".into())
}

/// Execute `show interfaces <args>`.
pub async fn run(syncd: &IpcEndpoint, pmon: &IpcEndpoint, args: &[&str]) -> Result<(), String> {
    let mut words: Vec<&str> = args.to_vec();
    let json = take_json(&mut words)?;

    // A leading interface selector (`Ethernet1-4`, `et1,49`, `ma1`).
    let mut selection: Option<Vec<InterfaceId>> = None;
    if let Some(first) = words.first() {
        if let Some(ids) = name::parse_selector(first) {
            selection = Some(ids);
            words.remove(0);
        }
    }
    let cmd = parse(&words)?;

    if matches!(
        cmd,
        Cmd::Transceiver
            | Cmd::TransceiverDetail
            | Cmd::TransceiverProperties
            | Cmd::TransceiverEeprom
    ) {
        let mut xcvrs = fetch::transceivers(pmon).await.map_err(fmt_err)?;
        if let Some(wanted) = &selection {
            xcvrs.retain(|x| wanted.contains(&x.id));
        }
        if json {
            let mut sorted = xcvrs;
            sorted.sort_by_key(|x| x.id);
            println!(
                "{}",
                json_object("transceivers", sorted.into_iter().map(|x| (x.id, x)))
            );
            return Ok(());
        }
        let text = match cmd {
            Cmd::Transceiver => render::transceiver::summary(&xcvrs),
            Cmd::TransceiverDetail => render::transceiver::detail(&xcvrs),
            Cmd::TransceiverEeprom => render::transceiver::eeprom(&xcvrs),
            Cmd::TransceiverProperties => {
                let fetched = fetch::interfaces(syncd).await.map_err(fmt_err)?;
                render::transceiver::properties(&xcvrs, &fetched.interfaces)
            }
            _ => unreachable!(),
        };
        print!("{text}");
        return Ok(());
    }

    let fetched = fetch::interfaces(syncd).await.map_err(fmt_err)?;
    let ctx = fetched.ctx;
    let interfaces = fetch::select(fetched.interfaces, selection.as_deref())?;

    if json {
        let mut sorted = interfaces;
        sorted.sort_by_key(|i| i.id);
        println!(
            "{}",
            json_object("interfaces", sorted.into_iter().map(|i| (i.id, i)))
        );
        return Ok(());
    }

    let text = match cmd {
        Cmd::Detail => render::detail::render(&interfaces),
        Cmd::Description => render::summary::description(&interfaces),
        Cmd::Status(filter) => render::summary::status(&interfaces, filter),
        Cmd::Counters => render::counters::counters(&interfaces),
        Cmd::CountersErrors => render::counters::errors(&interfaces),
        Cmd::CountersDiscards => render::counters::discards(&interfaces),
        Cmd::CountersRates => render::counters::rates(&interfaces),
        Cmd::CountersQueue => render::counters::queues(&interfaces),
        Cmd::CountersBins => render::counters::bins(&interfaces),
        Cmd::Capabilities => render::phys::capabilities(&interfaces),
        Cmd::Flowcontrol => render::phys::flowcontrol(&interfaces),
        Cmd::Negotiation { detail: false } => render::phys::negotiation(&interfaces),
        Cmd::Negotiation { detail: true } => render::phys::negotiation_detail(&interfaces),
        Cmd::Phy { detail: false } => render::phys::phy(&interfaces),
        Cmd::Phy { detail: true } => render::phys::phy_detail(&interfaces, &ctx),
        Cmd::Mac { detail: false } => render::phys::mac(&interfaces),
        Cmd::Mac { detail: true } => render::phys::mac_detail(&interfaces),
        Cmd::Switchport => render::l2::switchport(&interfaces, &ctx),
        Cmd::Trunk => render::l2::trunk(&interfaces, &ctx),
        Cmd::Vlans => render::l2::vlans(&interfaces, &ctx),
        Cmd::Transceiver
        | Cmd::TransceiverDetail
        | Cmd::TransceiverProperties
        | Cmd::TransceiverEeprom => unreachable!("handled above"),
    };
    print!("{text}");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn subcommands_parse_with_prefixes() {
        assert_eq!(parse(&[]).unwrap(), Cmd::Detail);
        assert_eq!(parse(&["desc"]).unwrap(), Cmd::Description);
        assert_eq!(parse(&["st"]).unwrap(), Cmd::Status(StatusFilter::All));
        assert_eq!(
            parse(&["status", "err"]).unwrap(),
            Cmd::Status(StatusFilter::ErrDisabled)
        );
        assert_eq!(parse(&["co"]).unwrap(), Cmd::Counters);
        assert_eq!(parse(&["counters", "e"]).unwrap(), Cmd::CountersErrors);
        assert_eq!(parse(&["counters", "b"]).unwrap(), Cmd::CountersBins);
        assert_eq!(parse(&["counters", "q"]).unwrap(), Cmd::CountersQueue);
        assert_eq!(parse(&["counters", "r"]).unwrap(), Cmd::CountersRates);
        assert_eq!(parse(&["counters", "d"]).unwrap(), Cmd::CountersDiscards);
        assert!(parse(&["t"]).is_err()); // transceiver vs trunk
        assert_eq!(parse(&["tra"]).unwrap(), Cmd::Transceiver);
        assert_eq!(
            parse(&["transceiver", "e"]).unwrap(),
            Cmd::TransceiverEeprom
        );
        assert_eq!(
            parse(&["neg", "det"]).unwrap(),
            Cmd::Negotiation { detail: true }
        );
        assert_eq!(parse(&["phy"]).unwrap(), Cmd::Phy { detail: false });
        assert_eq!(
            parse(&["mac", "detail"]).unwrap(),
            Cmd::Mac { detail: true }
        );
        assert_eq!(parse(&["sw"]).unwrap(), Cmd::Switchport);
        assert!(parse(&["tr"]).is_err()); // trunk vs transceiver
        assert_eq!(parse(&["tru"]).unwrap(), Cmd::Trunk);
        assert_eq!(parse(&["v"]).unwrap(), Cmd::Vlans);
        assert!(parse(&["status", "bogus"]).is_err());
        assert!(parse(&["description", "extra"]).is_err());
    }

    #[test]
    fn json_pipe_forms() {
        let mut words = vec!["counters", "|", "json"];
        assert!(take_json(&mut words).unwrap());
        assert_eq!(words, vec!["counters"]);

        let mut words = vec!["status", "|json"];
        assert!(take_json(&mut words).unwrap());
        assert_eq!(words, vec!["status"]);

        let mut words = vec!["status"];
        assert!(!take_json(&mut words).unwrap());

        let mut words = vec!["status", "|"];
        assert!(take_json(&mut words).is_err());

        let mut words = vec!["|", "json"];
        assert!(take_json(&mut words).unwrap());
        assert!(words.is_empty());
    }
}
