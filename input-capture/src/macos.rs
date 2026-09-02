use super::{
    Capture, CaptureError, CaptureEvent, HostInputState, Position,
    error::MacosCaptureCreationError, normalize_cursor_in_layout,
};
use async_trait::async_trait;
use bitflags::bitflags;
use core_foundation::{
    base::{CFRelease, TCFType, kCFAllocatorDefault},
    date::CFTimeInterval,
    number::{CFBooleanRef, kCFBooleanTrue},
    runloop::{CFRunLoop, CFRunLoopSource, CFRunLoopSourceRef, kCFRunLoopCommonModes},
    string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8},
};
use core_graphics::{
    base::{CGError, CGFloat, kCGErrorSuccess},
    display::{CGDisplay, CGPoint},
    event::{
        CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
        CGEventTapPlacement, CGEventTapProxy, CGEventType, CallbackResult, EventField,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
};
use futures_core::Stream;
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, CrossingModifier, Event, KeyboardEvent,
    MACOS_KEEP_AWAKE_EVENT_TAG, PointerEvent,
    display::{DisplayEdge, DisplayLayout},
    scancode,
};
use keycode::{KeyMap, KeyMapping};
use libc::c_void;
use once_cell::unsync::Lazy;
use std::{
    collections::{HashMap, HashSet},
    ffi::{CString, c_char},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll, ready},
    thread::{self},
};
use tokio::sync::{
    Mutex,
    mpsc::{self, Receiver, Sender},
    oneshot,
};

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Bounds {
    xmin: f64,
    xmax: f64,
    ymin: f64,
    ymax: f64,
}

fn display_edge(position: Position) -> DisplayEdge {
    match position {
        Position::Left => DisplayEdge::Left,
        Position::Right => DisplayEdge::Right,
        Position::Top => DisplayEdge::Top,
        Position::Bottom => DisplayEdge::Bottom,
    }
}

/// Build the Quartz display layout in the integer logical-point coordinate
/// space used by cursor events. Floor/ceil retain an entire display if macOS
/// ever reports fractional bounds while keeping contour operations exact.
fn display_layout_from_ids(ids: impl IntoIterator<Item = u32>) -> DisplayLayout {
    DisplayLayout::new(ids.into_iter().filter_map(|id| {
        let bounds = CGDisplay::new(id).bounds();
        let left = bounds.origin.x.floor();
        let top = bounds.origin.y.floor();
        let right = (bounds.origin.x + bounds.size.width).ceil();
        let bottom = (bounds.origin.y + bounds.size.height).ceil();
        if !left.is_finite()
            || !top.is_finite()
            || !right.is_finite()
            || !bottom.is_finite()
            || left < f64::from(i32::MIN)
            || top < f64::from(i32::MIN)
            || right > f64::from(i32::MAX)
            || bottom > f64::from(i32::MAX)
        {
            return None;
        }
        let x = left as i32;
        let y = top as i32;
        let width = u32::try_from((right - left) as i64).ok()?;
        let height = u32::try_from((bottom - top) as i64).ok()?;
        Some((x, y, width, height))
    }))
}

fn query_display_layout() -> Result<DisplayLayout, CGError> {
    CGDisplay::active_displays().map(display_layout_from_ids)
}

fn layout_bounds(layout: &DisplayLayout) -> Option<Bounds> {
    let bounds = layout.bounds()?;
    Some(Bounds {
        xmin: f64::from(bounds.x()),
        xmax: f64::from(bounds.right()),
        ymin: f64::from(bounds.y()),
        ymax: f64::from(bounds.bottom()),
    })
}

fn point_coordinate(value: f64) -> Option<i32> {
    let value = value.floor();
    (value.is_finite() && value >= f64::from(i32::MIN) && value <= f64::from(i32::MAX))
        .then_some(value as i32)
}

/// Return whether one Quartz mouse event is parked on the actual display
/// contour while still moving outward.
///
/// `CGEvent::location()` is already the current location of this event;
/// `MOUSE_EVENT_DELTA_*` describes movement since the preceding event. Adding
/// that delta to the current location predicts a second movement and can fire
/// early near a stepped monitor seam. WindowServer clamps a real outward move
/// onto the contour, so the delta is used only for its direction here.
fn crosses_display_contour(
    layout: &DisplayLayout,
    position: Position,
    location: (f64, f64),
    delta: (f64, f64),
) -> bool {
    if !delta.0.is_finite() || !delta.1.is_finite() {
        return false;
    }
    let Some(desired) = point_coordinate(location.0).zip(point_coordinate(location.1)) else {
        return false;
    };
    let Some(edge_point) = layout.project_point(display_edge(position), desired) else {
        return false;
    };

    match position {
        Position::Left => delta.0 < 0.0 && location.0 <= f64::from(edge_point.0),
        Position::Right => delta.0 > 0.0 && location.0 >= f64::from(edge_point.0),
        Position::Top => delta.1 < 0.0 && location.1 <= f64::from(edge_point.1),
        Position::Bottom => delta.1 > 0.0 && location.1 >= f64::from(edge_point.1),
    }
}

/// Cursor anchor used while capture is active. Left/top move one logical
/// point inward, matching the old exclusive-boundary behavior; the fallback
/// retains the contour pixel for an unusually one-point-wide display.
fn capture_anchor(
    layout: &DisplayLayout,
    position: Position,
    location: (f64, f64),
) -> Option<(f64, f64)> {
    let desired = point_coordinate(location.0).zip(point_coordinate(location.1))?;
    let edge = layout.project_point(display_edge(position), desired)?;
    let inward = match position {
        Position::Left => (edge.0.saturating_add(1), edge.1),
        Position::Top => (edge.0, edge.1.saturating_add(1)),
        Position::Right | Position::Bottom => edge,
    };
    let anchor = if layout.rectangles().any(|(_, rect)| rect.contains(inward)) {
        inward
    } else {
        edge
    };
    Some((f64::from(anchor.0), f64::from(anchor.1)))
}

#[derive(Debug)]
struct InputCaptureState {
    /// active capture positions
    active_clients: Lazy<HashSet<Position>>,
    /// Optional per-edge preflight. When absent, crossing retains the
    /// historical immediate-capture path with no extra check.
    crossing_modifiers: HashMap<Position, CrossingModifier>,
    /// the currently entered capture position, if any
    current_pos: Option<Position>,
    /// Whether this backend has successfully hidden the Quartz cursor. Keep
    /// this separate from `current_pos`: a disabled event tap must surrender
    /// capture ownership before it knows whether showing the cursor succeeded.
    /// The producer and periodic lifecycle poll can then retry a failed show
    /// without allowing local input to remain swallowed.
    cursor_hidden: bool,
    /// Generation of the most recently observed disabled-tap notification.
    /// Generations let an older producer completion avoid clearing a newer
    /// recovery that raced in behind it.
    tap_recovery_generation: u64,
    /// While present, the re-enabled event tap passes host input through but
    /// cannot begin another boundary crossing. This closes the ordering gap
    /// between the callback's direct `Begin` channel and the producer's later
    /// `AutoRelease` lifecycle event.
    tap_recovery_pending: Option<TapRecovery>,
    /// position where the cursor was captured
    enter_position: Option<CGPoint>,
    /// bounds of the input capture area
    bounds: Bounds,
    /// Every active display rectangle in Quartz's global logical-point space.
    /// Boundary detection uses this contour rather than treating the bounding
    /// union as a filled rectangle, which would create dead edges in stepped
    /// multi-monitor layouts.
    display_layout: DisplayLayout,
}

