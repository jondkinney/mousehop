mod imp;

use adw::subclass::prelude::*;
use gtk::glib::{self, Object};

use mousehop_ipc::{ClientConfig, ClientHandle, ClientState, ConnectionMode, IfaceKind};

glib::wrapper! {
    pub struct ClientObject(ObjectSubclass<imp::ClientObject>);
}

impl ClientObject {
    pub fn new(handle: ClientHandle, client: ClientConfig, state: ClientState) -> Self {
        let obj: Self = Object::builder()
            .property("handle", handle)
            .property("hostname", client.hostname)
            .property("port", client.port as u32)
            .property("position", client.pos.to_string())
            .property("active", state.active)
            .property(
                "ips",
                state
                    .ips
                    .iter()
                    .map(|ip| ip.to_string())
                    .collect::<Vec<_>>(),
            )
            .property("resolving", state.resolving)
            .property("peer-commit", peer_commit_to_string(state.peer_commit))
            .property("clipboard-send", client.clipboard_send)
            .build();
        // The candidate list, mode and active lock aren't GObject
        // properties (the row renders them imperatively into a
        // dropdown), so seed them after construction.
        obj.set_addresses(address_entries(&state));
        obj.set_mode(client.mode);
        obj.set_active_lock(state.active_lock.map(|ip| ip.to_string()));
        obj
    }

    pub fn get_data(&self) -> ClientData {
        self.imp().data.borrow().clone()
    }

    /// Replace the cached candidate-address list (IP + latency + active
    /// flag + interface kind). Pushed from `Window::update_client_state`
    /// and rendered into the address dropdown by `ClientRow`.
    pub fn set_addresses(&self, addresses: Vec<AddrEntry>) {
        self.imp().data.borrow_mut().addresses = addresses;
    }

    pub fn addresses(&self) -> Vec<AddrEntry> {
        self.imp().data.borrow().addresses.clone()
    }

    /// Base connection policy (Auto / Fastest). Pushed from
    /// `Window::update_client_config`.
    pub fn set_mode(&self, mode: ConnectionMode) {
        self.imp().data.borrow_mut().mode = mode;
    }

    pub fn mode(&self) -> ConnectionMode {
        self.imp().data.borrow().mode
    }

    /// The address pinned on the *current* network (resolved by the
    /// daemon), or `None`. Pushed from `Window::update_client_state`.
    pub fn set_active_lock(&self, locked: Option<String>) {
        self.imp().data.borrow_mut().active_lock = locked;
    }

    pub fn active_lock(&self) -> Option<String> {
        self.imp().data.borrow().active_lock.clone()
    }
}

/// One candidate address of a peer, as shown in the address selector.
#[derive(Default, Clone)]
pub struct AddrEntry {
    pub ip: String,
    pub latency: LatencyState,
    /// True for the address the live connection currently runs on.
    pub active: bool,
    /// Interface kind advertised by the peer, if known.
    pub kind: Option<IfaceKind>,
}

/// Per-address latency as surfaced to the user.
#[derive(Default, Clone, PartialEq, Debug)]
pub enum LatencyState {
    /// Not probed yet (e.g. a freshly-resolved address).
    #[default]
    Unknown,
    /// The probe couldn't measure a round-trip. Shown as a neutral
    /// dash rather than "unreachable": a TCP-connect probe can't tell
    /// a down host from one whose firewall drops TCP while mousehop's
    /// UDP/DTLS path still works — so we don't claim unreachability we
    /// can't prove. (The *active* address never lands here; its RTT
    /// comes from the live connection's ping/pong.)
    Unmeasured,
    /// Measured round-trip, in microseconds.
    Rtt(u32),
}

/// Build the sorted candidate list from a [`ClientState`], joining its
/// IP set with the latency map and flagging the active address.
pub fn address_entries(state: &ClientState) -> Vec<AddrEntry> {
    let active_ip = state.active_addr.map(|a| a.ip());
    let mut entries: Vec<AddrEntry> = state
        .ips
        .iter()
        .map(|ip| AddrEntry {
            ip: ip.to_string(),
            latency: match state.latencies.get(ip) {
                None => LatencyState::Unknown,
                Some(None) => LatencyState::Unmeasured,
                Some(Some(us)) => LatencyState::Rtt(*us),
            },
            active: Some(*ip) == active_ip,
            kind: state.interfaces.get(ip).copied(),
        })
        .collect();
    // Stable, address-sorted order so the dropdown doesn't reshuffle
    // between updates (HashSet iteration order is nondeterministic).
    entries.sort_by(|a, b| a.ip.cmp(&b.ip));
    entries
}

