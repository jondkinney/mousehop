use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    rc::Rc,
};

use slab::Slab;

use mousehop_ipc::{ClientConfig, ClientHandle, ClientState, ConnectionMode, IfaceKind, Position};

use crate::config::ConfigClient;

#[derive(Clone, Default)]
pub struct ClientManager {
    clients: Rc<RefCell<Slab<(ClientConfig, ClientState)>>>,
}

impl ClientManager {
    /// get all clients
    pub fn clients(&self) -> Vec<(ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(_, c)| c.clone())
            .collect::<Vec<_>>()
    }

    pub fn add_with_config(&self, config_client: ConfigClient) -> ClientHandle {
        let config = ClientConfig {
            hostname: config_client.hostname,
            fix_ips: config_client.ips.into_iter().collect(),
            port: config_client.port,
            pos: config_client.pos,
            cmd: config_client.enter_hook,
            mode: config_client.mode,
            network_locks: config_client.network_locks,
            clipboard_send: config_client.clipboard_send,
        };
        let state = ClientState {
            active: config_client.active,
            ips: HashSet::from_iter(config.fix_ips.iter().cloned()),
            ..Default::default()
        };
        let handle = self.add_client();
        self.set_config(handle, config);
        self.set_state(handle, state);
        handle
    }

    /// add a new client to this manager
    pub fn add_client(&self) -> ClientHandle {
        self.clients.borrow_mut().insert(Default::default()) as ClientHandle
    }

    /// set the config of the given client
    pub fn set_config(&self, handle: ClientHandle, config: ClientConfig) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *c = config;
        }
    }

    /// set the state of the given client
    pub fn set_state(&self, handle: ClientHandle, state: ClientState) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            *s = state;
        }
    }

    /// activate the given client
    /// returns, whether the client was activated
    pub fn activate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if !s.active => {
                s.active = true;
                true
            }
            _ => false,
        }
    }

    /// deactivate the given client
    /// returns, whether the client was deactivated
    pub fn deactivate_client(&self, handle: ClientHandle) -> bool {
        let mut clients = self.clients.borrow_mut();
        match clients.get_mut(handle as usize) {
            Some((_, s)) if s.active => {
                s.active = false;
                true
            }
            _ => false,
        }
    }

    /// find a client by its address
    ///
    /// Matches against the union of (a) the client's known ip set
    /// `s.ips` (`fix_ips` plus DNS-resolved addresses), and (b) the
    /// client's currently-active outbound DTLS address. The
    /// `active_addr` fallback covers mDNS-primary scenarios where
    /// the dialer picked an address that wasn't in DNS — the
    /// listen-side counterpart of that connection arrives from the
    /// same IP, which we'd otherwise drop on the floor (silently
    /// breaking peer-version display, among other things).
    pub fn get_client(&self, addr: SocketAddr) -> Option<ClientHandle> {
        // since there shouldn't be more than a handful of clients at any given
        // time this is likely faster than using a HashMap
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (_, s))| {
                if !s.active {
                    return None;
                }
                let ip = addr.ip();
                let active_match = s.active_addr.is_some_and(|a| a.ip() == ip);
                if s.ips.contains(&ip) || active_match {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    /// get the client at the given position
    pub fn client_at(&self, pos: Position) -> Option<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .find_map(|(k, (c, s))| {
                if s.active && c.pos == pos {
                    Some(k)
                } else {
                    None
                }
            })
            .map(|p| p as ClientHandle)
    }

    pub(crate) fn get_hostname(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow_mut()
            .get_mut(handle as usize)
            .and_then(|(c, _)| c.hostname.clone())
    }

    /// get the position of the corresponding client
    pub(crate) fn get_pos(&self, handle: ClientHandle) -> Option<Position> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.pos)
    }

    /// remove a client from the list
    pub fn remove_client(&self, client: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        // remove id from occupied ids
        self.clients.borrow_mut().try_remove(client as usize)
    }

    /// get the config & state of the given client
    pub fn get_state(&self, handle: ClientHandle) -> Option<(ClientConfig, ClientState)> {
        self.clients.borrow().get(handle as usize).cloned()
    }

    /// get the current config & state of all clients
    pub fn get_client_states(&self) -> Vec<(ClientHandle, ClientConfig, ClientState)> {
        self.clients
            .borrow()
            .iter()
            .map(|(k, v)| (k as ClientHandle, v.0.clone(), v.1.clone()))
            .collect()
    }

    /// update the fix ips of the client
    pub fn set_fix_ips(&self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        if let Some((c, _)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.fix_ips = fix_ips
        }
        self.update_ips(handle);
    }

    /// update the dns-ips of the client
    pub fn set_dns_ips(&self, handle: ClientHandle, dns_ips: Vec<IpAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.dns_ips = dns_ips
        }
        self.update_ips(handle);
    }

    fn update_ips(&self, handle: ClientHandle) {
        if let Some((c, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.ips = c
                .fix_ips
                .iter()
                .cloned()
                .chain(s.dns_ips.iter().cloned())
                .chain(s.discovered_ips.iter().cloned())
                .collect::<HashSet<_>>();
            // Drop per-address metadata for addresses that are no longer
            // candidates so the GUI never shows a stale RTT / label next
            // to an IP that has since disappeared from DNS / discovery /
            // the fix list.
            s.latencies.retain(|ip, _| s.ips.contains(ip));
            s.interfaces.retain(|ip, _| s.ips.contains(ip));
        }
    }

    /// update the hostname of the given client
    /// this automatically clears the active ip address and ips from dns
    pub fn set_hostname(&self, handle: ClientHandle, hostname: Option<String>) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((c, s)) = clients.get_mut(handle as usize) else {
            return false;
        };

        // hostname changed
        if c.hostname != hostname {
            c.hostname = hostname;
            s.active_addr = None;
            s.dns_ips.clear();
            drop(clients);
            self.update_ips(handle);
            true
        } else {
            false
        }
    }

    /// update the port of the client
    pub(crate) fn set_port(&self, handle: ClientHandle, port: u16) {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.port != port => {
                c.port = port;
                s.active_addr = s.active_addr.map(|a| SocketAddr::new(a.ip(), port));
            }
            _ => {}
        };
    }

    /// update the position of the client
    /// returns true, if a change in capture position is required (pos changed & client is active)
    pub(crate) fn set_pos(&self, handle: ClientHandle, pos: Position) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) if c.pos != pos => {
                log::info!("update pos {handle} {} -> {}", c.pos, pos);
                c.pos = pos;
                s.active
            }
            _ => false,
        }
    }

    /// update the enter hook command of the client
    pub(crate) fn set_enter_hook(&self, handle: ClientHandle, enter_hook: Option<String>) {
        if let Some((c, _s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            c.cmd = enter_hook;
        }
    }

    /// set resolving status of the client
    pub(crate) fn set_resolving(&self, handle: ClientHandle, status: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.resolving = status;
        }
    }

    /// get the enter hook command
    pub(crate) fn get_enter_cmd(&self, handle: ClientHandle) -> Option<String> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(c, _)| c.cmd.clone())
    }

    /// returns all clients that are currently registered
    pub(crate) fn registered_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    /// returns all clients that are currently active
    pub(crate) fn active_clients(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (_, s))| s.active)
            .map(|(h, _)| h as ClientHandle)
            .collect()
    }

    pub(crate) fn set_active_addr(&self, handle: ClientHandle, addr: Option<SocketAddr>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.active_addr = addr;
        }
    }

    pub(crate) fn set_alive(&self, handle: ClientHandle, alive: bool) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.alive = alive;
        }
    }

    pub(crate) fn set_peer_commit(&self, handle: ClientHandle, commit: Option<[u8; 8]>) {
        if let Some((_, s)) = self.clients.borrow_mut().get_mut(handle as usize) {
            s.peer_commit = commit;
        }
    }

    pub(crate) fn active_addr(&self, handle: ClientHandle) -> Option<SocketAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.active_addr)
    }

    pub(crate) fn alive(&self, handle: ClientHandle) -> bool {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.alive)
            .unwrap_or(false)
    }

    pub(crate) fn get_port(&self, handle: ClientHandle) -> Option<u16> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.port)
    }

    pub(crate) fn get_ips(&self, handle: ClientHandle) -> Option<HashSet<IpAddr>> {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(_, s)| s.ips.clone())
    }

    /// Update the per-pair clipboard-send gate for the given client.
    /// Returns `true` when the value changed (so callers can avoid
    /// no-op config writes / frontend broadcasts).
    pub(crate) fn set_clipboard_send(&self, handle: ClientHandle, enabled: bool) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, _)) if c.clipboard_send != enabled => {
                c.clipboard_send = enabled;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn get_mode(&self, handle: ClientHandle) -> ConnectionMode {
        self.clients
            .borrow()
            .get(handle as usize)
            .map(|(c, _)| c.mode)
            .unwrap_or_default()
    }

    /// Set the base connection policy *and* drop any explicit lock for
    /// the current network (picking Auto/Fastest releases a pin set on
    /// this LAN). Returns `true` if anything changed.
    pub(crate) fn set_mode(
        &self,
        handle: ClientHandle,
        mode: ConnectionMode,
        current_network: Option<&str>,
    ) -> bool {
        let mut clients = self.clients.borrow_mut();
        let Some((c, _)) = clients.get_mut(handle as usize) else {
            return false;
        };
        let mut changed = c.mode != mode;
        c.mode = mode;
        if let Some(net) = current_network {
            changed |= c.network_locks.remove(net).is_some();
        }
        changed
    }

    /// Pin `ip` for `current_network`. Returns `true` if the stored
    /// lock for this network changed.
    pub(crate) fn set_network_lock(
        &self,
        handle: ClientHandle,
        current_network: &str,
        ip: IpAddr,
    ) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, _)) if c.network_locks.get(current_network) != Some(&ip) => {
                c.network_locks.insert(current_network.to_string(), ip);
                true
            }
            _ => false,
        }
    }

    /// Resolve the lock that applies on `current_network` into
    /// `ClientState::active_lock`. Returns `true` if it changed, so the
    /// caller re-broadcasts / re-dials only on real moves.
    pub(crate) fn recompute_active_lock(
        &self,
        handle: ClientHandle,
        current_network: Option<&str>,
    ) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((c, s)) => {
                let lock = current_network
                    .and_then(|net| c.network_locks.get(net))
                    .copied();
                if s.active_lock != lock {
                    s.active_lock = lock;
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// The lock currently in force (already resolved against the active
    /// network by [`Self::recompute_active_lock`]).
    pub(crate) fn get_active_lock(&self, handle: ClientHandle) -> Option<IpAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| s.active_lock)
    }

    /// Lowest-latency *reachable* candidate, for `ConnectionMode::Fastest`.
    /// `None` when nothing has a usable measurement yet.
    pub(crate) fn lowest_latency_addr(&self, handle: ClientHandle) -> Option<IpAddr> {
        self.clients
            .borrow()
            .get(handle as usize)
            .and_then(|(_, s)| {
                s.latencies
                    .iter()
                    .filter_map(|(ip, rtt)| rtt.map(|us| (us, *ip)))
                    .min()
                    .map(|(_, ip)| ip)
            })
    }

    /// For `Fastest` mode: the candidate that is *substantially* faster
    /// than the currently-active address — at least 2× and 15 ms better
    /// — or `None` if the active path is already (near-)fastest or we
    /// lack measurements. The hysteresis margin keeps a marginally-
    /// faster path from triggering a switch.
    pub(crate) fn fastest_upgrade_candidate(&self, handle: ClientHandle) -> Option<IpAddr> {
        const MIN_ABSOLUTE_GAIN_US: u32 = 15_000;
        let clients = self.clients.borrow();
        let (_, s) = clients.get(handle as usize)?;
        let active = s.active_addr?.ip();
        let active_us = s.latencies.get(&active).copied().flatten()?;
        let (best_us, best_ip) = s
            .latencies
            .iter()
            .filter_map(|(ip, rtt)| rtt.map(|us| (us, *ip)))
            .min()?;
        let substantial = best_us.saturating_mul(2) < active_us
            && active_us.saturating_sub(best_us) >= MIN_ABSOLUTE_GAIN_US;
        (best_ip != active && substantial).then_some(best_ip)
    }

    /// Replace the mDNS-discovered address set for a client and refold
    /// the candidate set. Returns `true` if the discovered set changed.
    pub(crate) fn set_discovered_ips(&self, handle: ClientHandle, discovered: Vec<IpAddr>) -> bool {
        let changed = match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((_, s)) if s.discovered_ips != discovered => {
                s.discovered_ips = discovered;
                true
            }
            _ => false,
        };
        if changed {
            self.update_ips(handle);
        }
        changed
    }

    /// Merge per-address interface kinds advertised by the peer. Only
    /// keeps entries for current candidate addresses. Returns `true`
    /// if the stored map changed.
    pub(crate) fn set_interfaces(
        &self,
        handle: ClientHandle,
        interfaces: HashMap<IpAddr, IfaceKind>,
    ) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((_, s)) => {
                let filtered: HashMap<IpAddr, IfaceKind> = interfaces
                    .into_iter()
                    .filter(|(ip, _)| s.ips.contains(ip))
                    .collect();
                if s.interfaces != filtered {
                    s.interfaces = filtered;
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// Record the latest round-trip probe for one candidate address
    /// (`Some(micros)` reachable, `None` unreachable/timeout). Ignored
    /// if the address is no longer a candidate (it may have just been
    /// pruned by [`Self::update_ips`]). Returns `true` when the stored
    /// value changed, so the caller only re-broadcasts on real moves.
    pub(crate) fn set_latency(
        &self,
        handle: ClientHandle,
        ip: IpAddr,
        rtt_micros: Option<u32>,
    ) -> bool {
        match self.clients.borrow_mut().get_mut(handle as usize) {
            Some((_, s)) if s.ips.contains(&ip) => match s.latencies.get(&ip) {
                Some(prev) if *prev == rtt_micros => false,
                _ => {
                    s.latencies.insert(ip, rtt_micros);
                    true
                }
            },
            _ => false,
        }
    }

    /// Snapshot of `(handle, port, candidate ips)` for every
    /// registered client, for the background latency prober. The
    /// *active* address is excluded — its latency comes from the live
    /// connection's ping/pong RTT (accurate, and works through a
    /// firewall that drops the TCP probe), so probing it would only
    /// risk overwriting that with a TCP timeout. Clients left with no
    /// inactive candidates are dropped. Takes an owned copy so the
    /// prober never holds the borrow across its async probes.
    pub(crate) fn probe_targets(&self) -> Vec<(ClientHandle, u16, Vec<IpAddr>)> {
        self.clients
            .borrow()
            .iter()
            .filter_map(|(k, (c, s))| {
                let active = s.active_addr.map(|a| a.ip());
                let ips: Vec<IpAddr> = s
                    .ips
                    .iter()
                    .copied()
                    .filter(|ip| Some(*ip) != active)
                    .collect();
                (!ips.is_empty()).then_some((k as ClientHandle, c.port, ips))
            })
            .collect()
    }

    /// Snapshot of every client whose `clipboard_send` is true and
    /// whose state is `active`. Used by Service to fan-out a local
    /// clipboard change without holding the manager's borrow across
    /// async sends.
    pub(crate) fn clipboard_send_targets(&self) -> Vec<ClientHandle> {
        self.clients
            .borrow()
            .iter()
            .filter(|(_, (c, s))| c.clipboard_send && s.active)
            .map(|(k, _)| k as ClientHandle)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigClient;
    use std::net::Ipv4Addr;

    fn ip(a: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, a))
    }

    fn manager_with_ips(ips: &[IpAddr]) -> (ClientManager, ClientHandle) {
        let cm = ClientManager::default();
        let handle = cm.add_with_config(ConfigClient {
            ips: ips.iter().copied().collect(),
            hostname: None,
            port: 4252,
            pos: Default::default(),
            active: false,
            enter_hook: None,
            mode: ConnectionMode::Auto,
            network_locks: HashMap::new(),
            clipboard_send: false,
        });
        (cm, handle)
    }

    const NET_A: &str = "gw:aa:bb:cc:dd:ee:01";
    const NET_B: &str = "gw:aa:bb:cc:dd:ee:02";

    #[test]
    fn network_lock_only_applies_on_its_own_network() {
        let (cm, h) = manager_with_ips(&[ip(10), ip(20)]);
        // lock .10 on network A
        assert!(cm.set_network_lock(h, NET_A, ip(10)));
        // on network A the lock resolves
        assert!(cm.recompute_active_lock(h, Some(NET_A)));
        assert_eq!(cm.get_active_lock(h), Some(ip(10)));
        // on a different network there's no lock -> falls back to Auto
        assert!(cm.recompute_active_lock(h, Some(NET_B)));
        assert_eq!(cm.get_active_lock(h), None);
        // back on A it resolves again
        assert!(cm.recompute_active_lock(h, Some(NET_A)));
        assert_eq!(cm.get_active_lock(h), Some(ip(10)));
    }

    #[test]
    fn setting_mode_clears_current_network_lock_only() {
        let (cm, h) = manager_with_ips(&[ip(10)]);
        cm.set_network_lock(h, NET_A, ip(10));
        cm.set_network_lock(h, NET_B, ip(10));
        // switching to Fastest on network A clears A's lock, keeps B's
        assert!(cm.set_mode(h, ConnectionMode::Fastest, Some(NET_A)));
        assert_eq!(cm.get_mode(h), ConnectionMode::Fastest);
        cm.recompute_active_lock(h, Some(NET_A));
        assert_eq!(cm.get_active_lock(h), None);
        cm.recompute_active_lock(h, Some(NET_B));
        assert_eq!(cm.get_active_lock(h), Some(ip(10)));
    }

    #[test]
    fn lowest_latency_addr_picks_min_reachable() {
        let (cm, h) = manager_with_ips(&[ip(10), ip(20), ip(30)]);
        cm.set_latency(h, ip(10), Some(5000));
        cm.set_latency(h, ip(20), Some(900));
        cm.set_latency(h, ip(30), None); // unreachable -> ignored
        assert_eq!(cm.lowest_latency_addr(h), Some(ip(20)));
    }

    #[test]
    fn discovered_ips_fold_into_candidates() {
        let (cm, h) = manager_with_ips(&[ip(10)]);
        assert!(cm.set_discovered_ips(h, vec![ip(20), ip(30)]));
        let (_, state) = cm.get_state(h).unwrap();
        assert!(state.ips.contains(&ip(10))); // fix ip kept
        assert!(state.ips.contains(&ip(20))); // discovered folded in
        assert!(state.ips.contains(&ip(30)));
    }

    #[test]
    fn set_latency_only_for_known_addresses() {
        let (cm, h) = manager_with_ips(&[ip(10), ip(20)]);
        // a candidate address stores and reports a change
        assert!(cm.set_latency(h, ip(10), Some(800)));
        // re-storing the same value is not a change
        assert!(!cm.set_latency(h, ip(10), Some(800)));
        // a different value is a change
        assert!(cm.set_latency(h, ip(10), Some(900)));
        // an address that isn't a candidate is ignored
        assert!(!cm.set_latency(h, ip(99), Some(100)));
        let (_, state) = cm.get_state(h).unwrap();
        assert_eq!(state.latencies.get(&ip(10)), Some(&Some(900)));
        assert_eq!(state.latencies.get(&ip(99)), None);
    }

    #[test]
    fn update_ips_prunes_stale_latencies() {
        let (cm, h) = manager_with_ips(&[ip(10), ip(20)]);
        cm.set_latency(h, ip(10), Some(800));
        cm.set_latency(h, ip(20), None);
        // drop .20 from the candidate set
        cm.set_fix_ips(h, vec![ip(10)]);
        let (_, state) = cm.get_state(h).unwrap();
        assert!(state.latencies.contains_key(&ip(10)));
        assert!(
            !state.latencies.contains_key(&ip(20)),
            "latency for a removed address must be pruned"
        );
    }

    #[test]
    fn probe_targets_skips_clients_without_candidates() {
        let (cm, with) = manager_with_ips(&[ip(10), ip(20)]);
        let empty = cm.add_client(); // no candidate ips
        let targets = cm.probe_targets();
        let with_entry = targets.iter().find(|(h, _, _)| *h == with).unwrap();
        assert_eq!(with_entry.1, 4252);
        assert_eq!(with_entry.2.len(), 2);
        assert!(
            !targets.iter().any(|(h, _, _)| *h == empty),
            "a client with no candidate ips must not be a probe target"
        );
    }
}
