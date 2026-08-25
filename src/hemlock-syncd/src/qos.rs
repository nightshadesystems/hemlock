//! The QoS suite's syncd engine: the four global map objects and their
//! per-port bindings, per-port classification (trust mode + default
//! traffic class), egress scheduling and shaping, and named WRED/ECN
//! profiles.
//!
//! # What syncd owns
//!
//! The RPC surface is declarative and flat — one whole-port program per
//! port, one whole-map push for the globals — so everything below is an
//! implementation detail here:
//!
//! - **Map objects.** A map table with entries gets exactly one SAI
//!   object, rewritten in place when the table changes so bound ports
//!   keep their bindings. Ingress maps bind only where a port's trust
//!   mode needs them (`trust dscp` -> `DSCP_TO_TC`, `trust cos` ->
//!   `DOT1P_TO_TC`); the egress rewrite maps are global, so they bind
//!   on every front-panel port whenever their table is non-empty.
//! - **Scheduler profiles.** Deduplicated and refcounted by value, the
//!   same pattern as the FIB's next-hop groups: two ports asking for
//!   the same queue shape share one SAI scheduler, and the object is
//!   freed when the last queue unbinds. A queue at the platform default
//!   (DWRR weight 1, unshaped) binds no profile at all.
//! - **WRED objects.** Refcounted by profile name. The object is
//!   created on the first queue binding and freed on the last unbind,
//!   so an unreferenced profile costs the ASIC nothing — and a platform
//!   without WRED fails the *reference*, not the profile definition.
//! - **Port-Channel expansion.** A Port-Channel program expands to the
//!   current member ports and follows membership churn, exactly like a
//!   switchport or ACL binding does.

use std::collections::{BTreeMap, BTreeSet};

use hemlock_common::proto::v1 as pb;
use hemlock_sai::{Oid, QosMapType, SchedulerSpec, WredSpec};
use tonic::Status;

use crate::service::{lag_group_of, SyncdService};
use crate::state::{PortQosProgram, QosTrust, QueueQosProgram, WredProfileState};

/// Every map type, in a stable order for diffing and rendering.
const MAP_TYPES: [QosMapType; 4] = [
    QosMapType::DscpToTc,
    QosMapType::Dot1pToTc,
    QosMapType::TcToDscp,
    QosMapType::TcToDot1p,
];

/// A `(key, value)` list out of the proto's entry repeat, range-checked
/// against the map type's key and value domains.
#[allow(clippy::result_large_err)]
fn map_entries(
    table: &str,
    key_max: u8,
    value_max: u8,
    entries: &[pb::QosMapEntry],
) -> Result<BTreeMap<u8, u8>, Status> {
    let mut out = BTreeMap::new();
    for entry in entries {
        let key = u8::try_from(entry.key)
            .ok()
            .filter(|k| *k <= key_max)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "qos map {table}: key {} is out of range (0..{key_max})",
                    entry.key
                ))
            })?;
        let value = u8::try_from(entry.value)
            .ok()
            .filter(|v| *v <= value_max)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "qos map {table}: value {} is out of range (0..{value_max})",
                    entry.value
                ))
            })?;
        out.insert(key, value);
    }
    Ok(out)
}

fn entries_to_proto(table: &BTreeMap<u8, u8>) -> Vec<pb::QosMapEntry> {
    table
        .iter()
        .map(|(key, value)| pb::QosMapEntry {
            key: u32::from(*key),
            value: u32::from(*value),
        })
        .collect()
}

