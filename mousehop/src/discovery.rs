//! Bonjour / mDNS-SD service registration + discovery.
//!
//! Why this exists: when a peer machine has multiple interfaces on
//! the same subnet (Mac with Wi-Fi + Ethernet, Linux laptop with
//! Wi-Fi + USB-C dock, etc.), plain hostname resolution returns
//! every interface's IP and the dialer has no way to tell which one
//! the OS would *prefer* for outbound traffic. The current connect
//! path races them and uses whichever DTLS handshake completes first,
//! which is RTT-roughly-correct but not always what the user wanted
//! — Wi-Fi can win the race even when the user has Ethernet ranked
//! higher in macOS's service order.
//!
//! Each mousehop instance registers a `_mousehop._udp.local.`
//! Bonjour service whose TXT record advertises `primary=<ip>`, where
//! `<ip>` is the IPv4 of the interface that owns the default route
//! (which on macOS reflects service order). The dialer browses the
//! same service type, looks up the peer instance by hostname, and
//! prepends the primary IP to its connection-attempt list. If the
//! peer is on an old version with no advertised service (or mDNS
//! is firewalled), nothing breaks — we silently fall through to the
//! existing `connect_any` race.
//!
//! The whole subsystem is gated by the `mdns_discovery` config flag
//! (default true). Toggling it off shuts down the mDNS daemon and
//! all browse/registration state — useful on networks where mDNS
//! multicast (224.0.0.251) is firewalled.

use std::{
    cell::RefCell,
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    rc::Rc,
};

use local_channel::mpsc::Sender;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use mousehop_ipc::IfaceKind;
use tokio::task::{JoinHandle, spawn_local};

use crate::network::{default_interface_kind, is_routable_ip, local_addresses_with_kind};

const SERVICE_TYPE: &str = "_mousehop._udp.local.";
const TXT_PRIMARY_KEY: &str = "primary";
/// TXT keys advertising per-address interface kind, one per address:
/// `if_<ip> = wired|wifi|other`. Per-address (rather than one packed
/// value) so no single TXT string exceeds the 255-byte RFC 6763 cap.
const TXT_IFACE_PREFIX: &str = "if_";

/// A peer's mDNS service resolved. Carries everything the daemon needs
/// to fold the peer's interfaces into a client's candidate set and
/// label them — emitted to the service over a channel so it can match
/// the instance to a configured client and broadcast to the GUI.
pub(crate) enum DiscoveryEvent {
    Resolved {
        /// Normalized instance label, matched against a client's
        /// configured hostname (see [`normalize_mdns_name`]).
        instance: String,
        /// Every routable address the peer advertises (all its
        /// interfaces), so a dual-homed peer surfaces all of them
        /// without the user pinning anything.
        addresses: Vec<IpAddr>,
        /// Per-address interface kind from the peer's TXT record.
        interfaces: HashMap<IpAddr, IfaceKind>,
    },
}

fn iface_kind_str(kind: IfaceKind) -> &'static str {
    match kind {
        IfaceKind::Wired => "wired",
        IfaceKind::WiFi => "wifi",
        IfaceKind::Other => "other",
    }
}

fn parse_iface_kind(s: &str) -> Option<IfaceKind> {
    match s {
        "wired" => Some(IfaceKind::Wired),
        "wifi" => Some(IfaceKind::WiFi),
        "other" => Some(IfaceKind::Other),
        _ => None,
    }
}

/// Cross-platform: IP of the interface that owns the default route.
///
/// On macOS the default route reflects the user's service-order
/// ranking — that's exactly the "primary" the user expects when they
/// say "use Ethernet, not Wi-Fi". On Linux it reflects the lowest-
/// metric default route. On Windows it's whatever
/// `GetBestRoute2` selects.
fn primary_ipv4() -> Option<Ipv4Addr> {
    let iface = netdev::get_default_interface().ok()?;
    iface.ipv4.first().map(|net| net.addr())
}

fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "mousehop".to_string())
}

/// Strip a single trailing dot if present. Bonjour hostnames are
/// stored as fully-qualified ("foo.local."); user config typically
/// writes them without the trailing dot ("foo.local"). Normalize
/// to compare.
fn strip_trailing_dot(s: &str) -> &str {
    s.strip_suffix('.').unwrap_or(s)
}

