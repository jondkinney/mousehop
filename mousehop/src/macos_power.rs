//! macOS power integration for wake observation and outgoing capture.
//!
//! [`UserActivity`] keeps the local host out of its idle screen saver
//! while its input is captured for a peer. The rest of this module is
//! the daemon listen-side system-power observer:
//!
//! DTLS rides UDP; a peer connection that was alive before the host
//! slept has no FIN to deliver on wake, and the `read_loop`'s `recv`
//! sits on a socket whose peer may have already torn down and
//! reconnected on a fresh 4-tuple. `RECV_IDLE_TIMEOUT` in `listen.rs`
//! is the backstop, but waiting it out means several seconds of dead
//! input after every screensaver dismissal. This observer collapses
//! that to roughly the wake-event latency: on
//! `kIOMessageSystemHasPoweredOn`, signal the listener supervisor to
//! force-close every entry in `conns`. Each `read_loop`'s `recv` then
//! errors out, the existing exit path removes the slot, and a peer
//! reconnect lands on a clean accept.
//!
//! Mirrors the IOKit registration pattern in
//! `input-capture/src/macos.rs:810`, but spawned by the daemon —
//! capture lives elsewhere (and may not even be on this host).

use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use input_event::MACOS_KEEP_AWAKE_EVENT_TAG;
use libc::{c_char, c_int, c_void};
use std::thread::{self, JoinHandle};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// macOS keep-awake state for an outgoing capture session. The capture
/// event tap replaces real forwarded events with `kCGEventNull`, so
/// WindowServer never sees them as local input and would otherwise
/// start the screen saver while the user is controlling a peer.
///
/// `start` declares activity immediately; the capture loop calls
/// `pulse_if_active` periodically until `stop`. Each pulse combines a
/// power assertion (display wake/sleep) with a tagged, same-position
/// MouseMoved event (screen-saver idle). The capture tap recognizes
/// the shared tag and never forwards the synthetic event to the peer.
#[derive(Default)]
pub(crate) struct UserActivity {
    active: bool,
    assertion_id: u32,
}

impl UserActivity {
    pub(crate) fn start(&mut self) {
        self.active = true;
        self.pulse();
    }

    pub(crate) fn pulse_if_active(&mut self) {
        if self.active {
            self.pulse();
        }
    }

    pub(crate) fn stop(&mut self) {
        self.active = false;
        if self.assertion_id == 0 {
            return;
        }

        let id = std::mem::take(&mut self.assertion_id);
        let result = unsafe { IOPMAssertionRelease(id) };
        if result != K_IO_RETURN_SUCCESS {
            // User-activity assertions also expire on their own. If
            // powerd already retired this ID, there is nothing left
            // for us to release.
            log::debug!(
                "macos_power: user-activity assertion {id} was already inactive ({result:#x})"
            );
        }
    }

    fn pulse(&mut self) {
        self.declare_power_user_activity();

        match keep_awake_mouse_event() {
            Ok(event) => event.post(CGEventTapLocation::HID),
            Err(()) => log::warn!("macos_power: could not create keep-awake mouse event"),
        }
    }

    fn declare_power_user_activity(&mut self) {
        let reason = unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                c"Mousehop: controlling a peer".as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if reason.is_null() {
            log::warn!("macos_power: could not create user-activity description");
            return;
        }

        let result = unsafe {
            IOPMAssertionDeclareUserActivity(
                reason,
                K_IOPM_USER_ACTIVE_LOCAL,
                &mut self.assertion_id,
            )
        };
        unsafe { CFRelease(reason) };

        if result != K_IO_RETURN_SUCCESS {
            log::warn!("macos_power: could not declare user activity ({result:#x})");
            self.assertion_id = 0;
        }
    }
}

