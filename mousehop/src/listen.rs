use futures::{Stream, StreamExt};
use local_channel::mpsc::{Receiver, Sender, channel};
use mousehop_ipc::IncomingPeerConfig;
use mousehop_proto::{
    MAX_CLIPBOARD_SIZE, MAX_EVENT_SIZE, PROTOCOL_MAGIC, ProtoEvent, decode_clipboard_event,
    decode_display_layout_event, decode_fixed_event, encode_clipboard_event,
    encode_display_layout_event,
};
use rustls::pki_types::CertificateDer;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::time::MissedTickBehavior;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::{
    config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
    listener::listen,
};
use webrtc_util::{Conn, Error, conn::Listener};

use crate::{connect::handshake_allows_event, crypto};

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
    #[error(transparent)]
    WebrtcDtls(#[from] webrtc_dtls::Error),
    #[error("no listener could be bound on any local address")]
    NoBoundListener,
}

type ArcConn = Arc<dyn Conn + Send + Sync>;
type DynListener = Box<dyn Listener + Send + Sync>;
pub(crate) type ListenerSession = u64;

#[derive(Clone)]
struct AcceptedConnection {
    session: ListenerSession,
    conn: ArcConn,
}

#[derive(Clone, Default)]
struct ListenerSessions {
    next: Rc<Cell<ListenerSession>>,
    current: Rc<RefCell<HashMap<SocketAddr, ListenerSession>>>,
}

impl ListenerSessions {
    fn allocate(&self, addr: SocketAddr) -> ListenerSession {
        let mut session = self.next.get().wrapping_add(1);
        if session == 0 {
            session = 1;
        }
        self.next.set(session);
        self.current.borrow_mut().insert(addr, session);
        session
    }

    fn is_current(&self, addr: SocketAddr, session: ListenerSession) -> bool {
        self.current.borrow().get(&addr).copied() == Some(session)
    }

    fn remove_if_current(&self, addr: SocketAddr, session: ListenerSession) -> bool {
        if !self.is_current(addr, session) {
            return false;
        }
        self.current.borrow_mut().remove(&addr);
        true
    }
}

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
        session: ListenerSession,
    },
    Accept {
        addr: SocketAddr,
        session: ListenerSession,
        fingerprint: String,
    },
    Disconnected {
        addr: SocketAddr,
        session: ListenerSession,
    },
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct MousehopListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    listen_task: JoinHandle<()>,
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: ListenerSessions,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
    /// macOS-only: held for its `Drop` side effect (stops the
    /// CFRunLoop in the power-observer thread). The observer sends
    /// `()` into the wake channel on system-wake; the supervisor
    /// task drains that channel and force-closes peer conns so
    /// reconnect happens immediately after a screensaver/sleep
    /// dismissal instead of waiting out `RECV_IDLE_TIMEOUT`.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    power_observer: crate::macos_power::PowerObserver,
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

/// One bound DTLS listener and the task that accepts on it. Stored
/// in a `HashMap<IpAddr, ListenerSlot>` keyed by the local IPv4
/// address it's bound to so the supervisor can plug/unplug
/// listeners as interfaces appear/disappear.
struct ListenerSlot {
    /// Background task that calls `listener.accept()` in a loop and
    /// forwards events into the shared `listen_tx` / `conns`.
    /// Aborted on `Drop` so dropping the supervisor cleans up.
    accept_task: JoinHandle<()>,
}

