//! Interface identity: display names, abbreviations, parsing, ordering.
//!
//! Hemlock uses Arista-style display names (`Ethernet1`, `Management1`,
//! `Port-Channel1`, `Vlan10`, `Loopback0`) with the EOS abbreviations
//! (`Et1`, `Ma1`, `Po1`, `Vl10`, `Lo0`) in tabular output. The CLI accepts
//! full names, abbreviations, unique prefixes, and ranges (`Ethernet1-4`,
//! `Et1-4`, `et1,3,5-7`), all case-insensitively.

use std::fmt;

use serde::Serialize;

/// The interface families Hemlock models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Kind {
    // Variant order = tabular sort order, which is alphabetical on the
    // EOS abbreviations: Et, Lo, Ma, Po, Vl.
    Ethernet,
    Loopback,
    Management,
    PortChannel,
    Vlan,
}

impl Kind {
    /// Canonical full-name prefix.
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Ethernet => "Ethernet",
            Kind::Loopback => "Loopback",
            Kind::Management => "Management",
            Kind::PortChannel => "Port-Channel",
            Kind::Vlan => "Vlan",
        }
    }

    /// EOS tabular abbreviation prefix.
    pub fn abbrev(self) -> &'static str {
        match self {
            Kind::Ethernet => "Et",
            Kind::Loopback => "Lo",
            Kind::Management => "Ma",
            Kind::PortChannel => "Po",
            Kind::Vlan => "Vl",
        }
    }

    /// Rank for detail-block output order (`show interfaces`), which EOS
    /// prints as Ethernet, Management, Loopback, Vlan, Port-Channel.
    pub fn detail_rank(self) -> u8 {
        match self {
            Kind::Ethernet => 0,
            Kind::Management => 1,
            Kind::Loopback => 2,
            Kind::Vlan => 3,
            Kind::PortChannel => 4,
        }
    }

    fn all() -> [Kind; 5] {
        [
            Kind::Ethernet,
            Kind::Loopback,
            Kind::Management,
            Kind::PortChannel,
            Kind::Vlan,
        ]
    }
}

/// One interface, e.g. `Ethernet49` or `Vlan10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterfaceId {
    pub kind: Kind,
    pub num: u32,
}

impl InterfaceId {
    pub fn new(kind: Kind, num: u32) -> Self {
        Self { kind, num }
    }

    /// Full display name (`Port-Channel1`), used in detail-block output
    /// and as JSON keys.
    pub fn full_name(&self) -> String {
        format!("{}{}", self.kind.prefix(), self.num)
    }

    /// Abbreviated name (`Po1`), used in tabular output.
    pub fn abbrev(&self) -> String {
        format!("{}{}", self.kind.abbrev(), self.num)
    }
}

impl fmt::Display for InterfaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.kind.prefix(), self.num)
    }
}

/// Serialize as the full display name (JSON keys and values use full
/// interface names by convention).
impl Serialize for InterfaceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.full_name())
    }
}

/// Parse one interface name: full name, abbreviation, or any unambiguous
/// prefix of the family name, case-insensitively (`Ethernet1`, `Et1`,
/// `eth1`, `e1`, `po1`, `port-channel1`). Dashes in the alpha part are
/// ignored so `port-channel1` and `portchannel1` both work.
pub fn parse_one(input: &str) -> Option<InterfaceId> {
    let digits_at = input
        .find(|c: char| c.is_ascii_digit())
        .filter(|&i| i > 0)?;
    let (alpha, digits) = input.split_at(digits_at);
    let num: u32 = digits.parse().ok()?;
    Some(InterfaceId::new(kind_of(alpha)?, num))
}

