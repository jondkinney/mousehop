use crate::config::local_commit;
use crate::listen::{ListenEvent, ListenerCreationError, ListenerSession, MousehopListener};
use futures::StreamExt;
use input_emulation::{
    EdgeWarpOutcome, EmulationHandle, InputEmulation, InputEmulationError, ReceivePostProcessing,
};
use input_event::{
    ClipboardEvent, Event,
    display::{DisplayEdge, DisplayLayout},
};
use local_channel::mpsc::{Receiver, Sender, channel};
use mousehop_ipc::IncomingPeerConfig;
use mousehop_proto::{
    CAP_TRANSACTIONAL_HANDOVER, HandoverWarpStatus, HostInputState, LEAVE_HANDOVER,
    LEAVE_RELEASE_ONLY, Position, ProtoEvent,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    select,
    sync::oneshot,
    task::{JoinHandle, spawn_local},
};

fn to_pp(peer: &IncomingPeerConfig) -> ReceivePostProcessing {
    ReceivePostProcessing {
        natural_scroll: peer.natural_scroll,
        mouse_sensitivity: peer.mouse_sensitivity,
    }
}

fn update_locked_host(
    locked_hosts: &mut HashSet<SocketAddr>,
    addr: SocketAddr,
    state: HostInputState,
) -> bool {
    match state {
        HostInputState::Locked => locked_hosts.insert(addr),
        HostInputState::Unlocked => {
            locked_hosts.remove(&addr);
            false
        }
    }
}

fn remote_input_allowed(locked_hosts: &HashSet<SocketAddr>, addr: SocketAddr) -> bool {
    !locked_hosts.contains(&addr)
}

fn remote_session_owns_input(
    locked_hosts: &HashSet<SocketAddr>,
    owners: &HashMap<(SocketAddr, ListenerSession), u32>,
    addr: SocketAddr,
    session: ListenerSession,
) -> bool {
    remote_input_allowed(locked_hosts, addr) && owners.contains_key(&(addr, session))
}

