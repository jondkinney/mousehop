mod authorization_window;
mod client_object;
mod client_row;
mod clipboard_privacy_window;
mod fingerprint_window;
mod glyph_font;
mod key_object;
mod key_row;
#[cfg(target_os = "linux")]
mod linux_tray;
#[cfg(target_os = "macos")]
mod macos_privacy;
#[cfg(target_os = "macos")]
mod macos_status_item;
mod modal_keys;
mod release_shortcut;
mod running_app_object;
mod single_instance;
mod window;

use std::{env, process, str, sync::OnceLock};

use window::Window;

/// Local build's commit hash, set once by [`run`] before the GTK
/// main loop starts. Read by per-row UI to compare against each
/// peer's [`mousehop_ipc::ClientState::peer_commit`] for the
/// soft-warn version-mismatch indicator.
pub(crate) static LOCAL_COMMIT: OnceLock<[u8; 8]> = OnceLock::new();

/// Convenience: returns the local commit as an 8-char ASCII string,
/// or a placeholder if unset (which would indicate a programmer
/// error since [`run`] always sets it).
pub(crate) fn local_commit_str() -> String {
    LOCAL_COMMIT
        .get()
        .and_then(|c| std::str::from_utf8(c).ok())
        .unwrap_or("????????")
        .to_string()
}

/// Build-time facts surfaced in the preferences window's About
/// section. The main binary is the one that pulls in `shadow-rs`,
/// so it constructs this and hands it off to [`run`] — that way
/// the GTK crate doesn't need its own `build.rs` or a parallel
/// `shadow!` macro invocation.
pub struct BuildInfo {
    /// 8-byte ASCII short commit; used both for the peer-version
    /// mismatch indicator and the About section.
    pub local_commit: [u8; 8],
    /// Workspace package version, e.g. `"0.11.7"`.
    pub version: &'static str,
    /// Human-readable build timestamp, e.g. `"2026-05-22 15:33:42 +00:00"`.
    pub build_time: &'static str,
    /// Compiler banner, e.g. `"rustc 1.95.0 (59807616e 2026-04-14)"`.
    pub rust_version: &'static str,
    /// Upstream source URL, opened when the About → Source row is
    /// activated.
    pub source_url: &'static str,
}

pub(crate) static BUILD_INFO: OnceLock<BuildInfo> = OnceLock::new();

/// Accessor for the About section. Returns `None` only during the
/// brief window between process start and [`run`] storing the info.
pub(crate) fn build_info() -> Option<&'static BuildInfo> {
    BUILD_INFO.get()
}

use mousehop_ipc::FrontendEvent;

use adw::Application;
use gtk::{IconTheme, gdk::Display, glib::clone, prelude::*};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;
use self::key_object::KeyObject;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GtkError {
    #[error("gtk frontend exited with non zero exit code: {0}")]
    NonZeroExitCode(i32),
}

/// Arm a process-level force-exit backstop and request a normal app
/// quit. macOS-only because shutdown wedges (GTK main loop pumping
/// pending events while the daemon is also being torn down, a
/// CGEventTap that hasn't been removed yet, an outgoing IPC write
/// blocked on a closing socket) only show up on macOS in practice.
///
/// The backstop thread sleeps outside the GTK main loop so it fires
/// even if the loop itself is the thing that's wedged. If normal
/// cleanup completes first (the usual case) the process exits via
/// `main` returning and the sleeping backstop thread is killed by
/// the OS along with everything else — the timer never fires.
///
/// Calling this twice in quick succession is a no-op on the second
/// call so we don't spawn multiple backstops.
#[cfg(target_os = "macos")]
pub(crate) fn request_quit_with_backstop(app: &adw::Application) {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    static ARMED: OnceLock<AtomicBool> = OnceLock::new();
    let armed = ARMED.get_or_init(|| AtomicBool::new(false));
    if armed.swap(true, Ordering::SeqCst) {
        app.quit();
        return;
    }

    std::thread::Builder::new()
        .name("mousehop-quit-backstop".into())
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
            log::warn!("quit cleanup did not complete in 5s — force-exiting");
            std::process::exit(0);
        })
        .ok();
    app.quit();
}

