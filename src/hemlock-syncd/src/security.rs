//! The security suite's syncd engine: user ACLs, the internal entries
//! other features express through the same machinery (dot1x port
//! enforcement, DHCP-snooping/DAI CPU redirects), CoPP's compiled class
//! table over hostif trap groups, and port-security learn limits.
//!
//! # ACL materialization
//!
//! Every (physical port, stage) with anything to filter gets exactly
//! one hardware table. A Port-Channel binding expands to the member
//! ports (and follows membership churn). The table's single priority
//! space is partitioned into bands — higher wins:
//!
//! - **internal band** (`2_000_000_000 - seq`): dot1x enforcement
//!   first, then snooping/DAI redirects. A user rule can never shadow
//!   these.
//! - **user band** (`1_000_000_000 - ordinal`): the bound ACL's rules
//!   in rule-number order.
//! - **implicit deny** (priority 1): present whenever a user ACL is
//!   bound, with its own match counter.
//!
//! Re-materialization diffs by entry key, so editing one rule leaves
//! the other rules' entries — and their counter objects — untouched.
//!
//! Tables carry the match-field set of the bound ACL's family (IPv4
//! when only internal entries exist). Internal entries that need IPv4
//! matching (the DHCP redirects) cannot ride an IPv6 or MAC table;
//! such a combination fails the operation with a clear error instead
//! of silently filtering wrong.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use hemlock_common::proto::v1 as pb;
use hemlock_sai::{
    AclAction, AclFamily, AclFields, AclPacketAction, AclStage, Oid, PolicerSpec, TrapKind,
};
use tonic::Status;
use tracing::{info, warn};

use crate::actor::SaiHandle;
use crate::service::{lag_group_of, SyncdService};
use crate::state::{
    AclEntryKey, AclEntryObjs, AclProgram, AclRuleState, CoppClassState, InternalAcl,
    InternalAclEntry, PortAclTable, PortSecurityState,
};

const INTERNAL_PRIORITY_TOP: u32 = 2_000_000_000;
const USER_PRIORITY_TOP: u32 = 1_000_000_000;
const IMPLICIT_DENY_PRIORITY: u32 = 1;

/// One compiled CoPP class: fixed trap membership and default policer
/// rate/burst (pps / packets). Classes are not user-definable; config
/// can only override a class's rate and burst.
pub struct CoppClassDef {
    pub name: &'static str,
    pub rate: u32,
    pub burst: u32,
    /// (trap, punt-only). Punt-only traps stop hardware forwarding;
    /// copies forward normally and deliver a CPU copy.
    pub traps: &'static [(TrapKind, bool)],
}

/// The compiled class table, in `show copp` display order. The
/// `default` class owns no trap group: its policer rides the switch's
/// default trap group, policing everything not steered into a class.
pub const COPP_CLASSES: &[CoppClassDef] = &[
    CoppClassDef {
        name: "bpdu",
        rate: 512,
        burst: 128,
        traps: &[(TrapKind::Stp, true)],
    },
    CoppClassDef {
        name: "lacp",
        rate: 1000,
        burst: 256,
        traps: &[(TrapKind::Lacp, true)],
    },
    CoppClassDef {
        name: "eapol",
        rate: 256,
        burst: 64,
        traps: &[(TrapKind::Eapol, true)],
    },
    CoppClassDef {
        name: "igmp",
        rate: 1000,
        burst: 256,
        traps: &[
            (TrapKind::IgmpQuery, false),
            (TrapKind::IgmpLeave, false),
            (TrapKind::IgmpV1Report, false),
            (TrapKind::IgmpV2Report, false),
            (TrapKind::IgmpV3Report, false),
        ],
    },
    CoppClassDef {
        name: "mld",
        rate: 1000,
        burst: 256,
        traps: &[
            (TrapKind::MldV1V2, false),
            (TrapKind::MldV1Report, false),
            (TrapKind::MldV1Done, false),
            (TrapKind::MldV2Report, false),
        ],
    },
    CoppClassDef {
        name: "arp",
        rate: 2000,
        burst: 500,
        traps: &[
            (TrapKind::ArpRequest, false),
            (TrapKind::ArpResponse, false),
        ],
    },
    CoppClassDef {
        // A copy, not a trap: DHCP keeps forwarding in hardware; the
        // snooping redirects (internal ACL entries) override that on
        // snooped VLANs' ports.
        name: "dhcp",
        rate: 512,
        burst: 128,
        traps: &[(TrapKind::Dhcp, false)],
    },
    CoppClassDef {
        name: "ospf",
        rate: 2000,
        burst: 512,
        traps: &[(TrapKind::Ospf, true)],
    },
    CoppClassDef {
        name: "bgp",
        rate: 2000,
        burst: 512,
        traps: &[(TrapKind::Bgp, true)],
    },
    CoppClassDef {
        name: "vrrp",
        rate: 512,
        burst: 128,
        traps: &[(TrapKind::Vrrp, true)],
    },
    CoppClassDef {
        name: "ip2me",
        rate: 4000,
        burst: 1024,
        traps: &[(TrapKind::Ip2me, true)],
    },
    CoppClassDef {
        name: "acl-log",
        rate: 64,
        burst: 32,
        traps: &[(TrapKind::AclLog, true)],
    },
    CoppClassDef {
        name: "default",
        rate: 256,
        burst: 64,
        traps: &[],
    },
];

pub fn copp_class(name: &str) -> Option<&'static CoppClassDef> {
    COPP_CLASSES.iter().find(|c| c.name == name)
}

/// Program the compiled CoPP table at startup: one policer + trap
/// group per class with its member traps, and the `default` class's
/// policer on the switch default trap group. Without trap-group
/// support the ARP/IP2ME punt traps still install (unpoliced, into
/// the default group) so L3 host services keep working.
pub async fn program_copp(handle: &Arc<SaiHandle>) -> anyhow::Result<()> {
    if !handle.capabilities.copp {
        for (kind, trap_only) in [
            (TrapKind::ArpRequest, false),
            (TrapKind::ArpResponse, false),
            (TrapKind::Ip2me, true),
        ] {
            handle.create_hostif_trap(kind, trap_only, Oid(0)).await?;
        }
        info!("CoPP unsupported; ARP/IP2ME punt traps installed unpoliced");
        return Ok(());
    }
    for class in COPP_CLASSES {
        let policer = handle
            .create_policer(PolicerSpec {
                pps: true,
                rate: u64::from(class.rate),
                burst: u64::from(class.burst),
            })
            .await?;
        let group = if class.name == "default" {
            handle.set_default_trap_group_policer(Some(policer)).await?;
            None
        } else {
            Some(handle.create_hostif_trap_group(Some(policer)).await?)
        };
        let mut traps = Vec::new();
        if let Some(group) = group {
            for (kind, trap_only) in class.traps {
                traps.push(handle.create_hostif_trap(*kind, *trap_only, group).await?);
            }
        }
        let mut copp = handle
            .copp
            .write()
            .map_err(|_| anyhow::anyhow!("copp table poisoned"))?;
        copp.classes.insert(
            class.name,
            CoppClassState {
                rate: class.rate,
                burst: class.burst,
                overridden: false,
                policer: Some(policer),
                group,
                traps,
                base: Default::default(),
            },
        );
    }
    info!(classes = COPP_CLASSES.len(), "CoPP class table programmed");
    Ok(())
}

