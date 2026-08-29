//! The JSON API the web UI talks to. Read-only in phase 1: state is
//! fetched from syncd (interfaces, VLANs) and mgmtd (running config)
//! per request — webd holds no cache to go stale.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, FromRequestParts, State};
use axum::http::{header, request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, MethodRouter};
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

/// Every read-only endpoint: (path, handler). Kept as a table so
/// the router and the role-gate test read the same list.
fn get_routes() -> Vec<(&'static str, MethodRouter<SharedState>)> {
    vec![
        ("/api/session", get(session)),
        ("/api/interfaces", get(interfaces)),
        ("/api/vlans", get(vlans)),
        ("/api/svis", get(svis)),
        ("/api/lags", get(lags)),
        ("/api/spanning-tree", get(spanning_tree)),
        ("/api/mac-table", get(mac_table)),
        ("/api/snooping", get(snooping)),
        ("/api/storm-control", get(storm_control)),
        ("/api/mirror", get(mirror)),
        ("/api/routes", get(routes)),
        ("/api/arp", get(arp)),
        ("/api/ospf", get(ospf)),
        ("/api/bgp", get(bgp)),
        ("/api/vrrp", get(vrrp)),
        ("/api/acls", get(acls)),
        ("/api/copp", get(copp)),
        ("/api/port-security", get(port_security)),
        ("/api/dot1x", get(dot1x)),
        ("/api/snooping-sec", get(snooping_sec)),
        ("/api/qos/maps", get(qos_maps)),
        ("/api/qos/wred", get(qos_wred)),
        ("/api/qos/ports", get(qos_ports)),
        ("/api/lldp", get(lldp)),
        ("/api/dhcp", get(dhcp)),
        ("/api/sflow", get(sflow)),
        ("/api/snmp", get(snmp)),
        ("/api/ntp", get(ntp)),
        ("/api/system", get(system)),
        ("/api/system/identity", get(system_identity)),
        ("/api/system/users", get(system_users)),
        ("/api/system/logging", get(system_logging)),
        ("/api/system/commits", get(system_commits)),
        ("/api/system/image", get(system_image)),
        (
            "/api/system/tech-support/download",
            get(system_tech_support_download),
        ),
        ("/api/users", get(users)),
        ("/api/config", get(config)),
        ("/api/maintenance", get(maintenance)),
    ]
}

/// Endpoints that change something. Everything here except the two
/// login paths must appear in `hemlock_common::role::ADMIN_WEB_PATHS`
/// — `every_post_route_is_gated` is what makes that true.
fn post_routes() -> Vec<(&'static str, MethodRouter<SharedState>)> {
    vec![
        ("/api/login", post(login)),
        ("/api/logout", post(logout)),
        ("/api/interfaces/edit", post(interfaces_edit)),
        ("/api/vlans/edit", post(vlans_edit)),
        ("/api/svis/edit", post(svis_edit)),
        ("/api/lags/edit", post(lags_edit)),
        ("/api/spanning-tree/edit", post(spanning_tree_edit)),
        (
            "/api/spanning-tree/clear-errdisable",
            post(clear_errdisable),
        ),
        ("/api/mac-table/edit", post(mac_table_edit)),
        ("/api/mac-table/flush", post(mac_table_flush)),
        ("/api/snooping/edit", post(snooping_edit)),
        ("/api/storm-control/edit", post(storm_control_edit)),
        ("/api/mirror/edit", post(mirror_edit)),
        ("/api/routes/static/edit", post(static_routes_edit)),
        ("/api/arp/edit", post(arp_edit)),
        ("/api/arp/flush", post(arp_flush)),
        ("/api/ospf/edit", post(ospf_edit)),
        ("/api/bgp/edit", post(bgp_edit)),
        ("/api/vrrp/edit", post(vrrp_edit)),
        ("/api/acls/edit", post(acls_edit)),
        ("/api/acls/bindings/edit", post(acl_bindings_edit)),
        ("/api/acls/clear", post(acls_clear)),
        ("/api/copp/edit", post(copp_edit)),
        ("/api/copp/clear", post(copp_clear)),
        ("/api/port-security/edit", post(port_security_edit)),
        ("/api/port-security/clear", post(port_security_clear)),
        ("/api/dot1x/edit", post(dot1x_edit)),
        ("/api/dot1x/reauth", post(dot1x_reauth)),
        ("/api/snooping-sec/edit", post(snooping_sec_edit)),
        (
            "/api/snooping-sec/bindings/clear",
            post(snooping_sec_bindings_clear),
        ),
        ("/api/qos/maps/edit", post(qos_maps_edit)),
        ("/api/qos/wred/edit", post(qos_wred_edit)),
        ("/api/qos/ports/edit", post(qos_ports_edit)),
        ("/api/lldp/edit", post(lldp_edit)),
        ("/api/dhcp/relay/edit", post(dhcp_relay_edit)),
        ("/api/dhcp/server/edit", post(dhcp_server_edit)),
        ("/api/dhcp/leases/clear", post(dhcp_leases_clear)),
        ("/api/sflow/edit", post(sflow_edit)),
        ("/api/snmp/edit", post(snmp_edit)),
        ("/api/ntp/edit", post(ntp_edit)),
        ("/api/system/identity/edit", post(system_identity_edit)),
        ("/api/system/users/edit", post(system_users_edit)),
        ("/api/system/logging/edit", post(system_logging_edit)),
        ("/api/system/rollback", post(system_rollback)),
        ("/api/system/diag/ping", post(system_diag_ping)),
        ("/api/system/diag/traceroute", post(system_diag_traceroute)),
        ("/api/system/diag/cable", post(system_diag_cable)),
        ("/api/system/tech-support", post(system_tech_support)),
        (
            "/api/system/certificate/regenerate",
            post(system_certificate_regenerate),
        ),
        ("/api/system/web/edit", post(system_web_edit)),
        ("/api/users/add", post(users_add)),
        ("/api/config/restore", post(config_restore)),
        ("/api/reboot", post(reboot)),
        ("/api/reboot/cancel", post(reboot_cancel)),
        // Firmware images stream straight to disk; lift axum default
        // 2 MB body cap for this one route. 1 GiB, not 4: the limit is
        // a usize, and the AS4610's Cortex-A9 makes that 32 bits —
        // 4 GiB overflows. Images are a few hundred MB.
        (
            "/api/upgrade/upload",
            post(upgrade_upload).layer(DefaultBodyLimit::max(1024 * 1024 * 1024)),
        ),
        ("/api/upgrade/apply", post(upgrade_apply)),
        ("/api/upgrade/discard", post(upgrade_discard)),
    ]
}

/// POST endpoints an operator may reach: signing in and out.
#[cfg_attr(not(test), allow(dead_code))]
pub const PUBLIC_POSTS: &[&str] = &["/api/login", "/api/logout"];

pub fn router(state: SharedState) -> Router {
    let mut api = Router::new();
    for (path, handler) in get_routes().into_iter().chain(post_routes()) {
        api = api.route(path, handler);
    }
    api.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        role_gate,
    ))
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

fn cookie_token(headers: &header::HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}

/// Extractor gating every state endpoint on a live session.
struct Operator(auth::SessionInfo);

