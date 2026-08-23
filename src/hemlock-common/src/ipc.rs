//! IPC endpoint scaffolding.
//!
//! Hemlock daemons speak tonic gRPC to each other. In production the
//! transport is a unix domain socket under `/run/hemlock/`; for development
//! on non-unix hosts (and for portable integration tests) a TCP loopback
//! endpoint is supported with identical semantics.
//!
//! Endpoint syntax: `unix:/run/hemlock/syncd.sock` or `tcp:127.0.0.1:60051`.
//! A bare path is treated as a unix socket path.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use crate::HemlockError;

/// A parsed daemon endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEndpoint {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

impl FromStr for IpcEndpoint {
    type Err = HemlockError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix("unix:") {
            Ok(Self::Unix(PathBuf::from(path)))
        } else if let Some(addr) = s.strip_prefix("tcp:") {
            let addr = addr
                .parse()
                .map_err(|e| HemlockError::Ipc(format!("bad tcp endpoint {addr:?}: {e}")))?;
            Ok(Self::Tcp(addr))
        } else {
            Ok(Self::Unix(PathBuf::from(s)))
        }
    }
}

impl fmt::Display for IpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(p) => write!(f, "unix:{}", p.display()),
            Self::Tcp(a) => write!(f, "tcp:{a}"),
        }
    }
}

/// The Hemlock daemons with well-known endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Daemon {
    Syncd,
    Pmon,
    Mgmtd,
    Orch,
}

impl Daemon {
    pub fn name(self) -> &'static str {
        match self {
            Self::Syncd => "syncd",
            Self::Pmon => "pmon",
            Self::Mgmtd => "mgmtd",
            Self::Orch => "orch",
        }
    }

    /// Production default: a unix socket under `/run/hemlock/`. On non-unix
    /// development hosts, a fixed TCP loopback port per daemon.
    pub fn default_endpoint(self) -> IpcEndpoint {
        #[cfg(unix)]
        {
            IpcEndpoint::Unix(PathBuf::from(format!("/run/hemlock/{}.sock", self.name())))
        }
        #[cfg(not(unix))]
        {
            let port = match self {
                Self::Syncd => 60051,
                Self::Pmon => 60052,
                Self::Mgmtd => 60053,
                Self::Orch => 60054,
            };
            IpcEndpoint::Tcp(SocketAddr::from(([127, 0, 0, 1], port)))
        }
    }
}

/// tonic's transport error Displays as just "transport error"; the
/// actionable part (ECONNREFUSED vs EACCES vs ENOENT) is the deepest
/// source in its chain.
fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut deepest = err;
    while let Some(source) = deepest.source() {
        deepest = source;
    }
    deepest.to_string()
}

/// A unix connect() needs write permission on the socket inode, but the
/// daemons run as root and `UnixListener::bind` leaves the socket at
/// umask defaults (0755) — operator accounts get EACCES. Hand the socket
/// to the `hemlock` group (created at image build; operator accounts
/// belong to it) with group rw. Without the group (dev machines) the
/// socket stays owner-only, which covers same-user development.
#[cfg(unix)]
fn grant_operator_access(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let gid = hemlock_gid();
    if let Some(gid) = gid {
        if let Err(e) = std::os::unix::fs::chown(path, None, Some(gid)) {
            tracing::warn!(path = %path.display(), error = %e, "chgrp hemlock on socket failed");
        }
    }
    let mode = if gid.is_some() { 0o660 } else { 0o600 };
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        tracing::warn!(path = %path.display(), error = %e, "chmod on socket failed");
    }
}

/// The gid of the `hemlock` group, straight from /etc/group (avoids a
/// libc dependency for one getgrnam call).
#[cfg(unix)]
fn hemlock_gid() -> Option<u32> {
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    groups.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != "hemlock" {
            return None;
        }
        let _password = fields.next()?;
        fields.next()?.trim().parse().ok()
    })
}

