//! The commit engine and its gRPC surface (`hemlock.v1.Mgmt`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::intents::{self, Intents, PortChange};
use crate::osapply::OsApplier;
use crate::store::{RollbackMeta, Store};

pub struct Engine {
    store: Store,
    syncd: IpcEndpoint,
    orch: IpcEndpoint,
    os: OsApplier,
    frr: crate::frrapply::FrrApplier,
    snmp: crate::snmpapply::SnmpApplier,
    dnsmasq: crate::dnsmasqapply::DnsmasqApplier,
    commit_seq: u64,
    /// Pending commit-confirm: the pre-commit running text to restore if no
    /// confirmation arrives, plus a cancel handle for the timer task.
    pending_confirm: Option<PendingConfirm>,
}

struct PendingConfirm {
    cancel: tokio::sync::oneshot::Sender<()>,
}

pub type SharedEngine = Arc<Mutex<Engine>>;

impl Engine {
    pub fn new(store: Store, syncd: IpcEndpoint, orch: IpcEndpoint, os: OsApplier) -> Self {
        Self {
            store,
            syncd,
            orch,
            os,
            frr: crate::frrapply::FrrApplier::new(),
            snmp: crate::snmpapply::SnmpApplier::new(),
            dnsmasq: crate::dnsmasqapply::DnsmasqApplier::new(),
            commit_seq: 0,
            pending_confirm: None,
        }
    }

    /// One commit runs behind the engine mutex, so a hung dependency
    /// would wedge every later RPC until a daemon restart. All of
    /// mgmtd's calls are unary; a per-request deadline turns a hung
    /// syncd/orch into a failed commit instead.
    const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    async fn syncd_client(
        &self,
    ) -> Result<pb::syncd_client::SyncdClient<tonic::transport::Channel>> {
        let channel = self
            .syncd
            .connect_with_request_timeout(Self::RPC_TIMEOUT)
            .await
            .context("connecting to syncd")?;
        Ok(pb::syncd_client::SyncdClient::new(channel))
    }

    async fn orch_client(&self) -> Result<pb::orch_client::OrchClient<tonic::transport::Channel>> {
        let channel = self
            .orch
            .connect_with_request_timeout(Self::RPC_TIMEOUT)
            .await
            .context("connecting to orch")?;
        Ok(pb::orch_client::OrchClient::new(channel))
    }

    fn parse_intents(text: &str) -> Result<Intents> {
        let tree = hemlock_config::parse(text).map_err(|e| anyhow!("{e}"))?;
        intents::extract(&tree).map_err(|e| anyhow!("{e}"))
    }

