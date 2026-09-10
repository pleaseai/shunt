use super::Entry;
use anyhow::{bail, Context};
use serde_json::Value;
use std::collections::BTreeSet;

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn secret(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 16384 && s.bytes().all(|b| (33..=126).contains(&b)))
}

fn api_variable(provider: &str) -> String {
    match provider {
        "openai" => "OPENAI_API_KEY".into(),
        "xai" => "XAI_API_KEY".into(),
        "commandcode" => "SHUNT_COMMANDCODE_API_KEY".into(),
        _ => format!(
            "SHUNT_IMPORTED_{}_API_KEY",
            provider.to_ascii_uppercase().replace('-', "_")
        ),
    }
}

pub(super) fn discover(
    config: Option<&Value>,
    auth: Option<&Value>,
    selected: &[String],
    now: u128,
) -> anyhow::Result<(Vec<Entry>, Vec<String>)> {
    if selected.iter().any(|p| !safe_name(p)) {
        bail!("Invalid provider selector; use letters, digits, hyphens or underscores");
    }
    let mut entries = Vec::new();
    let mut notes = Vec::new();
    let mut seen = BTreeSet::new();
    let want = |name: &str| selected.is_empty() || selected.iter().any(|p| p == name);
    if let Some(config) = config {
        let root = config
            .as_object()
            .context("config.json must contain an object")?;
        if let Some(providers) = root.get("providers") {
            for (name, provider) in providers
                .as_object()
                .context("config.json providers must be an object")?
            {
                if !safe_name(name) {
                    notes.push("Skipped an invalid provider identifier".into());
                    continue;
                }
                seen.insert(name.clone());
                if !want(name) {
                    continue;
                }
                // Forward-proxy credentials and unsupported transports must not become provider keys.
                if provider.get("disabled").and_then(Value::as_bool) == Some(true)
                    || !matches!(
                        provider.get("adapter").and_then(Value::as_str),
                        Some("openai-chat" | "openai-responses" | "anthropic")
                    )
                {
                    notes.push(format!(
                        "Skipped {name}: disabled or unsupported API-key transport"
                    ));
                    continue;
                }
                if let Some(key) = provider.get("apiKey").and_then(secret) {
                    entries.push(Entry {
                        provider: name.clone(),
                        variable: api_variable(name),
                        secret: key.into(),
                    });
                } else {
                    notes.push(format!(
                        "Skipped {name}: no valid active API key (key pools are not guessed)"
                    ));
                }
            }
        }
    }
    if let Some(auth) = auth {
        for (name, record) in auth
            .as_object()
            .context("auth.json must contain an object")?
        {
            if !safe_name(name) {
                notes.push("Skipped an invalid auth provider identifier".into());
                continue;
            }
            seen.insert(name.clone());
            if !want(name) {
                continue;
            }
            let variable = match name.as_str() {
                "cursor" => "SHUNT_CURSOR_AUTH_TOKEN",
                "command-code" => "SHUNT_COMMAND_CODE_TOKEN",
                _ => {
                    notes.push(format!(
                        "Skipped {name}: subscription format not supported; use shunt login"
                    ));
                    continue;
                }
            };
            let credential = if record.get("accounts").is_some() {
                let active = record.get("activeAccountId").and_then(Value::as_str);
                let matches: Vec<_> = record
                    .get("accounts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|a| active.is_some() && a.get("id").and_then(Value::as_str) == active)
                    .collect();
                if matches.len() != 1
                    || matches[0].get("needsReauth").and_then(Value::as_bool) == Some(true)
                {
                    notes.push(format!("Skipped {name}: active account missing, ambiguous, or needs reauthentication"));
                    continue;
                }
                matches[0].get("credential").unwrap_or(&Value::Null)
            } else {
                record
            };
            let expires = credential.get("expires").and_then(Value::as_u64);
            let access = credential.get("access").and_then(secret);
            if expires.is_none_or(|t| u128::from(t) <= now + 60_000) || access.is_none() {
                notes.push(format!("Skipped {name}: expired or malformed access token"));
                continue;
            }
            entries.push(Entry {
                provider: name.clone(),
                variable: variable.into(),
                secret: access.unwrap().into(),
            });
        }
    }
    if selected.iter().any(|p| !seen.contains(p)) {
        bail!("A selected provider was not found in the source files");
    }
    let mut variables = BTreeSet::new();
    if entries
        .iter()
        .any(|e| !variables.insert(e.variable.clone()))
    {
        bail!("Selected credentials have colliding environment variable names; select one provider at a time");
    }
    Ok((entries, notes))
}