impl Drop for ListenerSlot {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl MousehopListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, IncomingPeerConfig>>>,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (request_port_change, request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();
        let connection_attempts: Arc<Mutex<VecDeque<String>>> = Default::default();

        let authorized = authorized_keys.clone();
        let verify_peer_certificate: Option<VerifyPeerCertificateFn> = {
            let connection_attempts = connection_attempts.clone();
            Some(Arc::new(
                move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                    assert!(certs.len() == 1);
                    let fingerprints = certs
                        .iter()
                        .map(|c| crypto::generate_fingerprint(c))
                        .collect::<Vec<_>>();
                    if authorized
                        .read()
                        .expect("lock")
                        .contains_key(&fingerprints[0])
                    {
                        Ok(())
                    } else {
                        let fingerprint = fingerprints.into_iter().next().expect("fingerprint");
                        connection_attempts
                            .lock()
                            .expect("lock")
                            .push_back(fingerprint);
                        Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
                    }
                },
            ))
        };
        let cfg = Config {
            certificates: vec![cert.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            verify_peer_certificate,
            ..Default::default()
        };

        let conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>> =
            Rc::new(AsyncMutex::new(HashMap::new()));
        let sessions = ListenerSessions::default();

        // Bind one listener per local address (v4 + v6, skipping
        // loopback / link-local / multicast) instead of a single
        // wildcard listener. With a single 0.0.0.0 / [::] bind on a
        // multi-homed host, replies use the kernel's preferred
        // outbound interface as source IP — which may not match the
        // IP the peer dialed, breaking DTLS 4-tuple matching.
        // Per-IP binds make replies symmetric automatically: each
        // listener's reply socket is bound to a specific IP, so the
        // kernel uses *that* IP as source.
        let initial_addrs = enumerate_listenable_addrs();
        if initial_addrs.is_empty() {
            // Fall back to 0.0.0.0 so we at least listen somewhere if
            // interface enumeration fails (very unusual).
            log::warn!("no listenable IP addresses found; falling back to 0.0.0.0");
        }
        let mut listeners: HashMap<IpAddr, ListenerSlot> = HashMap::new();
        let mut bound_count = 0usize;
        for ip in &initial_addrs {
            match try_bind_listener(*ip, port, &cfg).await {
                Ok(listener) => {
                    let task = spawn_accept_task(
                        listener,
                        listen_tx.clone(),
                        conns.clone(),
                        sessions.clone(),
                        connection_attempts.clone(),
                    );
                    listeners.insert(*ip, ListenerSlot { accept_task: task });
                    bound_count += 1;
                    log::info!("listening for DTLS on {ip}:{port}");
                }
                Err(e) => log::warn!("failed to bind listener on {ip}:{port}: {e}"),
            }
        }
        if bound_count == 0 {
            // Either enumeration returned no addrs, or every bind
            // failed. Try `0.0.0.0:port` as a last resort.
            let fallback = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
            match try_bind_listener(fallback, port, &cfg).await {
                Ok(listener) => {
                    let task = spawn_accept_task(
                        listener,
                        listen_tx.clone(),
                        conns.clone(),
                        sessions.clone(),
                        connection_attempts.clone(),
                    );
                    listeners.insert(fallback, ListenerSlot { accept_task: task });
                    log::info!(
                        "listening for DTLS on {fallback}:{port} (fallback — symmetric replies not guaranteed)"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        // macOS wake → force-close-all-conns plumbing. On non-macOS
        // the receiver is `None` and the supervisor's wake arm stays
        // permanently pending — no behavior change there.
        #[cfg(target_os = "macos")]
        let (power_observer, wake_rx) = {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let observer = crate::macos_power::PowerObserver::spawn(tx).await;
            (observer, Some(rx))
        };
        #[cfg(not(target_os = "macos"))]
        let wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;

        let listen_task = spawn_supervisor_task(
            port,
            cfg,
            listeners,
            listen_tx.clone(),
            conns.clone(),
            sessions.clone(),
            connection_attempts,
            request_port_change_rx,
            port_changed_tx,
            wake_rx,
        );

        Ok(Self {
            conns,
            sessions,
            listen_rx,
            listen_tx,
            listen_task,
            port_changed,
            request_port_change,
            #[cfg(target_os = "macos")]
            power_observer,
        })
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        self.request_port_change.send(port).expect("channel closed");
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed.recv().await.expect("channel closed")
    }

    pub(crate) fn current_session(&self, addr: SocketAddr) -> Option<ListenerSession> {
        self.sessions.current.borrow().get(&addr).copied()
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let conns = self.conns.lock().await;
        for slot in conns.values() {
            let _ = slot.conn.close().await;
        }
        self.listen_tx.close();
    }

    pub(crate) async fn reply(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
        event: ProtoEvent,
    ) {
        log::trace!("reply {event} >=>=>=>=>=> {addr} session {session}");
        let bytes_owned = match &event {
            ProtoEvent::Clipboard { .. } => encode_clipboard_event(&event),
            ProtoEvent::DisplayLayout { .. } => encode_display_layout_event(&event),
            _ => Ok(Vec::new()),
        };
        let bytes_owned = match bytes_owned {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            Ok(_) => None,
            Err(e) => {
                log::warn!("dropping invalid reply to {addr}: {e}");
                return;
            }
        };
        let bytes_fixed: ([u8; MAX_EVENT_SIZE], usize) = if bytes_owned.is_some() {
            ([0u8; MAX_EVENT_SIZE], 0)
        } else {
            event.into()
        };
        let buf: &[u8] = if let Some(bytes) = bytes_owned.as_deref() {
            bytes
        } else {
            &bytes_fixed.0[..bytes_fixed.1]
        };
        let conn = self
            .conns
            .lock()
            .await
            .get(&addr)
            .filter(|slot| slot.session == session)
            .map(|slot| slot.conn.clone());
        if let Some(conn) = conn {
            let _ = conn.send(buf).await;
        }
    }

    pub(crate) async fn get_certificate_fingerprint(
        &self,
        addr: SocketAddr,
        session: ListenerSession,
    ) -> Option<String> {
        let conn = self
            .conns
            .lock()
            .await
            .get(&addr)
            .filter(|slot| slot.session == session)
            .map(|slot| slot.conn.clone());
        if let Some(conn) = conn {
            let conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
            let certs = conn.connection_state().await.peer_certificates;
            let cert = certs.first()?;
            let fingerprint = crypto::generate_fingerprint(cert);
            Some(fingerprint)
        } else {
            None
        }
    }
}

impl Stream for MousehopListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            match self.listen_rx.poll_next_unpin(cx) {
                std::task::Poll::Ready(Some(event)) => {
                    let current = listen_event_is_current(&self.sessions, &event);
                    if current {
                        return std::task::Poll::Ready(Some(event));
                    }
                    log::debug!("dropping queued event from a replaced inbound DTLS session");
                }
                other => return other,
            }
        }
    }
}

fn listen_event_is_current(sessions: &ListenerSessions, event: &ListenEvent) -> bool {
    match event {
        ListenEvent::Msg { addr, session, .. } | ListenEvent::Accept { addr, session, .. } => {
            sessions.is_current(*addr, *session)
        }
        // The read loop removes the current token before it publishes this
        // terminal event. Accept it only while no replacement generation has
        // appeared.
        ListenEvent::Disconnected { addr, .. } => sessions.current.borrow().get(addr).is_none(),
        ListenEvent::Rejected { .. } => true,
    }
}

/// Whether an address is worth binding a DTLS listener to: a remote
/// peer must be able to reach it. Excludes loopback, link-local
/// (needs a scope id we don't carry), unspecified, and multicast.
/// Used by both the initial enumeration and the live `IfEvent::Up`
/// filter so the two stay in lockstep on what counts as listenable.
fn is_listenable_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local(),
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                // fe80::/10 — link-local; reachable only with a
                // scope id which neither our mDNS records nor the
                // peer config carry, so a bind that succeeds still
                // can't accept connections from a remote peer.
                && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    }
}