/// Pull the service-instance label off a Bonjour fullname.
///
/// `mdns-sd` returns fullnames as `"<instance>.<service-type>"` where
/// `<service-type>` is e.g. `"_mousehop._udp.local."`. The instance
/// label is the user-visible identifier the announcer chose for itself
/// — typically the system hostname, and the same string the user puts
/// in their mousehop config's `hostname = "..."`. We key
/// [`PrimaryCache`] on this instead of the SRV target so the dialer
/// matches the config hostname even when the announcer's SRV target
/// has macOS-style suffixes (`Foo.local` vs `Foo-2.local`) or other
/// drift.
fn instance_from_fullname<'a>(fullname: &'a str, service_type: &str) -> &'a str {
    let suffix = format!(".{service_type}");
    fullname.strip_suffix(&suffix).unwrap_or(fullname)
}

/// Strip a Bonjour conflict suffix — the ` (2)`, ` (3)`, … that
/// mDNS-SD/Bonjour append to an instance label when the chosen name
/// collides with another advertiser on the network (most commonly a
/// stale registration left behind by an unclean restart of our own
/// daemon). Without this, a peer that re-announces as `omarchy (2)`
/// would stop matching a client configured as `omarchy`, silently
/// dropping discovery aggregation + interface labels for it.
fn strip_conflict_suffix(s: &str) -> &str {
    let trimmed = s.trim_end();
    if let Some(open) = trimmed.rfind(" (") {
        if let Some(num) = trimmed[open + 2..].strip_suffix(')') {
            if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
                return trimmed[..open].trim_end();
            }
        }
    }
    s
}

/// Canonicalize a Bonjour/mDNS-SD name for cache lookup. Lower-cases,
/// drops a trailing FQDN dot, the `.local` link-local suffix, and any
/// Bonjour ` (N)` conflict suffix. The `.local` domain is implied for
/// everything mDNS-SD touches, so callers shouldn't have to remember
/// whether to include it — config that says `Foo.local`, an
/// announcer's instance label `Foo`, an SRV target `foo.local.`, and a
/// conflict-renamed `Foo (2)` all collapse to `foo`.
pub(crate) fn normalize_mdns_name(s: &str) -> String {
    let s = strip_trailing_dot(s);
    let s = s.strip_suffix(".local").unwrap_or(s);
    let s = strip_conflict_suffix(s);
    s.to_ascii_lowercase()
}

/// Shared `peer_hostname -> primary_ipv4` map, populated by Discovery
/// and read by the dialer (`connect_to_handle`). Owned by the dialer
/// path so its references survive across discovery enable/disable
/// cycles — when the user toggles discovery off, the daemon stops
/// publishing/browsing but cached hints stay queryable. A subsequent
/// re-enable populates fresh entries into the same map.
pub(crate) type PrimaryCache = Rc<RefCell<HashMap<String, IpAddr>>>;

pub(crate) struct Discovery {
    /// The mDNS daemon. `None` when the subsystem is disabled (config
    /// toggle off, or daemon failed to start). All public methods are
    /// no-ops when this is None.
    daemon: Option<ServiceDaemon>,
    /// Fullname of our registered service, kept so we can unregister
    /// on shutdown / before re-registering.
    registered_fullname: Option<String>,
    /// Shared cache (see [`PrimaryCache`]).
    primary_cache: PrimaryCache,
    /// Background task that consumes browse events and updates
    /// `primary_cache`. Aborted when discovery is disabled or torn
    /// down.
    browse_task: Option<JoinHandle<()>>,
    /// Port the dialer should connect to (advertised in the SRV
    /// record's port field). Tracked so we can re-register when the
    /// listen port changes.
    port: u16,
    /// Sink for [`DiscoveryEvent`]s, drained by the service. Held so a
    /// re-enable (`set_enabled`) hands the fresh browse task a clone.
    event_tx: Sender<DiscoveryEvent>,
}