pub fn run(info: BuildInfo) -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    LOCAL_COMMIT
        .set(info.local_commit)
        .expect("local_commit set once");
    BUILD_INFO.set(info).ok().expect("BUILD_INFO set once");

    // Refuse to open a second preferences window. Held until `run`
    // returns — i.e. for the whole GUI lifetime, including the joined
    // GTK thread on Windows.
    let _instance_guard = match single_instance::acquire() {
        Ok(guard) => Some(guard),
        Err(single_instance::AcquireError::AlreadyRunning) => {
            log::info!("a mousehop preferences window is already open — exiting");
            return Ok(());
        }
        // A lock we couldn't even create shouldn't strand the user
        // without a GUI — log it and run anyway.
        Err(e) => {
            log::warn!("GUI single-instance lock unavailable ({e}); continuing without it");
            None
        }
    };

    #[cfg(windows)]
    let ret = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // https://gitlab.gnome.org/GNOME/gtk/-/commit/52dbb3f372b2c3ea339e879689c1de535ba2c2c3 -> caused crash on windows
        .name("gtk".into())
        .spawn(gtk_main)
        .unwrap()
        .join()
        .unwrap();
    #[cfg(not(windows))]
    let ret = gtk_main();

    match ret {
        glib::ExitCode::SUCCESS => Ok(()),
        e => Err(GtkError::NonZeroExitCode(e.value())),
    }
}

fn gtk_main() -> glib::ExitCode {
    #[cfg(target_os = "macos")]
    {
        configure_macos_bundle_environment();
        install_macos_gtk_log_filter();
    }

    gio::resources_register_include!("mousehop.gresource").expect("Failed to register resources.");

    let app = Application::builder()
        .application_id("com.mousehop.Mousehop")
        .build();

    // Cross-thread bridge for "raise the existing window" pings from
    // sibling `mousehop` launches. The listener thread (installed by
    // `single_instance::acquire()` in `run()`) is not on the GTK
    // main thread and can't touch widgets, so it just nudges this
    // channel; the async task below runs in the main loop and
    // actually presents the window.
    let (present_tx, present_rx) = async_channel::bounded::<()>(8);
    single_instance::set_present_callback(move || {
        // try_send is fine — coalescing pings to "at least one" is
        // exactly the right semantics for "raise the window".
        let _ = present_tx.try_send(());
    });
    let app_for_present = app.clone();
    glib::spawn_future_local(async move {
        while present_rx.recv().await.is_ok() {
            log::info!("sibling launch detected — raising prefs window");
            if let Some(window) = app_for_present.windows().into_iter().next() {
                window.present();
            } else {
                // No window yet — `activate` will build one. Trigger
                // it so a sibling launch during startup also wakes
                // the UI up rather than racing past window
                // creation.
                app_for_present.activate();
            }
        }
    });

    app.connect_startup(|app| {
        load_icons();
        configure_text_rendering();
        // Register the bundled chord-chip faces with fontconfig before
        // any window — and so any text layout — is built, so Pango's
        // font map resolves the "omarchy"/"Adwaita Sans" families on its
        // first use.
        glyph_font::install();
        setup_actions(app);
        setup_menu(app);
    });
    // A second `activate` — GApplication's D-Bus uniqueness handing
    // off a relaunch on Linux, or any future re-activation — must
    // raise the existing window rather than build a second one: each
    // `build_ui` opens its own IPC connection and preferences window.
    app.connect_activate(|app| {
        if let Some(window) = app.windows().into_iter().next() {
            window.present();
            return;
        }
        build_ui(app);
    });

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

#[cfg(target_os = "macos")]
fn install_macos_gtk_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        if level == glib::LogLevel::Warning && is_gtk_theme_parser_warning(fields) {
            return glib::LogWriterOutput::Handled;
        }

        glib::log_writer_default(level, fields)
    });
}

#[cfg(target_os = "macos")]
fn is_gtk_theme_parser_warning(fields: &[glib::LogField<'_>]) -> bool {
    let mut domain = None;
    let mut message = None;

    for field in fields {
        match field.key() {
            "GLIB_DOMAIN" => domain = field.value_str(),
            "MESSAGE" => message = field.value_str(),
            _ => {}
        }
    }

    domain == Some("Gtk")
        && message.is_some_and(|message| message.starts_with("Theme parser warning: gtk.css:"))
}

#[cfg(target_os = "macos")]
fn configure_macos_bundle_environment() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(contents) = exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(std::path::Path::to_owned)
    else {
        return;
    };

    let share = contents.join("Resources").join("share");
    if !share.exists() {
        return;
    }

    let schemas = share.join("glib-2.0").join("schemas");
    if schemas.exists() {
        env::set_var("GSETTINGS_SCHEMA_DIR", schemas);
    }

    env::set_var("XDG_DATA_DIRS", &share);
    env::set_var(
        "GTK_DATA_PREFIX",
        contents.join("Resources").to_string_lossy().as_ref(),
    );
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/com/mousehop/Mousehop/icons");
}

