//! Provider credential fields are interpreted only in the Codex adapter.
use super::{Builder, Scope, invalid};
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
        .map(String::as_str)
        .unwrap_or("https://api.openai.com/v1");
    let mut scopes = Scopes {
        keys: vec![Scope::parse(base)?],
        chatgpt: vec![Scope::chatgpt_backend("https://chatgpt.com/backend-api")?],
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
            scopes.chatgpt.push(Scope::chatgpt_backend(&base)?);
        }
        if document
            .get("mcp_servers")
            .and_then(toml::Value::as_table)
            .is_some_and(|servers| servers.values().any(has_mcp_credentials))
        {
            return Err(invalid(
                "MCP credentials need a separate credential adapter; remove them from this Codex configuration",
            ));
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
            let base = effective
                .get("base_url")
                .and_then(toml::Value::as_str)
                .unwrap_or("https://api.openai.com/v1");
            let scope = Scope::parse(base)?;
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

fn has_mcp_credentials(server: &toml::Value) -> bool {
    server.as_table().is_some_and(|server| {
        [
            "env",
            "http_headers",
            "env_http_headers",
            "bearer_token_env_var",
        ]
        .iter()
        .any(|name| server.contains_key(*name))
    })
}