impl FromRequestParts<SharedState> for Operator {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        cookie_token(&parts.headers)
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

/// The role gate, in front of every API route.
///
/// Path-based rather than per-handler on purpose: a new privileged
/// endpoint cannot forget to opt in, because the gate does not consult
/// the handler at all. The list is
/// `hemlock_common::role::ADMIN_WEB_PATHS`, shared with the CLI, and
/// the refusal is the CLI wording verbatim.
async fn role_gate(
    State(state): State<SharedState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    if !hemlock_common::role::web_requires_admin(path) {
        return next.run(request).await;
    }
    // An unauthenticated request still answers 401 from the handler
    // extractor; only a signed-in operator gets the role refusal.
    let role = cookie_token(request.headers())
        .and_then(|token| state.sessions.touch(&token))
        .map(|session| session.role);
    match role {
        Some(role) if !role.is_admin() => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": hemlock_common::role::PERMISSION_DENIED })),
        )
            .into_response(),
        _ => next.run(request).await,
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

async fn login(
    State(state): State<SharedState>,
    parts: Parts,
    Json(req): Json<LoginRequest>,
) -> Response {
    // The configuration is the source of truth for who may log in and
    // with what; an account it does not manage falls back to the OS
    // user database (see auth::check).
    let account = match running_config(&state.mgmtd).await {
        Ok(text) => hemlock_config::parse(&text)
            .ok()
            .and_then(|tree| auth::config_account(&tree, &req.username)),
        Err(_) => None,
    };
    match auth::verify(
        state.dev_auth.as_ref(),
        account.as_ref(),
        &req.username,
        &req.password,
    )
    .await
    {
        Ok(()) => {
            // mgmtd answers with the authoritative role (including the
            // OS fallback for accounts the config does not manage) and
            // the console idle timeout, and registers the session so
            // `show system users` lists it.
            let from = peer_address(&parts);
            let who = who_am_i(&state, &req.username, &from).await;
            let token = state.sessions.create(
                auth::SessionInfo {
                    username: req.username.clone(),
                    role: who.role,
                    mgmtd_session_id: who.session_id,
                },
                who.timeout_mins,
            );
            tracing::info!(username = %req.username, role = %who.role, %from, "web login");
            (
                [(header::SET_COOKIE, session_cookie(&state, &token, false))],
                Json(json!({ "username": req.username, "role": who.role.as_str() })),
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

/// What mgmtd says about a freshly authenticated login.
struct WhoAmI {
    role: hemlock_common::role::Role,
    session_id: u64,
    timeout_mins: u32,
}

/// Ask mgmtd for the role, the console timeout, and a session handle.
/// An unreachable mgmtd leaves the session read-only: the console can
/// show nothing useful without it anyway, and failing closed is the
/// safe direction for a privilege decision.
async fn who_am_i(state: &AppState, username: &str, from: &str) -> WhoAmI {
    let fallback = WhoAmI {
        role: hemlock_common::role::Role::Operator,
        session_id: 0,
        timeout_mins: auth::DEFAULT_SESSION_TIMEOUT_MINS,
    };
    let Ok(mut client) = mgmtd_client(state).await else {
        return fallback;
    };
    match client
        .who_am_i(pb::WhoAmIRequest {
            user: username.to_string(),
            client: "web".into(),
            from: from.to_string(),
        })
        .await
    {
        Ok(response) => {
            let response = response.into_inner();
            WhoAmI {
                role: hemlock_common::role::Role::parse(&response.role).unwrap_or_default(),
                session_id: response.session_id,
                timeout_mins: if response.web_session_timeout_mins == 0 {
                    auth::DEFAULT_SESSION_TIMEOUT_MINS
                } else {
                    response.web_session_timeout_mins
                },
            }
        }
        Err(_) => fallback,
    }
}

/// The client address behind the console, for the session list. Uses
/// the reverse-proxy header when one is present, and otherwise says so
/// rather than inventing an address (webd binds directly on a switch,
/// where the header is absent and the peer is not visible to a
/// handler).
fn peer_address(parts: &Parts) -> String {
    for header_name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = parts.headers.get(header_name) {
            if let Ok(text) = value.to_str() {
                if let Some(first) = text.split(',').next() {
                    let first = first.trim();
                    if !first.is_empty() {
                        return first.to_string();
                    }
                }
            }
        }
    }
    parts
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| "web".into())
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
    if let Some(token) = cookie_token(&parts.headers) {
        // Drop it from mgmtd too, so the session list is not left with
        // a ghost until the stale sweep.
        let mgmtd_session_id = state.sessions.mgmtd_session_id(&token);
        if mgmtd_session_id != 0 {
            if let Ok(mut client) = mgmtd_client(&state).await {
                let _ = client
                    .close_session(pb::CloseSessionRequest {
                        session_id: mgmtd_session_id,
                    })
                    .await;
            }
        }
        state.sessions.remove(&token);
    }
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session_cookie(&state, "", true))],
    )
        .into_response()
}

async fn session(op: Operator, State(_state): State<SharedState>) -> Response {
    // Operator resolved the session; echo back who it belongs to and
    // what it may do — the console disables its edit affordances from
    // this.
    Json(json!({
        "username": op.0.username,
        "role": op.0.role.as_str(),
        "admin": op.0.role.is_admin(),
    }))
    .into_response()
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
    duplex: String,
    autoneg: bool,
    /// What the operator pinned, as opposed to what negotiated: 0 and
    /// "" mean nothing is forced.
    forced_speed_mbps: u32,
    forced_duplex: String,
    /// Speed/duplex modes the platform declares for this port
    /// (`["1G/full", "auto"]`) — the console offers exactly these.
    supported_modes: Vec<String>,
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
        duplex: i.duplex.clone(),
        autoneg: i.autoneg,
        forced_speed_mbps: i.forced_speed_mbps,
        forced_duplex: i.forced_duplex.clone(),
        supported_modes: i.supported_modes.clone(),
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
    mtu: u32,
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
                            mtu: iface.mtu,
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
    operator: &auth::SessionInfo,
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

    let tree_text = tree.to_text();
    let mut client = mgmtd_client(state).await?;
    let response = client
        .set_candidate(pb::ConfigText {
            text: tree_text.clone(),
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
            user: operator.username.clone(),
            client: "web".into(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    tracing::info!(commit_id = commit.commit_id, user = %operator.username, "web console commit");
    // A commit that promotes or demotes an account reaches the console
    // sessions already open, so the edit affordances follow at once.
    if let Ok(tree) = hemlock_config::parse(&tree_text) {
        for (name, role) in crate::system_edit::configured_roles(&tree) {
            state.sessions.set_role(&name, role);
        }
    }
    Ok(Json(json!({
        "commit_id": commit.commit_id,
        "applied": commit.applied_changes,
        "warnings": commit.warnings,
    }))
    .into_response())
}

async fn interfaces_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::edit::InterfaceEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::edit::apply_interface_edit(tree, &edit)
    })
    .await
}

/// One routed VLAN interface, plus the VLAN it fronts.
#[derive(Serialize)]
struct SviRowJson {
    /// The VLAN id; the interface is named `Vlan<id>`.
    vlan: u32,
    name: String,
    /// The VLAN's display name, so the page need not join two calls.
    vlan_name: String,
    address: Option<String>,
    mtu: u32,
    admin_up: bool,
    oper_up: bool,
}

/// The SVIs, plus every VLAN that could take one — the "New SVI"
/// picker offers the VLANs that have no routed interface yet, and
/// mgmtd refuses an SVI whose VLAN is undefined.
async fn svis(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let response = fetch_interfaces(&state).await?;
    let name_of = |id: u32| response.vlan_names.get(&id).cloned().unwrap_or_default();

    let mut svis: Vec<SviRowJson> = response
        .interfaces
        .iter()
        .filter(|i| i.kind == "vlan")
        .filter_map(|i| {
            let vlan: u32 = i.name.strip_prefix("Vlan")?.parse().ok()?;
            Some(SviRowJson {
                vlan,
                name: i.name.clone(),
                vlan_name: name_of(vlan),
                address: i.ip_addresses.first().cloned(),
                mtu: i.mtu,
                admin_up: i.admin_state != pb::AdminState::Down as i32,
                oper_up: i.oper_status == pb::OperStatus::Up as i32,
            })
        })
        .collect();
    svis.sort_by_key(|s| s.vlan);

    let vlans: Vec<serde_json::Value> = response
        .active_vlans
        .iter()
        .map(|&id| json!({ "id": id, "name": name_of(id) }))
        .collect();
    Ok(Json(json!({ "svis": svis, "vlans": vlans })))
}

async fn svis_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::edit::SviEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::edit::apply_svi_edit(tree, &edit)
    })
    .await
}

async fn vlans_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::edit::VlanEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::edit::apply_vlan_edit(tree, &edit)
    })
    .await
}

// ------------------------------------------------------- system suite

/// `GET /api/system/identity` — the configured identity plus the two
/// things the General page cannot derive on its own: the zone names to
/// offer, and what the OS currently answers (so a pending-but-applied
/// difference is visible).
async fn system_identity(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let system = tree.block("system").map(|(_, items)| items).unwrap_or(&[]);
    let leaf = |name: &str| ConfigTree::leaf_value(system, name).map(str::to_string);
    let name_servers: Vec<String> = system
        .iter()
        .filter_map(|item| match item {
            hemlock_config::Item::Leaf { name, values } if name == "name-server" => {
                values.first().cloned()
            }
            _ => None,
        })
        .collect();
    let banner = ConfigTree::phrase_values(system, "banner", "login")
        .and_then(|values| values.first().cloned());

    Ok(Json(json!({
        "hostname": leaf("hostname"),
        "timezone": leaf("timezone"),
        "domain_name": leaf("domain-name"),
        "name_servers": name_servers,
        "banner_login": banner,
        // What the box actually answers right now.
        "os_hostname": crate::hostname(),
        "os_timezone": os_timezone(),
        // The searchable picker offers exactly the installed database.
        "timezones": hemlock_common::tz::names(),
    })))
}

/// The time zone the OS is running in, from the `/etc/localtime`
/// symlink systemd maintains. Empty when it cannot be read.
fn os_timezone() -> String {
    if let Ok(text) = std::fs::read_to_string("/etc/timezone") {
        let text = text.trim();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    let Ok(target) = std::fs::read_link("/etc/localtime") else {
        return String::new();
    };
    let target = target.to_string_lossy().replace('\\', "/");
    match target.split_once("/zoneinfo/") {
        Some((_, zone)) => zone.to_string(),
        None => String::new(),
    }
}

async fn system_identity_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::system_edit::IdentityEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::system_edit::apply_identity_edit(tree, &edit)
    })
    .await
}

