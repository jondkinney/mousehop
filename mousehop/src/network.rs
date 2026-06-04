//! Local network identity + interface classification.
//!
//! Two jobs, both backed by `netdev` (cross-platform):
//!
//! 1. **Network fingerprint** ([`current_network_id`]) — a stable,
//!    opaque id for "which LAN am I on right now", used to scope
//!    per-network address locks (a lock set at home shouldn't pin a
//!    home IP at a coffee shop). The default-gateway MAC is the
//!    primary signal: it survives DHCP lease changes and distinguishes
//!    two different routers that both hand out `192.168.1.0/24`. Falls
//!    back to the default interface's subnet when no gateway MAC is
//!    available (point-to-point links, some VPNs).
//!
//! 2. **Interface advertisement** ([`local_addresses_with_kind`]) — the
//!    set of physical-interface addresses this host should advertise
//!    over mDNS, each tagged Wired/Wi-Fi so peers can label them. The
//!    interface *type* is invisible from L3, so the owner is the only
//!    one who can classify it.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr},
};

use mousehop_ipc::IfaceKind;
use netdev::interface::types::InterfaceType;

/// Opaque fingerprint of the machine's current network, or `None` when
/// it can't be determined (no gateway, no addressed default interface)
/// — in which case per-network locks simply don't apply and the base
/// [`mousehop_ipc::ConnectionMode`] is used.
pub(crate) fn current_network_id() -> Option<String> {
    // Primary: the default gateway's MAC. Stable per physical network.
    if let Ok(gw) = netdev::get_default_gateway() {
        let mac = gw.mac_addr.to_string();
        if !mac.is_empty() && mac != "00:00:00:00:00:00" {
            return Some(format!("gw:{mac}"));
        }
    }
    // Fallback: the default interface's subnet. Weaker (two different
    // 192.168.1.0/24 networks collide) but better than nothing.
    let iface = netdev::get_default_interface().ok()?;
    let net = iface.ipv4.first()?;
    Some(format!("net:{}/{}", net.network(), net.prefix_len()))
}

/// Map a `netdev` interface type to the coarse kind we surface to
/// users. Everything that isn't clearly Ethernet or Wi-Fi (loopback,
/// VPN/tunnel, bridge, cellular, unknown) collapses to `Other`.
pub(crate) fn iface_kind(t: InterfaceType) -> IfaceKind {
    match t {
        InterfaceType::Ethernet
        | InterfaceType::Ethernet3Megabit
        | InterfaceType::FastEthernetT
        | InterfaceType::FastEthernetFx
        | InterfaceType::GigabitEthernet => IfaceKind::Wired,
        InterfaceType::Wireless80211 | InterfaceType::PeerToPeerWireless => IfaceKind::WiFi,
        _ => IfaceKind::Other,
    }
}

/// Virtual-interface name prefixes to never advertise — container
/// bridges, libvirt, VM host-only nets, raw tun/tap. These often
/// present as Ethernet at the link layer (so the type check alone
/// misses them) yet carry host-local addresses no LAN peer can reach.
const VIRTUAL_IFACE_PREFIXES: &[&str] = &[
    "docker", "br-", "virbr", "veth", "vmnet", "vboxnet", "tap", "tun", "utun", "zt",
];

fn is_virtual_name(name: &str) -> bool {
    VIRTUAL_IFACE_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// True for addresses worth advertising / dialing: globally- or
/// LAN-routable unicast. Excludes loopback, unspecified, and link-local
/// (IPv4 169.254/16 and IPv6 `fe80::/10` — the latter needs a scope id
/// we can't carry, see `dns::is_unusable_candidate`).
pub(crate) fn is_routable_ip(ip: &IpAddr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => !v4.is_link_local(),
        IpAddr::V6(v6) => !is_v6_link_local(v6),
    }
}

fn is_v6_link_local(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// Interface kind of the default-route interface, used to label the
/// advertised "primary" address — which may live on a non-physical
/// interface (e.g. a VPN tunnel) that [`local_addresses_with_kind`]
/// otherwise filters out, leaving it unlabeled.
pub(crate) fn default_interface_kind() -> Option<IfaceKind> {
    netdev::get_default_interface()
        .ok()
        .map(|iface| iface_kind(iface.if_type))
}

/// Enumerate this host's advertisable physical-interface addresses and
/// their kinds. Returns `(addresses, ip -> kind)`. Only Wired/Wi-Fi
/// interfaces with routable addresses are included; loopback, VPN, and
/// container/virtual interfaces are dropped so peers don't see junk
/// candidates.
pub(crate) fn local_addresses_with_kind() -> (Vec<IpAddr>, HashMap<IpAddr, IfaceKind>) {
    let mut addrs = Vec::new();
    let mut kinds = HashMap::new();
    for iface in netdev::get_interfaces() {
        let kind = iface_kind(iface.if_type);
        if kind == IfaceKind::Other || is_virtual_name(&iface.name) {
            continue;
        }
        let v4 = iface.ipv4.iter().map(|n| IpAddr::V4(n.addr()));
        let v6 = iface.ipv6.iter().map(|n| IpAddr::V6(n.addr()));
        for ip in v4.chain(v6) {
            if is_routable_ip(&ip) {
                addrs.push(ip);
                kinds.insert(ip, kind);
            }
        }
    }
    (addrs, kinds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn classifies_interface_types() {
        assert_eq!(iface_kind(InterfaceType::GigabitEthernet), IfaceKind::Wired);
        assert_eq!(iface_kind(InterfaceType::Wireless80211), IfaceKind::WiFi);
        assert_eq!(iface_kind(InterfaceType::Tunnel), IfaceKind::Other);
        assert_eq!(iface_kind(InterfaceType::Loopback), IfaceKind::Other);
        assert_eq!(iface_kind(InterfaceType::Bridge), IfaceKind::Other);
    }

    #[test]
    fn routable_filter_excludes_loopback_and_link_local() {
        assert!(is_routable_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))));
        assert!(!is_routable_ip(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_routable_ip(&IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(!is_routable_ip(&"fe80::1".parse::<IpAddr>().unwrap()));
        assert!(is_routable_ip(&"fdb7:7db7::1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn virtual_names_are_skipped() {
        assert!(is_virtual_name("docker0"));
        assert!(is_virtual_name("br-1a2b3c"));
        assert!(is_virtual_name("veth9f8e"));
        assert!(!is_virtual_name("enp191s0"));
        assert!(!is_virtual_name("wlan0"));
    }
}
