use super::*;

fn files(auth: Value, config: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("auth.json".into(), auth.to_string()),
        ("config.toml".into(), config.into()),
    ])
}

#[test]
fn chatgpt_snapshot_replaces_all_tokens_and_preserves_only_required_metadata() {
    let claims = json!({"email":"private@example.invalid","https://api.openai.com/auth":{"chatgpt_account_id":"workspace","chatgpt_plan_type":"plus","arbitrary":"remove-me"}});
    let original_id = format!(
        "e30.{}.original-signed-token",
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let auth = json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"id_token":original_id,"access_token":"original-access-secret","refresh_token":"original-refresh-secret","account_id":"workspace"},"last_refresh":"2020-01-01T00:00:00Z"});
    let prepared = prepare(
        Path::new("/nonexistent"),
        files(auth, "model='example'\n"),
        BTreeMap::new(),
    )
    .unwrap();
    let snapshot = serde_json::to_string(&prepared.files).unwrap();
    for secret in [
        &original_id,
        "original-access-secret",
        "original-refresh-secret",
        "private@example.invalid",
        "remove-me",
    ] {
        assert!(!snapshot.contains(secret));
    }
    let sanitized: Value = serde_json::from_str(&prepared.files["auth.json"]).unwrap();
    assert_eq!(sanitized["tokens"]["account_id"], "workspace");
    let claims = jwt_claims(sanitized["tokens"]["id_token"].as_str().unwrap()).unwrap();
    assert_eq!(
        claims["https://api.openai.com/auth"]["chatgpt_plan_type"],
        "plus"
    );
    assert!(claims["exp"].as_i64().unwrap() > time::OffsetDateTime::now_utc().unix_timestamp());
    let access = jwt_claims(sanitized["tokens"]["access_token"].as_str().unwrap()).unwrap();
    assert!(access["exp"].as_i64().unwrap() > time::OffsetDateTime::now_utc().unix_timestamp());
    assert!(
        prepared
            .policy
            .origin(&"chatgpt.com:443".parse().unwrap())
            .is_some()
    );
    assert!(
        prepared
            .policy
            .origin(&"api.openai.com:443".parse().unwrap())
            .is_none()
    );
}

#[test]
fn provider_headers_and_environment_keys_are_scoped_and_not_exposed_in_files() {
    let config = "[model_providers.gateway]\nbase_url='https://models.example/v1'\nenv_key='MODEL_API_KEY'\nhttp_headers={X-Auth='private-header-value'}\n";
    let prepared = prepare(
        Path::new("/unused"),
        files(json!({"OPENAI_API_KEY":"original-api-key"}), config),
        BTreeMap::from([("MODEL_API_KEY".into(), "private-environment-value".into())]),
    )
    .unwrap();
    let snapshot = serde_json::to_string(&prepared.files).unwrap();
    for secret in [
        "original-api-key",
        "private-header-value",
        "private-environment-value",
    ] {
        assert!(!snapshot.contains(secret));
    }
    let config: toml::Value = toml::from_str(&prepared.files["config.toml"]).unwrap();
    let marker = config["model_providers"]["gateway"]["http_headers"]["X-Auth"]
        .as_str()
        .unwrap();
    let origin = prepared
        .policy
        .origin(&"models.example:443".parse().unwrap())
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("host", "models.example".parse().unwrap());
    headers.insert("x-auth", marker.parse().unwrap());
    prepared
        .policy
        .inject(
            &origin,
            &Method::POST,
            &"/v1/responses".parse().unwrap(),
            &mut headers,
        )
        .unwrap();
    assert_eq!(headers["x-auth"], "private-header-value");
    assert!(headers["x-auth"].is_sensitive());
}

#[test]
fn wrong_session_authority_path_and_duplicate_credentials_are_rejected() {
    let config = "[model_providers.test]\nbase_url='https://models.example/v1'\nexperimental_bearer_token='original-bearer-value'\n";
    let prepared = prepare(
        Path::new("/unused"),
        files(json!({"OPENAI_API_KEY":"original-api-key"}), config),
        BTreeMap::new(),
    )
    .unwrap();
    let config: toml::Value = toml::from_str(&prepared.files["config.toml"]).unwrap();
    let marker = config["model_providers"]["test"]["experimental_bearer_token"]
        .as_str()
        .unwrap();
    let origin = prepared
        .policy
        .origin(&"models.example:443".parse().unwrap())
        .unwrap();
    for (host, path, value) in [
        (
            "attacker.example",
            "/v1/responses",
            format!("Bearer {marker}"),
        ),
        (
            "models.example:invalid",
            "/v1/responses",
            format!("Bearer {marker}"),
        ),
        (
            "models.example.",
            "/v1/responses",
            format!("Bearer {marker}"),
        ),
        ("models.example", "/unrelated", format!("Bearer {marker}")),
        (
            "models.example",
            "/v1/../unrelated",
            format!("Bearer {marker}"),
        ),
        (
            "models.example",
            "/v1/%2e%2e/unrelated",
            format!("Bearer {marker}"),
        ),
        (
            "models.example",
            "/v1/%2f..%2funrelated",
            format!("Bearer {marker}"),
        ),
        (
            "models.example",
            "/v1/responses",
            "Bearer agenttomo-credential-other-session".into(),
        ),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("host", host.parse().unwrap());
        headers.insert("authorization", value.parse().unwrap());
        assert!(
            prepared
                .policy
                .inject(&origin, &Method::POST, &path.parse().unwrap(), &mut headers)
                .is_err()
        );
    }
    let mut headers = HeaderMap::new();
    headers.insert("host", "models.example".parse().unwrap());
    headers.append("authorization", format!("Bearer {marker}").parse().unwrap());
    headers.append("authorization", format!("Bearer {marker}").parse().unwrap());
    assert!(
        prepared
            .policy
            .inject(
                &origin,
                &Method::POST,
                &"/v1/responses".parse().unwrap(),
                &mut headers
            )
            .is_err()
    );
}

