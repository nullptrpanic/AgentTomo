//! Host-side credential grants and forwarding, independent of any agent format.
pub mod gateway;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, uri::Authority};
use std::{io, sync::Arc};
use tokio_util::sync::CancellationToken;

pub const MARKER: &str = "agenttomo-credential-";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn adapter_supplied_operations_and_reload_sources_are_scoped_and_revocable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reader = calls.clone();
        let placeholder = secret_header(&marker(), false).unwrap();
        let policy = Policy::new(vec![
            Grant::new(
                Scope::new("https://models.example.test/api", &[("POST", "messages")]).unwrap(),
                "x-api-key".parse().unwrap(),
                placeholder.clone(),
                Source::Reload(Arc::new(move || {
                    secret_header(
                        &format!("secret-{}", reader.fetch_add(1, Ordering::SeqCst)),
                        false,
                    )
                })),
            )
            .unwrap(),
        ]);
        let origin = policy
            .origin(&"models.example.test:443".parse().unwrap())
            .unwrap();
        let headers = || {
            let mut headers = HeaderMap::new();
            headers.insert("host", "models.example.test".parse().unwrap());
            headers.insert("x-api-key", placeholder.clone());
            headers
        };
        for index in 0..2 {
            let mut request = headers();
            policy
                .inject(
                    &origin,
                    &Method::POST,
                    &"/api/messages".parse().unwrap(),
                    &mut request,
                )
                .unwrap();
            assert_eq!(request["x-api-key"], format!("secret-{index}"));
            assert!(request["x-api-key"].is_sensitive());
        }
        for (method, path) in [
            (Method::GET, "/api/messages"),
            (Method::POST, "/api/responses"),
            (Method::POST, "/api/messages/extra"),
        ] {
            let mut request = headers();
            assert!(
                policy
                    .inject(&origin, &method, &path.parse().unwrap(), &mut request)
                    .is_err()
            );
            assert_eq!(request["x-api-key"], placeholder);
        }
        policy.lifetime.cancel();
        assert!(
            policy
                .inject(
                    &origin,
                    &Method::POST,
                    &"/api/messages".parse().unwrap(),
                    &mut headers()
                )
                .is_err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "rejected requests must not load host secrets"
        );
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct Origin {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}
impl Origin {
    pub fn matches(&self, authority: &Authority) -> bool {
        let host = authority.host().trim_matches(['[', ']']);
        let port = authority.port_u16();
        !authority.as_str().contains('@')
            && self.host.eq_ignore_ascii_case(host)
            && (port.is_some() || authority.as_str().len() == authority.host().len())
            && port.unwrap_or(if self.tls { 443 } else { 80 }) == self.port
    }
}

#[derive(Clone)]
pub struct Scope {
    origin: Origin,
    operations: Vec<(Method, String)>,
}
impl Scope {
    pub fn new(base: &str, operations: &[(&str, &str)]) -> io::Result<Self> {
        let uri: Uri = base
            .parse()
            .map_err(|_| invalid("invalid credential provider URL"))?;
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => {
                return Err(invalid("credential forwarding requires HTTP or HTTPS"));
            }
        };
        let authority = uri
            .authority()
            .ok_or_else(|| invalid("missing credential provider authority"))?;
        if authority.as_str().contains('@') || uri.query().is_some() {
            return Err(invalid(
                "credentials in provider URLs are unsupported; use authentication headers",
            ));
        }
        if authority.port_u16() == Some(0)
            || (authority.port_u16().is_none()
                && authority.as_str().len() != authority.host().len())
        {
            return Err(invalid("invalid credential provider port"));
        }
        Ok(Self {
            origin: Origin {
                host: authority
                    .host()
                    .trim_matches(['[', ']'])
                    .to_ascii_lowercase(),
                port: authority.port_u16().unwrap_or(if tls { 443 } else { 80 }),
                tls,
            },
            operations: operations
                .iter()
                .map(|(method, path)| {
                    Ok((
                        method
                            .parse::<Method>()
                            .map_err(|_| invalid("invalid credential method"))?,
                        format!("{}/{}", uri.path().trim_end_matches('/'), path),
                    ))
                })
                .collect::<io::Result<Vec<_>>>()?,
        })
    }
    fn permits(&self, origin: &Origin, method: &Method, path: &str) -> bool {
        self.origin == *origin
            && self
                .operations
                .iter()
                .any(|(allowed, target)| allowed == method && target == path)
    }
}

