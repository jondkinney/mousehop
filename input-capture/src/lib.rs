use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    future::Future,
    mem::swap,
    pin::Pin,
    task::{Poll, ready},
    time::Duration,
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;
use tokio::time::Sleep;

use input_event::{
    CrossingModifier, Event, KeyboardEvent, PointerEvent,
    display::{DisplayEdge, DisplayLayout},
    scancode,
};

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod clipboard;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod desktop_entries;
pub mod error;
pub mod frontmost_app;

#[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

/// Confirmed availability of input on the capturing host. Backends emit this
/// only when the operating system exposes an authoritative state transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostInputState {
    Unlocked,
    Locked,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// Capture on this handle is now active. `cursor`, when present,
    /// is the host's screen-space cursor position (in pixels) at the
    /// instant of the edge crossing — the capture loop normalizes it
    /// against the host's display bounds and forwards it to the peer
    /// as a [`ProtoEvent::CursorPos`] so the guest's cursor lands at
    /// the visually-corresponding point on its own screen. Backends
    /// that can't report cursor position emit `None`; the peer's
    /// cursor stays where it was on remote-takeover (no forced
    /// midpoint warp — that masquerades as a mid-screen crossing on
    /// fast re-crosses). `normalized_cursor` is computed by the backend from
    /// the same display snapshot that produced the edge crossing, closing the
    /// hotplug race between its event thread and the outer capture task.
    Begin {
        cursor: Option<(i32, i32)>,
        normalized_cursor: Option<(f32, f32)>,
    },
    /// input event coming from capture handle
    Input(Event),
    /// the capture wrapper detected sustained back-toward-host motion
    /// past the configured threshold (the user has pinned the cursor
    /// at the host-adjacent edge of the guest and kept pushing). The
    /// capture loop should treat this like a release-bind chord.
    AutoRelease,
    /// The host locked or unlocked. This is a host-wide lifecycle event, so a
    /// backend may route it through every registered capture position; the
    /// higher-level capture task is responsible for de-duplicating it.
    HostInputState(HostInputState),
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin { cursor: None, .. } => write!(f, "begin capture"),
            CaptureEvent::Begin {
                cursor: Some((x, y)),
                ..
            } => write!(f, "begin capture @ ({x}, {y})"),
            CaptureEvent::Input(e) => write!(f, "{e}"),
            CaptureEvent::AutoRelease => write!(f, "auto-release"),
            CaptureEvent::HostInputState(state) => write!(f, "host input {state:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
    InputCapturePortal,
    #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
    LayerShell,
    #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position
    id_map: HashMap<CaptureHandle, Position>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
    /// pixel threshold for the cross-platform auto-release-on-wall-
    /// press fallback. 0 disables. See `track_wall_press`.
    release_threshold_px: u32,
    /// position the cursor is currently captured into, if any. Tracks
    /// `Begin`/release transitions so the wall-press accumulator
    /// resets correctly across capture sessions.
    capture_pos: Option<Position>,
    /// True once the backend has already relinquished the current capture.
    /// Backend-originated AutoRelease events (focus loss, portal rebuild,
    /// macOS tap interruption) arrive after that local teardown; the outer
    /// task still calls `release()` to flush network state, but must not queue
    /// a second backend release that can race a newly-created capture.
    backend_capture_released: bool,
    /// Modeled cursor position on the guest along the entry axis,
    /// relative to the host-adjacent edge. 0 = at the entry edge,
    /// growing values = further into the guest. Clamped at 0 from
    /// below; clamped at the cached peer extent from above when
    /// available, otherwise unbounded (degraded fallback).
    virtual_pos: f64,
    /// Pixels of back-toward-host motion that the modeled cursor
    /// could not absorb (proposed virtual_pos < 0). Resets whenever
    /// the cursor is back in the interior or moving deeper.
    wall_pressure: f64,
    /// Modeled guest cursor position in the guest's screen space,
    /// updated by accumulating Motion deltas while captured. Seeded
    /// on `Begin` from the cross-axis warp target (if peer bounds
    /// are known) or the entry-edge midpoint otherwise — i.e. wherever
    /// the guest's cursor visually lands at Enter. Read on release
    /// to compute a host-side warp so the local cursor reappears at
    /// the matching point on the host's screen instead of jumping
    /// back to where capture started.
    virtual_cursor: Option<(f64, f64)>,
    /// Host-coord cursor at the moment of `Begin`, retained until
    /// `peer_bounds` arrives so we can retroactively seed
    /// `virtual_cursor` once the round-trip completes. Without this,
    /// a `Begin` that fires before the peer's `Bounds` reply leaves
    /// `virtual_cursor` stuck at `None` for the rest of the session
    /// — the wall-press accumulator skips updates and the
    /// release-time warp falls back to the original crossing
    /// y-value instead of where the cursor visually was on the peer.
    pending_begin_cursor: Option<(i32, i32)>,
    /// Normalized host cursor from the exact backend topology snapshot that
    /// produced Begin. This remains authoritative if the host display layout
    /// changes before receiver metadata arrives.
    pending_begin_normalized_cursor: Option<(f32, f32)>,
    /// Motion deltas that arrived while `virtual_cursor` was still
    /// `None` (between `Begin` and the late-arriving
    /// `set_peer_bounds`). Drained into the freshly-seeded
    /// `virtual_cursor` when the bootstrap completes so deltas
    /// during the round-trip aren't lost.
    pending_motion: (f64, f64),
    /// Receiver-scaled net motion since the current `Begin`. Geometry usually
    /// arrives before Ack, but retaining this lets a first full-topology reply
    /// re-seed the cursor after a legacy Bounds reply without losing the small
    /// amount of motion captured between those packets.
    motion_since_begin: (f64, f64),
    /// Per-position cache of peer display geometry. Populated when
    /// the peer responds with a `ProtoEvent::Bounds` event after
    /// Ack. Used as the upper clamp for `virtual_pos` so that
    /// pushing past the guest's actual far edge doesn't make the
    /// model run away. Only the entry-axis dimension is consulted.
    peer_bounds: HashMap<Position, (u32, u32)>,
    /// Full per-position peer topology, when supplied by a current peer. The
    /// bounding size remains cached separately for compatibility with older
    /// versions; this layout keeps the virtual guest cursor out of empty union
    /// space and makes one-way return warps follow the screen the cursor could
    /// actually occupy.
    peer_layouts: HashMap<Position, DisplayLayout>,
    /// Positions whose cached topology was confirmed after the current
    /// `Begin`. A retained layout is deliberately hidden at each new capture
    /// until that peer republishes it; otherwise a legacy peer's same-size
    /// Bounds could accidentally reactivate stale monitor contours.
    confirmed_peer_layouts: HashSet<Position>,
    /// Wrapping monotonic generation paired with each authoritative topology.
    /// Prevents an older hotplug datagram reordered behind a newer one from
    /// regressing the cursor model until the next periodic refresh.
    peer_layout_generations: HashMap<Position, (u64, u32)>,
    /// Per-position cached sensitivity multiplier the receiver
    /// applies to forwarded motion deltas before injection. Sent
    /// by the receiving peer via [`ProtoEvent::ReceiverSensitivity`]
    /// after Ack-on-Enter. The host's wall-press auto-release model
    /// scales each delta by this value so its model of "where the
    /// receiver's cursor would be" stays in sync with the receiver's
    /// actual cursor — without it, a sub-1.0 multiplier would let
    /// `wall_pressure` accumulate faster than reality and trigger
    /// AutoRelease before the receiver hit the wall. Default is
    /// 1.0; old peers that don't send this event leave the entry
    /// unset, matching the previous behavior.
    peer_sensitivity: HashMap<Position, f64>,
    /// True when wall_pressure has crossed `release_threshold_px` and
    /// `wall_press_timer` has been armed but not yet either elapsed
    /// or been cancelled. Cleared when the peer's handover Leave
    /// arrives (which routes through `release_no_host_warp` →
    /// `reset_wall_press_state`) or when the cursor moves back into
    /// the interior. The wall-press auto-release fires only after
    /// `wall_press_deadline` elapses without this being cleared —
    /// turning the historically race-y "wall-press vs peer-Leave"
    /// into an explicit fallback that only kicks in when the peer
    /// can't deliver a Leave (lock screen, restricted DE, dead peer).
    wall_press_pending: bool,
    /// Window after the threshold is crossed during which a peer
    /// Leave can cancel the deferred AutoRelease. Sized so a
    /// healthy LAN round-trip beats it comfortably.
    wall_press_deadline: Duration,
    /// Timer driving the deferred fire. Reset to deadline-from-now
    /// on first threshold crossing; polled in `poll_next` so the
    /// fire happens even when no further backend events arrive
    /// (the user pinned the cursor against the wall and stopped).
    wall_press_timer: Pin<Box<Sleep>>,
}

