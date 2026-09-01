use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, HostInputState, InputCapture, InputCaptureError,
    Position,
};
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use local_channel::mpsc::{Receiver, Sender, channel};
use mousehop_proto::{
    CAP_ATOMIC_HANDOVER, CAP_TRANSACTIONAL_HANDOVER, HandoverWarpStatus,
    HostInputState as ProtoHostInputState, LEAVE_HANDOVER, LEAVE_RELEASE_ONLY, ProtoEvent,
};
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::{ConnectionSession, MousehopConnection, MousehopConnectionEvent};

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

#[derive(Debug)]
enum CaptureRequest {
    /// release because the remote peer is taking over (they sent
    /// Enter+CursorPos). Skips the host-side warp so the peer's
    /// proportional CursorPos warp doesn't get clobbered by a
    /// racing local warp computed from stale virtual_cursor state.
    ReleaseForHandover(oneshot::Sender<bool>),
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
            active_session: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            host_input_state: HostInputState::Unlocked,
            host_input_generation: 0,
            lock_recovery_client: None,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            release_threshold_px: Rc::new(RefCell::new(release_threshold_px)),
            state: Default::default(),
            command_ctrl_mapper: Default::default(),
            pending_input: Default::default(),
            pending_handover: None,
            pending_leaves: Default::default(),
            active_handover_serial: None,
            modeling_disabled_for: None,
            next_handover_serial: 0,
            peer_capabilities: Default::default(),
            peer_pressed_keys: Default::default(),
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