/// Enumerate local addresses suitable for binding a DTLS listener.
/// Returns both IPv4 and IPv6 addresses; `is_listenable_addr` drops
/// the ones a remote peer cannot reach (loopback, link-local,
/// unspecified, multicast).
fn enumerate_listenable_addrs() -> Vec<IpAddr> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("get_if_addrs failed: {e}");
            return Vec::new();
        }
    };
    ifaces
        .into_iter()
        .map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) => IpAddr::V4(v4.ip),
            if_addrs::IfAddr::V6(v6) => IpAddr::V6(v6.ip),
        })
        .filter(|ip| is_listenable_addr(*ip))
        .collect()
}

async fn try_bind_listener(
    ip: IpAddr,
    port: u16,
    cfg: &Config,
) -> Result<DynListener, ListenerCreationError> {
    let addr = SocketAddr::new(ip, port);
    let listener = listen(addr, cfg.clone()).await?;
    Ok(Box::new(listener))
}

/// Spawn an accept loop for one bound listener. Each accepted
/// `(conn, addr)` is registered in the shared `conns` vec and an
/// `Accept` event is published. Verify-peer-certificate failures are
/// re-published as `Rejected` so the UI can surface unauthorized
/// fingerprints. The accept loop exits when the listener errors
/// out or its task is aborted (interface went down, port changed).
fn spawn_accept_task(
    listener: DynListener,
    listen_tx: Sender<ListenEvent>,
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: ListenerSessions,
    connection_attempts: Arc<Mutex<VecDeque<String>>>,
) -> JoinHandle<()> {
    spawn_local(async move {
        loop {
            // workaround for https://github.com/webrtc-rs/webrtc/issues/614
            let sleep = tokio::time::sleep(Duration::from_secs(2));
            tokio::select! {
                _ = sleep => continue,
                c = listener.accept() => match c {
                    Ok((conn, addr)) => {
                        log::info!("dtls client connected, ip: {addr}");
                        let session = sessions.allocate(addr);
                        let replaced = {
                            let mut conns_guard = conns.lock().await;
                            conns_guard.insert(
                                addr,
                                AcceptedConnection {
                                    session,
                                    conn: conn.clone(),
                                },
                            )
                        };
                        if let Some(replaced) = replaced {
                            log::info!(
                                "replacing prior DTLS session {} for reused peer address {addr}",
                                replaced.session
                            );
                            let _ = replaced.conn.close().await;
                        }
                        let dtls_conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
                        let certs = dtls_conn.connection_state().await.peer_certificates;
                        let cert = certs.first().expect("cert");
                        let fingerprint = crypto::generate_fingerprint(cert);
                        listen_tx
                            .send(ListenEvent::Accept {
                                addr,
                                session,
                                fingerprint,
                            })
                            .expect("channel closed");
                        spawn_local(read_loop(
                            conns.clone(),
                            sessions.clone(),
                            addr,
                            session,
                            conn,
                            listen_tx.clone(),
                        ));
                    }
                    Err(e) => {
                        if let Error::Std(ref se) = e {
                            if let Some(de) = se.0.downcast_ref::<webrtc_dtls::Error>() {
                                match de {
                                    webrtc_dtls::Error::ErrVerifyDataMismatch => {
                                        if let Some(fingerprint) =
                                            connection_attempts.lock().expect("lock").pop_front()
                                        {
                                            listen_tx
                                                .send(ListenEvent::Rejected { fingerprint })
                                                .expect("channel closed");
                                        }
                                    }
                                    _ => log::warn!("accept: {de}"),
                                }
                            } else {
                                log::warn!("accept: {se:?}");
                            }
                        } else {
                            log::warn!("accept: {e:?}");
                        }
                    }
                },
            };
        }
    })
}

