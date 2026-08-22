//! Kernel-interface sampling for the stats engine.
//!
//! The management port and virtual interfaces (Loopback / Vlan /
//! Port-Channel netdevs) are OS-level, not ASIC-level. State and
//! counters come from sysfs (`/sys/class/net/<dev>/...`, the same data
//! rtnetlink's RTM_GETLINK + rtnl_link_stats64 carries — sysfs is the
//! repo's established access path for netdev state); addresses come from
//! iproute2 (`ip -o -4 addr show`), which every Hemlock image carries.
//!
//! On non-unix development hosts sampling returns nothing and the CLI
//! simply shows no kernel interfaces.

use crate::ifstats::RawCounters;

/// One sampled kernel interface.
#[derive(Debug, Clone)]
pub struct NetdevSample {
    /// Display name (`Management1`, `Vlan10`, ...).
    pub name: String,
    /// "management" | "loopback" | "vlan" | "port-channel"
    pub kind: &'static str,
    pub admin_up: bool,
    pub oper_up: bool,
    pub mac: String,
    pub mtu: u32,
    /// 0 = unknown.
    pub speed_mbps: u64,
    pub duplex: String,
    pub ip_addresses: Vec<String>,
    pub counters: RawCounters,
}

/// Sample the management netdev (per the platform manifest) plus any
/// netdev named in Hemlock display convention (`Loopback0`, `Vlan10`,
/// `Port-Channel1` — created by later orchestration phases).
pub fn sample_all(management: Option<(&str, &str)>) -> Vec<NetdevSample> {
    #[cfg(unix)]
    {
        unix::sample_all(management)
    }
    #[cfg(not(unix))]
    {
        let _ = management;
        Vec::new()
    }
}

/// Classify a kernel netdev named in display convention.
#[cfg_attr(not(unix), allow(dead_code))] // only the unix sampler calls it
fn virtual_kind(name: &str) -> Option<&'static str> {
    for (prefix, kind) in [
        ("Loopback", "loopback"),
        ("Vlan", "vlan"),
        ("Port-Channel", "port-channel"),
    ] {
        if let Some(rest) = name.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return Some(kind);
            }
        }
    }
    None
}

#[cfg(unix)]
mod unix {
    use super::{virtual_kind, NetdevSample};
    use crate::ifstats::RawCounters;

    fn read(dev: &str, attr: &str) -> Option<String> {
        std::fs::read_to_string(format!("/sys/class/net/{dev}/{attr}"))
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn read_u64(dev: &str, attr: &str) -> u64 {
        read(dev, attr).and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    fn sample_one(os_dev: &str, name: String, kind: &'static str) -> Option<NetdevSample> {
        // Existence gate: a missing device yields no sample.
        let flags = read(os_dev, "flags")?;
        let flags = u32::from_str_radix(flags.trim_start_matches("0x"), 16).ok()?;
        let admin_up = flags & 0x1 != 0; // IFF_UP
        let oper_up = read(os_dev, "operstate").as_deref() == Some("up")
            // Loopbacks report operstate "unknown" while carrying traffic.
            || (kind == "loopback" && admin_up);
        // speed reads -1 when the link is down or the driver cannot say.
        let speed_mbps = read(os_dev, "speed")
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|s| *s > 0)
            .map(|s| s as u64)
            .unwrap_or(0);
        let stats = |attr: &str| read_u64(os_dev, &format!("statistics/{attr}"));
        let rx_packets = stats("rx_packets");
        let tx_packets = stats("tx_packets");
        let rx_multicast = stats("multicast");
        Some(NetdevSample {
            name,
            kind,
            admin_up,
            oper_up,
            mac: read(os_dev, "address").unwrap_or_default().to_lowercase(),
            mtu: read_u64(os_dev, "mtu") as u32,
            speed_mbps,
            duplex: read(os_dev, "duplex").unwrap_or_default(),
            ip_addresses: addresses(os_dev),
            counters: RawCounters {
                in_pkts: rx_packets,
                in_octets: stats("rx_bytes"),
                // The kernel has no rx unicast/broadcast split; report
                // unicast as total-minus-multicast and broadcast 0.
                in_ucast_pkts: rx_packets.saturating_sub(rx_multicast),
                in_mcast_pkts: rx_multicast,
                in_discards: stats("rx_dropped"),
                in_errors: stats("rx_errors"),
                in_crc_errors: stats("rx_crc_errors"),
                in_runts: stats("rx_length_errors"),
                in_giants: stats("rx_over_errors"),
                out_pkts: tx_packets,
                out_octets: stats("tx_bytes"),
                out_ucast_pkts: tx_packets,
                out_discards: stats("tx_dropped"),
                out_errors: stats("tx_errors"),
                collisions: stats("collisions"),
                ..RawCounters::default()
            },
        })
    }

    /// IPv4 addresses in CIDR form via iproute2.
    fn addresses(os_dev: &str) -> Vec<String> {
        let Ok(output) = std::process::Command::new("ip")
            .args(["-o", "-4", "addr", "show", "dev", os_dev])
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                fields.by_ref().find(|f| *f == "inet")?;
                fields.next().map(str::to_string)
            })
            .collect()
    }

    pub fn sample_all(management: Option<(&str, &str)>) -> Vec<NetdevSample> {
        let mut samples = Vec::new();
        if let Some((display, os_dev)) = management {
            if let Some(sample) = sample_one(os_dev, display.to_string(), "management") {
                samples.push(sample);
            }
        }
        if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let os_name = entry.file_name().to_string_lossy().into_owned();
                if let Some(kind) = virtual_kind(&os_name) {
                    if let Some(sample) = sample_one(&os_name, os_name.clone(), kind) {
                        samples.push(sample);
                    }
                }
            }
        }
        samples
    }
}