/// Pin font rasterization to the egui/hyprcorrect-equivalent
/// rendering pipeline so the release-shortcut chip — and incidentally
/// the rest of the prefs UI — matches hyprcorrect's crispness.
///
/// GTK's defaults (`gtk-xft-hintstyle=slight` + subpixel positioning
/// with `rgba=rgb` on most LCDs) hint glyph outlines to whole-pixel
/// boundaries and use color-fringed subpixel rasterization. Egui
/// skips both: it rasterizes glyphs straight from the bezier
/// outlines, no hinting, grayscale AA. Match that here so the
/// chord-chip's Omarchy logo + modifier glyphs come out edge-clean
/// instead of fuzzy + tinted.
///
/// Scope is app-wide because GTK only exposes these as
/// `GtkSettings` properties — there's no per-widget knob. The
/// downside is mild: body text loses fontconfig's slight pixel-snap
/// optimization. On the 2x-scale Omarchy displays this app actually
/// runs on, snapping is mostly a no-op anyway since 1pt already
/// rasterizes to 2 physical pixels.
fn configure_text_rendering() {
    let Some(settings) = gtk::Settings::default() else {
        return;
    };
    settings.set_gtk_xft_hintstyle(Some("hintnone"));
    settings.set_gtk_xft_rgba(Some("none"));
    settings.set_gtk_xft_antialias(1);
}

// Add application actions
fn setup_actions(app: &adw::Application) {
    // Quit action
    // This is important on macOS, where users expect a File->Quit action with a Cmd+Q shortcut.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            #[cfg(target_os = "macos")]
            request_quit_with_backstop(&app);
            #[cfg(not(target_os = "macos"))]
            app.quit();
        }
    });
    app.add_action(&quit_action);

    // Cmd+W → close the front window. GtkWindow's built-in `close`
    // action fires `close-request`, which on macOS we've hooked to
    // hide the window instead of destroying it (see `build_ui`). The
    // window's connect_hide handler then flips the activation policy
    // to Accessory — net effect is that Cmd+W collapses the GUI to a
    // menu-bar-only background helper, freeing focus for whatever the
    // user does next. Wired only on macOS so we don't override the
    // native Ctrl+W behavior on Linux/Windows.
    #[cfg(target_os = "macos")]
    app.set_accels_for_action("window.close", &["<Meta>w"]);

    // Super+W → hide the window on Linux. The Linux close-request
    // handler in `build_ui` intercepts the resulting close action and
    // hides the window rather than destroying it; the tray's hold
    // guard keeps the app alive in the background.
    #[cfg(target_os = "linux")]
    app.set_accels_for_action("window.close", &["<Super>w"]);
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &adw::Application) {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some("Close Window"), Some("window.close"));
    file_menu.append(Some("Quit"), Some("app.quit"));
    menu.append_submenu(Some("_File"), &file_menu);

    app.set_menubar(Some(&menu))
}

/// A daemon-IPC pump message: a decoded event, or a signal that the
/// reader ended (daemon died / socket closed) so the dispatch loop can
/// try to reconnect to a supervisor-restarted daemon.
enum IpcMsg {
    // Boxed: `FrontendEvent` is large while `Disconnected` carries no
    // data, so an unboxed variant bloats every channel slot (and trips
    // `clippy::large_enum_variant`). These are infrequent UI events, so
    // the per-event allocation is negligible.
    Event(Box<FrontendEvent>),
    Disconnected,
}