#[derive(Debug)]
enum ProducerEvent {
    /// `warp_target`, when present, is a screen-space (Quartz) point
    /// at which to warp the local cursor before showing it. Used to
    /// preserve cross-axis continuity on release: the visible cursor
    /// reappears at the host point matching where it visually was on
    /// the guest, instead of snapping back to the capture-start edge.
    Release {
        warp_target: Option<(i32, i32)>,
    },
    Create(Position),
    Destroy(Position),
    SetCrossingModifier(Position, Option<CrossingModifier>),
    Grab(Position),
    EventTapDisabled {
        recovery_generation: u64,
        interrupted_pos: Option<Position>,
    },
    ScreenUnlocked,
    DisplayReconfigured,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TapRecovery {
    generation: u64,
    /// Capture edge whose direct-channel `Begin` needs a matching
    /// `AutoRelease`. This may be filled later by a `Grab` that was queued
    /// immediately before the disabled-tap notification.
    interrupted_pos: Option<Position>,
    /// Set while holding the shared state lock before publishing AutoRelease.
    /// Carrying this bit into a superseding recovery generation prevents two
    /// rapid tap-disable notifications from releasing the same Begin twice.
    auto_release_claimed: bool,
}

/// Stop callback-side capture ownership while preserving the interrupted edge
/// for the producer task's wire-visible cleanup. This transition must happen
/// before a disabled event tap is re-enabled: otherwise a racing event can
/// still see `current_pos` and be swallowed as captured input.
fn event_tap_disabled_transition(
    current_pos: &mut Option<Position>,
    recovery_generation: &mut u64,
    recovery_pending: &mut Option<TapRecovery>,
) -> ProducerEvent {
    *recovery_generation = recovery_generation.wrapping_add(1);
    if *recovery_generation == 0 {
        *recovery_generation = 1;
    }
    let previous = recovery_pending.take();
    let interrupted_pos = current_pos
        .take()
        .or(previous.and_then(|recovery| recovery.interrupted_pos));
    let auto_release_claimed = previous.is_some_and(|recovery| recovery.auto_release_claimed);
    *recovery_pending = Some(TapRecovery {
        generation: *recovery_generation,
        interrupted_pos,
        auto_release_claimed,
    });
    ProducerEvent::EventTapDisabled {
        recovery_generation: *recovery_generation,
        interrupted_pos,
    }
}

fn complete_event_tap_recovery(pending: &mut Option<TapRecovery>, generation: u64) -> bool {
    if pending.as_ref().map(|recovery| recovery.generation) != Some(generation) {
        return false;
    }
    *pending = None;
    true
}

fn defer_grab_during_tap_recovery(pending: &mut Option<TapRecovery>, position: Position) -> bool {
    let Some(recovery) = pending.as_mut() else {
        return false;
    };
    recovery.interrupted_pos.get_or_insert(position);
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TapRecoveryClaim {
    SupersededOrClaimed,
    NoCapture,
    AutoRelease(Position),
}

fn claim_tap_recovery_auto_release(
    pending: &mut Option<TapRecovery>,
    generation: u64,
) -> TapRecoveryClaim {
    let Some(recovery) = pending
        .as_mut()
        .filter(|recovery| recovery.generation == generation)
    else {
        return TapRecoveryClaim::SupersededOrClaimed;
    };
    if recovery.auto_release_claimed {
        return TapRecoveryClaim::SupersededOrClaimed;
    }
    let Some(position) = recovery.interrupted_pos else {
        return TapRecoveryClaim::NoCapture;
    };
    recovery.auto_release_claimed = true;
    TapRecoveryClaim::AutoRelease(position)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProducerOutcome {
    host_input_state: Option<HostInputState>,
    auto_release: Option<Position>,
}

impl InputCaptureState {
    fn new() -> Result<Self, MacosCaptureCreationError> {
        let mut res = Self {
            active_clients: Lazy::new(HashSet::new),
            crossing_modifiers: HashMap::new(),
            current_pos: None,
            cursor_hidden: false,
            tap_recovery_generation: 0,
            tap_recovery_pending: None,
            enter_position: None,
            bounds: Bounds::default(),
            display_layout: DisplayLayout::default(),
        };
        res.update_bounds()?;
        Ok(res)
    }

    fn begin_event_tap_recovery(&mut self) -> ProducerEvent {
        event_tap_disabled_transition(
            &mut self.current_pos,
            &mut self.tap_recovery_generation,
            &mut self.tap_recovery_pending,
        )
    }

    fn crossed(&mut self, event: &CGEvent) -> Option<Position> {
        let location = event.location();
        let relative_x = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
        let relative_y = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);

        for &position in self.active_clients.iter() {
            if crosses_display_contour(
                &self.display_layout,
                position,
                (location.x, location.y),
                (relative_x, relative_y),
            ) {
                log::debug!("Crossed barrier into position: {position:?}");
                return Some(position);
            }
        }
        None
    }

    // Get the max bounds of all displays
    fn update_bounds(&mut self) -> Result<(), MacosCaptureCreationError> {
        let layout = query_display_layout().map_err(MacosCaptureCreationError::ActiveDisplays)?;

        self.commit_display_layout(layout);
        log::debug!("Updated displays bounds: {0:?}", self.bounds);
        Ok(())
    }

    /// Commit one complete geometry generation. If a monitor changes while
    /// capture is active, move the hidden-cursor anchor onto the equivalent
    /// cross-axis point of the new contour before the next event resets it.
    /// Otherwise an unplugged/rearranged display leaves `enter_position`
    /// pointing into empty space for the rest of the capture session.
    fn commit_display_layout(&mut self, layout: DisplayLayout) -> bool {
        // Only commit a complete, non-empty snapshot. A transient zero-display
        // result during reconfiguration otherwise destroys every capture
        // edge; retaining the last good topology self-heals at the next poll.
        let Some(bounds) = layout_bounds(&layout) else {
            log::warn!("update_bounds: no usable active displays; keeping previous topology");
            return false;
        };

        let geometry_changed = layout != self.display_layout;
        let refreshed_anchor = geometry_changed
            .then(|| self.current_pos.zip(self.enter_position))
            .flatten()
            .and_then(|(position, anchor)| {
                capture_anchor(&layout, position, (anchor.x, anchor.y))
                    .map(|(x, y)| CGPoint { x, y })
            });
        self.display_layout = layout;
        self.bounds = bounds;
        if let Some(anchor) = refreshed_anchor {
            log::info!(
                "display changed during capture; moved hidden cursor anchor to ({:.0}, {:.0})",
                anchor.x,
                anchor.y,
            );
            self.enter_position = Some(anchor);
        }
        true
    }

    /// start the input capture by
    fn start_capture(&mut self, event: &CGEvent, position: Position) -> Result<(), CaptureError> {
        let event_location = event.location();
        let (x, y) = capture_anchor(
            &self.display_layout,
            position,
            (event_location.x, event_location.y),
        )
        .ok_or(CaptureError::DisplayTopologyUnavailable)?;
        let location = CGPoint { x, y };
        self.enter_position = Some(location);
        self.reset_cursor()
    }

    /// resets the cursor to the position, where the capture started
    fn reset_cursor(&mut self) -> Result<(), CaptureError> {
        let pos = self.enter_position.expect("capture active");
        log::trace!("Resetting cursor position to: {}, {}", pos.x, pos.y);
        CGDisplay::warp_mouse_cursor_position(pos).map_err(CaptureError::WarpCursor)
    }

    fn hide_cursor(&mut self) -> Result<(), CaptureError> {
        if self.cursor_hidden {
            return Ok(());
        }
        CGDisplay::hide_cursor(&CGDisplay::main()).map_err(CaptureError::CoreGraphics)?;
        self.cursor_hidden = true;
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), CaptureError> {
        if !self.cursor_hidden {
            return Ok(());
        }
        CGDisplay::show_cursor(&CGDisplay::main()).map_err(CaptureError::CoreGraphics)?;
        self.cursor_hidden = false;
        Ok(())
    }

    /// A display/session transition can make the first show request fail. Once
    /// capture ownership is gone, retry opportunistically so the cursor cannot
    /// remain hidden until the whole backend is recreated.
    fn retry_show_cursor_if_unowned(&mut self, context: &str) {
        if self.current_pos.is_none() && self.cursor_hidden {
            if let Err(error) = self.show_cursor() {
                log::warn!("failed to show cursor during {context}; will retry: {error}");
            }
        }
    }

    async fn handle_producer_event(
        &mut self,
        producer_event: ProducerEvent,
    ) -> Result<ProducerOutcome, CaptureError> {
        log::debug!("handling event: {producer_event:?}");
        let mut outcome = ProducerOutcome::default();
        match producer_event {
            ProducerEvent::Release { warp_target } => {
                log::info!(
                    "[release-warp] handle_producer_event Release: current_pos={:?} warp_target={warp_target:?}",
                    self.current_pos
                );
                if self.current_pos.take().is_some() {
                    // We hold the callback's state mutex for this whole
                    // transition, so clearing ownership first prevents any
                    // later callback from swallowing input. Warp while the
                    // cursor is still hidden, then reveal it at that point.
                    if let Some((x, y)) = warp_target {
                        log::info!("[release-warp] warping local cursor to ({x}, {y})");
                        if let Err(e) = CGDisplay::warp_mouse_cursor_position(CGPoint {
                            x: x as CGFloat,
                            y: y as CGFloat,
                        }) {
                            log::warn!("[release-warp] warp_mouse_cursor_position failed: {e:?}");
                        }
                    }
                    self.show_cursor()?;
                } else {
                    // A disabled-tap transition clears `current_pos`
                    // synchronously. If its first show request failed, the
                    // ordinary outer release is another chance to repair it.
                    self.retry_show_cursor_if_unowned("capture release");
                }
            }
            ProducerEvent::Grab(pos) => {
                if defer_grab_during_tap_recovery(&mut self.tap_recovery_pending, pos) {
                    // `Begin` travels on the direct event channel, while Grab
                    // travels on the producer channel. A tap disable can set
                    // the shared recovery gate after Begin but before this
                    // queued Grab is consumed. Record the edge for the
                    // disabled event's AutoRelease, but do not re-hide the
                    // cursor or restore capture ownership.
                    log::debug!("discarding queued Grab({pos:?}) during CGEventTap recovery");
                } else if self.current_pos.is_none() {
                    self.hide_cursor()?;
                    self.current_pos = Some(pos);
                }
            }
            ProducerEvent::Create(p) => {
                self.active_clients.insert(p);
            }
            ProducerEvent::Destroy(p) => {
                self.active_clients.remove(&p);
                self.crossing_modifiers.remove(&p);
                if self.current_pos == Some(p) {
                    self.current_pos = None;
                    self.show_cursor()?;
                }
            }
            ProducerEvent::SetCrossingModifier(pos, modifier) => {
                if let Some(modifier) = modifier {
                    self.crossing_modifiers.insert(pos, modifier);
                } else {
                    self.crossing_modifiers.remove(&pos);
                }
            }
            ProducerEvent::EventTapDisabled {
                recovery_generation,
                interrupted_pos: _,
            } => {
                // Tap death can happen mid-capture (TCC Accessibility
                // revoked, tap-timeout, etc). Release state so we
                // don't leave the cursor hidden even if the outer
                // task only logs this error rather than propagating.
                if let Some(pos) = self.current_pos.take() {
                    if let Some(recovery) = self.tap_recovery_pending.as_mut() {
                        recovery.interrupted_pos.get_or_insert(pos);
                    }
                }
                self.retry_show_cursor_if_unowned("disabled event-tap cleanup");

                let recovery_claim = claim_tap_recovery_auto_release(
                    &mut self.tap_recovery_pending,
                    recovery_generation,
                );
                if let TapRecoveryClaim::AutoRelease(pos) = recovery_claim {
                    // The helper claims before this task drops the shared state
                    // lock to publish. A superseding disabled notification
                    // carries the claim and cannot release this Begin twice.
                    outcome.auto_release = Some(pos);
                }
                // A tap disabled while the host screen is locked is
                // the lock screen's secure-input mode, not a
                // permission revocation. AXIsProcessTrusted() is
                // unreliable in the locked loginwindow session and
                // can read `false` with the grant fully intact —
                // probing it here would exit the daemon on every
                // lock (and screen-saver). Treat lock as recoverable:
                // the `com.apple.screenIsUnlocked` observer on the
                // event-tap thread re-enables the tap once the user
                // logs back in. Bail out before the AX probe.
                if is_screen_locked() {
                    log::info!(
                        "CGEventTap disabled while screen locked — will re-enable on unlock"
                    );
                    outcome.host_input_state = Some(HostInputState::Locked);
                    return Ok(outcome);
                }
                // Distinguish AX revocation from a recoverable cause
                // (secure-input mode while typing in a password field
                // also fires TapDisabledByUserInput). If AX is gone,
                // the tap can't be recreated and the GUI's polling
                // watcher may not flip for a while when the user
                // *removed* the entry from System Settings → Privacy
                // & Security → Accessibility (vs just toggling it
                // off — removal can leave AXIsProcessTrusted reporting
                // cached-true in already-running processes). Exit
                // the daemon process directly: the GUI will see its
                // IPC connection drop and trigger its own
                // quit-with-backstop path. This is the only reliable
                // way to tear down a wedged HID-level tap quickly.
                if !unsafe { AXIsProcessTrusted() } {
                    log::error!(
                        "CGEventTap disabled and Accessibility no longer granted — daemon exiting"
                    );
                    std::process::exit(0);
                }
                if recovery_claim == TapRecoveryClaim::NoCapture {
                    return Err(CaptureError::EventTapDisabled);
                }
            }
            ProducerEvent::ScreenUnlocked => {
                // The distributed notification is authoritative in normal
                // operation, but verify the WindowServer state before telling
                // a peer it is safe to type a password. A racing/spurious
                // notification must degrade to no update, never a false
                // "unlocked" confirmation.
                if is_screen_locked() {
                    log::warn!("screen-unlocked notification arrived while session still locked");
                } else {
                    self.retry_show_cursor_if_unowned("screen unlock");
                    outcome.host_input_state = Some(HostInputState::Unlocked);
                    return Ok(outcome);
                }
            }
            ProducerEvent::DisplayReconfigured => {
                // The macOS display configuration changed — a monitor
                // was plugged in/out, the resolution changed, the
                // arrangement was rearranged, etc. Re-fetch the
                // active-display bounds so barrier crossings and the
                // cursor-warp on capture-start use the current
                // geometry instead of whatever was true at process
                // start.
                if let Err(e) = self.update_bounds() {
                    log::warn!("failed to refresh display bounds: {e}");
                } else {
                    log::info!("display reconfigured: {:?}", self.bounds);
                }
            }
        };
        Ok(outcome)
    }
}

async fn publish_host_input_state(
    event_tx: &Sender<(Position, CaptureEvent)>,
    positions: Vec<Position>,
    state: HostInputState,
) {
    for pos in positions {
        if let Err(e) = event_tx
            .send((pos, CaptureEvent::HostInputState(state)))
            .await
        {
            log::debug!("capture stream closed while publishing host input state: {e}");
            break;
        }
    }
}

// Device-dependent modifier bits from IOKit/hidsystem/IOLLEvent.h. Unlike
// Quartz's device-independent Shift/Control/Alternate/Command flags, these
// identify both the side and the current physical state. A FlagsChanged event
// can therefore be decoded without comparing a potentially stale aggregate
// mask (and while both keys of the same modifier are held).
const NX_DEVICE_LCTRL_KEY_MASK: u64 = 0x0000_0001;
const NX_DEVICE_LSHIFT_KEY_MASK: u64 = 0x0000_0002;
const NX_DEVICE_RSHIFT_KEY_MASK: u64 = 0x0000_0004;
const NX_DEVICE_LCOMMAND_KEY_MASK: u64 = 0x0000_0008;
const NX_DEVICE_RCOMMAND_KEY_MASK: u64 = 0x0000_0010;
const NX_DEVICE_LALT_KEY_MASK: u64 = 0x0000_0020;
const NX_DEVICE_RALT_KEY_MASK: u64 = 0x0000_0040;
const NX_DEVICE_RCTRL_KEY_MASK: u64 = 0x0000_2000;

const STANDARD_MODIFIER_KEYS: [(scancode::Linux, u64); 8] = [
    (scancode::Linux::KeyLeftShift, NX_DEVICE_LSHIFT_KEY_MASK),
    (scancode::Linux::KeyRightShift, NX_DEVICE_RSHIFT_KEY_MASK),
    (scancode::Linux::KeyLeftCtrl, NX_DEVICE_LCTRL_KEY_MASK),
    (scancode::Linux::KeyRightCtrl, NX_DEVICE_RCTRL_KEY_MASK),
    (scancode::Linux::KeyLeftAlt, NX_DEVICE_LALT_KEY_MASK),
    (scancode::Linux::KeyRightalt, NX_DEVICE_RALT_KEY_MASK),
    (scancode::Linux::KeyLeftMeta, NX_DEVICE_LCOMMAND_KEY_MASK),
    (scancode::Linux::KeyRightmeta, NX_DEVICE_RCOMMAND_KEY_MASK),
];

fn modifier_masks(flags: CGEventFlags) -> (XMods, XMods) {
    let mut depressed = XMods::empty();
    let mut locked = XMods::empty();

    if flags.contains(CGEventFlags::CGEventFlagShift) {
        depressed |= XMods::ShiftMask;
    }
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        depressed |= XMods::ControlMask;
    }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) {
        depressed |= XMods::Mod1Mask;
    }
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        depressed |= XMods::Mod4Mask;
    }
    if flags.contains(CGEventFlags::CGEventFlagAlphaShift) {
        locked |= XMods::LockMask;
    }

    (depressed, locked)
}

