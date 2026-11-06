//! Private association data, never an exported syscall/network observation.
//! Each Registry belongs to exactly one diagnostic network namespace.
use crate::process::Identity;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
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
    pending: AtomicUsize,
}

pub struct Pending(Registry);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.0.pending.fetch_sub(1, Ordering::Release);
        self.0.0.changed.notify_waiters();
    }
}

impl Registry {
    pub fn begin_datagram(&self) -> Pending {
        self.0.pending.fetch_add(1, Ordering::AcqRel);
        Pending(self.clone())
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
        let wait = async {
            loop {
                let changed = self.0.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if (key.transport != Transport::Udp || self.0.pending.load(Ordering::Acquire) == 0)
                    && let Some(metadata) = self.lookup(key)
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
