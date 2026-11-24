use super::*;
use crate::cli::{Cli, Command};
use clap::Parser;
use serde_json::{Value, json};
use std::path::PathBuf;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agenttomo-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn load(&self, file: Value, flags: &[&str]) -> io::Result<Config> {
        let path = self.0.join("config.json");
        fs::write(&path, file.to_string()).unwrap();
        let cli = Cli::parse_from(
            ["box", "playground", "-c", path.to_str().unwrap()]
                .into_iter()
                .chain(flags.iter().copied()),
        );
        let Command::Playground(options) = cli.command else {
            unreachable!()
        };
        Config::load(&options)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn blank_settings_use_defaults_and_command_line_flags_take_precedence() {
    let fixture = Fixture::new();
    let defaults = fixture
        .load(json!({"listen":"", "image":"", "agents":[]}), &[])
        .unwrap();
    assert_eq!(defaults.listen, SocketAddr::from(([127, 0, 0, 1], 8080)));
    assert_eq!(defaults.image, DEFAULT_PLAYGROUND_IMAGE);
    assert!(!defaults.capture_http);
    let file =
        json!({"listen":"127.0.0.1:4567", "image":"from-file", "capture_http":false, "agents":[]});
    let configured = fixture.load(file.clone(), &[]).unwrap();
    assert_eq!(configured.listen.port(), 4567);
    assert_eq!(configured.image, "from-file");
    assert!(configured.agents.is_empty());
    let cli = fixture
        .load(
            file,
            &[
                "--listen",
                "127.0.0.1:0",
                "--image",
                "from-cli",
                "--capture-http",
            ],
        )
        .unwrap();
    assert_eq!(cli.listen.port(), 0);
    assert_eq!(cli.image, "from-cli");
    assert!(cli.capture_http);
}

#[tokio::test]
async fn listed_agents_resolve_relative_to_the_file_and_keep_the_ark_key_on_the_host() {
    let fixture = Fixture::new();
    // A native Codex distribution and an executable stand-in for AgentKit.
    let bin = fixture.0.join("bin");
    fs::create_dir(&bin).unwrap();
    for file in ["codex", "codex-code-mode-host"] {
        fs::copy("/bin/true", bin.join(file)).unwrap();
    }
    let mut file = json!({"agents":[
        {"name":"agentkit", "path":"bin/codex", "apikey":"fixture-key", "endpoint":"ep-fixture"},
        {"name":"codex", "path":"bin/codex"}
    ]});
    let config = fixture.load(file.clone(), &[]).unwrap();
    let ids: Vec<_> = config
        .agents
        .iter()
        .map(|agent| agent.adapter().metadata().id)
        .collect();
    assert_eq!(ids, ["agentkit", "codex"]);
    assert!(config.capture_http);
    let agent = &config.agents[0];
    assert_eq!(
        agent.mounts().unwrap(),
        [(bin.join("codex"), "/opt/agenttomo-agentkit".into())]
    );
    let prepared = agent.snapshot().await.unwrap();
    let settings = &prepared.files["model.json"];
    assert!(!settings.contains("fixture-key"));
    let model: Value = serde_json::from_str(settings).unwrap();
    assert_eq!(model["model"], "ep-fixture");
    assert_eq!(model["base"], "https://ark.cn-beijing.volces.com/api/v3");
    file["capture_http"] = false.into();
    assert!(
        fixture
            .load(file.clone(), &[])
            .err()
            .unwrap()
            .to_string()
            .contains("capture_http")
    );
    file["capture_http"] = true.into();
    file["agents"][1]["path"] = "missing-codex".into();
    assert!(
        fixture
            .load(file, &[])
            .err()
            .unwrap()
            .to_string()
            .contains("codex")
    );
}

#[test]
fn invalid_credential_data_is_not_echoed_by_the_configuration_parser() {
    let fixture = Fixture::new();
    let file = json!({"agents":[{"name":"agentkit", "apikey":1234567891234567_u64}]});
    let error = fixture.load(file, &[]).err().unwrap().to_string();
    assert!(error.contains("config.json"));
    assert!(error.contains("line"));
    assert!(!error.contains("1234567891234567"));
}