fn crossing_modifier_held(modifier: CrossingModifier, flags: CGEventFlags) -> bool {
    let flag = match modifier {
        CrossingModifier::Control => CGEventFlags::CGEventFlagControl,
        CrossingModifier::Alt => CGEventFlags::CGEventFlagAlternate,
        CrossingModifier::Shift => CGEventFlags::CGEventFlagShift,
        CrossingModifier::Super => CGEventFlags::CGEventFlagCommand,
    };
    flags.contains(flag)
}

fn crossing_preflight_allows(required: Option<CrossingModifier>, flags: CGEventFlags) -> bool {
    required.is_none_or(|modifier| crossing_modifier_held(modifier, flags))
}

/// Snapshot the post-remapping modifier state carried by the mouse event that
/// crossed the boundary. A Session event tap sees Karabiner's virtual-HID
/// output, so these are deliberately the logical keys macOS currently exposes
/// rather than an attempt to bypass the user's remaps.
fn modifier_snapshot(flags: CGEventFlags) -> Vec<CaptureEvent> {
    let mut events = STANDARD_MODIFIER_KEYS
        .into_iter()
        .filter(|(_, mask)| flags.bits() & mask != 0)
        .map(|(key, _)| {
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: key as u32,
                state: 1,
            }))
        })
        .collect::<Vec<_>>();

    let (depressed, locked) = modifier_masks(flags);
    events.push(CaptureEvent::Input(Event::Keyboard(
        KeyboardEvent::Modifiers {
            depressed: depressed.bits(),
            latched: 0,
            locked: locked.bits(),
            group: 0,
        },
    )));
    events
}

fn modifier_key_state(key: u32, flags: CGEventFlags) -> u8 {
    use scancode::Linux::{
        KeyCapsLock, KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift, KeyRightCtrl,
        KeyRightShift, KeyRightalt, KeyRightmeta,
    };

    let Ok(key) = scancode::Linux::try_from(key) else {
        return 0;
    };

    let device_mask = match key {
        KeyLeftCtrl => NX_DEVICE_LCTRL_KEY_MASK,
        KeyLeftShift => NX_DEVICE_LSHIFT_KEY_MASK,
        KeyRightShift => NX_DEVICE_RSHIFT_KEY_MASK,
        KeyLeftMeta => NX_DEVICE_LCOMMAND_KEY_MASK,
        KeyRightmeta => NX_DEVICE_RCOMMAND_KEY_MASK,
        KeyLeftAlt => NX_DEVICE_LALT_KEY_MASK,
        KeyRightalt => NX_DEVICE_RALT_KEY_MASK,
        KeyRightCtrl => NX_DEVICE_RCTRL_KEY_MASK,
        // Caps Lock is handled as an atomic down/up tap by `get_events`.
        KeyCapsLock => return 0,
        // Fn/Globe has no evdev mapping in the shared keycode table and is not
        // synthesized here. Any future FlagsChanged key must get an explicit
        // physical-state source rather than reviving aggregate-mask ordering.
        _ => return 0,
    };

    u8::from(flags.bits() & device_mask != 0)
}

fn modifier_key_states(key: u32, flags: CGEventFlags) -> ([u8; 2], usize) {
    let is_caps_lock =
        scancode::Linux::try_from(key).is_ok_and(|key| key == scancode::Linux::KeyCapsLock);
    if is_caps_lock {
        ([1, 0], 2)
    } else {
        ([modifier_key_state(key, flags), 0], 1)
    }
}

