//! The 802.1X engine. hostapd is the authenticator (wired driver on
//! the hostif netdevs) and the RADIUS client; orch owns it end to end —
//! unlike FRR (mgmtd renders config, orch only queries), dot1x runtime
//! auth state must drive dataplane changes, so this engine renders the
//! hostapd config, manages the process, watches the control socket, and
//! flips port authorization via syncd's `SetPortAuthorized` (the
//! internal permit-EAPOL + deny-all ACL entries).
//!
//! Channel shape like the other engines: config and control-socket
//! events in; authorization decisions and hostapd directives out. The
//! control socket is abstracted as a [`CtrlEvent`] stream so engine
//! tests drive a scripted hostapd-ctrl simulator instead of a process.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// RADIUS servers, tried in config order.
    pub radius: Vec<Radius>,
    /// Seconds; 0 = reauthentication off.
    pub reauth_interval: u32,
    /// Ports running the authenticator.
    pub ports: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Radius {
    pub ip: String,
    pub key: String,
    pub port: u16,
    pub timeout: u16,
    pub retransmit: u16,
}

/// One hostapd control-socket event, resolved to a port. Constructed
/// by the (unix-only) runtime's line parser and by engine tests.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtrlEvent {
    /// EAP authentication succeeded for `mac`.
    AuthSuccess { port: String, mac: String },
    /// EAP failed (bad credentials, RADIUS timeout).
    AuthFailure { port: String, mac: String },
    /// The supplicant logged off or disconnected.
    Logoff { port: String, mac: String },
}

/// What the hostapd runtime should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    /// Config changed: re-render every port config and restart the
    /// process.
    Reconfigure(Config),
    /// Force reauthentication on one port (`clear dot1x interface`).
    Reauth(String),
}

#[derive(Debug, Clone)]
struct PortState {
    authorized: bool,
    supplicant: Option<String>,
    last_auth: Option<Instant>,
    failures: u32,
}

#[derive(Debug, Clone)]
pub struct PortSnapshot {
    pub port: String,
    pub authorized: bool,
    pub supplicant: Option<String>,
    pub last_auth: Option<Instant>,
    pub failures: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub radius: Vec<Radius>,
    pub reauth_interval: u32,
    pub ports: Vec<PortSnapshot>,
}

struct Inner {
    config: Config,
    ports: BTreeMap<String, PortState>,
}

pub struct EngineIo {
    /// Feed hostapd control-socket events here (the runtime, or a
    /// scripted simulator in tests).
    pub events_in: mpsc::UnboundedSender<CtrlEvent>,
    /// Authorization flips: (port, authorized) -> syncd
    /// SetPortAuthorized.
    pub auth_out: mpsc::UnboundedReceiver<(String, bool)>,
    /// Directives for the hostapd runtime.
    pub directives: mpsc::UnboundedReceiver<Directive>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    auth_tx: mpsc::UnboundedSender<(String, bool)>,
    directives_tx: mpsc::UnboundedSender<Directive>,
}

impl Engine {
    pub fn spawn() -> (Engine, EngineIo) {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel::<CtrlEvent>();
        let (auth_tx, auth_rx) = mpsc::unbounded_channel();
        let (directives_tx, directives_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Mutex::new(Inner {
            config: Config::default(),
            ports: BTreeMap::new(),
        }));
        let engine = Engine {
            inner: inner.clone(),
            auth_tx: auth_tx.clone(),
            directives_tx,
        };
        {
            let engine = engine.clone();
            tokio::spawn(async move {
                while let Some(event) = events_rx.recv().await {
                    engine.handle_event(event);
                }
            });
        }
        (
            engine,
            EngineIo {
                events_in: events_tx,
                auth_out: auth_rx,
                directives: directives_rx,
            },
        )
    }

    /// Full desired state, declaratively. New dot1x ports start
    /// unauthorized (enforcement on before the first supplicant);
    /// removed ports are re-authorized (enforcement off).
    pub fn set_config(&self, config: Config) {
        let changed_process;
        {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            if inner.config == config {
                return;
            }
            changed_process = inner.config.radius != config.radius
                || inner.config.reauth_interval != config.reauth_interval
                || inner.config.ports != config.ports;
            let stale: Vec<String> = inner
                .ports
                .keys()
                .filter(|port| !config.ports.contains(*port))
                .cloned()
                .collect();
            for port in stale {
                inner.ports.remove(&port);
                let _ = self.auth_tx.send((port, true));
            }
            for port in &config.ports {
                if !inner.ports.contains_key(port) {
                    inner.ports.insert(
                        port.clone(),
                        PortState {
                            authorized: false,
                            supplicant: None,
                            last_auth: None,
                            failures: 0,
                        },
                    );
                    let _ = self.auth_tx.send((port.clone(), false));
                }
            }
            inner.config = config.clone();
        }
        if changed_process {
            let _ = self.directives_tx.send(Directive::Reconfigure(config));
        }
    }

