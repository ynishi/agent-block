use std::borrow::Cow;
use std::sync::OnceLock;

use reqwest::Url;
use uuid::Uuid;

const REDACTED: &str = "[REDACTED]";

/// Returns a process-scoped agent ID that is generated once and reused for the
/// lifetime of the process.  The semantic scope of `agent_id` is
/// "one agent-block execution", which is coarser than `run_id` (per-call).
/// Both currently collapse to the same generated value in single-run
/// invocations, but the conceptual distinction is preserved so that future
/// deployments can evolve the two scopes independently (e.g. long-running
/// daemon vs. per-request).
fn process_agent_id() -> &'static str {
    static AGENT_ID: OnceLock<String> = OnceLock::new();
    AGENT_ID.get_or_init(|| Uuid::new_v4().to_string())
}

/// Build the observability context tuple `(trace_id, run_id, agent_id, agent_name)`.
///
/// Resolution order for `agent_id`:
/// 1. `AGENT_BLOCK_AGENT_ID` environment variable (non-empty)
/// 2. `fallback_agent_id` argument (non-None)
/// 3. Process-scoped auto-generated UUID v4 (generated once per process lifetime).
///    Scope: one agent-block execution = one `agent_id`.  Conceptually coarser than
///    `run_id` (per-call), though both may share the same value in simple invocations.
pub fn obs_context(fallback_agent_id: Option<&str>) -> (String, String, String, String) {
    let trace_id = std::env::var("AGENT_BLOCK_TRACE_ID").unwrap_or_default();
    let run_id = std::env::var("AGENT_BLOCK_RUN_ID").unwrap_or_default();
    let agent_id = std::env::var("AGENT_BLOCK_AGENT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| fallback_agent_id.map(ToString::to_string))
        .unwrap_or_else(|| process_agent_id().to_string());
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
        Err(_) => "[UNPARSEABLE]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_agent_id_is_non_empty_and_stable() {
        // process_agent_id() must return a non-empty value and the same
        // value on every call (OnceLock semantics within this process).
        let id1 = process_agent_id();
        let id2 = process_agent_id();
        assert!(!id1.is_empty(), "process_agent_id must not be empty");
        assert_eq!(
            id1, id2,
            "process_agent_id must be stable within the process"
        );
    }

    #[test]
    fn obs_context_fallback_agent_id_wins_over_auto() {
        // When ENV is absent and fallback_agent_id is provided, it takes priority.
        // This test avoids mutating global ENV to prevent parallelism flakiness.
        // We temporarily unset via a guard-free approach: only valid if ENV is absent.
        // Use a distinctive value that cannot collide with a real env setting.
        let fallback = "test-fallback-agent-xxx";
        // Ensure ENV is not set to this value (it may be set to something else).
        // If ENV IS set, skip assertion on fallback path (env wins per spec).
        if std::env::var("AGENT_BLOCK_AGENT_ID")
            .unwrap_or_default()
            .is_empty()
        {
            let (_, _, id, _) = obs_context(Some(fallback));
            assert_eq!(id, fallback);
        }
    }

    #[test]
    fn sanitize_url_strips_credentials_and_query() {
        let raw = "https://user:pass@example.com/path?q=1&r=2#frag";
        let got = sanitize_url(raw);
        assert_eq!(got, "https://example.com/path");
    }

    #[test]
    fn sanitize_url_malformed_returns_unparseable() {
        let raw = "not a valid url ://::garbage";
        let got = sanitize_url(raw);
        assert_eq!(got, "[UNPARSEABLE]");
    }

    #[test]
    fn sanitize_url_empty_string_returns_unparseable() {
        let got = sanitize_url("");
        assert_eq!(got, "[UNPARSEABLE]");
    }
}