/// `GET /api/system/users` — the configured accounts (config) beside
/// the live sessions (mgmtd's registry), the same two halves
/// `show system users` prints.
async fn system_users(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let configured = configured_users_json(&tree);

    let sessions = match mgmtd_client(&state).await {
        Ok(mut client) => client
            .list_sessions(pb::ListSessionsRequest {})
            .await
            .map(|response| {
                response
                    .into_inner()
                    .sessions
                    .into_iter()
                    .map(|session| {
                        json!({
                            "user": session.user,
                            "from": session.from,
                            "client": session.client,
                            "role": session.role,
                            "idle_secs": session.idle_secs,
                            "login_time": session.login_time,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    Ok(Json(json!({
        "users": configured,
        "sessions": sessions,
        "session_timeout": session_timeout_of(&tree),
        // The console mirrors the lockout guard, so it needs to know
        // who still counts as a usable administrator.
        "admins_with_password": configured
            .iter()
            .filter(|user| user["role"] == "admin" && user["auth"] == "password")
            .count(),
    })))
}

/// `system { login { user ... } }` as the grid renders it.
fn configured_users_json(tree: &ConfigTree) -> Vec<serde_json::Value> {
    let Some((_, system)) = tree.block("system") else {
        return Vec::new();
    };
    let Some((_, login)) = ConfigTree::blocks_named(system, "login").next() else {
        return Vec::new();
    };
    let mut users: Vec<serde_json::Value> = ConfigTree::blocks_named(login, "user")
        .filter_map(|(keys, children)| {
            let name = keys.first()?.clone();
            let ssh_keys: Vec<String> = children
                .iter()
                .filter_map(|item| match item {
                    hemlock_config::Item::Leaf { name, values } if name == "ssh-key" => {
                        values.first().cloned()
                    }
                    _ => None,
                })
                .collect();
            let has_password = ConfigTree::leaf_value(children, "password-hash").is_some();
            let auth = match (has_password, ssh_keys.is_empty()) {
                (true, _) => "password",
                (false, false) => "ssh-key",
                (false, true) => "none",
            };
            Some(json!({
                "name": name,
                // Least privilege: an omitted role is `operator`.
                "role": ConfigTree::leaf_value(children, "role").unwrap_or("operator"),
                "auth": auth,
                // The hash itself never leaves the box.
                "ssh_keys": ssh_keys,
            }))
        })
        .collect();
    users.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    users
}

/// `system { web { session-timeout } }`, or the default.
fn session_timeout_of(tree: &ConfigTree) -> u32 {
    tree.block("system")
        .and_then(|(_, system)| ConfigTree::blocks_named(system, "web").next())
        .and_then(|(_, web)| ConfigTree::leaf_value(web, "session-timeout"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(auth::DEFAULT_SESSION_TIMEOUT_MINS)
}

async fn system_users_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::system_edit::UserEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::system_edit::apply_user_edit(tree, &edit)
    })
    .await
}

/// `GET /api/system/logging` — the forwarding config plus a journal
/// tail. `?count=` selects how many lines, so the page can poll a
/// short window and fetch a longer one on demand.
async fn system_logging(
    _op: Operator,
    State(state): State<SharedState>,
    axum::extract::Query(query): axum::extract::Query<LogQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let logging = tree
        .block("system")
        .and_then(|(_, system)| ConfigTree::blocks_named(system, "logging").next())
        .map(|(_, items)| items)
        .unwrap_or(&[]);
    let hosts: Vec<serde_json::Value> = logging
        .iter()
        .filter_map(|item| match item {
            hemlock_config::Item::Leaf { name, values } if name == "host" => {
                let address = values.first()?;
                let setting = |wanted: &str| {
                    values[1..]
                        .chunks(2)
                        .find(|pair| pair.first().map(String::as_str) == Some(wanted))
                        .and_then(|pair| pair.get(1).cloned())
                };
                Some(json!({
                    "address": address,
                    "port": setting("port")
                        .and_then(|port| port.parse::<u16>().ok())
                        .unwrap_or(514),
                    "protocol": setting("protocol").unwrap_or_else(|| "udp".into()),
                }))
            }
            _ => None,
        })
        .collect();

    let mut client = mgmtd_client(&state).await?;
    let log = client
        .get_log(pb::GetLogRequest { count: query.count })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();

    Ok(Json(json!({
        "level": ConfigTree::leaf_value(logging, "level").unwrap_or("informational"),
        "hosts": hosts,
        "journal_available": log.available,
        "entries": log.entries.iter().map(|entry| json!({
            "time": entry.time_unix,
            "host": entry.host,
            "tag": entry.tag,
            "pid": entry.pid,
            "severity": entry.severity,
            "message": entry.message,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct LogQuery {
    /// 0 = mgmtd's default window.
    #[serde(default)]
    count: u32,
}

async fn system_logging_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::system_edit::LoggingEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::system_edit::apply_logging_edit(tree, &edit)
    })
    .await
}

/// `GET /api/system/commits` — the commit history, index 0 being the
/// running configuration.
async fn system_commits(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = mgmtd_client(&state).await?;
    let entries = client
        .list_rollbacks(pb::ListRollbacksRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner()
        .entries;
    Ok(Json(json!({
        "commits": entries.iter().map(|entry| json!({
            "index": entry.revisions_back,
            "time": entry.committed_at,
            "user": entry.user,
            "client": entry.client,
            "comment": entry.comment,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct RollbackRequestBody {
    /// Ring index to load and commit; 0 (the running config) is not a
    /// target.
    index: u32,
}

/// `POST /api/system/rollback` — load ring entry N into the candidate
/// and commit it, the same two RPCs `rollback <n>` uses.
async fn system_rollback(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<RollbackRequestBody>,
) -> Result<Response, ApiError> {
    if request.index == 0 {
        return Ok(errors(vec![
            "commit 0 is the running configuration; there is nothing to roll back to".to_string(),
        ]));
    }
    let mut client = mgmtd_client(&state).await?;
    if let Err(status) = client
        .rollback(pb::RollbackRequest {
            revisions_back: request.index,
        })
        .await
    {
        return Ok(errors(vec![status.message().to_string()]));
    }
    let commit = client
        .commit(pb::CommitRequest {
            comment: format!("rollback {}", request.index),
            confirm_timeout_secs: 0,
            user: _op.0.username.clone(),
            client: "web".into(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    tracing::info!(
        index = request.index,
        user = %_op.0.username,
        "web console rollback"
    );
    Ok(Json(json!({
        "commit_id": commit.commit_id,
        "applied": commit.applied_changes,
        "warnings": commit.warnings,
    }))
    .into_response())
}

/// `GET /api/system/image` — what runs now and what boots next.
async fn system_image(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = mgmtd_client(&state).await?;
    let info = client
        .get_image_info(pb::GetImageInfoRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "version": info.version,
        "installed_at": info.installed_at,
        "image_file": info.image_file,
        "kernel": info.kernel,
        "platform": info.platform,
        "next_boot": info.next_boot,
        "onie_rescue_armed": info.onie_rescue_armed,
    })))
}

// ------------------------------------------------------- diagnostics

#[derive(Deserialize)]
struct DiagRequest {
    host: String,
    /// Empty = let the kernel choose.
    #[serde(default)]
    source: String,
}

/// `POST /api/system/diag/ping` and `.../traceroute`.
///
/// The console cannot hand a terminal to a child the way the CLI does,
/// so webd runs the tool to completion with a bounded deadline and
/// returns the whole output. That is what a browser can render; the CLI
/// keeps the live form.
async fn system_diag_ping(
    _op: Operator,
    State(_state): State<SharedState>,
    Json(request): Json<DiagRequest>,
) -> Response {
    run_diag("ping", &request, &["-c", "5", "-w", "10"]).await
}

async fn system_diag_traceroute(
    _op: Operator,
    State(_state): State<SharedState>,
    Json(request): Json<DiagRequest>,
) -> Response {
    run_diag("traceroute", &request, &["-w", "2", "-q", "1"]).await
}

/// A host argument: an IP literal or a plausible hostname. Checked
/// before a process exists, so nothing shell-shaped reaches an argv.
fn valid_diag_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// A source interface: the kernel names its netdevs after the
/// interfaces, and only those characters can appear in one.
fn valid_diag_source(source: &str) -> bool {
    !source.is_empty()
        && source.len() <= 32
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// How long the console waits for one run before giving up. The bounds
/// passed to the tools are shorter, so this only fires if a tool hangs.
const DIAG_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

async fn run_diag(tool: &str, request: &DiagRequest, bounds: &[&str]) -> Response {
    let host = request.host.trim();
    if !valid_diag_host(host) {
        return errors(vec![format!("bad host {host:?}")]);
    }
    let source = request.source.trim();
    if !source.is_empty() && !valid_diag_source(source) {
        return errors(vec![format!("bad source interface {source:?}")]);
    }
    let mut args: Vec<String> = bounds.iter().map(|arg| (*arg).to_string()).collect();
    if !source.is_empty() {
        // Both tools bind to an interface; only the flag differs.
        args.push(if tool == "ping" { "-I" } else { "-i" }.to_string());
        args.push(source.to_string());
    }
    args.push(host.to_string());

    let run = tokio::process::Command::new(tool).args(&args).output();
    let output = match tokio::time::timeout(DIAG_DEADLINE, run).await {
        Err(_) => return errors(vec![format!("{tool} did not finish in time")]),
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return errors(vec![format!("{tool} is not installed on this switch")])
        }
        Ok(Err(err)) => return errors(vec![format!("cannot run {tool}: {err}")]),
        Ok(Ok(output)) => output,
    };
    // A non-zero exit is the tool answering (an unreachable host), not
    // an API failure, so the output is returned either way.
    Json(json!({
        "tool": tool,
        "host": host,
        "exit_ok": output.status.success(),
        "output": format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CableRequest {
    port: String,
    /// False (the default) replays the last sweep; true runs a new one,
    /// which interrupts the link.
    #[serde(default)]
    run: bool,
}

/// `POST /api/system/diag/cable` — replay or run a TDR sweep.
async fn system_diag_cable(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<CableRequest>,
) -> Result<Response, ApiError> {
    let mut client = syncd_client(&state).await?;
    let result = if request.run {
        client
            .run_cable_diagnostics(pb::RunCableDiagnosticsRequest {
                port: request.port.clone(),
            })
            .await
    } else {
        client
            .get_cable_diagnostics(pb::GetCableDiagnosticsRequest {
                port: request.port.clone(),
            })
            .await
    };
    match result {
        Ok(response) => {
            let response = response.into_inner();
            Ok(Json(json!({
                "port": response.port,
                "has_result": response.has_result,
                "run_at": response.run_at,
                "pairs": response.pairs.iter().map(|pair| json!({
                    "pair": pair.pair,
                    "state": pair.state,
                    "length_m": pair.length_m,
                })).collect::<Vec<_>>(),
            }))
            .into_response())
        }
        Err(status) => Ok(errors(vec![status.message().to_string()])),
    }
}

/// `POST /api/system/tech-support` — build a bundle and say where it
/// landed, so the console can offer it for download.
async fn system_tech_support(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Response, ApiError> {
    let mut client = mgmtd_client(&state).await?;
    match client.tech_support(pb::TechSupportRequest {}).await {
        Ok(response) => {
            let response = response.into_inner();
            Ok(Json(json!({
                "path": response.path,
                "size_bytes": response.size_bytes,
            }))
            .into_response())
        }
        Err(status) => Ok(errors(vec![status.message().to_string()])),
    }
}

/// Where mgmtd writes bundles; mirrored so the download check has
/// something to compare against.
const TECH_SUPPORT_DIR: &str = "/var/lib/hemlock";

#[derive(Deserialize)]
struct TechSupportQuery {
    path: String,
}

/// `GET /api/system/tech-support/download?path=` — stream a bundle
/// mgmtd wrote. The path is checked against the bundle directory and
/// the filename shape, so this cannot be turned into a general file
/// reader.
async fn system_tech_support_download(
    _op: Operator,
    State(_state): State<SharedState>,
    axum::extract::Query(query): axum::extract::Query<TechSupportQuery>,
) -> Response {
    let path = std::path::Path::new(&query.path);
    let inside_bundle_dir = path.parent() == Some(std::path::Path::new(TECH_SUPPORT_DIR));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| {
            inside_bundle_dir && name.starts_with("tech-support-") && name.ends_with(".tar.gz")
        });
    let Some(name) = name else {
        return errors(vec!["not a tech-support bundle".to_string()]);
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/gzip".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename={name}"),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(err) => errors(vec![format!("cannot read the bundle: {err}")]),
    }
}

/// `POST /api/system/certificate/regenerate` — new TLS pair, new
/// fingerprint. Sessions survive; webd restarts to serve it.
async fn system_certificate_regenerate(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Response, ApiError> {
    let mut client = mgmtd_client(&state).await?;
    match client
        .regenerate_certificate(pb::RegenerateCertificateRequest {})
        .await
    {
        Ok(response) => Ok(Json(json!({
            "fingerprint": response.into_inner().fingerprint,
            "restarting": true,
        }))
        .into_response()),
        Err(status) => Ok(errors(vec![status.message().to_string()])),
    }
}

async fn system_web_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::system_edit::WebEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::system_edit::apply_web_edit(tree, &edit)
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
    commit_edit(&state, &_op.0, "web console", |tree| {
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
    commit_edit(&state, &_op.0, "web console", |tree| {
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
    commit_edit(&state, &_op.0, "web console", |tree| {
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
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::switching_edit::apply_snooping_edit(tree, &edit)
    })
    .await
}

/// `GET /api/lldp` — orch's LLDP view: settings, per-port counters and
/// the aged neighbor table, plus the ports carrying `lldp disable` so
/// the editor can show the whole grid.
async fn lldp(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .get_lldp_state(pb::GetLldpStateRequest {
            port: String::new(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "enabled": response.enabled,
        "tx_interval": response.tx_interval,
        "hold_multiplier": response.hold_multiplier,
        "ttl": response.tx_interval.saturating_mul(response.hold_multiplier),
        "chassis_id": response.chassis_id,
        "system_name": response.system_name,
        "system_description": response.system_description,
        "management_address": response.management_address,
        "ports": response.ports.iter().map(|port| json!({
            "port": port.port,
            "enabled": port.enabled,
            "frames_tx": port.frames_tx,
            "frames_rx": port.frames_rx,
            "frames_discarded": port.frames_discarded,
            "ageouts": port.ageouts,
            "neighbors": port.neighbors.iter().map(|neighbor| json!({
                "chassis_id": neighbor.chassis_id,
                "chassis_id_subtype": neighbor.chassis_id_subtype,
                "port_id": neighbor.port_id,
                "port_id_subtype": neighbor.port_id_subtype,
                "port_description": neighbor.port_description,
                "system_name": neighbor.system_name,
                "system_description": neighbor.system_description,
                "management_address": neighbor.management_address,
                "ttl": neighbor.ttl,
                "age_secs": neighbor.age_secs,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })))
}

async fn lldp_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::services_edit::LldpEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::services_edit::apply_lldp_edit(tree, &edit)
    })
    .await
}

/// `GET /api/dhcp` — the DHCP page's state. The relay is a capability
/// of the snooping engine, so its per-VLAN servers and counters come
/// out of that engine's snapshot.
async fn dhcp(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .get_snoop_sec_state(pb::GetSnoopSecStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    // The server half is a separate engine; a page shows both.
    let server = orch
        .get_dhcp_server_state(pb::GetDhcpServerStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "relay": response.dhcp_relay.iter().map(|relay| json!({
            "vlan": relay.vlan,
            "servers": relay.servers,
            "giaddr": relay.giaddr,
            "to_server": relay.to_server,
            "to_client": relay.to_client,
            "dropped": relay.dropped,
        })).collect::<Vec<_>>(),
        "pools": server.pools.iter().map(|pool| {
            let config = pool.config.clone().unwrap_or_default();
            json!({
                "name": config.name,
                "network": config.network,
                "range_start": config.range_start,
                "range_end": config.range_end,
                "gateway": config.gateway,
                "dns_servers": config.dns_servers,
                "lease_time": config.lease_time,
                "domain_name": config.domain_name,
                "reservations": config.reservations.iter().map(|reservation| json!({
                    "mac": reservation.mac,
                    "address": reservation.address,
                })).collect::<Vec<_>>(),
                "in_use": pool.in_use,
                "capacity": pool.capacity,
            })
        }).collect::<Vec<_>>(),
        "leases": server.leases.iter().map(|lease| json!({
            "address": lease.address,
            "mac": lease.mac,
            "hostname": lease.hostname,
            "expires_at": lease.expires_at,
            "reservation": lease.reservation,
            "pool": lease.pool,
        })).collect::<Vec<_>>(),
    })))
}

async fn dhcp_server_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::services_edit::DhcpServerEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::services_edit::apply_dhcp_server_edit(tree, &edit)
    })
    .await
}

#[derive(Deserialize)]
struct DhcpLeaseClear {
    address: String,
}

/// `POST /api/dhcp/leases/clear` — release one lease. Not a config
/// edit: it changes dnsmasq's lease file, not the running config.
async fn dhcp_leases_clear(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<DhcpLeaseClear>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .clear_dhcp_lease(pb::ClearDhcpLeaseRequest {
            address: request.address,
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({ "cleared": response.cleared })))
}

async fn dhcp_relay_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::services_edit::DhcpRelayEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::services_edit::apply_dhcp_relay_edit(tree, &edit)
    })
    .await
}

/// `GET /api/sflow` — the exporter's view from orch plus the
/// programmed sampler from syncd (which owns the ASIC).
async fn sflow(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let export = orch
        .get_sflow_export_state(pb::GetSflowExportStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    // A syncd that cannot answer leaves `supported` true: the commit
    // gate already refused an unsupported platform.
    let supported = match syncd_client(&state).await {
        Ok(mut syncd) => syncd
            .get_sflow_state(pb::GetSflowStateRequest {})
            .await
            .map(|response| response.into_inner().supported)
            .unwrap_or(true),
        Err(_) => true,
    };
    Ok(Json(json!({
        "enabled": export.enabled,
        "supported": supported,
        "agent_address": export.agent_address,
        "agent_interface": export.agent_interface,
        "sample_rate": export.sample_rate,
        "polling_interval": export.polling_interval,
        "collectors": export.collectors.iter().map(|collector| json!({
            "address": collector.address,
            "port": collector.port,
        })).collect::<Vec<_>>(),
        "enabled_ports": export.enabled_ports,
        "disabled_ports": export.disabled_ports,
        "samples_taken": export.samples_taken,
        "counter_samples": export.counter_samples,
        "datagrams_sent": export.datagrams_sent,
        "datagrams_failed": export.datagrams_failed,
    })))
}

async fn sflow_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::services_edit::SflowEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::services_edit::apply_sflow_edit(tree, &edit)
    })
    .await
}

/// `GET /api/snmp` — orch's SNMP view: agent settings plus the AgentX
/// subagent's request counters.
async fn snmp(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .get_snmp_state(pb::GetSnmpStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "enabled": response.enabled,
        "agentx_connected": response.connected,
        "listen_interface": response.listen_interface,
        "listen_address": response.listen_address,
        "location": response.location,
        "contact": response.contact,
        "communities": response.communities.iter().map(|community| json!({
            "name": community.name,
            "source": community.source,
        })).collect::<Vec<_>>(),
        "users": response.users,
        "packets_in": response.packets_in,
        "packets_out": response.packets_out,
        "get_requests": response.get_requests,
        "getnext_requests": response.getnext_requests,
        "errors": response.errors,
    })))
}

async fn snmp_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::services_edit::SnmpEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::services_edit::apply_snmp_edit(tree, &edit)
    })
    .await
}

/// `GET /api/ntp` — orch's timesyncd view: configured servers plus
/// the live sync posture.
async fn ntp(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .get_ntp_state(pb::GetNtpStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "enabled": response.enabled,
        "servers": response.servers,
        "synchronized": response.synchronized,
        "server": response.server,
        "stratum": response.stratum,
        "poll_interval_secs": response.poll_interval_secs,
        "offset_usecs": response.offset_usecs,
        "delay_usecs": response.delay_usecs,
        "jitter_usecs": response.jitter_usecs,
        "last_sync_secs_ago": response.last_sync_secs_ago,
    })))
}

async fn ntp_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::services_edit::NtpEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::services_edit::apply_ntp_edit(tree, &edit)
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
    commit_edit(&state, &_op.0, "web console", |tree| {
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
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::switching_edit::apply_mirror_edit(tree, &edit)
    })
    .await
}

#[derive(Debug, Default, serde::Deserialize)]
struct FamilyQuery {
    /// "v4" (default) or "v6".
    #[serde(default)]
    family: String,
}

impl FamilyQuery {
    fn ipv6(&self) -> bool {
        self.family == "v6"
    }
}

/// `GET /api/routes[?family=v4|v6]` — the static-route config rows plus
/// the live RIB view (orch) and FIB summary (syncd), both degrading to
/// empty when their daemon is unreachable.
async fn routes(
    _op: Operator,
    State(state): State<SharedState>,
    axum::extract::Query(query): axum::extract::Query<FamilyQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let rib: Vec<serde_json::Value> = match orch_client(&state).await {
        Ok(mut orch) => orch
            .get_rib(pb::GetRibRequest {
                ipv6: query.ipv6(),
                page_size: 0,
                page_token: String::new(),
            })
            .await
            .map(|response| {
                response
                    .into_inner()
                    .routes
                    .into_iter()
                    .map(|route| {
                        json!({
                            "prefix": route.prefix,
                            "protocol": route.protocol,
                            "distance": route.distance,
                            "metric": route.metric,
                            "uptime_secs": route.uptime_secs,
                            "next_hops": route.next_hops.iter().map(|hop| json!({
                                "via": hop.via,
                                "interface": hop.interface,
                                "resolved": hop.resolved,
                            })).collect::<Vec<_>>(),
                            "fib": route.fib,
                            "interface": route.interface,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let summary = match syncd_client(&state).await {
        Ok(mut syncd) => syncd
            .get_fib_summary(pb::GetFibSummaryRequest {})
            .await
            .map(|response| {
                let summary = response.into_inner();
                json!({
                    "routes_v4": summary.routes_v4,
                    "routes_v6": summary.routes_v6,
                    "neighbors": summary.neighbors,
                    "next_hop_groups": summary.next_hop_groups,
                })
            })
            .ok(),
        Err(_) => None,
    };
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    // Repeated leaves per prefix are ECMP; aggregate to one row each.
    struct StaticRoute {
        next_hops: Vec<String>,
        drop: bool,
        distance: u32,
    }
    let mut statics: std::collections::BTreeMap<String, StaticRoute> = Default::default();
    if let Some((_, routing)) = tree.block("routing") {
        for (_, items) in ConfigTree::blocks_named(routing, "static") {
            for item in items {
                let hemlock_config::Item::Leaf { name, values } = item else {
                    continue;
                };
                let entry = statics.entry(name.clone()).or_insert(StaticRoute {
                    next_hops: Vec::new(),
                    drop: false,
                    distance: 1,
                });
                match values.as_slice() {
                    [keyword] if keyword == "drop" => entry.drop = true,
                    [next_hop, rest @ ..] => {
                        if !entry.next_hops.contains(next_hop) {
                            entry.next_hops.push(next_hop.clone());
                        }
                        if let [keyword, value] = rest {
                            if keyword == "distance" {
                                if let Ok(distance) = value.parse() {
                                    entry.distance = distance;
                                }
                            }
                        }
                    }
                    [] => {}
                }
            }
        }
    }
    let static_routes: Vec<serde_json::Value> = statics
        .iter()
        .map(|(prefix, route)| {
            json!({
                "prefix": prefix,
                "next_hops": route.next_hops,
                "drop": route.drop,
                "distance": route.distance,
            })
        })
        .collect();
    Ok(Json(json!({
        "static_routes": static_routes,
        "rib": rib,
        "summary": summary,
    })))
}

/// `GET /api/arp[?family=v4|v6]` — the kernel neighbor table from orch.
async fn arp(
    _op: Operator,
    State(state): State<SharedState>,
    axum::extract::Query(query): axum::extract::Query<FamilyQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let neighbors: Vec<serde_json::Value> = orch
        .get_neighbors(pb::GetNeighborsRequest { ipv6: query.ipv6() })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner()
        .neighbors
        .into_iter()
        .map(|entry| {
            json!({
                "ip": entry.ip,
                "mac": entry.mac,
                "interface": entry.interface,
                "is_static": entry.permanent,
                "age_secs": entry.age_secs,
            })
        })
        .collect();
    Ok(Json(json!({ "neighbors": neighbors })))
}

async fn arp_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::routing_edit::ArpEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::routing_edit::apply_arp_edit(tree, &edit)
    })
    .await
}

#[derive(Debug, Default, serde::Deserialize)]
struct ArpFlushRequest {
    /// Scope the flush to one address; empty = every dynamic entry.
    #[serde(default)]
    ip: String,
}

async fn arp_flush(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<ArpFlushRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let flushed = orch
        .clear_neighbors(pb::ClearNeighborsRequest { ip: request.ip })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner()
        .flushed;
    Ok(Json(json!({ "flushed": flushed })))
}

// ------------------------------------------------ FRR protocol families

fn leaf_json(items: &[hemlock_config::Item], name: &str) -> Option<String> {
    ConfigTree::leaf_value(items, name).map(str::to_string)
}

fn repeated_leaves(items: &[hemlock_config::Item], name: &str) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            hemlock_config::Item::Leaf { name: n, values } if n == name => values.first().cloned(),
            _ => None,
        })
        .collect()
}

/// `GET /api/ospf` — the configured process (running config) plus the
/// live state from orch/FRR (null when not running).
async fn ospf(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let config = tree.block("routing").and_then(|(_, routing)| {
        ConfigTree::blocks_named(routing, "ospf")
            .next()
            .map(|(_, ospf)| {
                let areas: Vec<serde_json::Value> = ospf
                    .iter()
                    .filter_map(|item| match item {
                        hemlock_config::Item::Block {
                            name,
                            keys,
                            children,
                        } if name == "area" => Some(json!({
                            "id": keys.first().cloned().unwrap_or_default(),
                            "networks": repeated_leaves(children, "network"),
                        })),
                        _ => None,
                    })
                    .collect();
                let interfaces: Vec<serde_json::Value> = ospf
                    .iter()
                    .filter_map(|item| match item {
                        hemlock_config::Item::Block {
                            name,
                            keys,
                            children,
                        } if name == "interface" => Some(json!({
                            "interface": keys.first().cloned().unwrap_or_default(),
                            "cost": leaf_json(children, "cost"),
                            "hello_interval": leaf_json(children, "hello-interval"),
                            "dead_interval": leaf_json(children, "dead-interval"),
                            "priority": leaf_json(children, "priority"),
                        })),
                        _ => None,
                    })
                    .collect();
                json!({
                    "router_id": leaf_json(ospf, "router-id"),
                    "maximum_paths": leaf_json(ospf, "maximum-paths"),
                    "areas": areas,
                    "passive_interfaces": repeated_leaves(ospf, "passive-interface"),
                    "redistribute": repeated_leaves(ospf, "redistribute"),
                    "interfaces": interfaces,
                })
            })
    });
    let live = match orch_client(&state).await {
        Ok(mut orch) => orch
            .get_ospf_state(pb::GetOspfStateRequest {})
            .await
            .map(|response| {
                let s = response.into_inner();
                json!({
                    "router_id": s.router_id,
                    "spf_runs": s.spf_runs,
                    "areas": s.areas.iter().map(|a| json!({
                        "id": a.id, "interfaces": a.interfaces,
                    })).collect::<Vec<_>>(),
                    "neighbors": s.neighbors.iter().map(|n| json!({
                        "router_id": n.router_id,
                        "priority": n.priority,
                        "state": n.state,
                        "dead_time_msecs": n.dead_time_msecs,
                        "address": n.address,
                        "interface": n.interface,
                    })).collect::<Vec<_>>(),
                    "interfaces": s.interfaces.iter().map(|i| json!({
                        "name": i.name,
                        "up": i.up,
                        "address": i.address,
                        "area": i.area,
                        "cost": i.cost,
                        "hello_interval": i.hello_interval,
                        "dead_interval": i.dead_interval,
                        "neighbors": i.neighbors,
                        "adjacent": i.adjacent,
                    })).collect::<Vec<_>>(),
                })
            })
            .ok(),
        Err(_) => None,
    };
    Ok(Json(json!({ "config": config, "state": live })))
}

async fn ospf_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::routing_edit::OspfEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::routing_edit::apply_ospf_edit(tree, &edit)
    })
    .await
}

/// `GET /api/bgp` — the configured process plus the live summary.
async fn bgp(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let config = tree.block("routing").and_then(|(_, routing)| {
        ConfigTree::blocks_named(routing, "bgp")
            .next()
            .map(|(_, bgp)| {
                let neighbors: Vec<serde_json::Value> = bgp
                    .iter()
                    .filter_map(|item| match item {
                        hemlock_config::Item::Block {
                            name,
                            keys,
                            children,
                        } if name == "neighbor" => Some(json!({
                            "ip": keys.first().cloned().unwrap_or_default(),
                            "remote_as": leaf_json(children, "remote-as"),
                            "description": leaf_json(children, "description"),
                            "shutdown": ConfigTree::has_leaf(children, "shutdown"),
                            "ebgp_multihop": leaf_json(children, "ebgp-multihop"),
                            "next_hop_self": ConfigTree::has_leaf(children, "next-hop-self"),
                        })),
                        _ => None,
                    })
                    .collect();
                json!({
                    "as_number": leaf_json(bgp, "as"),
                    "router_id": leaf_json(bgp, "router-id"),
                    "maximum_paths": leaf_json(bgp, "maximum-paths"),
                    "networks": repeated_leaves(bgp, "network"),
                    "redistribute": repeated_leaves(bgp, "redistribute"),
                    "neighbors": neighbors,
                })
            })
    });
    let live = match orch_client(&state).await {
        Ok(mut orch) => orch
            .get_bgp_state(pb::GetBgpStateRequest {
                neighbor: String::new(),
            })
            .await
            .map(|response| {
                let s = response.into_inner();
                json!({
                    "router_id": s.router_id,
                    "as_number": s.as_number,
                    "peers": s.peers.iter().map(|p| json!({
                        "ip": p.ip,
                        "remote_as": p.remote_as,
                        "state": p.state,
                        "up_down": p.up_down,
                        "msg_rcvd": p.msg_rcvd,
                        "msg_sent": p.msg_sent,
                        "pfx_rcvd": p.pfx_rcvd,
                    })).collect::<Vec<_>>(),
                })
            })
            .ok(),
        Err(_) => None,
    };
    Ok(Json(json!({ "config": config, "state": live })))
}

async fn bgp_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::routing_edit::BgpEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::routing_edit::apply_bgp_edit(tree, &edit)
    })
    .await
}

/// `GET /api/vrrp` — configured groups merged with live vrrpd state by
/// (interface, group).
async fn vrrp(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let live = match orch_client(&state).await {
        Ok(mut orch) => orch
            .get_vrrp_state(pb::GetVrrpStateRequest {})
            .await
            .map(|response| response.into_inner().groups)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut groups = Vec::new();
    if let Some((_, interfaces)) = tree.block("interfaces") {
        for item in interfaces {
            let hemlock_config::Item::Block {
                name: interface,
                keys,
                children,
            } = item
            else {
                continue;
            };
            if !keys.is_empty() {
                continue;
            }
            for (group_keys, body) in ConfigTree::blocks_named(children, "vrrp") {
                let group = group_keys.first().cloned().unwrap_or_default();
                let state = live
                    .iter()
                    .find(|g| g.interface == *interface && g.group.to_string() == group);
                groups.push(json!({
                    "interface": interface,
                    "group": group,
                    "addresses": repeated_leaves(body, "address"),
                    "priority": leaf_json(body, "priority"),
                    "advertisement_interval": leaf_json(body, "advertisement-interval"),
                    "preempt": !ConfigTree::has_leaf(body, "no-preempt"),
                    "state": state.map(|s| s.state.clone()),
                    "effective_priority": state.map(|s| s.effective_priority),
                    "virtual_mac": state.map(|s| s.virtual_mac.clone()),
                }));
            }
        }
    }
    Ok(Json(json!({ "groups": groups })))
}

async fn vrrp_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::routing_edit::VrrpEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::routing_edit::apply_vrrp_edit(tree, &edit)
    })
    .await
}

// ------------------------------------------------------------ QoS suite

/// One global map table as the Maps page renders it: the config
/// keyword, its column labels, and the entries sorted by key.
fn qos_map_table(
    table: &str,
    title: &str,
    key_label: &str,
    value_label: &str,
    default_note: &str,
    entries: &[pb::QosMapEntry],
) -> serde_json::Value {
    let mut rows: Vec<(u32, u32)> = entries.iter().map(|e| (e.key, e.value)).collect();
    rows.sort();
    json!({
        "table": table,
        "title": title,
        "key_label": key_label,
        "value_label": value_label,
        "default_note": default_note,
        "entries": rows.iter().map(|(key, value)| json!({
            "key": key,
            "value": value,
        })).collect::<Vec<_>>(),
    })
}

async fn qos_state(state: &AppState) -> Result<pb::GetQosStateResponse, ApiError> {
    let mut client = syncd_client(state).await?;
    Ok(client
        .get_qos_state(pb::GetQosStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner())
}

/// `GET /api/qos/maps` — the four global map tables.
async fn qos_maps(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let qos = qos_state(&state).await?;
    let (ingress, egress) = qos_capabilities(&state).await?;
    Ok(Json(json!({
        "tables": [
            qos_map_table(
                "dscp-to-tc",
                "DSCP to Traffic-Class",
                "DSCP",
                "TC",
                "0",
                &qos.dscp_to_tc,
            ),
            qos_map_table("cos-to-tc", "CoS to Traffic-Class", "CoS", "TC", "0", &qos.cos_to_tc),
            qos_map_table(
                "tc-to-dscp",
                "Traffic-Class to DSCP rewrite",
                "TC",
                "DSCP",
                "no rewrite",
                &qos.tc_to_dscp,
            ),
            qos_map_table(
                "tc-to-cos",
                "Traffic-Class to CoS rewrite",
                "TC",
                "CoS",
                "no rewrite",
                &qos.tc_to_cos,
            ),
        ],
        "qos_map_ingress": ingress,
        "qos_map_egress": egress,
    })))
}

/// The two qos-map capability bits, for gating the page's editors.
async fn qos_capabilities(state: &AppState) -> Result<(bool, bool), ApiError> {
    let mut client = syncd_client(state).await?;
    let info = client
        .get_switch_info(pb::GetSwitchInfoRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    let caps = info.capabilities.unwrap_or_default();
    Ok((caps.qos_map_ingress, caps.qos_map_egress))
}

async fn qos_maps_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::qos_edit::MapEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::qos_edit::apply_map_edit(tree, &edit)
    })
    .await
}

/// `GET /api/qos/wred` — the named profiles with their queue
/// references, plus the platform's buffer cap and capability posture
/// (the threshold slider is bounded by both).
async fn qos_wred(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let qos = qos_state(&state).await?;
    let profiles: Vec<serde_json::Value> = qos
        .wred_profiles
        .iter()
        .map(|profile| {
            // A member carrying its Port-Channel's program is credited
            // to the Port-Channel, so a reference is never listed twice.
            let references: Vec<serde_json::Value> = qos
                .ports
                .iter()
                .filter(|port| port.via_port_channel.is_empty())
                .flat_map(|port| {
                    port.queues
                        .iter()
                        .filter(|queue| queue.wred_profile == profile.name)
                        .map(move |queue| json!({ "port": port.port, "queue": queue.queue }))
                })
                .collect();
            json!({
                "name": profile.name,
                "min_threshold": profile.min_threshold_kb,
                "max_threshold": profile.max_threshold_kb,
                "drop_probability": profile.drop_probability,
                "ecn": profile.ecn,
                "references": references,
            })
        })
        .collect();
    Ok(Json(json!({
        "profiles": profiles,
        "buffer_kb": qos.buffer_kb,
        "wred_supported": qos.wred_supported,
        "ecn_supported": qos.ecn_supported,
    })))
}

async fn qos_wred_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::qos_edit::WredEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::qos_edit::apply_wred_edit(tree, &edit)
    })
    .await
}