/// The SAI-side threshold pair of a proto profile (KB -> bytes).
#[allow(clippy::result_large_err)]
fn wred_spec(profile: &pb::WredProfile) -> Result<WredSpec, Status> {
    let bytes = |kb: u32, what: &str| -> Result<u32, Status> {
        kb.checked_mul(1024).ok_or_else(|| {
            Status::invalid_argument(format!(
                "qos wred-profile {}: {what} {kb} KB is out of range",
                profile.name
            ))
        })
    };
    if profile.min_threshold_kb == 0 || profile.max_threshold_kb == 0 {
        return Err(Status::invalid_argument(format!(
            "qos wred-profile {}: min-threshold and max-threshold are required",
            profile.name
        )));
    }
    if profile.min_threshold_kb >= profile.max_threshold_kb {
        return Err(Status::invalid_argument(format!(
            "qos wred-profile {}: min-threshold must be below max-threshold",
            profile.name
        )));
    }
    let drop_probability = u8::try_from(profile.drop_probability)
        .ok()
        .filter(|p| (1..=100).contains(p))
        .ok_or_else(|| {
            Status::invalid_argument(format!(
                "qos wred-profile {}: drop-probability must be 1..100",
                profile.name
            ))
        })?;
    Ok(WredSpec {
        min_threshold_bytes: bytes(profile.min_threshold_kb, "min-threshold")?,
        max_threshold_bytes: bytes(profile.max_threshold_kb, "max-threshold")?,
        drop_probability,
        ecn: profile.ecn,
    })
}

fn profile_to_proto(name: &str, state: &WredProfileState) -> pb::WredProfile {
    pb::WredProfile {
        name: name.to_string(),
        min_threshold_kb: state.spec.min_threshold_bytes / 1024,
        max_threshold_kb: state.spec.max_threshold_bytes / 1024,
        drop_probability: u32::from(state.spec.drop_probability),
        ecn: state.spec.ecn,
    }
}

#[allow(clippy::result_large_err)]
impl SyncdService {
    fn qos_sai(e: hemlock_sai::SaiError) -> Status {
        Status::internal(format!("SAI: {e}"))
    }

    fn qos_poisoned() -> Status {
        Status::internal("qos table poisoned")
    }

    /// Egress queues per front-panel port, from the platform
    /// definition (8 on Helix4).
    pub(crate) fn qos_queue_count(&self) -> u8 {
        u8::try_from(self.inventory.uc_queues).unwrap_or(8)
    }

    fn require_qos_maps(&self, kind: QosMapType) -> Result<(), Status> {
        let caps = self.handle.capabilities;
        let ok = if kind.ingress() {
            caps.qos_map_ingress
        } else {
            caps.qos_map_egress
        };
        if ok {
            Ok(())
        } else {
            Err(Status::failed_precondition(format!(
                "{} QoS maps are not supported by this platform's SAI",
                if kind.ingress() {
                    "ingress"
                } else {
                    "egress rewrite"
                }
            )))
        }
    }

    /// Front-panel port names in front-panel index order.
    fn qos_ports_ordered(&self) -> Result<Vec<String>, Status> {
        let ports = self
            .handle
            .ports
            .read()
            .map_err(|_| Status::internal("port table poisoned"))?;
        let mut names: Vec<(u32, String)> = ports
            .iter()
            .map(|(name, port)| (port.def.index, name.clone()))
            .collect();
        names.sort();
        Ok(names.into_iter().map(|(_, name)| name).collect())
    }

    /// The program a physical port resolves to: its own, or its
    /// Port-Channel's. Returns the source display name alongside.
    fn resolved_qos(&self, port: &str) -> Result<Option<(String, PortQosProgram)>, Status> {
        let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
        if let Some(program) = world.programs.get(port) {
            return Ok(Some((port.to_string(), program.clone())));
        }
        let lags = self
            .handle
            .lags
            .read()
            .map_err(|_| Status::internal("lag table poisoned"))?;
        for lag in lags.values() {
            if lag.members.contains_key(port) {
                let name = format!("Port-Channel{}", lag.group);
                if let Some(program) = world.programs.get(&name) {
                    return Ok(Some((name, program.clone())));
                }
            }
        }
        Ok(None)
    }

    /// Take a reference on the scheduler profile for `spec`, creating
    /// the SAI object on first use. `None` = the platform default, which
    /// needs no object at all.
    async fn acquire_scheduler(&self, spec: SchedulerSpec) -> Result<Option<Oid>, Status> {
        if spec.is_default() {
            return Ok(None);
        }
        {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            if let Some(entry) = world.schedulers.get_mut(&spec) {
                entry.1 += 1;
                return Ok(Some(entry.0));
            }
        }
        let oid = self
            .handle
            .create_scheduler(spec)
            .await
            .map_err(Self::qos_sai)?;
        let duplicate = {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            match world.schedulers.get_mut(&spec) {
                // Another program raced us to the same shape: keep the
                // one already published and free ours.
                Some(entry) => {
                    entry.1 += 1;
                    Some((entry.0, oid))
                }
                None => {
                    world.schedulers.insert(spec, (oid, 1));
                    None
                }
            }
        };
        match duplicate {
            Some((existing, ours)) => {
                let _ = self.handle.remove_scheduler(ours).await;
                Ok(Some(existing))
            }
            None => Ok(Some(oid)),
        }
    }