fn remote_transaction_owns_input(
    locked_hosts: &HashSet<SocketAddr>,
    owners: &HashMap<(SocketAddr, ListenerSession), u32>,
    addr: SocketAddr,
    session: ListenerSession,
    serial: u32,
) -> bool {
    remote_input_allowed(locked_hosts, addr)
        && owners.get(&(addr, session)).copied() == Some(serial)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransactionalInputDisposition {
    Consume,
    ReportOwnershipLost,
}

fn classify_transactional_input(
    locked_hosts: &HashSet<SocketAddr>,
    owners: &HashMap<(SocketAddr, ListenerSession), u32>,
    addr: SocketAddr,
    session: ListenerSession,
    serial: u32,
) -> TransactionalInputDisposition {
    if remote_transaction_owns_input(locked_hosts, owners, addr, session, serial) {
        TransactionalInputDisposition::Consume
    } else {
        TransactionalInputDisposition::ReportOwnershipLost
    }
}

fn heartbeat_ownership_loss(
    peer_capabilities: &HashMap<(SocketAddr, ListenerSession), u32>,
    owners: &HashMap<(SocketAddr, ListenerSession), u32>,
    addr: SocketAddr,
    session: ListenerSession,
) -> Option<ProtoEvent> {
    peer_capabilities
        .get(&(addr, session))
        .is_some_and(|capabilities| capabilities & CAP_TRANSACTIONAL_HANDOVER != 0)
        .then(|| owners.get(&(addr, session)).copied())
        .flatten()
        .map(|serial| ProtoEvent::OwnershipLost { serial })
}

fn refresh_topology_generation(
    last_layout: &mut Option<DisplayLayout>,
    generation: &mut u32,
    current_layout: &Option<DisplayLayout>,
) {
    let Some(current_layout) = current_layout else {
        // A transient unavailable query must not consume a generation or
        // forget the last good snapshot. The emulation backend retains its
        // previous complete layout until a new one is ready.
        return;
    };
    if last_layout.as_ref() != Some(current_layout) {
        *generation = generation.wrapping_add(1);
        *last_layout = Some(current_layout.clone());
    }
}

fn accept_host_input_state(
    latest_states: &mut HashMap<SocketAddr, (u32, HostInputState)>,
    addr: SocketAddr,
    state: HostInputState,
    generation: u32,
) -> bool {
    match latest_states.get(&addr).copied() {
        None => {
            latest_states.insert(addr, (generation, state));
            true
        }
        Some((current_generation, current_state)) if generation == current_generation => {
            // Repeated state is an expected retry after a lost acknowledgement.
            // A different state with the same generation is contradictory and
            // must not mutate the input gate.
            state == current_state
        }
        Some((current_generation, _))
            if generation.wrapping_sub(current_generation) < (1 << 31) =>
        {
            latest_states.insert(addr, (generation, state));
            true
        }
        Some(_) => false,
    }
}

fn serial_is_newer(candidate: u32, current: u32) -> bool {
    candidate != current && candidate.wrapping_sub(current) < (1 << 31)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandoverDisposition {
    Apply,
    Reack,
    DropStale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompletedHandover {
    serial: u32,
    warp: HandoverWarpStatus,
    layout: Option<DisplayLayout>,
    topology_generation: u32,
}

fn classify_handover(completed: Option<u32>, candidate: u32) -> HandoverDisposition {
    match completed {
        None => HandoverDisposition::Apply,
        Some(current) if candidate == current => HandoverDisposition::Reack,
        Some(current) if serial_is_newer(candidate, current) => HandoverDisposition::Apply,
        Some(_) => HandoverDisposition::DropStale,
    }
}

fn handover_leave_revokes(completed: Option<u32>, owner: Option<u32>, candidate: u32) -> bool {
    match classify_handover(completed, candidate) {
        HandoverDisposition::Apply => true,
        HandoverDisposition::Reack => owner == Some(candidate),
        HandoverDisposition::DropStale => false,
    }
}

fn entry_edge(pos: Position) -> DisplayEdge {
    match pos {
        Position::Left => DisplayEdge::Left,
        Position::Right => DisplayEdge::Right,
        Position::Top => DisplayEdge::Top,
        Position::Bottom => DisplayEdge::Bottom,
    }
}

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        /// Exact inbound DTLS generation that replaced any prior connection
        /// from the same source address.
        session: ListenerSession,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// Exact inbound DTLS generation that owns this barrier.
        session: ListenerSession,
        /// position of the connection
        pos: mousehop_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
        session: ListenerSession,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify(oneshot::Sender<bool>),
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
    },
    /// Authenticated peer reported a confirmed lock/unlock transition. The
    /// receiver uses this only for recovery guidance; no input or password is
    /// accepted in response.
    RemoteHostState {
        addr: SocketAddr,
        fingerprint: String,
        state: HostInputState,
    },
    /// Authorized peer at `addr` delivered a clipboard frame whose
    /// receive-side gate evaluated true. The local clipboard has
    /// already been updated by [`ListenTask`]; Service consumes
    /// this event to refresh the `ClipboardMonitor`'s
    /// last-known-content (so the next poll doesn't re-emit it as a
    /// fresh local change) and to fan the payload out to other
    /// authorized peers whose `clipboard_send` is true.
    /// `from_fingerprint` is the *originator*'s certificate
    /// fingerprint stamped on the wire — distinct from the
    /// fingerprint of the peer at `addr` when the message has
    /// been forwarded through an intermediate hop.
    ClipboardReceived {
        addr: SocketAddr,
        from_fingerprint: String,
        content: String,
    },
}

enum EmulationRequest {
    Reenable,
    Release {
        addr: SocketAddr,
        session: ListenerSession,
        handover: bool,
    },
    ChangePort(u16),
    /// Replace the per-fingerprint receive-side post-processing
    /// table. Service pushes this on startup, on every authorization
    /// change, and whenever the user adjusts a peer's natural-scroll
    /// or sensitivity from the GUI.
    SetIncomingPeers(HashMap<String, IncomingPeerConfig>),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: MousehopListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
            addr_to_fingerprint: HashMap::new(),
            incoming_peers: HashMap::new(),
            locked_hosts: HashSet::new(),
            latest_host_input_states: HashMap::new(),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn send_leave_event(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
        handover: bool,
    ) {
        self.request_tx
            .send(EmulationRequest::Release {
                addr,
                session,
                handover,
            })
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    /// Push the latest authorized-peers table to the receive
    /// pipeline. Calls fire-and-forget; ListenTask resolves
    /// per-fingerprint settings against its addr→fingerprint cache
    /// and pushes per-handle post-processing into InputEmulation.
    pub(crate) fn set_incoming_peers(&self, peers: HashMap<String, IncomingPeerConfig>) {
        self.request_tx
            .send(EmulationRequest::SetIncomingPeers(peers))
            .expect("channel closed")
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: MousehopListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
    /// addr→fingerprint cache populated from `ListenEvent::Accept`.
    /// Lets ListenTask resolve `IncomingPeerConfig` for an incoming
    /// peer without a per-packet round-trip into the listener.
    addr_to_fingerprint: HashMap<SocketAddr, String>,
    /// Latest authorized-peers map pushed by Service. Read on Accept
    /// and on `SetIncomingPeers` to build the per-handle
    /// `ReceivePostProcessing` snapshots that go into InputEmulation.
    incoming_peers: HashMap<String, IncomingPeerConfig>,
    /// Peers that have authoritatively reported themselves locked. Input from
    /// these addresses is dropped until a confirmed unlock, guarding against
    /// queued/reordered datagrams after the sender released capture.
    locked_hosts: HashSet<SocketAddr>,
    /// Latest ordered host-input transition per authenticated DTLS session.
    /// A generation number prevents a delayed Locked retry from overriding a
    /// newer Unlocked transition after recovery has completed.
    latest_host_input_states: HashMap<SocketAddr, (u32, HostInputState)>,
}

impl ListenTask {
    fn post_processing_for_addr(&self, addr: SocketAddr) -> ReceivePostProcessing {
        self.addr_to_fingerprint
            .get(&addr)
            .and_then(|fp| self.incoming_peers.get(fp))
            .map(to_pp)
            .unwrap_or_default()
    }

    fn session_is_current(&self, addr: SocketAddr, session: ListenerSession) -> bool {
        self.listener.current_session(addr) == Some(session)
    }

    async fn request_capture_release(&self) -> bool {
        let (completion, completed) = oneshot::channel();
        self.event_tx
            .send(EmulationEvent::ReleaseNotify(completion))
            .expect("channel closed");
        tokio::time::timeout(Duration::from_secs(2), completed)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_entry_metadata(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
        post_processing: ReceivePostProcessing,
        bounds: Option<(u32, u32)>,
        layout: Option<DisplayLayout>,
        topology_epoch: u64,
        topology_generation: u32,
    ) -> bool {
        if !self.session_is_current(addr, session) {
            return false;
        }
        self.listener
            .reply(
                addr,
                session,
                ProtoEvent::ReceiverSensitivity {
                    mouse_sensitivity: post_processing.mouse_sensitivity,
                },
            )
            .await;
        if !self.session_is_current(addr, session) {
            return false;
        }
        if let Some((width, height)) = bounds {
            self.listener
                .reply(addr, session, ProtoEvent::Bounds { width, height })
                .await;
            if !self.session_is_current(addr, session) {
                return false;
            }
        }
        if let Some(layout) = layout {
            self.listener
                .reply(
                    addr,
                    session,
                    ProtoEvent::display_layout_generation(
                        layout,
                        topology_epoch,
                        topology_generation,
                    ),
                )
                .await;
            if !self.session_is_current(addr, session) {
                return false;
            }
        }
        true
    }

    async fn reply_handover_ack(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
        serial: u32,
        transactional: bool,
        warp: HandoverWarpStatus,
    ) {
        let event = if transactional {
            ProtoEvent::HandoverAck { serial, warp }
        } else {
            ProtoEvent::Ack(serial)
        };
        self.listener.reply(addr, session, event).await;
    }

    async fn run(mut self) {
        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(5));
        let mut topology_interval = tokio::time::interval(Duration::from_secs(2));
        let mut leave_retry_interval = tokio::time::interval(Duration::from_millis(100));
        leave_retry_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_response = HashMap::new();
        let mut rejected_connections = HashMap::new();
        let mut last_topology = None;
        let mut topology_generation = 0u32;
        let mut completed_handovers: HashMap<(SocketAddr, ListenerSession), CompletedHandover> =
            HashMap::new();
        let mut legacy_enter_ready: HashSet<(SocketAddr, ListenerSession)> = HashSet::new();
        let mut input_owner_sessions: HashMap<(SocketAddr, ListenerSession), u32> = HashMap::new();
        let mut peer_capabilities: HashMap<(SocketAddr, ListenerSession), u32> = HashMap::new();
        let mut pending_leaves: HashMap<(SocketAddr, ListenerSession, u32), u32> = HashMap::new();
        // Distinguish topology counters across daemon/peer restarts. A queued
        // datagram from an old same-address read loop can otherwise establish
        // a high generation after Begin and make the restarted sender's gen=1
        // refreshes look stale forever.
        let topology_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr, session }) => {
                        log::trace!("{event} <-<-<-<-<- {addr} session {session}");
                        last_response.insert(addr, (session, Instant::now()));
                        match event {
                            ProtoEvent::Enter(pos) => {
                                if !remote_input_allowed(&self.locked_hosts, addr) {
                                    log::debug!("dropping Enter from locked host {addr}");
                                } else if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr, session).await {
                                    if !self.session_is_current(addr, session) {
                                        continue;
                                    }
                                    log::info!("releasing capture before legacy entry from {addr} session {session}");
                                    if !self.request_capture_release().await
                                        || !self.session_is_current(addr, session)
                                    {
                                        log::debug!("legacy entry was superseded while capture released");
                                        continue;
                                    }
                                    // A lost prior Leave must not let a fresh
                                    // crossing inherit held keys in the existing
                                    // backend handle.
                                    self.emulation_proxy.remove(addr);
                                    let pp = self.post_processing_for_addr(addr);
                                    let layout = self.emulation_proxy.refresh_display_layout().await;
                                    let bounds = layout.as_ref().and_then(DisplayLayout::size);
                                    refresh_topology_generation(
                                        &mut last_topology,
                                        &mut topology_generation,
                                        &layout,
                                    );
                                    if !self.send_entry_metadata(
                                        addr,
                                        session,
                                        pp,
                                        bounds,
                                        layout,
                                        topology_epoch,
                                        topology_generation,
                                    ).await {
                                        continue;
                                    }
                                    legacy_enter_ready.insert((addr, session));
                                    input_owner_sessions.insert((addr, session), 0);
                                    self.listener.reply(addr, session, ProtoEvent::Ack(0)).await;
                                    if self.session_is_current(addr, session) {
                                        self.event_tx.send(EmulationEvent::Entered {
                                            addr,
                                            session,
                                            pos: to_ipc_pos(pos),
                                            fingerprint,
                                        }).expect("channel closed");
                                    }
                                }
                            }
                            ProtoEvent::HandoverEnter {
                                serial,
                                pos,
                                cross_fraction,
                            } => {
                                let key = (addr, session);
                                let transactional = peer_capabilities
                                    .get(&key)
                                    .is_some_and(|capabilities| {
                                        capabilities & CAP_TRANSACTIONAL_HANDOVER != 0
                                    });
                                if !remote_input_allowed(&self.locked_hosts, addr) {
                                    log::debug!("dropping handover {serial} from locked host {addr}");
                                    continue;
                                }
                                match classify_handover(
                                    completed_handovers.get(&key).map(|completed| completed.serial),
                                    serial,
                                ) {
                                    HandoverDisposition::Reack => {
                                        // The release and warp already completed; only the Ack
                                        // was lost. Never apply either side effect twice. Entry
                                        // metadata is independently lossy, though, so repeat its
                                        // idempotent snapshot before repeating the Ack. Do not
                                        // resurrect a transaction whose local return barrier has
                                        // already revoked receive-side ownership.
                                        if input_owner_sessions.get(&key).copied() != Some(serial) {
                                            log::debug!(
                                                "withholding Ack for revoked handover {serial} from {addr} session {session}"
                                            );
                                            continue;
                                        }
                                        let completed = completed_handovers
                                            .get(&key)
                                            .expect("classified completed handover")
                                            .clone();
                                        let pp = self.post_processing_for_addr(addr);
                                        // Preserve the exact geometry used by
                                        // the original warp. A hotplug after a
                                        // lost Ack must not relabel a different
                                        // topology as the one that was applied.
                                        let layout = completed.layout.clone();
                                        let bounds = layout.as_ref().and_then(DisplayLayout::size);
                                        if self
                                            .send_entry_metadata(
                                                addr,
                                                session,
                                                pp,
                                                bounds,
                                                layout,
                                                topology_epoch,
                                                completed.topology_generation,
                                            )
                                            .await
                                        {
                                            self.reply_handover_ack(
                                                addr,
                                                session,
                                                serial,
                                                transactional,
                                                completed.warp,
                                            )
                                            .await;
                                        }
                                        continue;
                                    }
                                    HandoverDisposition::DropStale => {
                                        log::debug!(
                                            "dropping stale handover {serial} from {addr} session {session}"
                                        );
                                        continue;
                                    }
                                    HandoverDisposition::Apply => {}
                                }
                                let Some(fingerprint) = self
                                    .listener
                                    .get_certificate_fingerprint(addr, session)
                                    .await
                                else {
                                    continue;
                                };
                                if !self.session_is_current(addr, session) {
                                    continue;
                                }
                                log::info!(
                                    "releasing capture before atomic handover {serial} from {addr} session {session}"
                                );
                                if !self.request_capture_release().await
                                    || !self.session_is_current(addr, session)
                                {
                                    log::debug!(
                                        "handover {serial} was superseded while capture released"
                                    );
                                    continue;
                                }
                                // Never carry pressed-key state from a prior
                                // crossing into this newer transaction when its
                                // Leave was lost.
                                self.emulation_proxy.remove(addr);
                                let (layout, warp) = if let Some(cross_fraction) = cross_fraction {
                                    let edge = entry_edge(pos);
                                    match self
                                        .emulation_proxy
                                        .warp_cursor_to_edge(edge, f64::from(cross_fraction))
                                        .await
                                    {
                                        Some(EdgeWarpOutcome::Applied(layout)) => {
                                            (Some(layout), HandoverWarpStatus::Applied)
                                        }
                                        Some(EdgeWarpOutcome::Unsupported) => {
                                            // Some emulation backends cannot place the cursor,
                                            // but can still report useful geometry for the peer.
                                            (
                                                self.emulation_proxy.refresh_display_layout().await,
                                                HandoverWarpStatus::Unsupported,
                                            )
                                        }
                                        None => {
                                            log::warn!(
                                                "handover {serial} cursor warp did not complete; withholding Ack"
                                            );
                                            continue;
                                        }
                                    }
                                } else {
                                    (
                                        self.emulation_proxy.refresh_display_layout().await,
                                        HandoverWarpStatus::NotRequested,
                                    )
                                };
                                if !self.session_is_current(addr, session) {
                                    continue;
                                }
                                let pp = self.post_processing_for_addr(addr);
                                let bounds = layout.as_ref().and_then(DisplayLayout::size);
                                refresh_topology_generation(
                                    &mut last_topology,
                                    &mut topology_generation,
                                    &layout,
                                );
                                let completed_layout = layout.clone();
                                if !self.send_entry_metadata(
                                    addr,
                                    session,
                                    pp,
                                    bounds,
                                    layout,
                                    topology_epoch,
                                    topology_generation,
                                ).await {
                                    continue;
                                }
                                input_owner_sessions.insert(key, serial);
                                completed_handovers.insert(
                                    key,
                                    CompletedHandover {
                                        serial,
                                        warp,
                                        layout: completed_layout,
                                        topology_generation,
                                    },
                                );
                                self.reply_handover_ack(
                                    addr,
                                    session,
                                    serial,
                                    transactional,
                                    warp,
                                )
                                .await;
                                if self.session_is_current(addr, session) {
                                    self.event_tx.send(EmulationEvent::Entered {
                                        addr,
                                        session,
                                        pos: to_ipc_pos(pos),
                                        fingerprint,
                                    }).expect("channel closed");
                                }
                            }
                            ProtoEvent::Leave(_)
                                if !peer_capabilities
                                    .get(&(addr, session))
                                    .is_some_and(|capabilities| {
                                        capabilities & CAP_TRANSACTIONAL_HANDOVER != 0
                                    }) =>
                            {
                                self.emulation_proxy.remove(addr);
                                legacy_enter_ready.remove(&(addr, session));
                                input_owner_sessions.remove(&(addr, session));
                            }
                            ProtoEvent::Leave(mode) => {
                                log::debug!(
                                    "dropping unscoped Leave({mode}) from transactional peer {addr} session {session}"
                                );
                            }
                            ProtoEvent::HandoverLeave { serial, .. } => {
                                let key = (addr, session);
                                let disposition = classify_handover(
                                    completed_handovers
                                        .get(&key)
                                        .map(|completed| completed.serial),
                                    serial,
                                );
                                let revoke = handover_leave_revokes(
                                    completed_handovers
                                        .get(&key)
                                        .map(|completed| completed.serial),
                                    input_owner_sessions.get(&key).copied(),
                                    serial,
                                );
                                match disposition {
                                    HandoverDisposition::Apply => {
                                        completed_handovers.insert(
                                            key,
                                            CompletedHandover {
                                                serial,
                                                warp: HandoverWarpStatus::NotRequested,
                                                layout: None,
                                                topology_generation,
                                            },
                                        );
                                    }
                                    HandoverDisposition::Reack | HandoverDisposition::DropStale => {}
                                }
                                if revoke {
                                    self.emulation_proxy.remove(addr);
                                    legacy_enter_ready.remove(&key);
                                    input_owner_sessions.remove(&key);
                                }
                                self.listener
                                    .reply(
                                        addr,
                                        session,
                                        ProtoEvent::HandoverLeaveAck { serial },
                                    )
                                    .await;
                            }
                            ProtoEvent::HandoverLeaveAck { serial } => {
                                pending_leaves.remove(&(addr, session, serial));
                            }
                            ProtoEvent::Input(event) => {
                                let transactional = peer_capabilities
                                    .get(&(addr, session))
                                    .is_some_and(|capabilities| {
                                        capabilities & CAP_TRANSACTIONAL_HANDOVER != 0
                                    });
                                if transactional {
                                    log::debug!(
                                        "dropping unscoped input from transactional peer {addr} session {session}"
                                    );
                                } else if !remote_session_owns_input(
                                    &self.locked_hosts,
                                    &input_owner_sessions,
                                    addr,
                                    session,
                                ) {
                                    log::trace!(
                                        "dropping input without active ownership from {addr} session {session}"
                                    );
                                } else {
                                    self.emulation_proxy.consume(event, addr);
                                }
                            }
                            ProtoEvent::HandoverInput { serial, event } => {
                                if classify_transactional_input(
                                    &self.locked_hosts,
                                    &input_owner_sessions,
                                    addr,
                                    session,
                                    serial,
                                ) == TransactionalInputDisposition::Consume
                                {
                                    self.emulation_proxy.consume(event, addr);
                                } else {
                                    log::debug!(
                                        "rejecting input for unowned handover {serial} from {addr} session {session}"
                                    );
                                    self.listener
                                        .reply(
                                            addr,
                                            session,
                                            ProtoEvent::OwnershipLost { serial },
                                        )
                                        .await;
                                }
                            }
                            ProtoEvent::Clipboard { from_fingerprint, content } => {
                                let receive_ok = self.addr_to_fingerprint
                                    .get(&addr)
                                    .and_then(|fp| self.incoming_peers.get(fp))
                                    .map(|peer| peer.clipboard_receive)
                                    .unwrap_or(false);
                                if !receive_ok {
                                    log::debug!(
                                        "dropping clipboard frame from {addr}: clipboard_receive disabled or unauthorized peer"
                                    );
                                } else {
                                    // Inject locally via the same
                                    // pipeline that handles input
                                    // events. InputEmulation::consume
                                    // short-circuits Clipboard events
                                    // to its ClipboardEmulation sink.
                                    self.emulation_proxy.consume(
                                        Event::Clipboard(ClipboardEvent::Text(content.clone())),
                                        addr,
                                    );
                                    // Hand off to Service so it can
                                    // (a) suppress an immediate self-
                                    // emit from the local
                                    // ClipboardMonitor poll, and (b)
                                    // forward to other peers honoring
                                    // the (originator, content)
                                    // recent-forwarded gate.
                                    self.event_tx.send(EmulationEvent::ClipboardReceived {
                                        addr,
                                        from_fingerprint,
                                        content,
                                    }).expect("channel closed");
                                }
                            }
                            ProtoEvent::HostInputState { state, generation } => {
                                if let Some(fingerprint) =
                                    self.addr_to_fingerprint.get(&addr).cloned()
                                {
                                    if !accept_host_input_state(
                                        &mut self.latest_host_input_states,
                                        addr,
                                        state,
                                        generation,
                                    ) {
                                        log::warn!(
                                            "dropping stale or contradictory host-input state from {addr}: {state:?} generation {generation}"
                                        );
                                    } else {
                                        if update_locked_host(&mut self.locked_hosts, addr, state) {
                                            // Independent teardown in case the
                                            // sender's following Leave or key-up
                                            // datagrams are lost.
                                            self.emulation_proxy.remove(addr);
                                            input_owner_sessions.remove(&(addr, session));
                                        }
                                        self.event_tx
                                            .send(EmulationEvent::RemoteHostState {
                                                addr,
                                                fingerprint,
                                                state,
                                            })
                                            .expect("channel closed");
                                        self.listener
                                            .reply(
                                                addr,
                                                session,
                                                ProtoEvent::HostInputStateAck {
                                                    state,
                                                    generation,
                                                },
                                            )
                                            .await;
                                    }
                                } else {
                                    log::warn!(
                                        "dropping host-input state from unmapped peer {addr}"
                                    );
                                }
                            }
                            ProtoEvent::Ping => self.listener.reply(addr, session, ProtoEvent::Pong(self.emulation_proxy.emulation_active.get())).await,
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello {
                                commit,
                                capabilities,
                                ..
                            } => {
                                peer_capabilities.insert((addr, session), capabilities);
                                self.listener.reply(addr, session, ProtoEvent::hello(local_commit())).await;
                                self.event_tx.send(EmulationEvent::PeerHello { addr, commit }).expect("channel closed");
                            }
                            // Capturing peer told us where on its own
                            // screen the user's cursor was, as a
                            // normalized fraction (nx, ny) ∈ [0, 1]
                            // plus the entry side (from our frame).
                            // Scale against our live display bounds
                            // and pin the on-axis dimension to the
                            // matching edge so the cursor lands at
                            // the visually-corresponding point.
                            // Works without a prior Bounds round-trip,
                            // so the very first crossing of a session
                            // also lands at the visually-corresponding
                            // point. The cross-axis multiply is
                            // clamped to dim - 1 so a host edge
                            // (nx == 1.0 or ny == 1.0) doesn't compute
                            // one pixel past the addressable column.
                            ProtoEvent::CursorPos { pos, nx, ny } => {
                                if !remote_input_allowed(&self.locked_hosts, addr) {
                                    log::trace!("dropping cursor warp from locked host {addr}");
                                } else if !legacy_enter_ready.contains(&(addr, session)) {
                                    // A legacy CursorPos can be reordered ahead
                                    // of Enter. Never warp until local capture
                                    // release for that exact session completed.
                                    log::debug!(
                                        "dropping cursor-first legacy warp from {addr} session {session}"
                                    );
                                } else {
                                    let (edge, cross_fraction) = match pos {
                                        Position::Left => (DisplayEdge::Left, ny),
                                        Position::Right => (DisplayEdge::Right, ny),
                                        Position::Top => (DisplayEdge::Top, nx),
                                        Position::Bottom => (DisplayEdge::Bottom, nx),
                                    };
                                    log::info!(
                                        "[cursor-pos] recv pos={pos:?} nx={nx:.3} ny={ny:.3} — projecting onto {edge:?} display contour"
                                    );
                                    let _ = self
                                        .emulation_proxy
                                        .warp_cursor_to_edge(edge, f64::from(cross_fraction))
                                        .await;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, session, fingerprint }) => {
                        // A reused UDP address is a distinct authenticated
                        // session. Do not let the old heartbeat make it look
                        // responsive before this connection speaks.
                        last_response.remove(&addr);
                        completed_handovers.retain(|(candidate, _), _| *candidate != addr);
                        legacy_enter_ready.retain(|(candidate, _)| *candidate != addr);
                        input_owner_sessions.retain(|(candidate, _), _| *candidate != addr);
                        peer_capabilities.retain(|(candidate, _), _| *candidate != addr);
                        pending_leaves.retain(|(candidate, _, _), _| *candidate != addr);
                        // Destroy any handle owned by the prior authenticated
                        // session before a same-address replacement can send
                        // input. This releases its pressed keys in the backend.
                        self.emulation_proxy.forget(addr);
                        log::debug!("accepted inbound DTLS session {session} from {addr}");
                        // A new authenticated session starts in unknown/unlocked
                        // transport state. A still-locked sender immediately
                        // re-publishes Locked from its retained recovery state.
                        self.locked_hosts.remove(&addr);
                        self.latest_host_input_states.remove(&addr);
                        self.addr_to_fingerprint.insert(addr, fingerprint.clone());
                        // Pre-cache the per-handle post-processing so
                        // EmulationTask can pick it up the moment the
                        // first Input from this addr arrives.
                        let pp = self.post_processing_for_addr(addr);
                        self.emulation_proxy.set_post_processing(addr, pp);
                        self.event_tx.send(EmulationEvent::Connected {
                            addr,
                            session,
                            fingerprint,
                        }).expect("channel closed");
                    }
                    Some(ListenEvent::Disconnected { addr, session }) => {
                        if last_response
                            .get(&addr)
                            .is_some_and(|(current, _)| *current == session)
                        {
                            last_response.remove(&addr);
                        }
                        self.locked_hosts.remove(&addr);
                        self.latest_host_input_states.remove(&addr);
                        self.addr_to_fingerprint.remove(&addr);
                        completed_handovers.remove(&(addr, session));
                        legacy_enter_ready.remove(&(addr, session));
                        input_owner_sessions.remove(&(addr, session));
                        peer_capabilities.remove(&(addr, session));
                        pending_leaves.retain(|(candidate, generation, _), _| {
                            *candidate != addr || *generation != session
                        });
                        self.emulation_proxy.forget(addr);
                        self.event_tx.send(EmulationEvent::Disconnected {
                            addr,
                            session,
                        }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                            }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release { addr, session, handover } => {
                        // Leave(0) remains the legacy handover signal so a
                        // new sender stays safe with old receivers. Only a
                        // one-way EnterOnly edge opts into the new mode: no
                        // Enter+CursorPos will follow, so the peer must use
                        // its own modeled host warp when releasing.
                        let mode = if handover {
                            LEAVE_HANDOVER
                        } else {
                            LEAVE_RELEASE_ONLY
                        };
                        if self.session_is_current(addr, session) {
                            // The local pointer crossed the exact return barrier,
                            // so this session must stop injecting immediately.
                            // Waiting for the reciprocal Leave makes held-key
                            // cleanup depend on another lossy datagram and lets
                            // stale input race the new local capture.
                            let serial = input_owner_sessions.remove(&(addr, session));
                            legacy_enter_ready.remove(&(addr, session));
                            self.emulation_proxy.remove(addr);
                            let transactional = peer_capabilities
                                .get(&(addr, session))
                                .is_some_and(|capabilities| {
                                    capabilities & CAP_TRANSACTIONAL_HANDOVER != 0
                                });
                            if let Some(serial) = serial.filter(|_| transactional) {
                                pending_leaves.insert((addr, session, serial), mode);
                                self.listener
                                    .reply(
                                        addr,
                                        session,
                                        ProtoEvent::HandoverLeave { serial, mode },
                                    )
                                    .await;
                            } else if !transactional {
                                self.listener
                                    .reply(addr, session, ProtoEvent::Leave(mode))
                                    .await;
                            }
                        }
                    }
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::SetIncomingPeers(peers) => {
                        self.incoming_peers = peers;
                        // Re-resolve every known address so the live
                        // backend picks up changes for currently-
                        // active peers, not just future ones.
                        let known_addrs: Vec<SocketAddr> = self.addr_to_fingerprint.keys().copied().collect();
                        for addr in known_addrs {
                            let pp = self.post_processing_for_addr(addr);
                            self.emulation_proxy.set_post_processing(addr, pp);
                            // Push the updated sensitivity to the
                            // capturing peer over the wire so their
                            // wall-press auto-release model matches
                            // immediately, without waiting for the
                            // next cross-back-then-cross-forward.
                            if let Some(session) = self.listener.current_session(addr) {
                                self.listener.reply(addr, session, ProtoEvent::ReceiverSensitivity {
                                    mouse_sensitivity: pp.mouse_sensitivity,
                                }).await;
                            }
                        }
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = heartbeat_interval.tick() => {
                    let timed_out: Vec<_> = last_response
                        .iter()
                        .filter_map(|(&addr, &(session, instant))| {
                            (instant.elapsed() > Duration::from_secs(1))
                                .then_some((addr, session))
                        })
                        .collect();
                    for (addr, session) in timed_out {
                        // The map is keyed by address, so verify the exact
                        // generation before removing a timeout snapshot that
                        // could have been superseded by a newer session.
                        if !last_response.get(&addr).is_some_and(|(current, instant)| {
                            *current == session
                                && instant.elapsed() > Duration::from_secs(1)
                        }) {
                            continue;
                        }
                        last_response.remove(&addr);
                        if !self.session_is_current(addr, session) {
                            continue;
                        }

                        // Preserve and report the exact active transaction
                        // before clearing it. This lets an entirely idle
                        // sender leave Sending immediately instead of waiting
                        // for another input event to discover ownership loss.
                        // Legacy peers receive no new control frame.
                        if let Some(event) = heartbeat_ownership_loss(
                            &peer_capabilities,
                            &input_owner_sessions,
                            addr,
                            session,
                        ) {
                            self.listener.reply(addr, session, event).await;
                            if !self.session_is_current(addr, session) {
                                continue;
                            }
                        }

                        log::warn!(
                            "releasing keys: {addr} session {session} not responding!"
                        );
                        self.locked_hosts.remove(&addr);
                        self.latest_host_input_states.remove(&addr);
                        self.emulation_proxy.remove(addr);
                        // A retry on this still-authenticated session must
                        // re-run release/warp and recreate the return barrier
                        // after the pseudo-disconnect. Re-Acking the prior
                        // completed serial would resume input without those
                        // side effects.
                        completed_handovers.remove(&(addr, session));
                        legacy_enter_ready.remove(&(addr, session));
                        input_owner_sessions.remove(&(addr, session));
                        self.event_tx
                            .send(EmulationEvent::Disconnected { addr, session })
                            .expect("channel closed");
                    }
                }
                _ = leave_retry_interval.tick() => {
                    let retries: Vec<_> = pending_leaves
                        .iter()
                        .map(|(&(addr, session, serial), &mode)| {
                            (addr, session, serial, mode)
                        })
                        .collect();
                    for (addr, session, serial, mode) in retries {
                        if self.session_is_current(addr, session) {
                            self.listener
                                .reply(
                                    addr,
                                    session,
                                    ProtoEvent::HandoverLeave { serial, mode },
                                )
                                .await;
                        } else {
                            pending_leaves.remove(&(addr, session, serial));
                        }
                    }
                }
                _ = topology_interval.tick() => {
                    // Republish the complete current topology to every
                    // actively responding peer, even when it is unchanged.
                    // These are UDP datagrams with no topology Ack; advancing
                    // a global "last sent" cache after one fire-and-forget
                    // send made a single lost Layout permanent until another
                    // hotplug or Enter. The two-second refresh is cheap and
                    // makes both packet loss and listener replacement
                    // self-healing. Old peers ignore the extension event.
                    let bounds = self.emulation_proxy.display_bounds();
                    let layout = self.emulation_proxy.display_layout();
                    refresh_topology_generation(
                        &mut last_topology,
                        &mut topology_generation,
                        &layout,
                    );
                    let addrs: Vec<(SocketAddr, ListenerSession)> = last_response
                        .iter()
                        .map(|(&addr, &(session, _))| (addr, session))
                        .collect();
                    for &(addr, session) in &addrs {
                        let pp = self.post_processing_for_addr(addr);
                        self.listener.reply(
                            addr,
                            session,
                            ProtoEvent::ReceiverSensitivity {
                                mouse_sensitivity: pp.mouse_sensitivity,
                            },
                        ).await;
                    }
                    if let Some((width, height)) = bounds {
                        for &(addr, session) in &addrs {
                            self.listener.reply(addr, session, ProtoEvent::Bounds { width, height }).await;
                        }
                    }
                    if let Some(layout) = layout {
                        for (addr, session) in addrs {
                            self.listener.reply(
                                addr,
                                session,
                                ProtoEvent::display_layout_generation(
                                    layout.clone(),
                                    topology_epoch,
                                    topology_generation,
                                ),
                            ).await;
                        }
                    }
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
    /// Cached display bounds. Refreshed each time the underlying
    /// InputEmulation is (re)created. `None` until the first
    /// successful query, or if the active backend doesn't report
    /// geometry.
    display_bounds: Rc<Cell<Option<(u32, u32)>>>,
    /// Cached full monitor topology paired with `display_bounds` from the same
    /// backend query. Refreshed every two seconds while emulation is active.
    display_layout: Rc<RefCell<Option<DisplayLayout>>>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    Remove(SocketAddr),
    /// Terminal DTLS teardown. Unlike `Remove`, also discard per-address
    /// settings because a reconnect normally arrives from a new ephemeral
    /// socket and must not leave one cache entry behind per old session.
    Forget(SocketAddr),
    Terminate,
    Reenable,
    /// Warp the local cursor to an absolute position. Used on
    /// `Enter` to seat the cursor at the entry edge so the
    /// capturing peer's wall-press model is synchronized.
    WarpToEdge(DisplayEdge, f64, oneshot::Sender<Option<EdgeWarpOutcome>>),
    /// Query and cache a fresh topology snapshot on the emulation task. Entry
    /// metadata uses this instead of the periodic cache so a hotplug cannot
    /// split cursor placement and advertised geometry across generations.
    RefreshDisplayLayout(oneshot::Sender<Option<DisplayLayout>>),
    /// Set the receive-side post-processing for events arriving
    /// from `addr`. Resolved by ListenTask from the persistent
    /// authorized-peers table; cached on the EmulationTask side
    /// keyed by addr until a handle exists, then pushed into
    /// InputEmulation by handle.
    SetPostProcessing(SocketAddr, ReceivePostProcessing),
}

impl EmulationProxy {
    fn new(backend: Option<input_emulation::Backend>) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let display_bounds = Rc::new(Cell::new(None));
        let display_layout = Rc::new(RefCell::new(None));
        let emulation_task = EmulationTask {
            backend,
            exit_requested: exit_requested.clone(),
            display_bounds: display_bounds.clone(),
            display_layout: display_layout.clone(),
            post_processing: HashMap::new(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
            display_bounds,
            display_layout,
        }
    }

    /// Display geometry of this device (cached). Refreshed each
    /// time the input emulation backend is (re)created.
    pub(crate) fn display_bounds(&self) -> Option<(u32, u32)> {
        self.display_bounds.get()
    }

    /// Full display topology paired with [`Self::display_bounds`].
    pub(crate) fn display_layout(&self) -> Option<DisplayLayout> {
        self.display_layout.borrow().clone()
    }

    /// Complete only after the active backend has applied the warp. The wire
    /// Ack for an atomic handover is held behind this completion so the sender
    /// cannot flush input while the cursor still belongs to the old screen.
    pub(crate) async fn warp_cursor_to_edge(
        &self,
        edge: DisplayEdge,
        cross_fraction: f64,
    ) -> Option<EdgeWarpOutcome> {
        if !self.emulation_active.get() {
            return None;
        }
        let (completion, completed) = oneshot::channel();
        if self
            .request_tx
            .send(ProxyRequest::WarpToEdge(edge, cross_fraction, completion))
            .is_err()
        {
            return None;
        }
        completed.await.unwrap_or(None)
    }

    /// Fetch geometry from the live backend rather than relying on the
    /// two-second cache used for background topology announcements.
    pub(crate) async fn refresh_display_layout(&self) -> Option<DisplayLayout> {
        if !self.emulation_active.get() {
            return None;
        }
        let (completion, completed) = oneshot::channel();
        if self
            .request_tx
            .send(ProxyRequest::RefreshDisplayLayout(completion))
            .is_err()
        {
            return None;
        }
        completed.await.unwrap_or(None)
    }

    /// Fire-and-forget per-addr post-processing update. Persists in
    /// the EmulationTask cache so settings survive backend respawns
    /// (CGEventTap timeout, portal session restart, etc.) and so a
    /// handle created later for this addr inherits the right values.
    pub(crate) fn set_post_processing(
        &self,
        addr: SocketAddr,
        post_processing: ReceivePostProcessing,
    ) {
        let _ = self
            .request_tx
            .send(ProxyRequest::SetPostProcessing(addr, post_processing));
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    fn forget(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Forget(addr))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    exit_requested: Rc<Cell<bool>>,
    /// Shared cache; refreshed each time we (re)create the inner
    /// InputEmulation. Read by `EmulationProxy::display_bounds`.
    display_bounds: Rc<Cell<Option<(u32, u32)>>>,
    /// Full topology cache from the same refresh as `display_bounds`.
    display_layout: Rc<RefCell<Option<DisplayLayout>>>,
    /// Per-addr receive-side post-processing snapshots. Pushed by
    /// ListenTask via `ProxyRequest::SetPostProcessing` whenever
    /// the underlying authorized-peers table changes. Re-applied to
    /// every newly created InputEmulation (handle by handle) so a
    /// backend respawn doesn't drop the user's settings.
    post_processing: HashMap<SocketAddr, ReceivePostProcessing>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
}

impl EmulationTask {
    fn cache_display_layout(&self, layout: Option<DisplayLayout>) {
        self.display_bounds
            .set(layout.as_ref().and_then(DisplayLayout::size));
        self.display_layout.replace(layout);
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::Forget(addr) => {
                        forget_peer_state(&mut self.handles, &mut self.post_processing, addr);
                    }
                    ProxyRequest::WarpToEdge(_, _, completion) => {
                        let _ = completion.send(None);
                    }
                    ProxyRequest::RefreshDisplayLayout(completion) => {
                        let _ = completion.send(None);
                    }
                    ProxyRequest::SetPostProcessing(addr, pp) => {
                        // No live backend yet, but cache the values so
                        // the next created backend picks them up the
                        // moment a handle is assigned for this addr.
                        self.post_processing.insert(addr, pp);
                    }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend) => r?,
            // Keep cache/handle teardown effective while the backend is being
            // requested; this branch returns only for Terminate.
            _ = wait_for_termination(
                &mut self.request_rx,
                &mut self.handles,
                &mut self.post_processing,
            ) => return Ok(()),
        };

        // Refresh the paired topology/bounds caches. Bounds are derived from
        // this exact layout snapshot so legacy and topology-aware peers never
        // initialize from two different monitor arrangements.
        let layout = emulation.display_layout();
        self.cache_display_layout(layout);

        // Re-apply per-handle post-processing for any handles we
        // already had before the backend was (re)created. New
        // handles created from `Input` will pick up their values
        // from the same cache.
        for (addr, &handle) in &self.handles {
            if let Some(&pp) = self.post_processing.get(addr) {
                emulation.set_post_processing(handle, pp);
            }
        }

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled,
            EmulationEvent::EmulationDisabled,
        );

        // create active handles
        match self.create_clients(&mut emulation).await {
            Ok(true) => {
                emulation.terminate().await;
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => {
                emulation.terminate().await;
                return Err(e);
            }
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<bool, InputEmulationError> {
        // Snapshot so a terminal Forget received while create() is pending can
        // remove the mapping without borrowing the map through the select.
        let clients: Vec<(SocketAddr, EmulationHandle)> = self
            .handles
            .iter()
            .map(|(&addr, &handle)| (addr, handle))
            .collect();
        for (addr, handle) in clients {
            if self.handles.get(&addr) != Some(&handle) {
                continue;
            }
            tokio::select! {
                _ = emulation.create(handle) => {
                    // Forget/Remove may have completed concurrently with
                    // create(). Do not leave an untracked backend handle.
                    if self.handles.get(&addr) != Some(&handle) {
                        emulation.destroy(handle).await;
                    } else if let Some(&pp) = self.post_processing.get(&addr) {
                        emulation.set_post_processing(handle, pp);
                    }
                },
                _ = wait_for_termination(
                    &mut self.request_rx,
                    &mut self.handles,
                    &mut self.post_processing,
                ) => return Ok(true),
            }
        }
        Ok(false)
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        // Re-query display geometry periodically so a monitor
        // hot-plug/unplug, a resolution change, or a MacBook
        // lid-open-after-clamshell is picked up without restarting
        // the backend. The shared cache feeds both the
        // ProtoEvent::Bounds reply on Enter and the CursorPos warp
        // clamping — left stale, it pins the cursor to the old
        // display size until mousehop is quit.
        let mut bounds_poll = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = bounds_poll.tick() => {
                    let current_layout = emulation.display_layout();
                    let current_bounds = current_layout.as_ref().and_then(DisplayLayout::size);
                    if current_bounds != self.display_bounds.get()
                        || self.display_layout.borrow().as_ref() != current_layout.as_ref()
                    {
                        log::info!(
                            "display geometry changed: bounds {:?} -> {current_bounds:?}, rects {} -> {}",
                            self.display_bounds.get(),
                            self.display_layout.borrow().as_ref().map_or(0, DisplayLayout::len),
                            current_layout.as_ref().map_or(0, DisplayLayout::len),
                        );
                        self.cache_display_layout(current_layout);
                    }
                }
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                // Apply any cached post-processing
                                // (set when the DTLS Accept arrived,
                                // before the first Input).
                                if let Some(&pp) = self.post_processing.get(&addr) {
                                    emulation.set_post_processing(handle, pp);
                                }
                                handle
                            }
                        };
                        emulation.consume(event, handle).await?;
                    },
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                        // Intentionally keep `post_processing[addr]`
                        // alive across handle removal. `Remove` fires
                        // on every `ProtoEvent::Leave` (cross-back to
                        // the peer's screen) and on the 1-second
                        // heartbeat timeout, neither of which means
                        // the DTLS session is gone for good. The same
                        // SocketAddr keeps delivering Input events on
                        // the next cross; we want the user's per-pair
                        // settings to follow the addr, not the
                        // ephemeral handle that gets minted fresh on
                        // each cross. A real DTLS disconnect followed
                        // by a reconnect arrives with a new
                        // SocketAddr (new ephemeral port), so a stale
                        // entry doesn't shadow a fresh one.
                    }
                    ProxyRequest::Forget(addr) => {
                        if let Some(handle) = forget_peer_state(
                            &mut self.handles,
                            &mut self.post_processing,
                            addr,
                        ) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::WarpToEdge(edge, cross_fraction, completion) => {
                        let result = emulation.warp_cursor_to_edge(edge, cross_fraction).await;
                        let outcome = match result {
                            Ok(EdgeWarpOutcome::Applied(layout)) => {
                                // Preserve the exact snapshot used to calculate
                                // the warp for the metadata sent before Ack.
                                self.cache_display_layout(Some(layout.clone()));
                                Some(EdgeWarpOutcome::Applied(layout))
                            }
                            Ok(EdgeWarpOutcome::Unsupported) => {
                                Some(EdgeWarpOutcome::Unsupported)
                            }
                            Err(e) => {
                                log::warn!("edge cursor warp failed: {e}");
                                None
                            }
                        };
                        let _ = completion.send(outcome);
                    }
                    ProxyRequest::RefreshDisplayLayout(completion) => {
                        let layout = emulation.display_layout();
                        self.cache_display_layout(layout.clone());
                        let _ = completion.send(layout);
                    }
                    ProxyRequest::SetPostProcessing(addr, pp) => {
                        self.post_processing.insert(addr, pp);
                        if let Some(&handle) = self.handles.get(&addr) {
                            emulation.set_post_processing(handle, pp);
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
            }
        }
    }
}

fn to_ipc_pos(pos: Position) -> mousehop_ipc::Position {
    match pos {
        Position::Left => mousehop_ipc::Position::Left,
        Position::Right => mousehop_ipc::Position::Right,
        Position::Top => mousehop_ipc::Position::Top,
        Position::Bottom => mousehop_ipc::Position::Bottom,
    }
}

/// Where to seat the local cursor when this device is entered.
/// `pos` is the protocol-level position in *this device's* frame
/// (already inverted from the host's perspective by the capture
/// side). For example, `Position::Left` means "the host is to my
/// left, the cursor entered from my left edge", so the cursor
/// should land at x=0. Y is centered along the entry edge for
/// Left/Right; X is centered for Top/Bottom.
async fn wait_for_termination(
    rx: &mut Receiver<ProxyRequest>,
    handles: &mut HashMap<SocketAddr, EmulationHandle>,
    post_processing: &mut HashMap<SocketAddr, ReceivePostProcessing>,
) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::Remove(addr) => {
                handles.remove(&addr);
            }
            ProxyRequest::Forget(addr) => {
                forget_peer_state(handles, post_processing, addr);
            }
            ProxyRequest::WarpToEdge(_, _, completion) => {
                let _ = completion.send(None);
            }
            ProxyRequest::RefreshDisplayLayout(completion) => {
                let _ = completion.send(None);
            }
            ProxyRequest::SetPostProcessing(addr, pp) => {
                post_processing.insert(addr, pp);
            }
            ProxyRequest::Reenable => continue,
        }
    }
}

fn forget_peer_state(
    handles: &mut HashMap<SocketAddr, EmulationHandle>,
    post_processing: &mut HashMap<SocketAddr, ReceivePostProcessing>,
    addr: SocketAddr,
) -> Option<EmulationHandle> {
    post_processing.remove(&addr);
    handles.remove(&addr)
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
mod lock_state_tests {
    use super::*;

    #[test]
    fn confirmed_lock_gates_input_until_confirmed_unlock() {
        let addr: SocketAddr = "192.0.2.8:4252".parse().expect("address");
        let mut locked_hosts = HashSet::new();
        assert!(remote_input_allowed(&locked_hosts, addr));

        assert!(update_locked_host(
            &mut locked_hosts,
            addr,
            HostInputState::Locked
        ));
        assert!(!remote_input_allowed(&locked_hosts, addr));
        assert!(
            !update_locked_host(&mut locked_hosts, addr, HostInputState::Locked),
            "a retry must not repeat teardown"
        );

        assert!(!update_locked_host(
            &mut locked_hosts,
            addr,
            HostInputState::Unlocked
        ));
        assert!(remote_input_allowed(&locked_hosts, addr));
    }

    #[test]
    fn input_requires_current_handover_ownership_and_leave_revokes_it() {
        let addr: SocketAddr = "192.0.2.8:4252".parse().expect("address");
        let session = 9;
        let locked_hosts = HashSet::new();
        let mut owners = HashMap::new();

        assert!(!remote_session_owns_input(
            &locked_hosts,
            &owners,
            addr,
            session
        ));
        owners.insert((addr, session), 41);
        assert!(remote_session_owns_input(
            &locked_hosts,
            &owners,
            addr,
            session
        ));
        assert_eq!(
            classify_transactional_input(&locked_hosts, &owners, addr, session, 41),
            TransactionalInputDisposition::Consume
        );
        assert_eq!(
            classify_transactional_input(&locked_hosts, &owners, addr, session, 40),
            TransactionalInputDisposition::ReportOwnershipLost,
            "late input from the previous crossing must not enter the newer owner"
        );
        owners.remove(&(addr, session));
        assert!(
            !remote_session_owns_input(&locked_hosts, &owners, addr, session),
            "a delayed Input after Leave must not recreate an emulation handle"
        );
        assert_eq!(
            classify_transactional_input(&locked_hosts, &owners, addr, session, 41),
            TransactionalInputDisposition::ReportOwnershipLost,
            "heartbeat teardown must tell the still-sending owner to release"
        );
    }

    #[test]
    fn heartbeat_proactively_reports_only_the_exact_transactional_owner() {
        let addr: SocketAddr = "192.0.2.8:4252".parse().expect("address");
        let other_addr: SocketAddr = "192.0.2.9:4252".parse().expect("address");
        let session = 9;
        let mut capabilities = HashMap::from([
            ((addr, session), CAP_TRANSACTIONAL_HANDOVER),
            ((other_addr, session), 0),
        ]);
        let owners = HashMap::from([((addr, session), 41), ((other_addr, session), 42)]);

        assert!(matches!(
            heartbeat_ownership_loss(&capabilities, &owners, addr, session),
            Some(ProtoEvent::OwnershipLost { serial: 41 })
        ));
        assert!(
            heartbeat_ownership_loss(&capabilities, &owners, addr, session + 1).is_none(),
            "a replacement listener session must not inherit the old serial"
        );
        assert!(
            heartbeat_ownership_loss(&capabilities, &owners, other_addr, session).is_none(),
            "legacy peers must not receive transactional control frames"
        );

        capabilities.remove(&(addr, session));
        assert!(heartbeat_ownership_loss(&capabilities, &owners, addr, session).is_none());
    }

    #[test]
    fn serialled_leave_never_revokes_a_newer_handover() {
        assert!(handover_leave_revokes(Some(41), Some(41), 41));
        assert!(
            handover_leave_revokes(Some(41), Some(41), 42),
            "a leave reordered ahead of its newer Enter closes the old owner"
        );
        assert!(
            !handover_leave_revokes(Some(42), Some(42), 41),
            "a delayed old Leave must preserve the newer owner"
        );
        assert!(
            !handover_leave_revokes(Some(41), None, 41),
            "a duplicate Leave is idempotent once ownership was removed"
        );
    }

    #[test]
    fn lost_ack_reuses_the_exact_applied_topology_snapshot() {
        let applied = DisplayLayout::new([(0, 0, 1920, 1080)]);
        let hotplugged = DisplayLayout::new([(-1024, 0, 1024, 768), (0, 0, 1920, 1080)]);
        let completed = CompletedHandover {
            serial: 17,
            warp: HandoverWarpStatus::Applied,
            layout: Some(applied.clone()),
            topology_generation: 3,
        };

        assert_ne!(completed.layout.as_ref(), Some(&hotplugged));
        assert_eq!(completed.layout, Some(applied));
        assert_eq!(completed.topology_generation, 3);
    }

    #[test]
    fn terminal_forget_removes_only_the_disconnected_peers_cached_state() {
        let disconnected: SocketAddr = "192.0.2.8:4252".parse().expect("address");
        let current: SocketAddr = "192.0.2.9:4252".parse().expect("address");
        let mut handles = HashMap::from([(disconnected, 3), (current, 4)]);
        let mut post_processing = HashMap::from([
            (
                disconnected,
                ReceivePostProcessing {
                    natural_scroll: true,
                    mouse_sensitivity: 1.25,
                },
            ),
            (current, ReceivePostProcessing::default()),
        ]);

        assert_eq!(
            forget_peer_state(&mut handles, &mut post_processing, disconnected),
            Some(3)
        );
        assert!(!handles.contains_key(&disconnected));
        assert!(!post_processing.contains_key(&disconnected));
        assert_eq!(handles.get(&current), Some(&4));
        assert!(post_processing.contains_key(&current));
    }

    #[test]
    fn stale_lock_cannot_override_newer_unlock() {
        let addr: SocketAddr = "192.0.2.8:4252".parse().expect("address");
        let mut latest_states = HashMap::new();

        assert!(accept_host_input_state(
            &mut latest_states,
            addr,
            HostInputState::Locked,
            7,
        ));
        assert!(accept_host_input_state(
            &mut latest_states,
            addr,
            HostInputState::Unlocked,
            8,
        ));
        assert!(
            !accept_host_input_state(&mut latest_states, addr, HostInputState::Locked, 7,),
            "a delayed Locked retry must not override generation 8"
        );
        assert!(accept_host_input_state(
            &mut latest_states,
            addr,
            HostInputState::Unlocked,
            8,
        ));
        assert!(
            !accept_host_input_state(&mut latest_states, addr, HostInputState::Locked, 8,),
            "one generation cannot carry contradictory states"
        );
        assert_eq!(
            latest_states.get(&addr),
            Some(&(8, HostInputState::Unlocked))
        );
    }

    #[test]
    fn topology_generation_changes_once_per_complete_layout() {
        let first = DisplayLayout::new([(0, 0, 1920, 1080)]);
        let second = DisplayLayout::new([(-1024, 0, 1024, 600), (0, 0, 1920, 1080)]);
        let mut last = None;
        let mut generation = 0;

        refresh_topology_generation(&mut last, &mut generation, &Some(first.clone()));
        assert_eq!(generation, 1);
        refresh_topology_generation(&mut last, &mut generation, &Some(first));
        assert_eq!(generation, 1, "periodic resend retains its generation");
        refresh_topology_generation(&mut last, &mut generation, &None);
        assert_eq!(generation, 1, "transient unavailable query is ignored");
        refresh_topology_generation(&mut last, &mut generation, &Some(second.clone()));
        assert_eq!(generation, 2);
        assert_eq!(last, Some(second));
    }

    #[test]
    fn atomic_handover_retries_reack_without_reapplying_side_effects() {
        assert_eq!(classify_handover(None, 41), HandoverDisposition::Apply);
        assert_eq!(classify_handover(Some(41), 41), HandoverDisposition::Reack);
        assert_eq!(
            classify_handover(Some(41), 40),
            HandoverDisposition::DropStale
        );
        assert_eq!(classify_handover(Some(41), 42), HandoverDisposition::Apply);

        let addr: SocketAddr = "192.0.2.9:4242".parse().expect("address");
        let session = 7;
        let mut completed = HashMap::from([(
            (addr, session),
            CompletedHandover {
                serial: 41,
                warp: HandoverWarpStatus::Applied,
                layout: Some(DisplayLayout::new([(0, 0, 1920, 1080)])),
                topology_generation: 7,
            },
        )]);
        completed.remove(&(addr, session));
        assert_eq!(
            classify_handover(
                completed
                    .get(&(addr, session))
                    .map(|completed| completed.serial),
                41,
            ),
            HandoverDisposition::Apply,
            "heartbeat teardown must make a same-serial retry rebuild ownership"
        );
    }

    #[test]
    fn atomic_handover_serial_order_wraps_across_zero() {
        assert_eq!(
            classify_handover(Some(u32::MAX), 1),
            HandoverDisposition::Apply
        );
        assert_eq!(
            classify_handover(Some(1), u32::MAX),
            HandoverDisposition::DropStale
        );
    }
}
