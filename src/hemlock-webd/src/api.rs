//! The JSON API the web UI talks to. Read-only in phase 1: state is
//! fetched from syncd (interfaces, VLANs) and mgmtd (running config)
//! per request — webd holds no cache to go stale.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{FromRequestParts, State};
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
    pub sessions: Sessions,
    pub dev_auth: Option<(String, String)>,
    pub secure_cookie: bool,
}

pub type SharedState = Arc<AppState>;

const SESSION_COOKIE: &str = "hemlock_session";

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/session", get(session))
        .route("/api/interfaces", get(interfaces))
        .route("/api/vlans", get(vlans))
        .route("/api/routes", get(routes))
        .route("/api/system", get(system))
        .route("/api/config", get(config))
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
