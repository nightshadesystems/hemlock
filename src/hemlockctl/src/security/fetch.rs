//! Live state for the security-suite shows: dataplane state (ACLs,
//! CoPP, port security) from syncd, protocol state (802.1X, DHCP
//! snooping, ARP inspection) from orch.

use anyhow::{Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

use super::model::{
    AclBinding, AclRule, AclState, AclTable, ArpInspection, CoppClass, CoppState, DaiVlanStats,
    DhcpSnooping, DhcpStatistics, Dot1xPort, Dot1xState, PortSecurityEntry, SecureMac,
    SnoopBinding, SnoopState, SnoopVlanStats, TcamStage,
};

async fn syncd_client(
    syncd: &IpcEndpoint,
) -> Result<pb::syncd_client::SyncdClient<tonic::transport::Channel>> {
    let channel = syncd.connect().await.context("connecting to syncd")?;
    Ok(pb::syncd_client::SyncdClient::new(channel))
}

async fn orch_client(
    orch: &IpcEndpoint,
) -> Result<pb::orch_client::OrchClient<tonic::transport::Channel>> {
    let channel = orch.connect().await.context("connecting to orch")?;
    Ok(pb::orch_client::OrchClient::new(channel))
}

// ------------------------------------------------- ACLs

/// The ACL state: every list with its counters and bindings, plus TCAM
/// utilization.
pub async fn acl_state(syncd: &IpcEndpoint) -> Result<AclState> {
    let response = syncd_client(syncd)
        .await?
        .get_acl_state(pb::GetAclStateRequest {})
        .await?
        .into_inner();
    Ok(AclState {
        acls: response.acls.into_iter().map(acl_table).collect(),
        tcam: response
            .tcam
            .into_iter()
            .map(|stage| TcamStage {
                stage: stage_word(stage.stage).to_string(),
                used: stage.used,
                available: stage.available,
            })
            .collect(),
    })
}

fn stage_word(stage: i32) -> &'static str {
    if stage == pb::AclStage::Egress as i32 {
        "egress"
    } else {
        "ingress"
    }
}

fn direction_word(stage: i32) -> &'static str {
    if stage == pb::AclStage::Egress as i32 {
        "out"
    } else {
        "in"
    }
}

fn acl_table(acl: pb::AclStateEntry) -> AclTable {
    let family = match acl.family {
        f if f == pb::AclFamily::Ipv6 as i32 => "ipv6",
        f if f == pb::AclFamily::Mac as i32 => "mac",
        _ => "ipv4",
    };
    AclTable {
        rules: acl
            .rules
            .iter()
            .enumerate()
            .map(|(i, rule)| acl_rule(family, rule, acl.matches.get(i).copied().unwrap_or(0)))
            .collect(),
        name: acl.name,
        family: family.to_string(),
        implicit_deny_matches: acl.implicit_deny_matches,
        bindings: acl
            .bindings
            .iter()
            .map(|binding| AclBinding {
                port: binding.port.clone(),
                direction: direction_word(binding.stage).to_string(),
            })
            .collect(),
    }
}

/// One rule, pre-rendered to its EOS words: protocol names, "any"
/// wildcards, joined port ranges, scaled policer rates.
fn acl_rule(family: &str, rule: &pb::AclRule, matches: u64) -> AclRule {
    let any = |text: &str| {
        if text.is_empty() {
            "any".to_string()
        } else {
            text.to_string()
        }
    };
    let (protocol, source, destination) = if family == "mac" {
        (
            None,
            mac_match(&rule.source_mac, &rule.source_mac_mask),
            mac_match(&rule.destination_mac, &rule.destination_mac_mask),
        )
    } else {
        let protocol = match rule.protocol {
            Some(6) => "tcp".to_string(),
            Some(17) => "udp".to_string(),
            Some(other) => other.to_string(),
            None => if family == "ipv6" { "ipv6" } else { "ip" }.to_string(),
        };
        (Some(protocol), any(&rule.source), any(&rule.destination))
    };
    AclRule {
        number: rule.number,
        permit: rule.permit,
        protocol,
        source,
        destination,
        port: rule.destination_port_low.map(|low| {
            let high = rule.destination_port_high.unwrap_or(low);
            if high == low {
                low.to_string()
            } else {
                format!("{low}-{high}")
            }
        }),
        log: rule.log,
        police: rule.police_rate.map(|rate| {
            format!(
                "{} {}",
                hemlock_common::net::format_police_rate(rate, rule.police_pps),
                hemlock_common::net::format_police_burst(
                    rule.police_burst.unwrap_or(0),
                    rule.police_pps
                )
            )
        }),
        matches,
    }
}

/// A MAC match as rendered: `mac/mask`, bare `mac` for an exact match,
/// `any` when unconstrained.
fn mac_match(mac: &str, mask: &str) -> String {
    if mac.is_empty() {
        return "any".to_string();
    }
    if mask.is_empty() {
        return mac.to_string();
    }
    format!("{mac}/{mask}")
}

/// `clear acl counters [<name>]`: zero the hardware match counters.
/// Returns the number of ACLs touched.
pub async fn clear_acl_counters(syncd: &IpcEndpoint, name: String) -> Result<u32> {
    Ok(syncd_client(syncd)
        .await?
        .clear_acl_counters(pb::ClearAclCountersRequest { name })
        .await?
        .into_inner()
        .cleared)
}

// ------------------------------------------------- Control-plane policing