/// `GET /api/qos/ports` — per-port effective config with live per-queue
/// counters, plus what the platform's SAI supports so the editors can
/// gate the same way commit does.
async fn qos_ports(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let qos = qos_state(&state).await?;
    let ports: Vec<serde_json::Value> = qos
        .ports
        .iter()
        .map(|port| {
            json!({
                "port": port.port,
                "trust": port.trust,
                "default_tc": port.default_tc,
                "shape_bps": port.shape_bps,
                "shaper": port.shape_bps.map(hemlock_common::net::display_shape_rate),
                "configured": port.configured,
                "via_port_channel": (!port.via_port_channel.is_empty())
                    .then(|| port.via_port_channel.clone()),
                "queues": port.queues.iter().map(|queue| json!({
                    "queue": queue.queue,
                    "mode": if queue.strict { "strict" } else { "dwrr" },
                    // A strict queue takes no DWRR share.
                    "weight": (!queue.strict).then_some(queue.weight),
                    "shape_bps": queue.shape_bps,
                    "shaper": queue.shape_bps.map(hemlock_common::net::display_shape_rate),
                    "wred_profile": (!queue.wred_profile.is_empty())
                        .then(|| queue.wred_profile.clone()),
                    "ecn": queue.ecn,
                    "tx_packets": queue.tx_packets,
                    "tx_bytes": queue.tx_bytes,
                    "dropped": queue.dropped,
                    "wred_dropped": queue.wred_dropped,
                    "ecn_marked": queue.ecn_marked,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({
        "ports": ports,
        "default_ports": qos.default_ports,
        "queue_count": qos.queue_count,
        "buffer_kb": qos.buffer_kb,
        "wred_supported": qos.wred_supported,
        "ecn_supported": qos.ecn_supported,
        "queue_shaper_supported": qos.queue_shaper_supported,
        "wred_profiles": qos.wred_profiles.iter().map(|p| p.name.clone())
            .collect::<Vec<_>>(),
    })))
}

async fn qos_ports_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::qos_edit::PortQosEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::qos_edit::apply_port_qos_edit(tree, &edit)
    })
    .await
}

// ------------------------------------------------------- security suite

fn acl_family_word(family: i32) -> &'static str {
    match pb::AclFamily::try_from(family) {
        Ok(pb::AclFamily::Ipv6) => "ipv6",
        Ok(pb::AclFamily::Mac) => "mac",
        _ => "ipv4",
    }
}

/// Binding direction display: ingress = "in", egress = "out".
fn acl_direction_word(stage: i32) -> &'static str {
    match pb::AclStage::try_from(stage) {
        Ok(pb::AclStage::Egress) => "out",
        _ => "in",
    }
}

