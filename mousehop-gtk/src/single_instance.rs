//! Process-wide single-instance guard for the mousehop preferences
//! GUI.
//!
//! The daemon already single-instances itself by binding
//! `mousehop-socket.sock` (see [`mousehop_ipc::AsyncFrontendListener`]),
//! but the GUI frontend had no equivalent guard — every `mousehop`
//! invocation built its own preferences window. This mirrors the
//! daemon's socket trick on a separate `mousehop-gui.sock` so a
//! second GUI launch detects the first and bows out.
//!
//! GApplication's D-Bus uniqueness covers this on Linux but is
//! unreliable on macOS (no session bus), so we don't rely on it.

use std::io::ErrorKind;

#[cfg(unix)]
use std::{
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

#[cfg(windows)]
use std::net::TcpListener;

use thiserror::Error;

/// TCP port used as the GUI single-instance lock on Windows. One
/// past the daemon's `127.0.0.1:5252` (see `mousehop_ipc`'s listener).
#[cfg(windows)]
const GUI_LOCK_PORT: u16 = 5253;

/// Filename of the GUI lock socket on Unix. Lives alongside the
/// daemon's `mousehop-socket.sock` — in `$XDG_RUNTIME_DIR` on Linux,
/// `~/Library/Caches` on macOS.
#[cfg(unix)]
const GUI_SOCKET_NAME: &str = "mousehop-gui.sock";

#[derive(Debug, Error)]
pub(crate) enum AcquireError {
    #[error("another mousehop preferences window is already running")]
    AlreadyRunning,
    #[error("could not determine the GUI lock path: {0}")]
    SocketPath(#[from] mousehop_ipc::SocketPathError),
    #[error("could not bind the GUI lock: {0}")]
    Bind(std::io::Error),
}

/// Held for the lifetime of the GUI process. While it is alive, an
/// `acquire()` from another process returns
/// [`AcquireError::AlreadyRunning`]. Dropping it releases the lock.
pub(crate) struct SingleInstanceGuard {
    #[cfg(unix)]
    _listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(windows)]
    _listener: TcpListener,
}

#[cfg(unix)]
pub(crate) fn acquire() -> Result<SingleInstanceGuard, AcquireError> {
    // Reuse the daemon's platform-correct directory logic and just
    // swap the filename so the GUI lock lands next to the daemon's.
    let socket_path = mousehop_ipc::default_socket_path()?.with_file_name(GUI_SOCKET_NAME);

    if socket_path.exists() {
        // A live GUI keeps this socket in the listening state, so a
        // connect succeeds; a connect failure means the file is a
        // stale leftover from a crashed GUI and is safe to remove.
        match UnixStream::connect(&socket_path) {
            Ok(_) => return Err(AcquireError::AlreadyRunning),
            Err(e) => {
                log::debug!("{socket_path:?}: {e} — removing stale GUI lock socket");
                let _ = std::fs::remove_file(&socket_path);
            }
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        // Another GUI bound it in the gap between our check and bind.
        Err(e) if e.kind() == ErrorKind::AddrInUse => return Err(AcquireError::AlreadyRunning),
        Err(e) => return Err(AcquireError::Bind(e)),
    };

    Ok(SingleInstanceGuard {
        _listener: listener,
        socket_path,
    })
}

#[cfg(windows)]
pub(crate) fn acquire() -> Result<SingleInstanceGuard, AcquireError> {
    match TcpListener::bind(("127.0.0.1", GUI_LOCK_PORT)) {
        Ok(listener) => Ok(SingleInstanceGuard {
            _listener: listener,
        }),
        Err(e) if e.kind() == ErrorKind::AddrInUse => Err(AcquireError::AlreadyRunning),
        Err(e) => Err(AcquireError::Bind(e)),
    }
}

#[cfg(unix)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        // Best-effort: a crash skips this and leaves a stale socket,
        // which the next `acquire()` detects and clears.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
