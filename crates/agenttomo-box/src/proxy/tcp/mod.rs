//! Persistent transparent TCP listeners. Payload observations originate here,
//! using private process metadata supplied independently by the tracer.
mod protocol;
pub(super) mod transfer;

use super::timing::TransferTiming;

use crate::{
    network::association::{self, Key, Metadata, Registry, Transport},
    recording::Observation,
};
use serde_json::json;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Context {
    pub connection_id: u64,
    pub key: Key,
    pub metadata: Option<Metadata>,
}
impl Context {
    pub(super) fn event(&self, kind: &'static str, mut data: serde_json::Value) -> Observation {
        data["connection_id"] = self.connection_id.into();
        association::fields(self.key, self.metadata.as_ref(), &mut data);
        Observation {
            source: "tcp_proxy",
            kind,
            data,
        }
    }
}

pub async fn run(
    listeners: Vec<std::net::TcpListener>,
    registry: Registry,
    events: mpsc::Sender<Observation>,
    finished: CancellationToken,
    inspection: Option<Arc<super::http::Inspection>>,
) -> io::Result<()> {
    let ids = Arc::new(AtomicU64::new(1));
    let mut services = JoinSet::new();
    for listener in listeners {
        services.spawn(serve(
            listener,
            registry.clone(),
            events.clone(),
            finished.clone(),
            ids.clone(),
            inspection.clone(),
        ));
    }
    while let Some(result) = services.join_next().await {
        result.map_err(io::Error::other)??;
    }
    if let Some(inspection) = inspection {
        inspection.flush().await?;
    }
    Ok(())
}

async fn serve(
    listener: std::net::TcpListener,
    registry: Registry,
    events: mpsc::Sender<Observation>,
    finished: CancellationToken,
    ids: Arc<AtomicU64>,
    inspection: Option<Arc<super::http::Inspection>>,
) -> io::Result<()> {
    let listener = TcpListener::from_std(listener)?;
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = finished.cancelled() => break,
            accepted = listener.accept() => {
                let (client, source) = accepted?;
                let timing = TransferTiming::begin("proxy_accept");
                let key = Key { transport: Transport::Tcp, source, destination: client.local_addr()? };
                let context = Context { key, connection_id: ids.fetch_add(1, Ordering::Relaxed), metadata: None };
                tasks.spawn(forward(client, context, registry.clone(), events.clone(), finished.clone(), timing, inspection.clone()));
            }
            result = tasks.join_next(), if !tasks.is_empty() => { result.unwrap().map_err(io::Error::other)??; }
        }
    }
    // A fast tracee may exit before the Tokio accept readiness is serviced.
    let listener = listener.into_std()?;
    loop {
        match listener.accept() {
            Ok((client, source)) => {
                let timing = TransferTiming::begin("proxy_accept");
                client.set_nonblocking(true)?;
                let key = Key {
                    transport: Transport::Tcp,
                    source,
                    destination: client.local_addr()?,
                };
                tasks.spawn(forward(
                    TcpStream::from_std(client)?,
                    Context {
                        key,
                        connection_id: ids.fetch_add(1, Ordering::Relaxed),
                        metadata: None,
                    },
                    registry.clone(),
                    events.clone(),
                    finished.clone(),
                    timing,
                    inspection.clone(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }
    drop(listener);
    while let Some(result) = tasks.join_next().await {
        result.map_err(io::Error::other)??;
    }
    Ok(())
}

async fn forward(
    client: TcpStream,
    mut context: Context,
    registry: Registry,
    events: mpsc::Sender<Observation>,
    finished: CancellationToken,
    timing: TransferTiming,
    inspection: Option<Arc<super::http::Inspection>>,
) -> io::Result<()> {
    let key = context.key;
    let metadata = registry.resolve(&key).await;
    if let Some(metadata) = &metadata {
        registry.remove(&key, metadata.socket_id);
    }
    context.metadata = metadata;
    let upstream =
        match tokio::time::timeout(Duration::from_secs(30), TcpStream::connect(key.destination))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "upstream connect timed out",
            )),
        };
    let mut outcome = match upstream {
        Ok(upstream) => {
            events
                .send(context.event("network.upstream", json!({"status":"connected"})))
                .await
                .map_err(io::Error::other)?;
            let result = if let Some(inspection) = inspection {
                super::http::forward(client, upstream, inspection, &context, &events, finished)
                    .await
            } else {
                transfer::copy(client, upstream, finished, &context, &events).await
            };
            match result {
                Ok(result) => {
                    json!({"status":result.reason, "bytes_up":result.up, "bytes_down":result.down})
                }
                Err(error) => {
                    json!({"status":"failed", "error":error.to_string(), "errno":error.raw_os_error(), "bytes_up":null, "bytes_down":null})
                }
            }
        }
        Err(error) => {
            events.send(context.event("network.upstream", json!({"status":"failed", "errno":error.raw_os_error(), "error":error.to_string()}))).await.map_err(io::Error::other)?;
            json!({"status":"failed", "error":error.to_string(), "errno":error.raw_os_error(), "bytes_up":0, "bytes_down":0})
        }
    };
    timing.complete(&mut outcome);
    events
        .send(context.event("network.transfer", outcome))
        .await
        .map_err(io::Error::other)
}