/// Project a motion delta onto the entry axis. Positive return =
/// "into guest", so virtual_pos increases as the user pushes deeper.
fn entry_axis_delta(position: Position, dx: f64, dy: f64) -> f64 {
    match position {
        // Position::Left = guest is to the LEFT of host. User entered
        // by moving left (-dx). Convention: positive = into guest.
        Position::Left => -dx,
        Position::Right => dx,
        Position::Top => -dy,
        Position::Bottom => dy,
    }
}

fn scale_motion(dx: f64, dy: f64, sensitivity: f64) -> (f64, f64) {
    (dx * sensitivity, dy * sensitivity)
}

pub(crate) fn normalize_cursor_in_layout(
    layout: &DisplayLayout,
    cursor: (i32, i32),
) -> Option<(f32, f32)> {
    let bounds = layout.bounds()?;
    let (host_w, host_h) = bounds.size();
    let (origin_x, origin_y) = bounds.origin();
    let (cx, cy) = cursor;
    let nx = ((cx - origin_x) as f32 / host_w as f32).clamp(0.0, 1.0);
    let ny = ((cy - origin_y) as f32 / host_h as f32).clamp(0.0, 1.0);
    Some((nx, ny))
}

fn wrapping_generation_is_newer(current: u32, candidate: u32) -> bool {
    candidate != current && candidate.wrapping_sub(current) < (1 << 31)
}

fn topology_version_is_newer(current: (u64, u32), candidate: (u64, u32)) -> bool {
    // An epoch identifies one receiver process, not an ordered clock value.
    // Wall-clock correction can make a restarted process choose a numerically
    // smaller epoch. Transport-session filtering excludes frames from the old
    // process, so any different epoch on the current session is authoritative;
    // only generations inside one epoch need wrapping-order comparison.
    candidate.0 != current.0 || wrapping_generation_is_newer(current.1, candidate.1)
}

fn display_edge(position: Position) -> DisplayEdge {
    match position {
        Position::Left => DisplayEdge::Left,
        Position::Right => DisplayEdge::Right,
        Position::Top => DisplayEdge::Top,
        Position::Bottom => DisplayEdge::Bottom,
    }
}

