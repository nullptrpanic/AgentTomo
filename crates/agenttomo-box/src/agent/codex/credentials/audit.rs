//! Inspect the prepared snapshot before any of its bytes reach Docker stdin.
use super::{MARKER, invalid, jwt_claims};
use serde_json::Value;
use std::{collections::BTreeMap, io};

pub(super) fn verify(files: &BTreeMap<String, String>, secrets: &[String]) -> io::Result<()> {
    if secrets.is_empty() {
        return Ok(());
    }
    for (name, contents) in files {
        check_text(name, secrets)?;
        let value: Value = if name.ends_with(".toml") {
            let parsed: toml::Value = toml::from_str(contents)
                .map_err(|_| invalid("cannot audit the Codex TOML snapshot"))?;
            serde_json::to_value(parsed)
                .map_err(|_| invalid("cannot audit the Codex TOML snapshot"))?
        } else {
            serde_json::from_str(contents)
                .map_err(|_| invalid("cannot audit the Codex JSON snapshot"))?
        };
        check_value(&value, secrets)?;
    }
    Ok(())
}

fn check_value(value: &Value, secrets: &[String]) -> io::Result<()> {
    match value {
        Value::String(text) => {
            check_text(text, secrets)?;
            // Inspect decoded placeholder claims as well as the JWT envelope.
            if text.contains(MARKER)
                && let Ok(claims) = jwt_claims(text)
            {
                check_value(&claims, secrets)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                check_value(value, secrets)?;
            }
        }
        Value::Object(values) => {
            for (name, value) in values {
                check_text(name, secrets)?;
                check_value(value, secrets)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_text(text: &str, secrets: &[String]) -> io::Result<()> {
    if secrets.iter().any(|secret| text.contains(secret)) {
        return Err(invalid(
            "host credential material remains in the Codex snapshot; sandbox startup stopped",
        ));
    }
    Ok(())
}