    fn handle_event(&self, event: CtrlEvent) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        match event {
            CtrlEvent::AuthSuccess { port, mac } => {
                let Some(state) = inner.ports.get_mut(&port) else {
                    return;
                };
                let flip = !state.authorized;
                state.authorized = true;
                state.supplicant = Some(mac);
                state.last_auth = Some(Instant::now());
                if flip {
                    let _ = self.auth_tx.send((port, true));
                }
            }
            CtrlEvent::AuthFailure { port, mac } => {
                let Some(state) = inner.ports.get_mut(&port) else {
                    return;
                };
                state.failures += 1;
                let flip = state.authorized;
                state.authorized = false;
                state.supplicant = (!mac.is_empty()).then_some(mac);
                if flip {
                    let _ = self.auth_tx.send((port, false));
                }
            }
            CtrlEvent::Logoff { port, .. } => {
                let Some(state) = inner.ports.get_mut(&port) else {
                    return;
                };
                let flip = state.authorized;
                state.authorized = false;
                state.supplicant = None;
                if flip {
                    let _ = self.auth_tx.send((port, false));
                }
            }
        }
    }

    /// Force reauthentication (`clear dot1x interface <port>`); false
    /// when the port runs no authenticator.
    pub fn reauth(&self, port: &str) -> bool {
        let enabled = self
            .inner
            .lock()
            .map(|inner| inner.ports.contains_key(port))
            .unwrap_or(false);
        if enabled {
            let _ = self.directives_tx.send(Directive::Reauth(port.to_string()));
        }
        enabled
    }

    pub fn snapshot(&self) -> Snapshot {
        let Ok(inner) = self.inner.lock() else {
            return Snapshot::default();
        };
        Snapshot {
            radius: inner.config.radius.clone(),
            reauth_interval: inner.config.reauth_interval,
            ports: inner
                .ports
                .iter()
                .map(|(port, state)| PortSnapshot {
                    port: port.clone(),
                    authorized: state.authorized,
                    supplicant: state.supplicant.clone(),
                    last_auth: state.last_auth,
                    failures: state.failures,
                })
                .collect(),
        }
    }
}

/// Render one port's hostapd config (wired driver on the port's hostif
/// netdev). One instance serves every port, one config file each;
/// RADIUS secrets ride in the file, so the runtime writes it mode
/// 0600.
pub fn render_hostapd_conf(port: &str, config: &Config, ctrl_dir: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("interface={port}\n"));
    out.push_str("driver=wired\n");
    out.push_str("ieee8021x=1\n");
    out.push_str("use_pae_group_addr=1\n");
    out.push_str(&format!("ctrl_interface={ctrl_dir}\n"));
    if config.reauth_interval > 0 {
        out.push_str(&format!("eap_reauth_period={}\n", config.reauth_interval));
    }
    // The switch's own RADIUS identity.
    out.push_str("own_ip_addr=127.0.0.1\n");
    for radius in &config.radius {
        out.push_str(&format!("auth_server_addr={}\n", radius.ip));
        out.push_str(&format!("auth_server_port={}\n", radius.port));
        out.push_str(&format!("auth_server_shared_secret={}\n", radius.key));
    }
    if let Some(first) = config.radius.first() {
        out.push_str(&format!(
            "radius_retry_primary_interval={}\n",
            u32::from(first.timeout) * (u32::from(first.retransmit) + 1)
        ));
    }
    out
}