/// The host-adjacent contour on the peer. `Position` is expressed from the
/// host's frame, so a peer to the right is entered through its left edge.
fn peer_entry_edge(position: Position) -> DisplayEdge {
    match position {
        Position::Left => DisplayEdge::Right,
        Position::Right => DisplayEdge::Left,
        Position::Top => DisplayEdge::Bottom,
        Position::Bottom => DisplayEdge::Top,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TopologyMotion {
    /// Union-relative receiver cursor after applying/clamping the delta.
    cursor: (f64, f64),
    /// Distance from the receiver cursor to the current host-adjacent contour.
    entry_distance: f64,
    /// Unabsorbed motion beyond that contour during this event.
    wall_overshoot: f64,
}

fn model_topology_motion(
    layout: &DisplayLayout,
    position: Position,
    current_relative: (f64, f64),
    dx: f64,
    dy: f64,
) -> Option<TopologyMotion> {
    let bounds = layout.bounds()?;
    let (origin_x, origin_y) = bounds.origin();
    let origin = (f64::from(origin_x), f64::from(origin_y));
    let current = (origin.0 + current_relative.0, origin.1 + current_relative.1);
    let raw_target = (current.0 + dx, current.1 + dy);
    let cursor = layout.clamp_to_nearest_display(raw_target)?;
    let cursor_pixel = (cursor.0.floor() as i32, cursor.1.floor() as i32);
    let edge = peer_entry_edge(position);
    let contour = layout.project_point(edge, cursor_pixel)?;
    let contour_axis = match edge {
        DisplayEdge::Left | DisplayEdge::Right => f64::from(contour.0),
        DisplayEdge::Top | DisplayEdge::Bottom => f64::from(contour.1),
    };
    let (entry_distance, wall_overshoot) = match edge {
        DisplayEdge::Left => (cursor.0 - contour_axis, contour_axis - raw_target.0),
        DisplayEdge::Right => (contour_axis - cursor.0, raw_target.0 - contour_axis),
        DisplayEdge::Top => (cursor.1 - contour_axis, contour_axis - raw_target.1),
        DisplayEdge::Bottom => (contour_axis - cursor.1, raw_target.1 - contour_axis),
    };

    Some(TopologyMotion {
        cursor: (cursor.0 - origin.0, cursor.1 - origin.1),
        entry_distance: entry_distance.max(0.0),
        wall_overshoot: wall_overshoot.max(0.0),
    })
}

impl InputCapture {
    /// create a new client with the given id
    pub async fn create(&mut self, id: CaptureHandle, pos: Position) -> Result<(), CaptureError> {
        assert!(!self.id_map.contains_key(&id));

        self.id_map.insert(id, pos);

        if let Some(v) = self.position_map.get_mut(&pos) {
            v.push(id);
            Ok(())
        } else {
            self.position_map.insert(pos, vec![id]);
            self.capture.create(pos).await
        }
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let pos = self
            .id_map
            .remove(&id)
            .expect("no position for this handle");

        log::debug!("destroying capture {id} @ {pos}");
        let remaining = self.position_map.get_mut(&pos).expect("id vector");
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {pos}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {pos} - no remaining ids");
            self.position_map.remove(&pos);
            self.capture.destroy(pos).await?;
        }
        Ok(())
    }

    /// Configure an optional backend-side modifier preflight for one edge.
    /// Backends that can observe modifier state before taking ownership use
    /// this to avoid a grab/release cycle for a blocked crossing. The outer
    /// capture task retains its gate as a safety net for other backends.
    pub async fn set_crossing_modifier(
        &mut self,
        pos: Position,
        modifier: Option<CrossingModifier>,
    ) -> Result<(), CaptureError> {
        self.capture.set_crossing_modifier(pos, modifier).await
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        // Compute the host-side warp target before resetting the
        // wall-press / virtual_cursor state — once those are cleared
        // we lose the data needed to figure out where the guest's
        // cursor visually was.
        let warp_target = self
            .capture_pos
            .and_then(|pos| self.host_warp_target_on_release(pos));
        log::info!(
            "[release-warp] capture_pos={:?} virtual_cursor={:?} peer_bounds={:?} display_bounds={:?} → warp_target={warp_target:?}",
            self.capture_pos,
            self.virtual_cursor,
            self.capture_pos
                .and_then(|p| self.peer_bounds.get(&p).copied()),
            self.capture.display_bounds(),
        );
        self.pressed_keys.clear();
        self.reset_wall_press_state();
        if self.backend_capture_released {
            log::debug!("backend already released capture; completing wrapper teardown only");
            return Ok(());
        }
        let result = self.capture.release(warp_target).await;
        if result.is_ok() {
            self.backend_capture_released = true;
        }
        result
    }

    /// Release without applying a host-side cursor warp. Used when
    /// the remote peer is taking over (it just sent us Enter +
    /// CursorPos): the proportional warp from CursorPos is the
    /// authoritative final position for our shared cursor, and the
    /// stale `virtual_cursor`-derived warp would race against it
    /// and frequently win — clobbering the proportional landing
    /// with whatever position Linux *thought* the peer's cursor was
    /// at before the user moved it.
    pub async fn release_no_host_warp(&mut self) -> Result<(), CaptureError> {
        log::info!(
            "[release-warp] handover release: capture_pos={:?} — skipping host warp, peer's CursorPos is authoritative",
            self.capture_pos,
        );
        self.pressed_keys.clear();
        self.reset_wall_press_state();
        if self.backend_capture_released {
            log::debug!("backend already released capture; completing handover teardown only");
            return Ok(());
        }
        let result = self.capture.release(None).await;
        if result.is_ok() {
            self.backend_capture_released = true;
        }
        result
    }

    /// Configure the wall-press auto-release pixel threshold.
    /// 0 disables. Effective immediately for the next motion event;
    /// no need to recreate the backend.
    pub fn set_release_threshold(&mut self, threshold: u32) {
        self.release_threshold_px = threshold;
    }

    /// Cache the peer's display geometry for a position. Used by
    /// the wall-press tracker as the upper bound for `virtual_pos`
    /// so the model can't run away when the user pushes past the
    /// peer's actual far edge.
    ///
    /// If `Begin` fired before this arrived (the round-trip
    /// bootstrap case — `Bounds` is sent in response to `Enter`,
    /// which is sent by the host AFTER `Begin` fires), seed
    /// `virtual_cursor` retroactively so the wall-press / release
    /// machinery has a baseline to track from. Drains any motion
    /// that piled up in `pending_motion` so deltas during the
    /// round-trip aren't lost.
    pub fn set_peer_bounds(&mut self, pos: Position, width: u32, height: u32) {
        log::debug!("peer at {pos} reports bounds {width}x{height}");
        if let Some(layout_size) = self.peer_layout(pos).and_then(DisplayLayout::size) {
            if layout_size != (width, height) {
                // DisplayLayout is the self-contained authoritative snapshot.
                // Bounds is a separate compatibility datagram and can arrive
                // late or out of order. Never let a delayed old Bounds disable
                // a newer topology. During a real hotplug, keep using the last
                // complete layout until the next DisplayLayout atomically
                // replaces it.
                log::debug!(
                    "ignoring out-of-order peer bounds {width}x{height} at {pos}; authoritative topology is {}x{}",
                    layout_size.0,
                    layout_size.1,
                );
                return;
            }
        }
        self.peer_bounds.insert(pos, (width, height));

        if self.virtual_cursor.is_none()
            && self.capture_pos == Some(pos)
            && (self.pending_begin_cursor.is_some()
                || self.pending_begin_normalized_cursor.is_some())
        {
            let begin_cursor = self.pending_begin_cursor;
            let seeded = self.initial_virtual_cursor(
                pos,
                begin_cursor,
                self.pending_begin_normalized_cursor,
            );
            if let Some((sx, sy)) = seeded {
                let (mx, my) = self.pending_motion;
                let topology_motion = self
                    .peer_layout(pos)
                    .and_then(|layout| model_topology_motion(layout, pos, (sx, sy), mx, my));
                if let Some(motion) = topology_motion {
                    self.virtual_cursor = Some(motion.cursor);
                    self.virtual_pos = motion.entry_distance;
                } else {
                    let peer_w = width as f64;
                    let peer_h = height as f64;
                    self.virtual_cursor =
                        Some(((sx + mx).clamp(0.0, peer_w), (sy + my).clamp(0.0, peer_h)));
                }
                self.pending_motion = (0.0, 0.0);
                log::info!(
                    "[bootstrap] seeded virtual_cursor={:?} after late peer_bounds at {pos} (drained pending_motion=({mx:.1}, {my:.1}))",
                    self.virtual_cursor
                );
            }
        }
    }

    /// Cache the peer's complete logical display topology. The topology is a
    /// self-contained, authoritative geometry snapshot: derive its legacy
    /// bounds locally so packet loss/reordering cannot leave a valid layout
    /// unusable while waiting for a separate `Bounds` datagram.
    pub fn set_peer_layout(
        &mut self,
        pos: Position,
        epoch: u64,
        generation: u32,
        layout: DisplayLayout,
    ) {
        let Some(layout_size) = layout.size() else {
            log::warn!("ignoring empty peer topology at {pos}");
            return;
        };

        let candidate_version = (epoch, generation);
        if let Some(&current_version) = self.peer_layout_generations.get(&pos) {
            if candidate_version == current_version {
                if self.peer_layouts.get(&pos) != Some(&layout) {
                    log::warn!(
                        "ignoring contradictory peer topology epoch {epoch}, generation {generation} at {pos}"
                    );
                    return;
                }
            } else if !topology_version_is_newer(current_version, candidate_version) {
                log::debug!(
                    "ignoring reordered peer topology epoch {epoch}, generation {generation} at {pos}; current version is {current_version:?}"
                );
                return;
            }
        }

        self.peer_layout_generations.insert(pos, candidate_version);
        let previous = self.peer_layouts.insert(pos, layout.clone());
        self.peer_bounds.insert(pos, layout_size);
        let layout_reactivated = self.confirmed_peer_layouts.insert(pos);
        if self.capture_pos != Some(pos) {
            return;
        }
        let layout_changed = previous.as_ref() != Some(&layout);
        if layout_changed || layout_reactivated {
            self.wall_pressure = 0.0;
            if std::mem::take(&mut self.wall_press_pending) {
                log::info!(
                    "cancelled pending wall release after peer topology changed or reactivated"
                );
            }
        }

        if layout_reactivated || previous.is_none() || self.virtual_cursor.is_none() {
            // Bounds and topology are sent before Ack, so this normally
            // re-seeds before any queued input is flushed. Reapply net motion
            // defensively in case those datagrams were scheduled separately.
            if let Some(seed) = self.initial_virtual_cursor(
                pos,
                self.pending_begin_cursor,
                self.pending_begin_normalized_cursor,
            ) {
                let (dx, dy) = self.motion_since_begin;
                if let Some(motion) = model_topology_motion(&layout, pos, seed, dx, dy) {
                    self.virtual_cursor = Some(motion.cursor);
                    self.virtual_pos = motion.entry_distance;
                } else {
                    self.virtual_cursor = Some(seed);
                }
                self.pending_motion = (0.0, 0.0);
            }
        } else if layout_changed {
            let (Some(previous), Some(cursor)) = (previous.as_ref(), self.virtual_cursor) else {
                return;
            };
            // A live hotplug may invalidate the old cursor coordinate. Clamp
            // it onto the nearest real display without inventing motion. The
            // cached cursor is union-relative, so first preserve its global
            // point across an origin move before interpreting it in the new
            // layout's coordinate space.
            let translated = match (previous.origin(), layout.origin()) {
                (Some(old_origin), Some(new_origin)) => (
                    cursor.0 + f64::from(old_origin.0) - f64::from(new_origin.0),
                    cursor.1 + f64::from(old_origin.1) - f64::from(new_origin.1),
                ),
                _ => cursor,
            };
            if let Some(motion) = model_topology_motion(&layout, pos, translated, 0.0, 0.0) {
                self.virtual_cursor = Some(motion.cursor);
                self.virtual_pos = motion.entry_distance;
            }
        }
    }

    /// Return the topology confirmed after the current `Begin`, while its
    /// derived compatibility bounds agree with the cached Bounds. A retained
    /// but unconfirmed layout is intentionally invisible to both cursor
    /// modeling and `set_peer_bounds` conflict checks.
    fn peer_layout(&self, pos: Position) -> Option<&DisplayLayout> {
        if !self.confirmed_peer_layouts.contains(&pos) {
            return None;
        }
        let bounds = self.peer_bounds.get(&pos)?;
        let layout = self.peer_layouts.get(&pos)?;
        (layout.size() == Some(*bounds)).then_some(layout)
    }

    /// Forget the cached peer geometry for a position. Called when
    /// the corresponding capture is destroyed so re-adding the same
    /// peer later (potentially with new geometry) starts fresh.
    pub fn clear_peer_bounds(&mut self, pos: Position) {
        self.peer_bounds.remove(&pos);
        self.peer_layouts.remove(&pos);
        self.confirmed_peer_layouts.remove(&pos);
        self.peer_layout_generations.remove(&pos);
    }

    /// Cache the receiver's per-pair motion-sensitivity multiplier
    /// for the given position. Used to scale the wall-press
    /// auto-release model's accumulator so it tracks the receiver's
    /// actual cursor advance instead of the raw deltas the host
    /// emits. Out-of-range / non-finite values are ignored to keep
    /// the model from diverging on a rogue peer.
    pub fn set_peer_sensitivity(&mut self, pos: Position, mouse_sensitivity: f64) {
        if !mouse_sensitivity.is_finite() || mouse_sensitivity <= 0.0 {
            log::warn!(
                "ignoring non-finite/non-positive peer sensitivity {mouse_sensitivity} for {pos}"
            );
            return;
        }
        log::debug!("peer at {pos} reports sensitivity {mouse_sensitivity:.3}");
        self.peer_sensitivity.insert(pos, mouse_sensitivity);
    }

    /// Drop the cached receiver sensitivity for a position. Mirrors
    /// `clear_peer_bounds` — called on capture destroy so a re-add
    /// starts at the 1.0 default until a fresh
    /// [`ProtoEvent::ReceiverSensitivity`] arrives.
    pub fn clear_peer_sensitivity(&mut self, pos: Position) {
        self.peer_sensitivity.remove(&pos);
    }

    /// Host's own display geometry — width and height in pixels of
    /// the union of all displays. Returns `None` when the active
    /// backend can't query its own bounds (e.g. dummy). Used by
    /// `host_normalized_cursor` to compute the
    /// [`ProtoEvent::CursorPos`] fraction the guest scales against
    /// its own bounds on Enter.
    pub fn display_bounds(&self) -> Option<(u32, u32)> {
        self.capture.display_bounds()
    }

    /// Host's screen-space cursor position normalized to the host's
    /// own display bounds (each axis in 0..1, clamped). Returns
    /// `None` when the active backend can't report its own bounds.
    /// Used for the self-sufficient `ProtoEvent::CursorPos` event
    /// (the receiver scales the normalized fraction against its
    /// own bounds and pins the entry axis to the matching edge), so
    /// the first crossing isn't blocked by the bootstrap problem
    /// `peer_warp_target` has — that variant requires a prior
    /// `Bounds` round-trip from the peer, which can't have happened
    /// yet on the very first Enter.
    pub fn host_normalized_cursor(&self, cursor: (i32, i32)) -> Option<(f32, f32)> {
        normalize_cursor_in_layout(&self.capture.display_layout()?, cursor)
    }

    /// Cursor warp target on the peer for a transition at `pos`,
    /// given the host's screen-space cursor position at the moment
    /// of crossing. Returns `None` when either the host's own
    /// `display_bounds` or the cached peer geometry is unavailable —
    /// in that case there's no warp target to compute and the peer's
    /// cursor stays wherever the most recent `CursorPos` (or, if none
    /// arrived this session, where it was) put it.
    ///
    /// Coordinates returned are pixels in the peer's screen space:
    /// the cross-axis is preserved as a normalized fraction of the
    /// host screen (so a host_y near the top maps to a peer_y near
    /// the top regardless of resolution mismatch), the on-axis is
    /// pinned to the peer's far edge for the entering side.
    pub fn peer_warp_target(&self, pos: Position, cursor: (i32, i32)) -> Option<(i32, i32)> {
        let normalized = self.host_normalized_cursor(cursor)?;
        self.peer_warp_target_from_normalized(pos, normalized)
    }

    fn peer_warp_target_from_normalized(
        &self,
        pos: Position,
        normalized: (f32, f32),
    ) -> Option<(i32, i32)> {
        let &(peer_w, peer_h) = self.peer_bounds.get(&pos)?;
        let nx = f64::from(normalized.0).clamp(0.0, 1.0);
        let ny = f64::from(normalized.1).clamp(0.0, 1.0);
        if let Some(layout) = self.peer_layout(pos) {
            let edge = peer_entry_edge(pos);
            let fraction = match edge {
                DisplayEdge::Left | DisplayEdge::Right => ny,
                DisplayEdge::Top | DisplayEdge::Bottom => nx,
            };
            let global = layout.project_fraction(edge, fraction)?;
            let peer_origin = layout.origin()?;
            return Some((global.0 - peer_origin.0, global.1 - peer_origin.1));
        }
        let peer_w_i = peer_w as i32;
        let peer_h_i = peer_h as i32;
        let target = match pos {
            // Peer to our Left → cursor exits on left, enters peer on right
            Position::Left => (peer_w_i.saturating_sub(1), (ny * peer_h as f64) as i32),
            // Peer to our Right → cursor enters peer on left
            Position::Right => (0, (ny * peer_h as f64) as i32),
            // Peer above → cursor enters peer on bottom
            Position::Top => ((nx * peer_w as f64) as i32, peer_h_i.saturating_sub(1)),
            // Peer below → cursor enters peer on top
            Position::Bottom => ((nx * peer_w as f64) as i32, 0),
        };
        Some(target)
    }

    /// Returns the upper-clamp value (along the entry axis) for the
    /// given position, or `f64::INFINITY` if the peer hasn't reported
    /// bounds yet.
    fn peer_extent(&self, pos: Position) -> f64 {
        let Some(&(w, h)) = self.peer_bounds.get(&pos) else {
            return f64::INFINITY;
        };
        match pos {
            Position::Left | Position::Right => f64::from(w),
            Position::Top | Position::Bottom => f64::from(h),
        }
    }

    fn reset_wall_press_state(&mut self) {
        self.capture_pos = None;
        self.virtual_pos = 0.0;
        self.wall_pressure = 0.0;
        self.virtual_cursor = None;
        self.pending_begin_cursor = None;
        self.pending_begin_normalized_cursor = None;
        self.pending_motion = (0.0, 0.0);
        self.motion_since_begin = (0.0, 0.0);
        // Cancel any deferred AutoRelease — release() / handover have
        // taken responsibility for the transition.
        self.wall_press_pending = false;
    }

    /// Initial guest-space cursor position for a freshly-started
    /// capture. Mirrors what the guest's emulation will visibly do on
    /// the corresponding `Enter`: the `CursorPos` proportional warp
    /// target if the host can compute one (capture backend reports
    /// cursor), otherwise the entry-edge midpoint as a fallback for
    /// the wall-press model's starting position.
    fn initial_virtual_cursor(
        &self,
        pos: Position,
        host_cursor: Option<(i32, i32)>,
        normalized_cursor: Option<(f32, f32)>,
    ) -> Option<(f64, f64)> {
        if let Some(normalized_cursor) = normalized_cursor {
            if let Some((x, y)) = self.peer_warp_target_from_normalized(pos, normalized_cursor) {
                return Some((x as f64, y as f64));
            }
        } else if let Some(host_cursor) = host_cursor {
            if let Some((x, y)) = self.peer_warp_target(pos, host_cursor) {
                return Some((x as f64, y as f64));
            }
        }
        if let Some(layout) = self.peer_layout(pos) {
            let point = layout.project_fraction(peer_entry_edge(pos), 0.5)?;
            let origin = layout.origin()?;
            return Some((
                f64::from(point.0) - f64::from(origin.0),
                f64::from(point.1) - f64::from(origin.1),
            ));
        }
        let &(peer_w, peer_h) = self.peer_bounds.get(&pos)?;
        let pw = peer_w as f64;
        let ph = peer_h as f64;
        Some(match pos {
            // `pos` locates the peer relative to this host, so the peer is
            // entered through the opposite edge.
            Position::Left => ((pw - 1.0).max(0.0), ph / 2.0),
            Position::Right => (0.0, ph / 2.0),
            Position::Top => (pw / 2.0, (ph - 1.0).max(0.0)),
            Position::Bottom => (pw / 2.0, 0.0),
        })
    }

    /// Where on the host's own screen the cursor should land when
    /// capture is released, given the modeled guest cursor position
    /// at the moment of release. Symmetric inverse of
    /// `peer_warp_target`: cross-axis is preserved as a normalized
    /// fraction of the peer's screen, on-axis is pinned to the
    /// host's far edge for the side the guest is on so the cursor
    /// reappears at the boundary it just crossed back through.
    fn host_warp_target_on_release(&self, pos: Position) -> Option<(i32, i32)> {
        let (gx, gy) = self.virtual_cursor?;
        let &(peer_w, peer_h) = self.peer_bounds.get(&pos)?;
        let layout = self.capture.display_layout()?;
        let bounds = layout.bounds()?;
        let (host_w, host_h) = bounds.size();
        if peer_w == 0 || peer_h == 0 || host_w == 0 || host_h == 0 {
            return None;
        }
        let (origin_x, origin_y) = bounds.origin();
        let nx = (gx / peer_w as f64).clamp(0.0, 1.0);
        let ny = (gy / peer_h as f64).clamp(0.0, 1.0);
        let host_w_i = host_w as i32;
        let host_h_i = host_h as i32;
        // Add the union origin back so the result is in pointer-event
        // coordinate space (which is what `CGDisplay::warp_mouse_cursor_position`
        // and friends consume), not "0..host_w" of the union rectangle.
        // Matters on macOS hosts whose primary isn't anchored at (0, 0)
        // — `display_bounds` returns just the size of the union, so the
        // origin needs to be reapplied to recover absolute coords.
        let rectangular_target = match pos {
            // Peer to our Left → cursor returns through host's left edge
            Position::Left => (origin_x, origin_y + (ny * host_h as f64) as i32),
            // Peer to our Right → cursor returns through host's right edge
            Position::Right => (
                origin_x + host_w_i.saturating_sub(1),
                origin_y + (ny * host_h as f64) as i32,
            ),
            // Peer above → cursor returns through host's top edge
            Position::Top => (origin_x + (nx * host_w as f64) as i32, origin_y),
            // Peer below → cursor returns through host's bottom edge
            Position::Bottom => (
                origin_x + (nx * host_w as f64) as i32,
                origin_y + host_h_i.saturating_sub(1),
            ),
        };
        layout.project_point(display_edge(pos), rectangular_target)
    }

    /// Update the wall-press accumulator from one event coming up
    /// from the backend. Sets `wall_press_pending` (and arms the
    /// timer) when the threshold is first crossed; the actual
    /// `AutoRelease` synthesis happens in `poll_next` once the
    /// deadline elapses without a peer Leave clearing the flag.
    fn track_wall_press(&mut self, pos: Position, event: &CaptureEvent) {
        match event {
            CaptureEvent::Begin {
                cursor,
                normalized_cursor,
            } => {
                // Receiver geometry and sensitivity belong to the previous
                // connection epoch. Current peers republish all three before
                // Ack; old peers publish Bounds only and must get the 1.0,
                // rectangular fallback rather than inheriting stale state.
                self.confirmed_peer_layouts.remove(&pos);
                self.peer_bounds.remove(&pos);
                // Keep the last complete layout and its ordering version as a
                // hidden baseline across Begin. Clearing confirmation makes it
                // ineligible for cursor modeling even if a legacy peer reports
                // same-size Bounds, while retaining epoch/generation lets us
                // reject a queued frame from a replaced connection or older
                // process.
                self.peer_sensitivity.remove(&pos);
                self.capture_pos = Some(pos);
                self.virtual_pos = 0.0;
                self.wall_pressure = 0.0;
                self.virtual_cursor = self.initial_virtual_cursor(pos, *cursor, *normalized_cursor);
                // Stash the host-coord cursor so set_peer_bounds can
                // retroactively seed virtual_cursor if peer_bounds
                // arrives after Begin.
                self.pending_begin_cursor = *cursor;
                self.pending_begin_normalized_cursor = *normalized_cursor;
                self.pending_motion = (0.0, 0.0);
                self.motion_since_begin = (0.0, 0.0);
                log::info!(
                    "[wp-begin] pos={pos} cursor={cursor:?} peer_bounds={:?} virtual_cursor={:?}",
                    self.peer_bounds.get(&pos).copied(),
                    self.virtual_cursor,
                );
            }
            CaptureEvent::AutoRelease => {
                // Don't reset virtual_cursor here — release() needs it
                // to compute the host-side warp target. The wrapper's
                // release() resets state after consuming it.
            }
            CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { dx, dy, .. })) => {
                let Some(active_pos) = self.capture_pos else {
                    return;
                };
                if active_pos != pos {
                    return;
                }

                // The receiver multiplies both axes before injecting
                // motion. Apply the same transform to every part of the
                // host-side cursor model, not only to the entry-axis wall
                // pressure, or the return landing drifts whenever a peer
                // uses non-default sensitivity.
                let sensitivity = self
                    .peer_sensitivity
                    .get(&active_pos)
                    .copied()
                    .unwrap_or(1.0);
                let (modeled_dx, modeled_dy) = scale_motion(*dx, *dy, sensitivity);
                self.motion_since_begin.0 += modeled_dx;
                self.motion_since_begin.1 += modeled_dy;

                // Track the guest cursor through its actual display topology
                // when a current peer supplied one. This mirrors receiver
                // clamping at monitor edges and recomputes distance from the
                // host-adjacent contour at the cursor's new cross coordinate.
                // The rectangular branch remains for older peers that report
                // Bounds only.
                let topology_motion = self.virtual_cursor.and_then(|cursor| {
                    self.peer_layout(active_pos).and_then(|layout| {
                        model_topology_motion(layout, active_pos, cursor, modeled_dx, modeled_dy)
                    })
                });
                let legacy_proposed = if let Some(motion) = topology_motion {
                    self.virtual_cursor = Some(motion.cursor);
                    self.virtual_pos = motion.entry_distance;
                    None
                } else {
                    match (
                        self.virtual_cursor.as_mut(),
                        self.peer_bounds.get(&active_pos),
                    ) {
                        (Some(vc), Some(&(peer_w, peer_h))) => {
                            vc.0 = (vc.0 + modeled_dx).clamp(0.0, peer_w as f64);
                            vc.1 = (vc.1 + modeled_dy).clamp(0.0, peer_h as f64);
                        }
                        // virtual_cursor not yet seeded (peer_bounds was
                        // None at Begin time and the round-trip hasn't
                        // completed yet). Buffer the deltas so they can
                        // be applied retroactively in set_peer_bounds
                        // once the bootstrap finishes — otherwise the
                        // motion that happened during the round-trip is
                        // silently lost and the release-time warp picks
                        // the wrong y.
                        (None, _) => {
                            self.pending_motion.0 += modeled_dx;
                            self.pending_motion.1 += modeled_dy;
                            log::debug!(
                                "[wp-motion] deferred dx={dx:.1} dy={dy:.1} (peer_bounds for {active_pos}: {:?})",
                                self.peer_bounds.get(&active_pos).copied(),
                            );
                        }
                        _ => {}
                    }

                    let delta = entry_axis_delta(active_pos, modeled_dx, modeled_dy);
                    let proposed = self.virtual_pos + delta;
                    let upper = self.peer_extent(active_pos);
                    self.virtual_pos = proposed.clamp(0.0, upper);
                    Some(proposed)
                };

                if self.release_threshold_px == 0 {
                    return;
                }

                let wall_overshoot = topology_motion.map_or_else(
                    || legacy_proposed.map_or(0.0, |proposed| (-proposed).max(0.0)),
                    |motion| motion.wall_overshoot,
                );
                if wall_overshoot > 0.0 {
                    // Motion overshot the host-adjacent edge —
                    // accumulate the unabsorbed amount as wall
                    // pressure.
                    self.wall_pressure += wall_overshoot;
                } else {
                    // Cursor moved into the interior or further in;
                    // reset so a brief bump against the wall followed
                    // by motion deeper into the guest doesn't combine
                    // with a later wall-press to fire spuriously.
                    self.wall_pressure = 0.0;
                    if std::mem::take(&mut self.wall_press_pending) {
                        log::info!(
                            "wall-press deferred AutoRelease cancelled (cursor moved away from entry edge)"
                        );
                    }
                }

                if self.wall_pressure >= f64::from(self.release_threshold_px)
                    && !self.wall_press_pending
                {
                    self.wall_press_pending = true;
                    self.wall_press_timer
                        .as_mut()
                        .reset(tokio::time::Instant::now() + self.wall_press_deadline);
                    log::info!(
                        "wall-press threshold reached ({:.0}px past entry edge, {}px threshold) — \
                         deferring AutoRelease for {}ms pending peer Leave",
                        self.wall_pressure,
                        self.release_threshold_px,
                        self.wall_press_deadline.as_millis(),
                    );
                }
                // Fire is now driven by the timer in `poll_next`, not
                // directly from this event — keeps the behavior gated
                // on "peer didn't claim handover in time" instead of
                // racing the peer's Leave.
            }
            _ => {}
        }
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let capture = create(backend).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
            release_threshold_px: 0,
            capture_pos: None,
            backend_capture_released: true,
            virtual_pos: 0.0,
            wall_pressure: 0.0,
            virtual_cursor: None,
            pending_begin_cursor: None,
            pending_begin_normalized_cursor: None,
            pending_motion: (0.0, 0.0),
            motion_since_begin: (0.0, 0.0),
            peer_bounds: HashMap::new(),
            peer_layouts: HashMap::new(),
            confirmed_peer_layouts: HashSet::new(),
            peer_layout_generations: HashMap::new(),
            peer_sensitivity: HashMap::new(),
            wall_press_pending: false,
            wall_press_deadline: Duration::from_millis(150),
            wall_press_timer: Box::pin(tokio::time::sleep(Duration::from_secs(0))),
        })
    }

    /// check whether the given keys are pressed
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        keys.iter().all(|k| self.pressed_keys.contains(k))
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(e) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(e)));
        }

        // Deferred wall-press fallback. If the threshold was crossed
        // and the deadline elapsed without a peer Leave clearing
        // `wall_press_pending` (release_no_host_warp →
        // reset_wall_press_state), synthesize AutoRelease for every
        // capture handle at the active position. Polled before the
        // backend so a fire still happens when the user pinned the
        // cursor against the wall and stopped moving (no further
        // backend events, but the deadline still has to elapse).
        if self.wall_press_pending && self.wall_press_timer.as_mut().poll(cx).is_ready() {
            self.wall_press_pending = false;
            log::info!(
                "wall-press deadline elapsed ({}ms) — firing AutoRelease (no peer Leave; \
                 assuming peer-side capture is unavailable, e.g. lock screen)",
                self.wall_press_deadline.as_millis(),
            );
            if let Some(pos) = self.capture_pos {
                if let Some(ids) = self.position_map.get(&pos).cloned() {
                    for id in ids {
                        self.pending.push_back((id, CaptureEvent::AutoRelease));
                    }
                }
            }
            if let Some(e) = self.pending.pop_front() {
                return Poll::Ready(Some(Ok(e)));
            }
        }

        // ready
        let event = ready!(self.capture.poll_next_unpin(cx));

        // stream closed
        let event = match event {
            Some(e) => e,
            None => return Poll::Ready(None),
        };

        // error occurred
        let (pos, event) = match event {
            Ok(e) => e,
            Err(e) => return Poll::Ready(Some(Err(e))),
        };

        match &event {
            CaptureEvent::Begin { .. } => self.backend_capture_released = false,
            CaptureEvent::AutoRelease => self.backend_capture_released = true,
            _ => {}
        }

        // handle key presses
        if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) = &event
        {
            self.update_pressed_keys(*key, *state);
        }

        // wall-press auto-release tracking. Runs against every event
        // before routing so a single global accumulator stays consistent
        // regardless of how many handles exist at this position. The
        // fire itself is deferred and driven by `wall_press_timer`
        // above so the peer's Leave can cancel it.
        self.track_wall_press(pos, &event);

        let len = self
            .position_map
            .get(&pos)
            .map(|ids| ids.len())
            .unwrap_or(0);

        match len {
            0 => {
                // The backend can queue a final AutoRelease while destroy()
                // removes the last handle at this edge. We consumed that
                // stale event, so its fd readiness cannot wake us again.
                // Schedule one clean top-level poll to arm every timer/backend
                // source instead of leaving the stream asleep indefinitely.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            1 => {
                let id = self.position_map.get(&pos).expect("no id")[0];
                Poll::Ready(Some(Ok((id, event))))
            }
            _ => {
                let mut position_map = HashMap::new();
                swap(&mut self.position_map, &mut position_map);
                {
                    for &id in position_map.get(&pos).expect("position") {
                        self.pending.push_back((id, event.clone()));
                    }
                }
                swap(&mut self.position_map, &mut position_map);

                Poll::Ready(Some(Ok(self.pending.pop_front().expect("event"))))
            }
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
    /// create a new client with the given id
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// destroy the client with the given id, if it exists
    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// release mouse. `warp_target`, when present, is a screen-space
    /// pixel point on the host's own display where the local cursor
    /// should be placed before becoming visible again — used to
    /// preserve cross-axis continuity when capture ends so the cursor
    /// reappears next to where it visually was on the guest, not at
    /// the spot where capture started. Backends that don't hide the
    /// system cursor or can't warp it can ignore the parameter.
    async fn release(&mut self, warp_target: Option<(i32, i32)>) -> Result<(), CaptureError>;

    /// Configure a modifier that must be held before this edge takes pointer
    /// ownership. The default is intentionally a no-op: the higher-level gate
    /// still enforces the setting for backends without a safe preflight API.
    async fn set_crossing_modifier(
        &mut self,
        _pos: Position,
        _modifier: Option<CrossingModifier>,
    ) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;

    /// Host's own display geometry. Default implementation returns
    /// `None`; backends that can query their own dimensions override
    /// (currently macOS via CGDisplay; others may add this later).
    fn display_bounds(&self) -> Option<(u32, u32)> {
        None
    }

    /// Top-left corner of the union of all displays in the host's
    /// global pointer-coordinate system. Defaults to (0, 0) — fine
    /// for any backend whose primary display is the origin (Windows,
    /// most X11/Wayland setups). Returns the actual `(xmin, ymin)`
    /// on macOS, where the global coordinate system is anchored at
    /// the primary's top-left and a left-attached external display
    /// occupies negative x. Used by `host_normalized_cursor` and
    /// `peer_warp_target` to correctly normalize cursor positions
    /// outside the primary display — without this, the
    /// `clamp(0.0, 1.0)` in those helpers silently maps every point
    /// on a non-origin display to the screen edge.
    fn display_origin(&self) -> (i32, i32) {
        (0, 0)
    }

    /// Full display topology in the same coordinates as capture events.
    /// The default preserves rectangular behavior for backends that expose
    /// only a union origin and size; multi-monitor backends override it so
    /// return warps can project onto the actual stepped screen contour.
    fn display_layout(&self) -> Option<DisplayLayout> {
        let (width, height) = self.display_bounds()?;
        let (x, y) = self.display_origin();
        let layout = DisplayLayout::new([(x, y, width, height)]);
        (!layout.is_empty()).then_some(layout)
    }
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    match backend {
        #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

#[cfg(all(
    unix,
    feature = "libei",
    feature = "layer_shell",
    not(target_os = "macos")
))]
fn desktop_is_hyprland(current_desktop: Option<&str>) -> bool {
    current_desktop.is_some_and(|desktop| {
        desktop
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("hyprland"))
    })
}

