//! In-process TCP forwarding. No ptrace, target memory or file-collector code.

mod transfer;

use crate::recording::Observation;
use serde_json::json;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
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
    pub tid: i32,
    pub task_id: u64,
    pub destination: SocketAddr,
}

impl Context {
    fn event(&self, kind: &'static str, mut data: serde_json::Value) -> Observation {
        data["connection_id"] = self.connection_id.into();
        data["tid"] = self.tid.into();
        data["task_id"] = self.task_id.into();
        data["destination"] = self.destination.to_string().into();
        Observation {
            source: "tcp_proxy",
            kind,
            data,
        }
    }
}

struct Route {
    context: Context,
    listener: std::net::TcpListener,
    cancel: CancellationToken,
}

pub struct Handle(mpsc::UnboundedSender<Route>);

pub struct Lease {
    pub address: SocketAddr,
    cancel: Option<CancellationToken>,
}

impl Lease {
    pub fn commit(&mut self) {
        self.cancel.take();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }
}

pub struct Service {
    requests: mpsc::UnboundedReceiver<Route>,
    events: mpsc::Sender<Observation>,
}

pub fn channel() -> (Handle, Service, mpsc::Receiver<Observation>) {
    let (sender, requests) = mpsc::unbounded_channel();
    let (events, receiver) = mpsc::channel(256);
    (Handle(sender), Service { requests, events }, receiver)
}

impl Handle {
    pub fn route(&self, context: Context) -> io::Result<Lease> {
        // Only a local bind happens in the caller; no upstream I/O can stall it.
        let ip = if context.destination.is_ipv4() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        };
        let listener = std::net::TcpListener::bind(SocketAddr::new(ip, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let cancel = CancellationToken::new();
        self.0
            .send(Route {
                context,
                listener,
                cancel: cancel.clone(),
            })
            .map_err(|_| io::Error::other("TCP proxy stopped"))?;
        Ok(Lease {
            address,
            cancel: Some(cancel),
        })
    }
}

impl Service {
    pub async fn run(mut self, scope_finished: CancellationToken) -> io::Result<()> {
        let mut tasks = JoinSet::new();
        let mut requests_open = true;
        loop {
            tokio::select! {
                route = self.requests.recv(), if requests_open => match route {
                    Some(route) => { tasks.spawn(forward(route, self.events.clone(), scope_finished.clone())); }
                    None => requests_open = false,
                },
                result = tasks.join_next(), if !tasks.is_empty() => { result.unwrap().map_err(io::Error::other)??; },
                else => break,
            }
        }
        Ok(())
    }
}

async fn forward(
    route: Route,
    events: mpsc::Sender<Observation>,
    scope_finished: CancellationToken,
) -> io::Result<()> {
    let context = &route.context;
    let work = async {
        let listener = TcpListener::from_std(route.listener)?;
        let (client, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "proxy accept timed out"))??;
        drop(listener);
        let upstream = match tokio::time::timeout(
            Duration::from_secs(30),
            TcpStream::connect(context.destination),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "upstream connect timed out",
            )),
        };
        match upstream {
            Ok(upstream) => {
                events
                    .send(context.event("network.upstream", json!({"status":"connected"})))
                    .await
                    .map_err(io::Error::other)?;
                let result = transfer::copy(client, upstream, scope_finished).await?;
                Ok(json!({"status":result.reason, "bytes_up":result.up, "bytes_down":result.down}))
            }
            Err(error) => {
                events.send(context.event("network.upstream", json!({"status":"failed","errno":error.raw_os_error(),"error":error.to_string()}))).await.map_err(io::Error::other)?;
                Err(error)
            }
        }
    };
    let outcome = tokio::select! {
        result = work => match result {
            Ok(data) => data,
            Err(error) => json!({"status":"failed","error":error.to_string(),"errno":error.raw_os_error(),"bytes_up":null,"bytes_down":null}),
        },
        _ = route.cancel.cancelled() => json!({"status":"cancelled","bytes_up":null,"bytes_down":null}),
    };
    events
        .send(context.event("network.transfer", outcome))
        .await
        .map_err(io::Error::other)
}
