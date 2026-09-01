use crate::client::ClientManager;
use crate::config::local_commit;
use crate::discovery::{PrimaryCache, normalize_mdns_name};
use input_event::{Event as InputEvent, KeyboardEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use mousehop_ipc::{ClientHandle, ConnectionMode, DEFAULT_PORT};
use mousehop_proto::{
    MAX_CLIPBOARD_SIZE, MAX_EVENT_SIZE, PROTOCOL_MAGIC, ProtoEvent, decode_clipboard_event,
    decode_display_layout_event, decode_fixed_event, encode_clipboard_event,
    encode_display_layout_event,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    io,
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

type ArcConn = Arc<dyn Conn + Send + Sync>;
pub(crate) type ConnectionSession = u64;

#[derive(Clone)]
struct ConnectionSlot {
    handle: ClientHandle,
    session: ConnectionSession,
    conn: ArcConn,
}

#[derive(Clone, Default)]
struct SessionTracker {
    next: Rc<Cell<ConnectionSession>>,
    current: Rc<RefCell<HashMap<ClientHandle, ConnectionSession>>>,
}

impl SessionTracker {
    fn allocate(&self, handle: ClientHandle) -> ConnectionSession {
        let mut session = self.next.get().wrapping_add(1);
        if session == 0 {
            session = 1;
        }
        self.next.set(session);
        self.current.borrow_mut().insert(handle, session);
        session
    }

    fn is_current(&self, handle: ClientHandle, session: ConnectionSession) -> bool {
        self.current.borrow().get(&handle).copied() == Some(session)
    }

    fn current(&self, handle: ClientHandle) -> Option<ConnectionSession> {
        self.current.borrow().get(&handle).copied()
    }

    fn remove_if_current(&self, handle: ClientHandle, session: ConnectionSession) -> bool {
        if !self.is_current(handle, session) {
            return false;
        }
        self.current.borrow_mut().remove(&handle);
        true
    }
}

#[derive(Debug, Error)]
pub(crate) enum MousehopConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

/// Events delivered from the outbound connection tasks to the capture loop.
/// A disconnect is deliberately distinct from a wire `Leave`: the peer is no
/// longer reachable, so capture must be released locally without trying to
/// send cleanup events over (and potentially reconnect) the dead session.
#[derive(Debug)]
pub(crate) enum MousehopConnectionEvent {
    Received {
        handle: ClientHandle,
        addr: SocketAddr,
        session: ConnectionSession,
        event: ProtoEvent,
    },
    Disconnected {
        handle: ClientHandle,
        session: ConnectionSession,
    },
}

/// Before a DTLS peer proves it speaks Mousehop, only its protocol `Hello`
/// may affect connection or input state. The caller still validates the
/// hello's magic and rejects a foreign value.
pub(crate) fn handshake_allows_event(hello_ok: bool, event: &ProtoEvent) -> bool {
    hello_ok || matches!(event, ProtoEvent::Hello { .. })
}

/// Cleanup already committed to an exact transport generation must remain
/// deliverable after Service removes or reconfigures the corresponding
/// `ClientManager` entry. Ordinary input is deliberately excluded: a stale
/// capture event must not gain permission merely because it still knows an old
/// session number.
fn is_session_cleanup_event(event: &ProtoEvent) -> bool {
    matches!(
        event,
        ProtoEvent::Leave(_)
            | ProtoEvent::HandoverLeave { .. }
            | ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key { state: 0, .. }))
            | ProtoEvent::HandoverInput {
                event: InputEvent::Keyboard(KeyboardEvent::Key { state: 0, .. }),
                ..
            }
            | ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }))
            | ProtoEvent::HandoverInput {
                event: InputEvent::Keyboard(KeyboardEvent::Modifiers {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0,
                }),
                ..
            }
    )
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial backoff between connect attempts that find no usable address
/// (no static IPs, no DNS-resolved IPs, no mDNS primary hint). Doubles
/// on each subsequent failure up to [`MAX_RETRY_BACKOFF`]. The backoff
/// is bypassed entirely when the input set changes (e.g. mDNS browse
/// resolves a primary, DNS lookup returns IPs) so a peer that comes
/// back online reconnects on the next mouse event without waiting.
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// Per-handle gate that throttles repeat connect attempts when nothing
/// new is available to dial. `signature` hashes the candidate set we
/// last attempted; if the current set differs we skip the gate and
/// retry immediately. Otherwise `next_attempt_at` enforces exponential
/// backoff capped at [`MAX_RETRY_BACKOFF`].
struct RetryState {
    next_attempt_at: Instant,
    backoff: Duration,
    signature: u64,
}

fn signature_of(ips: &HashSet<IpAddr>, primary: Option<IpAddr>) -> u64 {
    let mut sorted: Vec<IpAddr> = ips.iter().copied().collect();
    sorted.sort();
    let mut hasher = DefaultHasher::new();
    sorted.hash(&mut hasher);
    primary.hash(&mut hasher);
    hasher.finish()
}