/// The port-security engine: watch FDB learns and learn-limit
/// violations, maintain each secured port's learned set, and apply the
/// violation action (protect = freeze learning; shutdown = errdisable).
pub async fn port_security_watch(handle: Arc<SaiHandle>) {
    let mut fdb = handle.fdb_events.subscribe();
    let mut violations = handle.violations.subscribe();
    loop {
        tokio::select! {
            event = fdb.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let Some(port) = event.port else { continue };
                match event.kind {
                    hemlock_sai::FdbEventKind::Learned | hemlock_sai::FdbEventKind::Moved => {
                        let over_limit = {
                            let Ok(mut table) = handle.port_security.write() else { continue };
                            let Some(state) = table.get_mut(&port) else { continue };
                            if state.learned.contains_key(&event.mac) {
                                false
                            } else if (state.learned.len() as u32) < state.max {
                                state.learned.insert(event.mac.clone(), Instant::now());
                                false
                            } else {
                                true
                            }
                        };
                        if over_limit {
                            apply_violation(&handle, &port, &event.mac).await;
                        }
                    }
                    hemlock_sai::FdbEventKind::Aged | hemlock_sai::FdbEventKind::Flushed => {
                        if let Ok(mut table) = handle.port_security.write() {
                            if let Some(state) = table.get_mut(&port) {
                                state.learned.remove(&event.mac);
                            }
                        }
                    }
                }
            }
            event = violations.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                apply_violation(&handle, &event.port, &event.mac).await;
            }
        }
    }
}

/// Record a violation and enforce the port's configured action.
async fn apply_violation(handle: &SaiHandle, port: &str, mac: &str) {
    let shutdown = {
        let Ok(mut table) = handle.port_security.write() else {
            return;
        };
        let Some(state) = table.get_mut(port) else {
            return;
        };
        state.violations += 1;
        state.last_violation = Some((mac.to_string(), Instant::now()));
        state.shutdown
    };
    let sai_id = {
        let Ok(ports) = handle.ports.read() else {
            return;
        };
        let Some(state) = ports.get(port) else { return };
        state.sai_id
    };
    if shutdown {
        if let Err(err) = handle.set_admin_state(sai_id, false).await {
            warn!(%port, %err, "port-security shutdown failed");
            return;
        }
        if let Ok(mut ports) = handle.ports.write() {
            if let Some(state) = ports.get_mut(port) {
                state.admin_up = false;
                state.errdisable_reason = Some("port-security".into());
            }
        }
        warn!(%port, %mac, "port-security violation: errdisabled");
    } else {
        if let Err(err) = handle.set_port_learning(sai_id, false).await {
            warn!(%port, %err, "port-security protect (learning off) failed");
        }
        warn!(%port, %mac, "port-security violation: learning frozen (protect)");
    }
}

/// One entry a (port, stage) table should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredEntry {
    key: AclEntryKey,
    priority: u32,
    fields: AclFields,
    action: AclPacketAction,
    police: Option<PolicerSpec>,
    wants_counter: bool,
}

/// What a (port, stage) should materialize to.
struct Desired {
    family: AclFamily,
    user_acl: Option<String>,
    entries: Vec<DesiredEntry>,
}

/// The packet action of a user rule.
fn user_action(permit: bool, log: bool) -> AclPacketAction {
    match (permit, log) {
        (true, false) => AclPacketAction::Forward,
        (true, true) => AclPacketAction::Copy,
        (false, false) => AclPacketAction::Drop,
        (false, true) => AclPacketAction::Trap,
    }
}

/// Whether an internal entry's fields fit a table family. IPv4 tables
/// carry every field the internal entries use; IPv6/MAC tables carry
/// only ethertype + VLAN (+ MACs), so the DHCP redirects don't fit.
fn fits_family(fields: &AclFields, family: AclFamily) -> bool {
    match family {
        AclFamily::Ipv4 => true,
        AclFamily::Ipv6 | AclFamily::Mac => {
            fields.src_ip.is_none()
                && fields.dst_ip.is_none()
                && fields.protocol.is_none()
                && fields.src_port.is_none()
                && fields.dst_port.is_none()
                && fields.dscp.is_none()
        }
    }
}

/// Compute the desired program of one (port, stage). `Ok(None)` =
/// nothing to filter (no table). `Err` = a family conflict, reported
/// with the operator-facing message.
fn desired_entries(
    user: Option<(&str, &AclProgram)>,
    internal: Option<&InternalAcl>,
) -> Result<Option<Desired>, String> {
    let internal_entries: Vec<&InternalAclEntry> =
        internal.map(|i| i.ordered().collect()).unwrap_or_default();
    if user.is_none() && internal_entries.is_empty() {
        return Ok(None);
    }
    let family = user.map(|(_, p)| p.family).unwrap_or(AclFamily::Ipv4);
    let mut entries = Vec::new();
    for (seq, entry) in internal_entries.iter().enumerate() {
        if !fits_family(&entry.fields, family) {
            let (name, _) = user.expect("conflicts require a bound ACL");
            return Err(format!(
                "ACL {name} cannot share the port with DHCP snooping on this platform \
                 (the snooping redirect needs IPv4 matching)"
            ));
        }
        entries.push(DesiredEntry {
            key: AclEntryKey::Internal(seq as u32),
            priority: INTERNAL_PRIORITY_TOP - seq as u32,
            fields: entry.fields.clone(),
            action: entry.action,
            police: None,
            wants_counter: false,
        });
    }
    if let Some((name, program)) = user {
        for (ordinal, (number, rule)) in program.rules.iter().enumerate() {
            entries.push(DesiredEntry {
                key: AclEntryKey::User(*number),
                priority: USER_PRIORITY_TOP - ordinal as u32,
                fields: rule.fields.clone(),
                action: user_action(rule.permit, rule.log),
                police: rule.police,
                wants_counter: true,
            });
        }
        entries.push(DesiredEntry {
            key: AclEntryKey::ImplicitDeny,
            priority: IMPLICIT_DENY_PRIORITY,
            fields: AclFields::default(),
            action: AclPacketAction::Drop,
            police: None,
            wants_counter: true,
        });
        return Ok(Some(Desired {
            family,
            user_acl: Some(name.to_string()),
            entries,
        }));
    }
    Ok(Some(Desired {
        family,
        user_acl: None,
        entries,
    }))
}

// gRPC helpers naturally speak `Status`; its size is tonic's business.
#[allow(clippy::result_large_err)]
impl SyncdService {
    fn sai_err(e: hemlock_sai::SaiError) -> Status {
        Status::internal(format!("SAI: {e}"))
    }

