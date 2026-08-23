//! The JSON API the web UI talks to. Read-only in phase 1: state is
//! fetched from syncd (interfaces, VLANs) and mgmtd (running config)
//! per request — webd holds no cache to go stale.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, FromRequestParts, State};
use axum::http::{header, request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;
use hemlock_config::ConfigTree;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, Sessions};

pub struct AppState {
    pub mgmtd: IpcEndpoint,
    pub syncd: IpcEndpoint,
    pub orch: IpcEndpoint,
    pub sessions: Sessions,
    pub dev_auth: Option<(String, String)>,
    pub secure_cookie: bool,
    /// webd state directory (TLS material, staged upgrade images).
    pub state_dir: PathBuf,
}

pub type SharedState = Arc<AppState>;

const SESSION_COOKIE: &str = "hemlock_session";

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session))
        .route("/api/interfaces", get(interfaces))
        .route("/api/interfaces/edit", post(interfaces_edit))
        .route("/api/vlans", get(vlans))
        .route("/api/vlans/edit", post(vlans_edit))
        .route("/api/lags", get(lags))
        .route("/api/lags/edit", post(lags_edit))
        .route("/api/spanning-tree", get(spanning_tree))
        .route("/api/spanning-tree/edit", post(spanning_tree_edit))
        .route("/api/spanning-tree/clear-errdisable", post(clear_errdisable))
        .route("/api/mac-table", get(mac_table))
        .route("/api/mac-table/edit", post(mac_table_edit))
        .route("/api/mac-table/flush", post(mac_table_flush))
        .route("/api/snooping", get(snooping))
        .route("/api/snooping/edit", post(snooping_edit))
        .route("/api/storm-control", get(storm_control))
        .route("/api/storm-control/edit", post(storm_control_edit))
        .route("/api/mirror", get(mirror))
        .route("/api/mirror/edit", post(mirror_edit))
        .route("/api/routes", get(routes))
        .route("/api/system", get(system))
        .route("/api/users", get(users))
        .route("/api/users/add", post(users_add))
        .route("/api/config", get(config))
        .route("/api/config/restore", post(config_restore))
        .route("/api/maintenance", get(maintenance))
        .route("/api/reboot", post(reboot))
        .route("/api/reboot/cancel", post(reboot_cancel))
        // Firmware images stream straight to disk; lift axum's 2 MB
        // default body cap for this one route.
        .route(
            "/api/upgrade/upload",
            post(upgrade_upload).layer(DefaultBodyLimit::max(4 * 1024 * 1024 * 1024)),
        )
        .route("/api/upgrade/apply", post(upgrade_apply))
        .route("/api/upgrade/discard", post(upgrade_discard))
        .with_state(state)
}

// ---------------------------------------------------------------- errors

/// Handler error: upstream daemon trouble surfaces as 502 with the
/// cause, everything else as 500.
struct ApiError(anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::warn!(err = %format!("{:#}", self.0), "api request failed");
        (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

// ------------------------------------------------------------ auth layer

fn cookie_token(parts: &Parts) -> Option<String> {
    let cookies = parts.headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}

/// Extractor gating every state endpoint on a live session.
struct Operator(#[allow(dead_code)] String);

impl FromRequestParts<SharedState> for Operator {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        cookie_token(parts)
            .and_then(|token| state.sessions.touch(&token))
            .map(Operator)
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "not signed in" })),
                )
                    .into_response()
            })
    }
}

// --------------------------------------------------------------- clients

async fn syncd_client(
    state: &AppState,
) -> anyhow::Result<pb::syncd_client::SyncdClient<tonic::transport::Channel>> {
    let channel = state.syncd.connect().await?;
    Ok(pb::syncd_client::SyncdClient::new(channel))
}

async fn mgmtd_client(
    state: &AppState,
) -> anyhow::Result<pb::mgmt_client::MgmtClient<tonic::transport::Channel>> {
    let channel = state.mgmtd.connect().await?;
    Ok(pb::mgmt_client::MgmtClient::new(channel))
}

