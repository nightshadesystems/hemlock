//! hemlock-webd — the web console daemon.
//!
//! Serves the exported Next.js UI (static files) and a JSON API from one
//! origin, entirely in Rust (axum + rustls). State comes from the other
//! daemons over gRPC: syncd for interface/VLAN state, mgmtd for the
//! running config. Nothing here mutates config — phase 1 is read-only
//! plus login.
//!
//! The daemon is config-driven like sshd: mgmtd enables/disables the
//! systemd unit from `system { http }` / `system { https }`, and webd
//! reads the running config at startup to decide which listeners to
//! bind. Enabling https generates a self-signed certificate on first
//! use (see `tls`).

mod api;
mod auth;
mod edit;
mod tls;
mod users;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use hemlock_common::ipc::{Daemon, IpcEndpoint};
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "hemlock-webd", version = hemlock_common::VERSION, about)]
struct Args {
    /// mgmtd endpoint (running config, service state).
    #[arg(long)]
    mgmtd: Option<String>,

    /// syncd endpoint (interface and VLAN state).
    #[arg(long)]
    syncd: Option<String>,

    /// Directory holding the exported web UI (Next.js static export).
    #[arg(long, default_value_os_t = default_assets_dir())]
    assets: PathBuf,

    /// State directory (self-signed TLS certificate).
    #[arg(long, default_value_os_t = default_state_dir())]
    state_dir: PathBuf,

    /// Address the listeners bind on.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,

    #[arg(long, default_value_t = 80)]
    http_port: u16,

    #[arg(long, default_value_t = 443)]
    https_port: u16,

    /// Override the config-driven listener set (`http`, `https`, or
    /// `http,https`) — development without a running mgmtd.
    #[arg(long)]
    dev_listen: Option<String>,

    /// `user:password` accepted instead of the system user database —
    /// development hosts without /etc/shadow. Never set on a switch.
    #[arg(long)]
    dev_auth: Option<String>,
}

fn default_assets_dir() -> PathBuf {
    if cfg!(unix) {
        "/usr/share/hemlock/web".into()
    } else {
        "web/out".into()
    }
}

fn default_state_dir() -> PathBuf {
    if cfg!(unix) {
        "/var/lib/hemlock/web".into()
    } else {
        ".hemlock-web-state".into()
    }
}

/// Which listeners to run, from `system { http }` / `system { https }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Listeners {
    http: bool,
    https: bool,
}

fn parse_dev_listen(spec: &str) -> Result<Listeners> {
    let mut listeners = Listeners {
        http: false,
        https: false,
    };
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part {
            "http" => listeners.http = true,
            "https" => listeners.https = true,
            other => bail!("bad --dev-listen entry {other:?} (http or https)"),
        }
    }
    Ok(listeners)
}

/// The running config decides the listener set. mgmtd may still be
/// starting (boot order is not guaranteed), so retry for a while.
async fn config_listeners(mgmtd: &IpcEndpoint) -> Result<Listeners> {
    let mut delay = std::time::Duration::from_millis(500);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match api::running_config(mgmtd).await {
            Ok(text) => {
                let tree = hemlock_config::parse(&text)
                    .map_err(|e| anyhow::anyhow!("parsing running config: {e}"))?;
                let services = api::services_of(&tree);
                return Ok(Listeners {
                    http: services.http,
                    https: services.https,
                });
            }
            Err(err) if std::time::Instant::now() < deadline => {
                warn!(%err, "cannot read running config yet; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(5));
            }
            Err(err) => return Err(err).context("reading running config from mgmtd"),
        }
    }
}

fn hostname() -> String {
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "hemlock".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    hemlock_common::logging::init("info");
    let args = Args::parse();

    // rustls needs a process-wide crypto provider before any TLS config
    // is built; ring is the pure-build choice (no cmake/nasm).
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        warn!("rustls crypto provider was already installed");
    }

    let mgmtd: IpcEndpoint = match &args.mgmtd {
        Some(s) => s.parse()?,
        None => Daemon::Mgmtd.default_endpoint(),
    };
    let syncd: IpcEndpoint = match &args.syncd {
        Some(s) => s.parse()?,
        None => Daemon::Syncd.default_endpoint(),
    };

    let listeners = match &args.dev_listen {
        Some(spec) => parse_dev_listen(spec)?,
        None => config_listeners(&mgmtd).await?,
    };
    if !listeners.http && !listeners.https {
        // Not an error: a commit can disable the web UI between mgmtd's
        // systemctl calls and this process starting.
        info!("web ui is disabled in the running config; exiting");
        return Ok(());
    }

    let dev_auth = match &args.dev_auth {
        Some(spec) => match spec.split_once(':') {
            Some((user, password)) => {
                warn!(user, "development credential enabled (--dev-auth)");
                Some((user.to_string(), password.to_string()))
            }
            None => bail!("--dev-auth wants user:password"),
        },
        None => None,
    };

    if !args.assets.join("index.html").exists() {
        warn!(assets = %args.assets.display(), "no web UI build found; only /api will respond");
    }

    let state: api::SharedState = Arc::new(api::AppState {
        mgmtd,
        syncd,
        sessions: auth::Sessions::new(),
        dev_auth,
        // Cookies are marked Secure only when no plaintext listener
        // exists — with http enabled, a Secure cookie would break http
        // logins outright.
        secure_cookie: listeners.https && !listeners.http,
    });

    let static_files = ServeDir::new(&args.assets)
        .append_index_html_on_directories(true)
        .not_found_service(ServeFile::new(args.assets.join("404.html")));
    let app = api::router(state).fallback_service(static_files);

    let mut servers = tokio::task::JoinSet::new();
    if listeners.http {
        let addr = SocketAddr::new(args.bind, args.http_port);
        info!(%addr, "serving http");
        let app = app.clone();
        servers.spawn(async move { axum_server::bind(addr).serve(app.into_make_service()).await });
    }
    if listeners.https {
        let (cert, key) = tls::ensure_cert(&args.state_dir, &hostname())?;
        let config = RustlsConfig::from_pem_file(&cert, &key)
            .await
            .with_context(|| format!("loading TLS material from {}", args.state_dir.display()))?;
        let addr = SocketAddr::new(args.bind, args.https_port);
        info!(%addr, cert = %cert.display(), "serving https");
        let app = app.clone();
        servers.spawn(async move {
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await
        });
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            Ok(())
        }
        Some(finished) = servers.join_next() => {
            match finished {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(err).context("server error"),
                Err(err) => Err(err).context("server task panicked"),
            }
        }
    }
}