    /// The physical ports behind a binding target: a port itself, or a
    /// Port-Channel's current members.
    pub(crate) fn physical_targets(&self, name: &str) -> Result<Vec<String>, Status> {
        if let Some(group) = lag_group_of(name) {
            let lags = self
                .handle
                .lags
                .read()
                .map_err(|_| Status::internal("lag table poisoned"))?;
            let lag = lags.get(&group).ok_or_else(|| {
                Status::failed_precondition(format!("Port-Channel{group} not created"))
            })?;
            return Ok(lag.members.keys().cloned().collect());
        }
        let ports = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        if !ports.contains_key(name) {
            return Err(Status::not_found(format!("no such interface {name:?}")));
        }
        Ok(vec![name.to_string()])
    }

    /// The user ACL a physical port resolves to at a stage: a direct
    /// binding, or its Port-Channel's.
    fn resolved_user_acl(&self, port: &str, stage: AclStage) -> Result<Option<String>, Status> {
        let world = self
            .handle
            .acls
            .read()
            .map_err(|_| Status::internal("acl table poisoned"))?;
        if let Some(acl) = world.bindings.get(&(port.to_string(), stage)) {
            return Ok(Some(acl.clone()));
        }
        let lags = self
            .handle
            .lags
            .read()
            .map_err(|_| Status::internal("lag table poisoned"))?;
        for lag in lags.values() {
            if lag.members.contains_key(port) {
                let name = format!("Port-Channel{}", lag.group);
                if let Some(acl) = world.bindings.get(&(name, stage)) {
                    return Ok(Some(acl.clone()));
                }
            }
        }
        Ok(None)
    }

    /// Re-materialize one (physical port, stage) table to match the
    /// current bindings + internal entries.
    pub(crate) async fn apply_port_stage(&self, port: &str, stage: AclStage) -> Result<(), Status> {
        let user_acl = self.resolved_user_acl(port, stage)?;
        // Snapshot the inputs and the current table under read locks.
        #[allow(clippy::type_complexity)]
        let (desired, current): (
            Option<Desired>,
            Option<(Oid, AclFamily, Vec<(AclEntryKey, AclEntryObjs)>)>,
        ) = {
            let world = self
                .handle
                .acls
                .read()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            let user = match &user_acl {
                Some(name) => {
                    let program = world.acls.get(name).ok_or_else(|| {
                        Status::failed_precondition(format!("no such ACL {name:?}"))
                    })?;
                    Some((name.as_str(), program))
                }
                None => None,
            };
            let internal = world.internal.get(&(port.to_string(), stage));
            let desired = desired_entries(user, internal).map_err(Status::failed_precondition)?;
            let current = world.tables.get(&(port.to_string(), stage)).map(|t| {
                (
                    t.table,
                    t.family,
                    t.entries
                        .iter()
                        .map(|(k, v)| (*k, v.clone()))
                        .collect::<Vec<_>>(),
                )
            });
            (desired, current)
        };
        let port_id = self.port_sai_id(port)?;

        // Nothing wanted: tear the whole table down.
        let Some(desired) = desired else {
            if let Some((table, _, entries)) = current {
                self.handle
                    .bind_port_acl(port_id, stage, None)
                    .await
                    .map_err(Self::sai_err)?;
                for (_, objs) in &entries {
                    self.destroy_entry_objs(objs).await?;
                }
                self.handle
                    .remove_acl_table(table)
                    .await
                    .map_err(Self::sai_err)?;
                let mut world = self
                    .handle
                    .acls
                    .write()
                    .map_err(|_| Status::internal("acl table poisoned"))?;
                world.tables.remove(&(port.to_string(), stage));
            }
            return Ok(());
        };

        // A family change is a rebuild.
        let current = match current {
            Some((table, family, entries)) if family != desired.family => {
                self.handle
                    .bind_port_acl(port_id, stage, None)
                    .await
                    .map_err(Self::sai_err)?;
                for (_, objs) in &entries {
                    self.destroy_entry_objs(objs).await?;
                }
                self.handle
                    .remove_acl_table(table)
                    .await
                    .map_err(Self::sai_err)?;
                let mut world = self
                    .handle
                    .acls
                    .write()
                    .map_err(|_| Status::internal("acl table poisoned"))?;
                world.tables.remove(&(port.to_string(), stage));
                None
            }
            other => other,
        };

        let (table, mut existing, bind_needed): (Oid, BTreeMap<AclEntryKey, AclEntryObjs>, bool) =
            match current {
                Some((table, _, entries)) => (table, entries.into_iter().collect(), false),
                None => {
                    let table = self
                        .handle
                        .create_acl_table(stage, desired.family)
                        .await
                        .map_err(Self::sai_err)?;
                    (table, BTreeMap::new(), true)
                }
            };

        // Remove entries no longer desired.
        let wanted_keys: std::collections::BTreeSet<AclEntryKey> =
            desired.entries.iter().map(|d| d.key).collect();
        let stale: Vec<AclEntryKey> = existing
            .keys()
            .filter(|k| !wanted_keys.contains(*k))
            .copied()
            .collect();
        for key in stale {
            if let Some(objs) = existing.remove(&key) {
                self.destroy_entry_objs(&objs).await?;
            }
        }

        // Create or update the desired entries.
        let mut new_entries: BTreeMap<AclEntryKey, AclEntryObjs> = BTreeMap::new();
        for want in &desired.entries {
            let objs = match existing.remove(&want.key) {
                None => self.create_entry_objs(table, want, None).await?,
                Some(have) => {
                    let unchanged = have.priority == want.priority
                        && have.fields == want.fields
                        && have.action == want.action
                        && have.police == want.police;
                    if unchanged {
                        have
                    } else {
                        self.update_entry_objs(table, have, want).await?
                    }
                }
            };
            new_entries.insert(want.key, objs);
        }

        if bind_needed {
            if let Err(err) = self.handle.bind_port_acl(port_id, stage, Some(table)).await {
                return Err(Self::sai_err(err));
            }
        }

        let mut world = self
            .handle
            .acls
            .write()
            .map_err(|_| Status::internal("acl table poisoned"))?;
        world.tables.insert(
            (port.to_string(), stage),
            PortAclTable {
                table,
                family: desired.family,
                user_acl: desired.user_acl,
                entries: new_entries,
            },
        );
        Ok(())
    }

