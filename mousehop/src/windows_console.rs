//! Hand a GUI-subsystem build its console back — but only when the
//! invocation has something to say.
//!
//! `main.rs` builds the Windows binary with `windows_subsystem =
//! "windows"`, so double-clicking `mousehop.exe` brings up the
//! preferences window on its own instead of alongside a console window
//! that has to stay open for the app to keep running. The price of that
//! flag is that the process starts with no standard handles at all, so
//! the command-line surface — `mousehop cli …`, `mousehop --help`,
//! `mousehop daemon` — would print into the void.
//!
//! [`attach_parent`] buys it back: it attaches to the console of
//! whatever launched us and points the process's std handles at it, so
//! a command typed at a `cmd` or PowerShell prompt behaves the way the
//! old console build did.
//!
//! It deliberately does *not* attach for a bare `mousehop.exe` launch.
//! A process attached to a console is killed when that console window
//! closes (`CTRL_CLOSE_EVENT`), so attaching the GUI would put us
//! straight back at "you have to leave the terminal open" for anyone
//! who starts Mousehop by typing its name. `MOUSEHOP_LOG_LEVEL` opts a
//! GUI launch into console logging anyway, for support and debugging.
//!
//! One wart is inherent to the subsystem flag and not worth working
//! around: `cmd.exe` does not wait on a GUI-subsystem process, so the
//! prompt comes back immediately and our output interleaves with it.
//! Every GUI-subsystem tool that also has a CLI behaves this way.

use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};
use windows::core::{PCWSTR, w};

/// Wire stdout/stderr/stdin up to the launching terminal's console,
/// when this invocation is one that wants a console at all.
///
/// Must run before `env_logger` initialises — a logger built while
/// stderr has no handle logs nowhere.
pub fn attach_parent() {
    if !wants_console() {
        return;
    }
    unsafe {
        // Failure is routine and handled by `redirect` below: there is
        // no parent console (launched from Explorer or a shortcut), or
        // we are already attached to one (the daemon child of a
        // console-attached GUI inherits its parent's console at spawn,
        // regardless of subsystem).
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        redirect(STD_OUTPUT_HANDLE, w!("CONOUT$"));
        redirect(STD_ERROR_HANDLE, w!("CONOUT$"));
        redirect(STD_INPUT_HANDLE, w!("CONIN$"));
    }
}

/// Whether this invocation should be able to write to a terminal.
///
/// Any argument at all means the user typed a command — a subcommand,
/// `--help`, or a flag clap is about to reject with a message worth
/// reading. A bare launch is the GUI, which stays detached so it
/// outlives the terminal it may have been started from.
fn wants_console() -> bool {
    std::env::args_os().nth(1).is_some() || std::env::var_os("MOUSEHOP_LOG_LEVEL").is_some()
}

/// Point one standard handle at the console device, unless it already
/// points somewhere.
///
/// A handle that is already valid means the shell redirected this
/// stream (`mousehop cli list > out.txt`) or we inherited a live
/// console at spawn — either way the user already chose where it goes,
/// so leave it alone.
unsafe fn redirect(which: STD_HANDLE, device: PCWSTR) {
    if unsafe { GetStdHandle(which) }.is_ok() {
        return;
    }
    let console = unsafe {
        CreateFileW(
            device,
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    // Opening CONOUT$/CONIN$ fails when no console is attached at all,
    // which is the expected outcome of a launch with no parent console.
    let Ok(console) = console else {
        return;
    };
    let _ = unsafe { SetStdHandle(which, console) };
}