/// The running config text from mgmtd (also used at startup for the
/// listener decision, hence public and endpoint-based).
pub async fn running_config(mgmtd: &IpcEndpoint) -> anyhow::Result<String> {
    let channel = mgmtd.connect().await?;
    let mut client = pb::mgmt_client::MgmtClient::new(channel);
    Ok(client
        .get_config(pb::GetConfigRequest {
            source: pb::ConfigSource::Running as i32,
        })
        .await?
        .into_inner()
        .text)
}

/// Which `system { ... }` service blocks exist in a config tree.
pub struct Services {
    pub ssh: bool,
    pub http: bool,
    pub https: bool,
}

pub fn services_of(tree: &ConfigTree) -> Services {
    let block = |name: &str| -> bool {
        tree.block("system")
            .map(|(_, system)| ConfigTree::blocks_named(system, name).next().is_some())
            .unwrap_or(false)
    };
    Services {
        ssh: block("ssh"),
        http: block("http"),
        https: block("https"),
    }
}

// ----------------------------------------------------------------- login

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(State(state): State<SharedState>, Json(req): Json<LoginRequest>) -> Response {
    match auth::verify(state.dev_auth.as_ref(), &req.username, &req.password).await {
        Ok(()) => {
            let token = state.sessions.create(&req.username);
            tracing::info!(username = %req.username, "web login");
            (
                [(header::SET_COOKIE, session_cookie(&state, &token, false))],
                Json(json!({ "username": req.username })),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

fn session_cookie(state: &AppState, token: &str, clear: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax");
    if clear {
        cookie.push_str("; Max-Age=0");
    }
    if state.secure_cookie {
        cookie.push_str("; Secure");
    }
    cookie
}

async fn logout(State(state): State<SharedState>, parts: Parts) -> Response {
    if let Some(token) = cookie_token(&parts) {
        state.sessions.remove(&token);
    }
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session_cookie(&state, "", true))],
    )
        .into_response()
}

async fn session(_op: Operator, State(_state): State<SharedState>) -> Response {
    // Operator resolved the session; echo the username back.
    Json(json!({ "username": _op.0 })).into_response()
}

// ----------------------------------------------------------------- state

#[derive(Serialize)]
struct InterfaceJson {
    name: String,
    kind: String,
    index: u32,
    admin_up: bool,
    oper_up: bool,
    description: String,
    mac: String,
    mtu: u32,
    speed_mbps: u64,
    addresses: Vec<String>,
    switchport_mode: String,
    access_vlan: u32,
    native_vlan: u32,
    trunk_vlans: Vec<u32>,
    /// Physical media for display, e.g. "1000BASE-T", "SFP+".
    media: String,
}

fn interface_json(i: &pb::InterfaceState) -> InterfaceJson {
    InterfaceJson {
        name: i.name.clone(),
        kind: i.kind.clone(),
        index: i.index,
        admin_up: i.admin_state != pb::AdminState::Down as i32,
        oper_up: i.oper_status == pb::OperStatus::Up as i32,
        description: i.description.clone(),
        mac: i.mac.clone(),
        mtu: i.mtu,
        speed_mbps: i.speed_mbps,
        addresses: i.ip_addresses.clone(),
        switchport_mode: i.switchport_mode.clone(),
        access_vlan: i.access_vlan,
        native_vlan: i.native_vlan,
        trunk_vlans: i.trunk_vlans.clone(),
        media: i.media.clone(),
    }
}

async fn fetch_interfaces(state: &AppState) -> anyhow::Result<pb::GetInterfacesResponse> {
    let mut client = syncd_client(state).await?;
    Ok(client
        .get_interfaces(pb::GetInterfacesRequest { names: vec![] })
        .await?
        .into_inner())
}

async fn interfaces(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let response = fetch_interfaces(&state).await?;
    let interfaces: Vec<InterfaceJson> = response.interfaces.iter().map(interface_json).collect();
    Ok(Json(json!({ "interfaces": interfaces })))
}

#[derive(Serialize)]
struct SviJson {
    name: String,
    address: Option<String>,
}

#[derive(Serialize)]
struct VlanJson {
    id: u32,
    name: String,
    svi: Option<SviJson>,
    untagged: Vec<String>,
    tagged: Vec<String>,
}

async fn vlans(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let response = fetch_interfaces(&state).await?;
    let mut vlans: BTreeMap<u32, VlanJson> = response
        .active_vlans
        .iter()
        .map(|&id| {
            (
                id,
                VlanJson {
                    id,
                    name: response.vlan_names.get(&id).cloned().unwrap_or_default(),
                    svi: None,
                    untagged: vec![],
                    tagged: vec![],
                },
            )
        })
        .collect();

    for iface in &response.interfaces {
        match iface.kind.as_str() {
            // L2 memberships: routed (addressed) ports are not bridge
            // members, everything else is untagged in its access/native
            // VLAN and tagged on its trunk list.
            "ethernet" if iface.ip_addresses.is_empty() => {
                let default = |id: u32| if id == 0 { 1 } else { id };
                if iface.switchport_mode == "trunk" {
                    let native = default(iface.native_vlan);
                    if let Some(vlan) = vlans.get_mut(&native) {
                        vlan.untagged.push(iface.name.clone());
                    }
                    for id in &iface.trunk_vlans {
                        if let Some(vlan) = vlans.get_mut(id) {
                            vlan.tagged.push(iface.name.clone());
                        }
                    }
                } else {
                    let access = default(iface.access_vlan);
                    if let Some(vlan) = vlans.get_mut(&access) {
                        vlan.untagged.push(iface.name.clone());
                    }
                }
            }
            // SVIs: the Vlan<id> interface carries the VLAN's address.
            "vlan" => {
                if let Some(id) = iface.name.strip_prefix("Vlan").and_then(|d| d.parse().ok()) {
                    if let Some(vlan) = vlans.get_mut(&id) {
                        vlan.svi = Some(SviJson {
                            name: iface.name.clone(),
                            address: iface.ip_addresses.first().cloned(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    let vlans: Vec<VlanJson> = vlans.into_values().collect();
    Ok(Json(json!({ "vlans": vlans })))
}

// ----------------------------------------------------------------- edits

/// Run one config edit through mgmtd: base on the running config, apply
/// the tree edit, SetCandidate (validation), Commit. Rejections come
/// back as 422 with the validator's messages; the response carries the
/// commit's applied-changes list.
async fn commit_edit(
    state: &AppState,
    comment: &str,
    apply: impl FnOnce(&mut ConfigTree) -> Result<(), String>,
) -> Result<Response, ApiError> {
    let invalid = |errors: Vec<String>| {
        Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "errors": errors })),
        )
            .into_response())
    };

    let text = running_config(&state.mgmtd).await?;
    let mut tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    tree.normalize_interfaces();
    if let Err(message) = apply(&mut tree) {
        return invalid(vec![message]);
    }

    let mut client = mgmtd_client(state).await?;
    let response = client
        .set_candidate(pb::ConfigText {
            text: tree.to_text(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    if !response.valid {
        return invalid(response.errors);
    }
    let commit = client
        .commit(pb::CommitRequest {
            comment: comment.to_string(),
            confirm_timeout_secs: 0,
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    tracing::info!(commit_id = commit.commit_id, "web console commit");
    Ok(Json(json!({
        "commit_id": commit.commit_id,
        "applied": commit.applied_changes,
    }))
    .into_response())
}

async fn interfaces_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::edit::InterfaceEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::edit::apply_interface_edit(tree, &edit)
    })
    .await
}

async fn vlans_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::edit::VlanEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::edit::apply_vlan_edit(tree, &edit)
    })
    .await
}

async fn users(_op: Operator, State(state): State<SharedState>) -> Response {
    Json(json!({ "users": crate::users::list(state.dev_auth.as_ref()) })).into_response()
}

async fn users_add(
    _op: Operator,
    State(_state): State<SharedState>,
    Json(request): Json<crate::users::AddUserRequest>,
) -> Response {
    match crate::users::add(&request).await {
        Ok(()) => Json(json!({ "username": request.username })).into_response(),
        Err(message) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "errors": [message] })),
        )
            .into_response(),
    }
}