fn acl_stage_word(stage: i32) -> &'static str {
    match pb::AclStage::try_from(stage) {
        Ok(pb::AclStage::Egress) => "egress",
        _ => "ingress",
    }
}

/// IP protocol numbers back to the config keywords (58 is ICMPv6).
fn acl_protocol_word(protocol: u32) -> String {
    match protocol {
        6 => "tcp".into(),
        17 => "udp".into(),
        1 | 58 => "icmp".into(),
        other => other.to_string(),
    }
}

/// An L4 range back to the config's "443" / "67-68" text.
fn acl_port_text(low: Option<u32>, high: Option<u32>) -> Option<String> {
    let low = low?;
    match high {
        Some(high) if high != low => Some(format!("{low}-{high}")),
        _ => Some(low.to_string()),
    }
}

fn acl_mac_text(mac: &str, mask: &str) -> Option<String> {
    if mac.is_empty() {
        return None;
    }
    Some(if mask.is_empty() {
        mac.to_string()
    } else {
        format!("{mac}/{mask}")
    })
}

fn acl_ethertype_text(ethertype: u32) -> String {
    match ethertype {
        0x0800 => "ipv4".into(),
        0x86dd => "ipv6".into(),
        0x0806 => "arp".into(),
        other => format!("0x{other:04x}"),
    }
}