/// Update `retry_state[handle]` after a failed connect attempt: doubles
/// the backoff (capped at [`MAX_RETRY_BACKOFF`]) and stamps the
/// candidate-set signature so a later signature change can short-
/// circuit the gate.
fn record_retry_failure(
    retry_state: &Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
    handle: ClientHandle,
    ips: &HashSet<IpAddr>,
    primary: Option<IpAddr>,
) {
    let sig = signature_of(ips, primary);
    let mut map = retry_state.borrow_mut();
    let entry = map.entry(handle).or_insert(RetryState {
        next_attempt_at: Instant::now(),
        backoff: INITIAL_RETRY_BACKOFF,
        signature: sig,
    });
    entry.signature = sig;
    let next = entry.backoff;
    entry.next_attempt_at = Instant::now() + next;
    entry.backoff = (next * 2).min(MAX_RETRY_BACKOFF);
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, MousehopConnectionError)> {
    log::info!("connecting to {addr} ...");
    // Bind family must match the target's: a 0.0.0.0 socket fails
    // `connect()` to a v6 peer with EAFNOSUPPORT, and vice versa.
    // On a v4-only kernel the `[::]:0` bind itself errors out and
    // the caller treats it as a normal per-address connect failure.
    let bind_addr: &str = match addr {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let conn = Arc::new(
        UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, MousehopConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

/// Time the preferred address gets to handshake alone before the
/// rest of the candidate list joins the race. Modeled on RFC 8305
/// "happy eyeballs" v6→v4 fallback delay; long enough that a healthy
/// preferred address virtually always wins, short enough that a
/// broken preferred path only slightly delays connect.
const PREFERRED_ADDR_HEAD_START: Duration = Duration::from_millis(200);

async fn connect_any(
    addrs: &[SocketAddr],
    preferred: Option<SocketAddr>,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), MousehopConnectionError> {
    let mut joinset = JoinSet::new();
    if let Some(p) = preferred {
        // Dial the peer's mDNS-advertised primary first. If it
        // handshakes within `PREFERRED_ADDR_HEAD_START` we're done
        // before the others even start — the dialer biases toward
        // the OS-preferred interface (Mac service order, Linux
        // default route) without relying on RTT racing alone.
        joinset.spawn_local(connect(p, cert.clone()));
        let head_start = tokio::time::sleep(PREFERRED_ADDR_HEAD_START);
        tokio::pin!(head_start);
        loop {
            tokio::select! {
                _ = &mut head_start => break,
                Some(r) = joinset.join_next() => match r.expect("join error") {
                    Ok(conn) => return Ok(conn),
                    Err((a, e)) => log::warn!("failed to connect to {a}: `{e}`"),
                },
            }
        }
    }
    for &addr in addrs {
        if Some(addr) == preferred {
            // already racing; don't dial the same socket twice
            continue;
        }
        joinset.spawn_local(connect(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(MousehopConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

pub(crate) struct MousehopConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    /// In-flight dial token per handle. The token is also the prospective DTLS
    /// session generation, so reset/delete can invalidate a dial before Slab
    /// reuses the numeric handle for another peer.
    connecting: Rc<Mutex<HashMap<ClientHandle, ConnectionSession>>>,
    recv_rx: Receiver<MousehopConnectionEvent>,
    recv_tx: Sender<MousehopConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    /// Send timestamp of the most-recent keepalive ping per active
    /// address. `receive_loop` subtracts it on `Pong` to get the live
    /// round-trip latency of the *active* connection — measured over
    /// the real DTLS/UDP path, so it's accurate and works even where a
    /// host firewall drops the TCP probe (the active address is then
    /// excluded from TCP probing; see [`ClientManager::probe_targets`]).
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
    /// Map of `peer_hostname -> primary_ipv4` populated by the
    /// `Discovery` mDNS browse task. Read on every `connect_to_handle`
    /// to bias which address gets the handshake head-start. Empty
    /// when discovery is disabled or no peer hint has arrived yet.
    primary_hints: PrimaryCache,
    /// Per-handle retry gate. Suppresses connect spawns when the
    /// previous attempt failed and nothing new is available to dial,
    /// so an offline peer doesn't trigger a fresh `connect_to_handle`
    /// (and the associated DNS / mDNS lookup churn) on every mouse
    /// event. Cleared on successful connect; bypassed automatically
    /// when the candidate-set signature changes.
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
}

impl MousehopConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        primary_hints: PrimaryCache,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            sessions: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            ping_sent_at: Default::default(),
            primary_hints,
            retry_state: Default::default(),
        }
    }

    pub(crate) async fn recv(&mut self) -> MousehopConnectionEvent {
        loop {
            let event = self.recv_rx.recv().await.expect("channel closed");
            let is_current =
                match &event {
                    MousehopConnectionEvent::Received {
                        handle,
                        addr,
                        session,
                        ..
                    } => {
                        self.sessions.is_current(*handle, *session)
                            && self.conns.lock().await.get(addr).is_some_and(|slot| {
                                slot.handle == *handle && slot.session == *session
                            })
                    }
                    MousehopConnectionEvent::Disconnected {
                        handle, session, ..
                    } => self
                        .sessions
                        .current(*handle)
                        .is_none_or(|current| current == *session),
                };
            if is_current {
                return event;
            }
            log::debug!("dropping queued event from a replaced outbound DTLS session");
        }
    }

    /// Return the authenticated transport generation currently selected for
    /// `handle`. Capture pins a handover and every Ack-gated input event to
    /// this value so a reconnect cannot inherit an in-flight crossing.
    pub(crate) fn current_session(&self, handle: ClientHandle) -> Option<ConnectionSession> {
        self.client_manager.active_addr(handle)?;
        self.sessions.current(handle)
    }

    /// Cheap send-only handle that shares all the dialer state with
    /// `self`. The clone's `recv_rx` is a dead stub — only the
    /// original [`MousehopConnection`] (held by Capture) drains the
    /// live receiver. Used by Service to fan clipboard frames out
    /// without routing through the capture session loop.
    pub(crate) fn sender_clone(&self) -> Self {
        let (_, dead_rx) = channel();
        Self {
            cert: self.cert.clone(),
            client_manager: self.client_manager.clone(),
            conns: self.conns.clone(),
            sessions: self.sessions.clone(),
            connecting: self.connecting.clone(),
            recv_rx: dead_rx,
            recv_tx: self.recv_tx.clone(),
            ping_response: self.ping_response.clone(),
            ping_sent_at: self.ping_sent_at.clone(),
            primary_hints: self.primary_hints.clone(),
            retry_state: self.retry_state.clone(),
        }
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), MousehopConnectionError> {
        let event_display = format!("{event}");
        // Lock-recovery status is control-plane information, not input
        // emulation. It must still reach a peer whose latest Pong reported
        // emulation unavailable (including the reconnect window before the
        // first Pong), otherwise the recovery dialog can never explain why
        // forwarding stopped.
        let send_when_emulation_inactive = matches!(&event, ProtoEvent::HostInputState { .. });
        // Clipboard and display-topology frames are variable-length and can't
        // ride the fixed-size codec; route them through dedicated helpers. For
        // all other events the existing 21-byte path is faster.
        let bytes_owned: Option<Vec<u8>> = match &event {
            ProtoEvent::Clipboard { .. } => match encode_clipboard_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping oversize clipboard event for client {handle}: {e}");
                    return Ok(());
                }
            },
            ProtoEvent::DisplayLayout { .. } => match encode_display_layout_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping invalid display layout for client {handle}: {e}");
                    return Ok(());
                }
            },
            _ => None,
        };
        let bytes_fixed: ([u8; MAX_EVENT_SIZE], usize) = if bytes_owned.is_some() {
            ([0u8; MAX_EVENT_SIZE], 0)
        } else {
            event.into()
        };
        let buf: &[u8] = if let Some(v) = bytes_owned.as_deref() {
            v
        } else {
            &bytes_fixed.0[..bytes_fixed.1]
        };
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let slot = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(slot) = slot {
                if slot.handle != handle || !self.sessions.is_current(handle, slot.session) {
                    return Err(MousehopConnectionError::NotConnected);
                }
                if !self.client_manager.alive(handle) && !send_when_emulation_inactive {
                    return Err(MousehopConnectionError::TargetEmulationDisabled);
                }
                match slot.conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        let removed = disconnect(
                            &self.client_manager,
                            handle,
                            addr,
                            slot.session,
                            &self.conns,
                            &self.sessions,
                            &self.ping_response,
                            &self.ping_sent_at,
                            &self.recv_tx,
                        )
                        .await;
                        // Wake the receive/keepalive tasks even if this slot
                        // was replaced between lookup and the failed send.
                        let _ = slot.conn.close().await;
                        if let Some(removed) = removed {
                            if !Arc::ptr_eq(&removed, &slot.conn) {
                                let _ = removed.close().await;
                            }
                        }
                        return Err(e.into());
                    }
                }
                log::trace!("{event_display} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains_key(&handle) && self.should_attempt(handle) {
            // A prospective session is not established yet. Clear any stale
            // address first so `current_session()` cannot expose the dial token
            // to Capture as a usable DTLS generation.
            self.client_manager.set_active_addr(handle, None);
            self.client_manager.set_alive(handle, false);
            let session = self.sessions.allocate(handle);
            connecting.insert(handle, session);
            // connect in the background
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                session,
                self.conns.clone(),
                self.sessions.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
                self.ping_sent_at.clone(),
                self.primary_hints.clone(),
                self.retry_state.clone(),
            ));
        }
        Err(MousehopConnectionError::NotConnected)
    }

    /// Send only on the exact already-established DTLS session. Unlike
    /// [`Self::send`], this never starts a connection and never falls through
    /// to a replacement session. It is used for atomic handover retries,
    /// Ack-gated input, and release cleanup whose meaning belongs to one
    /// particular transport generation.
    pub(crate) async fn send_on_session(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
        session: ConnectionSession,
    ) -> Result<(), MousehopConnectionError> {
        let event_display = format!("{event}");
        let send_when_emulation_inactive = matches!(&event, ProtoEvent::HostInputState { .. });
        let session_cleanup = is_session_cleanup_event(&event);
        let bytes_owned: Option<Vec<u8>> = match &event {
            ProtoEvent::Clipboard { .. } => match encode_clipboard_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping oversize clipboard event for client {handle}: {e}");
                    return Ok(());
                }
            },
            ProtoEvent::DisplayLayout { .. } => match encode_display_layout_event(&event) {
                Ok(v) => Some(v),
                Err(e) => {
                    log::warn!("dropping invalid display layout for client {handle}: {e}");
                    return Ok(());
                }
            },
            _ => None,
        };
        let bytes_fixed: ([u8; MAX_EVENT_SIZE], usize) = if bytes_owned.is_some() {
            ([0u8; MAX_EVENT_SIZE], 0)
        } else {
            event.into()
        };
        let buf: &[u8] = if let Some(v) = bytes_owned.as_deref() {
            v
        } else {
            &bytes_fixed.0[..bytes_fixed.1]
        };

        // Resolve the already-pinned transport directly. Client deletion and
        // hostname/port reconfiguration can clear or mutate active_addr before
        // Capture processes its asynchronous Destroy and sends key-up/Leave.
        // Looking up the exact (handle, session) slot lets that cleanup finish
        // without ever falling through to a replacement generation.
        let (addr, slot) = {
            let conns = self.conns.lock().await;
            conns.iter().find_map(|(&addr, slot)| {
                (slot.handle == handle && slot.session == session).then(|| (addr, slot.clone()))
            })
        }
        .ok_or(MousehopConnectionError::NotConnected)?;
        if !self.sessions.is_current(handle, session) {
            return Err(MousehopConnectionError::NotConnected);
        }
        if self.client_manager.active_addr(handle) != Some(addr) && !session_cleanup {
            return Err(MousehopConnectionError::NotConnected);
        }
        if !self.client_manager.alive(handle) && !send_when_emulation_inactive && !session_cleanup {
            return Err(MousehopConnectionError::TargetEmulationDisabled);
        }
        if let Err(e) = slot.conn.send(buf).await {
            log::warn!("client {handle} session {session} failed to send: {e}");
            let removed = disconnect(
                &self.client_manager,
                handle,
                addr,
                session,
                &self.conns,
                &self.sessions,
                &self.ping_response,
                &self.ping_sent_at,
                &self.recv_tx,
            )
            .await;
            let _ = slot.conn.close().await;
            if let Some(removed) = removed {
                if !Arc::ptr_eq(&removed, &slot.conn) {
                    let _ = removed.close().await;
                }
            }
            return Err(e.into());
        }
        log::trace!("{event_display} >->->->->- {addr} session {session}");
        Ok(())
    }

    /// Tear down any live connection for `handle` and clear its retry
    /// gate so the next send re-dials from scratch. Called when the
    /// user changes the locked address: the path we're on may be the
    /// wrong interface now, so we drop it and let `connect_to_handle`
    /// re-evaluate (honoring the new lock) on the next event. Closing
    /// the connection after the shared disconnect teardown has notified
    /// capture, so the local pointer is released immediately rather than
    /// waiting for `receive_loop` to observe the close.
    pub(crate) async fn reset_handle(&self, handle: ClientHandle) {
        let session = self.sessions.current(handle);
        if let Some(session) = session {
            if let Some(addr) = self.client_manager.active_addr(handle) {
                let conn = disconnect(
                    &self.client_manager,
                    handle,
                    addr,
                    session,
                    &self.conns,
                    &self.sessions,
                    &self.ping_response,
                    &self.ping_sent_at,
                    &self.recv_tx,
                )
                .await;
                if let Some(conn) = conn {
                    let _ = conn.close().await;
                }
            } else {
                // No established address means this is an in-flight dial.
                // Invalidating its prospective session prevents it from
                // installing after config deletion/handle reuse.
                self.sessions.remove_if_current(handle, session);
            }
            remove_connecting_if_current(&self.connecting, handle, session).await;
        }
        self.retry_state.borrow_mut().remove(&handle);
    }

    /// Decide whether to spawn another `connect_to_handle` for `handle`.
    /// Returns true (and refreshes the recorded signature) when:
    ///   - we have no prior attempt for this handle, or
    ///   - the candidate-set signature has changed since the last
    ///     attempt (new IP from DNS, or new mDNS primary), or
    ///   - the recorded backoff has elapsed.
    ///
    /// Otherwise returns false; the caller treats this as "still in
    /// cooldown, keep returning NotConnected silently."
    fn should_attempt(&self, handle: ClientHandle) -> bool {
        let ips = self.client_manager.get_ips(handle).unwrap_or_default();
        let primary = self.client_manager.get_hostname(handle).and_then(|h| {
            let key = normalize_mdns_name(&h);
            self.primary_hints.borrow().get(&key).copied()
        });
        let sig = signature_of(&ips, primary);
        let mut state = self.retry_state.borrow_mut();
        match state.get_mut(&handle) {
            None => true,
            Some(s) if s.signature != sig => {
                s.signature = sig;
                s.next_attempt_at = Instant::now();
                s.backoff = INITIAL_RETRY_BACKOFF;
                true
            }
            Some(s) => Instant::now() >= s.next_attempt_at,
        }
    }
}