    /// Create the SAI objects behind one desired entry. `counter`
    /// reuses an existing counter object (its match count survives).
    async fn create_entry_objs(
        &self,
        table: Oid,
        want: &DesiredEntry,
        counter: Option<Oid>,
    ) -> Result<AclEntryObjs, Status> {
        let counter = match counter {
            Some(counter) => Some(counter),
            None if want.wants_counter => Some(
                self.handle
                    .create_acl_counter(table)
                    .await
                    .map_err(Self::sai_err)?,
            ),
            None => None,
        };
        let policer = match want.police {
            Some(spec) => Some(
                self.handle
                    .create_policer(spec)
                    .await
                    .map_err(Self::sai_err)?,
            ),
            None => None,
        };
        let action = AclAction {
            action: want.action,
            counter,
            policer,
        };
        let entry = self
            .handle
            .create_acl_entry(table, want.priority, want.fields.clone(), action)
            .await
            .map_err(Self::sai_err)?;
        Ok(AclEntryObjs {
            entry,
            counter,
            policer,
            priority: want.priority,
            fields: want.fields.clone(),
            action: want.action,
            police: want.police,
        })
    }

    /// Reconcile one changed entry, keeping its counter object (and the
    /// policer object when only the rate changed).
    async fn update_entry_objs(
        &self,
        table: Oid,
        have: AclEntryObjs,
        want: &DesiredEntry,
    ) -> Result<AclEntryObjs, Status> {
        // Settle the policer object first.
        let policer = match (have.policer, want.police) {
            (Some(policer), Some(spec)) => {
                if have.police != Some(spec) {
                    self.handle
                        .set_policer(policer, spec)
                        .await
                        .map_err(Self::sai_err)?;
                }
                Some(policer)
            }
            (None, Some(spec)) => Some(
                self.handle
                    .create_policer(spec)
                    .await
                    .map_err(Self::sai_err)?,
            ),
            (Some(_), None) | (None, None) => None,
        };
        let action = AclAction {
            action: want.action,
            counter: have.counter,
            policer,
        };
        let entry = if have.priority != want.priority || have.fields != want.fields {
            // Fields and priority are create-time shape: recreate the
            // entry around the surviving counter.
            self.handle
                .remove_acl_entry(have.entry)
                .await
                .map_err(Self::sai_err)?;
            self.handle
                .create_acl_entry(table, want.priority, want.fields.clone(), action)
                .await
                .map_err(Self::sai_err)?
        } else {
            self.handle
                .set_acl_entry_action(have.entry, action)
                .await
                .map_err(Self::sai_err)?;
            have.entry
        };
        // A dropped policer is removed after the entry stops
        // referencing it.
        if want.police.is_none() {
            if let Some(old) = have.policer {
                self.handle
                    .remove_policer(old)
                    .await
                    .map_err(Self::sai_err)?;
            }
        }
        Ok(AclEntryObjs {
            entry,
            counter: have.counter,
            policer,
            priority: want.priority,
            fields: want.fields.clone(),
            action: want.action,
            police: want.police,
        })
    }

    /// Remove one entry and everything it owns (counter included).
    async fn destroy_entry_objs(&self, objs: &AclEntryObjs) -> Result<(), Status> {
        self.handle
            .remove_acl_entry(objs.entry)
            .await
            .map_err(Self::sai_err)?;
        if let Some(policer) = objs.policer {
            self.handle
                .remove_policer(policer)
                .await
                .map_err(Self::sai_err)?;
        }
        if let Some(counter) = objs.counter {
            self.handle
                .remove_acl_counter(counter)
                .await
                .map_err(Self::sai_err)?;
            if let Ok(mut world) = self.handle.acls.write() {
                world.counter_base.remove(&counter.0);
            }
        }
        Ok(())
    }

    /// Re-materialize every (port, stage) a binding target expands to.
    pub(crate) async fn apply_targets(
        &self,
        targets: &[String],
        stage: AclStage,
    ) -> Result<(), Status> {
        for port in targets {
            self.apply_port_stage(port, stage).await?;
        }
        Ok(())
    }

    /// Re-expand a Port-Channel's ACL bindings after membership churn:
    /// stale members drop the LAG's entries, current members carry
    /// them.
    pub(crate) async fn refresh_lag_acls(
        &self,
        group: u16,
        stale: &[String],
        current: &[String],
    ) -> Result<(), Status> {
        let name = format!("Port-Channel{group}");
        let stages: Vec<AclStage> = {
            let world = self
                .handle
                .acls
                .read()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            [AclStage::Ingress, AclStage::Egress]
                .into_iter()
                .filter(|stage| world.bindings.contains_key(&(name.clone(), *stage)))
                .collect()
        };
        for stage in stages {
            for port in stale.iter().chain(current.iter()) {
                self.apply_port_stage(port, stage).await?;
            }
        }
        Ok(())
    }
}

// --- proto conversions -----------------------------------------------

#[allow(clippy::result_large_err)]
fn family_from_proto(value: i32) -> Result<AclFamily, Status> {
    match pb::AclFamily::try_from(value) {
        Ok(pb::AclFamily::Ipv4) => Ok(AclFamily::Ipv4),
        Ok(pb::AclFamily::Ipv6) => Ok(AclFamily::Ipv6),
        Ok(pb::AclFamily::Mac) => Ok(AclFamily::Mac),
        _ => Err(Status::invalid_argument("ACL family required")),
    }
}

fn family_to_proto(family: AclFamily) -> i32 {
    (match family {
        AclFamily::Ipv4 => pb::AclFamily::Ipv4,
        AclFamily::Ipv6 => pb::AclFamily::Ipv6,
        AclFamily::Mac => pb::AclFamily::Mac,
    }) as i32
}

#[allow(clippy::result_large_err)]
pub(crate) fn stage_from_proto(value: i32) -> Result<AclStage, Status> {
    match pb::AclStage::try_from(value) {
        Ok(pb::AclStage::Ingress) => Ok(AclStage::Ingress),
        Ok(pb::AclStage::Egress) => Ok(AclStage::Egress),
        _ => Err(Status::invalid_argument("ACL stage required")),
    }
}

fn stage_to_proto(stage: AclStage) -> i32 {
    (match stage {
        AclStage::Ingress => pb::AclStage::Ingress,
        AclStage::Egress => pb::AclStage::Egress,
    }) as i32
}