async fn orch_client(
    state: &AppState,
) -> anyhow::Result<pb::orch_client::OrchClient<tonic::transport::Channel>> {
    let channel = state.orch.connect().await?;
    Ok(pb::orch_client::OrchClient::new(channel))
}

// ----------------------------------------------------- switching suite

/// `GET /api/lags` — syncd identity/membership merged with orch's LACP
/// runtime (which degrades gracefully when orch is unreachable).
async fn lags(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let lags = client
        .get_lags(pb::GetLagsRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner()
        .lags;
    let lacp = match orch_client(&state).await {
        Ok(mut orch) => orch
            .get_lacp_state(pb::GetLacpStateRequest {})
            .await
            .map(|r| r.into_inner())
            .ok(),
        Err(_) => None,
    };
    let rows: Vec<serde_json::Value> = lags
        .iter()
        .map(|lag| {
            let state = lacp
                .as_ref()
                .and_then(|l| l.lags.iter().find(|s| s.group == lag.group));
            json!({
                "group": lag.group,
                "description": lag.description,
                "admin_up": lag.admin_up,
                "up": state.map(|s| s.up).unwrap_or_else(|| {
                    lag.members.iter().any(|m| m.enabled && m.oper_up)
                }),
                "lacp": state.map(|s| s.lacp).unwrap_or(true),
                "active_mode": state.map(|s| s.active_mode).unwrap_or(false),
                "bundled": state.map(|s| s.bundled).unwrap_or(0),
                "total": lag.members.len(),
                "min_links": state.map(|s| s.min_links).unwrap_or(0),
                "fallback_mode": state.map(|s| s.fallback_mode.clone()).unwrap_or_default(),
                "fallback_timeout_secs": state.map(|s| s.fallback_timeout_secs).unwrap_or(90),
                "fallback_active": state.map(|s| s.fallback_active).unwrap_or(false),
                "members": lag.members.iter().map(|member| {
                    let lacp_member = state.and_then(|s| {
                        s.members.iter().find(|m| m.port == member.port)
                    });
                    json!({
                        "port": member.port,
                        "enabled": member.enabled,
                        "oper_up": member.oper_up,
                        "status": lacp_member.map(|m| m.status.clone()).unwrap_or_else(|| {
                            if member.enabled && member.oper_up { "bundled" }
                            else if member.oper_up { "standby" } else { "down" }.into()
                        }),
                        "partner_system": lacp_member
                            .map(|m| m.partner_system.clone())
                            .unwrap_or_default(),
                        "partner_port": lacp_member.map(|m| m.partner_port).unwrap_or(0),
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({ "lags": rows })))
}

async fn lags_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::switching_edit::LagEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::switching_edit::apply_lag_edit(tree, &edit)
    })
    .await
}

/// `GET /api/spanning-tree` — orch's bridge view.
async fn spanning_tree(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let stp = orch
        .get_stp_state(pb::GetStpStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "mode": stp.mode,
        "bridge_priority": stp.bridge_priority,
        "bridge_mac": stp.bridge_mac,
        "root_priority": stp.root_priority,
        "root_mac": stp.root_mac,
        "is_root": stp.is_root,
        "root_cost": stp.root_cost,
        "root_port": stp.root_port,
        "hello_time": stp.hello_time,
        "max_age": stp.max_age,
        "forward_time": stp.forward_time,
        "mst_name": stp.mst_name,
        "mst_revision": stp.mst_revision,
        "instances": stp.instances.iter().map(|map| json!({
            "instance": map.instance,
            "vlans": map.vlans,
        })).collect::<Vec<_>>(),
        "topology_changes": stp.topology_changes,
        "seconds_since_tc": stp.seconds_since_tc,
        "last_tc_port": stp.last_tc_port,
        "ports": stp.ports.iter().map(|p| json!({
            "port": p.port,
            "role": p.role,
            "state": p.state,
            "cost": p.cost,
            "priority": p.priority,
            "portfast": p.portfast,
            "bpduguard": p.bpduguard,
            "errdisabled": p.errdisabled,
        })).collect::<Vec<_>>(),
    })))
}

async fn spanning_tree_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::switching_edit::StpEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::switching_edit::apply_stp_edit(tree, &edit)
    })
    .await
}

