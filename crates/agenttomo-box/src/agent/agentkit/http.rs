//! Client for the AgentKit application's native session and streaming endpoints.
use axum::{
    body::Bytes,
    http::{Method, Request, Response, header},
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    time::Duration,
};
use tokio::{
    net::TcpStream,
    time::{sleep, timeout},
};
use uuid::Uuid;

#[derive(Clone)]
pub(in crate::agent) struct Session {
    address: SocketAddr,
    pid: u32,
    id: String,
}

impl Session {
    pub(in crate::agent) fn new(address: SocketAddr, pid: u32) -> Self {
        Self {
            address,
            pid,
            id: Uuid::new_v4().to_string(),
        }
    }

    async fn post(&self, path: &str, body: Bytes) -> io::Result<Response<Incoming>> {
        let socket = TcpStream::connect(self.address).await?;
        // A server can exit between turns. Verify ownership after connecting,
        // before writing to a socket that the sandbox gateway may have accepted.
        if !self.listening().await? {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "AgentKit no longer owns its listening socket",
            ));
        }
        let (mut tx, connection) = http1::handshake(TokioIo::new(socket))
            .await
            .map_err(io::Error::other)?;
        tokio::spawn(connection);
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, self.address.to_string())
            .header(header::CONNECTION, "close")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .map_err(io::Error::other)?;
        let response = tx.send_request(request).await.map_err(io::Error::other)?;
        if !response.status().is_success() {
            return Err(io::Error::other(format!(
                "AgentKit {path} returned HTTP {}",
                response.status()
            )));
        }
        Ok(response)
    }

    async fn listening(&self) -> io::Result<bool> {
        // An early connect goes to the transparent proxy and can occupy the port
        // before the SDK binds. Wait for a LISTEN socket owned by the child.
        let loopback = u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets());
        let endpoint = format!("{loopback:08X}:{:04X}", self.address.port());
        let tcp = tokio::fs::read_to_string("/proc/net/tcp").await?;
        let inode = tcp.lines().find_map(|line| {
            let mut fields = line.split_whitespace().skip(1);
            let local = fields.next()?;
            let state = fields.nth(1)?;
            let inode = fields.nth(5)?;
            (local == endpoint && state == "0A").then_some(inode)
        });
        let Some(inode) = inode else { return Ok(false) };
        let socket = format!("socket:[{inode}]");
        let mut fds = tokio::fs::read_dir(format!("/proc/{}/fd", self.pid)).await?;
        while let Some(fd) = fds.next_entry().await? {
            if tokio::fs::read_link(fd.path())
                .await
                .is_ok_and(|path| path == Path::new(&socket))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn create(&self) -> io::Result<()> {
        timeout(Duration::from_secs(20), async {
            while !self.listening().await? {
                sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "AgentKit did not open its listening socket",
            )
        })??;
        let path = format!("/apps/agentkit/users/local/sessions/{}", self.id);
        let response = self.post(&path, Bytes::new()).await?;
        let bytes = Limited::new(response.into_body(), 64 * 1024)
            .collect()
            .await
            .map_err(io::Error::other)?
            .to_bytes();
        let session: Value = serde_json::from_slice(&bytes)?;
        if session["id"] != self.id {
            return Err(io::Error::other(
                "AgentKit did not create the requested session",
            ));
        }
        Ok(())
    }

    pub(in crate::agent) async fn run(&self, prompt: &str, first: bool) -> io::Result<()> {
        if first {
            self.create().await?;
        }
        let body = serde_json::to_vec(&json!({
            "appName":"agentkit", "userId":"local", "sessionId":self.id, "streaming":true,
            "newMessage":{"role":"user","parts":[{"text":prompt}]}
        }))?;
        let response = self.post("/run_sse", body.into()).await?;
        if !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("text/event-stream"))
        {
            return Err(io::Error::other("AgentKit did not return an event stream"));
        }
        let mut body = response.into_body();
        let mut parser = sse_parse::SseParser::new().max_buffer(crate::agent::MAX_CONTENT);
        let mut answered = false;
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.map_err(io::Error::other)?.into_data() {
                parser.feed(&data);
                if parser.overflows() > 0 {
                    return Err(io::Error::other("AgentKit event is too large"));
                }
                while let Some(event) = parser.next_event() {
                    answered |= final_text(&event.event, &event.data)?;
                }
            }
        }
        if !answered {
            return Err(io::Error::other(
                "AgentKit ended without a final text reply",
            ));
        }
        Ok(())
    }
}

pub(super) fn final_text(kind: &str, data: &str) -> io::Result<bool> {
    let event: Value = serde_json::from_str(data)?;
    if kind == "error"
        || event.get("error").is_some()
        || event["errorCode"].as_str().is_some_and(|v| !v.is_empty())
    {
        // Ark errors may include the upstream body. Only expose the HTTP status.
        let diagnostic = event["error"]
            .as_str()
            .or(event["errorMessage"].as_str())
            .unwrap_or("");
        let status = diagnostic
            .split_once("code: ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .or(event["errorCode"].as_str())
            .and_then(|code| code.parse::<u16>().ok())
            .filter(|n| (400..600).contains(n));
        return Err(io::Error::other(match status {
            Some(code) => format!("Model request failed (HTTP {code})"),
            None => "AgentKit reported a model error".into(),
        }));
    }
    Ok(event["partial"] != true
        && event["content"]["role"] == "model"
        && event["content"]["parts"].as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part["thought"] != true
                    && part["text"].as_str().is_some_and(|s| !s.trim().is_empty())
            })
        }))
}
