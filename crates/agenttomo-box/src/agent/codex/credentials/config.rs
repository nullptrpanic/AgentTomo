//! Provider credential fields are interpreted only in the Codex adapter.
use super::{Builder, Scope, chatgpt_scope, invalid, provider_scope};
use axum::http::Uri;
use std::{collections::BTreeMap, io};

pub(super) struct Scopes {
    pub keys: Vec<Scope>,
    pub chatgpt: Vec<Scope>,
}

pub(super) fn sanitize(
    files: &mut BTreeMap<String, String>,
    builder: &mut Builder,
) -> io::Result<Scopes> {
    let base = builder
        .host_environment
        .get("OPENAI_BASE_URL")
        .filter(|value| !value.trim().is_empty())
        .map(String::as_str)
        .unwrap_or("https://api.openai.com/v1");
    let mut scopes = Scopes {
        keys: vec![provider_scope(base)?],
        chatgpt: vec![chatgpt_scope("https://chatgpt.com/backend-api")?],
    };
    // Read base providers first, then overlay each named configuration. A profile
    // can inherit a base URL while supplying its own credential field.
    let mut documents = BTreeMap::new();
    for (name, contents) in files.iter().filter(|(name, _)| name.ends_with(".toml")) {
        let table: toml::Table =
            toml::from_str(contents).map_err(|_| invalid("invalid Codex TOML configuration"))?;
        documents.insert(name.clone(), table);
    }
    let base_providers = documents
        .get("config.toml")
        .and_then(|config| config.get("model_providers"))
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    for (name, document) in &mut documents {
        if let Some(base) = document
            .get("chatgpt_base_url")
            .and_then(toml::Value::as_str)
        {
            let base = base.trim_end_matches('/');
            let base = if base.ends_with("/backend-api") {
                base.to_owned()
            } else {
                format!("{base}/backend-api")
            };
            scopes.chatgpt.push(chatgpt_scope(&base)?);
        }
        validate_mcp(document)?;
        if let Some(otel) = document.get("otel") {
            validate_telemetry(otel)?;
        }
        // Sandbox authentication is synthetic and intentionally uses its private file.
        document.insert("cli_auth_credentials_store".into(), "file".into());
        let Some(providers) = document.get_mut("model_providers") else {
            continue;
        };
        let providers = providers
            .as_table_mut()
            .ok_or_else(|| invalid("invalid Codex model provider configuration"))?;
        for (id, provider) in providers {
            let table = provider
                .as_table_mut()
                .ok_or_else(|| invalid("invalid Codex model provider"))?;
            let mut effective = if name == "config.toml" {
                toml::Table::new()
            } else {
                base_providers
                    .get(id)
                    .and_then(toml::Value::as_table)
                    .cloned()
                    .unwrap_or_default()
            };
            effective.extend(table.clone());
            *table = effective.clone();
            if effective.contains_key("auth") || effective.contains_key("gateway_oauth") {
                return Err(invalid(
                    "command-backed and gateway OAuth credentials need a separate Codex credential adapter",
                ));
            }
            if let Some(query) = effective.get("query_params") {
                let query = query
                    .as_table()
                    .ok_or_else(|| invalid("invalid Codex provider query parameters"))?;
                if !query.iter().all(|(name, value)| {
                    value
                        .as_str()
                        .is_some_and(|v| public_query_parameter(name, v))
                }) {
                    return Err(invalid(
                        "unsupported Codex provider query parameter; use a scoped authentication header for credentials",
                    ));
                }
            }
            let base = effective
                .get("base_url")
                .and_then(toml::Value::as_str)
                .unwrap_or("https://api.openai.com/v1");
            let scope = provider_scope(base)?;
            if id == "openai"
                || effective
                    .get("requires_openai_auth")
                    .and_then(toml::Value::as_bool)
                    == Some(true)
            {
                scopes.keys.push(scope.clone());
                scopes.chatgpt.push(scope.clone());
            }
            if let Some(value) = table.get_mut("experimental_bearer_token") {
                let token = value
                    .as_str()
                    .ok_or_else(|| invalid("invalid Codex bearer token configuration"))?;
                *value = builder
                    .bind(
                        std::slice::from_ref(&scope),
                        "authorization",
                        token,
                        true,
                        None,
                        None,
                    )?
                    .into();
            }
            if let Some(headers) = table.get_mut("http_headers") {
                let headers = headers
                    .as_table_mut()
                    .ok_or_else(|| invalid("invalid Codex header configuration"))?;
                for (header, value) in headers {
                    let secret = value
                        .as_str()
                        .ok_or_else(|| invalid("invalid Codex header configuration"))?;
                    if metadata_header(header) {
                        axum::http::HeaderValue::from_str(secret)
                            .map_err(|_| invalid("invalid Codex header configuration"))?;
                        continue;
                    }
                    *value = builder
                        .bind(
                            std::slice::from_ref(&scope),
                            header,
                            secret,
                            false,
                            None,
                            None,
                        )?
                        .into();
                }
            }
            if let Some(name) = effective.get("env_key") {
                let name = name
                    .as_str()
                    .ok_or_else(|| invalid("invalid Codex environment key configuration"))?;
                builder.environment(std::slice::from_ref(&scope), "authorization", name, true)?;
            }
            if let Some(headers) = effective.get("env_http_headers") {
                let headers = headers
                    .as_table()
                    .ok_or_else(|| invalid("invalid Codex environment header configuration"))?;
                for (header, name) in headers {
                    let name = name
                        .as_str()
                        .ok_or_else(|| invalid("invalid Codex environment header configuration"))?;
                    builder.environment(std::slice::from_ref(&scope), header, name, false)?;
                }
            }
        }
    }
    for (name, document) in documents {
        files.insert(
            name,
            toml::to_string(&document)
                .map_err(|_| invalid("cannot serialize sanitized Codex configuration"))?,
        );
    }
    if !files.contains_key("config.toml") {
        files.insert(
            "config.toml".into(),
            "cli_auth_credentials_store = \"file\"\n".into(),
        );
    }
    Ok(scopes)
}