impl Discovery {
    /// Construct a Discovery sharing `primary_cache` with the dialer.
    /// If `enabled` is false, returns an inert instance — calling any
    /// method on it is a no-op. Same outcome when the mDNS daemon
    /// fails to start (e.g. multicast group already joined by some
    /// other process, or the OS lacks the permissions). In both
    /// cases we log a warning and continue without discovery; the
    /// dialer falls back to plain hostname resolution.
    pub(crate) fn new(
        port: u16,
        enabled: bool,
        primary_cache: PrimaryCache,
        event_tx: Sender<DiscoveryEvent>,
    ) -> Self {
        if !enabled {
            log::info!("mdns discovery disabled by config");
            return Self::inert(port, primary_cache, event_tx);
        }
        match ServiceDaemon::new() {
            Ok(daemon) => {
                let browse_task = start_browse(&daemon, primary_cache.clone(), event_tx.clone());
                let mut this = Self {
                    daemon: Some(daemon),
                    registered_fullname: None,
                    primary_cache,
                    browse_task,
                    port,
                    event_tx,
                };
                this.register();
                this
            }
            Err(e) => {
                log::warn!("mdns ServiceDaemon::new failed: {e}; discovery disabled");
                Self::inert(port, primary_cache, event_tx)
            }
        }
    }

    fn inert(port: u16, primary_cache: PrimaryCache, event_tx: Sender<DiscoveryEvent>) -> Self {
        Self {
            daemon: None,
            registered_fullname: None,
            primary_cache,
            browse_task: None,
            port,
            event_tx,
        }
    }

    /// Register `_mousehop._udp.local.` with our hostname + primary
    /// IP. Called on construction and again whenever the primary IP
    /// or port may have changed.
    fn register(&mut self) {
        let Some(daemon) = self.daemon.as_ref() else {
            return;
        };
        // Drop the old registration first so we don't leave stale
        // TXT records floating on the network.
        if let Some(old) = self.registered_fullname.take() {
            let _ = daemon.unregister(&old);
        }
        let host = local_hostname();
        let host_record = format!("{host}.local.");
        let primary = match primary_ipv4() {
            Some(ip) => ip,
            None => {
                log::warn!(
                    "mdns: no default-route interface; skipping registration (will retry on \
                     interface change)"
                );
                return;
            }
        };
        // Advertise *every* physical-interface address (so a dual-homed
        // peer surfaces all of them via discovery, no hand-pinning),
        // each tagged with its interface kind in a `if_<ip>` TXT key.
        let (mut addresses, mut kinds) = local_addresses_with_kind();
        if !addresses.contains(&IpAddr::V4(primary)) {
            addresses.push(IpAddr::V4(primary));
        }
        // The primary may live on an interface `local_addresses_with_kind`
        // filtered out (e.g. a VPN tunnel routing the default), so it
        // wouldn't otherwise get an `if_<ip>` label. Tag it with the
        // default interface's kind (Other if unknown) so every
        // advertised address is labeled.
        kinds
            .entry(IpAddr::V4(primary))
            .or_insert_with(|| default_interface_kind().unwrap_or(IfaceKind::Other));
        let mut props = HashMap::new();
        props.insert(TXT_PRIMARY_KEY.to_string(), primary.to_string());
        for (ip, kind) in &kinds {
            props.insert(
                format!("{TXT_IFACE_PREFIX}{ip}"),
                iface_kind_str(*kind).to_string(),
            );
        }
        let info = match ServiceInfo::new(
            SERVICE_TYPE,
            &host,
            &host_record,
            addresses.as_slice(),
            self.port,
            Some(props),
        ) {
            Ok(i) => i,
            Err(e) => {
                log::warn!("mdns ServiceInfo::new failed: {e}; skipping registration");
                return;
            }
        };
        let fullname = info.get_fullname().to_string();
        match daemon.register(info) {
            Ok(()) => {
                log::info!(
                    "mdns: registered {fullname} on {addresses:?}:{port} (primary {primary})",
                    port = self.port,
                );
                self.registered_fullname = Some(fullname);
            }
            Err(e) => log::warn!("mdns register failed: {e}"),
        }
    }

    /// Re-register with the current primary IP. Called periodically
    /// by the service's main loop so the TXT record reflects the
    /// active default-route interface even when interface changes
    /// don't arrive through if-watch.
    pub(crate) fn refresh(&mut self) {
        if self.daemon.is_some() {
            self.register();
        }
    }

    /// Re-register with a new port (config changed).
    pub(crate) fn set_port(&mut self, port: u16) {
        if self.port == port {
            return;
        }
        self.port = port;
        self.refresh();
    }