/// Build the mouse-jiggler half of a keep-awake pulse. The event's
/// position exactly matches the live cursor, so it resets HID and
/// screen-saver idle without a visible jump. Its private source and
/// shared user-data marker let the capture tap distinguish it from
/// physical input.
fn keep_awake_mouse_event() -> Result<CGEvent, ()> {
    let source = CGEventSource::new(CGEventSourceStateID::Private)?;
    let cursor = CGEvent::new(source.clone())?.location();
    let event =
        CGEvent::new_mouse_event(source, CGEventType::MouseMoved, cursor, CGMouseButton::Left)?;
    event.set_integer_value_field(
        EventField::EVENT_SOURCE_USER_DATA,
        MACOS_KEEP_AWAKE_EVENT_TAG,
    );
    Ok(event)
}

impl Drop for UserActivity {
    fn drop(&mut self) {
        self.stop();
    }
}

const K_IO_RETURN_SUCCESS: c_int = 0;
const K_IOPM_USER_ACTIVE_LOCAL: c_int = 0;
// `kCFStringEncodingUTF8` from CFString.h.
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// Owns the observer thread and the registration. Held by
/// `MousehopListener` for its lifetime; dropping it stops the
/// CFRunLoop, which lets the thread fall through to deregistration
/// and join. Construct via [`PowerObserver::spawn`].
pub(crate) struct PowerObserver {
    /// CFRunLoopRef of the observer thread, stored as `usize` so the
    /// struct is `Send` (the raw pointer is opaque). Zero when the
    /// thread failed to register and never sent us its run loop.
    run_loop: usize,
    thread: Option<JoinHandle<()>>,
}

impl PowerObserver {
    /// Spawn the observer thread. The thread reports its CFRunLoop
    /// back so `Drop` can stop it cleanly. Wake events arrive on
    /// `wake_tx`; an unbounded channel is used so the IOKit callback
    /// (which runs on the observer thread, not a tokio thread) never
    /// blocks on a busy receiver — a missed wake signal is recovered
    /// by the next one anyway.
    ///
    /// Async because the run-loop handshake is awaited via a
    /// `tokio::sync::oneshot` rather than a blocking std mpsc —
    /// the daemon's runtime is `current_thread`, so a blocking recv
    /// here would stall every other task on the same worker.
    pub(crate) async fn spawn(wake_tx: UnboundedSender<()>) -> Self {
        let (rl_tx, rl_rx) = oneshot::channel::<usize>();
        let thread = thread::Builder::new()
            .name("mousehop-power-observer".into())
            .spawn(move || run(wake_tx, rl_tx))
            .expect("spawn power observer thread");
        // Await the observer thread either reporting its run loop
        // (registration succeeded) or dropping `rl_tx` (registration
        // failed, thread exiting). Either way the wait is bounded by
        // the observer thread's startup, which is fast.
        let run_loop = rl_rx.await.unwrap_or(0);
        Self {
            run_loop,
            thread: Some(thread),
        }
    }
}