    /// Validate candidate text (syntax + intents + interface names).
    async fn validate(&self, text: &str) -> Result<()> {
        let wanted = Self::parse_intents(text)?;
        // Management blocks must name the manifest's management port
        // (any Management* is accepted off-switch, where no manifest
        // pins the name).
        if let Some(mgmt) = self.os.management_interface() {
            for name in wanted.management.keys() {
                if name != mgmt {
                    anyhow::bail!("unknown interface {name:?}");
                }
            }
        }
        // `maximum-paths` is capped at the probed ECMP width.
        let max_paths = wanted
            .ospf
            .as_ref()
            .map(|o| ("ospf", o.maximum_paths))
            .into_iter()
            .chain(wanted.bgp.as_ref().map(|b| ("bgp", b.maximum_paths)))
            .max_by_key(|(_, paths)| *paths);
        if let Some((family, paths)) = max_paths {
            if let Ok(mut client) = self.syncd_client().await {
                if let Ok(info) = client.get_switch_info(pb::GetSwitchInfoRequest {}).await {
                    let width = info
                        .into_inner()
                        .capabilities
                        .map(|c| c.ecmp_width)
                        .unwrap_or(0);
                    if u32::from(paths) > width {
                        anyhow::bail!(
                            "routing {family}: maximum-paths {paths} exceeds this platform's ECMP width of {width}"
                        );
                    }
                }
            }
        }
        // Security-suite capability gates: probed once, so a commit
        // needing an absent stage/policer/learn-limit fails at
        // validation with the platform error, before anything applies.
        let bindings = intents::acl_bindings(&wanted);
        // Port-security is absent here on purpose: syncd enforces it in
        // software when the ASIC has no learn limit, so it needs no
        // capability probe.
        let needs_security_caps = !bindings.is_empty() || !wanted.copp.is_empty();
        if needs_security_caps {
            if let Ok(mut client) = self.syncd_client().await {
                if let Ok(info) = client.get_switch_info(pb::GetSwitchInfoRequest {}).await {
                    let caps = info.into_inner().capabilities.unwrap_or_default();
                    if !bindings.is_empty() && !caps.acl_ingress {
                        anyhow::bail!("ACLs are not supported by this platform's SAI");
                    }
                    if bindings.keys().any(|(_, egress)| *egress) && !caps.acl_egress {
                        anyhow::bail!("egress ACLs are not supported by this platform's SAI");
                    }
                    let bound_polices = bindings.values().any(|acl| {
                        wanted
                            .acls
                            .get(acl)
                            .map(|a| a.rules.values().any(|r| r.police.is_some()))
                            .unwrap_or(false)
                    });
                    if bound_polices && !caps.acl_entry_policer {
                        anyhow::bail!("per-rule policers are not supported by this platform's SAI");
                    }
                    if !wanted.copp.is_empty() && !caps.copp {
                        anyhow::bail!(
                            "control-plane policing is not supported by this platform's SAI"
                        );
                    }
                }
            }
        }
        // QoS capability gates: probed once so a commit needing an
        // absent map direction, WRED/ECN, or a queue shaper fails with
        // the platform error before anything applies. The WRED
        // thresholds validate against the probed packet buffer here too
        // — nothing below mgmtd knows the board's buffer size.
        let qos_ports = intents::port_qos_state(&wanted);
        let needs_qos_caps = !wanted.qos_maps.is_empty() || !qos_ports.is_empty();
        if needs_qos_caps {
            if let Ok(mut client) = self.syncd_client().await {
                if let Ok(info) = client.get_switch_info(pb::GetSwitchInfoRequest {}).await {
                    let caps = info.into_inner().capabilities.unwrap_or_default();
                    let classification = !wanted.qos_maps.dscp_to_tc.is_empty()
                        || !wanted.qos_maps.cos_to_tc.is_empty()
                        || qos_ports
                            .values()
                            .any(|qos| qos.trust != intents::QosTrust::Untrusted);
                    if classification && !caps.qos_map_ingress {
                        anyhow::bail!(
                            "QoS classification maps are not supported by this platform's SAI"
                        );
                    }
                    let rewrite = !wanted.qos_maps.tc_to_dscp.is_empty()
                        || !wanted.qos_maps.tc_to_cos.is_empty();
                    if rewrite && !caps.qos_map_egress {
                        anyhow::bail!("QoS rewrite maps are not supported by this platform's SAI");
                    }
                    let queues = || qos_ports.values().flat_map(|qos| qos.queues.values());
                    if queues().any(|queue| queue.shape.is_some()) && !caps.queue_shaper {
                        anyhow::bail!("per-queue shapers are not supported by this platform's SAI");
                    }
                    let referenced: std::collections::BTreeSet<&String> = queues()
                        .filter_map(|queue| queue.wred_profile.as_ref())
                        .collect();
                    if !referenced.is_empty() && !caps.wred {
                        anyhow::bail!("WRED is not supported by this platform's SAI");
                    }
                    let ecn_wanted = referenced
                        .iter()
                        .filter_map(|name| wanted.wred_profiles.get(*name))
                        .any(|profile| profile.ecn);
                    if ecn_wanted && !caps.ecn {
                        anyhow::bail!("ECN marking is not supported by this platform's SAI");
                    }
                    let buffer_kb = caps.buffer_bytes_total / 1024;
                    if buffer_kb > 0 {
                        for (name, profile) in &wanted.wred_profiles {
                            let max = profile.max_threshold.unwrap_or(0);
                            if u64::from(max) > buffer_kb {
                                anyhow::bail!(
                                    "qos wred-profile {name}: max-threshold {max} KB exceeds this platform's {buffer_kb} KB packet buffer"
                                );
                            }
                        }
                    }
                }
            }
        }

        // sFlow is hardware sampling, so a platform whose SAI serves no
        // samplepacket objects fails the commit here rather than
        // pretending to sample.
        if wanted.sflow.enabled() {
            if let Ok(mut client) = self.syncd_client().await {
                if let Ok(info) = client.get_switch_info(pb::GetSwitchInfoRequest {}).await {
                    let caps = info.into_inner().capabilities.unwrap_or_default();
                    if let Some(message) = sflow_capability_error(&wanted, &caps) {
                        anyhow::bail!(message);
                    }
                }
            }
        }

        let port_references = !wanted.ports.is_empty()
            || !wanted.mac_table.statics.is_empty()
            || !wanted.mirror.is_empty()
            || [&wanted.igmp_snooping, &wanted.mld_snooping]
                .iter()
                .any(|s| s.vlans.values().any(|v| !v.mrouters.is_empty()));
        if !port_references {
            return Ok(());
        }
        let mut client = self.syncd_client().await?;
        let known_ports = client
            .list_ports(pb::ListPortsRequest {})
            .await
            .context("listing ports from syncd")?
            .into_inner()
            .ports;
        let known: std::collections::HashSet<_> =
            known_ports.iter().map(|p| p.name.clone()).collect();
        for name in wanted.ports.keys() {
            if !known.contains(name) {
                anyhow::bail!("unknown interface {name:?}");
            }
        }
        // A port shaper above the port's line rate is dead config; the
        // port table is the only place its speed is known.
        let port_speeds: std::collections::HashMap<String, u32> = known_ports
            .iter()
            .map(|port| (port.name.clone(), port.speed_mbps))
            .collect();
        for (name, qos) in &qos_ports {
            let Some(rate) = qos.shape else { continue };
            let Some(speed) = port_speeds.get(name) else {
                continue;
            };
            let line_rate = u64::from(*speed) * 1_000_000;
            if line_rate > 0 && rate > line_rate {
                anyhow::bail!(
                    "interface {name}: qos: shaper {} exceeds the port's {speed} Mbps line rate",
                    hemlock_common::net::format_shape_rate(rate)
                );
            }
        }

        // L2 references may also name a configured port-channel.
        let lags: std::collections::HashSet<String> = intents::lag_state(&wanted)
            .keys()
            .map(|group| format!("Port-Channel{group}"))
            .collect();
        let l2_ref = |name: &str| known.contains(name) || lags.contains(name);
        for ((mac, vlan), target) in &wanted.mac_table.statics {
            if let intents::FdbTarget::Port(port) = target {
                if !l2_ref(port) {
                    anyhow::bail!("mac-table static {mac} vlan {vlan}: unknown interface {port:?}");
                }
            }
        }
        for (session, mirror) in &wanted.mirror {
            for port in mirror.sources.keys() {
                if !l2_ref(port) {
                    anyhow::bail!("mirror session {session}: unknown source {port:?}");
                }
            }
            if let Some(dest) = &mirror.destination {
                if !known.contains(dest) {
                    anyhow::bail!(
                        "mirror session {session}: destination must be a physical port, got {dest:?}"
                    );
                }
            }
        }
        for (family, snooping) in [
            ("igmp-snooping", &wanted.igmp_snooping),
            ("mld-snooping", &wanted.mld_snooping),
        ] {
            for (vlan, config) in &snooping.vlans {
                for port in &config.mrouters {
                    if !l2_ref(port) {
                        anyhow::bail!("{family} vlan {vlan}: unknown mrouter interface {port:?}");
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply the delta between running and `new_text` — ASIC ports
    /// through syncd, OS-side families through the OS applier — then
    /// persist `new_text` as running. Returns the applied changes,
    /// described.
    async fn apply_and_persist(&mut self, new_text: &str, comment: &str) -> Result<Vec<String>> {
        let running_intents = Self::parse_intents(&self.store.running()?).unwrap_or_default();
        let wanted_intents = Self::parse_intents(new_text)?;
        let port_changes = intents::diff(&running_intents.ports, &wanted_intents.ports);

        if !port_changes.is_empty() {
            let mut client = self.syncd_client().await?;
            for change in &port_changes {
                client
                    .set_port_attrs(pb::SetPortAttrsRequest {
                        name: change.name.clone(),
                        admin_state: change.admin_up.map(|up| {
                            if up {
                                pb::AdminState::Up as i32
                            } else {
                                pb::AdminState::Down as i32
                            }
                        }),
                        description: change.description.clone(),
                        speed_mbps: change.speed_mbps,
                        duplex: change.duplex.clone(),
                        mtu: change.mtu,
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
        }

        // VLANs and switchport programs (ASIC-side through syncd):
        // ensure VLANs before the switchports that reference them, and
        // remove VLANs only after those ports are reprogrammed.
        let vlan_changes = intents::diff_vlans(&running_intents, &wanted_intents);
        let switchport_changes = intents::diff_switchports(&running_intents, &wanted_intents);
        if !vlan_changes.is_empty() || !switchport_changes.is_empty() {
            let mut client = self.syncd_client().await?;
            for change in &vlan_changes {
                let Some(vlan) = &change.ensure else {
                    continue;
                };
                client
                    .ensure_vlan(pb::EnsureVlanRequest {
                        id: change.id.into(),
                        name: vlan.description.clone().unwrap_or_default(),
                        suspend: vlan.suspended,
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
            for change in &switchport_changes {
                match &change.set {
                    Some(sp) => {
                        client
                            .set_port_switchport(switchport_request(&change.name, sp))
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                    None => {
                        client
                            .clear_port_switchport(pb::ClearPortSwitchportRequest {
                                name: change.name.clone(),
                            })
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                }
            }
            for change in vlan_changes.iter().filter(|c| c.ensure.is_none()) {
                client
                    .remove_vlan(pb::RemoveVlanRequest {
                        id: change.id.into(),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
        }

        let os_changes = intents::diff_os(&running_intents, &wanted_intents);

        // ASIC side of front-panel and SVI addresses (router interface +
        // routes) goes through syncd; the kernel side follows in the OS
        // applier. SVIs after VLAN ensures (above) so the VLAN exists.
        if !os_changes.ports.is_empty() || !os_changes.svis.is_empty() {
            let mut client = self.syncd_client().await?;
            for change in os_changes.ports.iter().chain(&os_changes.svis) {
                match (&change.set_address, &change.del_address) {
                    (Some(address), _) => {
                        client
                            .set_interface_address(pb::SetInterfaceAddressRequest {
                                name: change.name.clone(),
                                address: address.clone(),
                            })
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                    (None, Some(_)) => {
                        client
                            .clear_interface_address(pb::ClearInterfaceAddressRequest {
                                name: change.name.clone(),
                            })
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                    (None, None) => {}
                }
            }
        }

        // Switching-suite families: the mac-table, storm-control and
        // mirror deltas apply through syncd; the protocol families (LAG/
        // LACP, STP, snooping) go to orch when its engines land — until
        // then their diffs are computed and shown in the commit output.
        let lag_changes = intents::diff_lags(&running_intents, &wanted_intents);
        let stp_change = intents::diff_stp(&running_intents, &wanted_intents);
        let igmp_change = intents::diff_snooping(
            &running_intents.igmp_snooping,
            &wanted_intents.igmp_snooping,
        );
        let mld_change =
            intents::diff_snooping(&running_intents.mld_snooping, &wanted_intents.mld_snooping);
        let mac_changes = intents::diff_mac_table(&running_intents, &wanted_intents);
        let storm_changes = intents::diff_storm_control(&running_intents, &wanted_intents);
        let mirror_changes = intents::diff_mirror(&running_intents, &wanted_intents);

        // LAGs: ensure the syncd objects (and their switchport
        // programs), push the protocol state to orch (which drives the
        // member gates), then drop removed groups.
        let lacp_changed = running_intents.lacp != wanted_intents.lacp;
        if !lag_changes.is_empty() || lacp_changed {
            let mut client = self.syncd_client().await?;
            for change in lag_changes.iter().filter(|c| c.ensure.is_some()) {
                let ensure = change.ensure.as_ref().expect("filtered on is_some");
                self.ensure_lag(&mut client, change.group, ensure).await?;
            }
            self.push_lag_configs(&wanted_intents)
                .await
                .context("pushing LAG configs to orch")?;
            for change in lag_changes.iter().filter(|c| c.ensure.is_none()) {
                client
                    .remove_lag(pb::RemoveLagRequest {
                        group: u32::from(change.group),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
        }

        if !mac_changes.is_empty() || !storm_changes.is_empty() || !mirror_changes.is_empty() {
            let mut client = self.syncd_client().await?;
            self.apply_switching(
                &mut client,
                &running_intents,
                &mac_changes,
                &storm_changes,
                &mirror_changes,
            )
            .await?;
        }

        // The security suite: ACL definitions + bindings, CoPP class
        // overrides, and port-security ride syncd; dot1x and
        // snooping/DAI are whole-state pushes to their orch engines
        // (which drive the enforcement back into syncd).
        let acl_changes = intents::diff_acls(&running_intents, &wanted_intents);
        let acl_binding_changes = intents::diff_acl_bindings(&running_intents, &wanted_intents);
        let copp_changes = intents::diff_copp(&running_intents, &wanted_intents);
        let psec_changes = intents::diff_port_security(&running_intents, &wanted_intents);
        let dot1x_change = intents::diff_dot1x(&running_intents, &wanted_intents);
        let snoopsec_change = intents::diff_snoopsec(&running_intents, &wanted_intents);
        if !acl_changes.is_empty()
            || !acl_binding_changes.is_empty()
            || !copp_changes.is_empty()
            || !psec_changes.is_empty()
        {
            let mut client = self.syncd_client().await?;
            // Definitions before the bindings that reference them;
            // unbinds before removals so a dropped ACL is free by the
            // time it goes.
            for change in acl_changes.iter().filter(|c| c.ensure.is_some()) {
                let ensure = change.ensure.as_ref().expect("filtered on is_some");
                client
                    .ensure_acl(acl_request(&change.name, ensure))
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
            for change in acl_binding_changes.iter().filter(|c| c.acl.is_none()) {
                client
                    .unbind_port_acl(pb::UnbindPortAclRequest {
                        port: change.target.clone(),
                        stage: acl_stage(change.egress),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
            for change in acl_binding_changes.iter().filter(|c| c.acl.is_some()) {
                client
                    .bind_port_acl(pb::BindPortAclRequest {
                        port: change.target.clone(),
                        stage: acl_stage(change.egress),
                        acl: change.acl.clone().expect("filtered on is_some"),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
            for change in acl_changes.iter().filter(|c| c.ensure.is_none()) {
                client
                    .remove_acl(pb::RemoveAclRequest {
                        name: change.name.clone(),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
            for change in &copp_changes {
                client
                    .set_copp_class(pb::SetCoppClassRequest {
                        class: change.class.clone(),
                        rate: change.set.as_ref().and_then(|s| s.rate),
                        burst: change.set.as_ref().and_then(|s| s.burst),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
            for change in &psec_changes {
                match &change.set {
                    Some(ps) => {
                        client
                            .set_port_security(pb::SetPortSecurityRequest {
                                port: change.port.clone(),
                                maximum: ps.maximum,
                                shutdown: ps.shutdown,
                            })
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                    None => {
                        client
                            .clear_port_security(pb::ClearPortSecurityRequest {
                                port: change.port.clone(),
                            })
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                }
            }
        }
        if let Some(dot1x) = &dot1x_change {
            self.push_dot1x_config(dot1x)
                .await
                .context("pushing dot1x config to orch")?;
        }
        if let Some(snoopsec) = &snoopsec_change {
            self.push_snoopsec_config(snoopsec)
                .await
                .context("pushing dhcp-snooping/arp-inspection config to orch")?;
        }

        // The QoS suite: profiles push before the port programs that
        // reference them, the global maps before the ports whose trust
        // mode reads them, and profile removals last (a profile is only
        // free once no queue binds it).
        let qos_map_change = intents::diff_qos_maps(&running_intents, &wanted_intents);
        let wred_changes = intents::diff_wred_profiles(&running_intents, &wanted_intents);
        let port_qos_changes = intents::diff_port_qos(&running_intents, &wanted_intents);
        if qos_map_change.is_some() || !wred_changes.is_empty() || !port_qos_changes.is_empty() {
            let mut client = self.syncd_client().await?;
            for change in wred_changes.iter().filter(|c| c.ensure.is_some()) {
                let profile = change.ensure.as_ref().expect("filtered on is_some");
                client
                    .ensure_wred_profile(pb::EnsureWredProfileRequest {
                        profile: Some(wred_profile_request(&change.name, profile)),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
            if let Some(maps) = &qos_map_change {
                client
                    .set_qos_maps(qos_maps_request(maps))
                    .await
                    .context("applying qos maps")?;
            }
            for change in &port_qos_changes {
                match &change.set {
                    Some(qos) => {
                        client
                            .set_port_qos(port_qos_request(&change.port, qos))
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                    None => {
                        client
                            .clear_port_qos(pb::ClearPortQosRequest {
                                port: change.port.clone(),
                            })
                            .await
                            .with_context(|| format!("applying {}", change.describe()))?;
                    }
                }
            }
            for change in wred_changes.iter().filter(|c| c.ensure.is_none()) {
                client
                    .remove_wred_profile(pb::RemoveWredProfileRequest {
                        name: change.name.clone(),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
            }
        }

        // Spanning tree: MST instances in syncd, then the full state to
        // the orch engine (which drives the port states back into
        // syncd).
        if let Some(stp) = &stp_change {
            let mut client = self.syncd_client().await?;
            self.apply_stp_instances(&mut client, &running_intents.stp, &wanted_intents.stp)
                .await?;
            self.push_stp_config(stp)
                .await
                .context("pushing STP config to orch")?;
        }

        // The DHCP server: mgmtd renders the dnsmasq config below (with
        // the OS pass) and pushes the same pools to orch, which reads
        // the lease file that render names.
        let dhcp_server_change = running_intents.dhcp_server != wanted_intents.dhcp_server;
        if dhcp_server_change {
            self.push_dhcp_server_config(&wanted_intents)
                .await
                .context("pushing dhcp-server config to orch")?;
        }

        // sFlow: syncd owns the ASIC sampler (session + port binds),
        // orch owns the export. syncd goes first — a collector should
        // never be told about a sampler that failed to program.
        let sflow_change = intents::diff_sflow(&running_intents, &wanted_intents);
        if let Some(wanted) = &sflow_change {
            let ports = sflow_sampled_ports(&wanted_intents);
            let mut client = self.syncd_client().await?;
            client
                .set_sflow_sampling(pb::SetSflowSamplingRequest {
                    rate: if wanted.global.enabled() {
                        wanted.global.rate()
                    } else {
                        0
                    },
                    ports: ports.clone(),
                })
                .await
                .context("programming sflow sampling in syncd")?;
            self.push_sflow_config(&wanted_intents, &ports)
                .await
                .context("pushing sflow config to orch")?;
        }

        // SNMP: mgmtd renders snmpd.conf below (with the OS pass) and
        // pushes the same state to orch, whose AgentX subagent serves
        // the IF-MIB on the socket the render names.
        let snmp_change = (running_intents.snmp != wanted_intents.snmp
            || crate::intents::management_address(&running_intents)
                != crate::intents::management_address(&wanted_intents))
        .then(|| wanted_intents.clone());
        if let Some(wanted) = &snmp_change {
            self.push_snmp_config(wanted)
                .await
                .context("pushing snmp config to orch")?;
        }

        // LLDP: the whole wanted state to the orch engine, which owns
        // the advertisement timers and the neighbor table.
        let lldp_change = intents::diff_lldp(&running_intents, &wanted_intents);
        if let Some(lldp) = &lldp_change {
            self.push_lldp_config(lldp)
                .await
                .context("pushing lldp config to orch")?;
        }

        // Snooping: the full family state to the orch engines (which
        // drive the L2MC programming back into syncd).
        if let Some(snooping) = &igmp_change {
            self.push_snooping_config("igmp", snooping)
                .await
                .context("pushing igmp-snooping config to orch")?;
        }
        if let Some(snooping) = &mld_change {
            self.push_snooping_config("mld", snooping)
                .await
                .context("pushing mld-snooping config to orch")?;
        }

        // VRRP virtual MACs into the ASIC's My-MAC table. Runs with the
        // fallible syncd steps: a platform whose SAI refused the My-MAC
        // capability fails the commit here with the platform error.
        let my_macs_wanted = vrrp_my_macs(&wanted_intents);
        let my_macs_running = vrrp_my_macs(&running_intents);
        if my_macs_wanted != my_macs_running {
            let mut client = self.syncd_client().await?;
            for (vlan, mac) in my_macs_wanted.difference(&my_macs_running) {
                client
                    .ensure_my_mac(pb::EnsureMyMacRequest {
                        vlan: *vlan,
                        mac: mac.clone(),
                    })
                    .await
                    .with_context(|| format!("installing VRRP virtual MAC {mac}"))?;
            }
            for (vlan, mac) in my_macs_running.difference(&my_macs_wanted) {
                client
                    .remove_my_mac(pb::RemoveMyMacRequest {
                        vlan: *vlan,
                        mac: mac.clone(),
                    })
                    .await
                    .with_context(|| format!("removing VRRP virtual MAC {mac}"))?;
            }
        }

        // The kernel-side apply runs LAST, after every fallible step:
        // it is best-effort (never fails the commit), so were it to run
        // earlier a later syncd/orch failure would leave the kernel
        // changed (management address gone, say) while running kept the
        // old text — and the re-add would then diff to nothing, leaving
        // the box unreachable until a replay. Fallible first, then the
        // infallible OS pass, then persist.
        self.os.apply(&os_changes);

        // FRR rides behind the OS pass (VRRP macvlans must exist before
        // the reload); render-diff gated so unrelated commits leave FRR
        // alone. Best-effort like the OS applier.
        let frr_changed = crate::frrapply::render_frr(&running_intents)
            != crate::frrapply::render_frr(&wanted_intents);
        if frr_changed {
            self.frr.apply(&wanted_intents);
        }

        // snmpd rides the same render-diff gate: an unrelated commit
        // leaves the agent (and every established v3 session) alone.
        let snmpd_changed = crate::snmpapply::render_snmpd(&running_intents)
            != crate::snmpapply::render_snmpd(&wanted_intents);
        if snmpd_changed {
            self.snmp.apply(&wanted_intents);
        }

        // dnsmasq likewise: a restart hands every renewing client a new
        // transaction, so it happens only when the pools really moved.
        let dnsmasq_changed = crate::dnsmasqapply::render_dnsmasq(&running_intents)
            != crate::dnsmasqapply::render_dnsmasq(&wanted_intents);
        if dnsmasq_changed {
            self.dnsmasq.apply(&wanted_intents);
        }

        self.store.commit(
            new_text,
            &RollbackMeta {
                committed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                comment: comment.to_string(),
            },
        )?;
        self.commit_seq += 1;
        let mut described: Vec<String> = port_changes.iter().map(PortChange::describe).collect();
        described.extend(vlan_changes.iter().map(intents::VlanChange::describe));
        described.extend(
            switchport_changes
                .iter()
                .map(intents::SwitchportChange::describe),
        );
        described.extend(os_changes.describe());
        described.extend(lag_changes.iter().map(intents::LagChange::describe));
        if stp_change.is_some() {
            described.push("spanning-tree configuration updated".into());
        }
        if igmp_change.is_some() {
            described.push("igmp-snooping configuration updated".into());
        }
        if mld_change.is_some() {
            described.push("mld-snooping configuration updated".into());
        }
        if lldp_change.is_some() {
            described.push("lldp configuration updated".into());
        }
        if snmp_change.is_some() {
            described.push("snmp configuration updated".into());
        }
        if sflow_change.is_some() {
            described.push("sflow configuration updated".into());
        }
        if dhcp_server_change {
            described.push("dhcp-server configuration updated".into());
        }
        described.extend(mac_changes.describe());
        described.extend(storm_changes.iter().map(intents::StormChange::describe));
        described.extend(mirror_changes.iter().map(intents::MirrorChange::describe));
        described.extend(acl_changes.iter().map(intents::AclChange::describe));
        described.extend(
            acl_binding_changes
                .iter()
                .map(intents::AclBindingChange::describe),
        );
        described.extend(copp_changes.iter().map(intents::CoppChange::describe));
        described.extend(
            psec_changes
                .iter()
                .map(intents::PortSecurityChange::describe),
        );
        if qos_map_change.is_some() {
            described.push("qos maps updated".into());
        }
        described.extend(
            wred_changes
                .iter()
                .map(intents::WredProfileChange::describe),
        );
        described.extend(
            port_qos_changes
                .iter()
                .map(intents::PortQosChange::describe),
        );
        if dot1x_change.is_some() {
            described.push("dot1x configuration updated".into());
        }
        if snoopsec_change.is_some() {
            described.push("dhcp-snooping/arp-inspection configuration updated".into());
        }
        if frr_changed {
            described.push("frr configuration updated (ospf/bgp/vrrp)".into());
        }
        described.extend(wanted_intents.warnings.iter().cloned());
        Ok(described)
    }
}

impl Engine {
    /// Ensure one port-channel in syncd: the LAG object itself plus its
    /// switchport program. Membership and gates are orch's to drive.
    async fn ensure_lag(
        &self,
        client: &mut pb::syncd_client::SyncdClient<tonic::transport::Channel>,
        group: u16,
        ensure: &intents::LagEnsure,
    ) -> Result<()> {
        client
            .create_lag(pb::CreateLagRequest {
                group: u32::from(group),
                description: ensure.lag.description.clone().unwrap_or_default(),
                admin_up: ensure.lag.admin_up.unwrap_or(true),
            })
            .await
            .with_context(|| format!("ensuring Port-Channel{group}"))?;
        let name = format!("Port-Channel{group}");
        match &ensure.lag.switchport {
            Some(sp) => {
                client
                    .set_port_switchport(switchport_request(&name, sp))
                    .await
                    .with_context(|| format!("applying {name} switchport"))?;
            }
            None => {
                client
                    .clear_port_switchport(pb::ClearPortSwitchportRequest { name: name.clone() })
                    .await
                    .with_context(|| format!("clearing {name} switchport"))?;
            }
        }
        Ok(())
    }

    /// Push the full wanted LAG/LACP state to orch (declarative; orch
    /// tears down groups absent from the push). Orch being down fails
    /// the commit the same way syncd being down does.
    async fn push_lag_configs(&self, wanted: &Intents) -> Result<()> {
        let mut client = self.orch_client().await?;
        let lags = intents::lag_state(wanted)
            .into_iter()
            .map(|(group, ensure)| pb::LagConfig {
                group: u32::from(group),
                min_links: u32::from(ensure.lag.min_links.unwrap_or(0)),
                fallback: match ensure.lag.fallback {
                    Some(intents::LagFallback::Static) => pb::LagFallbackMode::Static,
                    Some(intents::LagFallback::Individual) => pb::LagFallbackMode::Individual,
                    None => pb::LagFallbackMode::None,
                } as i32,
                fallback_timeout_secs: u32::from(ensure.lag.fallback_timeout.unwrap_or(90)),
                members: ensure
                    .members
                    .iter()
                    .map(|(port, (mode, lacp))| pb::LacpMemberConfig {
                        port: port.clone(),
                        mode: match mode {
                            intents::LacpMode::Active => pb::LacpConfigMode::Active,
                            intents::LacpMode::Passive => pb::LacpConfigMode::Passive,
                            intents::LacpMode::On => pb::LacpConfigMode::On,
                        } as i32,
                        rate_fast: lacp.rate_fast,
                        port_priority: u32::from(lacp.port_priority.unwrap_or(32768)),
                    })
                    .collect(),
            })
            .collect();
        client
            .set_lag_configs(pb::SetLagConfigsRequest {
                lags,
                system_priority: u32::from(wanted.lacp.system_priority.unwrap_or(32768)),
            })
            .await
            .context("SetLagConfigs")?;
        Ok(())
    }

    /// Push one snooping family's full wanted state to orch.
    async fn push_snooping_config(
        &self,
        family: &str,
        wanted: &intents::SnoopingIntent,
    ) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_snooping_config(pb::SetSnoopingConfigRequest {
                family: family.to_string(),
                disabled: wanted.disabled,
                robustness: u32::from(wanted.robustness.unwrap_or(2)),
                vlans: wanted
                    .vlans
                    .iter()
                    .map(|(vlan, config)| pb::SnoopingVlanConfig {
                        vlan: u32::from(*vlan),
                        disabled: config.disabled,
                        fast_leave: config.fast_leave,
                        querier: config.querier,
                        querier_address: config.querier_address.clone().unwrap_or_default(),
                        mrouters: config.mrouters.clone(),
                    })
                    .collect(),
            })
            .await
            .context("SetSnoopingConfig")?;
        Ok(())
    }

    /// Push the whole wanted pool set to orch, which reads dnsmasq's
    /// lease file against it.
    async fn push_dhcp_server_config(&self, wanted: &Intents) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_dhcp_server_config(pb::SetDhcpServerConfigRequest {
                pools: wanted
                    .dhcp_server
                    .iter()
                    .map(|(name, pool)| {
                        let (start, end) = pool.range.unwrap_or((
                            std::net::Ipv4Addr::UNSPECIFIED,
                            std::net::Ipv4Addr::UNSPECIFIED,
                        ));
                        pb::DhcpPoolConfig {
                            name: name.clone(),
                            network: pool.network.clone().unwrap_or_default(),
                            range_start: start.to_string(),
                            range_end: end.to_string(),
                            gateway: pool
                                .default_gateway
                                .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED)
                                .to_string(),
                            dns_servers: pool
                                .dns_servers
                                .iter()
                                .map(|server| server.to_string())
                                .collect(),
                            lease_time: pool.lease(),
                            domain_name: pool.domain_name.clone().unwrap_or_default(),
                            reservations: pool
                                .reservations
                                .iter()
                                .map(|(mac, address)| pb::DhcpReservationConfig {
                                    mac: mac.clone(),
                                    address: address.to_string(),
                                })
                                .collect(),
                        }
                    })
                    .collect(),
            })
            .await
            .context("SetDhcpServerConfig")?;
        Ok(())
    }

    /// Push the whole wanted sFlow export state to orch. `ports` is
    /// the same sampled-port list syncd was programmed with, so
    /// `show sflow` and the ASIC can never disagree.
    async fn push_sflow_config(&self, wanted: &Intents, ports: &[String]) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_sflow_config(pb::SetSflowConfigRequest {
                enabled: wanted.sflow.enabled(),
                collectors: wanted
                    .sflow
                    .collectors
                    .iter()
                    .map(|collector| pb::SflowCollectorConfig {
                        address: collector.address.clone(),
                        port: u32::from(collector.port.unwrap_or(0)),
                    })
                    .collect(),
                sample_rate: wanted.sflow.rate(),
                polling_interval: u32::from(wanted.sflow.polling()),
                agent_address: crate::intents::management_address(wanted).unwrap_or_default(),
                agent_interface: crate::intents::management_name(wanted).unwrap_or_default(),
                enabled_ports: ports.to_vec(),
                disabled_ports: intents::sflow_state(wanted).disabled_ports,
            })
            .await
            .context("SetSflowConfig")?;
        Ok(())
    }

    /// Push the whole wanted SNMP state to orch (which runs the AgentX
    /// subagent). Passphrases stay in mgmtd's snmpd.conf render: orch
    /// never needs them, so they never cross the wire.
    async fn push_snmp_config(&self, wanted: &Intents) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_snmp_config(pb::SetSnmpConfigRequest {
                enabled: wanted.snmp.enabled,
                socket: crate::snmpapply::AGENTX_SOCKET.to_string(),
                location: wanted.snmp.location.clone().unwrap_or_default(),
                contact: wanted.snmp.contact.clone().unwrap_or_default(),
                communities: wanted
                    .snmp
                    .communities
                    .iter()
                    .map(|community| pb::SnmpCommunityConfig {
                        name: community.name.clone(),
                        source: community.source.clone().unwrap_or_default(),
                    })
                    .collect(),
                users: wanted.snmp.users.keys().cloned().collect(),
                listen_interface: crate::intents::management_name(wanted).unwrap_or_default(),
                listen_address: crate::intents::management_address(wanted).unwrap_or_default(),
            })
            .await
            .context("SetSnmpConfig")?;
        Ok(())
    }

    /// Push the whole wanted LLDP state to orch.
    async fn push_lldp_config(&self, wanted: &intents::LldpState) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_lldp_config(pb::SetLldpConfigRequest {
                disabled: wanted.global.disabled,
                tx_interval: u32::from(wanted.global.tx_interval.unwrap_or(0)),
                hold_multiplier: u32::from(wanted.global.hold_multiplier.unwrap_or(0)),
                disabled_ports: wanted.disabled_ports.clone(),
            })
            .await
            .context("SetLldpConfig")?;
        Ok(())
    }

    /// Push the whole dot1x state to orch (which owns hostapd and
    /// drives port authorization into syncd).
    async fn push_dot1x_config(&self, wanted: &intents::Dot1xState) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_dot1x_config(pb::SetDot1xConfigRequest {
                radius_servers: wanted
                    .intent
                    .radius_servers
                    .iter()
                    .map(|server| pb::RadiusServerConfig {
                        ip: server.ip.clone(),
                        key: server.key.clone().unwrap_or_default(),
                        port: u32::from(server.port),
                        timeout_secs: u32::from(server.timeout),
                        retransmit: u32::from(server.retransmit),
                    })
                    .collect(),
                reauth_interval: wanted.intent.reauth_interval,
                ports: wanted.ports.iter().cloned().collect(),
            })
            .await
            .context("SetDot1xConfig")?;
        Ok(())
    }

    /// Push the whole snooping/DAI state to orch (which drives the CPU
    /// redirects into syncd and validates the trapped traffic).
    async fn push_snoopsec_config(&self, wanted: &intents::SnoopSecState) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_snoop_sec_config(pb::SetSnoopSecConfigRequest {
                dhcp_vlans: wanted
                    .intent
                    .dhcp_vlans
                    .iter()
                    .map(|v| u32::from(*v))
                    .collect(),
                arp_vlans: wanted
                    .intent
                    .arp_vlans
                    .iter()
                    .map(|v| u32::from(*v))
                    .collect(),
                validate: wanted
                    .intent
                    .validate
                    .iter()
                    .map(|v| v.word().to_string())
                    .collect(),
                dhcp_trusted: wanted.dhcp_trusted.iter().cloned().collect(),
                arp_trusted: wanted.arp_trusted.iter().cloned().collect(),
                static_bindings: wanted
                    .intent
                    .static_bindings
                    .iter()
                    .map(|((mac, vlan), binding)| pb::StaticBindingConfig {
                        mac: mac.clone(),
                        vlan: u32::from(*vlan),
                        address: binding.address.clone(),
                        interface: binding.interface.clone(),
                    })
                    .collect(),
                dhcp_relay: wanted
                    .relay
                    .iter()
                    .map(|(vlan, (servers, giaddr))| pb::DhcpRelayVlanConfig {
                        vlan: u32::from(*vlan),
                        servers: servers.iter().map(|s| s.to_string()).collect(),
                        giaddr: giaddr.clone(),
                    })
                    .collect(),
            })
            .await
            .context("SetSnoopSecConfig")?;
        Ok(())
    }

    /// Reconcile the MST instance objects in syncd: create/update the
    /// wanted instances' VLAN mappings, drop the stale ones.
    async fn apply_stp_instances(
        &self,
        client: &mut pb::syncd_client::SyncdClient<tonic::transport::Channel>,
        running: &intents::StpIntent,
        wanted: &intents::StpIntent,
    ) -> Result<()> {
        for (instance, vlans) in &wanted.instances {
            if running.instances.get(instance) == Some(vlans) {
                continue;
            }
            client
                .create_stp_instance(pb::CreateStpInstanceRequest {
                    instance: u32::from(*instance),
                })
                .await
                .with_context(|| format!("creating mst instance {instance}"))?;
            client
                .set_stp_instance_vlans(pb::SetStpInstanceVlansRequest {
                    instance: u32::from(*instance),
                    vlans: vlans.iter().map(|v| u32::from(*v)).collect(),
                })
                .await
                .with_context(|| format!("mapping mst instance {instance} vlans"))?;
        }
        for instance in running.instances.keys() {
            if !wanted.instances.contains_key(instance) {
                client
                    .remove_stp_instance(pb::RemoveStpInstanceRequest {
                        instance: u32::from(*instance),
                    })
                    .await
                    .with_context(|| format!("removing mst instance {instance}"))?;
            }
        }
        Ok(())
    }

    /// Push the full wanted spanning-tree state to orch.
    async fn push_stp_config(&self, state: &intents::StpState) -> Result<()> {
        let mut client = self.orch_client().await?;
        client
            .set_stp_config(pb::SetStpConfigRequest {
                mode: match state.global.mode {
                    intents::StpMode::Mstp => "mstp",
                    intents::StpMode::Rstp => "rstp",
                    intents::StpMode::None => "none",
                }
                .into(),
                priority: u32::from(state.global.priority.unwrap_or(32768)),
                hello_time: u32::from(state.global.hello_time.unwrap_or(2)),
                max_age: u32::from(state.global.max_age.unwrap_or(20)),
                forward_time: u32::from(state.global.forward_time.unwrap_or(15)),
                mst_name: state.global.mst_name.clone().unwrap_or_default(),
                mst_revision: u32::from(state.global.mst_revision.unwrap_or(0)),
                instances: state
                    .global
                    .instances
                    .iter()
                    .map(|(instance, vlans)| pb::MstInstanceMap {
                        instance: u32::from(*instance),
                        vlans: vlans.iter().map(|v| u32::from(*v)).collect(),
                    })
                    .collect(),
                ports: state
                    .ports
                    .iter()
                    .map(|(port, config)| pb::StpPortConfig {
                        port: port.clone(),
                        portfast: config.portfast,
                        bpduguard: config.bpduguard,
                        cost: config.cost.unwrap_or(0),
                        priority: u32::from(config.port_priority.unwrap_or(128)),
                    })
                    .collect(),
            })
            .await
            .context("SetStpConfig")?;
        Ok(())
    }

    /// Apply the switching-suite deltas (mac-table, storm-control,
    /// mirror) through syncd. Mirror sources on port-channels are the
    /// one deferred case (the ASIC mirrors ports, not LAGs) and are
    /// skipped loudly.
    async fn apply_switching(
        &self,
        client: &mut pb::syncd_client::SyncdClient<tonic::transport::Channel>,
        running: &Intents,
        mac_changes: &intents::MacTableChanges,
        storm_changes: &[intents::StormChange],
        mirror_changes: &[intents::MirrorChange],
    ) -> Result<()> {
        for (mac, vlan) in &mac_changes.remove {
            client
                .remove_static_fdb(pb::RemoveStaticFdbRequest {
                    mac: mac.clone(),
                    vlan: u32::from(*vlan),
                })
                .await
                .with_context(|| format!("removing mac-table static {mac} vlan {vlan}"))?;
        }
        for (mac, vlan, target) in &mac_changes.add {
            let (port, drop) = match target {
                intents::FdbTarget::Port(port) => (port.clone(), false),
                intents::FdbTarget::Drop => (String::new(), true),
            };
            client
                .add_static_fdb(pb::AddStaticFdbRequest {
                    mac: mac.clone(),
                    vlan: u32::from(*vlan),
                    port,
                    drop,
                })
                .await
                .with_context(|| format!("applying mac-table static {mac} vlan {vlan}"))?;
        }
        if let Some(secs) = mac_changes.aging_time {
            client
                .set_fdb_aging_time(pb::SetFdbAgingTimeRequest { seconds: secs })
                .await
                .with_context(|| format!("applying mac-table aging-time {secs}"))?;
        }

        for change in storm_changes {
            client
                .set_port_storm_control(pb::SetPortStormControlRequest {
                    name: change.name.clone(),
                    class: match change.kind {
                        intents::StormKind::Broadcast => pb::StormClass::Broadcast,
                        intents::StormKind::Multicast => pb::StormClass::Multicast,
                        intents::StormKind::UnknownUnicast => pb::StormClass::UnknownUnicast,
                    } as i32,
                    level: change.level.clone(),
                })
                .await
                .with_context(|| format!("applying {}", change.describe()))?;
        }

        for change in mirror_changes {
            let Some(mirror) = &change.ensure else {
                client
                    .remove_mirror_session(pb::RemoveMirrorSessionRequest {
                        session: u32::from(change.session),
                    })
                    .await
                    .with_context(|| format!("applying {}", change.describe()))?;
                continue;
            };
            let Some(destination) = &mirror.destination else {
                warn!(
                    session = change.session,
                    "mirror session has no destination; not programmed"
                );
                continue;
            };
            client
                .create_mirror_session(pb::CreateMirrorSessionRequest {
                    session: u32::from(change.session),
                    destination: destination.clone(),
                })
                .await
                .with_context(|| format!("applying {}", change.describe()))?;
            // Detach sources the previous program had and this one lost.
            if let Some(had) = running.mirror.get(&change.session) {
                for port in had.sources.keys() {
                    if !mirror.sources.contains_key(port) {
                        client
                            .set_port_mirror(pb::SetPortMirrorRequest {
                                name: port.clone(),
                                session: u32::from(change.session),
                                direction: pb::MirrorDirection::None as i32,
                            })
                            .await
                            .with_context(|| {
                                format!(
                                    "detaching mirror source {port} (session {})",
                                    change.session
                                )
                            })?;
                    }
                }
            }
            for (port, direction) in &mirror.sources {
                if port.starts_with("Port-Channel") {
                    warn!(%port, "mirror source on a port-channel waits for LAG objects");
                    continue;
                }
                client
                    .set_port_mirror(pb::SetPortMirrorRequest {
                        name: port.clone(),
                        session: u32::from(change.session),
                        direction: match direction {
                            intents::MirrorDirection::Rx => pb::MirrorDirection::Rx,
                            intents::MirrorDirection::Tx => pb::MirrorDirection::Tx,
                            intents::MirrorDirection::Both => pb::MirrorDirection::Both,
                        } as i32,
                    })
                    .await
                    .with_context(|| {
                        format!(
                            "attaching mirror source {port} (session {})",
                            change.session
                        )
                    })?;
            }
        }
        Ok(())
    }
}

/// The (VLAN scope, virtual MAC) set the VRRP groups want in the
/// ASIC's My-MAC table (VLAN 0 = unscoped, for routed-port parents).
fn vrrp_my_macs(intents: &Intents) -> std::collections::BTreeSet<(u32, String)> {
    intents
        .vrrp
        .keys()
        .map(|(interface, group)| {
            let vlan = interface
                .strip_prefix("Vlan")
                .and_then(|id| id.parse::<u32>().ok())
                .unwrap_or(0);
            (vlan, intents::vrrp_virtual_mac(*group))
        })
        .collect()
}

/// The full wanted switchport program as a syncd request.
fn acl_stage(egress: bool) -> i32 {
    (if egress {
        pb::AclStage::Egress
    } else {
        pb::AclStage::Ingress
    }) as i32
}

/// A whole-ACL declarative program for syncd.
fn acl_request(name: &str, acl: &intents::AclIntent) -> pb::EnsureAclRequest {
    pb::EnsureAclRequest {
        name: name.to_string(),
        family: (match acl.family {
            intents::AclFamily::Ipv4 => pb::AclFamily::Ipv4,
            intents::AclFamily::Ipv6 => pb::AclFamily::Ipv6,
            intents::AclFamily::Mac => pb::AclFamily::Mac,
        }) as i32,
        rules: acl
            .rules
            .iter()
            .map(|(number, rule)| pb::AclRule {
                number: *number,
                permit: rule.permit,
                protocol: rule.protocol.map(u32::from),
                source: rule.source.clone().unwrap_or_default(),
                destination: rule.destination.clone().unwrap_or_default(),
                source_port_low: rule.source_port.map(|(low, _)| u32::from(low)),
                source_port_high: rule.source_port.map(|(_, high)| u32::from(high)),
                destination_port_low: rule.destination_port.map(|(low, _)| u32::from(low)),
                destination_port_high: rule.destination_port.map(|(_, high)| u32::from(high)),
                dscp: rule.dscp.map(u32::from),
                log: rule.log,
                police_rate: rule.police.map(|p| p.rate),
                police_burst: rule.police.map(|p| p.burst),
                police_pps: rule.police.map(|p| p.pps).unwrap_or(false),
                source_mac: rule
                    .source_mac
                    .as_ref()
                    .map(|(mac, _)| mac.clone())
                    .unwrap_or_default(),
                source_mac_mask: rule
                    .source_mac
                    .as_ref()
                    .map(|(_, mask)| mask.clone())
                    .unwrap_or_default(),
                destination_mac: rule
                    .destination_mac
                    .as_ref()
                    .map(|(mac, _)| mac.clone())
                    .unwrap_or_default(),
                destination_mac_mask: rule
                    .destination_mac
                    .as_ref()
                    .map(|(_, mask)| mask.clone())
                    .unwrap_or_default(),
                ethertype: rule.ethertype.map(u32::from),
            })
            .collect(),
    }
}

/// A global-map intent as the syncd request (all four tables at once).
/// Whether a running config has anything for syncd at boot. Kept out of
/// [`Engine::replay_running`] so it can be tested without a live syncd:
/// getting it wrong strands a whole family until the next commit.
fn needs_syncd_replay(running: &Intents) -> bool {
    !running.ports.is_empty()
        || !running.vlans.is_empty()
        || !running.svis.is_empty()
        || running.mac_table != intents::MacTableIntent::default()
        || !running.mirror.is_empty()
        || !intents::lag_state(running).is_empty()
        // QoS can be entirely global (maps and profiles with no port
        // touched), so it needs its own terms here.
        || running.qos_maps != intents::QosMapsIntent::default()
        || !running.wred_profiles.is_empty()
        || !intents::port_qos_state(running).is_empty()
        // The security families likewise: CoPP overrides and ACL
        // definitions touch no interface at all.
        || !running.acls.is_empty()
        || !running.copp.is_empty()
        || !intents::port_security_state(running).is_empty()
        || intents::dot1x_state(running) != intents::Dot1xState::default()
        || intents::snoopsec_state(running) != intents::SnoopSecState::default()
        // The services families are orch-owned and can be entirely
        // global (LLDP timers touch no interface at all).
        || intents::lldp_state(running) != intents::LldpState::default()
        || running.snmp != intents::SnmpIntent::default()
        || intents::sflow_state(running) != intents::SflowState::default()
        || !running.dhcp_server.is_empty()
}

/// Why a probed capability set cannot serve this config's sFlow, if it
/// cannot. Kept out of [`Engine::validate`] so the rule can be tested
/// against a capability set directly: getting it wrong means a switch
/// that reports sampling it never does.
fn sflow_capability_error(wanted: &Intents, caps: &pb::SwitchCapabilities) -> Option<String> {
    (wanted.sflow.enabled() && !caps.sflow)
        .then(|| "sflow sampling is not supported by this platform's SAI".to_string())
}

/// The front-panel ports sampling is programmed on: every port the
/// config knows about, minus the ones carrying `sflow disable`. An
/// empty list (or no collector) is how sampling gets torn down.
///
/// Only ports the config mentions are listed, which is deliberate:
/// syncd binds by name, and a port absent from the config is one
/// mgmtd cannot name.
fn sflow_sampled_ports(intents: &Intents) -> Vec<String> {
    if !intents.sflow.enabled() {
        return Vec::new();
    }
    intents
        .ports
        .iter()
        .filter(|(_, port)| !port.sflow_disabled)
        .map(|(name, _)| name.clone())
        .collect()
}

fn qos_maps_request(maps: &intents::QosMapsIntent) -> pb::SetQosMapsRequest {
    let table = |entries: &std::collections::BTreeMap<u8, u8>| -> Vec<pb::QosMapEntry> {
        entries
            .iter()
            .map(|(key, value)| pb::QosMapEntry {
                key: u32::from(*key),
                value: u32::from(*value),
            })
            .collect()
    };
    pb::SetQosMapsRequest {
        dscp_to_tc: table(&maps.dscp_to_tc),
        cos_to_tc: table(&maps.cos_to_tc),
        tc_to_dscp: table(&maps.tc_to_dscp),
        tc_to_cos: table(&maps.tc_to_cos),
    }
}

fn wred_profile_request(name: &str, profile: &intents::WredProfileIntent) -> pb::WredProfile {
    pb::WredProfile {
        name: name.to_string(),
        min_threshold_kb: profile.min_threshold.unwrap_or(0),
        max_threshold_kb: profile.max_threshold.unwrap_or(0),
        drop_probability: profile.drop_probability,
        ecn: profile.ecn,
    }
}

/// One port's whole QoS program as the syncd request.
fn port_qos_request(name: &str, qos: &intents::PortQosIntent) -> pb::SetPortQosRequest {
    pb::SetPortQosRequest {
        port: name.to_string(),
        trust: qos.trust.word().to_string(),
        default_tc: u32::from(qos.default_tc),
        shape_bps: qos.shape,
        queues: qos
            .queues
            .iter()
            .map(|(index, queue)| pb::QueueQos {
                queue: u32::from(*index),
                strict: queue.strict,
                weight: queue.weight.map(u32::from).unwrap_or(0),
                shape_bps: queue.shape,
                wred_profile: queue.wred_profile.clone().unwrap_or_default(),
            })
            .collect(),
    }
}

fn switchport_request(name: &str, sp: &intents::SwitchportIntent) -> pb::SetPortSwitchportRequest {
    pb::SetPortSwitchportRequest {
        name: name.to_string(),
        mode: match sp.mode {
            intents::SwitchportMode::Access => pb::SwitchportMode::Access,
            intents::SwitchportMode::Trunk => pb::SwitchportMode::Trunk,
            intents::SwitchportMode::Dot1qTunnel => pb::SwitchportMode::Dot1qTunnel,
        } as i32,
        access_vlan: sp.access_vlan.map(u32::from).unwrap_or(0),
        trunk_vlans: sp.trunk_vlans.iter().map(|v| u32::from(*v)).collect(),
        native_vlan: sp.native_vlan.map(u32::from).unwrap_or(0),
    }
}

impl Engine {
    /// Re-apply the full running config to syncd. Needed at startup:
    /// syncd boots ports to defaults, so the persisted running config must
    /// be replayed onto it (a restart of either daemon converges).
    pub async fn replay_running(&self) -> Result<usize> {
        let running = Self::parse_intents(&self.store.running()?)?;
        let lag_state = intents::lag_state(&running);
        if !needs_syncd_replay(&running) {
            return Ok(0);
        }
        let mut client = self.syncd_client().await?;
        let mut applied = 0;
        // VLANs first, so switchport replays find them.
        for (id, vlan) in &running.vlans {
            client
                .ensure_vlan(pb::EnsureVlanRequest {
                    id: (*id).into(),
                    name: vlan.description.clone().unwrap_or_default(),
                    suspend: vlan.suspended,
                })
                .await
                .with_context(|| format!("replaying vlan {id}"))?;
            applied += 1;
        }
        for (name, intent) in &running.ports {
            let request = pb::SetPortAttrsRequest {
                name: name.clone(),
                admin_state: intent.admin_up.map(|up| {
                    if up {
                        pb::AdminState::Up as i32
                    } else {
                        pb::AdminState::Down as i32
                    }
                }),
                description: intent.description.clone(),
                // Replay pins the config's own values; nothing to
                // revert at boot, so the "stop forcing" sentinels are
                // never sent here.
                speed_mbps: intent.speed_mbps,
                duplex: intent.duplex.map(|d| d.as_str().to_string()),
                mtu: intent.mtu,
            };
            if request.admin_state.is_some()
                || request.description.is_some()
                || request.speed_mbps.is_some()
                || request.duplex.is_some()
                || request.mtu.is_some()
            {
                client
                    .set_port_attrs(request)
                    .await
                    .with_context(|| format!("replaying config for {name}"))?;
                applied += 1;
            }
            if let Some(address) = &intent.address {
                client
                    .set_interface_address(pb::SetInterfaceAddressRequest {
                        name: name.clone(),
                        address: address.clone(),
                    })
                    .await
                    .with_context(|| format!("replaying address for {name}"))?;
                applied += 1;
            }
            if let Some(sp) = &intent.switchport {
                client
                    .set_port_switchport(switchport_request(name, sp))
                    .await
                    .with_context(|| format!("replaying switchport for {name}"))?;
                applied += 1;
            }
        }
        for (name, intent) in &running.svis {
            if let Some(address) = &intent.address {
                client
                    .set_interface_address(pb::SetInterfaceAddressRequest {
                        name: name.clone(),
                        address: address.clone(),
                    })
                    .await
                    .with_context(|| format!("replaying address for {name}"))?;
                applied += 1;
            }
        }

        // LAGs: syncd objects + switchport, then the protocol push to
        // orch (which reconciles member gates from there).
        if !lag_state.is_empty() {
            for (group, ensure) in &lag_state {
                self.ensure_lag(&mut client, *group, ensure)
                    .await
                    .with_context(|| format!("replaying Port-Channel{group}"))?;
                applied += 1;
            }
        }
        if !lag_state.is_empty() || running.lacp != intents::LacpGlobalIntent::default() {
            self.push_lag_configs(&running)
                .await
                .context("replaying LAG configs to orch")?;
        }

        // Spanning tree: MST instances + the engine config, when the
        // running config carries any STP state.
        let stp_state = intents::stp_state(&running);
        if stp_state != intents::StpState::default() {
            self.apply_stp_instances(&mut client, &intents::StpIntent::default(), &running.stp)
                .await
                .context("replaying mst instances")?;
            self.push_stp_config(&stp_state)
                .await
                .context("replaying STP config to orch")?;
            applied += 1;
        }

        // The DHCP server replays its pools to orch, so the lease view
        // matches the config after a restart of either.
        if !running.dhcp_server.is_empty() {
            self.push_dhcp_server_config(&running)
                .await
                .context("replaying dhcp-server config to orch")?;
            applied += 1;
        }

        // sFlow replays the sampler into the ASIC and the export
        // config into orch, in the same order a commit uses.
        if intents::sflow_state(&running) != intents::SflowState::default() {
            let ports = sflow_sampled_ports(&running);
            client
                .set_sflow_sampling(pb::SetSflowSamplingRequest {
                    rate: if running.sflow.enabled() {
                        running.sflow.rate()
                    } else {
                        0
                    },
                    ports: ports.clone(),
                })
                .await
                .context("replaying sflow sampling")?;
            self.push_sflow_config(&running, &ports)
                .await
                .context("replaying sflow config to orch")?;
            applied += 1;
        }

        // SNMP replays to orch whenever it is configured, so the
        // subagent reopens its AgentX session after a restart.
        if running.snmp != intents::SnmpIntent::default() {
            self.push_snmp_config(&running)
                .await
                .context("replaying snmp config to orch")?;
            applied += 1;
        }

        // LLDP replays to orch whenever the running config says
        // anything about it (an all-defaults config needs no push:
        // that is what the engine already runs).
        let lldp_state = intents::lldp_state(&running);
        if lldp_state != intents::LldpState::default() {
            self.push_lldp_config(&lldp_state)
                .await
                .context("replaying lldp config to orch")?;
            applied += 1;
        }

        // Snooping families with any explicit config replay to orch.
        for (family, snooping) in [
            ("igmp", &running.igmp_snooping),
            ("mld", &running.mld_snooping),
        ] {
            if *snooping != intents::SnoopingIntent::default() {
                self.push_snooping_config(family, snooping)
                    .await
                    .with_context(|| format!("replaying {family}-snooping config to orch"))?;
                applied += 1;
            }
        }

        // The switching-suite families replay as a diff against nothing.
        let empty = Intents::default();
        let mac_changes = intents::diff_mac_table(&empty, &running);
        let storm_changes = intents::diff_storm_control(&empty, &running);
        let mirror_changes = intents::diff_mirror(&empty, &running);
        if !mac_changes.is_empty() || !storm_changes.is_empty() || !mirror_changes.is_empty() {
            applied += mac_changes.add.len()
                + usize::from(mac_changes.aging_time.is_some())
                + storm_changes.len()
                + mirror_changes.len();
            self.apply_switching(
                &mut client,
                &empty,
                &mac_changes,
                &storm_changes,
                &mirror_changes,
            )
            .await
            .context("replaying switching families")?;
        }

        // The security suite: ACL definitions before the bindings that
        // reference them, then CoPP overrides and port-security. syncd
        // holds all of it in memory, so without the replay a syncd
        // restart drops every ACL from the ASIC — a fail-open on an
        // edge filter — until someone commits again.
        for (name, acl) in &running.acls {
            client
                .ensure_acl(acl_request(name, acl))
                .await
                .with_context(|| format!("replaying acl {name}"))?;
            applied += 1;
        }
        // A Port-Channel binding whose members orch has not gated up
        // yet materializes nothing and is re-expanded on the first
        // SetLagMembers, exactly as it is at commit time.
        for ((target, egress), acl) in intents::acl_bindings(&running) {
            client
                .bind_port_acl(pb::BindPortAclRequest {
                    port: target.clone(),
                    stage: acl_stage(egress),
                    acl,
                })
                .await
                .with_context(|| format!("replaying access-group on {target}"))?;
            applied += 1;
        }
        for (class, class_override) in &running.copp {
            client
                .set_copp_class(pb::SetCoppClassRequest {
                    class: class.clone(),
                    rate: class_override.rate,
                    burst: class_override.burst,
                })
                .await
                .with_context(|| format!("replaying copp class {class}"))?;
            applied += 1;
        }
        for (port, security) in intents::port_security_state(&running) {
            client
                .set_port_security(pb::SetPortSecurityRequest {
                    port: port.clone(),
                    maximum: security.maximum,
                    shutdown: security.shutdown,
                })
                .await
                .with_context(|| format!("replaying port-security on {port}"))?;
            applied += 1;
        }

        // dot1x and snooping/DAI are whole-state pushes to their orch
        // engines, which drive the enforcement back into syncd — the
        // same shape as the STP and IGMP/MLD replays above.
        let dot1x = intents::dot1x_state(&running);
        if dot1x != intents::Dot1xState::default() {
            self.push_dot1x_config(&dot1x)
                .await
                .context("replaying dot1x config to orch")?;
            applied += 1;
        }
        let snoopsec = intents::snoopsec_state(&running);
        if snoopsec != intents::SnoopSecState::default() {
            self.push_snoopsec_config(&snoopsec)
                .await
                .context("replaying dhcp-snooping/arp-inspection config to orch")?;
            applied += 1;
        }

        // QoS: profiles before the port programs that reference them,
        // and the global maps before the ports whose trust mode reads
        // them — the same ordering the commit applier uses. syncd holds
        // this state in memory only, so without the replay a syncd
        // restart would leave the ASIC unclassified until the next
        // commit.
        for (name, profile) in &running.wred_profiles {
            client
                .ensure_wred_profile(pb::EnsureWredProfileRequest {
                    profile: Some(wred_profile_request(name, profile)),
                })
                .await
                .with_context(|| format!("replaying qos wred-profile {name}"))?;
            applied += 1;
        }
        if running.qos_maps != intents::QosMapsIntent::default() {
            client
                .set_qos_maps(qos_maps_request(&running.qos_maps))
                .await
                .context("replaying qos maps")?;
            applied += 1;
        }
        for (port, qos) in intents::port_qos_state(&running) {
            client
                .set_port_qos(port_qos_request(&port, &qos))
                .await
                .with_context(|| format!("replaying qos for {port}"))?;
            applied += 1;
        }

        // VRRP virtual MACs back into the ASIC's My-MAC table.
        for (vlan, mac) in vrrp_my_macs(&running) {
            client
                .ensure_my_mac(pb::EnsureMyMacRequest { vlan, mac })
                .await
                .context("replaying VRRP virtual MACs")?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Replay the OS-side families (management address, static routes,
    /// sshd state) from the running config. Independent of syncd, so it
    /// runs once at startup rather than inside the syncd retry loop.
    /// FRR replays through the same idempotent apply — after the OS
    /// pass, so the VRRP macvlans exist before the reload.
    pub fn replay_os(&self) -> Result<()> {
        let running = Self::parse_intents(&self.store.running()?)?;
        self.os.replay(&running);
        self.frr.apply(&running);
        self.snmp.apply(&running);
        self.dnsmasq.apply(&running);
        Ok(())
    }
}

pub struct MgmtService {
    engine: SharedEngine,
}

impl MgmtService {
    pub fn new(engine: SharedEngine) -> Self {
        Self { engine }
    }
}

fn internal(err: anyhow::Error) -> Status {
    Status::internal(format!("{err:#}"))
}

#[tonic::async_trait]
impl pb::mgmt_server::Mgmt for MgmtService {
    async fn get_config(
        &self,
        request: Request<pb::GetConfigRequest>,
    ) -> Result<Response<pb::ConfigText>, Status> {
        let engine = self.engine.lock().await;
        let text = match request.into_inner().source {
            s if s == pb::ConfigSource::Candidate as i32 => engine.store.candidate(),
            _ => engine.store.running(),
        }
        .map_err(internal)?;
        Ok(Response::new(pb::ConfigText { text }))
    }

    async fn set_candidate(
        &self,
        request: Request<pb::ConfigText>,
    ) -> Result<Response<pb::SetCandidateResponse>, Status> {
        let text = request.into_inner().text;
        let engine = self.engine.lock().await;
        match engine.validate(&text).await {
            Ok(()) => {
                engine.store.set_candidate(&text).map_err(internal)?;
                Ok(Response::new(pb::SetCandidateResponse {
                    valid: true,
                    errors: vec![],
                }))
            }
            Err(err) => Ok(Response::new(pb::SetCandidateResponse {
                valid: false,
                errors: vec![format!("{err:#}")],
            })),
        }
    }

    async fn commit(
        &self,
        request: Request<pb::CommitRequest>,
    ) -> Result<Response<pb::CommitResponse>, Status> {
        let req = request.into_inner();
        let mut engine = self.engine.lock().await;

        // A new commit while a confirm is pending supersedes it.
        if let Some(pending) = engine.pending_confirm.take() {
            let _ = pending.cancel.send(());
            warn!("pending commit-confirm superseded by a new commit");
        }

        let candidate = engine.store.candidate().map_err(internal)?;
        engine
            .validate(&candidate)
            .await
            .map_err(|e| Status::failed_precondition(format!("candidate invalid: {e:#}")))?;

        let pre_commit_running = engine.store.running().map_err(internal)?;
        let changes = engine
            .apply_and_persist(&candidate, &req.comment)
            .await
            .map_err(internal)?;
        let commit_id = engine.commit_seq;
        info!(commit_id, changes = changes.len(), "committed");

        if req.confirm_timeout_secs > 0 {
            let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
            engine.pending_confirm = Some(PendingConfirm { cancel: cancel_tx });
            let shared = self.engine.clone();
            let timeout = Duration::from_secs(req.confirm_timeout_secs.into());
            tokio::spawn(async move {
                tokio::select! {
                    _ = cancel_rx => {} // confirmed or superseded
                    _ = tokio::time::sleep(timeout) => {
                        let mut engine = shared.lock().await;
                        if engine.pending_confirm.take().is_none() {
                            return; // raced with a confirm
                        }
                        warn!("commit-confirm window expired; rolling back");
                        match engine
                            .apply_and_persist(&pre_commit_running, "auto-rollback (commit-confirm expired)")
                            .await
                        {
                            Ok(_) => info!("auto-rollback complete"),
                            Err(err) => warn!(%err, "auto-rollback FAILED"),
                        }
                    }
                }
            });
        }

        Ok(Response::new(pb::CommitResponse {
            commit_id,
            applied_changes: changes,
        }))
    }

    async fn confirm_commit(
        &self,
        _request: Request<pb::ConfirmCommitRequest>,
    ) -> Result<Response<pb::ConfirmCommitResponse>, Status> {
        let mut engine = self.engine.lock().await;
        let was_pending = match engine.pending_confirm.take() {
            Some(pending) => {
                let _ = pending.cancel.send(());
                true
            }
            None => false,
        };
        Ok(Response::new(pb::ConfirmCommitResponse { was_pending }))
    }

    async fn rollback(
        &self,
        request: Request<pb::RollbackRequest>,
    ) -> Result<Response<pb::RollbackResponse>, Status> {
        let n = request.into_inner().revisions_back;
        let engine = self.engine.lock().await;
        let text = engine
            .store
            .rollback(n)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no rollback point {n}")))?;
        engine.store.set_candidate(&text).map_err(internal)?;
        Ok(Response::new(pb::RollbackResponse { loaded_text: text }))
    }

    async fn list_rollbacks(
        &self,
        _request: Request<pb::ListRollbacksRequest>,
    ) -> Result<Response<pb::ListRollbacksResponse>, Status> {
        let engine = self.engine.lock().await;
        Ok(Response::new(pb::ListRollbacksResponse {
            entries: engine
                .store
                .list_rollbacks()
                .into_iter()
                .map(|(n, meta)| pb::RollbackEntry {
                    revisions_back: n,
                    committed_at: meta.committed_at,
                    comment: meta.comment,
                })
                .collect(),
        }))
    }

    async fn discard(
        &self,
        _request: Request<pb::DiscardRequest>,
    ) -> Result<Response<pb::DiscardResponse>, Status> {
        let engine = self.engine.lock().await;
        engine.store.discard_candidate().map_err(internal)?;
        Ok(Response::new(pb::DiscardResponse {}))
    }

    async fn install_image(
        &self,
        request: Request<pb::InstallImageRequest>,
    ) -> Result<Response<pb::InstallImageResponse>, Status> {
        let req = request.into_inner();
        // Hold the engine lock for the duration: no commits land while
        // the OS image underneath them is being swapped.
        let _engine = self.engine.lock().await;
        info!(path = %req.path, force = req.force, "installing os image");
        let path = std::path::PathBuf::from(&req.path);
        let header =
            tokio::task::spawn_blocking(move || hemlock_common::image::install(&path, req.force))
                .await
                .map_err(|e| Status::internal(format!("install task failed: {e}")))?
                .map_err(Status::failed_precondition)?;
        if req.reboot {
            // Grace period so the response reaches the caller first.
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(750)).await;
                info!("rebooting into the new image");
                let _ = tokio::process::Command::new("systemctl")
                    .arg("reboot")
                    .status()
                    .await;
            });
        }
        Ok(Response::new(pb::InstallImageResponse {
            version: header.version,
            platform: header.platform,
        }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn intents_of(text: &str) -> Intents {
        Engine::parse_intents(text).unwrap()
    }

    /// The boot replay's gate: a config carrying any syncd-owned family
    /// must reach syncd after a restart. QoS can be entirely global, so
    /// maps and profiles count on their own — miss that and a restart
    /// leaves the ASIC unclassified until the next commit.
    #[test]
    fn qos_only_configs_still_replay_to_syncd() {
        assert!(!needs_syncd_replay(&Intents::default()));
        assert!(!needs_syncd_replay(&intents_of("system { ssh { } }")));

        // Global maps alone, no port touched.
        assert!(needs_syncd_replay(&intents_of(
            "qos { map { dscp-to-tc { dscp 46 tc 5 } } }"
        )));
        // A profile alone, likewise.
        assert!(needs_syncd_replay(&intents_of(
            "qos { wred-profile BULK { min-threshold 64\nmax-threshold 256 } }"
        )));
        // A Port-Channel program with no member ports configured.
        assert!(needs_syncd_replay(&intents_of(
            "interfaces { Port-Channel1 { qos { trust dscp } } }"
        )));
        // And the families that already gated it still do.
        assert!(needs_syncd_replay(&intents_of(
            "interfaces { Ethernet1 { description uplink } }"
        )));
        assert!(needs_syncd_replay(&intents_of("vlans { vlan 10 { } }")));
    }

    /// The security families are syncd-owned too, and an ACL or CoPP
    /// override can exist without touching any interface — so they gate
    /// the replay on their own. Missing this fails *open*: a syncd
    /// restart would drop every ACL from the ASIC.
    #[test]
    fn security_only_configs_still_replay_to_syncd() {
        assert!(needs_syncd_replay(&intents_of(
            "security { acl { ipv4 EDGE-IN { rule 10 { deny } } } }"
        )));
        assert!(needs_syncd_replay(&intents_of(
            "security { copp { class bpdu { rate 512 } } }"
        )));
        assert!(needs_syncd_replay(&intents_of(
            "security { dhcp-snooping { vlan 10 }
arp-inspection { vlan 10 } }"
        )));
    }

    /// Every syncd- or orch-owned family in a full config reaches the
    /// gate. A family that lands in `Intents` but never in
    /// `replay_running` is invisible until the next commit, so this
    /// pins the ones that are wired up.
    #[test]
    fn a_full_config_gates_the_replay() {
        let text = concat!(
            "vlans { vlan 10 { } }
",
            "interfaces {
",
            "  Ethernet1 { switchport { mode trunk
trunk vlans 10 }
",
            "    access-group EDGE-IN in
",
            "    qos { trust dscp
queue 7 { priority strict } } }
",
            "  Ethernet5 { port-security { maximum 4 } }
",
            "  Ethernet10 { dot1x }
",
            "  Management1 { address 10.42.0.9/24 }
",
            "}
",
            "security {
",
            "  acl { ipv4 EDGE-IN { rule 10 { deny } } }
",
            "  copp { class bpdu { rate 512 } }
",
            "  dot1x { radius-server 10.42.0.5 { key \"s3cret\" } }
",
            "  dhcp-snooping { vlan 10 }
",
            "}
",
            "qos { map { dscp-to-tc { dscp 46 tc 5 } } }
",
            "services { lldp { tx-interval 15 }
snmp { community public }
sflow { collector 10.42.0.20 } }
",
        );
        let running = intents_of(text);
        assert!(needs_syncd_replay(&running));
        // The pieces the replay walks are all present in the intents.
        assert!(!running.acls.is_empty());
        assert!(!intents::acl_bindings(&running).is_empty());
        assert!(!running.copp.is_empty());
        assert!(!intents::port_security_state(&running).is_empty());
        assert_ne!(
            intents::dot1x_state(&running),
            intents::Dot1xState::default()
        );
        assert_ne!(
            intents::snoopsec_state(&running),
            intents::SnoopSecState::default()
        );
        assert_ne!(running.qos_maps, intents::QosMapsIntent::default());
        assert!(!intents::port_qos_state(&running).is_empty());
        assert_ne!(intents::lldp_state(&running), intents::LldpState::default());
        assert_ne!(running.snmp, intents::SnmpIntent::default());
        assert_ne!(
            intents::sflow_state(&running),
            intents::SflowState::default()
        );
        // Every front-panel port in the config samples; none is
        // disabled, so the list is the whole set.
        assert_eq!(sflow_sampled_ports(&running).len(), running.ports.len());
    }

    /// LLDP is orch-owned and entirely global-capable, so it gates the
    /// replay on its own — a restart must not silently revert the
    /// timers to the engine defaults.
    #[test]
    fn services_only_configs_still_replay() {
        assert!(needs_syncd_replay(&intents_of(
            "services { lldp { disable } }"
        )));
        assert!(needs_syncd_replay(&intents_of(
            "interfaces { Ethernet3 { lldp disable } }"
        )));
        assert!(needs_syncd_replay(&intents_of(
            "interfaces { Management1 { address 10.42.0.9/24 } }
services { snmp { community public } }"
        )));
        assert!(needs_syncd_replay(&intents_of(
            "services { sflow { collector 10.42.0.20 } }"
        )));
    }

    /// A platform whose SAI serves no samplepacket objects fails the
    /// commit with the platform error rather than sampling nothing.
    #[test]
    fn sflow_without_the_capability_fails_the_commit() {
        let with_sflow = intents_of("services { sflow { collector 10.42.0.20 } }");
        let without = intents_of("");
        let caps = |sflow| pb::SwitchCapabilities {
            sflow,
            ..pb::SwitchCapabilities::default()
        };
        assert_eq!(
            sflow_capability_error(&with_sflow, &caps(false)).as_deref(),
            Some("sflow sampling is not supported by this platform's SAI")
        );
        // A platform that can sample, and a config that does not ask
        // to, both pass.
        assert_eq!(sflow_capability_error(&with_sflow, &caps(true)), None);
        assert_eq!(sflow_capability_error(&without, &caps(false)), None);
        // A rate with no collector is not "enabled", so it is refused
        // by the intent extractor rather than by the capability gate.
        assert!(
            hemlock_config::parse("services { sflow { sample-rate 4096 } }")
                .map(|tree| intents::extract(&tree))
                .unwrap()
                .is_err()
        );
    }

    /// Every services family across a commit-confirm cycle: the same
    /// gate decisions `apply_and_persist` makes for the commit that
    /// adds the family, and for the auto-rollback that removes it.
    ///
    /// The rollback half is the one that matters: an applier that only
    /// fires on "config appeared" would leave snmpd and dnsmasq running
    /// after the window expired, serving a config the box no longer has.
    #[test]
    fn services_families_apply_and_roll_back() {
        let empty = intents_of("");
        let full = intents_of(concat!(
            "vlans { vlan 99 { } }\n",
            "interfaces {\n",
            "  Management1 { address 10.42.0.9/24 }\n",
            "  Ethernet3 { lldp disable }\n",
            "  Ethernet4 { sflow disable }\n",
            "  Vlan99 { address 10.42.10.9/24\n",
            "    dhcp-relay server 10.42.0.5 }\n",
            "}\n",
            "services {\n",
            "  lldp { tx-interval 15 }\n",
            "  ntp { server 10.42.0.5 }\n",
            "  snmp { community public }\n",
            "  sflow { collector 10.42.0.20 }\n",
            "  dhcp-server { pool LAN-USERS {\n",
            "    network 10.0.10.0/24\n",
            "    range 10.0.10.100 10.0.10.200\n",
            "    default-gateway 10.0.10.1 } }\n",
            "}\n",
        ));

        // --- the commit: every family reports a change ---
        assert!(intents::diff_lldp(&empty, &full).is_some());
        assert!(intents::diff_os(&empty, &full).ntp.is_some());
        assert!(intents::diff_sflow(&empty, &full).is_some());
        assert!(intents::diff_snoopsec(&empty, &full).is_some());
        assert_ne!(empty.snmp, full.snmp);
        assert_ne!(empty.dhcp_server, full.dhcp_server);
        // ...and the two rendered services really do render.
        assert!(crate::snmpapply::render_snmpd(&full).is_some());
        assert!(crate::dnsmasqapply::render_dnsmasq(&full).is_some());
        // The sampler is programmed on every port but the disabled one.
        assert_eq!(sflow_sampled_ports(&full), vec!["Ethernet3".to_string()]);

        // --- the auto-rollback: every family reports the way back ---
        let back = intents::diff_lldp(&full, &empty).expect("lldp rolls back");
        assert_eq!(back, intents::LldpState::default());
        assert_eq!(
            intents::diff_os(&full, &empty).ntp,
            Some(intents::NtpIntent::default()),
            "no servers means timesyncd stops"
        );
        let back = intents::diff_sflow(&full, &empty).expect("sflow rolls back");
        assert!(!back.global.enabled());
        assert!(
            sflow_sampled_ports(&empty).is_empty(),
            "an empty port list is how the sampler is torn down"
        );
        let back = intents::diff_snoopsec(&full, &empty).expect("the relay rolls back");
        assert!(back.relay.is_empty());

        // The two render-gated services stop outright: the render is
        // None, which is what makes the applier disable the unit.
        assert!(crate::snmpapply::render_snmpd(&empty).is_none());
        assert!(crate::dnsmasqapply::render_dnsmasq(&empty).is_none());
        // ...and the gate that decides whether to apply at all fires,
        // so `None` actually reaches the applier.
        assert_ne!(
            crate::snmpapply::render_snmpd(&full),
            crate::snmpapply::render_snmpd(&empty)
        );
        assert_ne!(
            crate::dnsmasqapply::render_dnsmasq(&full),
            crate::dnsmasqapply::render_dnsmasq(&empty)
        );

        // A commit that touches none of them leaves every service
        // alone — an unrelated change must not bounce snmpd or dnsmasq.
        let unrelated = intents_of(&format!(
            "{}\nswitching {{ mac-table {{ aging-time 600 }} }}",
            "vlans { vlan 99 { } }"
        ));
        let unrelated_twice = intents_of(&format!(
            "{}\nswitching {{ mac-table {{ aging-time 600 }} }}",
            "vlans { vlan 99 { } vlan 100 { } }"
        ));
        assert_eq!(
            crate::snmpapply::render_snmpd(&unrelated),
            crate::snmpapply::render_snmpd(&unrelated_twice)
        );
        assert_eq!(
            crate::dnsmasqapply::render_dnsmasq(&unrelated),
            crate::dnsmasqapply::render_dnsmasq(&unrelated_twice)
        );
        assert_eq!(intents::diff_lldp(&unrelated, &unrelated_twice), None);
        assert_eq!(intents::diff_sflow(&unrelated, &unrelated_twice), None);
    }

    /// The sampled-port list is every configured front-panel port
    /// minus the ones carrying `sflow disable` — and it is empty
    /// whenever sFlow is off, which is how the sampler gets torn down.
    #[test]
    fn sflow_sampled_ports_exclude_disabled_ones() {
        let intents = intents_of(
            "services { sflow { collector 10.42.0.20 } }
interfaces { Ethernet1 { } Ethernet2 { } Ethernet4 { sflow disable } }",
        );
        assert_eq!(
            sflow_sampled_ports(&intents),
            vec!["Ethernet1".to_string(), "Ethernet2".to_string()]
        );
        // No collector = nothing sampled, whatever the ports say.
        let intents = intents_of("interfaces { Ethernet1 { } }");
        assert!(sflow_sampled_ports(&intents).is_empty());
    }
}
