//! Ark credentials are acquired and renewed only by the Playground host.
use super::{locate, setting};
use crate::proxy::credentials::{Source, secret_header};
use axum::http::HeaderValue;
use serde::Deserialize;
use serde_json::json;
use std::{
    io,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    process::Command,
    sync::watch,
    time::{sleep, timeout},
};

pub(super) enum Credential {
    Key(HeaderValue),
    Temporary(Exchange),
}

#[derive(Clone)]
pub(super) struct Exchange {
    cli: PathBuf,
    region: String,
    model: String,
    project: Option<String>,
}

struct Token {
    header: HeaderValue,
    expires: u64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Credential {
    pub(super) fn discover(model: &str) -> Option<Self> {
        if let Some(key) = setting("MODEL_AGENT_API_KEY") {
            return secret_header(&key, true).ok().map(Self::Key);
        }
        setting("VOLCENGINE_ACCESS_KEY")?;
        setting("VOLCENGINE_SECRET_KEY")?;
        let project = if model.starts_with("ep-") {
            None
        } else {
            Some(setting("VOLCENGINE_PROJECT_NAME")?)
        };
        Some(Self::Temporary(Exchange {
            cli: locate("AGENTTOMO_VOLCENGINE_CLI", "ve")?,
            region: setting("VOLCENGINE_REGION")?,
            model: model.to_owned(),
            project,
        }))
    }

    pub(super) async fn source(&self) -> io::Result<Source> {
        let exchange = match self {
            Self::Key(key) => return Ok(Source::Static(key.clone())),
            Self::Temporary(exchange) => exchange,
        };
        let (sender, receiver) = watch::channel(exchange.fetch().await?);
        let exchange = exchange.clone();
        tokio::spawn(async move {
            let mut wait = refresh_delay(sender.borrow().expires);
            loop {
                tokio::select! { _ = sender.closed() => break, _ = sleep(wait) => {} }
                let token = tokio::select! { _ = sender.closed() => break, token = exchange.fetch() => token };
                match token {
                    Ok(token) => {
                        wait = refresh_delay(token.expires);
                        if sender.send(token).is_err() {
                            break;
                        }
                    }
                    Err(_) => wait = Duration::from_secs(10),
                }
            }
        });
        Ok(Source::Reload(Arc::new(move || {
            let token = receiver.borrow();
            if token.expires <= now() {
                return Err(io::Error::other("the host Ark credential expired"));
            }
            Ok(token.header.clone())
        })))
    }
}
fn refresh_delay(expires: u64) -> Duration {
    Duration::from_secs(expires.saturating_sub(now() + 60).max(1))
}

impl Exchange {
    async fn fetch(&self) -> io::Result<Token> {
        let mut body =
            json!({"DurationSeconds":3600,"ResourceType":"endpoint","ResourceIds":[self.model]});
        if let Some(project) = &self.project {
            body["ResourceType"] = "presetendpoint".into();
            body["ProjectName"] = project.clone().into();
        }
        let mut cli = Command::new(&self.cli);
        cli.args([
            "ark",
            "GetApiKey",
            "--region",
            &self.region,
            "--body",
            &body.to_string(),
            "--output",
            "json",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
        let result = timeout(Duration::from_secs(20), cli.output())
            .await
            .map_err(|_| io::Error::other("host Ark credential exchange timed out"))??;
        if !result.status.success() {
            return Err(io::Error::other("host Ark credential exchange failed"));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Reply {
            api_key: String,
            expired_time: u64,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Envelope {
            result: Reply,
        }
        let reply = serde_json::from_slice::<Envelope>(&result.stdout)
            .map_err(|_| io::Error::other("invalid host Ark credential response"))?
            .result;
        if reply.api_key.is_empty() || reply.expired_time <= now() {
            return Err(io::Error::other("host Ark credential is absent or expired"));
        }
        Ok(Token {
            header: secret_header(&reply.api_key, true)?,
            expires: reply.expired_time,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[tokio::test]
    async fn cli_exchange_scopes_and_renews_credentials_and_rejects_expired_replies() {
        // All inputs and outputs of this stand-in stay on the test host.
        let dir = std::env::temp_dir().join(format!("agenttomo-ark-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let cli = dir.join("ve");
        fs::write(
            &cli,
            r#"#!/bin/sh
cd -P -- "${0%/*}" || exit 1
printf '%s\n' "$@" > args
cat reply
"#,
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        let reply = |key: &str, expires: u64| {
            let json = json!({"Result":{"ApiKey":key,"ExpiredTime":expires}});
            fs::write(dir.join("new-reply"), json.to_string()).unwrap();
            fs::rename(dir.join("new-reply"), dir.join("reply")).unwrap();
        };
        let read_body = || {
            let args = fs::read_to_string(dir.join("args")).unwrap();
            let args = args.lines().collect::<Vec<_>>();
            assert_eq!(
                &args[..5],
                &["ark", "GetApiKey", "--region", "cn-beijing", "--body"]
            );
            assert_eq!(&args[6..], &["--output", "json"]);
            serde_json::from_str::<serde_json::Value>(args[5]).unwrap()
        };
        let mut exchange = Exchange {
            cli,
            region: "cn-beijing".into(),
            model: "ep-test".into(),
            project: None,
        };
        reply("one", now() + 55);
        let Source::Reload(current) = Credential::Temporary(exchange.clone())
            .source()
            .await
            .unwrap()
        else {
            panic!("expected a renewable source")
        };
        assert_eq!(current().unwrap(), "Bearer one");
        assert_eq!(
            read_body(),
            json!({"DurationSeconds":3600,"ResourceType":"endpoint","ResourceIds":["ep-test"]})
        );
        reply("two", now() + 3600);
        timeout(Duration::from_secs(5), async {
            while current().unwrap() != "Bearer two" {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        drop(current);
        exchange.model = "preset-model".into();
        exchange.project = Some("demo".into());
        reply("expired", now());
        assert!(exchange.fetch().await.is_err());
        assert_eq!(
            read_body(),
            json!({"DurationSeconds":3600,"ResourceType":"presetendpoint","ResourceIds":["preset-model"],"ProjectName":"demo"})
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