    /// Drop a reference on a scheduler profile, freeing the object at
    /// zero.
    async fn release_scheduler(&self, spec: SchedulerSpec) -> Result<(), Status> {
        if spec.is_default() {
            return Ok(());
        }
        let free = {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            match world.schedulers.get_mut(&spec) {
                Some(entry) if entry.1 > 1 => {
                    entry.1 -= 1;
                    None
                }
                Some(entry) => {
                    let oid = entry.0;
                    world.schedulers.remove(&spec);
                    Some(oid)
                }
                None => None,
            }
        };
        if let Some(oid) = free {
            self.handle
                .remove_scheduler(oid)
                .await
                .map_err(Self::qos_sai)?;
        }
        Ok(())
    }

    /// Take a reference on a named WRED profile, creating its SAI
    /// object on first use.
    async fn acquire_wred(&self, name: &str) -> Result<Oid, Status> {
        let (existing, spec) = {
            let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
            let state = world.wred_profiles.get(name).ok_or_else(|| {
                Status::failed_precondition(format!("no such qos wred-profile {name:?}"))
            })?;
            (state.oid, state.spec)
        };
        if let Some(oid) = existing {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            if let Some(state) = world.wred_profiles.get_mut(name) {
                state.refs += 1;
            }
            return Ok(oid);
        }
        if !self.handle.capabilities.wred {
            return Err(Status::failed_precondition(
                "WRED is not supported by this platform's SAI",
            ));
        }
        if spec.ecn && !self.handle.capabilities.ecn {
            return Err(Status::failed_precondition(
                "ECN marking is not supported by this platform's SAI",
            ));
        }
        let oid = self.handle.create_wred(spec).await.map_err(Self::qos_sai)?;
        let duplicate = {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            let state = world.wred_profiles.get_mut(name).ok_or_else(|| {
                Status::failed_precondition(format!("no such qos wred-profile {name:?}"))
            })?;
            match state.oid {
                Some(published) => Some((published, oid)),
                None => {
                    state.oid = Some(oid);
                    state.refs += 1;
                    None
                }
            }
        };
        match duplicate {
            Some((published, ours)) => {
                let _ = self.handle.remove_wred(ours).await;
                let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
                if let Some(state) = world.wred_profiles.get_mut(name) {
                    state.refs += 1;
                }
                Ok(published)
            }
            None => Ok(oid),
        }
    }

    /// Drop a reference on a named WRED profile, freeing its object at
    /// zero. A profile deleted while still bound cannot reach here —
    /// [`Self::remove_wred_profile_impl`] refuses that.
    async fn release_wred(&self, name: &str) -> Result<(), Status> {
        let free = {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            match world.wred_profiles.get_mut(name) {
                Some(state) if state.refs > 1 => {
                    state.refs -= 1;
                    None
                }
                Some(state) => {
                    state.refs = 0;
                    state.oid.take()
                }
                None => None,
            }
        };
        if let Some(oid) = free {
            self.handle.remove_wred(oid).await.map_err(Self::qos_sai)?;
        }
        Ok(())
    }

    /// The map types a port should have bound, given its trust mode and
    /// which global tables carry entries.
    fn desired_maps(&self, trust: QosTrust) -> Result<BTreeSet<QosMapType>, Status> {
        let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
        let mut wanted = BTreeSet::new();
        // Classification: only the map the trust mode reads.
        if let Some(kind) = trust.map() {
            if world.map_objects.contains_key(&kind) {
                wanted.insert(kind);
            }
        }
        // Rewrite: global, so bound wherever the table has entries.
        for kind in [QosMapType::TcToDscp, QosMapType::TcToDot1p] {
            if world.map_objects.contains_key(&kind) {
                wanted.insert(kind);
            }
        }
        Ok(wanted)
    }

