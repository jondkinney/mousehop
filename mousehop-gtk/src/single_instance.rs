//! Process-wide single-instance guard for the mousehop preferences
//! GUI, with cross-process "raise the existing window" signalling.
//!
//! The daemon already single-instances itself by binding
//! `mousehop-socket.sock` (see [`mousehop_ipc::AsyncFrontendListener`]),
//! but the GUI frontend had no equivalent guard — every `mousehop`
//! invocation used to build its own preferences window and spawn its
//! own daemon child. This mirrors the daemon's socket trick on a
//! separate `mousehop-gui.sock` so a second GUI launch detects the
//! first; the extra twist on top is that the listener actually
//! accepts connections, and every accepted connection is treated as
//! a "raise the prefs window" ping. The second launch connects (then
//! drops the stream immediately) to fire one ping at the existing
//! instance before exiting — so launching mousehop from a desktop
//! launcher, a tray click, or a second terminal all do the same
//! thing as right-clicking the tray.
//!
//! GApplication's D-Bus uniqueness *also* covers this on Linux, but
//! is unreliable on macOS (no session bus) and our manual lock has
//! to exist anyway to avoid daemon-child duplication, so the
//! activation signal piggybacks on the same socket.

use std::io::ErrorKind;
use std::sync::OnceLock;

#[cfg(unix)]
use std::{
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
};

#[cfg(windows)]
use std::net::{TcpListener, TcpStream};

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
/// [`AcquireError::AlreadyRunning`] *and* fires a present-window
/// ping on this side. Dropping the guard releases the lock and
/// tears down the listener thread on the next accept.
pub(crate) struct SingleInstanceGuard {
    #[cfg(unix)]
    socket_path: PathBuf,
    // Listener is moved into the listener thread once
    // `start_present_listener` runs, so it's not stored here.
}

/// Callback the listener thread invokes per accepted "present" ping.
/// Stored in a static so the spawned thread doesn't need to capture
/// any GTK-bound types (which would force a Send bound the GTK
/// world can't satisfy).
type PresentCallback = Box<dyn Fn() + Send + Sync + 'static>;
static PRESENT_CALLBACK: OnceLock<PresentCallback> = OnceLock::new();

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
            Ok(stream) => {
                // The connection itself is the "present" signal —
                // dropping the stream immediately is fine; the
                // existing instance's accept loop sees one more
                // ready connection and fires its callback.
                drop(stream);
                return Err(AcquireError::AlreadyRunning);
            }
            Err(e) => {
                log::debug!("{socket_path:?}: {e} — removing stale GUI lock socket");
                let _ = std::fs::remove_file(&socket_path);
            }
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        // Another GUI bound it in the gap between our check and bind.
        // Race-window twin of the connect path above: signal them
        // before bowing out so the user still sees a window.
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if let Ok(stream) = UnixStream::connect(&socket_path) {
                drop(stream);
            }
            return Err(AcquireError::AlreadyRunning);
        }
        Err(e) => return Err(AcquireError::Bind(e)),
    };

    spawn_listener_thread(listener);

    Ok(SingleInstanceGuard { socket_path })
}

#[cfg(windows)]
pub(crate) fn acquire() -> Result<SingleInstanceGuard, AcquireError> {
    match TcpListener::bind(("127.0.0.1", GUI_LOCK_PORT)) {
        Ok(listener) => {
            spawn_listener_thread(listener);
            Ok(SingleInstanceGuard {})
        }
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            // Mirror the Unix path: ping the existing instance so it
            // raises its window before we exit.
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", GUI_LOCK_PORT)) {
                drop(stream);
            }
            Err(AcquireError::AlreadyRunning)
        }
        Err(e) => Err(AcquireError::Bind(e)),
    }
}

/// Install the closure that fires whenever a sibling launch connects
/// to our lock socket. Must be called from the GTK main thread
/// before `acquire()` (the listener thread cannot interact with GTK
/// directly — callbacks should marshal back via
/// `glib::idle_add_once`). Safe to call multiple times; only the
/// first call takes effect.
pub(crate) fn set_present_callback(cb: impl Fn() + Send + Sync + 'static) {
    let _ = PRESENT_CALLBACK.set(Box::new(cb));
}

#[cfg(unix)]
fn spawn_listener_thread(listener: UnixListener) {
    std::thread::Builder::new()
        .name("mousehop-gui-present".into())
        .spawn(move || {
            for incoming in listener.incoming() {
                match incoming {
                    Ok(stream) => {
                        drop(stream);
                        fire_present();
                    }
                    Err(e) => {
                        log::warn!("gui-present listener accept failed: {e}");
                        break;
                    }
                }
            }
        })
        .expect("spawn gui-present listener thread");
}

#[cfg(windows)]
fn spawn_listener_thread(listener: TcpListener) {
    std::thread::Builder::new()
        .name("mousehop-gui-present".into())
        .spawn(move || {
            for incoming in listener.incoming() {
                match incoming {
                    Ok(stream) => {
                        drop(stream);
                        fire_present();
                    }
                    Err(e) => {
                        log::warn!("gui-present listener accept failed: {e}");
                        break;
                    }
                }
            }
        })
        .expect("spawn gui-present listener thread");
}

fn fire_present() {
    if let Some(cb) = PRESENT_CALLBACK.get() {
        cb();
    } else {
        log::debug!("gui-present ping received before callback installed; ignoring");
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