async fn remove_connecting_if_current(
    connecting: &Mutex<HashMap<ClientHandle, ConnectionSession>>,
    handle: ClientHandle,
    session: ConnectionSession,
) {
    let mut connecting = connecting.lock().await;
    if connecting.get(&handle) == Some(&session) {
        connecting.remove(&handle);
    }
}

async fn finish_failed_dial(
    connecting: &Mutex<HashMap<ClientHandle, ConnectionSession>>,
    sessions: &SessionTracker,
    handle: ClientHandle,
    session: ConnectionSession,
) {
    sessions.remove_if_current(handle, session);
    remove_connecting_if_current(connecting, handle, session).await;
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    session: ConnectionSession,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    connecting: Rc<Mutex<HashMap<ClientHandle, ConnectionSession>>>,
    tx: Sender<MousehopConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
    primary_hints: PrimaryCache,
    retry_state: Rc<RefCell<HashMap<ClientHandle, RetryState>>>,
) -> Result<(), MousehopConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(ips_set) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = ips_set
            .iter()
            .copied()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        // mDNS-advertised primary IP for this peer, if known. Used
        // by `connect_any` as a head-start address: the dialer races
        // it alone for ~200ms before joining the rest of the list,
        // so a healthy primary almost always wins regardless of
        // raw RTT ordering.
        let primary_ip = client_manager.get_hostname(handle).and_then(|h| {
            let key = normalize_mdns_name(&h);
            primary_hints.borrow().get(&key).copied()
        });
        let primary_preferred = primary_ip.map(|ip| SocketAddr::new(ip, port));
        // Resolve the connection policy for this peer:
        //  * a per-network lock (already resolved against the current
        //    LAN in `active_lock`) pins the dial set to one address, so
        //    a dual-homed peer stops flapping between interfaces. Sticky
        //    — an unreachable locked address fails rather than silently
        //    falling back to another interface.
        //  * otherwise the base mode decides: `Auto` races every
        //    candidate biased to the mDNS primary; `Fastest` biases the
        //    head-start toward the lowest-latency reachable candidate
        //    (falling back to the mDNS primary before any probe lands).
        let (addrs, preferred) = match client_manager.get_active_lock(handle) {
            Some(ip) => {
                let sa = SocketAddr::new(ip, port);
                (vec![sa], Some(sa))
            }
            None => match client_manager.get_mode(handle) {
                ConnectionMode::Auto => (addrs, primary_preferred),
                ConnectionMode::Fastest => {
                    let fastest = client_manager
                        .lowest_latency_addr(handle)
                        .map(|ip| SocketAddr::new(ip, port));
                    (addrs, fastest.or(primary_preferred))
                }
            },
        };
        log::info!("client ({handle}) connecting ... (ips: {addrs:?}, preferred: {preferred:?})");
        if addrs.is_empty() && preferred.is_none() {
            // Nothing to dial. Bump backoff and bail without spawning
            // DTLS work or spamming logs on every subsequent mouse
            // event — `should_attempt` will keep gating until either
            // the backoff elapses or new info arrives.
            record_retry_failure(&retry_state, handle, &ips_set, primary_ip);
            finish_failed_dial(&connecting, &sessions, handle, session).await;
            return Err(MousehopConnectionError::NotConnected);
        }
        let res = connect_any(&addrs, preferred, cert).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                record_retry_failure(&retry_state, handle, &ips_set, primary_ip);
                finish_failed_dial(&connecting, &sessions, handle, session).await;
                return Err(e);
            }
        };
        let mut conns_guard = conns.lock().await;
        if !sessions.is_current(handle, session) || client_manager.get_state(handle).is_none() {
            drop(conns_guard);
            finish_failed_dial(&connecting, &sessions, handle, session).await;
            let _ = conn.close().await;
            log::debug!(
                "discarding completed dial for reset/replaced client {handle} session {session}"
            );
            return Err(MousehopConnectionError::NotConnected);
        }
        log::info!("client ({handle}) connected @ {addr}");
        // `alive` belongs to the authenticated protocol session, not merely
        // this client handle. A reconnect can otherwise inherit `true` from
        // the previous connection and send input before the new peer has
        // completed Hello/Pong validation.
        client_manager.set_alive(handle, false);
        client_manager.set_active_addr(handle, Some(addr));
        let replaced = conns_guard.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn: conn.clone(),
            },
        );
        drop(conns_guard);
        if let Some(replaced) = replaced {
            ping_response.borrow_mut().remove(&(addr, replaced.session));
            ping_sent_at.borrow_mut().remove(&(addr, replaced.session));
            log::info!(
                "replacing prior outbound DTLS session {} for {addr}",
                replaced.session
            );
            let _ = replaced.conn.close().await;
            if replaced.handle != handle
                && sessions.is_current(replaced.handle, replaced.session)
                && client_manager.active_addr(replaced.handle) == Some(addr)
            {
                client_manager.set_alive(replaced.handle, false);
                client_manager.set_active_addr(replaced.handle, None);
                client_manager.set_peer_commit(replaced.handle, None);
                sessions.remove_if_current(replaced.handle, replaced.session);
                let _ = tx.send(MousehopConnectionEvent::Disconnected {
                    handle: replaced.handle,
                    session: replaced.session,
                });
            }
        }
        remove_connecting_if_current(&connecting, handle, session).await;
        retry_state.borrow_mut().remove(&handle);

        // Protocol handshake. mousehop refuses any peer that does not
        // present a valid `Hello` (carrying `PROTOCOL_MAGIC`) shortly
        // after the DTLS connection authenticates — a deliberate hard
        // cut-over so mousehop never silently half-interoperates with
        // lan-mouse. `receive_loop` flips `hello_ok` once the peer's
        // echoed Hello validates; `hello_handshake` retransmits until
        // then and tears the connection down if the window elapses.
        let hello_ok = Rc::new(Cell::new(false));
        spawn_local(hello_handshake(addr, conn.clone(), hello_ok.clone()));

        // poll connection for active
        spawn_local(ping_pong(
            client_manager.clone(),
            handle,
            addr,
            session,
            conn.clone(),
            conns.clone(),
            sessions.clone(),
            tx.clone(),
            ping_response.clone(),
            ping_sent_at.clone(),
        ));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            session,
            conn,
            conns,
            sessions,
            tx,
            ping_response.clone(),
            ping_sent_at,
            hello_ok,
        ));
        return Ok(());
    }
    finish_failed_dial(&connecting, &sessions, handle, session).await;
    Err(MousehopConnectionError::NotConnected)
}