fn get_events(
    ev_type: &CGEventType,
    ev: &CGEvent,
    result: &mut Vec<CaptureEvent>,
) -> Result<(), CaptureError> {
    fn map_pointer_event(ev: &CGEvent) -> PointerEvent {
        PointerEvent::Motion {
            time: 0,
            dx: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X),
            dy: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y),
        }
    }

    fn map_key(ev: &CGEvent) -> Result<u32, CaptureError> {
        let code = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        match KeyMap::from_key_mapping(KeyMapping::Mac(code as u16)) {
            Ok(k) => Ok(k.evdev as u32),
            Err(()) => Err(CaptureError::KeyMapError(code)),
        }
    }

    match ev_type {
        CGEventType::KeyDown => {
            // Drop OS-generated auto-repeat KeyDowns. macOS streams a
            // run of KeyDown events (this field set to 1) while a key is
            // held; forwarding them would collide with the sink's own
            // repeat generation — every sink (Linux compositors, and the
            // macOS/Windows repeat tasks) synthesizes repeat from a
            // single held key. Forward only the genuine initial press so
            // the sink owns repeat timing.
            if ev.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0 {
                return Ok(());
            }
            let k = map_key(ev)?;
            result.push(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: k,
                state: 1,
            })));
        }
        CGEventType::KeyUp => {
            let k = map_key(ev)?;
            result.push(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: k,
                state: 0,
            })));
        }
        CGEventType::FlagsChanged => {
            let cg_flags = ev.get_flags();
            let (depressed, mods_locked) = modifier_masks(cg_flags);

            if let Ok(key) = map_key(ev) {
                // macOS emits one FlagsChanged event per Caps Lock toggle,
                // not separate physical down/up events. `modifier_key_states`
                // returns an atomic tap for Caps on both toggle-on and
                // toggle-off; standard modifiers return their one live phase.
                let (states, count) = modifier_key_states(key, cg_flags);
                result.extend(states[..count].iter().map(|&state| {
                    CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                        time: 0,
                        key,
                        state,
                    }))
                }));
            }

            let modifier_event = KeyboardEvent::Modifiers {
                depressed: depressed.bits(),
                latched: 0,
                locked: mods_locked.bits(),
                group: 0,
            };

            result.push(CaptureEvent::Input(Event::Keyboard(modifier_event)));
        }
        CGEventType::MouseMoved => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::LeftMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::RightMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::OtherMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::LeftMouseDown => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 1,
            })))
        }
        CGEventType::LeftMouseUp => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 0,
            })))
        }
        CGEventType::RightMouseDown => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_RIGHT,
                state: 1,
            })))
        }
        CGEventType::RightMouseUp => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_RIGHT,
                state: 0,
            })))
        }
        CGEventType::OtherMouseDown => {
            let btn_num = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                3 => BTN_BACK,
                4 => BTN_FORWARD,
                _ => BTN_MIDDLE,
            };
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 1,
            })))
        }
        CGEventType::OtherMouseUp => {
            let btn_num = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                3 => BTN_BACK,
                4 => BTN_FORWARD,
                _ => BTN_MIDDLE,
            };
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 0,
            })))
        }
        CGEventType::ScrollWheel => {
            // Emit scroll deltas in the *classic* mouse-wheel convention
            // (the historical baseline that predates natural scrolling)
            // regardless of the user's macOS Natural Scrolling
            // preference. Rationale:
            //
            //   1. Classic was the canonical scroll convention when
            //      the scroll wheel was invented; using it as the
            //      wire format keeps Mousehop predictable for any
            //      receiver, including non-natural-aware peers.
            //   2. Receivers opt into natural-feel via their own
            //      `natural_scroll` config, mirroring how libinput's
            //      natural_scroll knob works for physical input.
            //   3. macOS Natural Scrolling pre-flips POINT_DELTA at
            //      the OS layer; CGEventTap at Session placement sees
            //      events after that flip. So:
            //        Natural ON: POINT_DELTA already flipped (away
            //          from classic) → re-flip back to classic by
            //          NOT flipping in our code (sign = +1).
            //        Natural OFF: POINT_DELTA already in classic →
            //          flip once to invert away from raw and… wait,
            //          actually we want to land on classic regardless.
            //          With Natural OFF the OS gives us "raw classic"
            //          *as-the-mac-sees-it*; our peers' wl_pointer
            //          treats positive Y as "document moves down on
            //          screen" (natural-feel). To present classic
            //          feel on the wire we negate (sign = -1).
            //
            // Net result: wire is consistently classic-feel regardless
            // of the Mac's preference. Receivers can re-invert.
            let sign: i64 = if natural_scrolling_enabled() { 1 } else { -1 };
            if ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
                // kCGScrollWheelEventMomentumPhase (raw field 123 — core-graphics
                // 0.25 has no named constant). Non-zero = an OS-synthesised
                // momentum-coast delta the trackpad keeps emitting after the
                // finger lifts. Flag it so a non-macOS sink can drop it (it
                // would otherwise pin the sink's gap-inference kinetic scroll).
                const SCROLL_WHEEL_EVENT_MOMENTUM_PHASE: u32 = 123;
                let momentum = ev.get_integer_value_field(SCROLL_WHEEL_EVENT_MOMENTUM_PHASE) != 0;
                let v = sign
                    * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
                let h = sign
                    * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
                if v != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis: 0, // Vertical
                        value: v as f64,
                        momentum,
                    })));
                }
                // Rest-to-stop over the link. A cohort app stops its kinetic
                // coast when fingers rest on the trackpad; locally that's the
                // Wayland hold gesture, but no virtual-input backend can inject
                // a pointer gesture, so it can't cross the KVM. macOS signals a
                // finger touch-down as a CGScrollPhase Began(1)/MayBegin(128)
                // event with no movement (and no momentum). Forward a 1px nudge
                // on it so the sink cohort app's raw-delta re-touch path halts
                // the fling. These are edge events (one per touch-down), so a
                // motionless rest doesn't creep; a real scroll absorbs the 1px.
                const SCROLL_WHEEL_EVENT_SCROLL_PHASE: u32 = 99;
                let scroll_phase = ev.get_integer_value_field(SCROLL_WHEEL_EVENT_SCROLL_PHASE);
                if !momentum && v == 0 && h == 0 && matches!(scroll_phase, 1 | 128) {
                    result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis: 0, // Vertical — trips the sink's re-touch stop.
                        value: 1.0,
                        momentum: false,
                    })));
                }
                if h != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis: 1, // Horizontal
                        value: h as f64,
                        momentum,
                    })));
                }
            } else {
                // line based scrolling
                //
                // macOS already amplifies SCROLL_WHEEL_EVENT_DELTA based
                // on wheel velocity — a slow notch on a notched wheel
                // (e.g. MX Master 4) reports DELTA=1, a fast flick
                // reports DELTA=10+ per event. The wl_pointer v120
                // protocol expects one physical wheel click = 120
                // units, so map one macOS DELTA line to one full v120
                // tick. (The previous 3-lines-per-step ratio caused
                // single notches to truncate to discrete=0 on the
                // receiver, leaving Slack/Alacritty unscrollable until
                // 3+ notches accumulated.)
                const LINES_PER_STEP: i32 = 1;
                const V120_STEPS_PER_LINE: i32 = 120 / LINES_PER_STEP;
                let v =
                    sign * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
                let h =
                    sign * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
                if v != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(
                        PointerEvent::AxisDiscrete120 {
                            axis: 0, // Vertical
                            value: V120_STEPS_PER_LINE * v as i32,
                        },
                    )));
                }
                if h != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(
                        PointerEvent::AxisDiscrete120 {
                            axis: 1, // Horizontal
                            value: V120_STEPS_PER_LINE * h as i32,
                        },
                    )));
                }
            }
        }
        _ => (),
    }
    Ok(())
}

fn create_event_tap<'a>(
    client_state: Arc<Mutex<InputCaptureState>>,
    notify_tx: Sender<ProducerEvent>,
    event_tx: Sender<(Position, CaptureEvent)>,
) -> Result<(CGEventTap<'a>, Arc<OnceLock<usize>>), MacosCaptureCreationError> {
    // Shared slot for the tap's mach port pointer. Stored as `usize`
    // because raw pointers aren't `Send`, but the integer
    // representation is — and CGEventTapEnable is documented as
    // thread-safe. Set immediately after CGEventTap::new returns;
    // read by the callback to recover from either disabled-tap notification.
    let tap_mach_port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
    let tap_mach_port_cb = Arc::clone(&tap_mach_port);

    let cg_events_of_interest: Vec<CGEventType> = vec![
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ];

    let event_tap_callback = move |_proxy: CGEventTapProxy,
                                   event_type: CGEventType,
                                   cg_ev: &CGEvent| {
        // The daemon posts this same-position MouseMoved event to
        // reset macOS's screen-saver idle timer while capture is on a
        // peer. Let WindowServer consume it, but do not turn it into a
        // forwarded pointer delta or snap the hidden cursor to the
        // edge again. The shared tag is the contract between the
        // daemon and this tap.
        if is_mousehop_keep_awake_event(cg_ev) {
            return CallbackResult::Keep;
        }

        log::trace!("Got event from tap: {event_type:?}");
        let mut state = client_state.blocking_lock();
        let mut capture_position = None;
        let mut res_events = vec![];

        if matches!(
            event_type,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            // A disabled tap cannot safely retain capture ownership. Even a
            // recoverable timeout creates a gap in the forwarded input stream;
            // keeping `current_pos` would swallow local events after Quartz
            // resumes and could leave wire-visible keys held on the peer.
            // Clear capture state and reveal the cursor synchronously, then
            // queue AutoRelease cleanup before asking Quartz to re-enable the
            // tap. The user-input path uses the same ordering for secure-input
            // and Accessibility transitions.
            let reason = match event_type {
                CGEventType::TapDisabledByTimeout => "timeout",
                CGEventType::TapDisabledByUserInput => "user input",
                _ => unreachable!("non-disabled event entered disabled-tap handling"),
            };
            log::warn!("CGEventTap disabled by {reason} — releasing capture state");
            let producer_event = state.begin_event_tap_recovery();
            let interrupted_pos = match &producer_event {
                ProducerEvent::EventTapDisabled {
                    interrupted_pos, ..
                } => *interrupted_pos,
                _ => unreachable!("tap-disabled transition returned the wrong event"),
            };
            if let Some(Err(error)) = interrupted_pos.map(|_| state.show_cursor()) {
                log::error!("failed to show cursor after CGEventTap disable: {error}");
            }

            // Never block the producer while holding its state mutex: it must
            // acquire the same lock to turn this event into AutoRelease.
            drop(state);
            notify_tx
                .blocking_send(producer_event)
                .unwrap_or_else(|error| {
                    log::error!("failed to send CGEventTap-disabled notification: {error}");
                });

            // Re-enable last so no racing callback observes the stale capture
            // state and so the producer cleanup is already queued if Quartz
            // immediately delivers another event.
            if let Some(&port) = tap_mach_port_cb.get() {
                log::warn!("requesting CGEventTap re-enable after {reason}");
                unsafe {
                    CGEventTapEnable(port as *mut c_void, true);
                }
            } else {
                log::error!(
                    "CGEventTap disabled by {reason}, but mach port is unavailable for re-enable"
                );
            }
            return CallbackResult::Keep;
        }

        // Are we in a client?
        if let Some(current_pos) = state.current_pos {
            capture_position = Some(current_pos);
            get_events(&event_type, cg_ev, &mut res_events).unwrap_or_else(|e| {
                log::error!("Failed to get events: {e}");
            });

            // Keep (hidden) cursor at the edge of the screen
            if matches!(
                event_type,
                CGEventType::MouseMoved
                    | CGEventType::LeftMouseDragged
                    | CGEventType::RightMouseDragged
                    | CGEventType::OtherMouseDragged
            ) {
                state.reset_cursor().unwrap_or_else(|e| log::warn!("{e}"));
            }
        } else if matches!(event_type, CGEventType::MouseMoved)
            && state.tap_recovery_pending.is_none()
        {
            // Did we cross a barrier?
            if let Some(new_pos) = state.crossed(cg_ev) {
                // About to commit the cross — final gate: skip if the
                // host is locked, since the lock screen consumes
                // keyboard before our tap sees it and allowing the
                // cursor to leave would produce a mouse-only-on-peer
                // half-broken state. Polling CGSession only at this
                // commit point (rather than every MouseMoved) keeps
                // the per-event cost zero — `is_screen_locked()` is
                // an XPC to WindowServer (~10–50µs); a typical user
                // crosses a wall a few times per minute.
                let required_modifier = state.crossing_modifiers.get(&new_pos).copied();
                if !crossing_preflight_allows(required_modifier, cg_ev.get_flags()) {
                    log::debug!(
                        "crossing preflight blocked {new_pos:?}: {:?} is not held",
                        required_modifier.expect("checked above")
                    );
                } else if is_screen_locked() {
                    log::info!("host screen locked; suppressing cross to {new_pos:?}");
                } else {
                    capture_position = Some(new_pos);
                    // Snapshot the cursor's screen-space position at the
                    // instant of crossing — before start_capture's
                    // reset_cursor() snaps it to the edge. The peer uses
                    // this for the visually-corresponding warp on Enter
                    // so the cursor doesn't jump to the entry-edge midpoint.
                    let cross_loc = cg_ev.location();
                    let cursor_point = (cross_loc.x as i32, cross_loc.y as i32);
                    let normalized_cursor =
                        normalize_cursor_in_layout(&state.display_layout, cursor_point);
                    let cursor = Some(cursor_point);
                    state
                        .start_capture(cg_ev, new_pos)
                        .unwrap_or_else(|e| log::warn!("{e}"));
                    // The crossing is a MouseMoved event, but Quartz carries
                    // the currently-held per-side modifier device bits on it.
                    // Emit this snapshot immediately after Begin so modifiers
                    // held before crossing are represented on the peer too.
                    res_events.push(CaptureEvent::Begin {
                        cursor,
                        normalized_cursor,
                    });
                    res_events.extend(modifier_snapshot(cg_ev.get_flags()));
                    notify_tx
                        .blocking_send(ProducerEvent::Grab(new_pos))
                        .expect("Failed to send notification");
                }
            }
        }

        if let Some(pos) = capture_position {
            for e in res_events {
                // This callback runs on the kernel's event-delivery
                // thread: time spent here delays input for the whole
                // system, and a *blocked* callback freezes input
                // outright until kCGEventTapDisabledByTimeout fires —
                // the user sees laggy, dropped keystrokes. So never
                // block unconditionally on a backed-up forwarding
                // channel (a slow/congested peer link fills it).
                //
                // Pointer-motion is a delta stream: a dropped sample
                // is absorbed by the next one, so under pressure we
                // drop it rather than block. Keyboard, button, scroll
                // and lifecycle (Begin/AutoRelease) events are NOT
                // self-correcting — a dropped key-up sticks a key on
                // the peer — so for those, and only those, we accept
                // a brief block. They're low-volume, so the channel
                // is realistically only ever full of motion samples,
                // which the drop path drains.
                match event_tx.try_send((pos, e)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full((pos, e))) => {
                        if matches!(
                            e,
                            CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { .. }))
                        ) {
                            log::debug!("forwarding channel full — dropping pointer-motion sample");
                        } else {
                            log::warn!("forwarding channel full — blocking to preserve {e}");
                            // Closed only happens on shutdown; ignore.
                            let _ = event_tx.blocking_send((pos, e));
                        }
                    }
                    // Channel closed: the InputCapture instance is
                    // being dropped. Nothing to forward to; ignore.
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
            // Returning Drop should stop the event from being processed
            // but core fundation still returns the event
            cg_ev.set_type(CGEventType::Null);
            CallbackResult::Drop
        } else {
            CallbackResult::Keep
        }
    };

    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        cg_events_of_interest,
        event_tap_callback,
    )
    .map_err(|_| MacosCaptureCreationError::EventTapCreation)?;

    // Hand the mach port pointer to the callback so it can re-enable
    // the tap after a disabled-tap notification. The pointer is valid for the
    // lifetime of `tap` (which lives on the event-tap thread until
    // the run loop exits).
    let port_ptr = tap.mach_port().as_concrete_TypeRef() as usize;
    let _ = tap_mach_port.set(port_ptr);

    // Hand the same slot back to the caller so the screen-unlock
    // observer can re-enable the tap after a lock disables it.

    let tap_source: CFRunLoopSource = tap
        .mach_port()
        .create_runloop_source(0)
        .expect("Failed creating loop source");

    unsafe {
        CFRunLoop::get_current().add_source(&tap_source, kCFRunLoopCommonModes);
    }

    Ok((tap, tap_mach_port))
}