    pub(crate) fn release_for_handover(&self, completion: oneshot::Sender<bool>) {
        self.request_tx
            .send(CaptureRequest::ReleaseForHandover(completion))
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

#[derive(Clone, Debug, Default)]
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
#[derive(Clone, Debug, Default)]
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
    #[cfg(test)]
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

    #[cfg(test)]
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

#[cfg(test)]
fn is_ctrl_or_command(key: scancode::Linux) -> bool {
    matches!(
        key,
        scancode::Linux::KeyLeftCtrl
            | scancode::Linux::KeyRightCtrl
            | scancode::Linux::KeyLeftMeta
            | scancode::Linux::KeyRightmeta
    )
}

fn update_peer_pressed_keys(pressed: &mut HashSet<scancode::Linux>, event: &Event) {
    let Event::Keyboard(KeyboardEvent::Key { key, state, .. }) = event else {
        return;
    };
    let Ok(key) = scancode::Linux::try_from(*key) else {
        return;
    };
    if *state == 0 {
        pressed.remove(&key);
    } else {
        pressed.insert(key);
    }
}

/// Enter/Ack normally completes in a few milliseconds, but input generated in
/// that window is still semantically lossless. Keep a bounded raw-event queue
/// until the peer acknowledges Enter. Consecutive pointer motion is the only
/// coalesced event type: summing relative deltas preserves its meaning while
/// preventing a fast mouse from crowding out key and button transitions.
const MAX_PENDING_INPUT_EVENTS: usize = 256;

#[derive(Debug, Default)]
struct PendingInput {
    events: VecDeque<Event>,
}

impl PendingInput {
    /// Returns `false` rather than dropping a non-coalescible event when the
    /// bound is reached. The caller then releases capture, preserving the
    /// invariant that an active peer never observes a partial key sequence.
    fn push(&mut self, event: Event) -> bool {
        match event {
            Event::Pointer(PointerEvent::Motion { time, dx, dy }) => {
                if let Some(Event::Pointer(PointerEvent::Motion {
                    time: pending_time,
                    dx: pending_dx,
                    dy: pending_dy,
                })) = self.events.back_mut()
                {
                    *pending_time = time;
                    *pending_dx += dx;
                    *pending_dy += dy;
                    return true;
                }

                if self.events.len() == MAX_PENDING_INPUT_EVENTS {
                    return false;
                }
                self.events
                    .push_back(Event::Pointer(PointerEvent::Motion { time, dx, dy }));
                true
            }
            _event if self.events.len() == MAX_PENDING_INPUT_EVENTS => false,
            event => {
                self.events.push_back(event);
                true
            }
        }
    }

    fn clear(&mut self) {
        self.events.clear();
    }

    /// Apply sender-side aliases only as events leave the Ack gate. The mapper
    /// snapshot lets the caller roll back a wire-visible transition if its
    /// send fails, so release cleanup reflects what the peer actually saw.
    /// Alias collision events that produce no wire event remain committed.
    fn pop_mapped(&mut self, mapper: &mut CommandCtrlMapper) -> Option<(Event, CommandCtrlMapper)> {
        while let Some(event) = self.events.pop_front() {
            let mapper_before = mapper.clone();
            if let Some(mapped) = mapper.transform(event) {
                return Some((mapped, mapper_before));
            }
        }
        None
    }

    #[cfg(test)]
    fn drain_mapped(&mut self, mapper: &mut CommandCtrlMapper) -> Vec<Event> {
        let mut mapped = Vec::new();
        while let Some((event, _)) = self.pop_mapped(mapper) {
            mapped.push(event);
        }
        mapped
    }
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
    /// DTLS generation that owns `active_client`. Every event after Begin is
    /// pinned to it so a replacement connection cannot inherit the crossing.
    active_session: Option<ConnectionSession>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<CaptureRegistration>,
    conn: MousehopConnection,
    event_tx: Sender<ICaptureEvent>,
    /// Last authoritative host lock state seen from the capture backend.
    /// Input is never forwarded while this is `Locked`.
    host_input_state: HostInputState,
    /// Monotonic transition number paired with host-input state on the wire.
    /// Lets the receiver reject an old Locked datagram that arrives after a
    /// newer Unlocked transition.
    host_input_generation: u32,
    /// Peer that was being controlled when the host locked. Retained after
    /// capture is released so a sleeping peer can receive the state when it
    /// reconnects, and so a later confirmed unlock can dismiss its dialog.
    lock_recovery_client: Option<CaptureHandle>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    release_threshold_px: Rc<RefCell<u32>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
    command_ctrl_mapper: CommandCtrlMapper,
    pending_input: PendingInput,
    /// Exact Enter transaction being retried until the matching Ack arrives.
    pending_handover: Option<PendingHandover>,
    /// Transactional cleanup retried until the exact peer serial confirms it.
    pending_leaves: HashMap<(CaptureHandle, ConnectionSession, u32), PendingLeave>,
    /// Nonzero handover serial owning live input for a transactional peer.
    active_handover_serial: Option<u32>,
    /// Exact interval whose receiver reported that cursor warp is unsupported.
    /// Geometry datagrams are ignored for it so a reordered/periodic snapshot
    /// cannot reactivate wall-pressure or return-cursor modeling mid-crossing.
    modeling_disabled_for: Option<(CaptureHandle, ConnectionSession, u32)>,
    /// Nonzero serial source for capability-negotiated atomic handovers.
    next_handover_serial: u32,
    /// Capabilities learned from each exact outbound Hello/session.
    peer_capabilities: HashMap<(CaptureHandle, ConnectionSession), u32>,
    /// Logical keys that were successfully sent down to the active peer.
    /// Local capture state can run ahead of the wire during the Ack gate or a
    /// failed send, so release cleanup must use this wire-visible set.
    peer_pressed_keys: HashSet<scancode::Linux>,
    #[cfg(target_os = "macos")]
    user_activity: crate::macos_power::UserActivity,
}

#[derive(Clone, Debug)]
struct PendingHandover {
    handle: CaptureHandle,
    session: ConnectionSession,
    serial: u32,
    enter: ProtoEvent,
    /// Legacy peers require a separate proportional cursor frame. Atomic
    /// peers carry it inside `enter`, so this is `None` for them.
    legacy_cursor: Option<ProtoEvent>,
    transactional: bool,
}

impl PendingHandover {
    fn accepts_ack(&self, handle: CaptureHandle, session: ConnectionSession, serial: u32) -> bool {
        self.handle == handle && self.session == session && self.serial == serial
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingLeave {
    handle: CaptureHandle,
    session: ConnectionSession,
    serial: u32,
    mode: u32,
}

fn negotiated_capture_session(
    handle: CaptureHandle,
    current_session: Option<ConnectionSession>,
    peer_capabilities: &HashMap<(CaptureHandle, ConnectionSession), u32>,
) -> Option<ConnectionSession> {
    current_session.filter(|session| peer_capabilities.contains_key(&(handle, *session)))
}

impl PendingLeave {
    fn key(&self) -> (CaptureHandle, ConnectionSession, u32) {
        (self.handle, self.session, self.serial)
    }
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

    fn has_capture_at(&self, pos: Position) -> bool {
        self.captures.iter().any(|capture| capture.pos == pos)
    }

    /// Position-keyed peer geometry belongs to the outgoing Default peer,
    /// never to a same-edge EnterOnly registration. After removal, preserve
    /// it only when another Default capture at that edge is actively using it.
    fn should_clear_peer_metadata_after_removal(
        &self,
        pos: Position,
        removed_type: CaptureType,
    ) -> bool {
        if !self.has_capture_at(pos) {
            return true;
        }
        if removed_type != CaptureType::Default {
            return false;
        }
        !self.active_client.is_some_and(|handle| {
            self.captures.iter().any(|capture| {
                capture.handle == handle
                    && capture.pos == pos
                    && capture.capture_type == CaptureType::Default
            })
        })
    }

    fn try_get_pos(&self, handle: CaptureHandle) -> Option<Position> {
        self.captures
            .iter()
            .find(|capture| capture.handle == handle)
            .map(|capture| capture.pos)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.try_get_pos(handle).expect("no such capture")
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

    fn scoped_input(&self, event: Event) -> ProtoEvent {
        match self.active_handover_serial {
            Some(serial) => ProtoEvent::HandoverInput { serial, event },
            None => ProtoEvent::Input(event),
        }
    }

    fn modeling_is_enabled(&self, handle: CaptureHandle, session: ConnectionSession) -> bool {
        !self
            .modeling_disabled_for
            .is_some_and(|(peer, generation, _)| peer == handle && generation == session)
    }

    async fn flush_pending_input(
        &mut self,
        handle: CaptureHandle,
        session: ConnectionSession,
    ) -> bool {
        while let Some((event, mapper_before)) =
            self.pending_input.pop_mapped(&mut self.command_ctrl_mapper)
        {
            let wire_event = self.scoped_input(event.clone());
            match self.conn.send_on_session(wire_event, handle, session).await {
                Ok(()) => update_peer_pressed_keys(&mut self.peer_pressed_keys, &event),
                Err(e) => {
                    self.command_ctrl_mapper = mapper_before;
                    log::warn!("failed to flush input queued before Ack for client {handle}: {e}");
                    return false;
                }
            }
        }
        true
    }

    async fn retry_pending_leaves(&mut self) {
        let retries: Vec<_> = self.pending_leaves.values().copied().collect();
        for pending in retries {
            if let Err(e) = self
                .conn
                .send_on_session(
                    ProtoEvent::HandoverLeave {
                        serial: pending.serial,
                        mode: pending.mode,
                    },
                    pending.handle,
                    pending.session,
                )
                .await
            {
                log::warn!(
                    "leave {} retry for client {} session {} failed: {e}",
                    pending.serial,
                    pending.handle,
                    pending.session
                );
                self.pending_leaves.remove(&pending.key());
            }
        }
    }

    /// Retransmit the exact transaction selected at Begin. The event is never
    /// rebuilt from mutable cursor/topology state: a lost first datagram or
    /// Ack therefore repeats the same serial and landing point, which the
    /// receiver can safely deduplicate.
    async fn retry_pending_handover(&self) -> bool {
        let Some(pending) = self.pending_handover.clone() else {
            return true;
        };
        if let Err(e) = self
            .conn
            .send_on_session(pending.enter, pending.handle, pending.session)
            .await
        {
            log::warn!(
                "handover {} retry for client {} session {} failed: {e}",
                pending.serial,
                pending.handle,
                pending.session
            );
            return false;
        }
        if let Some(cursor) = pending.legacy_cursor {
            if let Err(e) = self
                .conn
                .send_on_session(cursor, pending.handle, pending.session)
                .await
            {
                log::warn!(
                    "legacy CursorPos retry for client {} session {} failed: {e}",
                    pending.handle,
                    pending.session
                );
                return false;
            }
        }
        true
    }

    async fn complete_handover_ack(
        &mut self,
        capture: &mut InputCapture,
        handle: CaptureHandle,
        session: ConnectionSession,
        serial: u32,
        warp: Option<HandoverWarpStatus>,
    ) -> Result<bool, CaptureError> {
        let Some(pending) = self.pending_handover.as_ref() else {
            return Ok(false);
        };
        if !pending.accepts_ack(handle, session, serial) || pending.transactional != warp.is_some()
        {
            return Ok(false);
        }

        if warp == Some(HandoverWarpStatus::Unsupported) {
            if let Some(pos) = self.try_get_pos(handle) {
                capture.clear_peer_bounds(pos);
                capture.clear_peer_sensitivity(pos);
            }
            self.modeling_disabled_for = Some((handle, session, serial));
            log::info!(
                "client {handle} session {session} cannot warp; disabling synchronized cursor modeling for handover {serial}"
            );
        }

        log::info!("client {handle} session {session} acknowledged handover {serial}");
        self.pending_handover = None;
        self.state = State::Sending;
        // Flush before returning to select so no live input can overtake the
        // snapshot/keystrokes captured in the Enter/Ack window.
        if !self.flush_pending_input(handle, session).await {
            self.clear_failed_session(handle, session);
            capture.release().await?;
        }
        Ok(true)
    }

    fn allocate_handover_serial(&mut self) -> u32 {
        self.next_handover_serial = self.next_handover_serial.wrapping_add(1);
        if self.next_handover_serial == 0 {
            self.next_handover_serial = 1;
        }
        self.next_handover_serial
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
                        CaptureRequest::ReleaseForHandover(completion) => {
                            // No live capture backend means there is no local
                            // grab to order before the receiver's warp.
                            let _ = completion.send(true);
                        }
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
        // ordinary release paths below. Complete peer cleanup while the
        // transport and backend are still alive: otherwise keepalive traffic
        // can leave the receiver holding wire-visible keys indefinitely even
        // though this capture stream has already ended.
        self.stop_user_activity();
        let cleanup_result = if self.active_client.is_some() {
            log::warn!("capture session exited while forwarding; releasing peer and local grab");
            self.release_capture(&mut capture).await
        } else {
            Ok(())
        };

        // FIXME replace with async drop when stabilized
        let terminate_result = capture.terminate().await;

        // Preserve the original stream/session error as the primary failure,
        // but never skip cleanup or backend termination while doing so.
        if let Err(error) = r {
            if let Err(cleanup_error) = cleanup_result {
                log::warn!("capture cleanup after session error also failed: {cleanup_error}");
            }
            if let Err(terminate_error) = terminate_result {
                log::warn!(
                    "capture termination after session error also failed: {terminate_error}"
                );
            }
            return Err(error);
        }
        cleanup_result?;
        terminate_result?;
        Ok(())
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
        let mut lock_recovery_tick = tokio::time::interval(Duration::from_secs(2));
        lock_recovery_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut handover_retry_tick = tokio::time::interval(Duration::from_millis(100));
        handover_retry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = user_activity_tick.tick() => self.pulse_user_activity(),
                _ = lock_recovery_tick.tick() => {
                    let _ = self.retry_lock_recovery().await;
                },
                _ = handover_retry_tick.tick() => {
                    if self.state == State::WaitingForAck
                        && self.pending_handover.is_some()
                        && !self.retry_pending_handover().await
                    {
                        let failed = self.pending_handover.as_ref().map(|pending| {
                            (pending.handle, pending.session)
                        });
                        if let Some((handle, session)) = failed {
                            self.clear_failed_session(handle, session);
                        }
                        capture.release().await?;
                    }
                    self.retry_pending_leaves().await;
                },
                event = capture.next() => match event {
                    Some(event) => self.handle_capture_event(capture, event?).await?,
                    None => return Ok(()),
                },
                connection_event = self.conn.recv() => {
                    let (handle, session, event) = match connection_event {
                        MousehopConnectionEvent::Received { handle, session, event, .. } => {
                            (handle, session, event)
                        }
                        MousehopConnectionEvent::Disconnected { handle, session } => {
                            self.peer_capabilities.remove(&(handle, session));
                            self.pending_leaves.retain(|_, pending| {
                                pending.handle != handle || pending.session != session
                            });
                            if self
                                .modeling_disabled_for
                                .is_some_and(|(peer, generation, _)| {
                                    peer == handle && generation == session
                                })
                            {
                                self.modeling_disabled_for = None;
                            }
                            if self.clear_failed_session(handle, session) {
                                log::info!(
                                    "releasing capture: active client {handle} disconnected"
                                );
                                capture.release().await?;
                            }
                            continue;
                        }
                    };
                    if let Some(active) = self.active_client {
                        if handle != active
                            && !matches!(
                                &event,
                                ProtoEvent::Hello { .. }
                                    | ProtoEvent::HostInputStateAck { .. }
                                    | ProtoEvent::HandoverLeave { .. }
                                    | ProtoEvent::HandoverLeaveAck { .. }
                            )
                        {
                            // Capture events belong to the active client, but
                            // connection and recovery control-plane events can
                            // arrive from the retained recovery peer.
                            continue
                        }
                    }

                    match event {
                        ProtoEvent::Ack(serial) => {
                            let completed = self.state == State::WaitingForAck
                                && self.active_client == Some(handle)
                                && self.active_session == Some(session)
                                && self
                                    .complete_handover_ack(
                                        capture,
                                        handle,
                                        session,
                                        serial,
                                        None,
                                    )
                                    .await?;
                            if !completed {
                                log::debug!(
                                    "ignoring stale/mismatched Ack({serial}) from client {handle} session {session}"
                                );
                            }
                        }
                        ProtoEvent::HandoverAck { serial, warp } => {
                            let completed = self.state == State::WaitingForAck
                                && self.active_client == Some(handle)
                                && self.active_session == Some(session)
                                && self
                                    .complete_handover_ack(
                                        capture,
                                        handle,
                                        session,
                                        serial,
                                        Some(warp),
                                    )
                                    .await?;
                            if !completed {
                                log::debug!(
                                    "ignoring stale/mismatched HandoverAck({serial}) from client {handle} session {session}"
                                );
                            }
                        }
                        ProtoEvent::HandoverLeaveAck { serial } => {
                            let was_pending = self
                                .pending_leaves
                                .remove(&(handle, session, serial))
                                .is_some();
                            if was_pending {
                                log::debug!(
                                    "client {handle} session {session} acknowledged leave {serial}"
                                );
                            }
                        }
                        ProtoEvent::OwnershipLost { serial }
                            if self.active_client == Some(handle)
                                && self.active_session == Some(session)
                                && self.active_handover_serial == Some(serial) =>
                        {
                            log::warn!(
                                "client {handle} session {session} lost ownership of handover {serial}; releasing capture"
                            );
                            self.release_capture(capture).await?;
                        }
                        ProtoEvent::OwnershipLost { serial } => {
                            log::debug!(
                                "ignoring ownership loss for stale handover {serial} from client {handle} session {session}"
                            );
                        }
                        ProtoEvent::HostInputStateAck { state, generation }
                            if completes_lock_recovery(
                                self.host_input_state,
                                self.host_input_generation,
                                self.lock_recovery_client,
                                handle,
                                state,
                                generation,
                            ) =>
                        {
                            // Keep the recovery target through Locked so it
                            // can receive a later Unlocked. Clear it only after
                            // the receiver explicitly confirms that it removed
                            // its locked-input gate.
                            log::info!(
                                "client {handle} acknowledged host-input recovery completion"
                            );
                            self.lock_recovery_client = None;
                        }
                        ProtoEvent::HostInputStateAck { .. } => {}
                        // A legacy Leave(0), or any future mode this
                        // receiver doesn't understand, retains the
                        // handover behavior: skip the host warp because
                        // the peer's Enter+CursorPos may be racing it on
                        // the shared cursor. A new peer can explicitly
                        // report a one-way EnterOnly return with
                        // LEAVE_RELEASE_ONLY; no CursorPos will follow, so
                        // the modeled host warp must be applied.
                        ProtoEvent::Leave(mode) if self.active_handover_serial.is_none() => {
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
                        ProtoEvent::Leave(mode) => {
                            log::debug!(
                                "ignoring unscoped Leave({mode}) during transactional handover"
                            );
                        }
                        ProtoEvent::HandoverLeave { serial, mode } => {
                            let owns_capture = self.active_client == Some(handle)
                                && self.active_session == Some(session)
                                && self.active_handover_serial == Some(serial);
                            if owns_capture {
                                if mode == LEAVE_RELEASE_ONLY {
                                    log::info!(
                                        "releasing capture: peer returned through a transactional release-only edge"
                                    );
                                    self.release_capture(capture).await?;
                                } else {
                                    if mode != LEAVE_HANDOVER {
                                        log::debug!(
                                            "unknown transactional Leave mode {mode}; treating it as a handover"
                                        );
                                    }
                                    log::info!(
                                        "releasing capture: peer is taking over handover {serial}"
                                    );
                                    self.release_capture_handover(capture).await?;
                                }
                            } else {
                                log::debug!(
                                    "acknowledging stale transactional Leave({serial}) without releasing current capture"
                                );
                            }
                            if let Err(e) = self
                                .conn
                                .send_on_session(
                                    ProtoEvent::HandoverLeaveAck { serial },
                                    handle,
                                    session,
                                )
                                .await
                            {
                                log::debug!(
                                    "failed to acknowledge transactional Leave({serial}): {e}"
                                );
                            }
                        }
                        // Peer reported its display geometry — cache it
                        // so the wall-press model has a real upper
                        // clamp on virtual_pos for this position.
                        ProtoEvent::Bounds { width, height }
                            if self.active_client == Some(handle)
                                && self.modeling_is_enabled(handle, session) =>
                        {
                            if let Some(pos) = self.try_get_pos(handle) {
                                capture.set_peer_bounds(pos, width, height);
                            }
                        }
                        // Current peers also report every real display
                        // rectangle. Bounds remains the fallback for older
                        // versions; topology keeps the modeled cursor out of
                        // empty space in stepped multi-monitor layouts.
                        ProtoEvent::DisplayLayout {
                            epoch,
                            generation,
                            layout,
                            ..
                        } if self.active_client == Some(handle)
                            && self.modeling_is_enabled(handle, session) => {
                            if let Some(pos) = self.try_get_pos(handle) {
                                capture.set_peer_layout(pos, epoch, generation, layout);
                            }
                        }
                        // Peer reported its per-pair receive-side
                        // sensitivity multiplier — feed it into the
                        // wall-press model so its delta accumulator
                        // tracks the receiver's actual cursor advance.
                        // Without this, a sub-1.0 multiplier on the
                        // receiver makes the host's auto-release model
                        // fire before the cursor reaches the wall.
                        ProtoEvent::ReceiverSensitivity { mouse_sensitivity }
                            if self.active_client == Some(handle)
                                && self.modeling_is_enabled(handle, session) =>
                        {
                            if let Some(pos) = self.try_get_pos(handle) {
                                capture.set_peer_sensitivity(pos, mouse_sensitivity);
                            }
                        }
                        // Peer's commit hash arrived on the outgoing
                        // DTLS connection. The connect-side
                        // receive_loop already wrote it to
                        // `client_manager`; bubble up to Service so
                        // the GUI's version-status row refreshes.
                        ProtoEvent::Hello { capabilities, .. } => {
                            self.peer_capabilities.retain(|(peer, generation), _| {
                                *peer != handle || *generation == session
                            });
                            self.peer_capabilities
                                .insert((handle, session), capabilities);
                            // Same-handle reconnects supersede the old DTLS
                            // generation. Their stale Disconnect is filtered
                            // at the transport, so the new Hello is also the
                            // authoritative signal to release any old grab.
                            if self.active_client == Some(handle)
                                && self.active_session.is_some_and(|active| active != session)
                            {
                                log::info!(
                                    "releasing capture: client {handle} replaced session {:?} with {session}",
                                    self.active_session
                                );
                                let old_session = self.active_session.expect("checked above");
                                self.clear_failed_session(handle, old_session);
                                capture.release().await?;
                            }
                            self.event_tx
                                .send(ICaptureEvent::PeerCommitUpdated(handle))
                                .expect("channel closed");
                            let _ = self.retry_lock_recovery_for(handle).await;
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
                    CaptureRequest::ReleaseForHandover(completion) => {
                        let result = self.release_capture_handover(capture).await;
                        let _ = completion.send(result.is_ok());
                        result?;
                    },
                    CaptureRequest::Create(h, p, t, command_as_ctrl) => {
                        self.add_capture(h, p, t, command_as_ctrl);
                        capture.create(h, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        let removed_type = self.get_type(h);
                        if self.lock_recovery_client == Some(h) {
                            self.lock_recovery_client = None;
                        }
                        if self.active_client == Some(h) {
                            // Finish the complete sender-side session before
                            // removing the handle-to-position mapping.  If the
                            // backend has to tear down an active barrier while
                            // processing Destroy, its later AutoRelease can no
                            // longer be routed through a removed handle and
                            // the peer would retain held keys/state.
                            log::info!(
                                "releasing capture: active client {h} is being removed"
                            );
                            self.release_capture(capture).await?;
                        }
                        let pos = self.get_pos(h);
                        self.remove_capture(h);
                        capture.destroy(h).await?;
                        if self.should_clear_peer_metadata_after_removal(pos, removed_type) {
                            // An EnterOnly registration can remain on the same
                            // edge after its outgoing Default peer is replaced;
                            // it must not keep the old peer's epoch/layout alive.
                            // Conversely, removing an unrelated inactive
                            // registration must not erase an active Default's
                            // return-warp and wall-pressure model.
                            capture.clear_peer_bounds(pos);
                            capture.clear_peer_sensitivity(pos);
                        }
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

        if let CaptureEvent::HostInputState(state) = &event {
            return self.handle_host_input_state(capture, *state).await;
        }

        // A backend event already queued when the lock transition arrived can
        // race behind the lifecycle notification. Once lock is confirmed,
        // discard every non-lifecycle event until a confirmed unlock; never
        // allow a stale Begin or key event to recreate forwarding.
        if self.host_input_state == HostInputState::Locked {
            log::debug!("discarding capture event while host input is locked");
            capture.release().await?;
            return Ok(());
        }

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

        let opposite_pos = to_proto_pos(self.get_pos(handle).opposite());

        // The backend computed this from the same topology snapshot that
        // produced Begin. Do not re-query here: the event may have crossed a
        // thread/channel while a monitor hotplug committed a newer layout.
        let cursor_pos = if let CaptureEvent::Begin {
            normalized_cursor: Some((nx, ny)),
            ..
        } = event
        {
            let pos = self.get_pos(handle);
            let proto_pos = to_proto_pos(pos.opposite());
            Some((proto_pos, nx, ny))
        } else {
            None
        };

        // A crossing cannot be transactionally pinned until the outbound
        // transport exists and Capture has consumed that exact session's
        // Hello. Pong can make the transport sendable before the queued Hello
        // reaches this select loop; treating missing capabilities as legacy in
        // that window makes a current peer reject the resulting unscoped
        // input. Preserve the first-crossing dial trigger only when there is
        // no transport at all. If a transport exists but Hello is still
        // queued, release without sending and let the next crossing use the
        // negotiated framing.
        let begin_session = if matches!(event, CaptureEvent::Begin { .. }) {
            let current_session = self.conn.current_session(handle);
            match negotiated_capture_session(handle, current_session, &self.peer_capabilities) {
                Some(session) => Some(session),
                None if current_session.is_none() => {
                    let _ = self
                        .conn
                        .send(ProtoEvent::Enter(opposite_pos), handle)
                        .await;
                    log::info!(
                        "releasing capture: client {handle} has no established DTLS session yet"
                    );
                    capture.release().await?;
                    return Ok(());
                }
                None => {
                    log::info!(
                        "releasing capture: waiting for client {handle} session {:?} Hello capabilities",
                        current_session.expect("checked above")
                    );
                    capture.release().await?;
                    return Ok(());
                }
            }
        } else {
            None
        };

        // Every fresh Begin starts a new acknowledgement and mapping
        // session, including same-handle re-entry after a backend-side
        // release or send failure. `active_client` can outlive those
        // release paths, so gating this reset on a handle change would
        // retain stale held-source state and incorrectly stay Sending.
        if matches!(event, CaptureEvent::Begin { .. }) {
            // Send Unlocked directly before a new Enter as well as on the
            // recovery timer. DTLS rides UDP, so send success is not proof the
            // receiver removed its locked-input gate; only the dedicated
            // HostInputStateAck completes recovery.
            if self.host_input_state == HostInputState::Unlocked {
                let _ = self.retry_lock_recovery().await;
            }
            self.start_user_activity();
            self.state = State::WaitingForAck;
            self.pending_input.clear();
            self.pending_handover = None;
            if !self.peer_pressed_keys.is_empty() {
                if let (Some(previous_handle), Some(previous_session)) =
                    (self.active_client, self.active_session)
                {
                    log::warn!(
                        "new capture began while client {previous_handle} still had wire-visible keys held; releasing them before re-entry"
                    );
                    self.release_peer_keys(
                        previous_handle,
                        previous_session,
                        self.active_handover_serial,
                    )
                    .await;
                } else {
                    self.peer_pressed_keys.clear();
                }
            }
            // Snapshot the mapping for this capture session. A config
            // change mid-chord is deliberately deferred until the next
            // Begin, and hand-edited settings are ignored off macOS so
            // Linux Super never changes meaning unexpectedly.
            let command_as_ctrl = cfg!(target_os = "macos") && self.command_as_ctrl(handle);
            self.command_ctrl_mapper.reset(command_as_ctrl);
            let changed_client = self.active_client.replace(handle) != Some(handle);
            // Use the exact session whose Hello readiness was checked above.
            // If it is replaced during one of the preceding async cleanup
            // operations, send_on_session rejects it rather than falling
            // through to the unnegotiated replacement.
            self.active_session = begin_session;
            if changed_client {
                self.event_tx
                    .send(ICaptureEvent::ClientEntered(handle))
                    .expect("channel closed");
            }
        }

        let (proto_event, mapper_before_send) = match &event {
            CaptureEvent::Begin { .. } => {
                let session = self
                    .active_session
                    .expect("Begin session was checked before capture activation");
                let supports_atomic = self
                    .peer_capabilities
                    .get(&(handle, session))
                    .is_some_and(|capabilities| capabilities & CAP_ATOMIC_HANDOVER != 0);
                let supports_transactional = self
                    .peer_capabilities
                    .get(&(handle, session))
                    .is_some_and(|capabilities| capabilities & CAP_TRANSACTIONAL_HANDOVER != 0);
                let (serial, enter, legacy_cursor) = if supports_atomic {
                    let serial = self.allocate_handover_serial();
                    let cross_fraction = cursor_pos.map(|(pos, nx, ny)| match pos {
                        mousehop_proto::Position::Left | mousehop_proto::Position::Right => ny,
                        mousehop_proto::Position::Top | mousehop_proto::Position::Bottom => nx,
                    });
                    (
                        serial,
                        ProtoEvent::HandoverEnter {
                            serial,
                            pos: opposite_pos,
                            cross_fraction,
                        },
                        None,
                    )
                } else {
                    (
                        0,
                        ProtoEvent::Enter(opposite_pos),
                        cursor_pos.map(|(pos, nx, ny)| ProtoEvent::CursorPos { pos, nx, ny }),
                    )
                };
                self.active_handover_serial = supports_transactional.then_some(serial);
                self.modeling_disabled_for = None;
                self.pending_handover = Some(PendingHandover {
                    handle,
                    session,
                    serial,
                    enter: enter.clone(),
                    legacy_cursor,
                    transactional: supports_transactional,
                });
                (enter, None)
            }
            CaptureEvent::Input(e) => match self.state {
                // Preserve raw input until acknowledgement, while retaining
                // the old Enter retransmission behavior on each event. Raw
                // queueing is important: Command-as-Control must update its
                // collision state only when the event can be sent.
                State::WaitingForAck => {
                    if !self.pending_input.push(e.clone()) {
                        log::warn!(
                            "pending input reached {MAX_PENDING_INPUT_EVENTS} events before Ack; \
                             releasing capture rather than dropping input"
                        );
                        self.release_capture(capture).await?;
                        return Ok(());
                    }
                    let Some(pending) = self.pending_handover.as_ref() else {
                        log::warn!("Ack-gated input has no pending handover; releasing capture");
                        self.release_capture(capture).await?;
                        return Ok(());
                    };
                    (pending.enter.clone(), None)
                }
                State::Sending => {
                    let mapper_before = self.command_ctrl_mapper.clone();
                    let Some(mapped) = self.command_ctrl_mapper.transform(e.clone()) else {
                        // A second physical source (e.g. Command while
                        // Ctrl is already held) did not change the
                        // logical key state, so there is nothing to send.
                        return Ok(());
                    };
                    (self.scoped_input(mapped), Some(mapper_before))
                }
            },
            CaptureEvent::AutoRelease => unreachable!("handled in early return above"),
            CaptureEvent::HostInputState(_) => {
                unreachable!("handled in early return above")
            }
        };

        let sent_input = match &proto_event {
            ProtoEvent::Input(event) | ProtoEvent::HandoverInput { event, .. } => {
                Some(event.clone())
            }
            _ => None,
        };
        let session = self
            .active_session
            .expect("active capture must retain its DTLS session");
        if let Err(e) = self
            .conn
            .send_on_session(proto_event, handle, session)
            .await
        {
            if let Some(mapper_before) = mapper_before_send {
                self.command_ctrl_mapper = mapper_before;
            }
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            // On a transport error, `send()` tears down the established
            // connection and queues a Disconnected event. On NotConnected,
            // there is likewise no peer that can receive cleanup. Release
            // synchronously here without starting/reusing a connection merely
            // to send key-ups/Leave for the failed session.
            self.clear_failed_session(handle, session);
            capture.release().await?;
            return Ok(());
        }
        if let Some(event) = sent_input {
            update_peer_pressed_keys(&mut self.peer_pressed_keys, &event);
        }

        // Legacy compatibility only: atomic peers already received this
        // landing fraction in the transaction above.
        let legacy_cursor = if matches!(event, CaptureEvent::Begin { .. }) {
            self.pending_handover
                .as_ref()
                .and_then(|pending| pending.legacy_cursor.clone())
        } else {
            None
        };
        if let Some(cursor_event @ ProtoEvent::CursorPos { pos, nx, ny }) = legacy_cursor {
            log::info!("[cursor-pos] legacy send pos={pos:?} nx={nx:.3} ny={ny:.3}");
            if let Err(e) = self
                .conn
                .send_on_session(cursor_event, handle, session)
                .await
            {
                log::warn!("CursorPos send failed; releasing capture: {e}");
                self.clear_failed_session(handle, session);
                capture.release().await?;
                return Ok(());
            }
        } else if matches!(event, CaptureEvent::Begin { .. })
            && self
                .pending_handover
                .as_ref()
                .is_some_and(|pending| matches!(pending.enter, ProtoEvent::Enter(_)))
        {
            log::info!(
                "[cursor-pos] send skipped — Begin had no cursor or host_normalized_cursor returned None"
            );
        }
        Ok(())
    }

    async fn handle_host_input_state(
        &mut self,
        capture: &mut InputCapture,
        state: HostInputState,
    ) -> Result<(), CaptureError> {
        if state == self.host_input_state {
            return Ok(());
        }
        self.host_input_state = state;
        self.host_input_generation = self.host_input_generation.wrapping_add(1);

        match state {
            HostInputState::Locked => {
                // Prefer the peer being controlled at the instant of lock. An
                // active handle can survive a send failure while that peer is
                // asleep, which is exactly the recovery case this state must
                // retain across reconnection.
                if let Some(active) = self.active_client {
                    self.lock_recovery_client = Some(active);
                }
                let _ = self.retry_lock_recovery().await;
                log::info!("host locked; releasing outgoing capture");
                self.release_capture(capture).await?;
            }
            HostInputState::Unlocked => {
                // Inform the same peer, but deliberately do not restart the
                // old capture. The user must make a fresh edge crossing.
                let _ = self.retry_lock_recovery().await;
            }
        }
        Ok(())
    }

    async fn retry_lock_recovery(&mut self) -> bool {
        let Some(handle) = self.lock_recovery_client else {
            return false;
        };
        self.retry_lock_recovery_for(handle).await
    }

    async fn retry_lock_recovery_for(&mut self, handle: CaptureHandle) -> bool {
        if self.lock_recovery_client != Some(handle) {
            return false;
        }
        let wire_state = to_proto_host_input_state(self.host_input_state);
        match self
            .conn
            .send(
                ProtoEvent::HostInputState {
                    state: wire_state,
                    generation: self.host_input_generation,
                },
                handle,
            )
            .await
        {
            Ok(()) => true,
            Err(e) => {
                log::debug!("host-input state for recovery client {handle} not delivered yet: {e}");
                false
            }
        }
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        self.notify_peer_of_leave().await;
        capture.release().await
    }

    /// Clear sender-side session state when delivery fails or the active
    /// outbound connection disappears. There is no confirmed peer left to
    /// receive key-ups or `Leave`, so this path intentionally avoids
    /// [`Self::notify_peer_of_leave`], which would otherwise start a new
    /// connection while trying to clean up the failed one. The caller performs
    /// the local backend release.
    fn clear_failed_session(&mut self, handle: CaptureHandle, session: ConnectionSession) -> bool {
        if self.active_client != Some(handle) || self.active_session != Some(session) {
            return false;
        }

        self.stop_user_activity();
        self.active_client = None;
        self.active_session = None;
        self.active_handover_serial = None;
        self.modeling_disabled_for = None;
        self.pending_input.clear();
        self.pending_handover = None;
        self.pending_leaves
            .retain(|_, pending| pending.handle != handle || pending.session != session);
        self.state = State::WaitingForAck;
        self.peer_pressed_keys.clear();
        self.command_ctrl_mapper.reset(false);
        true
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
        self.notify_peer_of_leave().await;
        capture.release_no_host_warp().await
    }

    async fn release_peer_keys(
        &mut self,
        handle: CaptureHandle,
        session: ConnectionSession,
        handover_serial: Option<u32>,
    ) {
        let keys = self.peer_pressed_keys.drain().collect::<Vec<_>>();
        for key in keys {
            let event = Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: key as u32,
                state: 0,
            });
            let key_up = match handover_serial {
                Some(serial) => ProtoEvent::HandoverInput { serial, event },
                None => ProtoEvent::Input(event),
            };
            if let Err(e) = self.conn.send_on_session(key_up, handle, session).await {
                log::warn!("failed to send key-up to client {handle}: {e}");
            }
        }
    }

    async fn notify_peer_of_leave(&mut self) {
        self.stop_user_activity();
        self.pending_input.clear();
        self.pending_handover = None;
        self.state = State::WaitingForAck;

        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            let session = self.active_session.take();
            let handover_serial = self.active_handover_serial.take();
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
            // Release exactly what reached the peer. The local capture set may
            // already contain a matching up that failed to send, or may hold
            // raw Command while the peer received an aliased Control.
            if let Some(session) = session {
                self.release_peer_keys(handle, session, handover_serial)
                    .await;
                // Reset the modifier mask too. The peer's input-emulation
                // layer keeps a separate XKB-style modifier state that's
                // updated by KeyboardEvent::Modifiers, distinct from the
                // pressed_keys set drained above. Without this, an
                // already-locked CapsLock would survive the release.
                let mods_zero_event = Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0,
                });
                let mods_zero = match handover_serial {
                    Some(serial) => ProtoEvent::HandoverInput {
                        serial,
                        event: mods_zero_event,
                    },
                    None => ProtoEvent::Input(mods_zero_event),
                };
                if let Err(e) = self.conn.send_on_session(mods_zero, handle, session).await {
                    log::warn!("failed to reset modifiers on client {handle}: {e}");
                }

                log::info!("sending Leave event to client {handle} session {session}");
                let leave = match handover_serial {
                    Some(serial) => {
                        let pending = PendingLeave {
                            handle,
                            session,
                            serial,
                            mode: LEAVE_HANDOVER,
                        };
                        self.pending_leaves.insert(pending.key(), pending);
                        ProtoEvent::HandoverLeave {
                            serial,
                            mode: pending.mode,
                        }
                    }
                    None => ProtoEvent::Leave(LEAVE_HANDOVER),
                };
                if let Err(e) = self.conn.send_on_session(leave, handle, session).await {
                    log::warn!("failed to send Leave to client {handle}: {e}");
                    if let Some(serial) = handover_serial {
                        self.pending_leaves.remove(&(handle, session, serial));
                    }
                }
            } else {
                self.peer_pressed_keys.clear();
            }
        }
        self.active_session = None;
        self.active_handover_serial = None;
        self.modeling_disabled_for = None;
        self.peer_pressed_keys.clear();
        // Keep neither source state nor the previous session's mapping after
        // Leave; the next Begin snapshots both from scratch.
        self.command_ctrl_mapper.reset(false);
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

fn to_proto_host_input_state(state: HostInputState) -> ProtoHostInputState {
    match state {
        HostInputState::Unlocked => ProtoHostInputState::Unlocked,
        HostInputState::Locked => ProtoHostInputState::Locked,
    }
}

fn completes_lock_recovery(
    host_input_state: HostInputState,
    host_input_generation: u32,
    recovery_client: Option<CaptureHandle>,
    ack_handle: CaptureHandle,
    ack_state: ProtoHostInputState,
    ack_generation: u32,
) -> bool {
    host_input_state == HostInputState::Unlocked
        && recovery_client == Some(ack_handle)
        && ack_state == ProtoHostInputState::Unlocked
        && ack_generation == host_input_generation
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
    use scancode::Linux::{
        KeyA, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift, KeyRightCtrl, KeyRightmeta,
    };

    fn capture_task(active_client: Option<CaptureHandle>) -> CaptureTask {
        let cert = webrtc_dtls::crypto::Certificate::generate_self_signed([
            "mousehop-disconnect-test".to_owned(),
        ])
        .expect("test certificate");
        let conn = MousehopConnection::new(cert, Default::default(), Default::default());
        let (event_tx, _event_rx) = channel();
        let (_request_tx, request_rx) = channel();
        CaptureTask {
            active_client,
            active_session: active_client.map(|_| 11),
            backend: None,
            cancellation_token: CancellationToken::new(),
            captures: Vec::new(),
            conn,
            event_tx,
            host_input_state: HostInputState::Unlocked,
            host_input_generation: 0,
            lock_recovery_client: None,
            release_bind: Default::default(),
            release_threshold_px: Default::default(),
            request_rx,
            state: State::WaitingForAck,
            command_ctrl_mapper: Default::default(),
            pending_input: Default::default(),
            pending_handover: None,
            pending_leaves: Default::default(),
            active_handover_serial: None,
            modeling_disabled_for: None,
            next_handover_serial: 0,
            peer_capabilities: Default::default(),
            peer_pressed_keys: Default::default(),
            #[cfg(target_os = "macos")]
            user_activity: Default::default(),
        }
    }

    fn key(key: scancode::Linux, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 17,
            key: key as u32,
            state,
        })
    }

    #[test]
    fn disconnect_clears_only_the_matching_active_capture_session() {
        let mut task = capture_task(Some(7));
        task.state = State::Sending;
        task.active_handover_serial = Some(29);
        task.modeling_disabled_for = Some((7, 11, 29));
        let pending_leave = PendingLeave {
            handle: 7,
            session: 11,
            serial: 28,
            mode: LEAVE_HANDOVER,
        };
        task.pending_leaves
            .insert(pending_leave.key(), pending_leave);
        assert!(task.pending_input.push(key(KeyA, 1)));
        task.command_ctrl_mapper.reset(true);
        task.peer_pressed_keys.insert(KeyA);

        assert!(!task.clear_failed_session(8, 11));
        assert!(!task.clear_failed_session(7, 12));
        assert_eq!(task.active_client, Some(7));
        assert_eq!(task.state, State::Sending);
        assert!(!task.pending_input.events.is_empty());
        assert!(task.command_ctrl_mapper.enabled);
        assert!(task.peer_pressed_keys.contains(&KeyA));

        assert!(task.clear_failed_session(7, 11));
        assert_eq!(task.active_client, None);
        assert_eq!(task.active_session, None);
        assert_eq!(task.active_handover_serial, None);
        assert_eq!(task.modeling_disabled_for, None);
        assert!(task.pending_leaves.is_empty());
        assert_eq!(task.state, State::WaitingForAck);
        assert!(task.pending_input.events.is_empty());
        assert!(!task.command_ctrl_mapper.enabled);
        assert!(task.peer_pressed_keys.is_empty());
    }

    #[test]
    fn handover_ack_requires_exact_handle_session_and_serial() {
        let pending = PendingHandover {
            handle: 7,
            session: 11,
            serial: 29,
            enter: ProtoEvent::HandoverEnter {
                serial: 29,
                pos: mousehop_proto::Position::Left,
                cross_fraction: Some(0.25),
            },
            legacy_cursor: None,
            transactional: false,
        };

        assert!(pending.accepts_ack(7, 11, 29));
        assert!(!pending.accepts_ack(8, 11, 29));
        assert!(!pending.accepts_ack(7, 12, 29));
        assert!(!pending.accepts_ack(7, 11, 28));
        assert!(!pending.accepts_ack(7, 11, 0));
    }

    #[test]
    fn current_build_scopes_input_and_leave_to_the_exact_handover() {
        let mut task = capture_task(Some(7));
        task.active_handover_serial = Some(29);
        assert!(matches!(
            task.scoped_input(key(KeyA, 1)),
            ProtoEvent::HandoverInput {
                serial: 29,
                event: Event::Keyboard(KeyboardEvent::Key { key, state: 1, .. }),
            } if key == KeyA as u32
        ));

        let pending = PendingLeave {
            handle: 7,
            session: 11,
            serial: 29,
            mode: LEAVE_HANDOVER,
        };
        assert_eq!(pending.key(), (7, 11, 29));

        task.modeling_disabled_for = Some((7, 11, 29));
        assert!(!task.modeling_is_enabled(7, 11));
        assert!(task.modeling_is_enabled(7, 12));

        let other = PendingLeave {
            handle: 8,
            session: 12,
            serial: 30,
            mode: LEAVE_HANDOVER,
        };
        task.pending_leaves.insert(pending.key(), pending);
        task.pending_leaves.insert(other.key(), other);
        assert!(task.pending_leaves.remove(&pending.key()).is_some());
        assert_eq!(task.pending_leaves.get(&other.key()), Some(&other));
    }

    #[test]
    fn handover_serial_allocator_skips_reserved_zero() {
        let mut task = capture_task(None);
        task.next_handover_serial = u32::MAX;
        assert_eq!(task.allocate_handover_serial(), 1);
        assert_eq!(task.allocate_handover_serial(), 2);
    }

    #[test]
    fn pong_cannot_make_capture_ready_before_exact_session_hello_is_consumed() {
        let handle = 7;
        let session = 11;
        let replacement = 12;
        let mut peer_capabilities = HashMap::new();

        // `Some(session)` models the DTLS transport already being selected and
        // Pong having made it sendable. Until Capture consumes Hello into its
        // own exact-session map, Begin must release without sending anything.
        assert_eq!(
            negotiated_capture_session(handle, Some(session), &peer_capabilities),
            None
        );

        peer_capabilities.insert((handle, session), 0);
        assert_eq!(
            negotiated_capture_session(handle, Some(session), &peer_capabilities),
            Some(session),
            "a consumed zero-capability Hello must still ready a legacy peer"
        );
        peer_capabilities.insert((handle, session), CAP_TRANSACTIONAL_HANDOVER);
        assert_eq!(
            negotiated_capture_session(handle, Some(session), &peer_capabilities),
            Some(session)
        );
        assert_eq!(
            negotiated_capture_session(handle, Some(replacement), &peer_capabilities),
            None,
            "an old session's Hello must not ready its replacement"
        );
    }

    #[test]
    fn removing_one_same_edge_handle_keeps_position_registration() {
        let mut task = capture_task(Some(7));
        task.add_capture(7, Position::Right, CaptureType::Default, false);
        task.add_capture(8, Position::Right, CaptureType::EnterOnly, false);

        task.remove_capture(8);

        assert!(task.has_capture_at(Position::Right));
        assert_eq!(task.try_get_pos(7), Some(Position::Right));
        assert_eq!(task.try_get_pos(8), None);
    }

    #[test]
    fn removing_default_clears_metadata_even_when_enter_only_remains() {
        let mut task = capture_task(Some(7));
        task.add_capture(7, Position::Right, CaptureType::Default, false);
        task.add_capture(8, Position::Right, CaptureType::EnterOnly, false);

        let removed_type = task.get_type(7);
        task.active_client = None;
        task.remove_capture(7);

        assert!(task.has_capture_at(Position::Right));
        assert!(task.should_clear_peer_metadata_after_removal(Position::Right, removed_type));
    }

    #[test]
    fn removing_inactive_same_edge_registration_preserves_active_default_metadata() {
        let mut task = capture_task(Some(7));
        task.add_capture(7, Position::Right, CaptureType::Default, false);
        task.add_capture(8, Position::Right, CaptureType::EnterOnly, false);

        let removed_type = task.get_type(8);
        task.remove_capture(8);

        assert!(!task.should_clear_peer_metadata_after_removal(Position::Right, removed_type));
    }

    #[test]
    fn removed_handle_metadata_is_ignored_without_position_lookup_panic() {
        let mut task = capture_task(None);
        task.add_capture(7, Position::Right, CaptureType::Default, false);
        task.remove_capture(7);

        assert_eq!(task.try_get_pos(7), None);
        assert_ne!(task.active_client, Some(7));
    }

    fn modifiers(depressed: u32) -> Event {
        Event::Keyboard(KeyboardEvent::Modifiers {
            depressed,
            latched: 0,
            locked: 0,
            group: 0,
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

    #[test]
    fn ack_gate_preserves_held_snapshot_and_fast_keys_in_order() {
        let expected = vec![
            key(KeyLeftShift, 1),
            modifiers(1 << 0),
            key(KeyA, 1),
            key(KeyA, 0),
            key(KeyLeftShift, 0),
            modifiers(0),
        ];
        let mut pending = PendingInput::default();
        for event in expected.iter().cloned() {
            assert!(pending.push(event));
        }

        let mut mapper = CommandCtrlMapper::default();
        assert_eq!(pending.drain_mapped(&mut mapper), expected);
        assert!(pending.events.is_empty());
    }

    #[test]
    fn ack_gate_applies_command_alias_once_for_same_side_overlap() {
        let mut pending = PendingInput::default();
        // This is the crossing snapshot order: physical Control precedes
        // physical Command. Both are held on the same side, so the peer must
        // see one logical Control press and one release, not duplicate edges.
        for event in [
            key(KeyLeftCtrl, 1),
            key(KeyLeftMeta, 1),
            modifiers(CONTROL_MASK | MOD4_MASK),
            key(KeyA, 1),
            key(KeyA, 0),
            key(KeyLeftMeta, 0),
            key(KeyLeftCtrl, 0),
            modifiers(0),
        ] {
            assert!(pending.push(event));
        }

        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);
        assert_eq!(
            pending.drain_mapped(&mut mapper),
            vec![
                key(KeyLeftCtrl, 1),
                modifiers(CONTROL_MASK),
                key(KeyA, 1),
                key(KeyA, 0),
                key(KeyLeftCtrl, 0),
                modifiers(0),
            ]
        );
        assert!(mapper.take_releases().is_empty());
    }

    #[test]
    fn ack_gate_failed_wire_transition_can_restore_release_state() {
        let mut pending = PendingInput::default();
        assert!(pending.push(key(KeyLeftMeta, 1)));
        assert!(pending.push(key(KeyLeftMeta, 0)));

        let mut mapper = CommandCtrlMapper::default();
        mapper.reset(true);

        // The down reached the peer.
        let (down, _) = pending.pop_mapped(&mut mapper).expect("mapped down");
        assert_eq!(down, key(KeyLeftCtrl, 1));

        // Model a failed send of the mapped up. Restoring the per-event
        // snapshot makes Leave cleanup synthesize the Ctrl-up the peer is
        // still owed.
        let (up, mapper_before_failed_send) = pending.pop_mapped(&mut mapper).expect("mapped up");
        assert_eq!(up, key(KeyLeftCtrl, 0));
        mapper = mapper_before_failed_send;
        assert_eq!(mapper.take_releases(), vec![KeyLeftCtrl]);
    }

    #[test]
    fn wire_pressed_state_survives_a_failed_key_up() {
        let mut pressed = HashSet::new();
        update_peer_pressed_keys(&mut pressed, &key(KeyA, 1));
        assert!(pressed.contains(&KeyA));

        // A failed send does not call update_peer_pressed_keys, so cleanup
        // still knows the peer is owed an up even though local capture has
        // already observed the physical release.
        assert!(pressed.contains(&KeyA));
        update_peer_pressed_keys(&mut pressed, &key(KeyA, 0));
        assert!(pressed.is_empty());

        update_peer_pressed_keys(&mut pressed, &modifiers(CONTROL_MASK));
        assert!(pressed.is_empty());
    }

    #[test]
    fn ack_gate_coalesces_only_consecutive_pointer_motion() {
        let mut pending = PendingInput::default();
        assert!(pending.push(Event::Pointer(PointerEvent::Motion {
            time: 1,
            dx: 2.0,
            dy: 3.0,
        })));
        assert!(pending.push(Event::Pointer(PointerEvent::Motion {
            time: 2,
            dx: -1.0,
            dy: 4.0,
        })));
        assert!(pending.push(key(KeyA, 1)));
        assert!(pending.push(Event::Pointer(PointerEvent::Motion {
            time: 3,
            dx: 8.0,
            dy: 9.0,
        })));

        let mut mapper = CommandCtrlMapper::default();
        assert_eq!(
            pending.drain_mapped(&mut mapper),
            vec![
                Event::Pointer(PointerEvent::Motion {
                    time: 2,
                    dx: 1.0,
                    dy: 7.0,
                }),
                key(KeyA, 1),
                Event::Pointer(PointerEvent::Motion {
                    time: 3,
                    dx: 8.0,
                    dy: 9.0,
                }),
            ]
        );
    }

    #[test]
    fn ack_gate_reports_full_without_mutating_existing_events() {
        let mut pending = PendingInput::default();
        for state in 0..MAX_PENDING_INPUT_EVENTS {
            assert!(pending.push(key(KeyA, (state % 2) as u8)));
        }
        assert!(!pending.push(key(KeyLeftShift, 1)));
        assert_eq!(pending.events.len(), MAX_PENDING_INPUT_EVENTS);

        pending.clear();
        assert!(pending.events.is_empty());
    }
}

#[cfg(test)]
mod lock_recovery_tests {
    use super::*;

    #[test]
    fn only_matching_unlocked_ack_completes_recovery() {
        let handle = 42;
        assert!(completes_lock_recovery(
            HostInputState::Unlocked,
            9,
            Some(handle),
            handle,
            ProtoHostInputState::Unlocked,
            9,
        ));
        assert!(!completes_lock_recovery(
            HostInputState::Locked,
            8,
            Some(handle),
            handle,
            ProtoHostInputState::Locked,
            8,
        ));
        assert!(!completes_lock_recovery(
            HostInputState::Unlocked,
            9,
            Some(handle),
            handle,
            ProtoHostInputState::Locked,
            8,
        ));
        assert!(!completes_lock_recovery(
            HostInputState::Unlocked,
            9,
            Some(handle),
            handle + 1,
            ProtoHostInputState::Unlocked,
            9,
        ));
        assert!(!completes_lock_recovery(
            HostInputState::Unlocked,
            9,
            Some(handle),
            handle,
            ProtoHostInputState::Unlocked,
            8,
        ));
    }
}
