use std::borrow::Cow;

use reqwest::Url;

const REDACTED: &str = "[REDACTED]";

pub fn obs_context(fallback_agent_id: Option<&str>) -> (String, String, String, String) {
    let trace_id = std::env::var("AGENT_BLOCK_TRACE_ID").unwrap_or_default();
    let run_id = std::env::var("AGENT_BLOCK_RUN_ID").unwrap_or_default();
    let agent_id = std::env::var("AGENT_BLOCK_AGENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| fallback_agent_id.map(ToString::to_string))
        .unwrap_or_default();
    let agent_name = std::env::var("AGENT_BLOCK_AGENT_NAME").unwrap_or_default();
    (trace_id, run_id, agent_id, agent_name)
}

pub fn obs_line(
    component: &str,
    event: &str,
    ctx: &(String, String, String, String),
    extra: &[(&str, &str)],
) -> String {
    let mut parts = vec![
        "prefix=ab.obs".to_string(),
        format!("event={}", event),
        format!("component={}", component),
        format!("trace_id={}", kv_escape("trace_id", &ctx.0)),
        format!("run_id={}", kv_escape("run_id", &ctx.1)),
        format!("agent_id={}", kv_escape("agent_id", &ctx.2)),
        format!("agent_name={}", kv_escape("agent_name", &ctx.3)),
    ];
    for (k, v) in extra {
        parts.push(format!("{}={}", k, kv_escape(k, v)));
    }
    parts.join(" ")
}

fn kv_escape(key: &str, value: &str) -> String {
    let safe = sanitize_value(key, value);
    if safe.is_empty() {
        "\"\"".to_string()
    } else if safe.chars().any(|c| c.is_whitespace() || c == '=') {
        serde_json::Value::String(safe.into_owned()).to_string()
    } else {
        safe.into_owned()
    }
}

fn sanitize_value<'a>(key: &str, value: &'a str) -> Cow<'a, str> {
    if is_sensitive_key(key) {
        return Cow::Borrowed(REDACTED);
    }
    if key.eq_ignore_ascii_case("url") {
        return Cow::Owned(sanitize_url(value));
    }
    Cow::Borrowed(value)
}

fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "authorization",
        "cookie",
        "set-cookie",
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "access_key",
        "private_key",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

fn sanitize_url(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => raw.to_string(),
    }
}