/// Number of times the connect side retransmits its `Hello` while
/// waiting for the peer to echo a valid one back, and the gap
/// between attempts. Their product is the effective handshake
/// deadline: if `hello_ok` is still unset after the final attempt
/// the peer never spoke a valid mousehop handshake and the
/// connection is closed.
const HELLO_MAX_ATTEMPTS: u32 = 8;
const HELLO_RETRY_INTERVAL: Duration = Duration::from_millis(750);

/// Drive the protocol handshake on a freshly-connected outbound DTLS
/// link. Retransmits our [`ProtoEvent::hello`] until `receive_loop`
/// flips `hello_ok` (the peer echoed a `PROTOCOL_MAGIC`-stamped
/// Hello) or the attempt budget runs out. A peer that never returns
/// a valid Hello — a stock lan-mouse, or anything that is not
/// mousehop — has its connection refused here. This is the
/// connect-side half of the deliberate hard cut-over from lan-mouse.
async fn hello_handshake(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    hello_ok: Rc<Cell<bool>>,
) {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::hello(local_commit()).into();
    for _ in 0..HELLO_MAX_ATTEMPTS {
        if hello_ok.get() {
            return;
        }
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }
        tokio::time::sleep(HELLO_RETRY_INTERVAL).await;
    }
    if !hello_ok.get() {
        log::warn!(
            "refusing {addr}: peer did not complete the mousehop handshake \
             (no valid Hello) — closing connection"
        );
        let _ = conn.close().await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn ping_pong(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
    conn: ArcConn,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    tx: Sender<MousehopConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
) {
    loop {
        if client_manager.get_state(handle).is_none()
            || !connection_is_current(&conns, &sessions, handle, addr, session).await
        {
            disconnect(
                &client_manager,
                handle,
                addr,
                session,
                &conns,
                &sessions,
                &ping_response,
                &ping_sent_at,
                &tx,
            )
            .await;
            let _ = conn.close().await;
            return;
        }
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if client_manager.get_state(handle).is_none()
                || !connection_is_current(&conns, &sessions, handle, addr, session).await
            {
                disconnect(
                    &client_manager,
                    handle,
                    addr,
                    session,
                    &conns,
                    &sessions,
                    &ping_response,
                    &ping_sent_at,
                    &tx,
                )
                .await;
                let _ = conn.close().await;
                return;
            }
            // Stamp the send time so `receive_loop` can derive the live
            // RTT from the matching Pong. On a LAN the Pong returns well
            // within the 500 ms inter-ping gap, so the most-recent stamp
            // is the one being answered.
            ping_sent_at
                .borrow_mut()
                .insert((addr, session), Instant::now());
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                disconnect(
                    &client_manager,
                    handle,
                    addr,
                    session,
                    &conns,
                    &sessions,
                    &ping_response,
                    &ping_sent_at,
                    &tx,
                )
                .await;
                let _ = conn.close().await;
                return;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&(addr, session)) {
            log::warn!("{addr} did not respond, closing connection");
            disconnect(
                &client_manager,
                handle,
                addr,
                session,
                &conns,
                &sessions,
                &ping_response,
                &ping_sent_at,
                &tx,
            )
            .await;
            let _ = conn.close().await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
    conn: ArcConn,
    conns: Rc<Mutex<HashMap<SocketAddr, ConnectionSlot>>>,
    sessions: SessionTracker,
    tx: Sender<MousehopConnectionEvent>,
    ping_response: Rc<RefCell<HashSet<(SocketAddr, ConnectionSession)>>>,
    ping_sent_at: Rc<RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>>,
    hello_ok: Rc<Cell<bool>>,
) {
    // Buffer sized for the largest legal variable-length frame so a single
    // DTLS recv never gets truncated. Fixed events use only the first
    // MAX_EVENT_SIZE bytes; topology remains below the clipboard cap.
    let mut buf = [0u8; MAX_CLIPBOARD_SIZE];
    while let Ok(n) = conn.recv(&mut buf).await {
        if n == 0 {
            continue;
        }
        let datagram = &buf[..n];
        let event = match decode_proto_datagram(datagram) {
            Some(event) => event,
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            None => {
                log::debug!("ignoring undecodable {n}-byte event from {addr}");
                continue;
            }
        };
        log::trace!("{addr} <==<==<== {event}");
        if client_manager.get_state(handle).is_none()
            || !connection_is_current(&conns, &sessions, handle, addr, session).await
        {
            log::debug!("ending replaced outbound DTLS receive session {session} for {addr}");
            break;
        }
        if !handshake_allows_event(hello_ok.get(), &event) {
            log::debug!("ignoring pre-Hello event from {addr}: {event}");
            continue;
        }
        match event {
            ProtoEvent::Pong(b) => {
                client_manager.set_active_addr(handle, Some(addr));
                client_manager.set_alive(handle, b);
                ping_response.borrow_mut().insert((addr, session));
                // Live RTT of the active connection over the real DTLS
                // path — accurate and firewall-proof (unlike the TCP
                // probe). Quantize to 100 µs to match the prober.
                if let Some(sent) = ping_sent_at.borrow_mut().remove(&(addr, session)) {
                    let us = sent.elapsed().as_micros().min(u32::MAX as u128) as u32;
                    client_manager.set_latency(handle, addr.ip(), Some(us - (us % 100)));
                }
            }
            ProtoEvent::Hello {
                magic,
                commit,
                capabilities,
            } => {
                if magic != PROTOCOL_MAGIC {
                    log::warn!(
                        "refusing {addr}: peer presented a foreign protocol \
                         handshake (not mousehop) — closing connection"
                    );
                    let _ = conn.close().await;
                    break;
                }
                hello_ok.set(true);
                client_manager.set_peer_commit(handle, Some(commit));
                // Forward to capture.rs so Service can
                // broadcast — without this the GUI's
                // version-status indicator only updates when
                // the listen-side `PeerHello` happens to
                // match `get_client(addr)`, which fails when
                // Mac dials in before Linux's outbound dial
                // has populated `active_addr`.
                tx.send(MousehopConnectionEvent::Received {
                    handle,
                    addr,
                    session,
                    event: ProtoEvent::Hello {
                        magic,
                        commit,
                        capabilities,
                    },
                })
                .expect("channel closed");
            }
            event => tx
                .send(MousehopConnectionEvent::Received {
                    handle,
                    addr,
                    session,
                    event,
                })
                .expect("channel closed"),
        }
    }
    log::debug!("{addr}: receive loop ended");
    disconnect(
        &client_manager,
        handle,
        addr,
        session,
        &conns,
        &sessions,
        &ping_response,
        &ping_sent_at,
        &tx,
    )
    .await;
}

/// Classify the first byte of a DTLS datagram and dispatch through
/// a variable-length codec or the fixed-buffer `try_into` path. Returns
/// `None` on any decode failure (bad tag, truncated payload, oversize frame).
fn decode_proto_datagram(bytes: &[u8]) -> Option<ProtoEvent> {
    use mousehop_proto::EventType;
    let tag = *bytes.first()?;
    if tag == EventType::Clipboard as u8 {
        return decode_clipboard_event(bytes).ok();
    }
    if tag == EventType::DisplayLayout as u8 {
        return decode_display_layout_event(bytes).ok();
    }
    decode_fixed_event(bytes).ok()
}

async fn connection_is_current(
    conns: &Mutex<HashMap<SocketAddr, ConnectionSlot>>,
    sessions: &SessionTracker,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
) -> bool {
    sessions.is_current(handle, session)
        && conns
            .lock()
            .await
            .get(&addr)
            .is_some_and(|slot| slot.handle == handle && slot.session == session)
}

#[allow(clippy::too_many_arguments)]
async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    session: ConnectionSession,
    conns: &Mutex<HashMap<SocketAddr, ConnectionSlot>>,
    sessions: &SessionTracker,
    ping_response: &RefCell<HashSet<(SocketAddr, ConnectionSession)>>,
    ping_sent_at: &RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>,
    tx: &Sender<MousehopConnectionEvent>,
) -> Option<ArcConn> {
    log::warn!("client ({handle}) @ {addr} session {session} connection closed");
    clear_session_ping_state(ping_response, ping_sent_at, addr, session);
    let mut conns = conns.lock().await;
    let removed = match conns.get(&addr) {
        Some(slot) if slot.handle == handle && slot.session == session => {
            conns.remove(&addr).map(|slot| slot.conn)
        }
        Some(_) => {
            log::debug!(
                "ignoring stale disconnect for client ({handle}) @ {addr}: session {session} was replaced"
            );
            return None;
        }
        None => None,
    };
    let active: Vec<SocketAddr> = conns.keys().copied().collect();
    drop(conns);

    // A receive task from an older address can finish after a replacement
    // connection has already become active. Only the task that owned the
    // current address may clear client state or release capture.
    let manager_removed = client_manager.get_state(handle).is_none();
    if sessions.is_current(handle, session)
        && (manager_removed || client_manager.active_addr(handle) == Some(addr))
    {
        if !manager_removed {
            client_manager.set_alive(handle, false);
            client_manager.set_active_addr(handle, None);
            client_manager.set_peer_commit(handle, None);
        }
        sessions.remove_if_current(handle, session);
        let _ = tx.send(MousehopConnectionEvent::Disconnected { handle, session });
    }
    log::info!("active connections: {active:?}");
    removed
}

fn clear_session_ping_state(
    ping_response: &RefCell<HashSet<(SocketAddr, ConnectionSession)>>,
    ping_sent_at: &RefCell<HashMap<(SocketAddr, ConnectionSession), Instant>>,
    addr: SocketAddr,
    session: ConnectionSession,
) {
    ping_response.borrow_mut().remove(&(addr, session));
    ping_sent_at.borrow_mut().remove(&(addr, session));
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::{Event, KeyboardEvent, PointerEvent, display::DisplayLayout};

    fn connection_map() -> Mutex<HashMap<SocketAddr, ConnectionSlot>> {
        Mutex::new(HashMap::new())
    }

    async fn test_conn() -> ArcConn {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("test socket"))
    }

    async fn connected_test_conn() -> (ArcConn, UdpSocket, SocketAddr) {
        let receiver = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("receiver socket");
        let addr = receiver.local_addr().expect("receiver address");
        let sender = UdpSocket::bind("127.0.0.1:0").await.expect("sender socket");
        sender.connect(addr).await.expect("connect sender");
        (Arc::new(sender), receiver, addr)
    }

    #[test]
    fn variable_topology_datagram_routes_through_display_codec() {
        let layout = DisplayLayout::new([(-1024, 0, 1024, 600), (0, 0, 3072, 1728)]);
        let bytes = encode_display_layout_event(&ProtoEvent::display_layout(layout.clone()))
            .expect("encode topology");
        assert!(matches!(
            decode_proto_datagram(&bytes),
            Some(ProtoEvent::DisplayLayout { layout: decoded, .. }) if decoded == layout
        ));

        let mut malformed = bytes;
        malformed[1] = mousehop_proto::DISPLAY_LAYOUT_VERSION.wrapping_add(1);
        assert!(decode_proto_datagram(&malformed).is_none());
    }

    #[test]
    fn handshake_rejects_all_non_hello_events_until_validated() {
        let input = ProtoEvent::Input(Event::Pointer(PointerEvent::Motion {
            time: 1,
            dx: 2.0,
            dy: 3.0,
        }));
        for event in [ProtoEvent::Ack(0), ProtoEvent::Pong(true), input] {
            assert!(!handshake_allows_event(false, &event));
            assert!(handshake_allows_event(true, &event));
        }
        assert!(handshake_allows_event(
            false,
            &ProtoEvent::hello(*b"deadbeef")
        ));
    }

    #[tokio::test]
    async fn active_disconnect_notifies_capture_even_if_map_was_already_drained() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        client_manager.set_peer_commit(handle, Some(*b"deadbeef"));
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([(addr, session)]));
        let ping_sent_at = RefCell::new(HashMap::from([((addr, session), Instant::now())]));
        let (tx, mut rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            addr,
            session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_none());
        assert!(matches!(
            rx.recv().await,
            Some(MousehopConnectionEvent::Disconnected { handle: disconnected, session: disconnected_session })
                if disconnected == handle && disconnected_session == session
        ));
        assert_eq!(client_manager.active_addr(handle), None);
        assert!(!client_manager.alive(handle));
        assert_eq!(sessions.current(handle), None);
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert_eq!(
            client_manager
                .get_state(handle)
                .expect("client state")
                .1
                .peer_commit,
            None
        );
    }

    #[tokio::test]
    async fn matching_connection_identity_removes_and_notifies() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let current = test_conn().await;
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([(addr, session)]));
        let ping_sent_at = RefCell::new(HashMap::from([((addr, session), Instant::now())]));
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn: current.clone(),
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        let (tx, mut rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            addr,
            session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_some_and(|conn| Arc::ptr_eq(&conn, &current)));
        assert!(matches!(
            rx.recv().await,
            Some(MousehopConnectionEvent::Disconnected { handle: disconnected, session: disconnected_session })
                if disconnected == handle && disconnected_session == session
        ));
        assert_eq!(client_manager.active_addr(handle), None);
        assert!(!client_manager.alive(handle));
        assert_eq!(sessions.current(handle), None);
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert!(conns.lock().await.get(&addr).is_none());
    }

    #[tokio::test]
    async fn stale_disconnect_does_not_clear_replacement_connection() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let old_addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let new_addr: SocketAddr = "127.0.0.2:4242".parse().expect("test address");
        let commit = *b"cafebabe";
        client_manager.set_active_addr(handle, Some(new_addr));
        client_manager.set_alive(handle, true);
        client_manager.set_peer_commit(handle, Some(commit));
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([(old_addr, session)]));
        let ping_sent_at = RefCell::new(HashMap::from([((old_addr, session), Instant::now())]));
        let (tx, _rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            old_addr,
            session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_none());
        assert_eq!(client_manager.active_addr(handle), Some(new_addr));
        assert!(client_manager.alive(handle));
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert_eq!(
            client_manager
                .get_state(handle)
                .expect("client state")
                .1
                .peer_commit,
            Some(commit)
        );
    }

    #[tokio::test]
    async fn stale_same_address_disconnect_preserves_replacement_connection() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let replacement = test_conn().await;
        let conns = connection_map();
        let sessions = SessionTracker::default();
        let old_session = sessions.allocate(handle);
        let replacement_session = sessions.allocate(handle);
        let ping_response = RefCell::new(HashSet::from([
            (addr, old_session),
            (addr, replacement_session),
        ]));
        let ping_sent_at = RefCell::new(HashMap::from([
            ((addr, old_session), Instant::now()),
            ((addr, replacement_session), Instant::now()),
        ]));
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session: replacement_session,
                conn: replacement.clone(),
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        let commit = *b"cafebabe";
        client_manager.set_peer_commit(handle, Some(commit));
        let (tx, _rx) = channel();

        let removed = disconnect(
            &client_manager,
            handle,
            addr,
            old_session,
            &conns,
            &sessions,
            &ping_response,
            &ping_sent_at,
            &tx,
        )
        .await;

        assert!(removed.is_none());
        assert_eq!(client_manager.active_addr(handle), Some(addr));
        assert!(client_manager.alive(handle));
        assert_eq!(
            ping_response.borrow().iter().copied().collect::<Vec<_>>(),
            vec![(addr, replacement_session)]
        );
        assert_eq!(ping_sent_at.borrow().len(), 1);
        assert!(
            ping_sent_at
                .borrow()
                .contains_key(&(addr, replacement_session))
        );
        assert_eq!(
            client_manager
                .get_state(handle)
                .expect("client state")
                .1
                .peer_commit,
            Some(commit)
        );
        let current = conns
            .lock()
            .await
            .get(&addr)
            .cloned()
            .expect("replacement remains");
        assert_eq!(current.session, replacement_session);
        assert!(Arc::ptr_eq(&current.conn, &replacement));
    }

    #[tokio::test]
    async fn queued_old_session_is_rejected_after_replacement() {
        let handle = 7;
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let sessions = SessionTracker::default();
        let old_session = sessions.allocate(handle);
        let current_session = sessions.allocate(handle);
        let conns = connection_map();
        let current_conn = test_conn().await;
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session: current_session,
                conn: current_conn,
            },
        );

        assert!(!sessions.is_current(handle, old_session));
        assert!(!connection_is_current(&conns, &sessions, handle, addr, old_session).await);
        assert!(connection_is_current(&conns, &sessions, handle, addr, current_session).await);
    }

    #[tokio::test]
    async fn deleted_client_terminal_disconnect_removes_slot_and_tracker() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let current = test_conn().await;
        let conns = Rc::new(connection_map());
        let sessions = SessionTracker::default();
        let session = sessions.allocate(handle);
        conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn: current.clone(),
            },
        );
        let ping_response = Rc::new(RefCell::new(HashSet::from([(addr, session)])));
        let ping_sent_at = Rc::new(RefCell::new(HashMap::from([(
            (addr, session),
            Instant::now(),
        )])));
        let (tx, mut rx) = channel();
        client_manager
            .remove_client(handle)
            .expect("remove configured client");

        ping_pong(
            client_manager,
            handle,
            addr,
            session,
            current,
            conns.clone(),
            sessions.clone(),
            tx,
            ping_response.clone(),
            ping_sent_at.clone(),
        )
        .await;

        assert!(conns.lock().await.is_empty());
        assert_eq!(sessions.current(handle), None);
        assert!(ping_response.borrow().is_empty());
        assert!(ping_sent_at.borrow().is_empty());
        assert!(matches!(
            rx.recv().await,
            Some(MousehopConnectionEvent::Disconnected {
                handle: disconnected,
                session: disconnected_session,
            }) if disconnected == handle && disconnected_session == session
        ));
    }

    #[tokio::test]
    async fn reset_handle_invalidates_inflight_dial_before_handle_reuse() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let cert = Certificate::generate_self_signed(["mousehop-dial-reset-test".to_owned()])
            .expect("test certificate");
        let connection =
            MousehopConnection::new(cert, client_manager.clone(), PrimaryCache::default());
        let session = connection.sessions.allocate(handle);
        connection.connecting.lock().await.insert(handle, session);

        connection.reset_handle(handle).await;

        assert_eq!(connection.sessions.current(handle), None);
        assert!(!connection.connecting.lock().await.contains_key(&handle));
    }

    #[tokio::test]
    async fn old_dial_completion_cannot_clear_replacement_attempt_token() {
        let handle = 7;
        let connecting = Mutex::new(HashMap::new());
        let sessions = SessionTracker::default();
        let old_session = sessions.allocate(handle);
        let replacement_session = sessions.allocate(handle);
        connecting.lock().await.insert(handle, replacement_session);

        finish_failed_dial(&connecting, &sessions, handle, old_session).await;

        assert_eq!(sessions.current(handle), Some(replacement_session));
        assert_eq!(
            connecting.lock().await.get(&handle).copied(),
            Some(replacement_session)
        );
    }

    #[tokio::test]
    async fn session_pinned_send_never_falls_through_to_replacement() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let cert = Certificate::generate_self_signed(["mousehop-session-send-test".to_owned()])
            .expect("test certificate");
        let connection =
            MousehopConnection::new(cert, client_manager.clone(), PrimaryCache::default());
        let old_session = connection.sessions.allocate(handle);
        let replacement_session = connection.sessions.allocate(handle);
        connection.conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session: replacement_session,
                conn: test_conn().await,
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);

        assert!(matches!(
            connection
                .send_on_session(ProtoEvent::Ack(17), handle, old_session)
                .await,
            Err(MousehopConnectionError::NotConnected)
        ));
        assert_eq!(
            connection.current_session(handle),
            Some(replacement_session)
        );
    }

    #[tokio::test]
    async fn pinned_release_cleanup_survives_client_manager_removal() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let cert = Certificate::generate_self_signed(["mousehop-release-cleanup-test".to_owned()])
            .expect("test certificate");
        let connection =
            MousehopConnection::new(cert, client_manager.clone(), PrimaryCache::default());
        let session = connection.sessions.allocate(handle);
        let (conn, receiver, addr) = connected_test_conn().await;
        connection.conns.lock().await.insert(
            addr,
            ConnectionSlot {
                handle,
                session,
                conn,
            },
        );
        client_manager.set_active_addr(handle, Some(addr));
        client_manager.set_alive(handle, true);
        client_manager
            .remove_client(handle)
            .expect("remove configured client before asynchronous capture cleanup");

        let cleanup = [
            ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 0,
            })),
            ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            })),
            ProtoEvent::Leave(0),
            ProtoEvent::HandoverInput {
                serial: 47,
                event: Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: 30,
                    state: 0,
                }),
            },
            ProtoEvent::HandoverInput {
                serial: 47,
                event: Event::Keyboard(KeyboardEvent::Modifiers {
                    depressed: 0,
                    latched: 0,
                    locked: 0,
                    group: 0,
                }),
            },
            ProtoEvent::HandoverLeave {
                serial: 47,
                mode: mousehop_proto::LEAVE_HANDOVER,
            },
        ];
        for expected in cleanup {
            connection
                .send_on_session(expected.clone(), handle, session)
                .await
                .expect("exact-session cleanup remains deliverable");
            let mut bytes = [0u8; MAX_EVENT_SIZE];
            let n = tokio::time::timeout(Duration::from_secs(1), receiver.recv(&mut bytes))
                .await
                .expect("cleanup datagram timeout")
                .expect("receive cleanup datagram");
            let decoded = decode_fixed_event(&bytes[..n]).expect("decode cleanup datagram");
            assert_eq!(format!("{decoded}"), format!("{expected}"));
        }

        let stale_key_down = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
            time: 1,
            key: 30,
            state: 1,
        }));
        assert!(matches!(
            connection
                .send_on_session(stale_key_down, handle, session)
                .await,
            Err(MousehopConnectionError::NotConnected)
        ));

        let stale_transactional_key_down = ProtoEvent::HandoverInput {
            serial: 47,
            event: Event::Keyboard(KeyboardEvent::Key {
                time: 1,
                key: 30,
                state: 1,
            }),
        };
        assert!(matches!(
            connection
                .send_on_session(stale_transactional_key_down, handle, session)
                .await,
            Err(MousehopConnectionError::NotConnected)
        ));
    }
}