#[derive(Deserialize)]
struct ClearErrdisableRequest {
    port: String,
}

/// `POST /api/spanning-tree/clear-errdisable` — re-enable a
/// BPDU-guard-errdisabled port.
async fn clear_errdisable(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<ClearErrdisableRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    client
        .set_port_errdisable(pb::SetPortErrdisableRequest {
            name: request.port.clone(),
            reason: String::new(),
        })
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(json!({ "port": request.port })))
}

#[derive(Deserialize)]
struct MacTableQuery {
    #[serde(default)]
    vlan: u32,
    #[serde(default)]
    port: String,
    #[serde(default)]
    mac: String,
    /// "" | "static" | "dynamic".
    #[serde(default)]
    kind: String,
    #[serde(default)]
    page_size: u32,
    #[serde(default)]
    page_token: String,
}

/// `GET /api/mac-table` — paged/filtered dump from syncd.
async fn mac_table(
    _op: Operator,
    State(state): State<SharedState>,
    axum::extract::Query(query): axum::extract::Query<MacTableQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .dump_fdb(pb::DumpFdbRequest {
            vlan: query.vlan,
            port: query.port,
            mac: query.mac,
            kind: match query.kind.as_str() {
                "static" => pb::FdbEntryKind::Static,
                "dynamic" => pb::FdbEntryKind::Dynamic,
                _ => pb::FdbEntryKind::Unspecified,
            } as i32,
            page_size: query.page_size,
            page_token: query.page_token,
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "aging_time_secs": response.aging_time_secs,
        "total": response.total,
        "next_page_token": response.next_page_token,
        "entries": response.entries.iter().map(|e| json!({
            "vlan": e.vlan,
            "mac": e.mac,
            "port": e.port,
            "drop": e.drop,
            "is_static": e.is_static,
            "moves": e.moves,
            "seconds_since_move": e.seconds_since_move,
        })).collect::<Vec<_>>(),
    })))
}