    /// Reconcile one physical port to the program it resolves to,
    /// touching only what changed.
    pub(crate) async fn apply_port_qos(&self, port: &str) -> Result<(), Status> {
        let resolved = self.resolved_qos(port)?;
        let (source, desired) = match resolved {
            Some((source, program)) => (source, program),
            None => (String::new(), PortQosProgram::default()),
        };
        let port_id = self.port_like_sai_id(port)?;
        let wanted_maps = self.desired_maps(desired.trust)?;
        let map_objects: BTreeMap<QosMapType, Oid> = {
            let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
            world.map_objects.clone()
        };
        let mut current = {
            let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
            world.applied.get(port).cloned().unwrap_or_default()
        };

        // Map bindings.
        for kind in MAP_TYPES {
            let want = wanted_maps.contains(&kind);
            if want == current.bound_maps.contains(&kind) {
                continue;
            }
            let map = if want {
                map_objects.get(&kind).copied()
            } else {
                None
            };
            if want && map.is_none() {
                continue;
            }
            self.handle
                .set_port_qos_map_binding(port_id, kind, map)
                .await
                .map_err(Self::qos_sai)?;
            if want {
                current.bound_maps.insert(kind);
            } else {
                current.bound_maps.remove(&kind);
            }
        }

        // Classification.
        if desired.default_tc != current.default_tc {
            self.handle
                .set_port_default_tc(port_id, desired.default_tc)
                .await
                .map_err(Self::qos_sai)?;
            current.default_tc = desired.default_tc;
        }
        current.trust = desired.trust;
        current.source = source;

        // Port shaper.
        if desired.shape_bps != current.shape_bps {
            self.handle
                .set_port_shaper(port_id, desired.shape_bps)
                .await
                .map_err(Self::qos_sai)?;
            current.shape_bps = desired.shape_bps;
        }

        // Egress queues: scheduler profile then WRED, per queue.
        for queue in 0..self.qos_queue_count() {
            let program = desired.queues.get(&queue).cloned().unwrap_or_default();
            let want_spec = program.scheduler();
            let have = current.queue_schedulers.get(&queue).copied();
            if have.map(|(spec, _)| spec) != Some(want_spec) {
                let new = self.acquire_scheduler(want_spec).await?;
                self.handle
                    .bind_queue_scheduler(port_id, u32::from(queue), new)
                    .await
                    .map_err(Self::qos_sai)?;
                if let Some((spec, _)) = have {
                    self.release_scheduler(spec).await?;
                }
                match new {
                    Some(oid) => {
                        current.queue_schedulers.insert(queue, (want_spec, oid));
                    }
                    None => {
                        current.queue_schedulers.remove(&queue);
                    }
                }
            }

            let want_wred = program.wred_profile.clone();
            let have_wred = current.queue_wreds.get(&queue).cloned();
            if have_wred.clone().unwrap_or_default() != want_wred {
                let new = if want_wred.is_empty() {
                    None
                } else {
                    Some(self.acquire_wred(&want_wred).await?)
                };
                self.handle
                    .bind_queue_wred(port_id, u32::from(queue), new)
                    .await
                    .map_err(Self::qos_sai)?;
                if let Some(old) = have_wred {
                    self.release_wred(&old).await?;
                }
                if want_wred.is_empty() {
                    current.queue_wreds.remove(&queue);
                } else {
                    current.queue_wreds.insert(queue, want_wred);
                }
            }
        }

        let default = current.trust == QosTrust::Untrusted
            && current.default_tc == 0
            && current.shape_bps.is_none()
            && current.queue_schedulers.is_empty()
            && current.queue_wreds.is_empty()
            && current.bound_maps.is_empty();
        let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
        if default {
            world.applied.remove(port);
        } else {
            world.applied.insert(port.to_string(), current);
        }
        Ok(())
    }

    /// Reconcile a set of physical ports.
    async fn apply_qos_targets(&self, targets: &[String]) -> Result<(), Status> {
        for port in targets {
            self.apply_port_qos(port).await?;
        }
        Ok(())
    }