#[derive(Clone)]
pub enum Source {
    Static(HeaderValue),
    Reload(Arc<dyn Fn() -> io::Result<HeaderValue> + Send + Sync>),
}
impl Source {
    fn header(&self) -> io::Result<HeaderValue> {
        match self {
            Self::Static(value) => Ok(value.clone()),
            Self::Reload(read) => read(),
        }
    }
}
pub struct Grant {
    scope: Scope,
    name: HeaderName,
    placeholder: HeaderValue,
    source: Source,
}
impl Grant {
    pub fn new(
        scope: Scope,
        name: HeaderName,
        placeholder: HeaderValue,
        source: Source,
    ) -> io::Result<Self> {
        if matches!(
            name.as_str(),
            "host" | "connection" | "upgrade" | "content-length" | "transfer-encoding"
        ) {
            return Err(invalid("transport headers cannot be used for credentials"));
        }
        Ok(Self {
            scope,
            name,
            placeholder,
            source,
        })
    }
}

pub struct Policy {
    grants: Vec<Grant>,
    pub lifetime: CancellationToken,
}
impl Policy {
    pub fn new(grants: Vec<Grant>) -> Self {
        Self {
            grants,
            lifetime: CancellationToken::new(),
        }
    }
    pub fn origin(&self, authority: &Authority) -> Option<Origin> {
        self.grants
            .iter()
            .find(|grant| grant.scope.origin.matches(authority))
            .map(|grant| grant.scope.origin.clone())
    }

    /// Validate all routing information before exposing a host header upstream.
    pub fn inject(
        &self,
        origin: &Origin,
        method: &Method,
        uri: &Uri,
        headers: &mut HeaderMap,
    ) -> io::Result<()> {
        if self.lifetime.is_cancelled() {
            return Err(invalid("Playground credential session has ended"));
        }
        let host = headers.get_all("host");
        let mut hosts = host.iter();
        let authority = hosts
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<Authority>().ok());
        if hosts.next().is_some()
            || !authority
                .as_ref()
                .is_some_and(|value| origin.matches(value))
            || uri.authority().is_some_and(|value| !origin.matches(value))
            || uri
                .scheme_str()
                .is_some_and(|scheme| scheme != if origin.tls { "https" } else { "http" })
        {
            return Err(invalid(
                "credential request authority does not match the connection",
            ));
        }
        let path = uri.path();
        let encoded = path.to_ascii_lowercase();
        if path.split('/').any(|part| matches!(part, "." | ".."))
            || path.contains('\\')
            || ["%2e", "%2f", "%5c"]
                .iter()
                .any(|value| encoded.contains(value))
        {
            return Err(invalid("credential request contains an ambiguous path"));
        }
        let mut replacements = Vec::new();
        for (name, value) in headers.iter() {
            if !is_placeholder(value) {
                continue;
            }
            if headers.get_all(name).iter().count() != 1 {
                return Err(invalid("duplicate credential request header"));
            }
            let grant = self
                .grants
                .iter()
                .find(|grant| {
                    grant.name == *name
                        && grant.placeholder == *value
                        && grant.scope.permits(origin, method, path)
                })
                .ok_or_else(|| {
                    invalid("credential placeholder is outside its session or provider scope")
                })?;
            replacements.push((name.clone(), grant.source.header()?));
        }
        for (name, value) in replacements {
            headers.insert(name, value);
        }
        Ok(())
    }
}

pub fn marker() -> String {
    format!("{MARKER}{}", uuid::Uuid::new_v4().simple())
}
fn is_placeholder(value: &HeaderValue) -> bool {
    value
        .as_bytes()
        .windows(MARKER.len())
        .any(|part| part == MARKER.as_bytes())
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
pub fn secret_header(value: &str, bearer: bool) -> io::Result<HeaderValue> {
    let value = if bearer {
        format!("Bearer {value}")
    } else {
        value.to_owned()
    };
    let mut value =
        HeaderValue::from_str(&value).map_err(|_| invalid("invalid credential header value"))?;
    value.set_sensitive(true);
    Ok(value)
}
