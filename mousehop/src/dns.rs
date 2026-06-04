use std::{collections::HashMap, io, net::IpAddr};

use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::net::lookup_host;
use tokio::task::{JoinHandle, spawn_local};

use tokio_util::sync::CancellationToken;

use mousehop_ipc::ClientHandle;

pub(crate) struct DnsResolver {
    cancellation_token: CancellationToken,
    task: Option<JoinHandle<()>>,
    request_tx: Sender<DnsRequest>,
    event_rx: Receiver<DnsEvent>,
}

struct DnsRequest {
    handle: ClientHandle,
    hostname: String,
}

pub(crate) enum DnsEvent {
    Resolving(ClientHandle),
    Resolved(ClientHandle, String, io::Result<Vec<IpAddr>>),
}

struct DnsTask {
    request_rx: Receiver<DnsRequest>,
    event_tx: Sender<DnsEvent>,
    cancellation_token: CancellationToken,
    active_tasks: HashMap<ClientHandle, JoinHandle<()>>,
}

impl DnsResolver {
    pub(crate) fn new() -> io::Result<Self> {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let dns_task = DnsTask {
            active_tasks: Default::default(),
            request_rx,
            event_tx,
            cancellation_token: cancellation_token.clone(),
        };
        let task = Some(spawn_local(dns_task.run()));
        Ok(Self {
            cancellation_token,
            task,
            event_rx,
            request_tx,
        })
    }

    pub(crate) fn resolve(&self, handle: ClientHandle, hostname: String) {
        let request = DnsRequest { handle, hostname };
        self.request_tx.send(request).expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> DnsEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        self.task.take().expect("task").await.expect("join error");
    }
}

impl DnsTask {
    async fn run(mut self) {
        let cancellation_token = self.cancellation_token.clone();
        tokio::select! {
            _ = self.do_dns() => {},
            _ = cancellation_token.cancelled() => {},
        }
    }

    async fn do_dns(&mut self) {
        while let Some(dns_request) = self.request_rx.recv().await {
            let DnsRequest { handle, hostname } = dns_request;

            /* abort previous dns task */
            let previous_task = self.active_tasks.remove(&handle);
            if let Some(task) = previous_task {
                if !task.is_finished() {
                    task.abort();
                }
            }

            self.event_tx
                .send(DnsEvent::Resolving(handle))
                .expect("channel closed");

            /* spawn task for dns request */
            let event_tx = self.event_tx.clone();
            let cancellation_token = self.cancellation_token.clone();

            let task = tokio::task::spawn_local(async move {
                tokio::select! {
                    result = resolve_hostname(&hostname) => {
                       event_tx
                           .send(DnsEvent::Resolved(handle, hostname, result))
                           .expect("channel closed");
                    }
                    _ = cancellation_token.cancelled() => {},
                }
            });
            self.active_tasks.insert(handle, task);
        }
    }
}

/// Resolve `hostname` via the operating system's full name-resolution
/// stack (`getaddrinfo` on Unix, GetAddrInfoEx on Windows). This walks
/// `/etc/nsswitch.conf` on Linux — picking up mDNS via Avahi, /etc/hosts,
/// and DNS — and uses Bonjour for `.local` names on macOS. Pure-DNS
/// resolvers like hickory miss all of those, which is why a Bonjour
/// hostname (e.g. `JKMBP-M4-Max.local`) wouldn't resolve before.
///
/// Port `0` is a placeholder — `lookup_host` requires `host:port` but we
/// only care about the IPs at this stage; the actual port is appended at
/// connection time.
async fn resolve_hostname(hostname: &str) -> io::Result<Vec<IpAddr>> {
    let addrs = lookup_host((hostname, 0)).await?;
    Ok(addrs
        .map(|sa| sa.ip())
        .filter(|ip| !is_unusable_candidate(ip))
        .collect())
}

/// IPv6 link-local addresses (`fe80::/10`) can't be dialed without a
/// scope/zone id (the `%interface` suffix), which neither the resolver
/// nor mousehop's config carries — so they are never usable connection
/// candidates. Drop them at resolution time so they don't pollute the
/// candidate set, get probed (always "unreachable"), join the connect
/// race, or clutter the GUI's address picker.
fn is_unusable_candidate(ip: &IpAddr) -> bool {
    match ip {
        // top 10 bits == 1111111010 -> fe80::/10
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
        IpAddr::V4(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn link_local_v6_is_filtered_but_routable_addrs_kept() {
        // fe80::/10 link-local -> unusable
        assert!(is_unusable_candidate(&IpAddr::V6(
            "fe80::49e:d8c6:6411:f9de".parse::<Ipv6Addr>().unwrap()
        )));
        // a routable ULA (fd00::/8) stays a candidate
        assert!(!is_unusable_candidate(&IpAddr::V6(
            "fdb7:7db7:2cc4:4c2d::1".parse::<Ipv6Addr>().unwrap()
        )));
        // ordinary IPv4 stays a candidate
        assert!(!is_unusable_candidate(&IpAddr::V4(Ipv4Addr::new(
            192, 168, 1, 153
        ))));
    }
}