    /// Re-expand a Port-Channel's QoS program after membership churn:
    /// stale members fall back to their own (or the default) program,
    /// current members pick the Port-Channel's up.
    pub(crate) async fn refresh_lag_qos(
        &self,
        group: u16,
        stale: &[String],
        current: &[String],
    ) -> Result<(), Status> {
        let name = format!("Port-Channel{group}");
        let programmed = {
            let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
            world.programs.contains_key(&name)
        };
        if !programmed {
            return Ok(());
        }
        for port in stale.iter().chain(current.iter()) {
            self.apply_port_qos(port).await?;
        }
        Ok(())
    }

    // --- RPC bodies ---------------------------------------------------

    pub(crate) async fn set_qos_maps_impl(&self, req: pb::SetQosMapsRequest) -> Result<(), Status> {
        let dscp_to_tc = map_entries("dscp-to-tc", 63, 7, &req.dscp_to_tc)?;
        let cos_to_tc = map_entries("cos-to-tc", 7, 7, &req.cos_to_tc)?;
        let tc_to_dscp = map_entries("tc-to-dscp", 7, 63, &req.tc_to_dscp)?;
        let tc_to_cos = map_entries("tc-to-cos", 7, 7, &req.tc_to_cos)?;
        let wanted = crate::state::QosMaps {
            dscp_to_tc,
            cos_to_tc,
            tc_to_dscp,
            tc_to_cos,
        };
        let (current, objects) = {
            let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
            (world.maps.clone(), world.map_objects.clone())
        };
        if current == wanted {
            return Ok(());
        }
        for kind in MAP_TYPES {
            let table = wanted.table(kind);
            if !table.is_empty() {
                self.require_qos_maps(kind)?;
            }
        }

        // Objects first: rewrite in place where one exists (so bound
        // ports keep their bindings), create where a table just gained
        // its first entry. Removals happen after the unbinds below.
        let mut removals: Vec<Oid> = Vec::new();
        for kind in MAP_TYPES {
            let table = wanted.table(kind);
            let entries: Vec<(u8, u8)> = table.iter().map(|(k, v)| (*k, *v)).collect();
            match (objects.get(&kind).copied(), table.is_empty()) {
                (Some(oid), false) => {
                    if current.table(kind) != table {
                        self.handle
                            .set_qos_map(oid, entries)
                            .await
                            .map_err(Self::qos_sai)?;
                    }
                }
                (Some(oid), true) => {
                    // Drop the published object now so the rebind pass
                    // below unbinds it; the SAI removal follows once no
                    // port holds it.
                    removals.push(oid);
                    let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
                    world.map_objects.remove(&kind);
                }
                (None, false) => {
                    let oid = self
                        .handle
                        .create_qos_map(kind, entries)
                        .await
                        .map_err(Self::qos_sai)?;
                    let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
                    world.map_objects.insert(kind, oid);
                }
                (None, true) => {}
            }
        }
        {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            world.maps = wanted;
        }

        // Rebind: a port is only touched when its own binding set
        // changed (a value-only edit rebinds nothing at all).
        let ports = self.qos_ports_ordered()?;
        self.apply_qos_targets(&ports).await?;

        for oid in removals {
            self.handle
                .remove_qos_map(oid)
                .await
                .map_err(Self::qos_sai)?;
        }
        Ok(())
    }