fn is_mousehop_keep_awake_event(event: &CGEvent) -> bool {
    event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == MACOS_KEEP_AWAKE_EVENT_TAG
}

fn event_tap_thread(
    client_state: Arc<Mutex<InputCaptureState>>,
    event_tx: Sender<(Position, CaptureEvent)>,
    notify_tx: Sender<ProducerEvent>,
    ready: std::sync::mpsc::Sender<Result<CFRunLoop, MacosCaptureCreationError>>,
    exit: oneshot::Sender<()>,
) {
    // Clone now: create_event_tap consumes notify_tx into its closure.
    let display_notify_tx = notify_tx.clone();

    let (_tap, tap_mach_port) = match create_event_tap(client_state, notify_tx, event_tx) {
        Err(e) => {
            ready.send(Err(e)).expect("channel closed");
            return;
        }
        Ok((tap, port)) => {
            let run_loop = CFRunLoop::get_current();
            ready.send(Ok(run_loop)).expect("channel closed");
            (tap, port)
        }
    };

    // Subscribe to the screen-unlock distributed notification. When
    // the host locks, the lock screen's password field enables
    // secure event input, which disables our session event tap.
    // macOS never re-enables it on unlock — without this observer
    // the daemon survives the lock but the tap stays dead and
    // mousehop silently stops capturing. Box-leak the refcon so the
    // C side has a stable observer pointer; reclaim it after the run
    // loop exits.
    let unlock_ctx = Box::into_raw(Box::new(UnlockObserverCtx {
        tap_mach_port: Arc::clone(&tap_mach_port),
        sender: display_notify_tx.clone(),
    }));
    let unlock_name = unsafe {
        let cstr = CString::new("com.apple.screenIsUnlocked").unwrap();
        CFStringCreateWithCString(
            kCFAllocatorDefault,
            cstr.as_ptr() as *const c_char,
            kCFStringEncodingUTF8,
        )
    };
    unsafe {
        // CFNotificationSuspensionBehaviorDeliverImmediately — the
        // observer's thread run loop is always running here, but
        // deliver-immediately also covers the brief window around a
        // lock/unlock transition.
        const DELIVER_IMMEDIATELY: isize = 4;
        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDistributedCenter(),
            unlock_ctx as *const c_void,
            screen_unlocked_callback,
            unlock_name,
            std::ptr::null(),
            DELIVER_IMMEDIATELY,
        );
    }

    // Register a Quartz display-reconfiguration callback so the
    // capture state's bounds get refreshed when the user plugs in a
    // monitor, changes resolution, or rearranges displays. The
    // callback runs on this thread's CFRunLoop. Box-leak the sender
    // so the C side has a stable user_info pointer; reclaim it after
    // the run loop exits.
    let display_user_info = Box::into_raw(Box::new(display_notify_tx.clone())) as *mut c_void;
    unsafe {
        CGDisplayRegisterReconfigurationCallback(
            display_reconfiguration_callback,
            display_user_info,
        );
    }

    // Also subscribe to system-power events so we recover from
    // sleep/wake, where the Quartz reconfigure callback may not
    // fire (or fires before our run loop is processing again, e.g.
    // clamshell-disconnect → lid-open). On wake we send the same
    // DisplayReconfigured event the existing handler consumes, so
    // bounds get refreshed for free.
    let mut power_notifier_object: u32 = 0;
    let mut power_notification_port: *mut c_void = std::ptr::null_mut();
    let power_ctx = Box::into_raw(Box::new(PowerCtx {
        sender: display_notify_tx,
        root_port: 0,
    }));
    let power_root_port = unsafe {
        let port = IORegisterForSystemPower(
            power_ctx as *mut c_void,
            &mut power_notification_port,
            power_callback,
            &mut power_notifier_object,
        );
        // Stash the root port for the callback's IOAllowPowerChange
        // ack — we couldn't know it at Box-construction time because
        // it's the registration's return value.
        (*power_ctx).root_port = port;
        if !power_notification_port.is_null() {
            let src_ref = IONotificationPortGetRunLoopSource(power_notification_port);
            if !src_ref.is_null() {
                let src = CFRunLoopSource::wrap_under_get_rule(src_ref);
                CFRunLoop::get_current().add_source(&src, kCFRunLoopCommonModes);
            }
        }
        port
    };

    log::debug!("running CFRunLoop...");
    CFRunLoop::run_current();
    log::debug!("event tap thread exiting!...");

    unsafe {
        CGDisplayRemoveReconfigurationCallback(display_reconfiguration_callback, display_user_info);
        // Reclaim the leaked sender Box so we don't leak a tokio
        // channel sender on every capture create/destroy cycle.
        drop(Box::from_raw(
            display_user_info as *mut Sender<ProducerEvent>,
        ));

        // Tear down the screen-unlock observer and reclaim its
        // refcon Box, mirroring the display-callback cleanup above.
        CFNotificationCenterRemoveEveryObserver(
            CFNotificationCenterGetDistributedCenter(),
            unlock_ctx as *const c_void,
        );
        drop(Box::from_raw(unlock_ctx));
        CFRelease(unlock_name as *const c_void);

        if power_notifier_object != 0 {
            let _ = IODeregisterForSystemPower(&mut power_notifier_object);
        }
        if !power_notification_port.is_null() {
            IONotificationPortDestroy(power_notification_port);
        }
        let _ = power_root_port;
        drop(Box::from_raw(power_ctx));
    }

    let _ = exit.send(());
}

/// Query whether the host's screen is locked. Asks the WindowServer
/// for the current login session dictionary and looks up the
/// `CGSSessionScreenIsLocked` key. The key is `kCFBooleanTrue` when
/// locked; on Sequoia 15+ it's typically absent when unlocked rather
/// than `kCFBooleanFalse`, so missing-or-nil is treated as unlocked.
/// Costs ~10–50µs per call (an XPC round-trip to WindowServer);
/// called from the event tap only at an attempted edge crossing plus a
/// two-second lifecycle poll, so the amortized cost is negligible.
fn is_screen_locked() -> bool {
    let key = unsafe {
        let cstr = CString::new("CGSSessionScreenIsLocked").unwrap();
        CFStringCreateWithCString(
            kCFAllocatorDefault,
            cstr.as_ptr() as *const c_char,
            kCFStringEncodingUTF8,
        )
    };
    let dict = unsafe { CGSessionCopyCurrentDictionary() };
    if dict.is_null() {
        unsafe { CFRelease(key as *const c_void) };
        return false;
    }
    let value = unsafe { CFDictionaryGetValue(dict, key as *const c_void) };
    let locked = !value.is_null() && unsafe { CFBooleanGetValue(value as CFBooleanRef) };
    unsafe {
        CFRelease(dict as *const c_void);
        CFRelease(key as *const c_void);
    }
    locked
}

/// Refcon for the IOKit system-power callback. Bundles the channel
/// sender (so the callback can post `DisplayReconfigured` on wake)
/// and the `io_connect_t` root port (so the callback can ack
/// sleep-related messages with `IOAllowPowerChange`). Built on the
/// event-tap thread, used only by the callback on the same thread —
/// never crosses thread boundaries, so no Send/Sync needed.
struct PowerCtx {
    sender: Sender<ProducerEvent>,
    root_port: u32,
}

/// IOKit system-power callback. Fires for every power-management
/// transition (CanSleep, WillSleep, WillPowerOn, HasPoweredOn).
/// We only care about `kIOMessageSystemHasPoweredOn` (post-wake);
/// for the sleep-pending messages we just ack so the kernel doesn't
/// hold the system in its "waiting for clients" state for the full
/// 30-second timeout.
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
    // `event_tap_thread`; valid until the run loop exits and the box
    // is reclaimed. The callback only fires while the run loop runs
    // on that thread, so the box is live here.
    let ctx = unsafe { &*(refcon as *const PowerCtx) };
    match msg_type {
        K_IO_MESSAGE_CAN_SYSTEM_SLEEP | K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            // Ack so the OS doesn't stall on its 30s default timeout.
            // `msg_arg` carries the notification ID (an `intptr_t`);
            // pass it through verbatim.
            unsafe {
                IOAllowPowerChange(ctx.root_port, msg_arg as isize);
            }
        }
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => {
            // Bounce a DisplayReconfigured into the producer so
            // `update_bounds()` runs. Covers the case where Quartz's
            // own reconfigure callback didn't fire (or fired during
            // the sleep window) — e.g. clamshell-disconnect →
            // lid-open transitions.
            log::info!("system woke from sleep; refreshing display bounds");
            if let Err(e) = ctx.sender.blocking_send(ProducerEvent::DisplayReconfigured) {
                log::warn!("failed to post wake → DisplayReconfigured: {e}");
            }
        }
        _ => {}
    }
}