#[allow(clippy::result_large_err)]
fn rule_state_from_proto(rule: &pb::AclRule) -> Result<(u32, AclRuleState), Status> {
    let bad = |what: String| Status::invalid_argument(what);
    if rule.number == 0 {
        return Err(bad("rule number must be >= 1".into()));
    }
    let mut fields = AclFields::default();
    if let Some(protocol) = rule.protocol {
        fields.protocol =
            Some(u8::try_from(protocol).map_err(|_| bad(format!("bad protocol {protocol}")))?);
    }
    if !rule.source.is_empty() {
        fields.src_ip =
            Some(hemlock_common::net::parse_cidr(&rule.source).map_err(Status::invalid_argument)?);
    }
    if !rule.destination.is_empty() {
        fields.dst_ip = Some(
            hemlock_common::net::parse_cidr(&rule.destination).map_err(Status::invalid_argument)?,
        );
    }
    let port_range = |low: Option<u32>, high: Option<u32>| -> Result<Option<(u16, u16)>, Status> {
        match (low, high) {
            (None, None) => Ok(None),
            (Some(low), Some(high)) => {
                let low = u16::try_from(low).map_err(|_| bad(format!("bad port {low}")))?;
                let high = u16::try_from(high).map_err(|_| bad(format!("bad port {high}")))?;
                if low > high {
                    return Err(bad(format!("bad port range {low}-{high}")));
                }
                Ok(Some((low, high)))
            }
            _ => Err(bad("port range needs both bounds".into())),
        }
    };
    fields.src_port = port_range(rule.source_port_low, rule.source_port_high)?;
    fields.dst_port = port_range(rule.destination_port_low, rule.destination_port_high)?;
    if let Some(dscp) = rule.dscp {
        if dscp > 63 {
            return Err(bad(format!("bad dscp {dscp} (0..63)")));
        }
        fields.dscp = Some(dscp as u8);
    }
    #[allow(clippy::type_complexity)]
    let mac_match = |mac: &str, mask: &str| -> Result<Option<([u8; 6], [u8; 6])>, Status> {
        if mac.is_empty() {
            return Ok(None);
        }
        let (mac, _) = SyncdService::mac_bytes(mac)?;
        let mask = if mask.is_empty() {
            [0xff; 6]
        } else {
            SyncdService::mac_bytes(mask)?.0
        };
        Ok(Some((mac, mask)))
    };
    fields.src_mac = mac_match(&rule.source_mac, &rule.source_mac_mask)?;
    fields.dst_mac = mac_match(&rule.destination_mac, &rule.destination_mac_mask)?;
    if let Some(ethertype) = rule.ethertype {
        fields.ethertype =
            Some(u16::try_from(ethertype).map_err(|_| bad(format!("bad ethertype {ethertype}")))?);
    }
    let police = match (rule.police_rate, rule.police_burst) {
        (None, None) => None,
        (Some(rate), Some(burst)) => Some(PolicerSpec {
            pps: rule.police_pps,
            rate,
            burst,
        }),
        _ => return Err(bad("police needs both rate and burst".into())),
    };
    Ok((
        rule.number,
        AclRuleState {
            permit: rule.permit,
            log: rule.log,
            fields,
            police,
        },
    ))
}

fn rule_to_proto(number: u32, rule: &AclRuleState) -> pb::AclRule {
    let f = &rule.fields;
    pb::AclRule {
        number,
        permit: rule.permit,
        protocol: f.protocol.map(u32::from),
        source: f
            .src_ip
            .map(|(ip, len)| format!("{ip}/{len}"))
            .unwrap_or_default(),
        destination: f
            .dst_ip
            .map(|(ip, len)| format!("{ip}/{len}"))
            .unwrap_or_default(),
        source_port_low: f.src_port.map(|(low, _)| u32::from(low)),
        source_port_high: f.src_port.map(|(_, high)| u32::from(high)),
        destination_port_low: f.dst_port.map(|(low, _)| u32::from(low)),
        destination_port_high: f.dst_port.map(|(_, high)| u32::from(high)),
        dscp: f.dscp.map(u32::from),
        log: rule.log,
        police_rate: rule.police.map(|p| p.rate),
        police_burst: rule.police.map(|p| p.burst),
        police_pps: rule.police.map(|p| p.pps).unwrap_or(false),
        source_mac: f
            .src_mac
            .map(|(mac, _)| crate::actor::format_mac(mac))
            .unwrap_or_default(),
        source_mac_mask: f
            .src_mac
            .map(|(_, mask)| crate::actor::format_mac(mask))
            .unwrap_or_default(),
        destination_mac: f
            .dst_mac
            .map(|(mac, _)| crate::actor::format_mac(mac))
            .unwrap_or_default(),
        destination_mac_mask: f
            .dst_mac
            .map(|(_, mask)| crate::actor::format_mac(mask))
            .unwrap_or_default(),
        ethertype: f.ethertype.map(u32::from),
    }
}

// --- RPC bodies ------------------------------------------------------

#[allow(clippy::result_large_err)]
impl SyncdService {
    fn require_acls(&self) -> Result<(), Status> {
        if self.handle.capabilities.acl_ingress {
            Ok(())
        } else {
            Err(Status::failed_precondition(
                "ACLs are not supported by this platform's SAI",
            ))
        }
    }

    fn require_egress_acls(&self) -> Result<(), Status> {
        if self.handle.capabilities.acl_egress {
            Ok(())
        } else {
            Err(Status::failed_precondition(
                "egress ACLs are not supported by this platform's SAI",
            ))
        }
    }

    fn require_entry_policers(&self) -> Result<(), Status> {
        if self.handle.capabilities.acl_entry_policer {
            Ok(())
        } else {
            Err(Status::failed_precondition(
                "per-rule policers are not supported by this platform's SAI",
            ))
        }
    }

    pub(crate) async fn ensure_acl_impl(&self, req: pb::EnsureAclRequest) -> Result<(), Status> {
        if req.name.is_empty() {
            return Err(Status::invalid_argument("ACL name required"));
        }
        let family = family_from_proto(req.family)?;
        let mut rules = BTreeMap::new();
        for rule in &req.rules {
            let (number, state) = rule_state_from_proto(rule)?;
            if rules.insert(number, state).is_some() {
                return Err(Status::invalid_argument(format!(
                    "duplicate rule {number} in ACL {}",
                    req.name
                )));
            }
        }
        let program = AclProgram { family, rules };
        // Where is this ACL currently in force?
        let bound_targets: Vec<(String, AclStage)> = {
            let world = self
                .handle
                .acls
                .read()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            world
                .bindings
                .iter()
                .filter(|(_, acl)| **acl == req.name)
                .map(|((target, stage), _)| (target.clone(), *stage))
                .collect()
        };
        if !bound_targets.is_empty() && program.rules.values().any(|r| r.police.is_some()) {
            self.require_entry_policers()?;
        }
        {
            let mut world = self
                .handle
                .acls
                .write()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            world.acls.insert(req.name.clone(), program);
        }
        for (target, stage) in bound_targets {
            let targets = self.physical_targets(&target)?;
            self.apply_targets(&targets, stage).await?;
        }
        Ok(())
    }

    pub(crate) async fn remove_acl_impl(&self, name: &str) -> Result<(), Status> {
        let mut world = self
            .handle
            .acls
            .write()
            .map_err(|_| Status::internal("acl table poisoned"))?;
        if world.bindings.values().any(|acl| acl == name) {
            return Err(Status::failed_precondition(format!("ACL {name} is in use")));
        }
        if world.acls.remove(name).is_none() {
            return Err(Status::not_found(format!("no such ACL {name:?}")));
        }
        Ok(())
    }