/// Integration test against a real veth pair. Linux + CAP_NET_ADMIN
/// (root) only; opt in with `--features veth-tests`.
#[cfg(all(test, unix, feature = "veth-tests"))]
mod veth_tests {
    use super::sample_all;

    fn ip(args: &[&str]) -> bool {
        std::process::Command::new("ip")
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    struct VethPair;
    impl Drop for VethPair {
        fn drop(&mut self) {
            let _ = ip(&["link", "del", "veth-hemA"]);
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn samples_a_live_veth_pair() {
        let _ = ip(&["link", "del", "veth-hemA"]);
        assert!(
            ip(&[
                "link",
                "add",
                "veth-hemA",
                "type",
                "veth",
                "peer",
                "name",
                "veth-hemB"
            ]),
            "creating a veth pair needs CAP_NET_ADMIN (run as root)"
        );
        let _cleanup = VethPair;
        assert!(ip(&["link", "set", "veth-hemA", "up"]));
        assert!(ip(&["link", "set", "veth-hemB", "up"]));
        assert!(ip(&["addr", "add", "10.99.0.1/24", "dev", "veth-hemA"]));

        // Treat veth-hemA as the management netdev.
        let samples = sample_all(Some(("Management1", "veth-hemA")));
        let mgmt = samples.iter().find(|s| s.name == "Management1").unwrap();
        assert!(mgmt.admin_up);
        assert!(mgmt.oper_up);
        assert_eq!(mgmt.mtu, 1500);
        assert_eq!(mgmt.mac.len(), 17, "colon MAC: {}", mgmt.mac);
        assert!(mgmt.mac.chars().all(|c| c.is_ascii_hexdigit() || c == ':'));
        assert!(
            mgmt.ip_addresses.iter().any(|a| a == "10.99.0.1/24"),
            "addresses: {:?}",
            mgmt.ip_addresses
        );
    }
}

#[cfg(test)]
mod tests {
    use super::virtual_kind;

    #[test]
    fn display_named_netdevs_classify() {
        assert_eq!(virtual_kind("Loopback0"), Some("loopback"));
        assert_eq!(virtual_kind("Vlan10"), Some("vlan"));
        assert_eq!(virtual_kind("Port-Channel1"), Some("port-channel"));
        assert_eq!(virtual_kind("eth0"), None);
        assert_eq!(virtual_kind("Vlan"), None);
        assert_eq!(virtual_kind("VlanX"), None);
        assert_eq!(virtual_kind("lo"), None);
    }
}
