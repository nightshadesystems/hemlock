//! IP address/prefix parsing shared by the CLI (immediate feedback at
//! the prompt) and mgmtd (authoritative config validation): interface
//! addresses in CIDR form and static-route prefixes. std-only, IPv4
//! and IPv6.

use std::net::IpAddr;

/// Parse `address/prefix-length`. Host bits may be set — an interface
/// address wants them; route prefixes go through
/// [`require_canonical_prefix`].
pub fn parse_cidr(text: &str) -> Result<(IpAddr, u8), String> {
    let Some((addr, len)) = text.split_once('/') else {
        return Err(format!("{text:?} is not <address>/<prefix-length>"));
    };
    let addr: IpAddr = addr
        .parse()
        .map_err(|_| format!("bad IP address {addr:?}"))?;
    let len: u8 = len
        .parse()
        .map_err(|_| format!("bad prefix length {len:?}"))?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    if len > max {
        return Err(format!("prefix length /{len} exceeds /{max}"));
    }
    Ok((addr, len))
}

/// Canonical `network/len` form of a route prefix (host bits cleared):
/// `10.42.10.9/24` -> `10.42.10.0/24`.
pub fn canonical_prefix(prefix: &str) -> Result<String, String> {
    let (addr, len) = parse_cidr(prefix)?;
    Ok(format!("{}/{len}", network(addr, len)))
}

/// A route prefix that must already be canonical: host bits set is an
/// error naming the canonical form, never a silent rewrite.
pub fn require_canonical_prefix(prefix: &str) -> Result<String, String> {
    let canonical = canonical_prefix(prefix)?;
    if canonical != prefix {
        return Err(format!("host bits set; did you mean {canonical}?"));
    }
    Ok(canonical)
}

/// Validate a static-route next hop: a plain address (never a prefix)
/// in the same address family as the route's prefix.
pub fn validate_next_hop(prefix: &str, next_hop: &str) -> Result<(), String> {
    let (addr, _) = parse_cidr(prefix)?;
    if next_hop.contains('/') {
        return Err(format!(
            "next hop {next_hop:?} must be a plain address, not a prefix"
        ));
    }
    let next_hop: IpAddr = next_hop
        .parse()
        .map_err(|_| format!("bad next-hop address {next_hop:?}"))?;
    if addr.is_ipv4() != next_hop.is_ipv4() {
        return Err(format!(
            "next hop {next_hop} does not match the address family of {prefix}"
        ));
    }
    Ok(())
}

/// A MAC address, canonicalized to colon-separated lowercase. Accepts
/// the colon (`00:50:56:be:ef:01`), dashed (`00-50-56-be-ef-01`) and
/// dotted (`0050.56be.ef01`) forms.
pub fn parse_mac(text: &str) -> Result<String, String> {
    let hex: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("bad MAC address {text:?}"));
    }
    let hex = hex.to_ascii_lowercase();
    let octets: Vec<&str> = (0..6).map(|i| &hex[i * 2..i * 2 + 2]).collect();
    Ok(octets.join(":"))
}

/// [`parse_mac`], additionally requiring a unicast address.
pub fn parse_unicast_mac(text: &str) -> Result<String, String> {
    let mac = parse_mac(text)?;
    let first = u8::from_str_radix(&mac[0..2], 16).unwrap_or(0);
    if first & 1 != 0 {
        return Err(format!("{mac} is not a unicast MAC address"));
    }
    Ok(mac)
}

/// A MAC and-mask, canonicalized to colon-separated lowercase. Same
/// accepted forms as [`parse_mac`]; any bit pattern is a valid mask
/// (`ff:ff:ff:00:00:00` matches an OUI).
pub fn parse_mac_mask(text: &str) -> Result<String, String> {
    parse_mac(text).map_err(|_| format!("bad MAC mask {text:?}"))
}

/// An ACL policer rate: bits/sec with an optional `k`/`m`/`g` suffix
/// (`10m` = 10,000,000 bps) or packets/sec with a `pps` suffix
/// (`2000pps`). Returns the numeric rate and whether it is in pps.
pub fn parse_police_rate(text: &str) -> Result<(u64, bool), String> {
    let bad = || format!("bad rate {text:?} (<bps>[k|m|g] or <n>pps)");
    let lower = text.to_ascii_lowercase();
    let (digits, unit) = split_unit(&lower);
    let value: u64 = if digits.is_empty() {
        return Err(bad());
    } else {
        digits.parse().map_err(|_| bad())?
    };
    let (rate, pps) = match unit {
        "" => (value, false),
        "k" => (value.checked_mul(1_000).ok_or_else(bad)?, false),
        "m" => (value.checked_mul(1_000_000).ok_or_else(bad)?, false),
        "g" => (value.checked_mul(1_000_000_000).ok_or_else(bad)?, false),
        "pps" => (value, true),
        _ => return Err(bad()),
    };
    if rate == 0 {
        return Err(bad());
    }
    Ok((rate, pps))
}

