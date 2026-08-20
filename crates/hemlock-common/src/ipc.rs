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
                let listener = tokio::net::UnixListener::bind(path)
                    .map_err(|e| HemlockError::io(path, e))?;
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

    /// Connect a tonic channel to this endpoint.
    pub async fn connect(&self) -> Result<tonic::transport::Channel, HemlockError> {
        match self {
            IpcEndpoint::Tcp(addr) => {
                let uri = format!("http://{addr}");
                tonic::transport::Endpoint::try_from(uri)
                    .map_err(|e| HemlockError::Ipc(e.to_string()))?
                    .connect()
                    .await
                    .map_err(|e| HemlockError::Ipc(format!("connect {self}: {e}")))
            }
            #[cfg(unix)]
            IpcEndpoint::Unix(path) => {
                let path = path.clone();
                // The URI is required by the API but unused for unix sockets.
                tonic::transport::Endpoint::try_from("http://hemlock.invalid")
                    .map_err(|e| HemlockError::Ipc(e.to_string()))?
                    .connect_with_connector(tower::service_fn(move |_| {
                        let path = path.clone();
                        async move {
                            let stream = tokio::net::UnixStream::connect(path).await?;
                            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                        }
                    }))
                    .await
                    .map_err(|e| HemlockError::Ipc(format!("connect {self}: {e}")))
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
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_forms() {
        assert_eq!(
            "unix:/run/hemlock/syncd.sock".parse::<IpcEndpoint>().unwrap(),
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