/// Resolve an alpha prefix (`et`, `po`, `port-channel`) to its family;
/// `None` when unknown or ambiguous.
fn kind_of(alpha: &str) -> Option<Kind> {
    let needle: String = alpha
        .chars()
        .filter(|c| *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    if needle.is_empty() {
        return None;
    }
    let mut hit = None;
    for kind in Kind::all() {
        let full: String = kind
            .prefix()
            .chars()
            .filter(|c| *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        if full.starts_with(&needle) {
            if hit.is_some() {
                return None; // ambiguous
            }
            hit = Some(kind);
        }
    }
    hit
}

/// Parse an interface selector: comma-separated names and ranges
/// (`Ethernet1-4,49`, `Et1-4,Ma1`). A bare number range or trailing
/// number after a comma reuses the previous family (`Et1-4,49` selects
/// Ethernet1-4 and Ethernet49). Returns the expanded, de-duplicated,
/// naturally sorted list, or `None` if any piece fails to parse.
pub fn parse_selector(input: &str) -> Option<Vec<InterfaceId>> {
    let mut out: Vec<InterfaceId> = Vec::new();
    let mut last_kind: Option<Kind> = None;
    for piece in input.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            return None;
        }
        let (kind, span) = if piece.chars().next()?.is_ascii_digit() {
            (last_kind?, piece)
        } else {
            let digits_at = piece.find(|c: char| c.is_ascii_digit())?;
            let (alpha, rest) = piece.split_at(digits_at);
            (kind_of(alpha)?, rest)
        };
        last_kind = Some(kind);
        let (lo, hi) = match span.split_once('-') {
            Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
            None => {
                let n = span.parse().ok()?;
                (n, n)
            }
        };
        if lo > hi || hi - lo > 4096 {
            return None;
        }
        for num in lo..=hi {
            out.push(InterfaceId::new(kind, num));
        }
    }
    out.sort();
    out.dedup();
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn full_names_and_abbreviations_round_trip() {
        let cases = [
            (InterfaceId::new(Kind::Ethernet, 49), "Ethernet49", "Et49"),
            (InterfaceId::new(Kind::Management, 1), "Management1", "Ma1"),
            (
                InterfaceId::new(Kind::PortChannel, 1),
                "Port-Channel1",
                "Po1",
            ),
            (InterfaceId::new(Kind::Vlan, 10), "Vlan10", "Vl10"),
            (InterfaceId::new(Kind::Loopback, 0), "Loopback0", "Lo0"),
        ];
        for (id, full, abbrev) in cases {
            assert_eq!(id.full_name(), full);
            assert_eq!(id.abbrev(), abbrev);
            assert_eq!(parse_one(full), Some(id));
            assert_eq!(parse_one(abbrev), Some(id));
            assert_eq!(parse_one(&full.to_lowercase()), Some(id));
            assert_eq!(parse_one(&abbrev.to_uppercase()), Some(id));
        }
    }

    #[test]
    fn prefixes_parse_case_insensitively() {
        assert_eq!(parse_one("e1"), Some(InterfaceId::new(Kind::Ethernet, 1)),);
        assert_eq!(
            parse_one("eth49"),
            Some(InterfaceId::new(Kind::Ethernet, 49)),
        );
        assert_eq!(
            parse_one("portchannel1"),
            Some(InterfaceId::new(Kind::PortChannel, 1)),
        );
        assert_eq!(
            parse_one("po1"),
            Some(InterfaceId::new(Kind::PortChannel, 1)),
        );
        assert_eq!(parse_one("m1"), Some(InterfaceId::new(Kind::Management, 1)));
        assert_eq!(parse_one("v10"), Some(InterfaceId::new(Kind::Vlan, 10)));
        assert_eq!(parse_one("lo0"), Some(InterfaceId::new(Kind::Loopback, 0)));
        assert_eq!(parse_one("status"), None);
        assert_eq!(parse_one("Ethernet"), None); // no number
        assert_eq!(parse_one("42"), None); // no family
    }

    #[test]
    fn selectors_expand_ranges_and_reuse_the_family() {
        assert_eq!(
            parse_selector("Ethernet1-4").unwrap(),
            (1..=4)
                .map(|n| InterfaceId::new(Kind::Ethernet, n))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parse_selector("et1-2,49,Ma1").unwrap(),
            vec![
                InterfaceId::new(Kind::Ethernet, 1),
                InterfaceId::new(Kind::Ethernet, 2),
                InterfaceId::new(Kind::Ethernet, 49),
                InterfaceId::new(Kind::Management, 1),
            ]
        );
        assert!(parse_selector("Ethernet4-1").is_none());
        assert!(parse_selector("1-4").is_none()); // no family to inherit
        assert!(parse_selector("Et1,,2").is_none());
    }

    #[test]
    fn natural_sort_is_by_family_then_number() {
        let mut ids = [
            InterfaceId::new(Kind::Vlan, 10),
            InterfaceId::new(Kind::Ethernet, 10),
            InterfaceId::new(Kind::Ethernet, 9),
            InterfaceId::new(Kind::Ethernet, 2),
            InterfaceId::new(Kind::Management, 1),
            InterfaceId::new(Kind::PortChannel, 1),
            InterfaceId::new(Kind::Loopback, 0),
            InterfaceId::new(Kind::Ethernet, 1),
        ];
        ids.sort();
        let names: Vec<String> = ids.iter().map(InterfaceId::abbrev).collect();
        assert_eq!(
            names,
            ["Et1", "Et2", "Et9", "Et10", "Lo0", "Ma1", "Po1", "Vl10"]
        );
    }

    #[test]
    fn detail_rank_orders_ethernet_management_loopback_vlan_portchannel() {
        let mut ids = [
            InterfaceId::new(Kind::PortChannel, 1),
            InterfaceId::new(Kind::Vlan, 10),
            InterfaceId::new(Kind::Loopback, 0),
            InterfaceId::new(Kind::Management, 1),
            InterfaceId::new(Kind::Ethernet, 2),
        ];
        ids.sort_by_key(|id| (id.kind.detail_rank(), id.num));
        let names: Vec<String> = ids.iter().map(InterfaceId::full_name).collect();
        assert_eq!(
            names,
            [
                "Ethernet2",
                "Management1",
                "Loopback0",
                "Vlan10",
                "Port-Channel1"
            ]
        );
    }
}