/// Short interface-kind label for the picker, e.g. `Wired` / `Wi-Fi`.
/// `None` when the peer didn't advertise a kind (older build / mDNS
/// off) so we just omit it rather than guess.
pub fn iface_label(kind: Option<IfaceKind>) -> Option<&'static str> {
    match kind {
        Some(IfaceKind::Wired) => Some("Wired"),
        Some(IfaceKind::WiFi) => Some("Wi-Fi"),
        Some(IfaceKind::Other) => Some("Other"),
        None => None,
    }
}

/// Human-readable latency label, e.g. `0.8 ms`, `74 ms`,
/// `unreachable`, or `…` when not yet probed.
pub fn format_latency(latency: &LatencyState) -> String {
    match latency {
        LatencyState::Unknown => "…".to_string(),
        LatencyState::Unmeasured => "—".to_string(),
        LatencyState::Rtt(us) => {
            let ms = *us as f64 / 1000.0;
            if ms < 10.0 {
                format!("{ms:.1} ms")
            } else {
                format!("{ms:.0} ms")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mousehop_ipc::ClientState;
    use std::collections::{HashMap, HashSet};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn ip(a: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, a))
    }

    #[test]
    fn format_latency_scales_units_and_states() {
        assert_eq!(format_latency(&LatencyState::Unknown), "…");
        assert_eq!(format_latency(&LatencyState::Unmeasured), "—");
        assert_eq!(format_latency(&LatencyState::Rtt(800)), "0.8 ms");
        assert_eq!(format_latency(&LatencyState::Rtt(5_300)), "5.3 ms");
        assert_eq!(format_latency(&LatencyState::Rtt(74_000)), "74 ms");
    }

    #[test]
    fn address_entries_are_sorted_with_latency_and_active_flag() {
        let mut latencies = HashMap::new();
        latencies.insert(ip(20), Some(800u32));
        latencies.insert(ip(30), None); // probed but unreachable
        // ip(10) is intentionally absent from `latencies` → Unknown.
        let mut interfaces = HashMap::new();
        interfaces.insert(ip(20), IfaceKind::Wired);
        interfaces.insert(ip(30), IfaceKind::WiFi);
        let state = ClientState {
            ips: HashSet::from([ip(20), ip(10), ip(30)]),
            latencies,
            interfaces,
            active_addr: Some(SocketAddr::new(ip(20), 4252)),
            ..Default::default()
        };
        let entries = address_entries(&state);
        assert_eq!(
            entries.iter().map(|e| e.ip.as_str()).collect::<Vec<_>>(),
            vec!["192.168.1.10", "192.168.1.20", "192.168.1.30"]
        );
        assert_eq!(entries[0].latency, LatencyState::Unknown);
        assert!(!entries[0].active);
        assert_eq!(entries[0].kind, None);
        assert_eq!(entries[1].latency, LatencyState::Rtt(800));
        assert!(entries[1].active, "active address must be flagged");
        assert_eq!(entries[1].kind, Some(IfaceKind::Wired));
        assert_eq!(entries[2].latency, LatencyState::Unmeasured);
        assert_eq!(entries[2].kind, Some(IfaceKind::WiFi));
    }

    #[test]
    fn iface_labels_are_human_readable() {
        assert_eq!(iface_label(Some(IfaceKind::Wired)), Some("Wired"));
        assert_eq!(iface_label(Some(IfaceKind::WiFi)), Some("Wi-Fi"));
        assert_eq!(iface_label(None), None);
    }
}

/// Render the 8-byte ASCII commit hash carried in
/// [`mousehop_ipc::ClientState::peer_commit`] as a `String`. `None`
/// in → `None` out (peer hasn't sent a Hello yet, or speaks an older
/// proto).
pub fn peer_commit_to_string(commit: Option<[u8; 8]>) -> Option<String> {
    commit.and_then(|c| std::str::from_utf8(&c).ok().map(str::to_string))
}

#[derive(Default, Clone)]
pub struct ClientData {
    pub handle: ClientHandle,
    pub hostname: Option<String>,
    pub port: u32,
    pub active: bool,
    pub position: String,
    pub resolving: bool,
    pub ips: Vec<String>,
    pub peer_commit: Option<String>,
    pub clipboard_send: bool,
    /// Candidate addresses with per-address latency, rendered into the
    /// address-selector dropdown. Not a GObject property — updated via
    /// [`ClientObject::set_addresses`].
    pub addresses: Vec<AddrEntry>,
    /// Base connection policy (Auto / Fastest).
    pub mode: ConnectionMode,
    /// Address pinned on the current network (string form), or `None`.
    pub active_lock: Option<String>,
}