/// An ACL policer burst: bytes with an optional `k`/`m`/`g` suffix
/// (`256k` = 256,000 bytes) or packets with a `pkts` suffix. Returns
/// the numeric burst and whether it is in packets.
pub fn parse_police_burst(text: &str) -> Result<(u64, bool), String> {
    let bad = || format!("bad burst {text:?} (<bytes>[k|m|g] or <n>pkts)");
    let lower = text.to_ascii_lowercase();
    let (digits, unit) = split_unit(&lower);
    let value: u64 = if digits.is_empty() {
        return Err(bad());
    } else {
        digits.parse().map_err(|_| bad())?
    };
    let (burst, pkts) = match unit {
        "" => (value, false),
        "k" => (value.checked_mul(1_000).ok_or_else(bad)?, false),
        "m" => (value.checked_mul(1_000_000).ok_or_else(bad)?, false),
        "g" => (value.checked_mul(1_000_000_000).ok_or_else(bad)?, false),
        "pkts" => (value, true),
        _ => return Err(bad()),
    };
    if burst == 0 {
        return Err(bad());
    }
    Ok((burst, pkts))
}

/// A canonical rendering of a rate/burst pair back to the suffixed
/// config/show form (`10000000,false` -> `10m`; `2000,true` -> `2000pps`).
pub fn format_police_rate(rate: u64, pps: bool) -> String {
    if pps {
        return format!("{rate}pps");
    }
    format_scaled(rate)
}

/// The burst analog of [`format_police_rate`] (`pkts` suffix).
pub fn format_police_burst(burst: u64, pkts: bool) -> String {
    if pkts {
        return format!("{burst}pkts");
    }
    format_scaled(burst)
}

fn format_scaled(value: u64) -> String {
    for (factor, suffix) in [(1_000_000_000, "g"), (1_000_000, "m"), (1_000, "k")] {
        if value >= factor && value % factor == 0 {
            return format!("{}{suffix}", value / factor);
        }
    }
    value.to_string()
}

fn split_unit(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (&text[..end], &text[end..])
}

/// A layer-4 port match: a single port (`443`) or an inclusive range
/// (`67-68`, low < high). Canonical form is the input with any
/// degenerate range collapsed — but degenerate/reversed ranges are
/// errors, never rewritten.
pub fn parse_port_match(text: &str) -> Result<(u16, u16), String> {
    let bad = || format!("bad port {text:?} (<0-65535> or <a>-<b>)");
    match text.split_once('-') {
        None => {
            let port: u16 = text.parse().map_err(|_| bad())?;
            Ok((port, port))
        }
        Some((low, high)) => {
            let low: u16 = low.parse().map_err(|_| bad())?;
            let high: u16 = high.parse().map_err(|_| bad())?;
            if low >= high {
                return Err(format!("bad port range {text:?} (low must be < high)"));
            }
            Ok((low, high))
        }
    }
}

/// A storm-control percent level, canonicalized to two decimals
/// (`10` -> `10.00`, `10.5` -> `10.50`); 0.00..=100.00.
pub fn parse_storm_level(text: &str) -> Result<String, String> {
    let bad = || format!("bad level {text:?} (0.00..100.00)");
    let (whole, frac) = match text.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (text, ""),
    };
    if whole.is_empty()
        || whole.len() > 3
        || frac.len() > 2
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return Err(bad());
    }
    let whole_n: u32 = whole.parse().map_err(|_| bad())?;
    let hundredths: u32 = format!("{frac:0<2}").parse().map_err(|_| bad())?;
    if whole_n > 100 || (whole_n == 100 && hundredths > 0) {
        return Err(bad());
    }
    Ok(format!("{whole_n}.{hundredths:02}"))
}