/// Quartz display-reconfiguration callback. Fires twice per change:
/// once with `kCGDisplayBeginConfigurationFlag` set (BEFORE the
/// change is applied — the bounds are still stale at this point),
/// then again afterwards with the actual change flags (Add, Remove,
/// Mode, DesktopShapeChanged, etc.). Skip the begin phase; on the
/// real notification, kick the producer task to refresh bounds.
extern "C" fn display_reconfiguration_callback(_display: u32, flags: u32, user_info: *mut c_void) {
    const K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG: u32 = 1 << 0;
    if flags & K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG != 0 {
        return;
    }
    if user_info.is_null() {
        return;
    }
    // SAFETY: user_info is a Box::into_raw of Sender<ProducerEvent>
    // owned by `event_tap_thread`. It's valid for the lifetime of
    // that thread; the registration is removed before the box is
    // freed. The callback only fires while the run loop is running
    // on that thread, so we know the box is live here.
    let sender = unsafe { &*(user_info as *const Sender<ProducerEvent>) };
    if let Err(e) = sender.blocking_send(ProducerEvent::DisplayReconfigured) {
        log::warn!("failed to notify display reconfiguration: {e}");
    }
}

/// Refcon for the screen-unlock distributed-notification observer.
/// Holds the tap's mach-port slot so the callback can re-enable a
/// tap that the lock screen's secure-input mode disabled. Built on
/// the event-tap thread, used only by the callback on the same
/// thread's run loop — never crosses threads after construction.
struct UnlockObserverCtx {
    tap_mach_port: Arc<OnceLock<usize>>,
    sender: Sender<ProducerEvent>,
}

/// CFNotificationCenter callback for `com.apple.screenIsUnlocked`.
/// When the host screen locks, its password field enables secure
/// event input, which disables our session event tap. macOS does
/// not re-enable the tap on unlock — we must, or the daemon keeps
/// running but captures nothing. Re-enabling an already-enabled tap
/// is a documented no-op, so calling this is safe even on the rare
/// unlock where the tap rode through the lock intact.
extern "C" fn screen_unlocked_callback(
    _center: *mut c_void,
    observer: *mut c_void,
    _name: CFStringRef,
    _object: *const c_void,
    _user_info: CFDictionaryRef,
) {
    if observer.is_null() {
        return;
    }
    // SAFETY: `observer` is the `Box::into_raw(Box::new(UnlockObserverCtx))`
    // owned by `event_tap_thread`; valid until the run loop exits and
    // the observer is removed. The callback only fires while the run
    // loop runs on that thread, so the box is live here.
    let ctx = unsafe { &*(observer as *const UnlockObserverCtx) };
    match ctx.tap_mach_port.get() {
        Some(&port) => {
            log::info!("screen unlocked — re-enabling CGEventTap");
            unsafe { CGEventTapEnable(port as *mut c_void, true) };
            if let Err(e) = ctx.sender.blocking_send(ProducerEvent::ScreenUnlocked) {
                log::warn!("failed to publish screen-unlocked transition: {e}");
            }
        }
        None => log::warn!("screen unlocked but tap mach port not yet stored — cannot re-enable"),
    }
}

pub struct MacOSInputCapture {
    event_rx: Receiver<(Position, CaptureEvent)>,
    notify_tx: Sender<ProducerEvent>,
    run_loop: CFRunLoop,
}

impl MacOSInputCapture {
    pub async fn new() -> Result<Self, MacosCaptureCreationError> {
        request_macos_capture_permissions()?;

        let state = Arc::new(Mutex::new(InputCaptureState::new()?));
        // Generously sized: the event-tap callback feeds this from
        // the kernel's input thread and only drops/blocks once it's
        // full (see the try_send path in `create_event_tap`). A deep
        // buffer rides out brief consumer stalls — a slow DTLS send,
        // a GC-like tokio scheduling gap — without dropping motion
        // samples or blocking the callback.
        let (event_tx, event_rx) = mpsc::channel(1024);
        let lifecycle_event_tx = event_tx.clone();
        let (notify_tx, mut notify_rx) = mpsc::channel(32);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (tap_exit_tx, mut tap_exit_rx) = oneshot::channel();

        unsafe {
            configure_cf_settings()?;
        }

        log::info!("Enabling CGEvent tap");
        let event_tap_thread_state = state.clone();
        let event_tap_notify = notify_tx.clone();
        thread::spawn(move || {
            event_tap_thread(
                event_tap_thread_state,
                event_tx,
                event_tap_notify,
                ready_tx,
                tap_exit_tx,
            )
        });

        // wait for event tap creation result
        let run_loop = ready_rx.recv().expect("channel closed")?;

        let _tap_task: tokio::task::JoinHandle<()> = tokio::task::spawn_local(async move {
            let mut last_host_input_state = HostInputState::Unlocked;
            // Safety net for display-geometry changes the Quartz
            // reconfiguration callback misses. That callback is
            // registered on the event-tap thread's run loop (see
            // `event_tap_thread`), but in practice it does not fire for
            // some transitions — notably opening/closing the lid on a
            // docked MacBook: the system stays awake (so the IOKit
            // power-wake path can't cover it either), the callback never
            // arrives, and `self.bounds` stays frozen at its startup
            // value. `crossed`/`start_capture` then clamp to a stale
            // rectangle — an unreachable dead band, or a too-early
            // crossing onto a display that's no longer there. Re-derive
            // the bounds on a short interval so a missed reconfiguration
            // self-heals within a couple of seconds. Mirrors the
            // emulation side's poll (EmulationTask::do_emulation_session).
            let mut bounds_poll = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                tokio::select! {
                    producer_event = notify_rx.recv() => {
                        let Some(producer_event) = producer_event else {
                            break;
                        };
                        let tap_recovery_generation = match &producer_event {
                            ProducerEvent::EventTapDisabled {
                                recovery_generation,
                                ..
                            } => Some(*recovery_generation),
                            _ => None,
                        };
                        let mut capture_state = state.lock().await;
                        let outcome = match capture_state.handle_producer_event(producer_event).await {
                            Ok(outcome) => outcome,
                            Err(e) => {
                                log::error!("Failed to handle producer event: {e}");
                                ProducerOutcome::default()
                            }
                        };
                        let transition = match outcome.host_input_state {
                            Some(next) if next != last_host_input_state => {
                                last_host_input_state = next;
                                Some((
                                    next,
                                    capture_state.active_clients.iter().copied().collect(),
                                ))
                            }
                            _ => None,
                        };
                        let auto_release = outcome.auto_release;
                        drop(capture_state);
                        if let Some(pos) = auto_release {
                            if lifecycle_event_tx
                                .send((pos, CaptureEvent::AutoRelease))
                                .await
                                .is_err()
                            {
                                log::debug!(
                                    "outer capture task exited before tap interruption was delivered"
                                );
                            }
                        }
                        if let Some((next, positions)) = transition {
                            publish_host_input_state(&lifecycle_event_tx, positions, next).await;
                        }
                        if let Some(generation) = tap_recovery_generation {
                            let mut state = state.lock().await;
                            if complete_event_tap_recovery(
                                &mut state.tap_recovery_pending,
                                generation,
                            ) {
                                log::debug!(
                                    "completed CGEventTap recovery generation {generation}"
                                );
                            }
                        }
                    }
                    _ = bounds_poll.tick() => {
                        let mut state = state.lock().await;
                        state.retry_show_cursor_if_unowned("periodic lifecycle poll");
                        let prev = state.bounds;
                        match state.update_bounds() {
                            Ok(()) if state.bounds != prev => log::info!(
                                "display geometry changed (poll): {prev:?} -> {:?}",
                                state.bounds,
                            ),
                            Ok(()) => {}
                            Err(e) => log::warn!("periodic bounds refresh failed: {e}"),
                        }
                        // Poll as a safety net for a missed secure-input tap
                        // disable or distributed unlock notification. This is
                        // the same authoritative WindowServer state used by
                        // the immediate callbacks, sampled only every two
                        // seconds rather than on the hot input path.
                        let observed = if is_screen_locked() {
                            HostInputState::Locked
                        } else {
                            HostInputState::Unlocked
                        };
                        let transition = (observed != last_host_input_state).then(|| {
                            last_host_input_state = observed;
                            (observed, state.active_clients.iter().copied().collect())
                        });
                        drop(state);
                        if let Some((next, positions)) = transition {
                            publish_host_input_state(&lifecycle_event_tx, positions, next).await;
                        }
                    }
                    _ = &mut tap_exit_rx => break,
                }
            }
            // show cursor
            let _ = CGDisplay::show_cursor(&CGDisplay::main());
        });

        Ok(Self {
            event_rx,
            notify_tx,
            run_loop,
        })
    }
}

fn request_macos_capture_permissions() -> Result<(), MacosCaptureCreationError> {
    check_macos_capture_permissions(
        request_accessibility_permission,
        request_input_monitoring_permission,
    )
}

fn check_macos_capture_permissions<A, I>(
    accessibility_granted: A,
    input_monitoring_granted: I,
) -> Result<(), MacosCaptureCreationError>
where
    A: FnOnce() -> bool,
    I: FnOnce() -> bool,
{
    // The GUI owns the explicit user-visible Accessibility prompt. Do not touch
    // the CoreGraphics permission API until Accessibility is present: on
    // macOS it can route through the same authorization helper and queue an
    // additional request alongside the GUI's intentional one.
    if !accessibility_granted() {
        return Err(MacosCaptureCreationError::AccessibilityPermission);
    }
    if !input_monitoring_granted() {
        return Err(MacosCaptureCreationError::InputMonitoringPermission);
    }
    Ok(())
}

#[cfg(test)]
mod permission_tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn skips_input_monitoring_check_without_accessibility() {
        let input_monitoring_checked = Cell::new(false);

        let result = check_macos_capture_permissions(
            || false,
            || {
                input_monitoring_checked.set(true);
                true
            },
        );

        assert!(matches!(
            result,
            Err(MacosCaptureCreationError::AccessibilityPermission)
        ));
        assert!(!input_monitoring_checked.get());
    }

    #[test]
    fn recognizes_only_the_shared_keep_awake_event_tag() {
        let source = CGEventSource::new(CGEventSourceStateID::Private).expect("event source");
        let event = CGEvent::new(source).expect("event");

        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            MACOS_KEEP_AWAKE_EVENT_TAG,
        );
        assert!(is_mousehop_keep_awake_event(&event));

        let untagged_source =
            CGEventSource::new(CGEventSourceStateID::Private).expect("untagged event source");
        let untagged = CGEvent::new(untagged_source).expect("untagged event");
        assert!(!is_mousehop_keep_awake_event(&untagged));
    }
}

#[cfg(test)]
mod event_tap_tests {
    use super::*;