/// Supervisor task: owns the set of active listeners, watches for
/// interface up/down events via `if_watch`, and rebuilds listeners
/// on port change. Each listener slot is keyed by its local IPv4
/// address; on `IfEvent::Up` we add a slot, on `IfEvent::Down` we
/// drop one (which aborts its accept task).
#[allow(clippy::too_many_arguments)]
fn spawn_supervisor_task(
    initial_port: u16,
    cfg: Config,
    initial_listeners: HashMap<IpAddr, ListenerSlot>,
    listen_tx: Sender<ListenEvent>,
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: ListenerSessions,
    connection_attempts: Arc<Mutex<VecDeque<String>>>,
    mut request_port_change_rx: Receiver<u16>,
    port_changed_tx: Sender<Result<u16, ListenerCreationError>>,
    mut wake_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
) -> JoinHandle<()> {
    spawn_local(async move {
        let mut port = initial_port;
        let mut listeners = initial_listeners;
        let mut watcher = match if_watch::tokio::IfWatcher::new() {
            Ok(w) => Some(w),
            Err(e) => {
                log::warn!(
                    "if_watch::IfWatcher::new failed: {e}; interface plug/unplug \
                     will not be detected (restart mousehop to pick up new addrs)"
                );
                None
            }
        };
        // Periodic reconciliation: enumerate live IPs and diff against
        // `listeners`. Network.framework on macOS doesn't reliably fire
        // `IfEvent::Down` when an interface is administratively
        // disabled (e.g. user toggles Wi-Fi off in System Settings),
        // leaving stale slots bound to vanished IPs that no traffic
        // can reach. Polling every 30s catches whatever if-watch
        // misses — both adds (covers missed Up events too) and drops.
        // `Skip` so a long suspend (laptop closed for hours) doesn't
        // burst-fire backlog ticks at resume.
        let mut reconcile_tick = tokio::time::interval(Duration::from_secs(30));
        reconcile_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the immediate-first tick — we just enumerated at startup
        // and don't want to thrash listeners on the first iteration.
        reconcile_tick.tick().await;
        loop {
            tokio::select! {
                _ = reconcile_tick.tick() => {
                    let current_ips: HashSet<IpAddr> =
                        enumerate_listenable_addrs().into_iter().collect();
                    let to_drop: Vec<IpAddr> = listeners
                        .keys()
                        .filter(|ip| !current_ips.contains(*ip))
                        .copied()
                        .collect();
                    for ip in to_drop {
                        // `to_drop` was just collected from
                        // `listeners.keys()` and we run single-
                        // threaded, so the remove always returns Some.
                        listeners.remove(&ip);
                        log::info!(
                            "reconcile: dropping stale listener on {ip}:{port} \
                             (IP no longer present on any interface)"
                        );
                    }
                    // `try_bind_listener` is async and may fail, so
                    // `entry().or_insert_with(...)` doesn't fit — we
                    // only want to insert on bind success. Match the
                    // `Entry::Vacant` slot up front so the same hash
                    // lookup covers both the existence check and the
                    // later insert, satisfying clippy::map_entry.
                    for ip in current_ips {
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            listeners.entry(ip)
                        {
                            match try_bind_listener(ip, port, &cfg).await {
                                Ok(l) => {
                                    let task = spawn_accept_task(
                                        l,
                                        listen_tx.clone(),
                                        conns.clone(),
                                        sessions.clone(),
                                        connection_attempts.clone(),
                                    );
                                    slot.insert(ListenerSlot { accept_task: task });
                                    log::info!(
                                        "reconcile: now listening on {ip}:{port} \
                                         (IP appeared without an Up event)"
                                    );
                                }
                                Err(e) => log::warn!(
                                    "reconcile: failed to bind on {ip}:{port}: {e}"
                                ),
                            }
                        }
                    }
                }
                ev = async {
                    match watcher.as_mut() {
                        Some(w) => w.select_next_some().await,
                        None => std::future::pending().await,
                    }
                } => match ev {
                    Ok(if_watch::IfEvent::Up(net)) => {
                        let ip = net.addr();
                        if is_listenable_addr(ip) && !listeners.contains_key(&ip) {
                            match try_bind_listener(ip, port, &cfg).await {
                                Ok(l) => {
                                    let task = spawn_accept_task(
                                        l,
                                        listen_tx.clone(),
                                        conns.clone(),
                                        sessions.clone(),
                                        connection_attempts.clone(),
                                    );
                                    listeners.insert(ip, ListenerSlot { accept_task: task });
                                    log::info!("interface up: now listening on {ip}:{port}");
                                }
                                Err(e) => log::warn!("failed to bind on {ip}:{port}: {e}"),
                            }
                        }
                    }
                    Ok(if_watch::IfEvent::Down(net)) => {
                        let ip = net.addr();
                        if listeners.remove(&ip).is_some() {
                            log::info!("interface down: stopped listening on {ip}:{port}");
                        }
                    }
                    Err(e) => log::debug!("if_watch error: {e}"),
                },
                wake = async {
                    match wake_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // `None` would mean the wake-sender was dropped;
                    // treat that as observer-gone, ignore further
                    // wakes by stripping the receiver. `Some(())` is
                    // the post-wake signal — force-close every
                    // accepted DTLS slot. Each `read_loop`'s `recv`
                    // errors out and the existing exit path removes
                    // the slot, so a peer reconnect lands on a clean
                    // accept instead of stacking on top of a stale
                    // session for up to `RECV_IDLE_TIMEOUT`.
                    match wake {
                        Some(()) => {
                            let g = conns.lock().await;
                            log::info!(
                                "supervisor: post-wake — closing {} peer conn(s) \
                                 to force fresh reconnect",
                                g.len()
                            );
                            for (a, c) in g.iter() {
                                log::debug!("post-wake close: {a}");
                                let _ = c.conn.close().await;
                            }
                        }
                        None => {
                            log::debug!(
                                "supervisor: wake channel closed; \
                                 power observer no longer signaling"
                            );
                            wake_rx = None;
                        }
                    }
                }
                p = request_port_change_rx.recv() => {
                    let new_port = p.expect("channel closed");
                    listeners.clear(); // Drop aborts each accept task
                    let mut bound = 0usize;
                    let addrs = enumerate_listenable_addrs();
                    for ip in &addrs {
                        match try_bind_listener(*ip, new_port, &cfg).await {
                            Ok(l) => {
                                let task = spawn_accept_task(
                                    l,
                                    listen_tx.clone(),
                                    conns.clone(),
                                    sessions.clone(),
                                    connection_attempts.clone(),
                                );
                                listeners.insert(*ip, ListenerSlot { accept_task: task });
                                bound += 1;
                            }
                            Err(e) => log::warn!("port change: failed to bind {ip}:{new_port}: {e}"),
                        }
                    }
                    if bound == 0 {
                        port_changed_tx
                            .send(Err(ListenerCreationError::NoBoundListener))
                            .expect("channel closed");
                    } else {
                        port = new_port;
                        port_changed_tx
                            .send(Ok(port))
                            .expect("channel closed");
                    }
                }
            }
        }
    })
}