impl Drop for PowerObserver {
    fn drop(&mut self) {
        if self.run_loop != 0 {
            unsafe { CFRunLoopStop(self.run_loop as *mut c_void) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Refcon for the IOKit callback. Carries the wake-event sender so
/// `power_callback` can post on `kIOMessageSystemHasPoweredOn`, and
/// the root power port so it can ack sleep messages with
/// `IOAllowPowerChange`. Built on the observer thread, only ever
/// touched by `power_callback` on the same thread.
struct PowerCtx {
    wake_tx: UnboundedSender<()>,
    root_port: u32,
}

extern "C" fn power_callback(
    refcon: *mut c_void,
    _service: u32,
    msg_type: u32,
    msg_arg: *mut c_void,
) {
    const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
    const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
    const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;

    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is `Box::into_raw(Box::new(PowerCtx))` owned by
    // `run`; valid until the run loop returns and the box is reclaimed.
    // The callback only fires while CFRunLoopRun is active.
    let ctx = unsafe { &*(refcon as *const PowerCtx) };
    match msg_type {
        K_IO_MESSAGE_CAN_SYSTEM_SLEEP | K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            // Ack so the kernel doesn't stall on its 30s default
            // timeout. We have no objection to sleep.
            unsafe {
                IOAllowPowerChange(ctx.root_port, msg_arg as isize);
            }
        }
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
            log::info!("macos_power: system woke; signaling listener to drop peer conns");
            // Unbounded send is non-blocking; a closed receiver
            // (listener gone) means the signal is no longer needed.
            let _ = ctx.wake_tx.send(());
        }
        _ => {}
    }
}

fn run(wake_tx: UnboundedSender<()>, rl_tx: oneshot::Sender<usize>) {
    let ctx = Box::into_raw(Box::new(PowerCtx {
        wake_tx,
        root_port: 0,
    }));
    let mut notifier_object: u32 = 0;
    let mut notification_port: *mut c_void = std::ptr::null_mut();
    let root_port = unsafe {
        IORegisterForSystemPower(
            ctx as *mut c_void,
            &mut notification_port,
            power_callback,
            &mut notifier_object,
        )
    };
    if root_port == 0 || notification_port.is_null() {
        log::warn!("macos_power: IORegisterForSystemPower failed; observer inactive");
        unsafe {
            drop(Box::from_raw(ctx));
        }
        return;
    }
    // Stash the root port for the callback's IOAllowPowerChange ack.
    unsafe {
        (*ctx).root_port = root_port;
    }

    let src = unsafe { IONotificationPortGetRunLoopSource(notification_port) };
    if src.is_null() {
        log::warn!("macos_power: IONotificationPortGetRunLoopSource returned null");
        unsafe {
            IODeregisterForSystemPower(&mut notifier_object);
            IONotificationPortDestroy(notification_port);
            drop(Box::from_raw(ctx));
        }
        return;
    }
    let run_loop = unsafe { CFRunLoopGetCurrent() };
    unsafe {
        CFRunLoopAddSource(run_loop, src, kCFRunLoopCommonModes);
    }
    // Report the run loop back so Drop can stop us.
    let _ = rl_tx.send(run_loop as usize);

    log::debug!("macos_power: CFRunLoop running");
    unsafe {
        CFRunLoopRun();
    }
    log::debug!("macos_power: CFRunLoop returned; cleaning up");

    unsafe {
        IODeregisterForSystemPower(&mut notifier_object);
        IONotificationPortDestroy(notification_port);
        drop(Box::from_raw(ctx));
    }
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port_ref: *mut *mut c_void,
        callback: extern "C" fn(*mut c_void, u32, u32, *mut c_void),
        notifier: *mut u32,
    ) -> u32;
    fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
    fn IONotificationPortGetRunLoopSource(notify: *mut c_void) -> *mut c_void;
    fn IONotificationPortDestroy(notify: *mut c_void);
    fn IOAllowPowerChange(kernel_port: u32, notification_id: isize) -> i32;
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: *const c_void,
        user_type: c_int,
        assertion_id: *mut u32,
    ) -> c_int;
    fn IOPMAssertionRelease(assertion_id: u32) -> c_int;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_string: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFRelease(value: *const c_void);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: *mut c_void);
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
}

#[cfg(test)]
mod tests {
    use core_graphics::event::EventField;
    use input_event::MACOS_KEEP_AWAKE_EVENT_TAG;

    use super::{UserActivity, keep_awake_mouse_event};

    #[test]
    fn user_activity_assertion_starts_and_stops() {
        let mut activity = UserActivity::default();

        activity.start();
        assert!(activity.active);
        assert_ne!(activity.assertion_id, 0);

        activity.stop();
        assert!(!activity.active);
        assert_eq!(activity.assertion_id, 0);
    }

    #[test]
    fn keep_awake_mouse_event_carries_the_capture_filter_tag() {
        let event = keep_awake_mouse_event().expect("keep-awake event");

        assert_eq!(
            event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA),
            MACOS_KEEP_AWAKE_EVENT_TAG
        );
    }
}