    /// Toggle the subsystem on/off. Off → unregister, abort browse,
    /// drop daemon. On → spin up afresh, reusing the same shared
    /// cache so any prior hints stay queryable until overwritten.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        let currently = self.daemon.is_some();
        if currently == enabled {
            return;
        }
        if enabled {
            *self = Self::new(
                self.port,
                true,
                self.primary_cache.clone(),
                self.event_tx.clone(),
            );
        } else {
            self.shutdown();
        }
    }

    fn shutdown(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            if let Some(name) = self.registered_fullname.take() {
                let _ = daemon.unregister(&name);
            }
            let _ = daemon.shutdown();
        }
        if let Some(task) = self.browse_task.take() {
            task.abort();
        }
        // Don't clear primary_cache: the dialer may still benefit
        // from the last-known hints, and a re-enable would otherwise
        // lose them until each peer's next announcement.
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn a background task that browses `_mousehop._udp.local.` and
/// keeps `primary_cache` updated as ServiceResolved / ServiceRemoved
/// events arrive.
fn start_browse(
    daemon: &ServiceDaemon,
    primary_cache: Rc<RefCell<HashMap<String, IpAddr>>>,
    event_tx: Sender<DiscoveryEvent>,
) -> Option<JoinHandle<()>> {
    let receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("mdns browse failed: {e}");
            return None;
        }
    };
    Some(spawn_local(async move {
        while let Ok(event) = receiver.recv_async().await {
            match event {
                ServiceEvent::ServiceResolved(resolved) => {
                    let instance = instance_from_fullname(resolved.get_fullname(), SERVICE_TYPE);
                    let key = normalize_mdns_name(instance);
                    let target = strip_trailing_dot(resolved.get_hostname());

                    // primary= biases the dialer's head-start (kept in
                    // the shared cache the dialer reads directly).
                    let primary = resolved
                        .get_property_val_str(TXT_PRIMARY_KEY)
                        .and_then(|s| s.parse::<IpAddr>().ok());
                    if let Some(ip) = primary {
                        primary_cache.borrow_mut().insert(key.clone(), ip);
                    }

                    // All advertised, routable addresses become
                    // candidates (drops fe80 link-local etc).
                    let addresses: Vec<IpAddr> = resolved
                        .get_addresses()
                        .iter()
                        .map(|scoped| scoped.to_ip_addr())
                        .filter(is_routable_ip)
                        .collect();

                    // Per-address interface kinds from `if_<ip>` keys.
                    let mut interfaces = HashMap::new();
                    for prop in resolved.txt_properties.iter() {
                        if let Some(ip_str) = prop.key().strip_prefix(TXT_IFACE_PREFIX) {
                            if let (Ok(ip), Some(kind)) =
                                (ip_str.parse::<IpAddr>(), parse_iface_kind(prop.val_str()))
                            {
                                interfaces.insert(ip, kind);
                            }
                        }
                    }

                    log::info!(
                        "mdns: peer instance={key} (target={target}) primary={primary:?} \
                         addrs={addresses:?} (port={port})",
                        port = resolved.get_port(),
                    );
                    let _ = event_tx.send(DiscoveryEvent::Resolved {
                        instance: key,
                        addresses,
                        interfaces,
                    });
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    // Best-effort: the fullname is "<instance>._lan-
                    // mouse._udp.local." — we don't have the host
                    // record handy here, so drop on next browse-
                    // resolved instead of trying to map back. Keeps
                    // the cache slightly stale on goodbye but never
                    // wrong: if the peer comes back with a different
                    // primary, the next ServiceResolved overwrites.
                    log::debug!("mdns: service removed {fullname}");
                }
                _ => {}
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_variants_to_same_key() {
        let base = normalize_mdns_name("omarchy");
        assert_eq!(normalize_mdns_name("Omarchy.local"), base);
        assert_eq!(normalize_mdns_name("omarchy.local."), base);
        // Bonjour conflict-rename still matches the configured name.
        assert_eq!(normalize_mdns_name("omarchy (2)"), base);
        assert_eq!(normalize_mdns_name("Omarchy (13).local"), base);
        // A parenthetical that isn't a numeric conflict suffix is kept.
        assert_ne!(normalize_mdns_name("omarchy (beta)"), base);
    }
}
