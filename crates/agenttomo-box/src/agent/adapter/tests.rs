use super::*;
use crate::agent::Scope;
use futures_util::future::BoxFuture;
use std::sync::Arc;
use tokio::io::AsyncReadExt;

struct Example;
static EXAMPLE: Example = Example;

impl AgentAdapter for Example {
    fn metadata(&self) -> Metadata {
        Metadata {
            id: "example",
            name: "Example Agent",
            avatar: "E",
            authentication_hint: "Example login",
        }
    }
    fn discover(&self) -> Option<Arc<dyn Installation>> {
        None
    }
    fn observation_rules(&self) -> ObservationRules {
        ObservationRules {
            runtime_executables: vec!["example-helper".into()],
            private_paths: vec!["/records/example".into()],
        }
    }
    fn home(&self) -> &'static Path {
        Path::new("/records/example")
    }
    fn configuration_file(&self, name: &str) -> bool {
        name == "example.json"
    }
    fn configure(&self, command: &mut Command) -> io::Result<()> {
        command.env("EXAMPLE_CONFIGURED", "yes");
        Ok(())
    }
    fn launch(&self, request: &LaunchRequest<'_>) -> io::Result<Launch> {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s:%s\\n' \"$AGENTTOMO_TASK_ID\" \"$EXAMPLE_CONFIGURED\"; cat",
        ]);
        self.configure(&mut command)?;
        Ok(Launch {
            command,
            input: serde_json::to_vec(
                &serde_json::json!({"message":request.prompt,"previous":request.conversation}),
            )?,
        })
    }
    fn read_output(
        &self,
        mut input: RuntimeReader,
        _diagnostics: mpsc::Sender<String>,
    ) -> BoxFuture<'_, io::Result<RuntimeOutput>> {
        Box::pin(async move {
            let mut bytes = String::new();
            input.read_to_string(&mut bytes).await?;
            let (prefix, body) = bytes
                .split_once('\n')
                .ok_or_else(|| io::Error::other("missing launch environment"))?;
            let body: serde_json::Value = serde_json::from_str(body)?;
            Ok(RuntimeOutput {
                conversation_id: Some(format!(
                    "{prefix}:{}:{}",
                    body["previous"].as_str().unwrap(),
                    body["message"].as_str().unwrap()
                )),
                error: None,
            })
        })
    }
}

#[test]
fn no_agent_does_not_select_a_runtime_but_keeps_private_preview_paths() {
    assert!(select(None, ADAPTERS).unwrap().is_none());
    let rules = observation_rules(None);
    assert!(rules.runtime_executables.is_empty());
    for path in [
        "/records/codex/auth.json",
        "/root/.agenttomo-config/config.toml",
    ] {
        assert!(
            rules
                .private_paths
                .iter()
                .any(|root| Path::new(path).starts_with(root))
        );
    }
}

#[test]
fn registered_adapters_resolve_without_a_codex_dispatch_branch() {
    let registry = [Adapter::new(&EXAMPLE)];
    let selected = select(Some("example"), &registry).unwrap().unwrap();
    assert_eq!(selected.metadata().name, "Example Agent");
    let rules = observation_rules(Some(selected));
    assert_eq!(rules.runtime_executables, ["example-helper"]);
    assert!(
        rules
            .private_paths
            .contains(&PathBuf::from("/records/example"))
    );
    assert!(select(Some("codex"), &registry).is_err());
    assert!(select(Some("unknown"), ADAPTERS).is_err());
    let codex: Adapter = serde_json::from_str("\"codex\"").unwrap();
    assert_eq!(serde_json::to_string(&codex).unwrap(), "\"codex\"");
    assert!(serde_json::from_str::<Adapter>("\"unknown\"").is_err());
}

#[tokio::test]
async fn shared_launcher_uses_plugin_input_and_lifecycle_reader() {
    let adapter = Adapter::new(&EXAMPLE);
    let scope = Scope {
        agent_instance_id: "agent".into(),
        task_id: "task".into(),
    };
    let mut child =
        super::super::workspace::start(adapter, &scope, Some("previous"), "hello\nworld")
            .await
            .unwrap();
    let (diagnostics, _notices) = mpsc::channel(1);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        adapter.read_output(Box::new(child.stdout.take().unwrap()), diagnostics),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    assert_eq!(
        output.conversation_id.as_deref(),
        Some("task:yes:previous:hello\nworld")
    );
    assert!(output.error.is_none());
}
