use super::*;
use crate::agent::adapter::Installation;
use axum::http::{HeaderMap, Method, Uri, header};

#[tokio::test]
async fn a_reused_port_cannot_receive_the_next_prompt_after_the_agent_exits() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let session = super::http::Session::new(listener.local_addr().unwrap(), u32::MAX);
    let other_service = tokio::spawn(async move {
        let Ok(accepted) =
            tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
        else {
            return 0;
        };
        let (mut socket, _) = accepted.unwrap();
        let mut bytes = [0; 1024];
        let count = socket.read(&mut bytes).await.unwrap();
        if count > 0 {
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        }
        count
    });
    assert!(session.run("private follow-up", false).await.is_err());
    assert_eq!(other_service.await.unwrap(), 0);
}

#[test]
fn runtime_requires_a_final_visible_answer_and_sanitizes_model_errors() {
    let partial = r#"{"partial":true,"content":{"role":"model","parts":[{"text":"first"}]}}"#;
    let thought = r#"{"content":{"role":"model","parts":[{"thought":true,"text":"internal"}]}}"#;
    let answer = r#"{"content":{"role":"model","parts":[{"text":"final"}]}}"#;
    assert!(!super::http::final_text("message", partial).unwrap());
    assert!(!super::http::final_text("message", thought).unwrap());
    assert!(super::http::final_text("message", answer).unwrap());
    let failure = r#"{"error":"generation failed: Error code: 401 - sensitive upstream body"}"#;
    let diagnostic = super::http::final_text("error", failure)
        .unwrap_err()
        .to_string();
    assert!(diagnostic.contains("401"));
    assert!(!diagnostic.contains("sensitive"));
    assert!(super::http::final_text("message", r#"{"errorCode":"server_error"}"#).is_err());
}

#[tokio::test]
async fn mounts_only_the_binary_and_forwards_a_session_marker_only_to_chat() {
    let agent = Installed {
        binary: PathBuf::from("/fixture/dist/agenttomo-agentkit"),
        model: "ep-test".into(),
        base: "https://model.example/api/v3".into(),
        credential: credentials::Credential::Key(secret_header("fixture-only", true).unwrap()),
    };
    assert_eq!(
        agent.mounts().unwrap(),
        vec![(agent.binary.clone(), PROGRAM.to_owned())]
    );
    let prepared = agent.snapshot().await.unwrap();
    assert_eq!(prepared.files.len(), 1);
    let config = &prepared.files[CONFIG];
    assert!(!config.contains("fixture-only"));
    let settings: Settings = serde_json::from_str(config).unwrap();
    let other = agent.snapshot().await.unwrap();
    assert_ne!(other.files[CONFIG], *config);

    let uri: Uri = "https://model.example/api/v3/chat/completions"
        .parse()
        .unwrap();
    let origin = prepared.policy.origin(uri.authority().unwrap()).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "model.example".parse().unwrap());
    headers.insert(
        header::AUTHORIZATION,
        secret_header(&settings.placeholder, true).unwrap(),
    );
    assert!(
        other
            .policy
            .inject(&origin, &Method::POST, &uri, &mut headers)
            .is_err()
    );
    assert!(
        prepared
            .policy
            .inject(&origin, &Method::GET, &uri, &mut headers)
            .is_err()
    );
    assert!(
        prepared
            .policy
            .inject(
                &origin,
                &Method::POST,
                &"/api/v3/models".parse().unwrap(),
                &mut headers
            )
            .is_err()
    );
    prepared
        .policy
        .inject(&origin, &Method::POST, &uri, &mut headers)
        .unwrap();
    assert_eq!(headers[header::AUTHORIZATION], "Bearer fixture-only");
}