/// `GET /api/acls` — syncd's programmed ACLs with live per-rule
/// counters, port bindings, and per-stage TCAM utilization. Police
/// values render in the CLI's suffixed form ("10m", "256k", "2000pps").
async fn acls(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .get_acl_state(pb::GetAclStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    let acls: Vec<serde_json::Value> = response
        .acls
        .iter()
        .map(|acl| {
            let rules: Vec<serde_json::Value> = acl
                .rules
                .iter()
                .enumerate()
                .map(|(index, rule)| {
                    json!({
                        "number": rule.number,
                        "action": if rule.permit { "permit" } else { "deny" },
                        "protocol": rule.protocol.map(acl_protocol_word),
                        "source": (!rule.source.is_empty()).then(|| rule.source.clone()),
                        "destination": (!rule.destination.is_empty())
                            .then(|| rule.destination.clone()),
                        "source_port": acl_port_text(rule.source_port_low, rule.source_port_high),
                        "destination_port": acl_port_text(
                            rule.destination_port_low,
                            rule.destination_port_high,
                        ),
                        "dscp": rule.dscp,
                        "log": rule.log,
                        "police": rule.police_rate.map(|rate| json!({
                            "rate": hemlock_common::net::format_police_rate(rate, rule.police_pps),
                            "burst": rule.police_burst.map(|burst| {
                                hemlock_common::net::format_police_burst(burst, rule.police_pps)
                            }),
                        })),
                        "source_mac": acl_mac_text(&rule.source_mac, &rule.source_mac_mask),
                        "destination_mac": acl_mac_text(
                            &rule.destination_mac,
                            &rule.destination_mac_mask,
                        ),
                        "ethertype": rule.ethertype.map(acl_ethertype_text),
                        "matches": acl.matches.get(index).copied().unwrap_or(0),
                    })
                })
                .collect();
            let total: u64 = acl.matches.iter().sum::<u64>() + acl.implicit_deny_matches;
            json!({
                "name": acl.name,
                "family": acl_family_word(acl.family),
                "rules": rules,
                "implicit_deny_matches": acl.implicit_deny_matches,
                "bindings": acl.bindings.iter().map(|binding| json!({
                    "port": binding.port,
                    "direction": acl_direction_word(binding.stage),
                })).collect::<Vec<_>>(),
                "total_matches": total,
            })
        })
        .collect();
    Ok(Json(json!({
        "acls": acls,
        "tcam": response.tcam.iter().map(|stage| json!({
            "stage": acl_stage_word(stage.stage),
            "used": stage.used,
            "available": stage.available,
        })).collect::<Vec<_>>(),
    })))
}

async fn acls_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::security_edit::AclEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::security_edit::apply_acl_edit(tree, &edit)
    })
    .await
}