pub(super) fn metadata_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept"
            | "accept-encoding"
            | "content-type"
            | "content-encoding"
            | "user-agent"
            | "connection"
            | "upgrade"
            | "content-length"
            | "transfer-encoding"
            | "host"
    )
}

// Query parameters have no host-side credential adapter. Only publish values
// with a known public meaning, including the Azure API version selector.
fn public_query_parameter(name: &str, value: &str) -> bool {
    if !name.eq_ignore_ascii_case("api-version") {
        return false;
    }
    let date = value.strip_suffix("-preview").unwrap_or(value);
    let version = value.strip_prefix('v').unwrap_or(value);
    value == "preview"
        || version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        || time::Date::parse(date, &time::format_description::well_known::Iso8601::DATE).is_ok()
}

fn validate_endpoint(value: &toml::Value) -> io::Result<()> {
    let error = || {
        invalid(
            "unsupported Codex service URL; use an HTTP(S) endpoint without embedded credentials or private query parameters",
        )
    };
    let url = value.as_str().ok_or_else(error)?;
    let uri: Uri = url.parse().map_err(|_| error())?;
    let authority = uri.authority().ok_or_else(error)?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || authority.as_str().contains('@')
        || url.contains(['#', '\\'])
    {
        return Err(error());
    }
    if uri.query().is_some() {
        let axum::extract::Query(params) =
            axum::extract::Query::<Vec<(String, String)>>::try_from_uri(&uri)
                .map_err(|_| error())?;
        if !params
            .iter()
            .all(|(name, value)| public_query_parameter(name, value))
        {
            return Err(error());
        }
    }
    Ok(())
}

fn validate_mcp(config: &toml::Table) -> io::Result<()> {
    let unsupported = || {
        invalid(
            "MCP credentials need a separate credential adapter; remove them from this Codex configuration",
        )
    };
    let idp = config
        .get("mcp_enterprise_managed_auth")
        .and_then(|auth| auth.get("idp"));
    if idp.and_then(|idp| idp.get("client_secret")).is_some() {
        return Err(unsupported());
    }
    let Some(servers) = config.get("mcp_servers") else {
        return Ok(());
    };
    let servers = servers
        .as_table()
        .ok_or_else(|| invalid("invalid Codex MCP configuration"))?;
    for server in servers.values() {
        let server = server
            .as_table()
            .ok_or_else(|| invalid("invalid Codex MCP server"))?;
        if [
            "env",
            "http_headers",
            "http_headers_helper",
            "env_http_headers",
            "bearer_token_env_var",
        ]
        .iter()
        .any(|name| server.contains_key(*name))
            || server
                .get("oauth")
                .and_then(|oauth| oauth.get("client_secret"))
                .is_some()
        {
            return Err(unsupported());
        }
        if let Some(url) = server.get("url") {
            validate_endpoint(url)?;
        }
    }
    Ok(())
}

fn public_telemetry_header(name: &str, value: &toml::Value) -> bool {
    let Some(value) = value.as_str() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    value.to_ascii_lowercase().split(',').all(|item| {
        matches!(
            (name.as_str(), item.trim_matches([' ', '\t'])),
            ("accept", "*/*")
                | (
                    "accept" | "content-type",
                    "application/json"
                        | "application/json; charset=utf-8"
                        | "application/x-protobuf"
                        | "application/protobuf"
                        | "application/grpc"
                        | "application/grpc+proto"
                        | "application/grpc+json"
                )
                | (
                    "accept-encoding" | "content-encoding",
                    "identity" | "gzip" | "deflate" | "br" | "zstd"
                )
        )
    })
}

fn validate_telemetry(value: &toml::Value) -> io::Result<()> {
    let error = || {
        invalid(
            "unsupported Codex telemetry credentials or exporter settings; use an exporter without authentication",
        )
    };
    let table = value.as_table().ok_or_else(error)?;
    if table.contains_key("bearer_token") || table.contains_key("headers_helper") {
        return Err(error());
    }
    if let Some(headers) = table.get("headers")
        && !headers
            .as_table()
            .ok_or_else(error)?
            .iter()
            .all(|(name, value)| public_telemetry_header(name, value))
    {
        return Err(error());
    }
    if let Some(endpoint) = table.get("endpoint") {
        validate_endpoint(endpoint)?;
    }
    for name in ["exporter", "trace_exporter", "metrics_exporter"] {
        if let Some(exporter) = table.get(name) {
            if matches!(exporter.as_str(), Some("none" | "statsig")) {
                continue;
            }
            for (transport, settings) in exporter.as_table().ok_or_else(error)? {
                if !matches!(transport.as_str(), "otlp-http" | "otlp-grpc") {
                    return Err(error());
                }
                validate_telemetry(settings)?;
            }
        }
    }
    Ok(())
}