    #[test]
    fn disabled_tap_transition_clears_capture_and_reports_it_exactly_once() {
        let mut current_pos = Some(Position::Bottom);
        let mut generation = 0;
        let mut pending = None;

        let first = event_tap_disabled_transition(&mut current_pos, &mut generation, &mut pending);
        assert_eq!(current_pos, None);
        assert_eq!(generation, 1);
        assert_eq!(
            pending,
            Some(TapRecovery {
                generation: 1,
                interrupted_pos: Some(Position::Bottom),
                auto_release_claimed: false,
            })
        );
        assert!(matches!(
            first,
            ProducerEvent::EventTapDisabled {
                recovery_generation: 1,
                interrupted_pos: Some(Position::Bottom)
            }
        ));

        // A second disabled notification before another Begin must not emit a
        // duplicate AutoRelease for capture ownership already surrendered, but
        // it does supersede the first recovery gate.
        let second = event_tap_disabled_transition(&mut current_pos, &mut generation, &mut pending);
        assert_eq!(generation, 2);
        assert_eq!(
            pending,
            Some(TapRecovery {
                generation: 2,
                interrupted_pos: Some(Position::Bottom),
                auto_release_claimed: false,
            })
        );
        assert!(matches!(
            second,
            ProducerEvent::EventTapDisabled {
                recovery_generation: 2,
                interrupted_pos: Some(Position::Bottom)
            }
        ));

        // Completing the older producer event must not reopen crossing while
        // the newer disabled notification is still queued.
        assert!(!complete_event_tap_recovery(&mut pending, 1));
        assert_eq!(
            pending.as_ref().map(|recovery| recovery.generation),
            Some(2)
        );
        assert!(complete_event_tap_recovery(&mut pending, 2));
        assert_eq!(pending, None);
    }

    #[test]
    fn disabled_tap_generation_never_uses_zero_after_wrap() {
        let mut current_pos = None;
        let mut generation = u64::MAX;
        let mut pending = None;

        let event = event_tap_disabled_transition(&mut current_pos, &mut generation, &mut pending);

        assert_eq!(generation, 1);
        assert_eq!(
            pending.as_ref().map(|recovery| recovery.generation),
            Some(1)
        );
        assert!(matches!(
            event,
            ProducerEvent::EventTapDisabled {
                recovery_generation: 1,
                interrupted_pos: None
            }
        ));
    }

    #[test]
    fn queued_grab_is_folded_into_the_shared_recovery_gate() {
        let mut current_pos = None;
        let mut generation = 0;
        let mut pending = None;
        let _ = event_tap_disabled_transition(&mut current_pos, &mut generation, &mut pending);

        assert!(defer_grab_during_tap_recovery(&mut pending, Position::Left));
        assert_eq!(
            pending,
            Some(TapRecovery {
                generation: 1,
                interrupted_pos: Some(Position::Left),
                auto_release_claimed: false,
            })
        );
        assert_eq!(
            claim_tap_recovery_auto_release(&mut pending, 1),
            TapRecoveryClaim::AutoRelease(Position::Left)
        );
        assert_eq!(
            claim_tap_recovery_auto_release(&mut pending, 1),
            TapRecoveryClaim::SupersededOrClaimed
        );

        // A second disabled notification racing with publication carries the
        // claim forward, so its newer generation cannot publish a duplicate.
        let _ = event_tap_disabled_transition(&mut current_pos, &mut generation, &mut pending);
        assert_eq!(
            claim_tap_recovery_auto_release(&mut pending, 2),
            TapRecoveryClaim::SupersededOrClaimed
        );

        let mut no_recovery = None;
        assert!(!defer_grab_during_tap_recovery(
            &mut no_recovery,
            Position::Right
        ));
    }
}

#[cfg(test)]
mod modifier_tests {
    use super::*;
    use scancode::Linux::{
        KeyCapsLock, KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift, KeyRightCtrl,
        KeyRightShift, KeyRightalt, KeyRightmeta,
    };

    fn flags(bits: u64) -> CGEventFlags {
        CGEventFlags::from_bits_retain(bits)
    }

    #[test]
    fn crossing_preflight_is_bypassed_when_the_gate_is_disabled() {
        assert!(crossing_preflight_allows(None, CGEventFlags::empty()));
        assert!(!crossing_preflight_allows(
            Some(CrossingModifier::Control),
            CGEventFlags::empty()
        ));
        assert!(crossing_preflight_allows(
            Some(CrossingModifier::Control),
            CGEventFlags::CGEventFlagControl
        ));
    }

    #[test]
    fn device_flags_report_each_physical_modifier_key_independently() {
        let cases = [
            (KeyLeftCtrl, NX_DEVICE_LCTRL_KEY_MASK),
            (KeyLeftShift, NX_DEVICE_LSHIFT_KEY_MASK),
            (KeyRightShift, NX_DEVICE_RSHIFT_KEY_MASK),
            (KeyLeftMeta, NX_DEVICE_LCOMMAND_KEY_MASK),
            (KeyRightmeta, NX_DEVICE_RCOMMAND_KEY_MASK),
            (KeyLeftAlt, NX_DEVICE_LALT_KEY_MASK),
            (KeyRightalt, NX_DEVICE_RALT_KEY_MASK),
            (KeyRightCtrl, NX_DEVICE_RCTRL_KEY_MASK),
        ];

        for (key, device_mask) in cases {
            assert_eq!(
                modifier_key_state(key as u32, flags(device_mask)),
                1,
                "{key:?} should be pressed when its device bit is set",
            );
            assert_eq!(
                modifier_key_state(key as u32, flags(0)),
                0,
                "{key:?} should be released when its device bit is clear",
            );
        }
    }

    #[test]
    fn one_shift_can_be_released_while_the_other_remains_pressed() {
        let right_only = flags(NX_DEVICE_RSHIFT_KEY_MASK | CGEventFlags::CGEventFlagShift.bits());

        assert_eq!(modifier_key_state(KeyLeftShift as u32, right_only), 0,);
        assert_eq!(modifier_key_state(KeyRightShift as u32, right_only), 1,);
    }

    #[test]
    fn aggregate_flags_do_not_turn_a_released_key_into_a_press() {
        let aggregate_shift_only = CGEventFlags::CGEventFlagShift;

        assert_eq!(
            modifier_key_state(KeyLeftShift as u32, aggregate_shift_only),
            0,
        );
    }

    #[test]
    fn crossing_snapshot_emits_every_held_side_then_aggregate_modifiers() {
        let all_device_bits = STANDARD_MODIFIER_KEYS
            .iter()
            .fold(0, |bits, (_, mask)| bits | mask);
        let aggregate_bits = CGEventFlags::CGEventFlagShift.bits()
            | CGEventFlags::CGEventFlagControl.bits()
            | CGEventFlags::CGEventFlagAlternate.bits()
            | CGEventFlags::CGEventFlagCommand.bits()
            | CGEventFlags::CGEventFlagAlphaShift.bits();
        let snapshot = modifier_snapshot(flags(all_device_bits | aggregate_bits));

        let keys = snapshot[..STANDARD_MODIFIER_KEYS.len()]
            .iter()
            .map(|event| match event {
                CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    key, state: 1, ..
                })) => scancode::Linux::try_from(*key).expect("known modifier"),
                other => panic!("expected held modifier key-down, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            STANDARD_MODIFIER_KEYS
                .iter()
                .map(|(key, _)| *key)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            snapshot.last(),
            Some(&CaptureEvent::Input(Event::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: (XMods::ShiftMask
                        | XMods::ControlMask
                        | XMods::Mod1Mask
                        | XMods::Mod4Mask)
                        .bits(),
                    latched: 0,
                    locked: XMods::LockMask.bits(),
                    group: 0,
                }
            )))
        );
    }

    #[test]
    fn crossing_snapshot_reports_both_shifts_without_duplicating_the_mask() {
        let snapshot = modifier_snapshot(flags(
            NX_DEVICE_LSHIFT_KEY_MASK
                | NX_DEVICE_RSHIFT_KEY_MASK
                | CGEventFlags::CGEventFlagShift.bits(),
        ));

        assert_eq!(snapshot.len(), 3);
        assert_eq!(
            snapshot[0],
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: KeyLeftShift as u32,
                state: 1,
            }))
        );
        assert_eq!(
            snapshot[1],
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: KeyRightShift as u32,
                state: 1,
            }))
        );
        assert_eq!(
            snapshot[2],
            CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: XMods::ShiftMask.bits(),
                latched: 0,
                locked: 0,
                group: 0,
            }))
        );
    }

    #[test]
    fn caps_lock_emits_a_down_up_pair_on_both_lock_cycles() {
        assert_eq!(
            modifier_key_states(KeyCapsLock as u32, CGEventFlags::CGEventFlagAlphaShift),
            ([1, 0], 2),
        );
        assert_eq!(
            modifier_key_states(KeyCapsLock as u32, CGEventFlags::empty()),
            ([1, 0], 2),
        );
    }

    #[test]
    fn standard_modifier_still_emits_one_phase() {
        assert_eq!(
            modifier_key_states(KeyLeftCtrl as u32, flags(NX_DEVICE_LCTRL_KEY_MASK),),
            ([1, 0], 1),
        );
    }

    fn stepped_mac_layout() -> DisplayLayout {
        DisplayLayout::new([(0, 0, 3072, 1728), (-1728, 0, 1728, 1117)])
    }

    #[test]
    fn crossing_uses_the_exposed_contour_not_the_union_rectangle() {
        let layout = stepped_mac_layout();
        assert_eq!(layout.origin(), Some((-1728, 0)));
        assert_eq!(layout.size(), Some((4800, 1728)));

        // Above the built-in display's bottom, x=0 is an internal seam and
        // must not capture. Below it, x=0 is the main display's true left edge.
        assert!(!crosses_display_contour(
            &layout,
            Position::Left,
            (0.0, 500.0),
            (-1.0, 0.0),
        ));
        assert!(crosses_display_contour(
            &layout,
            Position::Left,
            (0.0, 1500.0),
            (-1.0, 0.0),
        ));
        assert!(crosses_display_contour(
            &layout,
            Position::Left,
            (-1728.0, 500.0),
            (-1.0, 0.0),
        ));

        // Merely moving parallel while parked on an edge is not a crossing.
        assert!(!crosses_display_contour(
            &layout,
            Position::Left,
            (-1728.0, 500.0),
            (0.0, 20.0),
        ));

        // Quartz locations are post-delta. Predicting another move from a
        // near-edge event would capture before the cursor actually reaches
        // the main display's exposed lower step.
        assert!(!crosses_display_contour(
            &layout,
            Position::Left,
            (10.0, 1120.0),
            (-20.0, -20.0),
        ));
        // Once that diagonal move lands on the built-in display, its left
        // contour is the built-in's outer edge rather than the internal seam.
        assert!(!crosses_display_contour(
            &layout,
            Position::Left,
            (-10.0, 1100.0),
            (-20.0, -20.0),
        ));

        assert!(!crosses_display_contour(
            &layout,
            Position::Right,
            (3066.0, 500.0),
            (10.0, 0.0),
        ));
        assert!(crosses_display_contour(
            &layout,
            Position::Right,
            (3071.0, 500.0),
            (10.0, 0.0),
        ));
    }

    #[test]
    fn capture_anchor_tracks_each_step_of_the_mac_contour() {
        let layout = stepped_mac_layout();
        assert_eq!(
            capture_anchor(&layout, Position::Left, (-1728.0, 500.0)),
            Some((-1727.0, 500.0)),
        );
        assert_eq!(
            capture_anchor(&layout, Position::Left, (0.0, 1500.0)),
            Some((1.0, 1500.0)),
        );
        assert_eq!(
            capture_anchor(&layout, Position::Bottom, (-1000.0, 1116.0)),
            Some((-1000.0, 1116.0)),
        );
        assert_eq!(
            capture_anchor(&layout, Position::Bottom, (1000.0, 1727.0)),
            Some((1000.0, 1727.0)),
        );
    }

    #[test]
    fn active_capture_anchor_is_reprojected_after_display_reconfiguration() {
        let old_layout = DisplayLayout::new([(0, 0, 1920, 1080)]);
        let mut state = InputCaptureState {
            active_clients: Lazy::new(HashSet::new),
            crossing_modifiers: HashMap::new(),
            current_pos: Some(Position::Right),
            cursor_hidden: true,
            tap_recovery_generation: 0,
            tap_recovery_pending: None,
            enter_position: Some(CGPoint {
                x: 1919.0,
                y: 900.0,
            }),
            bounds: layout_bounds(&old_layout).expect("old bounds"),
            display_layout: old_layout,
        };

        // The old display was replaced by a smaller display shifted right.
        // The prior hidden anchor is now outside every screen, so both axes
        // must be projected onto the new right contour.
        let new_layout = DisplayLayout::new([(100, 0, 1000, 700)]);
        assert!(state.commit_display_layout(new_layout));

        let anchor = state.enter_position.expect("reprojected anchor");
        assert_eq!((anchor.x, anchor.y), (1099.0, 699.0));
        assert_eq!(
            state.bounds,
            Bounds {
                xmin: 100.0,
                xmax: 1100.0,
                ymin: 0.0,
                ymax: 700.0,
            }
        );
    }
}

