//! Minimal sd_notify: tell systemd a `Type=notify` service is ready.
//!
//! Implemented directly over the `NOTIFY_SOCKET` datagram protocol so the
//! daemons need no libsystemd binding. A missing socket (manual runs,
//! dev hosts, tests) is a silent no-op.

/// Send `READY=1` to the service manager. Errors are returned for the
/// caller to log; they must never be fatal — the daemon is exactly as
/// functional without systemd watching.
#[cfg(unix)]
pub fn notify_ready() -> std::io::Result<bool> {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(false);
    };
    let socket = std::os::unix::net::UnixDatagram::unbound()?;
    let bytes = path.as_encoded_bytes();
    if let Some(name) = bytes.strip_prefix(b"@") {
        // Abstract-namespace socket (Linux only).
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            socket.send_to_addr(b"READY=1", &addr)?;
            return Ok(true);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            return Ok(false);
        }
    }
    socket.send_to(b"READY=1", std::path::Path::new(&path))?;
    Ok(true)
}

#[cfg(not(unix))]
pub fn notify_ready() -> std::io::Result<bool> {
    Ok(false)
}
