//! Codex authentication policy. Only sanitized files cross the Docker boundary.
mod audit;
mod config;
pub mod gateway;
#[cfg(test)]
mod tests;

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri, uri::Authority};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

const MARKER: &str = "agenttomo-credential-";
pub struct Prepared {
    pub files: BTreeMap<String, String>,
    pub policy: Policy,
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
struct Scope {
    origin: Origin,
    path: String,
    chatgpt_backend: bool,
}
impl Scope {
    fn parse(base: &str) -> io::Result<Self> {
        let uri: Uri = base
            .parse()
            .map_err(|_| invalid("invalid Codex provider URL"))?;
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => {
                return Err(invalid(
                    "Codex credential forwarding requires HTTP or HTTPS",
                ));
            }
        };
        let authority = uri
            .authority()
            .ok_or_else(|| invalid("missing Codex provider authority"))?;
        if authority.as_str().contains('@') || uri.query().is_some() {
            return Err(invalid(
                "credentials in provider URLs are unsupported; use authentication headers",
            ));
        }
        if authority.port_u16() == Some(0)
            || (authority.port_u16().is_none()
                && authority.as_str().len() != authority.host().len())
        {
            return Err(invalid("invalid Codex provider port"));
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
            path: uri.path().trim_end_matches('/').to_owned(),
            chatgpt_backend: false,
        })
    }
    fn chatgpt_backend(base: &str) -> io::Result<Self> {
        let mut scope = Self::parse(base)?;
        scope.chatgpt_backend = true;
        Ok(scope)
    }
    fn permits(&self, origin: &Origin, method: &Method, path: &str) -> bool {
        if self.origin != *origin {
            return false;
        }
        let Some(relative) = path
            .strip_prefix(&self.path)
            .and_then(|suffix| suffix.strip_prefix('/'))
        else {
            return false;
        };
        // These are the model operations for which this adapter holds credentials.
        // Other endpoints on the same host cannot borrow the authentication grant.
        let backend = self.chatgpt_backend;
        let model = if backend {
            relative.strip_prefix("codex/").unwrap_or("")
        } else {
            relative
        };
        matches!(
            (method.as_str(), model),
            ("GET" | "HEAD", "models")
                | ("GET" | "POST", "responses")
                | ("POST", "responses/compact")
        ) || backend
            && matches!(
                (method.as_str(), relative),
                (
                    "GET" | "HEAD",
                    "wham/accounts/check"
                        | "wham/settings/user"
                        | "plugins/featured"
                        | "ps/plugins/installed"
                        | "ps/plugins/list"
                        | "ps/plugins/suggested/codex"
                ) | ("POST", "ps/mcp" | "codex/analytics-events/events")
            )
    }
}

#[derive(Clone)]
enum Source {
    Static(HeaderValue),
    Login {
        path: PathBuf,
        account: Option<String>,
        chatgpt: bool,
    },
}
impl Source {
    fn header(&self) -> io::Result<HeaderValue> {
        match self {
            Self::Static(value) => Ok(value.clone()),
            Self::Login {
                path,
                account,
                chatgpt,
            } => {
                use std::io::Read;
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .and_then(|file| file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes))
                    .map_err(|_| {
                        invalid("cannot read the host Codex login; sign in on the host and retry")
                    })?;
                if bytes.len() > 16 * 1024 * 1024 {
                    return Err(invalid("host Codex login exceeds the size limit"));
                }
                let auth: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| invalid("invalid host Codex login"))?;
                let value = if *chatgpt {
                    if auth["tokens"]["account_id"].as_str() != account.as_deref() {
                        return Err(invalid(
                            "host Codex account changed; open a new Playground session",
                        ));
                    }
                    let token = text(&auth["tokens"]["access_token"])?;
                    if jwt_claims(token)
                        .ok()
                        .and_then(|claims| claims["exp"].as_i64())
                        .is_some_and(|expiry| {
                            expiry <= time::OffsetDateTime::now_utc().unix_timestamp()
                        })
                    {
                        return Err(invalid(
                            "host Codex authentication has expired; sign in on the host and retry",
                        ));
                    }
                    token
                } else {
                    text(&auth["OPENAI_API_KEY"])?
                };
                secret_header(value, true)
            }
        }
    }
}
struct Grant {
    scope: Scope,
    name: HeaderName,
    placeholder: HeaderValue,
    source: Source,
}