    pub(crate) async fn set_port_qos_impl(&self, req: pb::SetPortQosRequest) -> Result<(), Status> {
        let trust = QosTrust::parse(&req.trust).ok_or_else(|| {
            Status::invalid_argument(format!(
                "{}: trust must be dscp, cos or untrusted (got {:?})",
                req.port, req.trust
            ))
        })?;
        let default_tc = u8::try_from(req.default_tc)
            .ok()
            .filter(|tc| *tc <= 7)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "{}: default-tc {} is out of range (0..7)",
                    req.port, req.default_tc
                ))
            })?;
        let queue_count = self.qos_queue_count();
        let mut queues: BTreeMap<u8, QueueQosProgram> = BTreeMap::new();
        for queue in &req.queues {
            let index = u8::try_from(queue.queue)
                .ok()
                .filter(|q| *q < queue_count)
                .ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "{}: queue {} is out of range (0..{})",
                        req.port,
                        queue.queue,
                        queue_count - 1
                    ))
                })?;
            let weight = u8::try_from(queue.weight)
                .ok()
                .filter(|w| *w <= 127)
                .ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "{} queue {index}: weight {} is out of range (1..127)",
                        req.port, queue.weight
                    ))
                })?;
            if queue.strict && queue.weight > 0 {
                return Err(Status::invalid_argument(format!(
                    "{} queue {index}: strict and weight are mutually exclusive",
                    req.port
                )));
            }
            if queue.shape_bps.is_some() && !self.handle.capabilities.queue_shaper {
                return Err(Status::failed_precondition(
                    "per-queue shapers are not supported by this platform's SAI",
                ));
            }
            if !queue.wred_profile.is_empty() {
                let known = {
                    let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
                    world.wred_profiles.contains_key(&queue.wred_profile)
                };
                if !known {
                    return Err(Status::failed_precondition(format!(
                        "{} queue {index}: no such qos wred-profile {:?}",
                        req.port, queue.wred_profile
                    )));
                }
            }
            queues.insert(
                index,
                QueueQosProgram {
                    strict: queue.strict,
                    weight: if weight == 0 { 1 } else { weight },
                    shape_bps: queue.shape_bps,
                    wred_profile: queue.wred_profile.clone(),
                },
            );
        }
        // Queues left at the platform default carry no program at all,
        // so a re-push of an unchanged port diffs to nothing.
        queues.retain(|_, program| *program != QueueQosProgram::default());

        let targets = self.physical_targets(&req.port)?;
        {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            world.programs.insert(
                req.port.clone(),
                PortQosProgram {
                    trust,
                    default_tc,
                    shape_bps: req.shape_bps,
                    queues,
                },
            );
        }
        self.apply_qos_targets(&targets).await
    }

    pub(crate) async fn clear_port_qos_impl(&self, port: &str) -> Result<(), Status> {
        let targets = self.physical_targets(port)?;
        {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            world.programs.remove(port);
        }
        self.apply_qos_targets(&targets).await
    }

    pub(crate) async fn ensure_wred_profile_impl(
        &self,
        req: pb::EnsureWredProfileRequest,
    ) -> Result<(), Status> {
        let profile = req
            .profile
            .ok_or_else(|| Status::invalid_argument("wred profile is required"))?;
        if profile.name.is_empty() {
            return Err(Status::invalid_argument("wred profile name is required"));
        }
        let spec = wred_spec(&profile)?;
        let buffer_kb = self.handle.capabilities.buffer_bytes_total / 1024;
        if buffer_kb > 0 && u64::from(profile.max_threshold_kb) > buffer_kb {
            return Err(Status::failed_precondition(format!(
                "qos wred-profile {}: max-threshold {} KB exceeds this platform's {buffer_kb} KB packet buffer",
                profile.name, profile.max_threshold_kb
            )));
        }
        // A live object updates in place, so bound queues keep their
        // binding and their counters.
        let live = {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            let state = world.wred_profiles.entry(profile.name.clone()).or_default();
            let unchanged = state.spec == spec;
            state.spec = spec;
            if unchanged {
                None
            } else {
                state.oid
            }
        };
        if let Some(oid) = live {
            if spec.ecn && !self.handle.capabilities.ecn {
                return Err(Status::failed_precondition(
                    "ECN marking is not supported by this platform's SAI",
                ));
            }
            self.handle
                .set_wred(oid, spec)
                .await
                .map_err(Self::qos_sai)?;
        }
        Ok(())
    }

    pub(crate) async fn remove_wred_profile_impl(&self, name: &str) -> Result<(), Status> {
        let free = {
            let mut world = self.handle.qos.write().map_err(|_| Self::qos_poisoned())?;
            let Some(state) = world.wred_profiles.get(name) else {
                return Ok(());
            };
            if state.refs > 0 {
                return Err(Status::failed_precondition(format!(
                    "qos wred-profile {name} is still bound by {} queue(s)",
                    state.refs
                )));
            }
            world.wred_profiles.remove(name).and_then(|state| state.oid)
        };
        if let Some(oid) = free {
            self.handle.remove_wred(oid).await.map_err(Self::qos_sai)?;
        }
        Ok(())
    }

    pub(crate) async fn qos_state_impl(&self) -> Result<pb::GetQosStateResponse, Status> {
        let caps = self.handle.capabilities;
        let queue_count = self.qos_queue_count();
        let (maps, profiles) = {
            let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
            (
                world.maps.clone(),
                world
                    .wred_profiles
                    .iter()
                    .map(|(name, state)| profile_to_proto(name, state))
                    .collect::<Vec<_>>(),
            )
        };

        // Every front-panel port, then every configured Port-Channel:
        // the summary renderer filters on `configured`, the per-port
        // one wants a row for any port the operator names.
        let mut rows = Vec::new();
        let mut default_ports = 0u32;
        for name in self.qos_ports_ordered()? {
            let row = self.qos_port_row(&name, queue_count, true)?;
            if !row.configured {
                default_ports += 1;
            }
            rows.push(row);
        }
        let lag_names: Vec<String> = {
            let world = self.handle.qos.read().map_err(|_| Self::qos_poisoned())?;
            world
                .programs
                .keys()
                .filter(|name| lag_group_of(name).is_some())
                .cloned()
                .collect()
        };
        for name in lag_names {
            rows.push(self.qos_port_row(&name, queue_count, false)?);
        }

        Ok(pb::GetQosStateResponse {
            dscp_to_tc: entries_to_proto(&maps.dscp_to_tc),
            cos_to_tc: entries_to_proto(&maps.cos_to_tc),
            tc_to_dscp: entries_to_proto(&maps.tc_to_dscp),
            tc_to_cos: entries_to_proto(&maps.tc_to_cos),
            wred_profiles: profiles,
            ports: rows,
            default_ports,
            buffer_kb: u32::try_from(caps.buffer_bytes_total / 1024).unwrap_or(0),
            wred_supported: caps.wred,
            ecn_supported: caps.ecn,
            queue_shaper_supported: caps.queue_shaper,
            queue_count: u32::from(queue_count),
        })
    }

    /// One port's row: effective program plus, for physical ports, the
    /// live per-queue counters.
    fn qos_port_row(
        &self,
        name: &str,
        queue_count: u8,
        with_counters: bool,
    ) -> Result<pb::PortQosState, Status> {
        let resolved = self.resolved_qos(name)?;
        let (source, program) = match resolved {
            Some((source, program)) => (source, program),
            None => (String::new(), PortQosProgram::default()),
        };
        let ecn_of = |profile: &str| -> bool {
            self.handle
                .qos
                .read()
                .ok()
                .and_then(|world| world.wred_profiles.get(profile).map(|state| state.spec.ecn))
                .unwrap_or(false)
        };
        let counters = if with_counters {
            self.queue_samples(name)
        } else {
            Vec::new()
        };
        let queues = (0..queue_count)
            .map(|queue| {
                let q = program.queues.get(&queue).cloned().unwrap_or_default();
                let sample = counters.iter().find(|s| s.label == format!("UC{queue}"));
                pb::QueueQosState {
                    queue: u32::from(queue),
                    strict: q.strict,
                    weight: u32::from(if q.weight == 0 { 1 } else { q.weight }),
                    shape_bps: q.shape_bps,
                    ecn: !q.wred_profile.is_empty() && ecn_of(&q.wred_profile),
                    wred_profile: q.wred_profile,
                    tx_packets: sample.map(|s| s.pkts).unwrap_or(0),
                    tx_bytes: sample.map(|s| s.bytes).unwrap_or(0),
                    dropped: sample.map(|s| s.dropped_pkts).unwrap_or(0),
                    wred_dropped: sample.map(|s| s.wred_dropped).unwrap_or(0),
                    ecn_marked: sample.map(|s| s.ecn_marked).unwrap_or(0),
                }
            })
            .collect();
        Ok(pb::PortQosState {
            port: name.to_string(),
            trust: program.trust.word().to_string(),
            default_tc: u32::from(program.default_tc),
            shape_bps: program.shape_bps,
            queues,
            configured: !source.is_empty(),
            via_port_channel: if source == name {
                String::new()
            } else {
                source
            },
        })
    }
}