fn automatic_backend_order(_current_desktop: Option<&str>) -> Vec<Backend> {
    let backends = vec![
        #[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
        Backend::InputCapturePortal,
        #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
        Backend::LayerShell,
        #[cfg(all(unix, feature = "x11", not(target_os = "macos")))]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ];

    // Hyprland's portal and compositor can briefly disagree about output
    // geometry during hotplug. A barrier accepted by the portal is then a
    // fatal protocol error in the compositor, which tears down the EIS seat
    // (including its keyboard). Layer-shell gets its edges directly from the
    // compositor and is the stable native capture path on Hyprland. Explicit
    // backend overrides remain authoritative.
    #[cfg(all(
        unix,
        feature = "libei",
        feature = "layer_shell",
        not(target_os = "macos")
    ))]
    if desktop_is_hyprland(_current_desktop) {
        let mut backends = backends;
        backends.swap(0, 1);
        return backends;
    }

    backends
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    let current_desktop = std::env::var("XDG_CURRENT_DESKTOP").ok();
    for backend in automatic_backend_order(current_desktop.as_deref()) {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

#[cfg(test)]
mod tests {
    use super::{
        Backend, CaptureEvent, InputCapture, Position, model_topology_motion,
        normalize_cursor_in_layout, scale_motion, wrapping_generation_is_newer,
    };
    #[cfg(all(
        unix,
        feature = "libei",
        feature = "layer_shell",
        not(target_os = "macos")
    ))]
    use super::{automatic_backend_order, desktop_is_hyprland};
    use input_event::display::DisplayLayout;

    #[test]
    fn cursor_model_uses_receiver_sensitivity_on_both_axes() {
        assert_eq!(scale_motion(20.0, 100.0, 1.5), (30.0, 150.0));
    }

    #[test]
    fn crossing_normalization_uses_the_supplied_topology_snapshot() {
        let old_layout = DisplayLayout::new([(-1728, 0, 1728, 1117), (0, 0, 3072, 1728)]);
        let new_layout = DisplayLayout::new([(0, 0, 3072, 1728)]);
        let cursor = (-1000, 500);

        assert_eq!(
            normalize_cursor_in_layout(&old_layout, cursor),
            Some((728.0 / 4800.0, 500.0 / 1728.0))
        );
        assert_eq!(
            normalize_cursor_in_layout(&new_layout, cursor),
            Some((0.0, 500.0 / 1728.0))
        );
    }

    #[tokio::test]
    async fn late_peer_metadata_reuses_begin_snapshot_fraction() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");
        let normalized = (728.0_f32 / 4800.0, 500.0_f32 / 1728.0);
        capture.track_wall_press(
            Position::Bottom,
            &CaptureEvent::Begin {
                cursor: Some((-1000, 500)),
                normalized_cursor: Some(normalized),
            },
        );

        let guest = DisplayLayout::new([
            (-1024, 0, 1024, 600),
            (0, 0, 3072, 1728),
            (836, 1728, 1280, 360),
        ]);
        capture.set_peer_layout(Position::Bottom, 1, 1, guest);

        assert_eq!(capture.virtual_cursor, Some((621.0, 0.0)));
    }

    #[test]
    fn topology_model_tracks_a_step_in_the_host_adjacent_contour() {
        // Main display plus a shorter display on its left. Coordinates passed
        // to the model are relative to the (-1728, 0) union origin.
        let layout = DisplayLayout::new([(0, 0, 3072, 1728), (-1728, 0, 1728, 1117)]);

        let across = model_topology_motion(&layout, Position::Right, (0.0, 1000.0), 1728.0, 0.0)
            .expect("move onto main display");
        assert_eq!(across.cursor, (1728.0, 1000.0));
        assert_eq!(across.entry_distance, 1728.0);

        // Moving below the shorter left display changes the real left contour
        // from x=-1728 to x=0. The cursor is now on the host-adjacent edge,
        // rather than 1728 px deep as a union-rectangle model would claim.
        let below_step = model_topology_motion(&layout, Position::Right, across.cursor, 0.0, 200.0)
            .expect("move below step");
        assert_eq!(below_step.cursor, (1728.0, 1200.0));
        assert_eq!(below_step.entry_distance, 0.0);

        let wall = model_topology_motion(&layout, Position::Right, below_step.cursor, -25.0, 0.0)
            .expect("push against stepped left edge");
        assert_eq!(wall.cursor, below_step.cursor);
        assert_eq!(wall.wall_overshoot, 25.0);
    }

    #[test]
    fn topology_model_clamps_empty_space_but_allows_a_jump_onto_another_display() {
        let layout = DisplayLayout::new([(0, 0, 3072, 1728), (-1728, 0, 1728, 1117)]);
        let current = (728.0, 1116.0); // global (-1000, 1116), on the left display

        let clamped = model_topology_motion(&layout, Position::Right, current, 0.0, 100.0)
            .expect("clamp at shorter display bottom");
        assert_eq!(clamped.cursor, current);

        let jumped = model_topology_motion(&layout, Position::Right, current, 1000.0, 100.0)
            .expect("endpoint is on main display");
        assert_eq!(jumped.cursor, (1728.0, 1216.0));
        assert_eq!(jumped.entry_distance, 0.0);
    }

    #[test]
    fn topology_model_matches_hyprland_nearest_output_gap_clamp() {
        let layout = DisplayLayout::new([
            (-1024, 0, 1024, 600),
            (0, 0, 3072, 1728),
            (836, 1728, 1280, 360),
        ]);
        // Union-relative (3224, 1700) is global (2200, 1700) on DP-5.
        // Moving down ends in the gap. DP-6's right edge is closer than
        // DP-5's bottom edge, so Hyprland snaps there.
        let motion = model_topology_motion(&layout, Position::Right, (3224.0, 1700.0), 0.0, 200.0)
            .expect("nearest monitor clamp");
        assert_eq!(motion.cursor, (3139.0, 1900.0));
    }

    #[tokio::test]
    async fn cursorless_fallback_starts_on_the_peers_host_facing_edge() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");

        for position in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            capture.set_peer_bounds(position, 1920, 1080);
        }

        assert_eq!(
            capture.initial_virtual_cursor(Position::Left, None, None),
            Some((1919.0, 540.0))
        );
        assert_eq!(
            capture.initial_virtual_cursor(Position::Right, None, None),
            Some((0.0, 540.0))
        );
        assert_eq!(
            capture.initial_virtual_cursor(Position::Top, None, None),
            Some((960.0, 1079.0))
        );
        assert_eq!(
            capture.initial_virtual_cursor(Position::Bottom, None, None),
            Some((960.0, 0.0))
        );

        let stepped = DisplayLayout::new([(0, 0, 1000, 1000), (-1000, 0, 1000, 300)]);
        capture.set_peer_bounds(Position::Right, 2000, 1000);
        capture.set_peer_layout(Position::Right, 100, 1, stepped);
        assert_eq!(
            capture.initial_virtual_cursor(Position::Right, None, None),
            Some((1000.0, 500.0)),
            "the midpoint follows the stepped left contour instead of empty union space",
        );
    }

    #[tokio::test]
    async fn delayed_legacy_bounds_cannot_disable_newer_topology() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");
        let current = DisplayLayout::new([(-1000, 0, 1000, 600), (0, 0, 2000, 1200)]);

        capture.set_peer_layout(Position::Right, 100, 7, current.clone());
        capture.set_peer_bounds(Position::Right, 1920, 1080);

        assert_eq!(capture.peer_bounds[&Position::Right], (3000, 1200));
        assert_eq!(capture.peer_layout(Position::Right), Some(&current));
    }

    #[tokio::test]
    async fn same_size_legacy_bounds_do_not_reactivate_retained_layout() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");
        let retained = DisplayLayout::new([(0, 0, 1000, 1000), (-1000, 0, 1000, 300)]);

        capture.set_peer_layout(Position::Right, 100, 7, retained);
        capture.track_wall_press(
            Position::Right,
            &CaptureEvent::Begin {
                cursor: Some((100, 100)),
                normalized_cursor: None,
            },
        );
        capture.set_peer_bounds(Position::Right, 2000, 1000);

        assert_eq!(capture.peer_bounds[&Position::Right], (2000, 1000));
        assert_eq!(capture.peer_layout(Position::Right), None);
        assert_eq!(capture.virtual_cursor, Some((0.0, 500.0)));
    }

    #[tokio::test]
    async fn different_size_legacy_bounds_replace_hidden_layout_bounds() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");
        let retained = DisplayLayout::new([(0, 0, 1000, 1000), (-1000, 0, 1000, 300)]);

        capture.set_peer_layout(Position::Right, 100, 7, retained);
        capture.track_wall_press(
            Position::Right,
            &CaptureEvent::Begin {
                cursor: Some((100, 100)),
                normalized_cursor: None,
            },
        );
        capture.set_peer_bounds(Position::Right, 2560, 1440);

        assert_eq!(capture.peer_bounds[&Position::Right], (2560, 1440));
        assert_eq!(capture.peer_layout(Position::Right), None);
        assert_eq!(capture.virtual_cursor, Some((0.0, 720.0)));
    }

    #[tokio::test]
    async fn accepted_current_layout_reactivates_and_reseeds_retained_topology() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");
        let retained = DisplayLayout::new([(0, 0, 1000, 1000), (-1000, 0, 1000, 300)]);

        capture.set_peer_layout(Position::Right, 100, 7, retained.clone());
        capture.track_wall_press(
            Position::Right,
            &CaptureEvent::Begin {
                cursor: Some((100, 100)),
                normalized_cursor: None,
            },
        );
        capture.set_peer_bounds(Position::Right, 2000, 1000);
        assert_eq!(capture.virtual_cursor, Some((0.0, 500.0)));

        // A current peer may republish the same generation when entering a
        // new capture. Accepting it confirms that the retained contour belongs
        // to this Begin and must replace the temporary rectangular bootstrap.
        capture.set_peer_layout(Position::Right, 100, 7, retained.clone());

        assert_eq!(capture.peer_layout(Position::Right), Some(&retained));
        assert_eq!(capture.virtual_cursor, Some((1000.0, 500.0)));
    }

    #[tokio::test]
    async fn reordered_topology_generation_cannot_regress_cursor_model() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");
        let older = DisplayLayout::new([(0, 0, 1920, 1080)]);
        let newer = DisplayLayout::new([(-1024, 0, 1024, 600), (0, 0, 3072, 1728)]);

        capture.set_peer_layout(Position::Right, 100, 11, newer.clone());
        capture.set_peer_layout(Position::Right, 100, 10, older);

        assert_eq!(capture.peer_layout(Position::Right), Some(&newer));
        assert_eq!(capture.peer_layout_generations[&Position::Right], (100, 11));
    }

    #[tokio::test]
    async fn restarted_peer_epoch_is_identity_not_wall_clock_order() {
        let mut capture = InputCapture::new(Some(Backend::Dummy))
            .await
            .expect("dummy capture");
        let old = DisplayLayout::new([(0, 0, 1920, 1080)]);
        let current = DisplayLayout::new([(-1024, 0, 1024, 600), (0, 0, 3072, 1728)]);

        capture.set_peer_layout(Position::Right, 200, 50, old.clone());
        capture.track_wall_press(
            Position::Right,
            &CaptureEvent::Begin {
                cursor: Some((100, 100)),
                normalized_cursor: None,
            },
        );
        // The restarted sender begins its generation counter again. Its clock
        // may also have moved backwards, so the new epoch is authoritative by
        // identity even though its numeric value is smaller. The transport
        // drops any later frames from the replaced DTLS session.
        capture.set_peer_layout(Position::Right, 100, 1, current.clone());

        assert_eq!(capture.peer_layout(Position::Right), Some(&current));
        assert_eq!(capture.peer_layout_generations[&Position::Right], (100, 1));
    }

    #[test]
    fn topology_generation_order_handles_wraparound() {
        assert!(wrapping_generation_is_newer(u32::MAX, 0));
        assert!(wrapping_generation_is_newer(10, 11));
        assert!(!wrapping_generation_is_newer(11, 10));
        assert!(!wrapping_generation_is_newer(11, 11));
    }

    #[cfg(all(
        unix,
        feature = "libei",
        feature = "layer_shell",
        not(target_os = "macos")
    ))]
    #[test]
    fn hyprland_is_detected_in_colon_separated_desktop_names() {
        assert!(desktop_is_hyprland(Some("Hyprland")));
        assert!(desktop_is_hyprland(Some("omarchy:Hyprland")));
        assert!(!desktop_is_hyprland(Some("GNOME")));
        assert!(!desktop_is_hyprland(None));
    }

    #[cfg(all(
        unix,
        feature = "libei",
        feature = "layer_shell",
        not(target_os = "macos")
    ))]
    #[test]
    fn hyprland_prefers_layer_shell_but_other_desktops_prefer_portal() {
        let hyprland = automatic_backend_order(Some("Hyprland"));
        assert_eq!(hyprland[0], Backend::LayerShell);
        assert_eq!(hyprland[1], Backend::InputCapturePortal);

        let gnome = automatic_backend_order(Some("GNOME"));
        assert_eq!(gnome[0], Backend::InputCapturePortal);
        assert_eq!(gnome[1], Backend::LayerShell);
    }
}
