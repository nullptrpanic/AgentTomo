//! Private association data, never an exported syscall/network observation.
//! Each Registry belongs to exactly one diagnostic network namespace.
use crate::process::Identity;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::Notify;

const MAX_RETIRED_UDP_GENERATIONS: usize = 256;
const PRUNE_THRESHOLD: usize = 16384;
const ENTRY_TTL: Duration = Duration::from_secs(120);
const PRUNE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Udp,
}
impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Key {
    pub transport: Transport,
    pub source: SocketAddr,
    pub destination: SocketAddr,
}
impl Key {
    fn canonical(mut self) -> Self {
        self.source = super::address::canonical(self.source);
        self.destination = super::address::canonical(self.destination);
        self
    }
}

#[derive(Clone)]
pub struct Metadata {
    since: SystemTime,
    pub socket_id: u64,
    pub socket_ids: Vec<u64>,
    pub actors: Vec<Identity>,
}
impl Metadata {
    fn new(socket_id: u64, since: SystemTime) -> Self {
        Self {
            since,
            socket_id,
            socket_ids: vec![socket_id],
            actors: Vec::new(),
        }
    }
    pub fn fields(&self, data: &mut Value, tcp: bool) {
        data["socket_id"] = if self.socket_ids.len() == 1 {
            self.socket_id.into()
        } else {
            Value::Null
        };
        data["socket_ids"] = json!(self.socket_ids);
        data["participants"] = json!(self.actors);
        data["attribution"] = if tcp {
            "connect_initiator"
        } else {
            "flow_participants"
        }
        .into();
        // A flow shared by multiple writers has no single packet-level owner.
        if self.actors.len() == 1 {
            crate::process::fields(Some(self.actors[0]), data);
        }
    }
}

#[derive(Clone, Default)]
pub struct Registry(Arc<Inner>);
#[derive(Default)]
struct Inner {
    entries: Mutex<Entries>,
    changed: Notify,
    pending: Mutex<HashMap<PendingScope, BTreeMap<SystemTime, usize>>>,
}

struct Entry {
    current: Metadata,
    previous: VecDeque<Metadata>,
    updated: Instant,
}

#[derive(Default)]
struct Entries {
    flows: HashMap<Key, Entry>,
    last_sweep: Option<Instant>,
    expired_through: Option<Instant>,
}

impl Entries {
    fn get(&self, key: &Key) -> Option<&Entry> {
        self.flows.get(key).filter(|entry| {
            self.expired_through
                .is_none_or(|cutoff| entry.updated > cutoff)
        })
    }