pub struct Policy {
    grants: Vec<Grant>,
    pub lifetime: CancellationToken,
}
impl Policy {
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

struct Builder {
    grants: Vec<Grant>,
    host_environment: BTreeMap<String, String>,
    environment: BTreeMap<String, String>,
    secrets: Vec<String>,
}
impl Builder {
    fn bind(
        &mut self,
        scopes: &[Scope],
        name: &str,
        value: &str,
        bearer: bool,
        source: Option<Source>,
        placeholder: Option<String>,
    ) -> io::Result<String> {
        let name: HeaderName = name
            .parse()
            .map_err(|_| invalid("invalid Codex credential header name"))?;
        if !value.is_empty() {
            self.secrets.push(value.to_owned());
        }
        if name == "authorization"
            && let Some((_, token)) = value.split_once(' ')
            && !token.trim().is_empty()
        {
            self.secrets.push(token.trim().to_owned());
        }
        if matches!(
            name.as_str(),
            "host" | "connection" | "upgrade" | "content-length" | "transfer-encoding"
        ) {
            return Err(invalid(
                "transport headers cannot be used for Codex credentials",
            ));
        }
        let source = source.unwrap_or(Source::Static(secret_header(value, bearer)?));
        let marker = placeholder.unwrap_or_else(marker);
        let placeholder = secret_header(&marker, bearer)?;
        for scope in scopes {
            self.grants.push(Grant {
                scope: scope.clone(),
                name: name.clone(),
                placeholder: placeholder.clone(),
                source: source.clone(),
            });
        }
        Ok(marker)
    }
    fn environment(
        &mut self,
        scopes: &[Scope],
        header: &str,
        name: &str,
        bearer: bool,
    ) -> io::Result<()> {
        let Some(value) = self
            .host_environment
            .get(name)
            .filter(|value| !value.is_empty())
            .cloned()
        else {
            return Ok(());
        };
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || matches!(
                name,
                "PATH" | "HOME" | "CODEX_HOME" | "SSL_CERT_FILE" | "CODEX_CA_CERTIFICATE"
            )
            || crate::proxy::outbound::ENVIRONMENT_NAMES.contains(&name)
        {
            return Err(invalid(
                "Codex authentication references a reserved environment variable",
            ));
        }
        let placeholder = self.environment.get(name).cloned();
        if config::metadata_header(header) {
            secret_header(&value, false)?;
            self.environment.insert(name.to_owned(), value);
            return Ok(());
        }
        let placeholder = self.bind(scopes, header, &value, bearer, None, placeholder)?;
        self.environment.insert(name.to_owned(), placeholder);
        Ok(())
    }
}

pub fn prepare(
    home: &Path,
    mut files: BTreeMap<String, String>,
    environment: BTreeMap<String, String>,
) -> io::Result<Prepared> {
    let mut builder = Builder {
        grants: Vec::new(),
        host_environment: environment,
        environment: BTreeMap::new(),
        secrets: Vec::new(),
    };
    let scopes = config::sanitize(&mut files, &mut builder)?;
    for name in ["OPENAI_API_KEY", "CODEX_API_KEY"] {
        builder.environment(&scopes.keys, "authorization", name, true)?;
    }
    if let Some(base) = builder.host_environment.get("OPENAI_BASE_URL") {
        Scope::parse(base)?;
        builder
            .environment
            .insert("OPENAI_BASE_URL".into(), base.clone());
    }
    if let Some(contents) = files.get_mut("auth.json") {
        let auth: Value =
            serde_json::from_str(contents).map_err(|_| invalid("invalid host Codex auth.json"))?;
        for value in [
            &auth["OPENAI_API_KEY"],
            &auth["tokens"]["access_token"],
            &auth["tokens"]["refresh_token"],
            &auth["tokens"]["id_token"],
        ] {
            if let Some(secret) = value.as_str().filter(|value| !value.is_empty()) {
                builder.secrets.push(secret.to_owned());
            }
        }
        if [
            "agent_identity",
            "personal_access_token",
            "bedrock_api_key",
            "bedrock_access_keys",
        ]
        .iter()
        .any(|name| !auth[name].is_null())
        {
            return Err(invalid(
                "Codex credential isolation currently supports ChatGPT login and API keys",
            ));
        }
        let mut sanitized = json!({"OPENAI_API_KEY":null});
        if auth["auth_mode"] == "chatgpt"
            || (auth["auth_mode"].is_null() && auth["tokens"].is_object())
        {
            let tokens = &auth["tokens"];
            if !tokens["account_id"].is_null() && !tokens["account_id"].is_string() {
                return Err(invalid("invalid Codex account metadata"));
            }
            let access = text(&tokens["access_token"])?;
            let id = text(&tokens["id_token"])?;
            let mut claims = jwt_claims(id)?;
            if !claims.is_object()
                || (!claims["https://api.openai.com/auth"].is_null()
                    && !claims["https://api.openai.com/auth"].is_object())
            {
                return Err(invalid("invalid Codex identity metadata"));
            }
            let mut metadata = serde_json::Map::new();
            for name in [
                "chatgpt_plan_type",
                "chatgpt_user_id",
                "chatgpt_account_id",
                "chatgpt_account_is_fedramp",
                "user_id",
            ] {
                if let Some(value) = claims["https://api.openai.com/auth"].get(name) {
                    let valid = if name == "chatgpt_account_is_fedramp" {
                        value.is_boolean()
                    } else {
                        value.is_null() || value.is_string()
                    };
                    if !valid {
                        return Err(invalid("invalid Codex identity metadata"));
                    }
                    metadata.insert(name.into(), value.clone());
                }
            }
            claims = json!({"exp":time::OffsetDateTime::now_utc().unix_timestamp() + 3650 * 86400_i64,"https://api.openai.com/auth":metadata});
            let id = format!(
                "{}.{}.{}",
                URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?),
                marker()
            );
            let source = Source::Login {
                path: home.join("auth.json"),
                account: tokens["account_id"].as_str().map(str::to_owned),
                chatgpt: true,
            };
            let access = builder.bind(
                &scopes.chatgpt,
                "authorization",
                access,
                true,
                Some(source),
                // A future expiry keeps Codex from attempting to refresh the
                // placeholder. Real token expiry is checked by the host source.
                Some(id.clone()),
            )?;
            sanitized["auth_mode"] = "chatgpt".into();
            sanitized["tokens"] = json!({"id_token":id,"access_token":access,"refresh_token":marker(),"account_id":tokens["account_id"]});
            sanitized["last_refresh"] = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(io::Error::other)?
                .into();
        } else if let Some(key) = auth["OPENAI_API_KEY"]
            .as_str()
            .filter(|key| !key.is_empty())
        {
            let source = Source::Login {
                path: home.join("auth.json"),
                account: None,
                chatgpt: false,
            };
            sanitized["OPENAI_API_KEY"] = builder
                .bind(&scopes.keys, "authorization", key, true, Some(source), None)?
                .into();
        } else {
            return Err(invalid(
                "unsupported or empty host Codex authentication record",
            ));
        }
        *contents = serde_json::to_string(&sanitized)?;
    }
    files.insert(
        "agenttomo-environment.json".into(),
        serde_json::to_string(&builder.environment.into_iter().collect::<Vec<_>>())?,
    );
    audit::verify(&files, &builder.secrets)?;
    Ok(Prepared {
        files,
        policy: Policy {
            grants: builder.grants,
            lifetime: CancellationToken::new(),
        },
    })
}

fn marker() -> String {
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
fn text(value: &Value) -> io::Result<&str> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("incomplete host Codex authentication record"))
}
fn jwt_claims(token: &str) -> io::Result<Value> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| invalid("invalid Codex identity token"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| invalid("invalid Codex identity token"))?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("invalid Codex identity token"))
}
fn secret_header(value: &str, bearer: bool) -> io::Result<HeaderValue> {
    let value = if bearer {
        format!("Bearer {value}")
    } else {
        value.to_owned()
    };
    let mut value = HeaderValue::from_str(&value)
        .map_err(|_| invalid("invalid Codex credential header value"))?;
    value.set_sensitive(true);
    Ok(value)
}