async fn mac_table_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::switching_edit::MacTableEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::switching_edit::apply_mac_table_edit(tree, &edit)
    })
    .await
}

#[derive(Deserialize)]
struct FlushRequest {
    #[serde(default)]
    vlan: u32,
    #[serde(default)]
    port: String,
}

/// `POST /api/mac-table/flush` — flush dynamic entries (scoped).
async fn mac_table_flush(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<FlushRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .flush_fdb(pb::FlushFdbRequest {
            vlan: request.vlan,
            port: request.port,
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({ "flushed": response.flushed })))
}

#[derive(Deserialize)]
struct SnoopingQuery {
    #[serde(default = "default_family")]
    family: String,
}

fn default_family() -> String {
    "igmp".into()
}

/// `GET /api/snooping?family=igmp|mld` — orch's snooping view.
async fn snooping(
    _op: Operator,
    State(state): State<SharedState>,
    axum::extract::Query(query): axum::extract::Query<SnoopingQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .get_snooping_state(pb::GetSnoopingStateRequest {
            family: query.family,
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "enabled": response.enabled,
        "robustness": response.robustness,
        "vlans": response.vlans.iter().map(|vlan| json!({
            "vlan": vlan.vlan,
            "enabled": vlan.enabled,
            "fast_leave": vlan.fast_leave,
            "querier_enabled": vlan.querier_enabled,
            "querier_address": vlan.querier_address,
            "querier_active": vlan.querier_active,
            "static_mrouters": vlan.static_mrouters,
            "dynamic_mrouters": vlan.dynamic_mrouters,
            "groups": vlan.groups.iter().map(|group| json!({
                "group": group.group,
                "version": group.version,
                "ports": group.ports,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })))
}

async fn snooping_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::switching_edit::SnoopingEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::switching_edit::apply_snooping_edit(tree, &edit)
    })
    .await
}

/// `GET /api/storm-control` — syncd's per-port levels, rates, drops.
async fn storm_control(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .get_storm_control(pb::GetStormControlRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "entries": response.entries.iter().map(|e| json!({
            "name": e.name,
            "kind": match e.class {
                c if c == pb::StormClass::Broadcast as i32 => "broadcast",
                c if c == pb::StormClass::Multicast as i32 => "multicast",
                _ => "unknown-unicast",
            },
            "level": e.level,
            "rate_kbps": e.rate_kbps,
            "drops": e.drops,
            "active": e.active,
        })).collect::<Vec<_>>(),
    })))
}