/// The CoPP class table.
pub async fn copp_state(syncd: &IpcEndpoint) -> Result<CoppState> {
    let response = syncd_client(syncd)
        .await?
        .get_copp_state(pb::GetCoppStateRequest {})
        .await?
        .into_inner();
    Ok(CoppState {
        classes: response
            .classes
            .into_iter()
            .map(|class| CoppClass {
                class: class.class,
                rate: class.rate,
                burst: class.burst,
                overridden: class.overridden,
                conforming: class.conforming,
                dropped: class.dropped,
            })
            .collect(),
    })
}

/// `clear copp counters`.
pub async fn clear_copp_counters(syncd: &IpcEndpoint) -> Result<()> {
    syncd_client(syncd)
        .await?
        .clear_copp_counters(pb::ClearCoppCountersRequest {})
        .await?;
    Ok(())
}

// ------------------------------------------------- Port security

/// The port-security table, optionally scoped to one port ("" = every
/// enabled port).
pub async fn port_security(syncd: &IpcEndpoint, port: &str) -> Result<Vec<PortSecurityEntry>> {
    let response = syncd_client(syncd)
        .await?
        .get_port_security_state(pb::GetPortSecurityStateRequest {
            port: port.to_string(),
        })
        .await?
        .into_inner();
    Ok(response
        .ports
        .into_iter()
        .map(|entry| PortSecurityEntry {
            port: entry.port,
            maximum: entry.maximum,
            shutdown: entry.shutdown,
            learned: entry
                .learned
                .into_iter()
                .map(|mac| SecureMac {
                    mac: mac.mac,
                    age_secs: mac.age_secs,
                })
                .collect(),
            violations: entry.violations,
            last_violation_mac: (!entry.last_violation_mac.is_empty())
                .then_some(entry.last_violation_mac),
            last_violation_secs_ago: entry.last_violation_secs_ago,
            errdisabled: entry.errdisabled,
        })
        .collect())
}

/// `clear port-security [interface <port>]`: reset learned MACs and
/// errdisable state ("" = every enabled port). Returns the number of
/// ports reset.
pub async fn reset_port_security(syncd: &IpcEndpoint, port: String) -> Result<u32> {
    Ok(syncd_client(syncd)
        .await?
        .reset_port_security(pb::ResetPortSecurityRequest { port })
        .await?
        .into_inner()
        .cleared)
}

// ------------------------------------------------- 802.1X

/// The 802.1X authenticator state, optionally scoped to one port
/// ("" = every dot1x port).
pub async fn dot1x_state(orch: &IpcEndpoint, port: &str) -> Result<Dot1xState> {
    let response = orch_client(orch)
        .await?
        .get_dot1x_state(pb::GetDot1xStateRequest {
            port: port.to_string(),
        })
        .await?
        .into_inner();
    Ok(Dot1xState {
        radius_servers: response.radius_servers,
        reauth_interval_secs: response.reauth_interval,
        ports: response
            .ports
            .into_iter()
            .map(|entry| Dot1xPort {
                port: entry.port,
                status: entry.status,
                supplicant_mac: (!entry.supplicant_mac.is_empty()).then_some(entry.supplicant_mac),
                last_auth_secs_ago: entry.last_auth_secs_ago,
                failures: entry.failures,
            })
            .collect(),
    })
}

/// `clear dot1x interface <port>`: force reauthentication. False when
/// the port is not running the authenticator.
pub async fn dot1x_reauth(orch: &IpcEndpoint, port: String) -> Result<bool> {
    Ok(orch_client(orch)
        .await?
        .dot1x_reauth(pb::Dot1xReauthRequest { port })
        .await?
        .into_inner()
        .triggered)
}

// ------------------------------------------------- DHCP snooping + DAI

/// The snooping-security state (DHCP snooping and ARP inspection both).
pub async fn snoop_state(orch: &IpcEndpoint) -> Result<SnoopState> {
    let state = orch_client(orch)
        .await?
        .get_snoop_sec_state(pb::GetSnoopSecStateRequest {})
        .await?
        .into_inner();
    Ok(SnoopState {
        dhcp: DhcpSnooping {
            vlans: state.dhcp_vlans,
            trusted: state.dhcp_trusted,
            bindings: state
                .bindings
                .into_iter()
                .map(|binding| SnoopBinding {
                    mac: binding.mac,
                    ip: binding.address,
                    lease_secs: binding.lease_secs,
                    is_static: binding.is_static,
                    vlan: binding.vlan,
                    interface: binding.interface,
                })
                .collect(),
            statistics: DhcpStatistics {
                vlans: state
                    .dhcp_stats
                    .into_iter()
                    .map(|stats| SnoopVlanStats {
                        vlan: stats.vlan,
                        packets: stats.packets,
                        dropped: stats.dropped,
                    })
                    .collect(),
                untrusted_server_drops: state.untrusted_server_drops,
            },
        },
        arp: ArpInspection {
            vlans: state.arp_vlans,
            // Empty = the default validation.
            validate: if state.validate.is_empty() {
                vec!["src-mac".to_string()]
            } else {
                state.validate
            },
            trusted: state.arp_trusted,
            statistics: state
                .arp_stats
                .into_iter()
                .map(|stats| DaiVlanStats {
                    vlan: stats.vlan,
                    forwarded: stats.forwarded,
                    dropped: stats.dropped,
                    bad_binding: stats.bad_binding,
                    bad_src_mac: stats.bad_src_mac,
                })
                .collect(),
        },
    })
}

/// `clear dhcp snooping binding [<mac>]`: flush dynamic bindings
/// ("" = every dynamic binding). Returns the number flushed.
pub async fn clear_snoop_binding(orch: &IpcEndpoint, mac: String) -> Result<u32> {
    Ok(orch_client(orch)
        .await?
        .clear_snoop_binding(pb::ClearSnoopBindingRequest { mac })
        .await?
        .into_inner()
        .cleared)
}