    fn prune(&mut self, key: &Key, now: Instant) {
        if self.flows.len() >= PRUNE_THRESHOLD {
            // Advance logical expiry even between physical sweeps. A later
            // removal may take the table below the threshold before a lookup.
            self.expired_through = now.checked_sub(ENTRY_TTL);
            if self
                .last_sweep
                .is_none_or(|last| now.duration_since(last) >= PRUNE_INTERVAL)
            {
                self.flows.retain(|_, entry| {
                    self.expired_through
                        .is_none_or(|cutoff| entry.updated > cutoff)
                });
                self.last_sweep = Some(now);
                return;
            }
        }
        if self.expired_through.is_some_and(|cutoff| {
            self.flows
                .get(key)
                .is_some_and(|entry| entry.updated <= cutoff)
        }) {
            self.flows.remove(key);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PendingScope {
    source: Option<SocketAddr>,
    destination: Option<SocketAddr>,
}

pub struct Pending {
    pub started: SystemTime,
    registry: Registry,
    scopes: Vec<PendingScope>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let mut pending = self.registry.0.pending.lock().unwrap();
        for scope in &self.scopes {
            if let Some(starts) = pending.get_mut(scope) {
                if let Some(count) = starts.get_mut(&self.started) {
                    *count -= 1;
                    if *count == 0 {
                        starts.remove(&self.started);
                    }
                }
                if starts.is_empty() {
                    pending.remove(scope);
                }
            }
        }
        drop(pending);
        self.registry.0.changed.notify_waiters();
    }
}

impl Registry {
    pub fn begin_datagram(
        &self,
        source: Option<SocketAddr>,
        destinations: &[Option<SocketAddr>],
    ) -> Pending {
        let source = source.map(super::address::canonical);
        let scopes: Vec<_> = destinations
            .iter()
            .map(|destination| PendingScope {
                source,
                destination: destination.map(super::address::canonical),
            })
            .collect();
        let mut pending = self.0.pending.lock().unwrap();
        let started = SystemTime::now();
        for scope in &scopes {
            *pending
                .entry(*scope)
                .or_default()
                .entry(started)
                .or_default() += 1;
        }
        Pending {
            started,
            registry: self.clone(),
            scopes,
        }
    }

    fn has_pending(&self, key: &Key, arrived: Option<SystemTime>) -> bool {
        let pending = self.0.pending.lock().unwrap();
        // An auto-bound socket's source is known only after its first send.
        // Unknown endpoints conservatively match that portion of the tuple.
        [Some(key.source), None].into_iter().any(|source| {
            [Some(key.destination), None]
                .into_iter()
                .any(|destination| {
                    pending
                        .get(&PendingScope {
                            source,
                            destination,
                        })
                        .and_then(BTreeMap::first_key_value)
                        .is_some_and(|(started, _)| arrived.is_none_or(|at| *started <= at))
                })
        })
    }
    pub fn register(
        &self,
        key: Key,
        socket_id: u64,
        actor: Identity,
        started: SystemTime,
        udp_overlap: impl FnOnce(&[u64]) -> bool,
    ) {
        let key = key.canonical();
        let mut entries = self.0.entries.lock().unwrap();
        entries.prune(&key, Instant::now());
        let mut replace_udp = None;
        if key.transport == Transport::Udp
            && let Some(entry) = entries.flows.get(&key)
            && !entry.current.socket_ids.contains(&socket_id)
        {
            let ids = entry.current.socket_ids.clone();
            drop(entries);
            // Inspect the kernel socket table outside the proxy's shared lock.
            if !udp_overlap(&ids) {
                replace_udp = Some(ids);
            }
            entries = self.0.entries.lock().unwrap();
            entries.prune(&key, Instant::now());
        }
        let entry = entries.flows.entry(key).or_insert_with(|| Entry {
            current: Metadata::new(socket_id, started),
            previous: VecDeque::new(),
            updated: Instant::now(),
        });
        if replace_udp.as_ref() == Some(&entry.current.socket_ids) {
            // Retain boundaries for datagrams already queued in the kernel or
            // worker. An older packet outside the window has unknown ownership.
            if entry.previous.len() == MAX_RETIRED_UDP_GENERATIONS {
                entry.previous.pop_front();
            }
            entry.previous.push_back(std::mem::replace(
                &mut entry.current,
                Metadata::new(socket_id, started),
            ));
        } else if key.transport == Transport::Tcp && entry.current.socket_id != socket_id {
            entry.current = Metadata::new(socket_id, started);
        }
        let metadata = &mut entry.current;
        metadata.since = metadata.since.min(started);
        if !metadata.socket_ids.contains(&socket_id) {
            metadata.socket_ids.push(socket_id);
        }
        if !metadata.actors.contains(&actor) {
            metadata.actors.push(actor);
        }
        entry.updated = Instant::now();
        drop(entries);
        self.0.changed.notify_waiters();
    }

    pub fn remove(&self, key: &Key, socket_id: u64) {
        let key = key.canonical();
        let mut entries = self.0.entries.lock().unwrap();
        if entries
            .flows
            .get(&key)
            .is_some_and(|entry| entry.current.socket_id == socket_id)
        {
            entries.flows.remove(&key);
        }
    }

    pub async fn resolve(&self, key: &Key, arrived: Option<SystemTime>) -> Option<Metadata> {
        let key = key.canonical();
        let wait = async {
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if (key.transport != Transport::Udp || !self.has_pending(&key, arrived))
                    && let Some(entry) = self.0.entries.lock().unwrap().get(&key)
                {
                    return match arrived {
                        Some(at) if at < entry.current.since => entry
                            .previous
                            .iter()
                            .rev()
                            .find(|generation| generation.since <= at)
                            .cloned(),
                        _ => Some(entry.current.clone()),
                    };
                }
                changed.await;
            }
        };
        tokio::time::timeout(Duration::from_millis(250), wait)
            .await
            .ok()
            .flatten()
    }
}

pub fn fields(key: Key, metadata: Option<&Metadata>, data: &mut Value) {
    crate::process::fields(None, data);
    data["transport"] = key.transport.name().into();
    data["source_address"] = key.source.to_string().into();
    data["destination"] = key.destination.to_string().into();
    data["network_backend"] = "netfilter_tproxy".into();
    data["attribution"] = "unavailable".into();
    if let Some(metadata) = metadata {
        metadata.fields(data, key.transport == Transport::Tcp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn udp(source: u16, destination: u16) -> Key {
        Key {
            transport: Transport::Udp,
            source: ([127, 0, 0, 1], source).into(),
            destination: ([127, 0, 0, 2], destination).into(),
        }
    }

    fn register(registry: &Registry, key: Key, id: u64) {
        registry.register(
            key,
            id,
            Identity {
                pid: id as i32,
                process_instance_id: id,
            },
            SystemTime::now(),
            |_| true,
        );
    }

    #[test]
    fn busy_registrations_do_not_rescan_the_table_or_reuse_expired_owners() {
        let registry = Registry::default();
        for port in 1..=16384 {
            register(&registry, udp(port, 53), u64::from(port));
        }
        let busy = udp(1, 53);
        register(&registry, busy, 1);
        let old = udp(2, 53);
        registry
            .0
            .entries
            .lock()
            .unwrap()
            .flows
            .get_mut(&old)
            .unwrap()
            .updated = Instant::now() - Duration::from_secs(121);
        register(&registry, busy, 1);
        assert!(
            registry.0.entries.lock().unwrap().flows.contains_key(&old),
            "unrelated registration swept twice"
        );
        registry.remove(&udp(3, 53), 3);
        assert!(registry.0.entries.lock().unwrap().get(&old).is_none());
        register(&registry, old, 999);
        let mut table = registry.0.entries.lock().unwrap();
        assert_eq!(table.flows.get(&old).unwrap().current.socket_ids, [999]);
        table.flows.get_mut(&old).unwrap().updated = Instant::now() - Duration::from_secs(121);
        table.last_sweep = Some(Instant::now() - PRUNE_INTERVAL);
        drop(table);
        register(&registry, udp(3, 53), 3);
        register(&registry, busy, 1);
        assert!(!registry.0.entries.lock().unwrap().flows.contains_key(&old));
    }

    #[tokio::test]
    async fn packet_arrival_selects_its_generation_and_ignores_later_sends() {
        let registry = Registry::default();
        let key = udp(1001, 2000);
        let time = |id| SystemTime::UNIX_EPOCH + Duration::from_secs(id);
        for id in 1..=258 {
            registry.register(
                key,
                id,
                Identity {
                    pid: id as i32,
                    process_instance_id: id,
                },
                time(id),
                |_| false,
            );
        }
        let _later = registry.begin_datagram(Some(key.source), &[Some(key.destination)]);
        for (arrival, expected) in [(1, None), (2, Some(2)), (257, Some(257)), (258, Some(258))] {
            let metadata = tokio::time::timeout(
                Duration::from_millis(30),
                registry.resolve(&key, Some(time(arrival))),
            )
            .await
            .unwrap();
            assert_eq!(metadata.map(|m| m.socket_id), expected);
        }
    }

    #[tokio::test]
    async fn pending_send_does_not_hold_an_unrelated_registered_flow() {
        let registry = Registry::default();
        let first = udp(1001, 2000);
        let second = udp(1002, 2000);
        register(&registry, first, 1);
        register(&registry, second, 2);
        let _pending = registry.begin_datagram(Some(first.source), &[Some(first.destination)]);
        let metadata =
            tokio::time::timeout(Duration::from_millis(30), registry.resolve(&second, None)).await;
        assert!(
            metadata.is_ok_and(|value| value.is_some()),
            "an unrelated flow waited for another socket's send"
        );
    }

    #[tokio::test]
    async fn unknown_source_waits_only_for_its_destination_and_releases_on_drop() {
        let registry = Registry::default();
        let first = udp(1001, 2000);
        let other = udp(1002, 2001);
        register(&registry, first, 1);
        register(&registry, other, 2);
        let pending = registry.begin_datagram(None, &[Some(first.destination)]);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), registry.resolve(&other, None))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), registry.resolve(&first, None))
                .await
                .is_err()
        );
        drop(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), registry.resolve(&first, None))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn matching_sends_preserve_all_successful_writers_until_they_finish() {
        let registry = Registry::default();
        let key = udp(1001, 2000);
        let first = registry.begin_datagram(Some(key.source), &[Some(key.destination)]);
        let second = registry.begin_datagram(Some(key.source), &[Some(key.destination)]);
        register(&registry, key, 1);
        drop(first);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), registry.resolve(&key, None))
                .await
                .is_err()
        );
        register(&registry, key, 2);
        drop(second);
        let metadata = registry.resolve(&key, None).await.unwrap();
        assert_eq!(metadata.actors.len(), 2);
        assert_eq!(metadata.socket_ids, vec![1, 2]);
        assert!(registry.0.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ipv6_flowinfo_is_not_a_sixth_tuple_component() {
        let registry = Registry::default();
        let observed = Key {
            transport: Transport::Tcp,
            source: "[::1]:30000".parse().unwrap(),
            destination: "[2001:db8::1]:443".parse().unwrap(),
        };
        let mut requested = observed;
        if let SocketAddr::V6(destination) = &mut requested.destination {
            destination.set_flowinfo(123);
        }
        registry.register(
            requested,
            1,
            Identity {
                pid: 10,
                process_instance_id: 1,
            },
            SystemTime::now(),
            |_| unreachable!(),
        );
        assert!(registry.resolve(&observed, None).await.is_some());
    }
}