/// Max silence on an accepted DTLS session before it's torn down.
/// DTLS rides UDP, which carries no FIN — a peer that goes silent
/// (asleep, network-partitioned, daemon killed) leaves an otherwise-
/// blocked `recv` waiting forever, and the slot in `conns` becomes
/// a zombie that nothing else evicts. The connector side sends a
/// `Ping` cycle roughly every 2s, so under healthy operation we see
/// traffic well within this window; any longer gap means the path
/// is broken in a way our reply attempts won't recover from. Set
/// generously (3–5× the connector's natural cadence) so a paused
/// peer process during e.g. a stop-the-world GC isn't mistaken for
/// a dead connection.
const RECV_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

async fn read_loop(
    conns: Rc<AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>>,
    sessions: ListenerSessions,
    addr: SocketAddr,
    session: ListenerSession,
    conn: ArcConn,
    dtls_tx: Sender<ListenEvent>,
) -> Result<(), Error> {
    // Buffer sized for the largest legal variable-length frame; fixed
    // mouse/keyboard datagrams use only the first MAX_EVENT_SIZE bytes and
    // topology remains below the clipboard cap.
    let mut b = [0u8; MAX_CLIPBOARD_SIZE];

    // Handshake gate: the peer must present a `Hello` carrying
    // `PROTOCOL_MAGIC` within `HELLO_TIMEOUT`. `hello_watchdog`
    // closes the connection if it doesn't; a `Hello` with the wrong
    // magic is refused immediately below. This is the listen-side
    // half of the deliberate hard cut-over from lan-mouse — a
    // foreign peer is rejected with a clear log instead of silently
    // half-working.
    let hello_ok = Rc::new(Cell::new(false));
    spawn_local(hello_watchdog(addr, conn.clone(), hello_ok.clone()));

    loop {
        let n = match tokio::time::timeout(RECV_IDLE_TIMEOUT, conn.recv(&mut b)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => break,
            Err(_) => {
                log::warn!(
                    "{addr}: no datagram in {RECV_IDLE_TIMEOUT:?} — closing stale connection"
                );
                let _ = conn.close().await;
                break;
            }
        };
        if n == 0 {
            continue;
        }
        if !accepted_connection_is_current(&conns, addr, session).await {
            log::debug!("ending replaced inbound DTLS read session {session} for {addr}");
            break;
        }
        let datagram = &b[..n];
        match decode_listen_datagram(datagram) {
            Some(event) if !handshake_allows_event(hello_ok.get(), &event) => {
                log::debug!("ignoring pre-Hello event from {addr}: {event}");
            }
            Some(ProtoEvent::Hello {
                magic,
                commit,
                capabilities,
            }) => {
                if magic != PROTOCOL_MAGIC {
                    log::warn!(
                        "refusing {addr}: peer presented a foreign protocol \
                         handshake (not mousehop) — closing connection"
                    );
                    break;
                }
                hello_ok.set(true);
                dtls_tx
                    .send(ListenEvent::Msg {
                        event: ProtoEvent::Hello {
                            magic,
                            commit,
                            capabilities,
                        },
                        addr,
                        session,
                    })
                    .expect("channel closed");
            }
            Some(event) => dtls_tx
                .send(ListenEvent::Msg {
                    event,
                    addr,
                    session,
                })
                .expect("channel closed"),
            None => {
                // Skip the malformed/unknown datagram and keep
                // listening. Each DTLS recv returns one full
                // datagram, so a parse error here can't desync a
                // stream; the next call gets a fresh, framed
                // message. This makes the protocol forward-
                // compatible: a peer running a newer Mousehop
                // version can introduce additional event types
                // and old peers will simply ignore them rather
                // than dropping the connection.
                log::debug!("ignoring undecodable {n}-byte event from {addr}");
            }
        }
    }
    log::info!("dtls client disconnected {addr:?}");
    let mut conns = conns.lock().await;
    // A peer can reconnect with the same UDP 4-tuple before the old
    // read loop observes its close. Remove this slot only if it still
    // belongs to our exact Arc; a stale loop must never evict the
    // replacement connection.
    if remove_connection_if_current(&mut conns, addr, session).is_none() {
        log::debug!("ignoring stale disconnect for replaced DTLS client {addr:?}");
    } else {
        sessions.remove_if_current(addr, session);
        drop(conns);
        let _ = dtls_tx.send(ListenEvent::Disconnected { addr, session });
    }
    Ok(())
}