fn request_accessibility_permission() -> bool {
    // Silent check. The GUI owns the one-time user-visible prompt at
    // startup (see mousehop_gtk::macos_privacy) so retries triggered by
    // clicking the "Reenable" button don't pop a fresh Accessibility
    // alert every time.
    unsafe { AXIsProcessTrusted() }
}

fn request_input_monitoring_permission() -> bool {
    // Silent check, same reasoning as above.
    unsafe { CGPreflightListenEventAccess() }
}

impl Drop for MacOSInputCapture {
    fn drop(&mut self) {
        self.run_loop.stop();
    }
}

#[async_trait]
impl Capture for MacOSInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        log::debug!("creating capture, {pos}");
        self.notify_tx
            .send(ProducerEvent::Create(pos))
            .await
            .map_err(|_| CaptureError::CaptureUpdatesClosed)?;
        log::debug!("done !");
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        log::debug!("destroying capture {pos}");
        self.notify_tx
            .send(ProducerEvent::Destroy(pos))
            .await
            .map_err(|_| CaptureError::CaptureUpdatesClosed)?;
        log::debug!("done !");
        Ok(())
    }

    async fn release(&mut self, warp_target: Option<(i32, i32)>) -> Result<(), CaptureError> {
        log::info!("[release-warp] macOS backend release(warp_target={warp_target:?})");
        log::debug!("notifying Release");
        self.notify_tx
            .send(ProducerEvent::Release { warp_target })
            .await
            .map_err(|_| CaptureError::CaptureUpdatesClosed)?;
        Ok(())
    }

    async fn set_crossing_modifier(
        &mut self,
        pos: Position,
        modifier: Option<CrossingModifier>,
    ) -> Result<(), CaptureError> {
        self.notify_tx
            .send(ProducerEvent::SetCrossingModifier(pos, modifier))
            .await
            .map_err(|_| CaptureError::CaptureUpdatesClosed)
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn display_bounds(&self) -> Option<(u32, u32)> {
        self.display_layout()?.size()
    }

    fn display_origin(&self) -> (i32, i32) {
        // Top-left of the union of all active displays. Matters when
        // a secondary monitor is positioned LEFT of (or ABOVE) the
        // primary — the global pointer-coordinate system is anchored
        // at the primary's top-left, so a left-attached external
        // gives cursor x ∈ [-w, 0). Without this offset,
        // host_normalized_cursor / peer_warp_target's clamp(0, 1)
        // silently maps every point on the external to "left edge"
        // and the receiver warps to the wrong column.
        self.display_layout()
            .and_then(|layout| layout.origin())
            .unwrap_or((0, 0))
    }

    fn display_layout(&self) -> Option<DisplayLayout> {
        let layout = query_display_layout().ok()?;
        (!layout.is_empty()).then_some(layout)
    }
}

impl Stream for MacOSInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}

type CGSConnectionID = u32;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGSSetConnectionProperty(
        cid: CGSConnectionID,
        targetCID: CGSConnectionID,
        key: CFStringRef,
        value: CFBooleanRef,
    ) -> CGError;
    fn _CGSDefaultConnection() -> CGSConnectionID;
}

type CFDictionaryRef = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFBooleanGetValue(boolean: CFBooleanRef) -> bool;

    /// The system-wide distributed notification center. Carries
    /// cross-process notifications such as `com.apple.screenIsLocked`
    /// and `com.apple.screenIsUnlocked`.
    fn CFNotificationCenterGetDistributedCenter() -> *mut c_void;
    /// Register `callback` for `name`, delivered on the run loop of
    /// the registering thread. `observer` is an opaque key reused by
    /// the removal call and handed back to the callback.
    fn CFNotificationCenterAddObserver(
        center: *mut c_void,
        observer: *const c_void,
        callback: extern "C" fn(
            *mut c_void,
            *mut c_void,
            CFStringRef,
            *const c_void,
            CFDictionaryRef,
        ),
        name: CFStringRef,
        object: *const c_void,
        suspension_behavior: isize,
    );
    fn CFNotificationCenterRemoveEveryObserver(center: *mut c_void, observer: *const c_void);
}

extern "C" {
    fn CGEventSourceSetLocalEventsSuppressionInterval(
        event_source: CGEventSource,
        seconds: CFTimeInterval,
    );
    fn CGPreflightListenEventAccess() -> bool;
    /// Re-enable an event tap that was disabled by a
    /// `kCGEventTapDisabledByTimeout` event. The Apple-documented
    /// recovery path: see Quartz Event Services Reference. The `tap`
    /// argument is a `CFMachPortRef`; we pass the raw pointer so we
    /// can store it as `usize` for cross-thread sharing.
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);

    /// Register a callback invoked when the display configuration
    /// changes (monitor add/remove, resolution change, mirror,
    /// rearrange, etc). See Quartz Display Services Reference.
    fn CGDisplayRegisterReconfigurationCallback(
        callback: extern "C" fn(u32, u32, *mut c_void),
        user_info: *mut c_void,
    ) -> CGError;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: extern "C" fn(u32, u32, *mut c_void),
        user_info: *mut c_void,
    ) -> CGError;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    /// Register the calling process for system-power notifications.
    /// Returns the `io_connect_t` root power port (used later in
    /// `IOAllowPowerChange` to ack sleep-related messages) and writes
    /// the notification port + an `io_object_t` notifier through the
    /// out-pointers. The returned notification port carries a
    /// CFRunLoopSource we attach to this thread's run loop so the
    /// callback fires inline with the existing event-tap loop.
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port_ref: *mut *mut c_void,
        callback: extern "C" fn(*mut c_void, u32, u32, *mut c_void),
        notifier: *mut u32,
    ) -> u32;
    fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
    fn IONotificationPortGetRunLoopSource(notify: *mut c_void) -> CFRunLoopSourceRef;
    fn IONotificationPortDestroy(notify: *mut c_void);
    /// Ack a kIOMessageCanSystemSleep / kIOMessageSystemWillSleep so
    /// the OS doesn't stall on its 30s default timeout waiting for us.
    /// Required even when we have no objection — silence is treated as
    /// "still thinking" by the kernel.
    fn IOAllowPowerChange(kernel_port: u32, notification_id: isize) -> i32;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Read `com.apple.swipescrolldirection` from the global preferences
/// domain. Returns `true` when Natural Scrolling is enabled (the
/// modern macOS default) — the same default macOS uses if the key
/// is unset. Used to decide whether to invert scroll deltas before
/// forwarding them to a peer that has its own fixed convention.
fn natural_scrolling_enabled() -> bool {
    unsafe {
        let key_cstr = CString::new("com.apple.swipescrolldirection").unwrap();
        let key = CFStringCreateWithCString(
            kCFAllocatorDefault,
            key_cstr.as_ptr() as *const c_char,
            kCFStringEncodingUTF8,
        );
        if key.is_null() {
            return true;
        }
        let value = CFPreferencesCopyAppValue(key, kCFPreferencesAnyApplication);
        CFRelease(key as *const c_void);
        if value.is_null() {
            // Key absent → modern macOS default is enabled.
            return true;
        }
        // The preference is stored as a CFBoolean; kCFBooleanTrue
        // and kCFBooleanFalse are singleton instances, so a pointer
        // compare is correct and sufficient.
        let is_true = (value as CFBooleanRef) == kCFBooleanTrue;
        CFRelease(value);
        is_true
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFPreferencesCopyAppValue(key: CFStringRef, application_id: CFStringRef) -> *const c_void;
    static kCFPreferencesAnyApplication: CFStringRef;
}

unsafe fn configure_cf_settings() -> Result<(), MacosCaptureCreationError> {
    // When we warp the cursor using CGWarpMouseCursorPosition local events are suppressed for a short time
    // this leeds to the cursor not flowing when crossing back from a clinet, set this to to 0 stops the warp
    // from working, set a low value by trial and error, 0.05s seems good. 0.25s is the default
    let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|_| MacosCaptureCreationError::EventSourceCreation)?;
    CGEventSourceSetLocalEventsSuppressionInterval(event_source, 0.05);
    // FIXME Memory Leak

    // This is a private settings that allows the cursor to be hidden while in the background.
    // It is used by Barrier and other apps.
    let key = CString::new("SetsCursorInBackground").unwrap();
    let cf_key = CFStringCreateWithCString(
        kCFAllocatorDefault,
        key.as_ptr() as *const c_char,
        kCFStringEncodingUTF8,
    );
    if CGSSetConnectionProperty(
        _CGSDefaultConnection(),
        _CGSDefaultConnection(),
        cf_key,
        kCFBooleanTrue,
    ) != kCGErrorSuccess
    {
        return Err(MacosCaptureCreationError::CGCursorProperty);
    }
    CFRelease(cf_key as *const c_void);
    Ok(())
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}