async fn acl_bindings_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::security_edit::AclBindingEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::security_edit::apply_acl_binding_edit(tree, &edit)
    })
    .await
}

#[derive(Deserialize)]
struct AclClearRequest {
    /// Scope to one ACL; absent = every ACL.
    #[serde(default)]
    name: Option<String>,
}

/// `POST /api/acls/clear` — zero the match counters.
async fn acls_clear(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<AclClearRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .clear_acl_counters(pb::ClearAclCountersRequest {
            name: request.name.unwrap_or_default(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({ "cleared": response.cleared })))
}

/// `GET /api/copp` — syncd's class table, matching `show copp`.
async fn copp(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .get_copp_state(pb::GetCoppStateRequest {})
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "classes": response.classes.iter().map(|class| json!({
            "class": class.class,
            "rate": class.rate,
            "burst": class.burst,
            "overridden": class.overridden,
            "conforming": class.conforming,
            "dropped": class.dropped,
        })).collect::<Vec<_>>(),
    })))
}

async fn copp_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::security_edit::CoppEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::security_edit::apply_copp_edit(tree, &edit)
    })
    .await
}

/// `POST /api/copp/clear` — zero every class counter.
async fn copp_clear(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    client
        .clear_copp_counters(pb::ClearCoppCountersRequest {})
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(json!({ "cleared": true })))
}

/// `GET /api/port-security` — syncd's per-port view: limits, learned
/// MACs with ages, violations, errdisable.
async fn port_security(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .get_port_security_state(pb::GetPortSecurityStateRequest {
            port: String::new(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({
        "ports": response.ports.iter().map(|entry| json!({
            "port": entry.port,
            "maximum": entry.maximum,
            "violation": if entry.shutdown { "shutdown" } else { "protect" },
            "learned": entry.learned.iter().map(|mac| json!({
                "mac": mac.mac,
                "age_secs": mac.age_secs,
            })).collect::<Vec<_>>(),
            "violations": entry.violations,
            "last_violation_mac": entry.last_violation_mac,
            "last_violation_secs_ago": entry.last_violation_secs_ago,
            "errdisabled": entry.errdisabled,
        })).collect::<Vec<_>>(),
    })))
}

async fn port_security_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::security_edit::PortSecurityEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::security_edit::apply_port_security_edit(tree, &edit)
    })
    .await
}

#[derive(Deserialize)]
struct PortSecurityClearRequest {
    /// Scope to one port; absent = every enabled port.
    #[serde(default)]
    port: Option<String>,
}

/// `POST /api/port-security/clear` — forget learned MACs and lift any
/// errdisable.
async fn port_security_clear(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<PortSecurityClearRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut client = syncd_client(&state).await?;
    let response = client
        .reset_port_security(pb::ResetPortSecurityRequest {
            port: request.port.unwrap_or_default(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({ "cleared": response.cleared })))
}

/// `GET /api/dot1x` — the configured RADIUS servers (keys omitted) and
/// per-port `dot1x` markers from the running config, merged with orch's
/// authenticator runtime — which degrades gracefully when orch is
/// unreachable (status comes back null).
async fn dot1x(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let mut servers: Vec<serde_json::Value> = Vec::new();
    let mut config_reauth: Option<u32> = None;
    if let Some((_, security)) = tree.block("security") {
        if let Some((_, dot1x)) = ConfigTree::blocks_named(security, "dot1x").next() {
            config_reauth =
                ConfigTree::leaf_value(dot1x, "reauth-interval").and_then(|v| v.parse().ok());
            for (keys, body) in ConfigTree::blocks_named(dot1x, "radius-server") {
                servers.push(json!({
                    "ip": keys.first().cloned().unwrap_or_default(),
                    "port": leaf_json(body, "port"),
                    "timeout": leaf_json(body, "timeout"),
                    "retransmit": leaf_json(body, "retransmit"),
                    // The secret never leaves the config.
                    "has_key": ConfigTree::has_leaf(body, "key"),
                }));
            }
        }
    }
    let mut enabled: Vec<String> = Vec::new();
    if let Some((_, interfaces)) = tree.block("interfaces") {
        for item in interfaces {
            if let hemlock_config::Item::Block {
                name,
                keys,
                children,
            } = item
            {
                if keys.is_empty() && ConfigTree::has_leaf(children, "dot1x") {
                    enabled.push(name.clone());
                }
            }
        }
    }
    let live = match orch_client(&state).await {
        Ok(mut orch) => orch
            .get_dot1x_state(pb::GetDot1xStateRequest {
                port: String::new(),
            })
            .await
            .map(|response| response.into_inner())
            .ok(),
        Err(_) => None,
    };
    let ports: Vec<serde_json::Value> = enabled
        .iter()
        .map(|port| {
            let state = live
                .as_ref()
                .and_then(|l| l.ports.iter().find(|p| p.port == *port));
            json!({
                "port": port,
                "status": state.map(|p| p.status.clone()),
                "supplicant_mac": state.map(|p| p.supplicant_mac.clone()).unwrap_or_default(),
                "last_auth_secs_ago": state.and_then(|p| p.last_auth_secs_ago),
                "failures": state.map(|p| p.failures).unwrap_or(0),
            })
        })
        .collect();
    Ok(Json(json!({
        "radius_servers": servers,
        "reauth_interval": live
            .as_ref()
            .map(|l| l.reauth_interval)
            .or(config_reauth)
            .unwrap_or(0),
        "ports": ports,
        "live": live.is_some(),
    })))
}

async fn dot1x_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::security_edit::Dot1xEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::security_edit::apply_dot1x_edit(tree, &edit)
    })
    .await
}