    pub(crate) async fn bind_port_acl_impl(
        &self,
        req: pb::BindPortAclRequest,
    ) -> Result<(), Status> {
        let stage = stage_from_proto(req.stage)?;
        self.require_acls()?;
        if stage == AclStage::Egress {
            self.require_egress_acls()?;
        }
        let has_police = {
            let world = self
                .handle
                .acls
                .read()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            let program = world
                .acls
                .get(&req.acl)
                .ok_or_else(|| Status::failed_precondition(format!("no such ACL {:?}", req.acl)))?;
            program.rules.values().any(|r| r.police.is_some())
        };
        if has_police {
            self.require_entry_policers()?;
        }
        let targets = self.physical_targets(&req.port)?;
        let key = (req.port.clone(), stage);
        let previous = {
            let mut world = self
                .handle
                .acls
                .write()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            world.bindings.insert(key.clone(), req.acl.clone())
        };
        if let Err(err) = self.apply_targets(&targets, stage).await {
            // Roll the binding back and best-effort restore the ports.
            if let Ok(mut world) = self.handle.acls.write() {
                match previous {
                    Some(previous) => {
                        world.bindings.insert(key, previous);
                    }
                    None => {
                        world.bindings.remove(&key);
                    }
                }
            }
            for port in &targets {
                let _ = self.apply_port_stage(port, stage).await;
            }
            return Err(err);
        }
        Ok(())
    }

    pub(crate) async fn unbind_port_acl_impl(
        &self,
        req: pb::UnbindPortAclRequest,
    ) -> Result<(), Status> {
        let stage = stage_from_proto(req.stage)?;
        let removed = {
            let mut world = self
                .handle
                .acls
                .write()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            world.bindings.remove(&(req.port.clone(), stage))
        };
        if removed.is_none() {
            return Ok(());
        }
        let targets = self.physical_targets(&req.port)?;
        self.apply_targets(&targets, stage).await
    }

    pub(crate) async fn acl_state_impl(&self) -> Result<pb::GetAclStateResponse, Status> {
        struct AclCounters {
            per_rule: Vec<(u32, Vec<Oid>)>,
            implicit: Vec<Oid>,
        }
        // Snapshot under the read lock; counter reads happen after.
        #[allow(clippy::type_complexity)]
        let (programs, counters, bindings, used, base): (
            Vec<(String, AclFamily, Vec<(u32, AclRuleState)>)>,
            Vec<AclCounters>,
            Vec<(String, Vec<(String, AclStage)>)>,
            Vec<(AclStage, u32)>,
            std::collections::HashMap<u64, u64>,
        ) = {
            let world = self
                .handle
                .acls
                .read()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            let mut programs = Vec::new();
            let mut counters = Vec::new();
            let mut bindings = Vec::new();
            for (name, program) in &world.acls {
                programs.push((
                    name.clone(),
                    program.family,
                    program.rules.iter().map(|(n, r)| (*n, r.clone())).collect(),
                ));
                let mut per_rule: Vec<(u32, Vec<Oid>)> =
                    program.rules.keys().map(|n| (*n, Vec::new())).collect();
                let mut implicit = Vec::new();
                for table in world.tables.values() {
                    if table.user_acl.as_deref() != Some(name.as_str()) {
                        continue;
                    }
                    for (key, objs) in &table.entries {
                        let Some(counter) = objs.counter else {
                            continue;
                        };
                        match key {
                            AclEntryKey::User(number) => {
                                if let Some((_, list)) =
                                    per_rule.iter_mut().find(|(n, _)| n == number)
                                {
                                    list.push(counter);
                                }
                            }
                            AclEntryKey::ImplicitDeny => implicit.push(counter),
                            AclEntryKey::Internal(_) => {}
                        }
                    }
                }
                counters.push(AclCounters { per_rule, implicit });
                bindings.push((
                    name.clone(),
                    world
                        .bindings
                        .iter()
                        .filter(|(_, acl)| *acl == name)
                        .map(|((target, stage), _)| (target.clone(), *stage))
                        .collect(),
                ));
            }
            let mut used = vec![(AclStage::Ingress, 0u32), (AclStage::Egress, 0u32)];
            for ((_, stage), table) in world.tables.iter().map(|(k, v)| (k.clone(), v)) {
                let width = if table.family == AclFamily::Ipv6 {
                    2
                } else {
                    1
                };
                if let Some((_, total)) = used.iter_mut().find(|(s, _)| *s == stage) {
                    *total += table.entries.len() as u32 * width;
                }
            }
            (
                programs,
                counters,
                bindings,
                used,
                world.counter_base.clone(),
            )
        };

        let read = |oid: Oid, base: &std::collections::HashMap<u64, u64>| {
            let base = *base.get(&oid.0).unwrap_or(&0);
            async move { (oid, base) }
        };
        let mut acls = Vec::new();
        for (((name, family, rules), counter_set), (_, acl_bindings)) in
            programs.into_iter().zip(counters).zip(bindings)
        {
            let mut matches = Vec::new();
            for (_, oids) in &counter_set.per_rule {
                let mut total = 0u64;
                for oid in oids {
                    let (oid, baseline) = read(*oid, &base).await;
                    let raw = self.handle.get_acl_counter(oid).await.unwrap_or(0);
                    total += raw.saturating_sub(baseline);
                }
                matches.push(total);
            }
            let mut implicit_total = 0u64;
            for oid in &counter_set.implicit {
                let (oid, baseline) = read(*oid, &base).await;
                let raw = self.handle.get_acl_counter(oid).await.unwrap_or(0);
                implicit_total += raw.saturating_sub(baseline);
            }
            acls.push(pb::AclStateEntry {
                name,
                family: family_to_proto(family),
                rules: rules
                    .iter()
                    .map(|(number, rule)| rule_to_proto(*number, rule))
                    .collect(),
                matches,
                implicit_deny_matches: implicit_total,
                bindings: acl_bindings
                    .into_iter()
                    .map(|(port, stage)| pb::AclBindingState {
                        port,
                        stage: stage_to_proto(stage),
                    })
                    .collect(),
            });
        }
        let mut tcam = Vec::new();
        for (stage, used) in used {
            let available = self.handle.acl_available_entries(stage).await.unwrap_or(0);
            tcam.push(pb::TcamStageState {
                stage: stage_to_proto(stage),
                used,
                available,
            });
        }
        Ok(pb::GetAclStateResponse { acls, tcam })
    }

    pub(crate) async fn clear_acl_counters_impl(&self, name: &str) -> Result<u32, Status> {
        let oids: Vec<Oid> = {
            let world = self
                .handle
                .acls
                .read()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            if !name.is_empty() && !world.acls.contains_key(name) {
                return Err(Status::not_found(format!("no such ACL {name:?}")));
            }
            world
                .tables
                .values()
                .filter(|t| name.is_empty() || t.user_acl.as_deref() == Some(name))
                .flat_map(|t| t.entries.values().filter_map(|o| o.counter))
                .collect()
        };
        let mut baselines = Vec::new();
        for oid in &oids {
            let raw = self.handle.get_acl_counter(*oid).await.unwrap_or(0);
            baselines.push((oid.0, raw));
        }
        let mut world = self
            .handle
            .acls
            .write()
            .map_err(|_| Status::internal("acl table poisoned"))?;
        let cleared = baselines.len() as u32;
        for (oid, raw) in baselines {
            world.counter_base.insert(oid, raw);
        }
        Ok(cleared)
    }