async fn accepted_connection_is_current(
    conns: &AsyncMutex<HashMap<SocketAddr, AcceptedConnection>>,
    addr: SocketAddr,
    session: ListenerSession,
) -> bool {
    conns
        .lock()
        .await
        .get(&addr)
        .is_some_and(|slot| slot.session == session)
}

fn remove_connection_if_current(
    conns: &mut HashMap<SocketAddr, AcceptedConnection>,
    addr: SocketAddr,
    session: ListenerSession,
) -> Option<AcceptedConnection> {
    conns
        .get(&addr)
        .is_some_and(|current| current.session == session)
        .then(|| conns.remove(&addr))
        .flatten()
}

/// Connection-establishment deadline. A peer that has not presented
/// a `PROTOCOL_MAGIC`-stamped `Hello` within this window is not a
/// mousehop instance and its connection is closed. The connect side
/// retransmits its `Hello` across a shorter span, so a genuine
/// mousehop peer always flips `hello_ok` well before this fires.
const HELLO_TIMEOUT: Duration = Duration::from_secs(12);

/// Close `conn` if `hello_ok` is still unset after [`HELLO_TIMEOUT`].
/// Reaching the close path means the peer never spoke a valid
/// mousehop handshake — the listen-side enforcement of the hard
/// cut-over from lan-mouse.
async fn hello_watchdog(addr: SocketAddr, conn: ArcConn, hello_ok: Rc<Cell<bool>>) {
    tokio::time::sleep(HELLO_TIMEOUT).await;
    if !hello_ok.get() {
        log::warn!(
            "refusing {addr}: peer did not complete the mousehop handshake \
             within {HELLO_TIMEOUT:?} — closing connection"
        );
        let _ = conn.close().await;
    }
}

