use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, scancode};
use local_channel::mpsc::{Receiver, Sender, channel};
use mousehop_proto::{LEAVE_HANDOVER, LEAVE_RELEASE_ONLY, ProtoEvent};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::MousehopConnection;

pub(crate) struct Capture {
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// A capture barrier was entered. `handover` is true when a Default
    /// capture shares the edge, so the EnterOnly return will be followed by
    /// this device's own Enter + CursorPos rather than ending at a local
    /// release.
    CaptureBegin {
        handle: CaptureHandle,
        handover: bool,
    },
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
    /// The connect-side received the peer's `Hello` echo and
    /// updated `client_manager.peer_commit` for `handle`. Forwarded
    /// upward so Service can broadcast `FrontendEvent::State` and
    /// the GUI's per-row version-status indicator picks up the new
    /// value. The listen-side path independently emits
    /// [`crate::emulation::EmulationEvent::PeerHello`], but it
    /// races with `active_addr` population — when an incoming
    /// `Hello` arrives before our outbound dial completes, the
    /// listen path's `get_client(addr)` returns `None` and the
    /// commit silently goes unsurfaced. The connect-side path
    /// fires later but reliably, so it carries the broadcast as a
    /// belt-and-suspenders fallback.
    PeerCommitUpdated(CaptureHandle),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// release because the remote peer is taking over (they sent
    /// Enter+CursorPos). Skips the host-side warp so the peer's
    /// proportional CursorPos warp doesn't get clobbered by a
    /// racing local warp computed from stale virtual_cursor state.
    ReleaseForHandover,
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType, bool),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// set the auto-release pixel threshold (macOS only). 0 disables.
    SetReleaseThreshold(u32),
    /// Update the Command-to-Control alias stored for an outgoing
    /// capture. A capture already in progress keeps its mapping until
    /// the next Begin so a configuration change cannot split a chord.
    SetCommandAsCtrl(CaptureHandle, bool),
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: MousehopConnection,
        release_bind: Vec<scancode::Linux>,
        release_threshold_px: u32,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let capture_task = CaptureTask {
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            release_threshold_px: Rc::new(RefCell::new(release_threshold_px)),
            state: Default::default(),
            command_ctrl_mapper: Default::default(),
            #[cfg(target_os = "macos")]
            user_activity: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: mousehop_ipc::Position,
        capture_type: CaptureType,
        command_as_ctrl: bool,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(
                handle,
                pos,
                capture_type,
                command_as_ctrl,
            ))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release_for_handover(&self) {
        self.request_tx
            .send(CaptureRequest::ReleaseForHandover)
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    pub(crate) fn set_release_threshold(&mut self, threshold: u32) {
        let _ = self
            .request_tx
            .send(CaptureRequest::SetReleaseThreshold(threshold));
    }

    pub(crate) fn set_command_as_ctrl(&self, handle: CaptureHandle, enabled: bool) {
        let _ = self
            .request_tx
            .send(CaptureRequest::SetCommandAsCtrl(handle, enabled));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

// XKB/X11 modifier-mask bits used by every capture/emulation backend.
// Command is represented as Mod4 in the cross-platform event stream.
const CONTROL_MASK: u32 = 1 << 2;
const MOD4_MASK: u32 = 1 << 6;

#[derive(Debug, Default)]
struct AliasSide {
    physical_ctrl_down: bool,
    physical_command_down: bool,
}

impl AliasSide {
    fn logical_ctrl_down(&self) -> bool {
        self.physical_ctrl_down || self.physical_command_down
    }

    /// Update one physical source and return the new logical Ctrl
    /// state only when it actually transitioned. This reference-like
    /// state prevents Ctrl-up from being emitted while the other
    /// physical source is still held.
    fn update(&mut self, command: bool, down: bool) -> Option<bool> {
        let before = self.logical_ctrl_down();
        let source = if command {
            &mut self.physical_command_down
        } else {
            &mut self.physical_ctrl_down
        };
        if *source == down {
            return None;
        }
        *source = down;
        let after = self.logical_ctrl_down();
        (before != after).then_some(after)
    }
}

/// Per-capture sender-side Command-to-Control alias. The peer sees
/// ordinary evdev Control events, so no network protocol support is
/// required on the receiver.
#[derive(Debug, Default)]
struct CommandCtrlMapper {
    enabled: bool,
    left: AliasSide,
    right: AliasSide,
}

impl CommandCtrlMapper {
    fn reset(&mut self, enabled: bool) {
        *self = Self {
            enabled,
            ..Default::default()
        };
    }

    fn transform(&mut self, event: Event) -> Option<Event> {
        if !self.enabled {
            return Some(event);
        }

        match event {
            Event::Keyboard(KeyboardEvent::Key { time, key, state }) => {
                self.transform_key(time, key, state)
            }
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            }) => Some(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: command_mask_as_ctrl(depressed),
                latched: command_mask_as_ctrl(latched),
                locked: command_mask_as_ctrl(locked),
                group,
            })),
            other => Some(other),
        }
    }

    fn transform_key(&mut self, time: u32, key: u32, state: u8) -> Option<Event> {
        use scancode::Linux::{KeyLeftCtrl, KeyLeftMeta, KeyRightCtrl, KeyRightmeta};

        let Ok(source) = scancode::Linux::try_from(key) else {
            return Some(Event::Keyboard(KeyboardEvent::Key { time, key, state }));
        };
        let down = state != 0;
        let (side, command, target) = match source {
            KeyLeftCtrl => (&mut self.left, false, KeyLeftCtrl),
            KeyLeftMeta => (&mut self.left, true, KeyLeftCtrl),
            KeyRightCtrl => (&mut self.right, false, KeyRightCtrl),
            KeyRightmeta => (&mut self.right, true, KeyRightCtrl),
            _ => return Some(Event::Keyboard(KeyboardEvent::Key { time, key, state })),
        };

        side.update(command, down).map(|logical_down| {
            Event::Keyboard(KeyboardEvent::Key {
                time,
                key: target as u32,
                state: u8::from(logical_down),
            })
        })
    }

    /// Clear alias state and return the logical Ctrl keys that still
    /// need an up event. Raw Meta/Ctrl keys must not also be flushed:
    /// that would either leak Meta-up or release a colliding Ctrl too
    /// early on the receiver.
    fn take_releases(&mut self) -> Vec<scancode::Linux> {
        if !self.enabled {
            return Vec::new();
        }
        let mut releases = Vec::with_capacity(2);
        if self.left.logical_ctrl_down() {
            releases.push(scancode::Linux::KeyLeftCtrl);
        }
        if self.right.logical_ctrl_down() {
            releases.push(scancode::Linux::KeyRightCtrl);
        }
        self.left = AliasSide::default();
        self.right = AliasSide::default();
        releases
    }

    fn take_release_keys(
        &mut self,
        raw_pressed: impl IntoIterator<Item = scancode::Linux>,
    ) -> Vec<scancode::Linux> {
        if !self.enabled {
            return raw_pressed.into_iter().collect();
        }
        let mut releases: Vec<_> = raw_pressed
            .into_iter()
            .filter(|key| !is_ctrl_or_command(*key))
            .collect();
        releases.extend(self.take_releases());
        releases
    }
}