impl IpcEndpoint {
    /// Serve a tonic router on this endpoint until `shutdown` resolves.
    pub async fn serve(
        &self,
        router: tonic::transport::server::Router,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), HemlockError> {
        match self {
            IpcEndpoint::Tcp(addr) => {
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .map_err(|e| HemlockError::Ipc(format!("bind {addr}: {e}")))?;
                let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
                router
                    .serve_with_incoming_shutdown(incoming, shutdown)
                    .await
                    .map_err(|e| HemlockError::Ipc(format!("serve {self}: {e}")))
            }
            #[cfg(unix)]
            IpcEndpoint::Unix(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| HemlockError::io(dir, e))?;
                }
                // Remove a stale socket from a previous run.
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(HemlockError::io(path, e)),
                }
                let listener =
                    tokio::net::UnixListener::bind(path).map_err(|e| HemlockError::io(path, e))?;
                grant_operator_access(path);
                let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
                router
                    .serve_with_incoming_shutdown(incoming, shutdown)
                    .await
                    .map_err(|e| HemlockError::Ipc(format!("serve {self}: {e}")))
            }
            #[cfg(not(unix))]
            IpcEndpoint::Unix(path) => Err(HemlockError::Ipc(format!(
                "unix socket endpoints are not supported on this platform: {}",
                path.display()
            ))),
        }
    }

    /// Connect a tonic channel to this endpoint. Connecting is bounded
    /// (10s) so a hung peer surfaces as an error instead of blocking the
    /// caller forever; individual RPCs have no deadline (streams stay
    /// open indefinitely) — callers that only make unary calls and must
    /// never wedge use [`Self::connect_with_request_timeout`].
    pub async fn connect(&self) -> Result<tonic::transport::Channel, HemlockError> {
        self.connect_inner(None).await
    }

    /// Like [`Self::connect`], but every RPC on the channel also gets a
    /// per-request deadline. Only for clients that make unary calls —
    /// the deadline would kill long-lived streaming RPCs.
    pub async fn connect_with_request_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<tonic::transport::Channel, HemlockError> {
        self.connect_inner(Some(timeout)).await
    }

    async fn connect_inner(
        &self,
        request_timeout: Option<std::time::Duration>,
    ) -> Result<tonic::transport::Channel, HemlockError> {
        const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        let apply = |endpoint: tonic::transport::Endpoint| {
            let endpoint = endpoint.connect_timeout(CONNECT_TIMEOUT);
            match request_timeout {
                Some(t) => endpoint.timeout(t),
                None => endpoint,
            }
        };
        // A generic fn, not a closure: the two arms pass distinct opaque
        // future types, which a closure's inferred argument can't unify.
        fn bounded<F: std::future::Future>(future: F) -> tokio::time::Timeout<F> {
            tokio::time::timeout(CONNECT_TIMEOUT, future)
        }
        match self {
            IpcEndpoint::Tcp(addr) => {
                let uri = format!("http://{addr}");
                let endpoint = apply(
                    tonic::transport::Endpoint::try_from(uri)
                        .map_err(|e| HemlockError::Ipc(e.to_string()))?,
                );
                bounded(endpoint.connect())
                    .await
                    .map_err(|_| HemlockError::Ipc(format!("connect {self}: timed out")))?
                    .map_err(|e| HemlockError::Ipc(format!("connect {self}: {}", root_cause(&e))))
            }
            #[cfg(unix)]
            IpcEndpoint::Unix(path) => {
                let path = path.clone();
                // The URI is required by the API but unused for unix sockets.
                let endpoint = apply(
                    tonic::transport::Endpoint::try_from("http://hemlock.invalid")
                        .map_err(|e| HemlockError::Ipc(e.to_string()))?,
                );
                bounded(endpoint.connect_with_connector(tower::service_fn(move |_| {
                    let path = path.clone();
                    async move {
                        let stream = tokio::net::UnixStream::connect(path).await?;
                        Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                    }
                })))
                .await
                .map_err(|_| HemlockError::Ipc(format!("connect {self}: timed out")))?
                .map_err(|e| HemlockError::Ipc(format!("connect {self}: {}", root_cause(&e))))
            }
            #[cfg(not(unix))]
            IpcEndpoint::Unix(path) => Err(HemlockError::Ipc(format!(
                "unix socket endpoints are not supported on this platform: {}",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_forms() {
        assert_eq!(
            "unix:/run/hemlock/syncd.sock"
                .parse::<IpcEndpoint>()
                .unwrap(),
            IpcEndpoint::Unix(PathBuf::from("/run/hemlock/syncd.sock"))
        );
        assert_eq!(
            "tcp:127.0.0.1:60051".parse::<IpcEndpoint>().unwrap(),
            IpcEndpoint::Tcp(SocketAddr::from(([127, 0, 0, 1], 60051)))
        );
        // Bare path defaults to unix.
        assert_eq!(
            "/tmp/x.sock".parse::<IpcEndpoint>().unwrap(),
            IpcEndpoint::Unix(PathBuf::from("/tmp/x.sock"))
        );
        assert!("tcp:nonsense".parse::<IpcEndpoint>().is_err());
    }

    #[test]
    fn display_round_trips() {
        for s in ["unix:/run/hemlock/pmon.sock", "tcp:127.0.0.1:60052"] {
            assert_eq!(s.parse::<IpcEndpoint>().unwrap().to_string(), s);
        }
    }
}