    pub(crate) async fn set_copp_class_impl(
        &self,
        req: pb::SetCoppClassRequest,
    ) -> Result<(), Status> {
        self.require_capability(self.handle.capabilities.copp, "control-plane policing")?;
        let def = copp_class(&req.class).ok_or_else(|| {
            Status::invalid_argument(format!("unknown CoPP class {:?}", req.class))
        })?;
        let rate = req.rate.unwrap_or(def.rate);
        let burst = req.burst.unwrap_or(def.burst);
        if rate == 0 || burst == 0 {
            return Err(Status::invalid_argument("rate and burst must be positive"));
        }
        let policer = {
            let copp = self
                .handle
                .copp
                .read()
                .map_err(|_| Status::internal("copp table poisoned"))?;
            copp.classes
                .get(def.name)
                .and_then(|c| c.policer)
                .ok_or_else(|| Status::failed_precondition("CoPP classes not programmed"))?
        };
        self.handle
            .set_policer(
                policer,
                PolicerSpec {
                    pps: true,
                    rate: u64::from(rate),
                    burst: u64::from(burst),
                },
            )
            .await
            .map_err(Self::sai_err)?;
        let mut copp = self
            .handle
            .copp
            .write()
            .map_err(|_| Status::internal("copp table poisoned"))?;
        if let Some(class) = copp.classes.get_mut(def.name) {
            class.rate = rate;
            class.burst = burst;
            class.overridden = req.rate.is_some() || req.burst.is_some();
        }
        Ok(())
    }

    pub(crate) async fn copp_state_impl(&self) -> Result<pb::GetCoppStateResponse, Status> {
        let programmed: BTreeMap<&'static str, CoppClassState> = self
            .handle
            .copp
            .read()
            .map_err(|_| Status::internal("copp table poisoned"))?
            .classes
            .clone();
        let mut classes = Vec::new();
        for def in COPP_CLASSES {
            let state = programmed.get(def.name);
            let (rate, burst, overridden) = match state {
                Some(state) => (state.rate, state.burst, state.overridden),
                None => (def.rate, def.burst, false),
            };
            let (conforming, dropped) = match state.and_then(|s| s.policer.map(|p| (p, s.base))) {
                Some((policer, base)) => {
                    let stats = self.handle.policer_stats(policer).await.unwrap_or_default();
                    (
                        stats.conforming.saturating_sub(base.conforming),
                        stats.dropped.saturating_sub(base.dropped),
                    )
                }
                None => (0, 0),
            };
            classes.push(pb::CoppClassState {
                class: def.name.to_string(),
                rate,
                burst,
                overridden,
                conforming,
                dropped,
            });
        }
        Ok(pb::GetCoppStateResponse { classes })
    }

