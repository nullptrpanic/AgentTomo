//! Codex login/config formats and allowed credential endpoints.
mod audit;
mod config;
#[cfg(test)]
mod tests;
use super::super::adapter::Prepared;
use crate::proxy::credentials::{Grant, MARKER, Policy, Scope, Source, marker, secret_header};
#[cfg(test)]
use axum::http::{HeaderMap, Method};
use axum::http::{HeaderName, HeaderValue};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

struct Login {
    path: PathBuf,
    account: Option<String>,
    chatgpt: bool,
}
impl Login {
    fn header(&self) -> io::Result<HeaderValue> {
        let Self {
            path,
            account,
            chatgpt,
        } = self;
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
        let auth: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid host Codex login"))?;
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
                .is_some_and(|expiry| expiry <= time::OffsetDateTime::now_utc().unix_timestamp())
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

fn provider_scope(base: &str) -> io::Result<Scope> {
    Scope::new(
        base,
        &[
            ("GET", "models"),
            ("HEAD", "models"),
            ("GET", "responses"),
            ("POST", "responses"),
            ("POST", "responses/compact"),
        ],
    )
}
fn chatgpt_scope(base: &str) -> io::Result<Scope> {
    Scope::new(
        base,
        &[
            ("GET", "codex/models"),
            ("HEAD", "codex/models"),
            ("GET", "codex/responses"),
            ("POST", "codex/responses"),
            ("POST", "codex/responses/compact"),
            ("GET", "wham/accounts/check"),
            ("HEAD", "wham/accounts/check"),
            ("GET", "wham/settings/user"),
            ("HEAD", "wham/settings/user"),
            ("GET", "plugins/featured"),
            ("HEAD", "plugins/featured"),
            ("GET", "ps/plugins/installed"),
            ("HEAD", "ps/plugins/installed"),
            ("GET", "ps/plugins/list"),
            ("HEAD", "ps/plugins/list"),
            ("GET", "ps/plugins/suggested/codex"),
            ("HEAD", "ps/plugins/suggested/codex"),
            ("POST", "ps/mcp"),
            ("POST", "codex/analytics-events/events"),
        ],
    )
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
        let source = source.unwrap_or(Source::Static(secret_header(value, bearer)?));
        let marker = placeholder.unwrap_or_else(marker);
        let placeholder = secret_header(&marker, bearer)?;
        for scope in scopes {
            self.grants.push(Grant::new(
                scope.clone(),
                name.clone(),
                placeholder.clone(),
                source.clone(),
            )?);
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
        provider_scope(base)?;
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
            let source = Login {
                path: home.join("auth.json"),
                account: tokens["account_id"].as_str().map(str::to_owned),
                chatgpt: true,
            };
            let access = builder.bind(
                &scopes.chatgpt,
                "authorization",
                access,
                true,
                Some(Source::Reload(std::sync::Arc::new(move || source.header()))),
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
            let source = Login {
                path: home.join("auth.json"),
                account: None,
                chatgpt: false,
            };
            sanitized["OPENAI_API_KEY"] = builder
                .bind(
                    &scopes.keys,
                    "authorization",
                    key,
                    true,
                    Some(Source::Reload(std::sync::Arc::new(move || source.header()))),
                    None,
                )?
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
        policy: Policy::new(builder.grants),
    })
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
