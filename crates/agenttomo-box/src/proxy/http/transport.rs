//! Shared HTTP framing, connection lifecycle and upgraded byte streams.
use super::HttpBody;
use crate::proxy::BoxIo;
use axum::{
    body::Bytes,
    http::{Request, Response},
};
use hyper::{
    body::{Body, Incoming},
    client::conn::http1::SendRequest,
    service::service_fn,
    upgrade::OnUpgrade,
};
use hyper_util::rt::TokioIo;
use std::{future::Future, io, sync::Arc, time::Duration};
use tokio::{sync::Mutex, time::timeout};
use tokio_util::{
    sync::CancellationToken,
    task::{AbortOnDropHandle, TaskTracker},
};

pub use super::io::opaque;

pub async fn send<B>(
    sender: Arc<Mutex<SendRequest<B>>>,
    request: Request<B>,
) -> Result<Response<Incoming>, hyper::Error>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut sender = sender.lock().await;
    sender.ready().await?;
    sender.send_request(request).await
}

pub async fn bridge<B, F, Fut>(
    client: BoxIo,
    upstream: BoxIo,
    finished: CancellationToken,
    writes: TaskTracker,
    handler: F,
) -> io::Result<()>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    F: Fn(Request<Incoming>, Arc<Mutex<SendRequest<B>>>) -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<Response<HttpBody>>> + Send + 'static,
{
    let (sender, connection) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .handshake::<_, B>(TokioIo::new(upstream))
        .await
        .map_err(io::Error::other)?;
    let driver = AbortOnDropHandle::new(tokio::spawn(connection.with_upgrades()));
    let sender = Arc::new(Mutex::new(sender));
    let service = service_fn(move |request| handler(request, sender.clone()));
    let connection = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .half_close(true)
        .serve_connection(TokioIo::new(client), service)
        .with_upgrades();
    let mut connection = Box::pin(connection);
    let result = tokio::select! {
        biased;
        result = &mut connection => result.map_err(io::Error::other),
        _ = finished.cancelled() => {
            connection.as_mut().graceful_shutdown();
            timeout(Duration::from_secs(5), &mut connection).await.map_err(io::Error::other).and_then(|result| result.map_err(io::Error::other))
        },
    };
    driver.abort();
    let _ = driver.await;
    // Drop body observers before flushing their capture/upgrade jobs.
    drop(connection);
    writes.close();
    writes.wait().await;
    result
}

pub async fn upgraded(client: OnUpgrade, upstream: OnUpgrade) -> io::Result<(BoxIo, BoxIo)> {
    let (client, upstream) = timeout(Duration::from_secs(5), async {
        tokio::try_join!(client, upstream)
    })
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    Ok((
        Box::new(TokioIo::new(client)),
        Box::new(TokioIo::new(upstream)),
    ))
}