    pub(crate) async fn clear_copp_counters_impl(&self) -> Result<(), Status> {
        let policers: Vec<(&'static str, Oid)> = self
            .handle
            .copp
            .read()
            .map_err(|_| Status::internal("copp table poisoned"))?
            .classes
            .iter()
            .filter_map(|(name, c)| c.policer.map(|p| (*name, p)))
            .collect();
        let mut bases = Vec::new();
        for (name, policer) in policers {
            let stats = self.handle.policer_stats(policer).await.unwrap_or_default();
            bases.push((name, stats));
        }
        let mut copp = self
            .handle
            .copp
            .write()
            .map_err(|_| Status::internal("copp table poisoned"))?;
        for (name, stats) in bases {
            if let Some(class) = copp.classes.get_mut(name) {
                class.base = stats;
            }
        }
        Ok(())
    }

    pub(crate) async fn set_port_security_impl(
        &self,
        req: pb::SetPortSecurityRequest,
    ) -> Result<(), Status> {
        self.require_capability(self.handle.capabilities.port_learn_limit, "port-security")?;
        if !(1..=1024).contains(&req.maximum) {
            return Err(Status::invalid_argument(format!(
                "bad maximum {} (1..1024)",
                req.maximum
            )));
        }
        let port_id = self.port_sai_id(&req.port)?;
        self.handle
            .set_port_learn_limit(port_id, Some(req.maximum))
            .await
            .map_err(Self::sai_err)?;
        // Seed the secure set from what the port already learned.
        let seed: Vec<String> = {
            let fdb = self
                .handle
                .fdb
                .read()
                .map_err(|_| Status::internal("fdb table poisoned"))?;
            fdb.dynamics
                .iter()
                .filter(|(_, e)| e.port == req.port)
                .map(|((_, mac), _)| mac.clone())
                .collect()
        };
        let mut table = self
            .handle
            .port_security
            .write()
            .map_err(|_| Status::internal("port-security table poisoned"))?;
        let state = table
            .entry(req.port.clone())
            .or_insert_with(|| PortSecurityState {
                max: req.maximum,
                shutdown: req.shutdown,
                learned: BTreeMap::new(),
                violations: 0,
                last_violation: None,
            });
        state.max = req.maximum;
        state.shutdown = req.shutdown;
        for mac in seed {
            state.learned.entry(mac).or_insert_with(Instant::now);
        }
        Ok(())
    }

    pub(crate) async fn clear_port_security_impl(&self, port: &str) -> Result<(), Status> {
        let removed = {
            let mut table = self
                .handle
                .port_security
                .write()
                .map_err(|_| Status::internal("port-security table poisoned"))?;
            table.remove(port)
        };
        if removed.is_none() {
            return Ok(());
        }
        let port_id = self.port_sai_id(port)?;
        self.handle
            .set_port_learn_limit(port_id, None)
            .await
            .map_err(Self::sai_err)?;
        self.handle
            .set_port_learning(port_id, true)
            .await
            .map_err(Self::sai_err)?;
        self.reenable_if_psec_errdisabled(port).await?;
        Ok(())
    }

    /// Re-enable a port that port-security errdisabled.
    async fn reenable_if_psec_errdisabled(&self, port: &str) -> Result<(), Status> {
        let errdisabled = {
            let ports = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            ports
                .get(port)
                .map(|p| p.errdisable_reason.as_deref() == Some("port-security"))
                .unwrap_or(false)
        };
        if !errdisabled {
            return Ok(());
        }
        let port_id = self.port_sai_id(port)?;
        self.handle
            .set_admin_state(port_id, true)
            .await
            .map_err(Self::sai_err)?;
        let mut ports = self
            .handle
            .ports
            .write()
            .map_err(|_| Status::internal("port table poisoned"))?;
        if let Some(state) = ports.get_mut(port) {
            state.admin_up = true;
            state.errdisable_reason = None;
        }
        Ok(())
    }

    pub(crate) async fn port_security_state_impl(
        &self,
        filter: &str,
    ) -> Result<pb::GetPortSecurityStateResponse, Status> {
        let entries: Vec<(String, PortSecurityState)> = {
            let table = self
                .handle
                .port_security
                .read()
                .map_err(|_| Status::internal("port-security table poisoned"))?;
            table
                .iter()
                .filter(|(port, _)| filter.is_empty() || *port == filter)
                .map(|(port, state)| (port.clone(), state.clone()))
                .collect()
        };
        if !filter.is_empty() && entries.is_empty() {
            return Err(Status::not_found(format!(
                "port-security is not enabled on {filter}"
            )));
        }
        let errdisabled: std::collections::HashSet<String> = {
            let ports = self
                .handle
                .ports
                .read()
                .map_err(|_| Status::internal("port table poisoned"))?;
            ports
                .iter()
                .filter(|(_, p)| p.errdisable_reason.as_deref() == Some("port-security"))
                .map(|(name, _)| name.clone())
                .collect()
        };
        let mut rows = Vec::new();
        for (port, state) in entries {
            rows.push(pb::PortSecurityEntry {
                errdisabled: errdisabled.contains(&port),
                maximum: state.max,
                shutdown: state.shutdown,
                learned: state
                    .learned
                    .iter()
                    .map(|(mac, since)| pb::SecureMacState {
                        mac: mac.clone(),
                        age_secs: since.elapsed().as_secs(),
                    })
                    .collect(),
                violations: state.violations,
                last_violation_mac: state
                    .last_violation
                    .as_ref()
                    .map(|(mac, _)| mac.clone())
                    .unwrap_or_default(),
                last_violation_secs_ago: state
                    .last_violation
                    .as_ref()
                    .map(|(_, when)| when.elapsed().as_secs()),
                port,
            });
        }
        rows.sort_by(|a, b| a.port.cmp(&b.port));
        Ok(pb::GetPortSecurityStateResponse { ports: rows })
    }

    pub(crate) async fn reset_port_security_impl(&self, filter: &str) -> Result<u32, Status> {
        let ports: Vec<String> = {
            let table = self
                .handle
                .port_security
                .read()
                .map_err(|_| Status::internal("port-security table poisoned"))?;
            table
                .keys()
                .filter(|port| filter.is_empty() || *port == filter)
                .cloned()
                .collect()
        };
        if !filter.is_empty() && ports.is_empty() {
            return Err(Status::not_found(format!(
                "port-security is not enabled on {filter}"
            )));
        }
        for port in &ports {
            {
                let mut table = self
                    .handle
                    .port_security
                    .write()
                    .map_err(|_| Status::internal("port-security table poisoned"))?;
                if let Some(state) = table.get_mut(port) {
                    state.learned.clear();
                    state.violations = 0;
                    state.last_violation = None;
                }
            }
            let port_id = self.port_sai_id(port)?;
            // Undo a protect freeze and drop the secure dynamics.
            self.handle
                .set_port_learning(port_id, true)
                .await
                .map_err(Self::sai_err)?;
            if self.handle.capabilities.fdb_flush {
                let _ = self.handle.flush_fdb(None, Some(port_id)).await;
                if let Ok(mut fdb) = self.handle.fdb.write() {
                    fdb.dynamics.retain(|_, e| e.port != *port);
                }
            }
            self.reenable_if_psec_errdisabled(port).await?;
        }
        Ok(ports.len() as u32)
    }

    pub(crate) async fn set_port_authorized_impl(
        &self,
        req: pb::SetPortAuthorizedRequest,
    ) -> Result<(), Status> {
        self.require_acls()?;
        self.port_sai_id(&req.port)?;
        {
            let mut world = self
                .handle
                .acls
                .write()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            let key = (req.port.clone(), AclStage::Ingress);
            let internal = world.internal.entry(key.clone()).or_default();
            if req.authorized {
                internal.dot1x.clear();
            } else {
                internal.dot1x = vec![
                    // EAPOL punts to the CPU for hostapd; everything
                    // else drops until the supplicant authenticates.
                    InternalAclEntry {
                        fields: AclFields {
                            ethertype: Some(0x888e),
                            ..Default::default()
                        },
                        action: AclPacketAction::Trap,
                    },
                    InternalAclEntry {
                        fields: AclFields::default(),
                        action: AclPacketAction::Drop,
                    },
                ];
            }
            if internal.is_empty() {
                world.internal.remove(&key);
            }
        }
        self.apply_port_stage(&req.port, AclStage::Ingress).await
    }

    pub(crate) async fn set_snoop_redirects_impl(
        &self,
        req: pb::SetSnoopRedirectsRequest,
    ) -> Result<(), Status> {
        self.require_acls()?;
        let mut affected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        {
            let mut world = self
                .handle
                .acls
                .write()
                .map_err(|_| Status::internal("acl table poisoned"))?;
            // Declarative: drop every existing snooping entry, then
            // build the new program.
            for ((port, _), internal) in world.internal.iter_mut() {
                if !internal.snoop.is_empty() {
                    internal.snoop.clear();
                    affected.insert(port.clone());
                }
            }
            let push = |world: &mut crate::state::AclWorld,
                        port: &str,
                        entry: InternalAclEntry,
                        affected: &mut std::collections::BTreeSet<String>| {
                let key = (port.to_string(), AclStage::Ingress);
                world.internal.entry(key).or_default().snoop.push(entry);
                affected.insert(port.to_string());
            };
            for program in &req.dhcp {
                let vlan = program.vlan as u16;
                let dhcp_fields = AclFields {
                    vlan: Some(vlan),
                    protocol: Some(17),
                    dst_port: Some((67, 68)),
                    ..Default::default()
                };
                for port in &program.untrusted_ports {
                    push(
                        &mut world,
                        port,
                        InternalAclEntry {
                            fields: dhcp_fields.clone(),
                            action: AclPacketAction::Trap,
                        },
                        &mut affected,
                    );
                }
                for port in &program.trusted_ports {
                    push(
                        &mut world,
                        port,
                        InternalAclEntry {
                            fields: dhcp_fields.clone(),
                            action: AclPacketAction::Copy,
                        },
                        &mut affected,
                    );
                }
            }
            for program in &req.arp {
                let arp_fields = AclFields {
                    vlan: Some(program.vlan as u16),
                    ethertype: Some(0x0806),
                    ..Default::default()
                };
                for port in &program.untrusted_ports {
                    push(
                        &mut world,
                        port,
                        InternalAclEntry {
                            fields: arp_fields.clone(),
                            action: AclPacketAction::Trap,
                        },
                        &mut affected,
                    );
                }
            }
            let empty: Vec<(String, AclStage)> = world
                .internal
                .iter()
                .filter(|(_, internal)| internal.is_empty())
                .map(|(key, _)| key.clone())
                .collect();
            for key in empty {
                world.internal.remove(&key);
            }
        }
        for port in affected {
            self.apply_port_stage(&port, AclStage::Ingress).await?;
        }
        Ok(())
    }
}
