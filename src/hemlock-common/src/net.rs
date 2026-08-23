//! IP address/prefix parsing shared by the CLI (immediate feedback at
//! the prompt) and mgmtd (authoritative config validation): interface
//! addresses in CIDR form and static-route prefixes. std-only, IPv4
//! and IPv6.

use std::net::IpAddr;

/// Parse `address/prefix-length`. Host bits may be set — an interface
/// address wants them; route prefixes go through [`validate_route`].
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

/// Validate a static route and return its canonical prefix. The next
/// hop must parse and match the prefix's address family.
pub fn validate_route(prefix: &str, next_hop: &str) -> Result<String, String> {
    let (addr, len) = parse_cidr(prefix)?;
    let next_hop: IpAddr = next_hop
        .parse()
        .map_err(|_| format!("bad next-hop address {next_hop:?}"))?;
    if addr.is_ipv4() != next_hop.is_ipv4() {
        return Err(format!(
            "next hop {next_hop} does not match the address family of {prefix}"
        ));
    }
    Ok(format!("{}/{len}", network(addr, len)))
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
    fn validates_routes() {
        assert_eq!(
            validate_route("0.0.0.0/0", "10.42.10.1").unwrap(),
            "0.0.0.0/0"
        );
        // Family mismatch and bad next hops are rejected.
        assert!(validate_route("0.0.0.0/0", "2001:db8::1").is_err());
        assert!(validate_route("::/0", "10.0.0.1").is_err());
        assert!(validate_route("0.0.0.0/0", "gateway").is_err());
    }
}