#[derive(Deserialize)]
struct Dot1xReauthRequest {
    port: String,
}

/// `POST /api/dot1x/reauth` — force reauthentication on one port.
async fn dot1x_reauth(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<Dot1xReauthRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .dot1x_reauth(pb::Dot1xReauthRequest { port: request.port })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({ "triggered": response.triggered })))
}

/// `GET /api/snooping-sec` — DHCP-snooping + DAI config (VLAN lists,
/// validate checks, trusted ports, static bindings — all from the
/// running config) merged with orch's binding table and drop counters,
/// which degrade gracefully when orch is unreachable (config statics
/// stand in for the binding table).
async fn snooping_sec(
    _op: Operator,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let text = running_config(&state.mgmtd).await?;
    let tree =
        hemlock_config::parse(&text).map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
    let mut dhcp_vlans: Vec<u32> = Vec::new();
    let mut arp_vlans: Vec<u32> = Vec::new();
    let mut validate: Vec<String> = Vec::new();
    let mut static_bindings: Vec<serde_json::Value> = Vec::new();
    if let Some((_, security)) = tree.block("security") {
        if let Some((_, dhcp)) = ConfigTree::blocks_named(security, "dhcp-snooping").next() {
            dhcp_vlans = repeated_leaves(dhcp, "vlan")
                .iter()
                .filter_map(|v| v.parse().ok())
                .collect();
            for item in dhcp {
                let hemlock_config::Item::Leaf { name, values } = item else {
                    continue;
                };
                if name != "binding" {
                    continue;
                }
                static_bindings.push(json!({
                    "mac": values.first().cloned().unwrap_or_default(),
                    "address": values.get(3).cloned().unwrap_or_default(),
                    "lease_secs": serde_json::Value::Null,
                    "is_static": true,
                    "vlan": values.get(2).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0),
                    "interface": values.get(5).cloned().unwrap_or_default(),
                }));
            }
        }
        if let Some((_, arp)) = ConfigTree::blocks_named(security, "arp-inspection").next() {
            arp_vlans = repeated_leaves(arp, "vlan")
                .iter()
                .filter_map(|v| v.parse().ok())
                .collect();
            validate = repeated_leaves(arp, "validate");
        }
    }
    let mut dhcp_trusted: Vec<String> = Vec::new();
    let mut arp_trusted: Vec<String> = Vec::new();
    if let Some((_, interfaces)) = tree.block("interfaces") {
        for item in interfaces {
            let hemlock_config::Item::Block {
                name,
                keys,
                children,
            } = item
            else {
                continue;
            };
            if !keys.is_empty() {
                continue;
            }
            if ConfigTree::has_leaf(children, "dhcp-snooping") {
                dhcp_trusted.push(name.clone());
            }
            if ConfigTree::has_leaf(children, "arp-inspection") {
                arp_trusted.push(name.clone());
            }
        }
    }
    let live = match orch_client(&state).await {
        Ok(mut orch) => orch
            .get_snoop_sec_state(pb::GetSnoopSecStateRequest {})
            .await
            .map(|response| response.into_inner())
            .ok(),
        Err(_) => None,
    };
    let bindings: Vec<serde_json::Value> = match &live {
        Some(live) => live
            .bindings
            .iter()
            .map(|binding| {
                json!({
                    "mac": binding.mac,
                    "address": binding.address,
                    "lease_secs": binding.lease_secs,
                    "is_static": binding.is_static,
                    "vlan": binding.vlan,
                    "interface": binding.interface,
                })
            })
            .collect(),
        None => static_bindings,
    };
    Ok(Json(json!({
        "dhcp": {
            "vlans": dhcp_vlans,
            "trusted": dhcp_trusted,
            "stats": live.as_ref().map(|l| l.dhcp_stats.iter().map(|s| json!({
                "vlan": s.vlan,
                "packets": s.packets,
                "dropped": s.dropped,
            })).collect::<Vec<_>>()).unwrap_or_default(),
            "untrusted_server_drops": live
                .as_ref()
                .map(|l| l.untrusted_server_drops)
                .unwrap_or(0),
        },
        "arp": {
            "vlans": arp_vlans,
            "validate": validate,
            "trusted": arp_trusted,
            "stats": live.as_ref().map(|l| l.arp_stats.iter().map(|s| json!({
                "vlan": s.vlan,
                "forwarded": s.forwarded,
                "dropped": s.dropped,
                "bad_binding": s.bad_binding,
                "bad_src_mac": s.bad_src_mac,
            })).collect::<Vec<_>>()).unwrap_or_default(),
        },
        "bindings": bindings,
        "live": live.is_some(),
    })))
}

async fn snooping_sec_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::security_edit::SnoopingSecEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::security_edit::apply_snooping_sec_edit(tree, &edit)
    })
    .await
}

#[derive(Deserialize)]
struct SnoopBindingClearRequest {
    /// Scope to one MAC; absent = every dynamic binding.
    #[serde(default)]
    mac: Option<String>,
}

/// `POST /api/snooping-sec/bindings/clear` — drop dynamic bindings.
async fn snooping_sec_bindings_clear(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<SnoopBindingClearRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut orch = orch_client(&state).await?;
    let response = orch
        .clear_snoop_binding(pb::ClearSnoopBindingRequest {
            mac: request.mac.unwrap_or_default(),
        })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner();
    Ok(Json(json!({ "cleared": response.cleared })))
}

async fn static_routes_edit(
    _op: Operator,
    State(state): State<SharedState>,
    Json(edit): Json<crate::routing_edit::StaticRouteEdit>,
) -> Result<Response, ApiError> {
    commit_edit(&state, &_op.0, "web console", |tree| {
        crate::routing_edit::apply_static_route_edit(tree, &edit)
    })
    .await
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
    commit_edit(&state, &_op.0, "web console restore", |tree| {
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
    /// Arm ONIE rescue mode for this boot only. Immediate reboots
    /// only — arming and then waiting would leave the switch in a state
    /// nobody asked for if the schedule were cancelled.
    #[serde(default)]
    onie_rescue: bool,
}

async fn reboot(
    _op: Operator,
    State(state): State<SharedState>,
    Json(request): Json<RebootRequest>,
) -> Response {
    if !cfg!(unix) {
        return errors(vec!["reboot is only available on the switch".to_string()]);
    }
    if request.onie_rescue && request.in_minutes != 0 {
        return errors(vec![
            "ONIE rescue applies to an immediate reboot only".to_string()
        ]);
    }
    if request.in_minutes == 0 {
        // mgmtd owns the reboot: it arms ONIE rescue and holds the
        // engine lock, so no commit lands between the decision and the
        // box going down. The same path `request reboot` takes.
        let mut client = match mgmtd_client(&state).await {
            Ok(client) => client,
            Err(err) => return errors(vec![format!("{err:#}")]),
        };
        return match client
            .reboot(pb::RebootRequest {
                onie_rescue: request.onie_rescue,
            })
            .await
        {
            Ok(response) => Json(json!({
                "rebooting": true,
                "onie_rescue_armed": response.into_inner().onie_rescue_armed,
            }))
            .into_response(),
            Err(status) => errors(vec![status.message().to_string()]),
        };
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

    /// The one test that makes the shared role table load-bearing:
    /// every endpoint that changes something is either in
    /// `ADMIN_WEB_PATHS` or one of the two login paths. Adding a POST
    /// route without gating it fails here rather than shipping open.
    #[test]
    fn every_post_route_is_gated() {
        let ungated: Vec<&str> = post_routes()
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| {
                !hemlock_common::role::web_requires_admin(path) && !PUBLIC_POSTS.contains(path)
            })
            .collect();
        assert!(
            ungated.is_empty(),
            "these POST endpoints are not in hemlock_common::role::ADMIN_WEB_PATHS: {ungated:?}"
        );
    }

    /// And the reverse: a path in the table that no route serves is a
    /// stale entry, which quietly weakens the first test.
    #[test]
    fn the_role_table_names_no_phantom_endpoints() {
        let served: Vec<&str> = get_routes()
            .into_iter()
            .chain(post_routes())
            .map(|(path, _)| path)
            .collect();
        let phantom: Vec<&&str> = hemlock_common::role::ADMIN_WEB_PATHS
            .iter()
            .filter(|path| !served.contains(*path))
            .collect();
        assert!(
            phantom.is_empty(),
            "ADMIN_WEB_PATHS names endpoints webd does not serve: {phantom:?}"
        );
    }

    /// Reading is never gated: an operator sees the whole console.
    #[test]
    fn no_read_only_route_is_gated() {
        let gated: Vec<&str> = get_routes()
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| hemlock_common::role::web_requires_admin(path))
            .collect();
        assert!(
            gated.is_empty(),
            "read-only routes must stay open: {gated:?}"
        );
    }

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