async fn storm_control_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::switching_edit::StormEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::switching_edit::apply_storm_edit(tree, &edit)
    })
    .await
}

/// `GET /api/mirror` — syncd's session state.
async fn mirror(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .get_mirror_sessions(pb::GetMirrorSessionsRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "sessions": response.sessions.iter().map(|s| json!({
            "session": s.session,
            "destination": s.destination,
            "destination_up": s.destination_up,
            "sources": s.sources.iter().map(|source| json!({
                "port": source.name,
                "direction": match source.direction {
                    d if d == pb::MirrorDirection::Rx as i32 => "rx",
                    d if d == pb::MirrorDirection::Tx as i32 => "tx",
                    _ => "both",
                },
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })))
}

async fn mirror_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::switching_edit::MirrorEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, "web console", |tree| {
        crate::switching_edit::apply_mirror_edit(tree, &edit)
    })
    .await
}

async fn routes(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let mut static_routes = Vec::new();
    if let Some((_, routing)) = tree.block("routing") {
        if let Some((_, statics)) = ConfigTree::blocks_named(routing, "static").next() {
            for item in statics {
                if let hemlock_config::Item::Leaf { name, values } = item {
                    static_routes.push(json!({
                        "prefix": name,
                        "next_hop": values.first().cloned().unwrap_or_default(),
                    }));
                }
            }
        }
    }
    Ok(Json(json!({ "static_routes": static_routes })))
}

async fn system(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let info = client
        .get_switch_info(pb::GetSwitchInfoRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    let platform_model = fetch_interfaces(&state)
        .await
        .map(|r| r.platform_model)
        .unwrap_or_default();

    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let services = services_of(&tree);

    Ok(Json(json!({
        "hostname": crate::hostname(),
        "version": hemlock_common::VERSION,
        "platform_id": info.platform_id,
        "platform_model": platform_model,
        "backend": info.backend,
        "port_count": info.port_count,
        "uptime_secs": uptime_secs(),
        "services": {
            "ssh": services.ssh,
            "http": services.http,
            "https": services.https,
        },
    })))
}

fn uptime_secs() -> Option<u64> {
    let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
    let seconds: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    Some(seconds as u64)
}

async fn config(_op: Operator, State(state): State<SharedState>) -> Result<Response, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

// ----------------------------------------------------------- maintenance

fn errors(errors: Vec<String>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "errors": errors })),
    )
        .into_response()
}

