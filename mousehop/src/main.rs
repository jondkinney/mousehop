use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use mousehop::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use mousehop_cli::CliError;
#[cfg(feature = "gtk")]
use mousehop_gtk::GtkError;
use mousehop_ipc::{IpcError, IpcListenerCreationError};
use std::{
    future::Future,
    io,
    process::{self, Child},
};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum MousehopError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    // init logging
    let env = Env::default().filter_or("MOUSEHOP_LOG_LEVEL", "info");
    env_logger::init_from_env(env);

    // Route panics to a durable logfile before anything else can run.
    // The daemon child's stderr is /dev/null under LaunchServices, so
    // without this a panic (which `panic = "abort"` turns into an
    // immediate process death) leaves no trace anywhere the user can
    // see — which is exactly why the screensaver-lockup crash went
    // undiagnosed. Tag each record by role so a shared log file stays
    // attributable.
    let role = if std::env::args().skip(1).any(|a| a == "daemon") {
        "daemon"
    } else {
        "gui"
    };
    mousehop::panic_log::install(role);

    // On a Linux `cargo install` (no AUR / Flatpak / distro package)
    // the binary alone wouldn't appear in launchers. This silently
    // writes the .desktop entry + icon into ~/.local/share on the
    // first launch, once. Best-effort — never blocks startup.
    #[cfg(all(unix, not(target_os = "macos")))]
    mousehop::desktop_install::ensure_first_launch();

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn run() -> Result<(), MousehopError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(mousehop_cli::run(cli_args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(MousehopError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
            Command::Firewall(args) => {
                let code = mousehop::firewall::run(config.port(), args.remove, args.dry_run);
                process::exit(code);
            }
            #[cfg(target_os = "macos")]
            Command::AxProbe => {
                // Fresh-process probe of TCC Accessibility state. Spawned
                // by the daemon's TCC.db watcher (see mousehop::tcc_watch
                // on macOS) to bypass cached-trust state in already-running
                // processes — particularly important for the "remove from
                // list" case where AXIsProcessTrusted in the parent keeps
                // reporting cached-true. Exit 0 = granted, 1 = revoked.
                let granted = mousehop::macos_tcc_probe::is_accessibility_granted();
                process::exit(if granted { 0 } else { 1 });
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(all(feature = "gtk", unix))]
            {
                run_gui_with_daemon_supervision()?;
            }
            #[cfg(all(feature = "gtk", not(unix)))]
            {
                // Non-unix has no signal-based bounded teardown and no
                // daemon supervisor (the screensaver-panic crash this
                // guards against is macOS-specific). Keep the original
                // spawn → run → kill lifecycle.
                let mut service = start_service()?;
                let res = mousehop_gtk::run(gui_build_info());
                let _ = service.kill();
                let _ = service.wait();
                res?;
            }
            #[cfg(not(feature = "gtk"))]
            {
                // run daemon if gtk is diabled
                match run_async(run_service(config)) {
                    Err(MousehopError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), MousehopError>
where
    F: Future<Output = Result<(), E>>,
    MousehopError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

fn start_service() -> Result<Child, io::Error> {
    let child = process::Command::new(std::env::current_exe()?)
        .args(std::env::args().skip(1))
        .arg("daemon")
        .spawn()?;
    Ok(child)
}

/// Build-time facts handed to the GTK frontend's About section.
#[cfg(feature = "gtk")]
fn gui_build_info() -> mousehop_gtk::BuildInfo {
    mousehop_gtk::BuildInfo {
        local_commit: config::local_commit(),
        version: config::build::PKG_VERSION,
        build_time: config::build::BUILD_TIME,
        rust_version: config::build::RUST_VERSION,
        source_url: "https://github.com/jondkinney/mousehop",
    }
}

/// Run the GTK frontend with a supervised daemon child.
///
/// The daemon does the actual capture/emulation/networking; the GUI is
/// a menu-bar control panel in the parent process. If the daemon dies
/// *abnormally* (e.g. a panic on a screensaver / sleep edge) while the
/// GUI is up, a watcher thread respawns it so input sharing self-heals
/// instead of going dead until the user relaunches the app — the GUI
/// reconnects to the fresh daemon (see `mousehop_gtk::build_ui`). A
/// clean `exit(0)` (an intentional shutdown such as an Accessibility
/// revoke) is left alone.
///
/// The supervisor polls `try_wait` rather than blocking on `wait` so
/// the quit path can take the lock to terminate the current child.
#[cfg(all(unix, feature = "gtk"))]
fn run_gui_with_daemon_supervision() -> Result<(), MousehopError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let shutting_down = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(start_service()?));

    let supervisor = {
        let shutting_down = shutting_down.clone();
        let child = child.clone();
        std::thread::Builder::new()
            .name("mousehop-daemon-supervisor".into())
            .spawn(move || {
                const POLL: Duration = Duration::from_millis(200);
                const MIN_BACKOFF: Duration = Duration::from_millis(500);
                const MAX_BACKOFF: Duration = Duration::from_secs(30);
                let mut backoff = MIN_BACKOFF;
                let mut spawned_at = Instant::now();
                loop {
                    let exited = {
                        let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
                        guard.try_wait()
                    };
                    if shutting_down.load(Ordering::SeqCst) {
                        break;
                    }
                    match exited {
                        Ok(None) => std::thread::sleep(POLL),
                        Ok(Some(status)) if status.success() => {
                            log::info!(
                                "daemon child exited cleanly; not restarting (intentional shutdown)"
                            );
                            break;
                        }
                        Ok(Some(status)) => {
                            // Reset the backoff if the daemon had been
                            // healthy for a while; otherwise grow it so a
                            // daemon that crashes on every launch can't
                            // hot-loop.
                            if spawned_at.elapsed() >= Duration::from_secs(10) {
                                backoff = MIN_BACKOFF;
                            }
                            log::warn!(
                                "daemon child exited abnormally ({status}); restarting in {backoff:?}"
                            );
                            std::thread::sleep(backoff);
                            if shutting_down.load(Ordering::SeqCst) {
                                break;
                            }
                            match start_service() {
                                Ok(c) => {
                                    *child.lock().unwrap_or_else(|e| e.into_inner()) = c;
                                    spawned_at = Instant::now();
                                    backoff = (backoff * 2).min(MAX_BACKOFF);
                                }
                                Err(e) => {
                                    log::error!("failed to restart daemon child: {e}");
                                    backoff = (backoff * 2).min(MAX_BACKOFF);
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("daemon supervisor: try_wait failed: {e}");
                            std::thread::sleep(POLL);
                        }
                    }
                }
            })
            .ok()
    };

    let res = mousehop_gtk::run(gui_build_info());

    // GUI exited (user quit). Stop the supervisor and tear down the
    // current daemon child with the same bounded SIGINT → SIGKILL
    // cleanup as before. The lock is released before joining the
    // supervisor so it can observe `shutting_down` and exit.
    shutting_down.store(true, Ordering::SeqCst);
    {
        let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
        let pid = guard.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match guard.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    log::warn!("daemon child did not exit on SIGINT in 3s — sending SIGKILL");
                    let _ = guard.kill();
                    let _ = guard.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    log::error!("waiting for daemon child: {e}");
                    break;
                }
            }
        }
    }
    if let Some(h) = supervisor {
        let _ = h.join();
    }

    res?;
    Ok(())
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");

    // macOS-only: detect AX-permission "remove from list" by polling
    // TCC.db's mtime and confirming via a fresh subprocess. The
    // existing in-process AXIsProcessTrusted polling in the GUI only
    // catches the toggle-off case; the remove case leaves the cached
    // trust state stuck at true forever. See `macos_tcc_watch`.
    //
    // `MOUSEHOP_DISABLE_TCC_WATCH` opts out: an unsigned dev/headless
    // build has no Accessibility grant, so the watcher would exit the
    // daemon immediately. Setting this keeps it alive for advertising-
    // /network-only runs (input capture/emulation still no-op without
    // the grant).
    #[cfg(target_os = "macos")]
    if std::env::var_os("MOUSEHOP_DISABLE_TCC_WATCH").is_none() {
        mousehop::macos_tcc_watch::spawn();
    }

    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