/// The network address of `addr/len` (host bits cleared).
pub fn network(addr: IpAddr, len: u8) -> IpAddr {
    match addr {
        IpAddr::V4(v4) => {
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            IpAddr::V4((u32::from(v4) & mask).into())
        }
        IpAddr::V6(v6) => {
            let mask = if len == 0 {
                0
            } else {
                u128::MAX << (128 - len)
            };
            IpAddr::V6((u128::from(v6) & mask).into())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_interface_addresses() {
        assert!(parse_cidr("10.42.10.9/24").is_ok());
        assert!(parse_cidr("2001:db8::1/64").is_ok());
        assert!(parse_cidr("10.42.10.9").is_err()); // no length
        assert!(parse_cidr("10.42.10.9/33").is_err());
        assert!(parse_cidr("2001:db8::1/129").is_err());
        assert!(parse_cidr("banana/24").is_err());
        assert!(parse_cidr("10.0.0.1/x").is_err());
    }

    #[test]
    fn canonicalizes_route_prefixes() {
        assert_eq!(canonical_prefix("0.0.0.0/0").unwrap(), "0.0.0.0/0");
        assert_eq!(canonical_prefix("10.42.10.9/24").unwrap(), "10.42.10.0/24");
        assert_eq!(canonical_prefix("10.1.2.3/32").unwrap(), "10.1.2.3/32");
        assert_eq!(canonical_prefix("2001:db8::1/64").unwrap(), "2001:db8::/64");
    }

    #[test]
    fn requires_canonical_prefixes() {
        assert_eq!(require_canonical_prefix("0.0.0.0/0").unwrap(), "0.0.0.0/0");
        assert_eq!(
            require_canonical_prefix("2001:db8:99::/48").unwrap(),
            "2001:db8:99::/48"
        );
        // Host bits set name the canonical form instead of rewriting.
        assert_eq!(
            require_canonical_prefix("10.99.1.0/16").unwrap_err(),
            "host bits set; did you mean 10.99.0.0/16?"
        );
        assert!(require_canonical_prefix("banana/8").is_err());
    }

    #[test]
    fn parses_police_rates_and_bursts() {
        assert_eq!(parse_police_rate("10m").unwrap(), (10_000_000, false));
        assert_eq!(parse_police_rate("256k").unwrap(), (256_000, false));
        assert_eq!(parse_police_rate("1g").unwrap(), (1_000_000_000, false));
        assert_eq!(parse_police_rate("2000pps").unwrap(), (2000, true));
        assert_eq!(parse_police_rate("512").unwrap(), (512, false));
        assert!(parse_police_rate("0").is_err());
        assert!(parse_police_rate("10x").is_err());
        assert!(parse_police_rate("pps").is_err());
        assert!(parse_police_rate("").is_err());

        assert_eq!(parse_police_burst("256k").unwrap(), (256_000, false));
        assert_eq!(parse_police_burst("64pkts").unwrap(), (64, true));
        assert!(parse_police_burst("64pps").is_err());

        // Round-trips through the canonical rendering.
        assert_eq!(format_police_rate(10_000_000, false), "10m");
        assert_eq!(format_police_rate(2000, true), "2000pps");
        assert_eq!(format_police_burst(256_000, false), "256k");
        assert_eq!(format_police_burst(64, true), "64pkts");
        assert_eq!(format_police_rate(1500, false), "1500");
    }

    #[test]
    fn parses_port_matches() {
        assert_eq!(parse_port_match("443").unwrap(), (443, 443));
        assert_eq!(parse_port_match("67-68").unwrap(), (67, 68));
        assert!(parse_port_match("68-67").is_err());
        assert!(parse_port_match("67-67").is_err());
        assert!(parse_port_match("http").is_err());
        assert!(parse_port_match("70000").is_err());
    }

    #[test]
    fn parses_mac_masks() {
        assert_eq!(
            parse_mac_mask("FF:FF:FF:00:00:00").unwrap(),
            "ff:ff:ff:00:00:00"
        );
        assert_eq!(
            parse_mac_mask("ffff.ff00.0000").unwrap(),
            "ff:ff:ff:00:00:00"
        );
        assert!(parse_mac_mask("ff:ff").is_err());
    }

    #[test]
    fn validates_next_hops() {
        assert!(validate_next_hop("0.0.0.0/0", "10.42.10.1").is_ok());
        assert!(validate_next_hop("2001:db8:99::/48", "2001:db8:9::1").is_ok());
        // Family mismatch, prefixes, and bad addresses are rejected.
        assert!(validate_next_hop("0.0.0.0/0", "2001:db8::1").is_err());
        assert!(validate_next_hop("::/0", "10.0.0.1").is_err());
        assert!(validate_next_hop("0.0.0.0/0", "10.0.0.1/24").is_err());
        assert!(validate_next_hop("0.0.0.0/0", "gateway").is_err());
    }
}