fn command_mask_as_ctrl(mask: u32) -> u32 {
    let had_command = mask & MOD4_MASK != 0;
    let mut mapped = mask & !MOD4_MASK;
    if had_command {
        mapped |= CONTROL_MASK;
    }
    mapped
}

fn is_ctrl_or_command(key: scancode::Linux) -> bool {
    matches!(
        key,
        scancode::Linux::KeyLeftCtrl
            | scancode::Linux::KeyRightCtrl
            | scancode::Linux::KeyLeftMeta
            | scancode::Linux::KeyRightmeta
    )
}

#[derive(Clone, Copy, Debug)]
struct CaptureRegistration {
    handle: CaptureHandle,
    pos: Position,
    capture_type: CaptureType,
    command_as_ctrl: bool,
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<CaptureRegistration>,
    conn: MousehopConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    release_threshold_px: Rc<RefCell<u32>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
    command_ctrl_mapper: CommandCtrlMapper,
    #[cfg(target_os = "macos")]
    user_activity: crate::macos_power::UserActivity,
}

impl CaptureTask {
    fn add_capture(
        &mut self,
        handle: CaptureHandle,
        pos: Position,
        capture_type: CaptureType,
        command_as_ctrl: bool,
    ) {
        self.captures.push(CaptureRegistration {
            handle,
            pos,
            capture_type,
            command_as_ctrl,
        });
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|capture| capture.handle != handle);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|capture| capture.pos == pos && capture.capture_type == CaptureType::Default)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|capture| capture.handle == handle)
            .expect("no such capture")
            .pos
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|capture| capture.handle == handle)
            .expect("no such capture")
            .capture_type
    }

    fn command_as_ctrl(&self, handle: CaptureHandle) -> bool {
        self.captures
            .iter()
            .find(|capture| capture.handle == handle)
            .is_some_and(|capture| capture.command_as_ctrl)
    }

    fn set_command_as_ctrl(&mut self, handle: CaptureHandle, enabled: bool) {
        if let Some(capture) = self
            .captures
            .iter_mut()
            .find(|capture| capture.handle == handle)
        {
            capture.command_as_ctrl = enabled;
        }
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t, command_as_ctrl) => {
                            self.add_capture(h, p, t, command_as_ctrl)
                        }
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::ReleaseForHandover => { /* nothing to do */ }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                        CaptureRequest::SetReleaseThreshold(threshold) => {
                            *self.release_threshold_px.borrow_mut() = threshold;
                        }
                        CaptureRequest::SetCommandAsCtrl(handle, enabled) => {
                            self.set_command_as_ctrl(handle, enabled);
                        }
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        // Push the configured auto-release threshold to the freshly
        // created InputCapture. The wall-press detection is
        // cross-platform — every backend benefits.
        capture.set_release_threshold(*self.release_threshold_px.borrow());

        let r = self.do_capture_session(&mut capture).await;

        // Any backend/session exit ends the remote-control interval,
        // including cancellation and capture errors that bypass the
        // ordinary release paths below.
        self.stop_user_activity();

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for registration in captures {
            tokio::select! {
                r = capture.create(registration.handle, registration.pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        // A screen saver can have a shorter idle delay than display
        // sleep. Refresh the macOS user-activity assertion often
        // enough to cover either while capture remains on a peer.
        let mut user_activity_tick = tokio::time::interval(Duration::from_secs(30));
        user_activity_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = user_activity_tick.tick() => self.pulse_user_activity(),
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(_) => {
                            log::info!("client {handle} acknowledged the connection!");
                            self.state = State::Sending;
                        }
                        // A legacy Leave(0), or any future mode this
                        // receiver doesn't understand, retains the
                        // handover behavior: skip the host warp because
                        // the peer's Enter+CursorPos may be racing it on
                        // the shared cursor. A new peer can explicitly
                        // report a one-way EnterOnly return with
                        // LEAVE_RELEASE_ONLY; no CursorPos will follow, so
                        // the modeled host warp must be applied.
                        ProtoEvent::Leave(mode) => {
                            if mode == LEAVE_RELEASE_ONLY {
                                log::info!(
                                    "releasing capture: peer returned through a release-only edge"
                                );
                                self.release_capture(capture).await?;
                            } else {
                                if mode != LEAVE_HANDOVER {
                                    log::debug!(
                                        "unknown Leave mode {mode}; treating it as a legacy handover"
                                    );
                                }
                                log::info!("releasing capture: peer is taking over");
                                self.release_capture_handover(capture).await?;
                            }
                        },
                        // Peer reported its display geometry — cache it
                        // so the wall-press model has a real upper
                        // clamp on virtual_pos for this position.
                        ProtoEvent::Bounds { width, height } => {
                            let pos = self.get_pos(handle);
                            capture.set_peer_bounds(pos, width, height);
                        }
                        // Peer reported its per-pair receive-side
                        // sensitivity multiplier — feed it into the
                        // wall-press model so its delta accumulator
                        // tracks the receiver's actual cursor advance.
                        // Without this, a sub-1.0 multiplier on the
                        // receiver makes the host's auto-release model
                        // fire before the cursor reaches the wall.
                        ProtoEvent::ReceiverSensitivity { mouse_sensitivity } => {
                            let pos = self.get_pos(handle);
                            capture.set_peer_sensitivity(pos, mouse_sensitivity);
                        }
                        // Peer's commit hash arrived on the outgoing
                        // DTLS connection. The connect-side
                        // receive_loop already wrote it to
                        // `client_manager`; bubble up to Service so
                        // the GUI's version-status row refreshes.
                        ProtoEvent::Hello { .. } => {
                            self.event_tx
                                .send(ICaptureEvent::PeerCommitUpdated(handle))
                                .expect("channel closed");
                        }
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => {
                        // Capture is already running, so there is
                        // nothing to restart — but "re-enable" is also
                        // the action reached for when a peer came back
                        // from sleep and input still isn't flowing.
                        // A session that outlived its peer still holds
                        // the compositor's pointer lock, which pins the
                        // local cursor and stops focus following the
                        // mouse. Tear it down here so the one action
                        // that brings the keyboard back hands the mouse
                        // back with it.
                        log::info!("releasing capture: re-enable requested while active");
                        self.release_capture(capture).await?;
                    },
                    CaptureRequest::ReleaseForHandover => self.release_capture_handover(capture).await?,
                    CaptureRequest::Create(h, p, t, command_as_ctrl) => {
                        self.add_capture(h, p, t, command_as_ctrl);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        if self.active_client == Some(h) {
                            self.stop_user_activity();
                        }
                        let pos = self.get_pos(h);
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                        // Drop the cached geometry — the next client
                        // added at this position may report different
                        // bounds.
                        capture.clear_peer_bounds(pos);
                        // Same lifecycle for the cached sensitivity
                        // — re-add starts at the 1.0 default until a
                        // fresh ReceiverSensitivity arrives.
                        capture.clear_peer_sensitivity(pos);
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    CaptureRequest::SetReleaseThreshold(threshold) => {
                        *self.release_threshold_px.borrow_mut() = threshold;
                        capture.set_release_threshold(threshold);
                    }
                    CaptureRequest::SetCommandAsCtrl(handle, enabled) => {
                        self.set_command_as_ctrl(handle, enabled);
                    }
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        // Backend self-released (currently only macOS, when sustained
        // back-toward-host motion crosses the configured threshold).
        // Drive the same teardown path as the release-bind chord so
        // the peer gets a Leave + key-up flush.
        if matches!(event, CaptureEvent::AutoRelease) {
            log::info!("releasing capture: backend auto-release");
            return self.release_capture(capture).await;
        }

        if matches!(event, CaptureEvent::Begin { .. }) {
            let handover = self.is_default_capture_at(self.get_pos(handle));
            self.event_tx
                .send(ICaptureEvent::CaptureBegin { handle, handover })
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // Every fresh Begin starts a new acknowledgement and mapping
        // session, including same-handle re-entry after a backend-side
        // release or send failure. `active_client` can outlive those
        // release paths, so gating this reset on a handle change would
        // retain stale held-source state and incorrectly stay Sending.
        if matches!(event, CaptureEvent::Begin { .. }) {
            self.start_user_activity();
            self.state = State::WaitingForAck;
            // Snapshot the mapping for this capture session. A config
            // change mid-chord is deliberately deferred until the next
            // Begin, and hand-edited settings are ignored off macOS so
            // Linux Super never changes meaning unexpectedly.
            let command_as_ctrl = cfg!(target_os = "macos") && self.command_as_ctrl(handle);
            self.command_ctrl_mapper.reset(command_as_ctrl);
            let changed_client = self.active_client.replace(handle) != Some(handle);
            if changed_client {
                self.event_tx
                    .send(ICaptureEvent::ClientEntered(handle))
                    .expect("channel closed");
            }
        }

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());

        // If we're starting a fresh capture and the backend reported
        // a cursor position at the moment of crossing, send a
        // `CursorPos` (host-normalized fraction + entry side from the
        // peer's frame) right after Enter. The peer scales against
        // its own live bounds and pins the on-axis dimension to the
        // matching edge — self-sufficient, no prior `Bounds`
        // round-trip needed, so the very first crossing also lands
        // the cursor at the visually-corresponding point.
        let cursor_pos = if let CaptureEvent::Begin {
            cursor: Some(cursor),
        } = event
        {
            let pos = self.get_pos(handle);
            capture.host_normalized_cursor(cursor).map(|(nx, ny)| {
                let proto_pos = to_proto_pos(pos.opposite());
                (proto_pos, nx, ny)
            })
        } else {
            None
        };

        let proto_event = match &event {
            CaptureEvent::Begin { .. } => ProtoEvent::Enter(opposite_pos),
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter(opposite_pos),
                State::Sending => {
                    let Some(mapped) = self.command_ctrl_mapper.transform(e.clone()) else {
                        // A second physical source (e.g. Command while
                        // Ctrl is already held) did not change the
                        // logical key state, so there is nothing to send.
                        return Ok(());
                    };
                    ProtoEvent::Input(mapped)
                }
            },
            CaptureEvent::AutoRelease => unreachable!("handled in early return above"),
        };

        if let Err(e) = self.conn.send(proto_event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            self.stop_user_activity();
            capture.release().await?;
            return Ok(());
        }

        // Send CursorPos right after Enter so the receiver can warp
        // its cursor to the visually-corresponding point on its own
        // screen — overrides the entry-edge-midpoint warp the
        // receiver otherwise applies on Enter.
        if let Some((pos, nx, ny)) = cursor_pos {
            log::info!("[cursor-pos] send pos={pos:?} nx={nx:.3} ny={ny:.3}");
            if let Err(e) = self
                .conn
                .send(ProtoEvent::CursorPos { pos, nx, ny }, handle)
                .await
            {
                log::warn!("CursorPos send failed: {e}");
            }
        } else if matches!(event, CaptureEvent::Begin { .. }) {
            log::info!(
                "[cursor-pos] send skipped — Begin had no cursor or host_normalized_cursor returned None"
            );
        }
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.notify_peer_of_leave(capture).await;
        capture.release().await
    }

    /// Release path used when the peer is taking over (they sent
    /// Enter+CursorPos). Same teardown — synthesize key-ups, reset
    /// mods, send Leave — but skip the host-side cursor warp so it
    /// doesn't race against the peer's authoritative CursorPos
    /// warp on our shared cursor.
    async fn release_capture_handover(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), CaptureError> {
        self.notify_peer_of_leave(capture).await;
        capture.release_no_host_warp().await
    }

    async fn notify_peer_of_leave(&mut self, capture: &mut InputCapture) {
        self.stop_user_activity();

        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Synthesize key-up events for every logical key still held
            // BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            // With Command->Ctrl enabled, the capture's raw set contains
            // physical Meta/Ctrl keys, while the peer saw collision-
            // coalesced logical Ctrl keys. Never flush the raw aliases:
            // ask the mapper for exactly the logical releases still due.
            let release_keys = self
                .command_ctrl_mapper
                .take_release_keys(capture.take_pressed_keys());
            for key in release_keys {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self
                .conn
                .send(ProtoEvent::Leave(LEAVE_HANDOVER), handle)
                .await
            {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
    }

    fn start_user_activity(&mut self) {
        #[cfg(target_os = "macos")]
        self.user_activity.start();
    }

    fn pulse_user_activity(&mut self) {
        #[cfg(target_os = "macos")]
        self.user_activity.pulse_if_active();
    }

    fn stop_user_activity(&mut self) {
        #[cfg(target_os = "macos")]
        self.user_activity.stop();
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn to_capture_pos(pos: mousehop_ipc::Position) -> input_capture::Position {
    match pos {
        mousehop_ipc::Position::Left => input_capture::Position::Left,
        mousehop_ipc::Position::Right => input_capture::Position::Right,
        mousehop_ipc::Position::Top => input_capture::Position::Top,
        mousehop_ipc::Position::Bottom => input_capture::Position::Bottom,
    }
}

fn to_proto_pos(pos: input_capture::Position) -> mousehop_proto::Position {
    match pos {
        input_capture::Position::Left => mousehop_proto::Position::Left,
        input_capture::Position::Right => mousehop_proto::Position::Right,
        input_capture::Position::Top => mousehop_proto::Position::Top,
        input_capture::Position::Bottom => mousehop_proto::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}

#[cfg(test)]
mod command_ctrl_tests {
    use super::*;
    use scancode::Linux::{KeyA, KeyLeftCtrl, KeyLeftMeta, KeyRightCtrl, KeyRightmeta};

    fn key(key: scancode::Linux, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 17,
            key: key as u32,
            state,
        })
    }

    #[test]
    fn disabled_mapper_is_an_exact_passthrough() {
        let mut mapper = CommandCtrlMapper::default();
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 1)),
            Some(key(KeyLeftMeta, 1))
        );
        let modifiers = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: MOD4_MASK,
            latched: MOD4_MASK,
            locked: MOD4_MASK,
            group: 2,
        });
        assert_eq!(mapper.transform(modifiers.clone()), Some(modifiers));
    }

    #[test]
    fn maps_left_and_right_command_to_same_side_control() {
        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);

        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 1)),
            Some(key(KeyLeftCtrl, 1))
        );
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 0)),
            Some(key(KeyLeftCtrl, 0))
        );
        assert_eq!(
            mapper.transform(key(KeyRightmeta, 1)),
            Some(key(KeyRightCtrl, 1))
        );
        assert_eq!(
            mapper.transform(key(KeyRightmeta, 0)),
            Some(key(KeyRightCtrl, 0))
        );
        assert_eq!(mapper.transform(key(KeyA, 1)), Some(key(KeyA, 1)));
    }

    #[test]
    fn physical_control_and_command_share_one_logical_press() {
        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);

        // Physical Ctrl first: Command adds a source but no second down;
        // releasing Ctrl cannot release the still-held Command alias.
        assert_eq!(
            mapper.transform(key(KeyLeftCtrl, 1)),
            Some(key(KeyLeftCtrl, 1))
        );
        assert_eq!(mapper.transform(key(KeyLeftMeta, 1)), None);
        assert_eq!(mapper.transform(key(KeyLeftCtrl, 0)), None);
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 0)),
            Some(key(KeyLeftCtrl, 0))
        );

        mapper.reset(true);
        // Same guarantee in the opposite press and release order.
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 1)),
            Some(key(KeyLeftCtrl, 1))
        );
        assert_eq!(mapper.transform(key(KeyLeftCtrl, 1)), None);
        assert_eq!(mapper.transform(key(KeyLeftMeta, 0)), None);
        assert_eq!(
            mapper.transform(key(KeyLeftCtrl, 0)),
            Some(key(KeyLeftCtrl, 0))
        );
    }

    #[test]
    fn left_and_right_aliases_are_independent() {
        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);

        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 1)),
            Some(key(KeyLeftCtrl, 1))
        );
        assert_eq!(
            mapper.transform(key(KeyRightmeta, 1)),
            Some(key(KeyRightCtrl, 1))
        );
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 0)),
            Some(key(KeyLeftCtrl, 0))
        );
        assert_eq!(
            mapper.transform(key(KeyRightmeta, 0)),
            Some(key(KeyRightCtrl, 0))
        );
    }

    #[test]
    fn duplicate_and_unmatched_alias_events_do_not_change_logical_state() {
        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);

        assert_eq!(mapper.transform(key(KeyLeftMeta, 0)), None);
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 1)),
            Some(key(KeyLeftCtrl, 1))
        );
        assert_eq!(mapper.transform(key(KeyLeftMeta, 1)), None);
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 0)),
            Some(key(KeyLeftCtrl, 0))
        );
        assert_eq!(mapper.transform(key(KeyLeftMeta, 0)), None);
    }

    #[test]
    fn modifier_masks_alias_mod4_to_control_and_preserve_other_bits() {
        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);
        let shift = 1 << 0;
        let lock = 1 << 1;
        let alt = 1 << 3;
        let unknown = 1 << 12;
        let input = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: MOD4_MASK | shift | alt,
            latched: MOD4_MASK | unknown,
            locked: MOD4_MASK | lock,
            group: 3,
        });
        let expected = Event::Keyboard(KeyboardEvent::Modifiers {
            depressed: CONTROL_MASK | shift | alt,
            latched: CONTROL_MASK | unknown,
            locked: CONTROL_MASK | lock,
            group: 3,
        });
        assert_eq!(mapper.transform(input), Some(expected));

        // An existing physical Control bit naturally collides into the
        // same single logical bit rather than being toggled away.
        assert_eq!(command_mask_as_ctrl(CONTROL_MASK | MOD4_MASK), CONTROL_MASK);
    }

    #[test]
    fn release_cleanup_emits_one_logical_up_and_clears_state() {
        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 1)),
            Some(key(KeyLeftCtrl, 1))
        );
        assert_eq!(mapper.transform(key(KeyLeftCtrl, 1)), None);
        let releases = mapper.take_release_keys([KeyLeftMeta, KeyLeftCtrl, KeyA]);
        assert_eq!(releases.len(), 2);
        assert!(releases.contains(&KeyLeftCtrl));
        assert!(releases.contains(&KeyA));
        assert!(!releases.contains(&KeyLeftMeta));
        assert!(mapper.take_release_keys([]).is_empty());
    }

    #[test]
    fn reset_drops_stale_source_state_for_the_next_capture() {
        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);
        assert_eq!(
            mapper.transform(key(KeyLeftMeta, 1)),
            Some(key(KeyLeftCtrl, 1))
        );
        mapper.reset(true);
        assert_eq!(mapper.transform(key(KeyLeftMeta, 0)), None);
    }
}
