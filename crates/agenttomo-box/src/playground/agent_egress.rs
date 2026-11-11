//! Reach a host-loopback proxy from owned Docker sessions without host networking.
use crate::agent::codex::credentials::gateway::Gateway;
use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::{
    net::{TcpListener, TcpStream},
    process::Command,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

const LOCAL_SETTINGS: &str = ".agenttomo/playground.json";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalSettings {
    proxy: Option<String>,
}

async fn configured_environment(
    path: &std::path::Path,
    inherited: Vec<(String, String)>,
) -> io::Result<Vec<(String, String)>> {
    let explicit = inherited
        .iter()
        .any(|(name, _)| !name.eq_ignore_ascii_case("no_proxy"));
    let mut variables: Vec<_> = inherited
        .into_iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .collect();
    // An explicitly empty proxy variable disables the local fallback as well.
    if explicit {
        return Ok(variables);
    }
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(variables),
        Err(error) => {
            return Err(io::Error::other(format!(
                "cannot read {}: {error}",
                path.display()
            )));
        }
    };
    let settings: LocalSettings = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::other(format!("invalid {}: {error}", path.display())))?;
    if let Some(proxy) = settings.proxy {
        if proxy.trim().is_empty() {
            return Err(io::Error::other("configured Playground proxy is empty"));
        }
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            variables.push((name.to_owned(), proxy.clone()));
        }
        eprintln!("AgentTomo: using proxy settings from {}", path.display());
    }
    Ok(variables)
}

#[derive(Clone, Default)]
pub struct Peers(Arc<Mutex<HashMap<String, HashSet<IpAddr>>>>);
impl Peers {
    pub async fn register(&self, container: &str) -> io::Result<()> {
        let output = Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
                container,
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Err(io::Error::other("cannot inspect the owned sandbox network"));
        }
        let ips = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<HashSet<IpAddr>, _>>()
            .map_err(io::Error::other)?;
        self.0.lock().unwrap().insert(container.to_owned(), ips);
        Ok(())
    }
    pub fn remove(&self, container: &str) {
        self.0.lock().unwrap().remove(container);
    }
    fn allows(&self, ip: IpAddr) -> bool {
        self.0
            .lock()
            .unwrap()
            .values()
            .any(|peers| peers.contains(&ip))
    }
    fn owner(&self, ip: IpAddr) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(_, peers)| peers.contains(&ip))
            .map(|(owner, _)| owner.clone())
    }
}

pub async fn configure(
    explicit: Option<crate::proxy::outbound::Proxy>,
    disabled: bool,
    peers: Peers,
    shutdown: CancellationToken,
    gateway: Option<Arc<Gateway>>,
) -> io::Result<Option<crate::proxy::outbound::Proxy>> {
    let proxy = if disabled {
        None
    } else if let Some(proxy) = explicit {
        Some(proxy)
    } else {
        let inherited: Vec<_> = crate::proxy::outbound::ENVIRONMENT_NAMES
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect();
        let has_override = inherited
            .iter()
            .any(|(name, _)| !name.eq_ignore_ascii_case("no_proxy"));
        let configured =
            configured_environment(std::path::Path::new(LOCAL_SETTINGS), inherited).await?;
        let urls: HashSet<_> = configured
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("no_proxy"))
            .map(|(_, value)| value.as_str())
            .collect();
        if urls.len() > 1 {
            return Err(io::Error::other(
                "conflicting proxy variables; choose one box upstream with --proxy",
            ));
        }
        let selected = urls
            .into_iter()
            .next()
            .map(str::parse::<crate::proxy::outbound::Proxy>)
            .transpose()
            .map_err(io::Error::other)?;
        // Preserve an explicitly empty inherited override as direct mode.
        selected.or_else(|| (!has_override).then(Default::default))
    };
    if let Some(gateway) = gateway {
        let listener = TcpListener::bind(SocketAddr::new(bridge_gateway().await?, 0)).await?;
        let address = listener.local_addr()?;
        tokio::spawn(credential_relay(
            listener,
            proxy,
            peers,
            gateway,
            shutdown.child_token(),
        ));
        return Ok(Some(
            format!("http://{address}")
                .parse()
                .map_err(io::Error::other)?,
        ));
    }
    let Some(proxy) = proxy else { return Ok(None) };
    let host = proxy.host();
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return Ok(Some(proxy));
    }
    let ip = host
        .parse()
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let target = SocketAddr::new(ip, proxy.port());
    let listener = TcpListener::bind(SocketAddr::new(bridge_gateway().await?, 0)).await?;
    let address = listener.local_addr()?;
    tokio::spawn(relay(listener, target, peers, shutdown.child_token()));
    Ok(Some(
        format!("http://{address}")
            .parse()
            .map_err(io::Error::other)?,
    ))
}

async fn credential_relay(
    listener: TcpListener,
    proxy: Option<crate::proxy::outbound::Proxy>,
    peers: Peers,
    gateway: Arc<Gateway>,
    shutdown: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((client, source)) = accepted else { break };
                let Some(owner) = peers.owner(source.ip()) else { continue };
                connections.spawn(gateway.clone().serve(client, owner, proxy.clone(), shutdown.child_token()));
            },
            _ = connections.join_next(), if !connections.is_empty() => {},
        }
    }
}

async fn bridge_gateway() -> io::Result<IpAddr> {
    let output = Command::new("docker")
        .args([
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{(index .IPAM.Config 0).Gateway}}",
        ])
        .output()
        .await?;
    if !output.status.success() {
        return Err(io::Error::other(
            "cannot find Docker bridge gateway for the host proxy",
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(io::Error::other)
}
async fn relay(
    listener: TcpListener,
    target: SocketAddr,
    peers: Peers,
    shutdown: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((mut client, source)) = accepted else { break };
                if !peers.allows(source.ip()) { continue }
                connections.spawn(async move {
                    if let Ok(Ok(mut upstream)) = tokio::time::timeout(std::time::Duration::from_secs(15), TcpStream::connect(target)).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                });
            },
            _ = connections.join_next(), if !connections.is_empty() => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_proxy_survives_a_clean_shell_and_explicit_environment_wins() {
        let path =
            std::env::temp_dir().join(format!("agenttomo-proxy-{}.json", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, br#"{"proxy":"http://127.0.0.1:8118"}"#)
            .await
            .unwrap();
        let configured =
            configured_environment(&path, vec![("NO_PROXY".into(), "localhost".into())])
                .await
                .unwrap();
        let overridden = configured_environment(
            &path,
            vec![("HTTPS_PROXY".into(), "http://example.test:8888".into())],
        )
        .await
        .unwrap();
        let disabled = configured_environment(&path, vec![("HTTP_PROXY".into(), String::new())])
            .await
            .unwrap();
        tokio::fs::write(&path, b"{").await.unwrap();
        let invalid = configured_environment(&path, Vec::new()).await;
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(invalid.unwrap_err().to_string().contains("invalid"));
        assert!(configured.contains(&("HTTPS_PROXY".into(), "http://127.0.0.1:8118".into())));
        assert!(configured.contains(&("HTTP_PROXY".into(), "http://127.0.0.1:8118".into())));
        assert!(configured.contains(&("ALL_PROXY".into(), "http://127.0.0.1:8118".into())));
        assert!(configured.contains(&("NO_PROXY".into(), "localhost".into())));
        assert_eq!(
            overridden,
            vec![("HTTPS_PROXY".into(), "http://example.test:8888".into())]
        );
        assert!(disabled.is_empty());
        assert!(
            configured_environment(&path, Vec::new())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