/// Pump daemon → frontend events off the blocking IPC reader into the
/// async channel. On any read error or clean EOF it emits
/// `IpcMsg::Disconnected` so the dispatch loop can reconnect. Spawned
/// fresh on every (re)connection.
fn spawn_ipc_reader(
    mut reader: mousehop_ipc::FrontendEventReader,
    sender: async_channel::Sender<IpcMsg>,
) {
    gio::spawn_blocking(move || {
        while let Some(e) = reader.next_event() {
            match e {
                Ok(e) => {
                    if sender.send_blocking(IpcMsg::Event(Box::new(e))).is_err() {
                        return; // dispatch loop gone; stop pumping
                    }
                }
                Err(e) => {
                    log::warn!("daemon IPC read error: {e}");
                    break;
                }
            }
        }
        let _ = sender.send_blocking(IpcMsg::Disconnected);
    });
}

/// Reconnect to a daemon the supervisor is expected to have restarted.
/// Polls the IPC socket for a bounded window; on success it swaps the
/// window's request writer, restarts the reader pump, and returns
/// `true`. Returns `false` if the daemon never came back (an
/// intentional shutdown), so the caller exits the GUI as before.
async fn reconnect_ipc(window: &Window, sender: &async_channel::Sender<IpcMsg>) -> bool {
    const ATTEMPTS: u32 = 10; // ~10s, polled once per second
    log::warn!("daemon IPC dropped — trying to reconnect to a restarted daemon");
    for _ in 0..ATTEMPTS {
        glib::timeout_future_seconds(1).await;
        if let Ok((reader, writer)) = mousehop_ipc::connect() {
            window.rebind_frontend(writer);
            spawn_ipc_reader(reader, sender.clone());
            log::info!("reconnected to daemon");
            return true;
        }
    }
    false
}