/// Classify a DTLS datagram by its first-byte event-type tag and
/// route through a variable-length codec or the fixed-buffer `try_into` path.
/// Mirrors `decode_proto_datagram` on the connect side so both directions
/// accept the same wire formats.
fn decode_listen_datagram(bytes: &[u8]) -> Option<ProtoEvent> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    async fn test_conn() -> ArcConn {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("test socket"))
    }

    #[tokio::test]
    async fn stale_same_address_read_loop_cannot_remove_replacement() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let replacement = test_conn().await;
        let old_session = 1;
        let replacement_session = 2;
        let mut conns = HashMap::from([(
            addr,
            AcceptedConnection {
                session: replacement_session,
                conn: replacement.clone(),
            },
        )]);

        assert!(remove_connection_if_current(&mut conns, addr, old_session).is_none());
        assert!(Arc::ptr_eq(
            &conns.get(&addr).expect("replacement remains").conn,
            &replacement,
        ));
    }

    #[tokio::test]
    async fn current_read_loop_removes_only_its_connection() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let current = test_conn().await;
        let session = 7;
        let mut conns = HashMap::from([(
            addr,
            AcceptedConnection {
                session,
                conn: current.clone(),
            },
        )]);

        let removed = remove_connection_if_current(&mut conns, addr, session)
            .expect("current connection removed");
        assert_eq!(removed.session, session);
        assert!(Arc::ptr_eq(&removed.conn, &current));
        assert!(!conns.contains_key(&addr));
    }

    #[test]
    fn queued_old_session_is_filtered_after_same_address_accept() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let sessions = ListenerSessions::default();
        let old = sessions.allocate(addr);
        let replacement = sessions.allocate(addr);

        assert!(!sessions.is_current(addr, old));
        assert!(sessions.is_current(addr, replacement));
    }

    #[test]
    fn terminal_event_is_dropped_if_same_address_was_replaced() {
        let addr: SocketAddr = "127.0.0.1:4242".parse().expect("test address");
        let sessions = ListenerSessions::default();
        let ended = sessions.allocate(addr);
        assert!(sessions.remove_if_current(addr, ended));
        let disconnected = ListenEvent::Disconnected {
            addr,
            session: ended,
        };
        assert!(listen_event_is_current(&sessions, &disconnected));

        let _replacement = sessions.allocate(addr);
        assert!(!listen_event_is_current(&sessions, &disconnected));
    }
}