#[test]
fn malformed_configuration_and_unsupported_auth_errors_do_not_echo_credentials() {
    for config in [
        "secret='private-value\n",
        "[model_providers.test.auth]\ncommand='private-value'\n",
        "[model_providers.test]\nhttp_headers='private-value'\n",
        "[model_providers.test]\nenv_http_headers='private-value'\n",
        "[model_providers.test]\nenv_key=['private-value']\n",
    ] {
        let error = prepare(
            Path::new("/unused"),
            files(json!({"OPENAI_API_KEY":"fixture-key"}), config),
            BTreeMap::new(),
        )
        .err()
        .unwrap();
        assert!(!error.to_string().contains("private-value"));
    }
    assert!(
        prepare(
            Path::new("/unused"),
            files(json!({"personal_access_token":"private-value"}), ""),
            BTreeMap::new()
        )
        .is_err()
    );
}

#[test]
fn expired_login_requires_host_reauthentication_and_account_changes_are_rejected() {
    let home =
        std::env::temp_dir().join(format!("agenttomo-expired-login-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    let id = format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"workspace"}}"#)
    );
    let expired = format!(
        "e30.{}.expired-secret",
        URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#)
    );
    let mut auth = json!({"auth_mode":"chatgpt","tokens":{"id_token":id,"access_token":expired,"refresh_token":"fixture-refresh","account_id":"workspace"}});
    let path = home.join("auth.json");
    std::fs::write(&path, auth.to_string()).unwrap();
    let prepared = prepare(&home, files(auth.clone(), ""), BTreeMap::new()).unwrap();
    let sanitized: Value = serde_json::from_str(&prepared.files["auth.json"]).unwrap();
    let origin = prepared
        .policy
        .origin(&"chatgpt.com:443".parse().unwrap())
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("host", "chatgpt.com".parse().unwrap());
    headers.insert(
        "authorization",
        format!(
            "Bearer {}",
            sanitized["tokens"]["access_token"].as_str().unwrap()
        )
        .parse()
        .unwrap(),
    );
    let uri = "/backend-api/codex/responses".parse().unwrap();
    let error = prepared
        .policy
        .inject(&origin, &Method::POST, &uri, &mut headers.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("expired"));
    assert!(!error.contains(&expired));
    auth["tokens"]["access_token"] = "refreshed-by-external-codex".into();
    std::fs::write(&path, auth.to_string()).unwrap();
    let mut updated = headers.clone();
    prepared
        .policy
        .inject(&origin, &Method::POST, &uri, &mut updated)
        .unwrap();
    assert_eq!(
        updated["authorization"],
        "Bearer refreshed-by-external-codex"
    );
    auth["tokens"]["account_id"] = "other-workspace".into();
    std::fs::write(&path, auth.to_string()).unwrap();
    assert!(
        prepared
            .policy
            .inject(&origin, &Method::POST, &uri, &mut headers)
            .unwrap_err()
            .to_string()
            .contains("account changed")
    );
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn login_is_reloaded_without_writes_and_session_revocation_blocks_injection() {
    let home = std::env::temp_dir().join(format!(
        "agenttomo-credential-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&home).unwrap();
    let auth = json!({"OPENAI_API_KEY":"original-api-key"});
    let path = home.join("auth.json");
    std::fs::write(&path, auth.to_string()).unwrap();
    let prepared = prepare(&home, files(auth, ""), BTreeMap::new()).unwrap();
    let sanitized: Value = serde_json::from_str(&prepared.files["auth.json"]).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("host", "api.openai.com".parse().unwrap());
    headers.insert(
        "authorization",
        format!("Bearer {}", sanitized["OPENAI_API_KEY"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    let origin = prepared
        .policy
        .origin(&"api.openai.com:443".parse().unwrap())
        .unwrap();
    let uri = "/v1/responses".parse().unwrap();
    let original = headers.clone();
    prepared
        .policy
        .inject(&origin, &Method::POST, &uri, &mut headers)
        .unwrap();
    assert_eq!(headers["authorization"], "Bearer original-api-key");
    let updated = json!({"OPENAI_API_KEY":"externally-updated-key"}).to_string();
    std::fs::write(&path, &updated).unwrap();
    let mut headers = original.clone();
    prepared
        .policy
        .inject(&origin, &Method::POST, &uri, &mut headers)
        .unwrap();
    assert_eq!(headers["authorization"], "Bearer externally-updated-key");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
    prepared.policy.lifetime.cancel();
    assert!(
        prepared
            .policy
            .inject(&origin, &Method::POST, &uri, &mut original.clone())
            .is_err()
    );
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn named_profile_inherited_credentials_follow_the_profiles_destination() {
    let mut configuration = files(
        json!({"OPENAI_API_KEY":"original-api-key"}),
        "[model_providers.test]\nbase_url='https://first.example/v1'\nexperimental_bearer_token='original-bearer-value'\n",
    );
    configuration.insert(
        "alternate.config.toml".into(),
        "[model_providers.test]\nbase_url='https://second.example/v1'\n".into(),
    );
    let prepared = prepare(Path::new("/unused"), configuration, BTreeMap::new()).unwrap();
    let profile: toml::Value = toml::from_str(&prepared.files["alternate.config.toml"]).unwrap();
    let token = profile["model_providers"]["test"]
        .get("experimental_bearer_token")
        .and_then(toml::Value::as_str);
    assert!(
        token.is_some(),
        "a profile needs its own scoped placeholder for inherited credentials"
    );
    let origin = prepared
        .policy
        .origin(&"second.example:443".parse().unwrap())
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("host", "second.example".parse().unwrap());
    headers.insert(
        "authorization",
        format!("Bearer {}", token.unwrap()).parse().unwrap(),
    );
    prepared
        .policy
        .inject(
            &origin,
            &Method::POST,
            &"/v1/responses".parse().unwrap(),
            &mut headers,
        )
        .unwrap();
    assert_eq!(headers["authorization"], "Bearer original-bearer-value");
}

#[test]
fn malformed_identity_metadata_is_rejected_before_bootstrap() {
    for claims in [
        json!("invalid-claims"),
        json!({"https://api.openai.com/auth":{"chatgpt_user_id":{"credential":"private-secret"}}}),
    ] {
        let id = format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let auth = json!({"auth_mode":"chatgpt","tokens":{"id_token":id,"access_token":"host-secret","refresh_token":"refresh-secret","account_id":"workspace"}});
        assert!(prepare(Path::new("/unused"), files(auth, ""), BTreeMap::new()).is_err());
    }
}

#[test]
fn known_credentials_in_other_configuration_fields_stop_bootstrap() {
    let auth = json!({"OPENAI_API_KEY":"host-private-api-key"});
    let configuration = files(auth.clone(), "model='host-private-api-key'\n");
    let error = prepare(Path::new("/unused"), configuration, BTreeMap::new()).err();
    assert!(
        error.is_some(),
        "credentials copied into an unrelated field must be rejected"
    );
    assert!(!error.unwrap().to_string().contains("host-private-api-key"));
    let mut configuration = files(auth, "");
    configuration.insert(
        "models_cache.json".into(),
        json!({"unrelated":"host-private-api-key"}).to_string(),
    );
    assert!(prepare(Path::new("/unused"), configuration, BTreeMap::new()).is_err());
    let id = format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_user_id":"host-access-secret"}}"#)
    );
    let auth = json!({"auth_mode":"chatgpt","tokens":{"id_token":id,"access_token":"host-access-secret","refresh_token":"host-refresh-secret","account_id":"workspace"}});
    assert!(
        prepare(Path::new("/unused"), files(auth, ""), BTreeMap::new()).is_err(),
        "synthetic JWT metadata cannot carry an original token"
    );
}

#[test]
fn custom_provider_base_paths_do_not_acquire_chatgpt_backend_semantics() {
    let configuration = "[model_providers.test]\nbase_url='https://models.example/backend-api'\nexperimental_bearer_token='host-provider-key'\n";
    let prepared = prepare(
        Path::new("/unused"),
        BTreeMap::from([("config.toml".into(), configuration.into())]),
        BTreeMap::new(),
    )
    .unwrap();
    let config: toml::Value = toml::from_str(&prepared.files["config.toml"]).unwrap();
    let marker = config["model_providers"]["test"]["experimental_bearer_token"]
        .as_str()
        .unwrap();
    let origin = prepared
        .policy
        .origin(&"models.example:443".parse().unwrap())
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("host", "models.example".parse().unwrap());
    headers.insert("authorization", format!("Bearer {marker}").parse().unwrap());
    prepared
        .policy
        .inject(
            &origin,
            &Method::POST,
            &"/backend-api/responses".parse().unwrap(),
            &mut headers,
        )
        .unwrap();
    assert_eq!(headers["authorization"], "Bearer host-provider-key");
}
