//! Private association data, never an exported syscall/network observation.
//! Each Registry belongs to exactly one diagnostic network namespace.
use crate::process::Identity;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

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
    pub socket_id: u64,
    pub socket_ids: Vec<u64>,
    pub actors: Vec<Identity>,
}
impl Metadata {
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
    entries: Mutex<HashMap<Key, (Metadata, Instant)>>,
    changed: Notify,
    pending: Mutex<HashMap<PendingScope, usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct PendingScope {
    source: Option<SocketAddr>,
    destination: Option<SocketAddr>,
}

pub struct Pending {
    registry: Registry,
    scopes: Vec<PendingScope>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let mut pending = self.registry.0.pending.lock().unwrap();
        for scope in &self.scopes {
            if let Some(count) = pending.get_mut(scope) {
                *count -= 1;
                if *count == 0 {
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
        for scope in &scopes {
            *pending.entry(*scope).or_default() += 1;
        }
        Pending {
            registry: self.clone(),
            scopes,
        }
    }

    fn has_pending(&self, key: &Key) -> bool {
        let pending = self.0.pending.lock().unwrap();
        // An auto-bound socket's source is known only after its first send.
        // Unknown endpoints conservatively match that portion of the tuple.
        [Some(key.source), None].into_iter().any(|source| {
            [Some(key.destination), None]
                .into_iter()
                .any(|destination| {
                    pending.contains_key(&PendingScope {
                        source,
                        destination,
                    })
                })
        })
    }
    pub fn register(&self, key: Key, socket_id: u64, actor: Identity) {
        let key = key.canonical();
        let mut entries = self.0.entries.lock().unwrap();
        if entries.len() >= 16384 {
            entries.retain(|_, (_, time)| time.elapsed() < Duration::from_secs(120));
        }
        let (metadata, time) = entries.entry(key).or_insert_with(|| {
            (
                Metadata {
                    socket_id,
                    socket_ids: vec![socket_id],
                    actors: Vec::new(),
                },
                Instant::now(),
            )
        });
        if key.transport == Transport::Tcp && metadata.socket_id != socket_id {
            *metadata = Metadata {
                socket_id,
                socket_ids: vec![socket_id],
                actors: Vec::new(),
            };
        }
        // UDP permits multiple live sockets to share an identical five-tuple.
        // Keep all observed participants until this proxy flow ends; replacing
        // the entry would incorrectly label their packets as the latest sender.
        if !metadata.socket_ids.contains(&socket_id) {
            metadata.socket_ids.push(socket_id);
        }
        if !metadata.actors.contains(&actor) {
            metadata.actors.push(actor);
        }
        *time = Instant::now();
        drop(entries);
        self.0.changed.notify_waiters();
    }
    pub fn lookup(&self, key: &Key) -> Option<Metadata> {
        self.0
            .entries
            .lock()
            .unwrap()
            .get(&key.canonical())
            .map(|(m, _)| m.clone())
    }
    pub fn remove(&self, key: &Key, socket_id: u64) {
        let key = key.canonical();
        let mut entries = self.0.entries.lock().unwrap();
        if entries
            .get(&key)
            .is_some_and(|(m, _)| m.socket_id == socket_id)
        {
            entries.remove(&key);
        }
    }
    pub async fn resolve(&self, key: &Key) -> Option<Metadata> {
        let key = key.canonical();
        let wait = async {
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if (key.transport != Transport::Udp || !self.has_pending(&key))
                    && let Some(metadata) = self.lookup(&key)
                {
                    return metadata;
                }
                changed.await;
            }
        };
        tokio::time::timeout(Duration::from_millis(250), wait)
            .await
            .ok()
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
        );
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
            tokio::time::timeout(Duration::from_millis(30), registry.resolve(&second)).await;
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
            tokio::time::timeout(Duration::from_millis(30), registry.resolve(&other))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), registry.resolve(&first))
                .await
                .is_err()
        );
        drop(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), registry.resolve(&first))
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
            tokio::time::timeout(Duration::from_millis(20), registry.resolve(&key))
                .await
                .is_err()
        );
        register(&registry, key, 2);
        drop(second);
        let metadata = registry.resolve(&key).await.unwrap();
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
        );
        assert!(registry.resolve(&observed).await.is_some());
    }
}