/// The hostapd runtime: consume directives, render configs (0600),
/// (re)start the process, and translate its control-socket traffic
/// into [`CtrlEvent`]s. Linux-only and best-effort — a dev host
/// without hostapd still runs the engine (ports simply stay
/// unauthorized until a real authenticator answers).
pub async fn run_hostapd(
    mut directives: mpsc::UnboundedReceiver<Directive>,
    events: mpsc::UnboundedSender<CtrlEvent>,
) {
    let run_dir = std::path::PathBuf::from("/run/hemlock/hostapd");
    let mut child: Option<std::process::Child> = None;
    while let Some(directive) = directives.recv().await {
        match directive {
            Directive::Reconfigure(config) => {
                if let Some(mut old) = child.take() {
                    let _ = old.kill();
                    let _ = old.wait();
                }
                if config.ports.is_empty() || config.radius.is_empty() {
                    continue;
                }
                if let Err(err) = std::fs::create_dir_all(&run_dir) {
                    tracing::warn!(%err, "cannot create hostapd run dir");
                    continue;
                }
                let mut conf_paths = Vec::new();
                for port in &config.ports {
                    let path = run_dir.join(format!("{port}.conf"));
                    let text =
                        render_hostapd_conf(port, &config, &run_dir.join("ctrl").to_string_lossy());
                    if let Err(err) = write_private(&path, &text) {
                        tracing::warn!(%err, %port, "cannot write hostapd config");
                        continue;
                    }
                    conf_paths.push(path);
                }
                match std::process::Command::new("hostapd")
                    .args(conf_paths.iter().map(|p| p.as_os_str()))
                    .spawn()
                {
                    Ok(spawned) => {
                        child = Some(spawned);
                        for port in &config.ports {
                            spawn_ctrl_reader(
                                run_dir.join("ctrl").join(port),
                                port.clone(),
                                events.clone(),
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, "cannot start hostapd; dot1x ports stay unauthorized");
                    }
                }
            }
            Directive::Reauth(port) => {
                // Poke the port's control socket; hostapd re-runs EAP.
                send_ctrl_command(&run_dir.join("ctrl").join(&port), "REAUTHENTICATE");
            }
        }
    }
}

/// Write a file the RADIUS secret rides in: owner-only permissions.
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Attach to one port's hostapd control socket and translate its
/// unsolicited events. Best-effort: a missing socket retries slowly.
fn spawn_ctrl_reader(
    socket: std::path::PathBuf,
    port: String,
    events: mpsc::UnboundedSender<CtrlEvent>,
) {
    std::thread::Builder::new()
        .name(format!("hostapd-ctrl-{port}"))
        .spawn(move || {
            #[cfg(unix)]
            loop {
                let Ok(stream) = attach_ctrl(&socket) else {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    continue;
                };
                let mut buffer = [0u8; 1024];
                while let Ok(n) = stream.recv(&mut buffer) {
                    let line = String::from_utf8_lossy(&buffer[..n]);
                    if let Some(event) = parse_ctrl_event(&port, &line) {
                        if events.send(event).is_err() {
                            return;
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = (socket, port, events);
            }
        })
        .ok();
}

#[cfg(unix)]
fn attach_ctrl(socket: &std::path::Path) -> std::io::Result<std::os::unix::net::UnixDatagram> {
    let local = std::env::temp_dir().join(format!("hemlock-hostapd-{}", std::process::id()));
    let _ = std::fs::remove_file(&local);
    let stream = std::os::unix::net::UnixDatagram::bind(&local)?;
    stream.connect(socket)?;
    stream.send(b"ATTACH")?;
    Ok(stream)
}

#[cfg(unix)]
fn send_ctrl_command(socket: &std::path::Path, command: &str) {
    if let Ok(stream) = attach_ctrl(socket) {
        let _ = stream.send(command.as_bytes());
    }
}

#[cfg(not(unix))]
fn send_ctrl_command(_socket: &std::path::Path, _command: &str) {}

/// One hostapd control-interface line -> a [`CtrlEvent`]. The wired
/// driver reports stations by MAC.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_ctrl_event(port: &str, line: &str) -> Option<CtrlEvent> {
    let line = line.trim_start_matches(|c: char| c == '<' || c.is_ascii_digit() || c == '>');
    let mac_after = |marker: &str| {
        line.split(marker)
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_string)
    };
    if line.contains("CTRL-EVENT-EAP-SUCCESS") || line.contains("AP-STA-CONNECTED") {
        return Some(CtrlEvent::AuthSuccess {
            port: port.to_string(),
            mac: mac_after("AP-STA-CONNECTED ")
                .or_else(|| mac_after("CTRL-EVENT-EAP-SUCCESS "))
                .unwrap_or_default(),
        });
    }
    if line.contains("CTRL-EVENT-EAP-FAILURE") {
        return Some(CtrlEvent::AuthFailure {
            port: port.to_string(),
            mac: mac_after("CTRL-EVENT-EAP-FAILURE ").unwrap_or_default(),
        });
    }
    if line.contains("AP-STA-DISCONNECTED") {
        return Some(CtrlEvent::Logoff {
            port: port.to_string(),
            mac: mac_after("AP-STA-DISCONNECTED ").unwrap_or_default(),
        });
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn config(ports: &[&str]) -> Config {
        Config {
            radius: vec![Radius {
                ip: "10.42.0.5".into(),
                key: "s3cret".into(),
                port: 1812,
                timeout: 5,
                retransmit: 3,
            }],
            reauth_interval: 3600,
            ports: ports.iter().map(|p| p.to_string()).collect(),
        }
    }

    /// The scripted hostapd-ctrl flow: enable -> unauthorized, EAP
    /// success -> authorized, logoff / reauth-failure -> back to
    /// unauthorized, disable -> re-authorized. Every dataplane flip is
    /// observed on `auth_out` (what syncd would receive).
    #[tokio::test]
    async fn auth_flow_drives_port_flips() {
        let (engine, mut io) = Engine::spawn();
        engine.set_config(config(&["Ethernet10"]));
        assert_eq!(
            io.auth_out.recv().await.unwrap(),
            ("Ethernet10".into(), false)
        );
        assert!(matches!(
            io.directives.recv().await.unwrap(),
            Directive::Reconfigure(_)
        ));

        io.events_in
            .send(CtrlEvent::AuthSuccess {
                port: "Ethernet10".into(),
                mac: "00:1c:73:0c:aa:10".into(),
            })
            .unwrap();
        assert_eq!(
            io.auth_out.recv().await.unwrap(),
            ("Ethernet10".into(), true)
        );
        let snapshot = engine.snapshot();
        assert!(snapshot.ports[0].authorized);
        assert_eq!(
            snapshot.ports[0].supplicant.as_deref(),
            Some("00:1c:73:0c:aa:10")
        );

        // Logoff drops authorization without counting a failure.
        io.events_in
            .send(CtrlEvent::Logoff {
                port: "Ethernet10".into(),
                mac: "00:1c:73:0c:aa:10".into(),
            })
            .unwrap();
        assert_eq!(
            io.auth_out.recv().await.unwrap(),
            ("Ethernet10".into(), false)
        );
        assert_eq!(engine.snapshot().ports[0].failures, 0);

        // A reauth failure counts and keeps the port unauthorized.
        io.events_in
            .send(CtrlEvent::AuthFailure {
                port: "Ethernet10".into(),
                mac: "00:1c:73:0c:aa:10".into(),
            })
            .unwrap();
        // No flip (already unauthorized): poll the snapshot instead.
        for _ in 0..100 {
            if engine.snapshot().ports[0].failures == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(engine.snapshot().ports[0].failures, 1);

        // Forced reauth is a directive for hostapd, not a state change.
        assert!(engine.reauth("Ethernet10"));
        assert_eq!(
            io.directives.recv().await.unwrap(),
            Directive::Reauth("Ethernet10".into())
        );
        assert!(!engine.reauth("Ethernet11"));

        // Disabling re-authorizes (enforcement entries removed).
        engine.set_config(config(&[]));
        assert_eq!(
            io.auth_out.recv().await.unwrap(),
            ("Ethernet10".into(), true)
        );
        assert!(engine.snapshot().ports.is_empty());
    }

    #[test]
    fn hostapd_conf_renders_wired_authenticator() {
        let conf = render_hostapd_conf(
            "Ethernet10",
            &config(&["Ethernet10"]),
            "/run/hemlock/hostapd/ctrl",
        );
        assert!(conf.contains("interface=Ethernet10\n"));
        assert!(conf.contains("driver=wired\n"));
        assert!(conf.contains("ieee8021x=1\n"));
        assert!(conf.contains("eap_reauth_period=3600\n"));
        assert!(conf.contains("auth_server_addr=10.42.0.5\n"));
        assert!(conf.contains("auth_server_shared_secret=s3cret\n"));
    }

    #[test]
    fn ctrl_lines_parse() {
        assert_eq!(
            parse_ctrl_event("Ethernet10", "<3>AP-STA-CONNECTED 00:1c:73:0c:aa:10"),
            Some(CtrlEvent::AuthSuccess {
                port: "Ethernet10".into(),
                mac: "00:1c:73:0c:aa:10".into()
            })
        );
        assert_eq!(
            parse_ctrl_event("Ethernet10", "<3>CTRL-EVENT-EAP-FAILURE 00:1c:73:0c:aa:10"),
            Some(CtrlEvent::AuthFailure {
                port: "Ethernet10".into(),
                mac: "00:1c:73:0c:aa:10".into()
            })
        );
        assert_eq!(
            parse_ctrl_event("Ethernet10", "<3>AP-STA-DISCONNECTED 00:1c:73:0c:aa:10"),
            Some(CtrlEvent::Logoff {
                port: "Ethernet10".into(),
                mac: "00:1c:73:0c:aa:10".into()
            })
        );
        assert_eq!(
            parse_ctrl_event("Ethernet10", "<3>CTRL-EVENT-CONNECTED"),
            None
        );
    }
}