fn build_ui(app: &Application) {
    log::debug!("connecting to mousehop-socket");
    let (reader, writer) = match mousehop_ipc::connect() {
        Ok(conn) => conn,
        Err(e) => {
            log::error!("{e}");
            process::exit(1);
        }
    };
    log::debug!("connected to mousehop-socket");

    // The reader pump feeds daemon events into this channel; the
    // dispatch loop below holds the other end plus a `sender` clone, so
    // the channel survives across daemon restarts. The pump emits
    // `IpcMsg::Disconnected` when the daemon goes away, which drives the
    // reconnect path (the daemon supervisor in the main binary restarts
    // a crashed daemon).
    let (sender, receiver) = async_channel::bounded::<IpcMsg>(10);
    spawn_ipc_reader(reader, sender.clone());

    let window = Window::new(app, writer);
    #[cfg(target_os = "linux")]
    {
        // Hide-on-close: the X button, GTK's `window.close` action
        // (bound to Super+W above), and any WM-level close request
        // (Hyprland's Super+W if mapped, an `hyprctl dispatch
        // killactive`, etc.) all funnel through `close-request`.
        // Returning `Stop` keeps the GtkWindow alive so the tray's
        // "Open Mousehop" can re-present it without rebuilding state.
        window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
        // Stash the hold guard for the lifetime of the process so the
        // GtkApplication does not exit when the last visible window
        // closes — the tray needs to outlive the GUI.
        thread_local! {
            static TRAY_HOLD: std::cell::OnceCell<gio::ApplicationHoldGuard> =
                const { std::cell::OnceCell::new() };
        }
        let hold = linux_tray::setup(app, &window);
        TRAY_HOLD.with(|cell| {
            let _ = cell.set(hold);
        });
    }
    #[cfg(target_os = "macos")]
    {
        window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
        // Toggle the Dock icon based on the window's visibility:
        // Regular while the window is open (Dock icon shown so the
        // user has a familiar way back to the app), Accessory while
        // the window is hidden and only the menu-bar item is around
        // (matches the LSUIElement-style background-helper feel).
        window.connect_show(|_| {
            macos_status_item::set_activation_policy(macos_status_item::ACTIVATION_POLICY_REGULAR);
        });
        window.connect_hide(|_| {
            macos_status_item::set_activation_policy(
                macos_status_item::ACTIVATION_POLICY_ACCESSORY,
            );
        });
        macos_status_item::setup(app, &window);
        // First-launch TCC prompts. No-op when already granted.
        macos_privacy::fire_initial_prompts();
        // Watch the Accessibility grant continuously for the lifetime
        // of the process. On a grant, swap the warning row into its
        // "relaunch required" state (the daemon subprocess already
        // bailed and can't recover without a restart) and present the
        // modal relaunch prompt — this transition is the one
        // unambiguous "relaunch needed" moment. On a REVOKE, quit
        // immediately — an active CGEventTap at HeadInsertEventTap can
        // wedge system input if the process lingers after losing AX,
        // and forcing the process to exit is the only bulletproof way
        // to guarantee the kernel tears the tap down.
        let window_weak = window.downgrade();
        let app_weak = app.downgrade();
        macos_privacy::watch_accessibility_state(move |change| match change {
            macos_privacy::AccessibilityChange::Granted => {
                if let Some(window) = window_weak.upgrade() {
                    window.present();
                    window.refresh_capture_emulation_status();
                    window.show_relaunch_required_dialog();
                }
            }
            macos_privacy::AccessibilityChange::Revoked => {
                log::warn!("Accessibility revoked — quitting to avoid wedging system input");
                if let Some(app) = app_weak.upgrade() {
                    request_quit_with_backstop(&app);
                }
            }
        });
    }

    glib::spawn_future_local(clone!(
        #[weak]
        window,
        async move {
            // Hold `sender` so the channel stays open across daemon
            // restarts (the reader pump's clone is dropped when it ends;
            // `IpcMsg::Disconnected` is what drives the reconnect).
            let pump_sender = sender;
            loop {
                let notify = match receiver.recv().await {
                    Ok(IpcMsg::Event(notify)) => *notify,
                    Ok(IpcMsg::Disconnected) => {
                        if reconnect_ipc(&window, &pump_sender).await {
                            continue;
                        }
                        log::error!("daemon did not return after restart window — exiting GUI");
                        process::exit(1);
                    }
                    // Channel fully closed (no senders left) — nothing
                    // can drive the UI anymore.
                    Err(_) => process::exit(1),
                };
                match notify {
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state)
                    }
                    FrontendEvent::Deleted(client) => window.delete_client(client),
                    FrontendEvent::State(handle, config, state) => {
                        window.update_client_config(handle, config);
                        window.update_client_state(handle, state);
                    }
                    FrontendEvent::NoSuchClient(_) => {}
                    FrontendEvent::Error(e) => window.show_toast(e.as_str()),
                    FrontendEvent::Enumerate(clients) => window.update_client_list(clients),
                    FrontendEvent::PortChanged(port, msg) => window.update_port(port, msg),
                    FrontendEvent::CaptureStatus(s) => window.set_capture(s.into()),
                    FrontendEvent::EmulationStatus(s) => window.set_emulation(s.into()),
                    FrontendEvent::AuthorizedUpdated(keys) => window.set_authorized_keys(keys),
                    FrontendEvent::PublicKeyFingerprint(fp) => window.set_pk_fp(&fp),
                    FrontendEvent::ConnectionAttempt { fingerprint } => {
                        window.request_authorization(&fingerprint);
                    }
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        window.show_toast(format!("device connected: {addr}").as_str());
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint: _,
                        addr,
                        pos,
                    } => {
                        window.show_toast(format!("device entered: {addr} ({pos})").as_str());
                    }
                    FrontendEvent::IncomingDisconnected(addr) => {
                        window.show_toast(format!("{addr} disconnected").as_str());
                    }
                    FrontendEvent::ReleaseThreshold(threshold) => {
                        window.set_release_threshold(threshold);
                    }
                    FrontendEvent::ReleaseBind(chord) => {
                        window.set_release_bind(chord);
                    }
                    FrontendEvent::MdnsDiscovery(enabled) => {
                        window.set_mdns_discovery(enabled);
                    }
                    FrontendEvent::SuppressedAppsUpdated(apps) => {
                        window.set_suppressed_apps(apps);
                    }
                    FrontendEvent::RunningApps(apps) => {
                        window.set_running_apps(apps);
                    }
                }
            }
        }
    ));

    #[cfg(not(target_os = "macos"))]
    window.present();

    // On macOS, default to presenting the main window on every launch
    // so the user gets a visible confirmation that the app is running
    // — including the post-grant relaunch and normal Dock/Finder/`open`
    // launches. Opt out by setting `MOUSEHOP_HIDDEN=1` in the
    // environment (useful for a LaunchAgent / login-item configuration
    // where the user wants the app to come up quietly into the menu
    // bar only, with no window on boot).
    #[cfg(target_os = "macos")]
    if env::var_os("MOUSEHOP_HIDDEN").is_none() {
        window.present();
    }
}