async fn maintenance(_op: Operator, State(state): State<SharedState>) -> Response {
    Json(json!({
        "hostname": crate::hostname(),
        "version": hemlock_common::VERSION,
        "scheduled_reboot": crate::maint::scheduled_shutdown(),
        "staged_image": crate::maint::staged_info(&state.state_dir),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RestoreRequest {
    text: String,
}

/// Replace the whole configuration with an uploaded backup: parse it,
/// run it through mgmtd's validator, commit.
async fn config_restore(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<RestoreRequest>,
) -> Result<Response, ApiError> {
    let restored = match hemlock_config::parse(&request.text) {
        Ok(tree) => tree,
        Err(e) => return Ok(errors(vec![format!("configuration does not parse: {e}")])),
    };
    commit_edit(&state, "web console restore", |tree| {
        *tree = restored;
        tree.normalize_interfaces();
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
struct RebootRequest {
    /// 0 = reboot now; otherwise minutes from now.
    #[serde(default)]
    in_minutes: u64,
}

async fn reboot(
    _op: Operator,
    State(_state): State<SharedState>,
    Json(request): Json<RebootRequest>,
) -> Response {
    if !cfg!(unix) {
        return errors(vec!["reboot is only available on the switch".to_string()]);
    }
    if request.in_minutes == 0 {
        crate::maint::reboot_now();
        return Json(json!({ "rebooting": true })).into_response();
    }
    if request.in_minutes > 7 * 24 * 60 {
        return errors(vec!["reboot delay must be at most a week".to_string()]);
    }
    match crate::maint::schedule_reboot(request.in_minutes).await {
        Ok(at_unix) => Json(json!({ "scheduled": true, "at_unix": at_unix })).into_response(),
        Err(message) => errors(vec![message]),
    }
}

async fn reboot_cancel(_op: Operator, State(_state): State<SharedState>) -> Response {
    match crate::maint::cancel_reboot().await {
        Ok(()) => Json(json!({ "cancelled": true })).into_response(),
        Err(message) => errors(vec![message]),
    }
}

/// Raw-body image upload, streamed to the staging area on disk.
async fn upgrade_upload(
    _op: Operator,
    State(state): State<SharedState>,
    body: axum::body::Body,
) -> Response {
    match crate::maint::stage_upload(&state.state_dir, body.into_data_stream()).await {
        Ok(staged) => Json(json!({ "staged_image": staged })).into_response(),
        Err(message) => errors(vec![message]),
    }
}

#[derive(Deserialize)]
struct UpgradeApplyRequest {
    /// Reboot into the new image once it is written (the default).
    #[serde(default = "default_true")]
    reboot: bool,
    /// Install even if the image targets a different platform.
    #[serde(default)]
    force: bool,
}

fn default_true() -> bool {
    true
}

async fn upgrade_apply(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<UpgradeApplyRequest>,
) -> Result<Response, ApiError> {
    let staged = crate::maint::staged_path(&state.state_dir);
    if !staged.exists() {
        return Ok(errors(vec!["no staged image".to_string()]));
    }
    // mgmtd performs the install (shared engine, serialized against
    // commits) — the same path `hemlockctl upgrade` takes.
    let path = staged.canonicalize().unwrap_or(staged);
    let mut client = mgmtd_client(&state).await?;
    match client
        .install_image(pb::InstallImageRequest {
            path: path.display().to_string(),
            force: request.force,
            reboot: false,
        })
        .await
    {
        Ok(response) => {
            let response = response.into_inner();
            crate::maint::discard_staged(&state.state_dir).await;
            if request.reboot {
                crate::maint::reboot_now();
            }
            Ok(
                Json(json!({ "version": response.version, "rebooting": request.reboot }))
                    .into_response(),
            )
        }
        Err(status) => Ok(errors(vec![status.message().to_string()])),
    }
}

async fn upgrade_discard(_op: Operator, State(state): State<SharedState>) -> Response {
    crate::maint::discard_staged(&state.state_dir).await;
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_service_blocks() {
        let tree = hemlock_config::parse("system {\n    ssh {\n    }\n    http {\n    }\n}\n")
            .unwrap_or_default();
        let services = services_of(&tree);
        assert!(services.ssh);
        assert!(services.http);
        assert!(!services.https);

        let services = services_of(&ConfigTree::default());
        assert!(!services.ssh && !services.http && !services.https);
    }
}
