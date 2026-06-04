//! Background per-address latency prober.
//!
//! A multi-homed peer (e.g. a laptop with both a wired and a Wi-Fi
//! address on the same subnet) exposes several candidate IPs. To let
//! the user pick *which* interface to lock, the GUI shows the measured
//! round-trip latency next to each address — including the ones we are
//! **not** currently connected on. Those inactive candidates have no
//! live connection to read an RTT from, so this module actively probes
//! every candidate on a slow cadence.
//!
//! ## Why a TCP connect probe
//!
//! mousehop's data path is DTLS-over-UDP and it does not run a TCP
//! listener, so a probe connect to `(ip, port)` normally elicits an
//! immediate TCP RST ("connection refused") from a reachable peer. The
//! refusal round-trip still measures the layer-3 path latency — which
//! is exactly the signal the user needs to tell a ~0.8 ms wired link
//! from a ~70 ms Wi-Fi one — without privileges (no raw ICMP) and
//! without disturbing the live DTLS session. A host that is down or
//! firewalled yields a timeout, surfaced as "unreachable".
//!
//! Structure mirrors [`crate::dns::DnsResolver`]: a `spawn_local` task
//! owns the cadence and ships [`ProbeResult`]s back over a
//! single-threaded channel that [`crate::service`] selects on.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::{
    net::TcpStream,
    task::{JoinHandle, spawn_local},
    time,
};
use tokio_util::sync::CancellationToken;

use mousehop_ipc::ClientHandle;

use crate::client::ClientManager;

/// How often every candidate address is re-probed. Deliberately slow:
/// latency is a human-facing decision aid, not a control loop, and a
/// tighter cadence would burn ephemeral ports / TCP RSTs for no real
/// benefit. One round of probes is a handful of connects per peer.
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Per-probe hard deadline. A candidate that does not answer (down,
/// firewalled, or the cable just got unplugged) resolves to
/// "unreachable" within this window rather than hanging the sample.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// One latency sample for a single candidate address of a client.
/// `rtt_micros` is `Some(us)` when the address answered (TCP accept or
/// refusal) and `None` when the probe timed out / the host was
/// unreachable.
pub(crate) struct ProbeResult {
    pub handle: ClientHandle,
    pub ip: IpAddr,
    pub rtt_micros: Option<u32>,
}

/// Owns the background probe task and the receiving end of its result
/// stream. Dropped/torn down on service shutdown via [`Self::terminate`].
pub(crate) struct LatencyProber {
    cancellation_token: CancellationToken,
    task: Option<JoinHandle<()>>,
    event_rx: Receiver<ProbeResult>,
}

impl LatencyProber {
    /// Spawn the prober. It pulls its targets straight from
    /// `client_manager` (a cheap `Rc` clone) on every tick, so callers
    /// never have to push the candidate set in — new clients, DNS
    /// updates and lock changes are all picked up automatically on the
    /// next cycle.
    pub(crate) fn new(client_manager: ClientManager) -> Self {
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let task = LatencyTask {
            client_manager,
            event_tx,
            cancellation_token: cancellation_token.clone(),
        };
        Self {
            cancellation_token,
            task: Some(spawn_local(task.run())),
            event_rx,
        }
    }

    /// Await the next probe result. Used as a `tokio::select!` arm in
    /// the service loop.
    pub(crate) async fn event(&mut self) -> ProbeResult {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

struct LatencyTask {
    client_manager: ClientManager,
    event_tx: Sender<ProbeResult>,
    cancellation_token: CancellationToken,
}

impl LatencyTask {
    async fn run(self) {
        let cancellation_token = self.cancellation_token.clone();
        tokio::select! {
            _ = self.probe_loop() => {},
            _ = cancellation_token.cancelled() => {},
        }
    }

    async fn probe_loop(&self) {
        let mut tick = time::interval(PROBE_INTERVAL);
        // A long suspend shouldn't replay a burst of missed ticks.
        tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // Snapshot targets into owned data; never hold the manager
            // borrow across the probe `await`s below.
            let targets = self.client_manager.probe_targets();
            for (handle, port, ips) in targets {
                for ip in ips {
                    let tx = self.event_tx.clone();
                    // One task per address so a timing-out candidate
                    // can't delay the others or the next cadence tick.
                    spawn_local(async move {
                        let rtt_micros = probe_addr(SocketAddr::new(ip, port)).await;
                        // Receiver lives for the whole service; a send
                        // error only means we're shutting down.
                        let _ = tx.send(ProbeResult {
                            handle,
                            ip,
                            rtt_micros,
                        });
                    });
                }
            }
        }
    }
}

/// Time a single TCP connect attempt. A successful connect *or* a
/// `ConnectionRefused` both prove the host is reachable and yield a
/// layer-3 round-trip; anything else (timeout, host/net unreachable)
/// is reported as `None`.
async fn probe_addr(addr: SocketAddr) -> Option<u32> {
    let start = Instant::now();
    match time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => Some(elapsed_micros(start)),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            Some(elapsed_micros(start))
        }
        Ok(Err(_)) | Err(_) => None,
    }
}

fn elapsed_micros(start: Instant) -> u32 {
    start.elapsed().as_micros().min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // Windows Defender Firewall on hosted runners silently drops SYN
    // to port 1 instead of returning RST, so the probe times out and
    // the assertion would mis-fire. The production matching of
    // `ConnectionRefused` still works on Windows.
    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn refused_connection_counts_as_reachable() {
        // Port 1 on loopback is effectively always closed, so the
        // kernel answers our SYN with an immediate RST. That refusal
        // still proves the host is reachable and yields a measurable
        // round-trip — it must NOT be reported as unreachable
        // (`None`), which is what distinguishes this probe from a real
        // timeout.
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 1));
        assert!(
            probe_addr(addr).await.is_some(),
            "a refused (RST) connection should be treated as reachable"
        );
    }

    #[tokio::test]
    async fn unroutable_address_times_out_as_unreachable() {
        // 192.0.2.0/24 (TEST-NET-1, RFC 5737) is reserved and never
        // routed, so the probe must hit its timeout and report `None`.
        let addr = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 4252));
        assert!(
            probe_addr(addr).await.is_none(),
            "an unroutable address should be reported as unreachable"
        );
    }
}
