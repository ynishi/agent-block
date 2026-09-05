//! Reading the log with SQL: the request, and the one place SQL text is
//! touched.
//!
//! The kernel does not own a query language.  The log lives in a SQLite
//! table whose columns are published ([`super::sqlite_store::events_schema`]),
//! and a caller reads it by writing SQL — no builder, no specification
//! object, no typed row.  What this module owns is the small amount of work
//! that has to happen *around* that SQL so it stays a read of this kernel's
//! log rather than a way into the database:
//!
//! 1. the statement is **one** statement and it **reads** — it starts with
//!    `SELECT` or `WITH`, and nothing follows the `;` if there is one, so
//!    `INSERT` / `PRAGMA` / `ATTACH` / `a; b` never reach the connection
//!    ([`plan`]).  The reader connection refuses a write on its own too
//!    ([`super::sqlite_store`]), and the prepared statement is asked whether
//!    it is read-only before it runs: three answers to one question, because
//!    the cost of being wrong is a caller writing through a view;
//! 2. values are **bound, never interpolated**.  Nothing a caller passes as a
//!    parameter is spliced into the text.  The one rewrite this module makes
//!    inserts *placeholders* — never a value — which is what keeps "the
//!    kernel does not build SQL out of data" true;
//! 3. the two reserved parameters are resolved: `$stream` is the session's
//!    own stream, and `$sessions` is the set the caller asked to read across.
//!
//! # The `$sessions` rewrite, exactly
//!
//! `$sessions` is the one token in a caller's SQL the kernel rewrites, and
//! this is the whole of the rule:
//!
//! * the rewritten token is the exact text `$sessions`, found while walking
//!   the statement as SQL — so an occurrence inside a string literal, a
//!   quoted identifier (`"…"`, `[…]`, `` `…` ``) or a comment is left alone,
//!   as is a longer name that merely starts with it (`$sessions2`);
//! * each occurrence is replaced by `(:knl_sessions_0, :knl_sessions_1, …)`,
//!   one named placeholder per id in the set, in order.  `WHERE stream IN
//!   $sessions` therefore compiles as `WHERE stream IN (:knl_sessions_0,
//!   :knl_sessions_1)` for a set of two;
//! * every other byte of the statement is handed to SQLite exactly as the
//!   caller wrote it;
//! * the ids themselves are *bound* to those placeholders by the backend.
//!   They are never written into the text, so a session id containing a
//!   quote is a value like any other.
//!
//! The placeholders are **named**, not anonymous `?`.  SQLite numbers an
//! anonymous parameter "one greater than the largest index used so far",
//! which means the index an inserted `?` receives depends on what the caller
//! wrote around it; a named slot is looked up by name, so the kernel's own
//! bindings cannot be confused with the caller's however the two are mixed.
//! `:knl_sessions_*` and `$stream` are therefore reserved: a parameter a
//! caller names in that shape is the kernel's, not theirs.

use std::ops::Range;
use std::time::Duration;

use serde_json::{Map, Value};

use super::{KnlError, KnlResult};

/// How long a query may run before it is interrupted, unless `opts` says
/// otherwise.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// How many rows a query returns before the rest are cut off, unless `opts`
/// says otherwise.  A cut is reported ([`QueryRows::truncated`]) rather than
/// left for the caller to infer from a suspiciously round count.
pub const DEFAULT_LIMIT: usize = 1_000;

/// The reserved parameter that is the session's own stream.
///
/// Matched on the whole name, prefix included: `$stream` is the kernel's,
/// while `:stream` / `@stream` are ordinary names a caller may use for
/// anything.
pub const STREAM_PARAM: &str = "$stream";

/// The reserved token that expands to the set of streams being read.
pub const SESSIONS_TOKEN: &str = "$sessions";

/// The prefix of the named placeholders [`SESSIONS_TOKEN`] expands to.
pub const SESSION_SLOT_PREFIX: &str = ":knl_sessions_";

/// The statements a query may be.
///
/// A closed list, checked on the text before SQLite ever sees it.  It is not
/// the only guard — the connection is read-only and the prepared statement is
/// asked whether it writes — but it is the one that can say *why* in the
/// caller's terms.
const READ_KEYWORDS: [&str; 2] = ["SELECT", "WITH"];

/// The name of the `n`-th session placeholder, as SQLite reports it.
pub fn session_slot(index: usize) -> String {
    format!("{SESSION_SLOT_PREFIX}{index}")
}

/// The values a caller bound to its own parameters.
///
/// Positional for `?`, named for `:name` / `@name` / `$name`, and the two are
/// not mixed: a statement is written one way or the other.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum QueryParams {
    /// No parameters of the caller's own.
    #[default]
    None,
    /// Values for the anonymous `?` parameters, in the order they appear.
    Positional(Vec<Value>),
    /// Values by name.  The key is the name *without* its prefix character,
    /// so `{ kind = "note" }` answers `:kind`, `@kind` and `$kind` alike.
    Named(Map<String, Value>),
}

/// What a caller asked for beyond the SQL itself.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryOpts {
    /// The streams `$sessions` expands to.  `None` is the session's own
    /// stream and nothing else; an empty list is refused, because a set that
    /// selects nothing is a mistake rather than a request.
    pub sessions: Option<Vec<String>>,
    /// How long the query may run.  Must be positive: a zero timeout would
    /// be a query that is interrupted before it starts, and "no timeout at
    /// all" is not on offer.
    pub timeout_ms: u64,
    /// How many rows to return before reporting a cut.
    pub limit: usize,
}

impl Default for QueryOpts {
    fn default() -> Self {
        Self {
            sessions: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            limit: DEFAULT_LIMIT,
        }
    }
}

/// A validated query, ready for a backend to prepare and bind.
///
/// The SQL here is the caller's, with `$sessions` expanded — the only
/// difference between this text and what was passed in.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    /// The statement to prepare.
    pub sql: String,
    /// What `$stream` binds to: the session's own stream.
    pub stream: String,
    /// What the `:knl_sessions_*` placeholders bind to, in order.
    pub sessions: Vec<String>,
    /// The caller's own values.
    pub params: QueryParams,
    /// The deadline for the whole query.
    pub timeout: Duration,
    /// The row cap.
    pub limit: usize,
}

/// The rows a query returned, and whether there were more.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryRows {
    /// One map per row: column name to value.  A `NULL` column is *absent*
    /// rather than present-and-null, so it reads as `nil` on the Lua side.
    pub rows: Vec<Map<String, Value>>,
    /// Whether the query had more rows than [`QueryOpts::limit`] allowed.
    pub truncated: bool,
}

/// Validate `sql`, expand `$sessions`, and settle what everything binds to.
///
/// `stream` is the session's own stream: what `$stream` binds to, and the
/// default set `$sessions` expands to.
pub fn plan(
    sql: &str,
    params: QueryParams,
    opts: &QueryOpts,
    stream: &str,
) -> KnlResult<QueryPlan> {
    let sessions = match opts.sessions.as_ref() {
        None => vec![stream.to_string()],
        Some(list) if list.is_empty() => {
            return Err(KnlError::Validation(
                "opts.sessions is empty; a set that selects no stream is not a request \
                 (omit it to read this session's own)"
                    .to_string(),
            ));
        }
        Some(list) => list.clone(),
    };
    if opts.timeout_ms == 0 {
        return Err(KnlError::Validation(
            "opts.timeout_ms must be a positive whole number of milliseconds".to_string(),
        ));
    }

    let scanned = scan(sql)?;
    if !READ_KEYWORDS.contains(&scanned.keyword.as_str()) {
        return Err(KnlError::Validation(format!(
            "a query reads: it must start with SELECT or WITH, got {:?}",
            scanned.keyword
        )));
    }

    Ok(QueryPlan {
        sql: expand_sessions(sql, &scanned.sessions, sessions.len()),
        stream: stream.to_string(),
        sessions,
        params,
        timeout: Duration::from_millis(opts.timeout_ms),
        limit: opts.limit,
    })
}

/// What the walk over a statement found.
struct Scanned {
    /// The first word, upper-cased — the statement's kind.
    keyword: String,
    /// Where each `$sessions` token is, in byte offsets.
    sessions: Vec<Range<usize>>,
}

/// Walk `sql` as SQL: find its first word, find the `$sessions` tokens, and
/// refuse a second statement.
///
/// It is a scanner and not a parser.  All it has to tell apart is code from
/// the three things that look like code and are not — string literals,
/// quoted identifiers and comments — because a `;` or a `$sessions` inside
/// one of those is text, not syntax.  Everything else is SQLite's to
/// understand.
fn scan(sql: &str) -> KnlResult<Scanned> {
    let bytes = sql.as_bytes();
    let mut at = 0;
    let mut keyword: Option<String> = None;
    let mut sessions: Vec<Range<usize>> = Vec::new();
    // Set by a `;`.  Anything but whitespace and comments after it is a
    // second statement, which is the shape a caller would smuggle a write in
    // as — and rusqlite's `prepare` compiles only the first statement, so an
    // unnoticed tail would be silently dropped rather than run.  Either way
    // the answer is to refuse it.
    let mut ended = false;

    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if byte == b'-' && bytes.get(at + 1) == Some(&b'-') {
            at += 2;
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(at + 1) == Some(&b'*') {
            at += 2;
            while at < bytes.len() && !(bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/')) {
                at += 1;
            }
            at = usize::min(at + 2, bytes.len());
            continue;
        }

        if ended {
            return Err(KnlError::Validation(
                "a query is one statement: there is SQL after the `;`".to_string(),
            ));
        }

        match byte {
            b';' => {
                ended = true;
                at += 1;
            }
            b'\'' | b'"' | b'`' | b'[' => at = skip_quoted(bytes, at),
            b'$' => {
                let end = ident_end(bytes, at + 1);
                if &sql[at..end] == SESSIONS_TOKEN {
                    sessions.push(at..end);
                }
                at = end;
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' => {
                let end = ident_end(bytes, at);
                if keyword.is_none() {
                    keyword = Some(sql[at..end].to_ascii_uppercase());
                }
                at = end;
            }
            // Punctuation, operators, digits, and any non-ASCII byte: not
            // something this scanner has a question about.  Advancing one
            // byte through a multi-byte character is safe because a UTF-8
            // continuation byte can never be one of the ASCII bytes matched
            // above, and no slice is taken at this offset.
            _ => at += 1,
        }
    }

    let keyword = keyword.ok_or_else(|| {
        KnlError::Validation("a query needs a statement; the SQL is empty".to_string())
    })?;
    Ok(Scanned { keyword, sessions })
}

/// The offset just past the quoted run starting at `start`.
///
/// Handles the four quotings SQLite accepts, and the doubled-quote escape
/// (`'it''s'`, `"a""b"`) for the three that have one.  An unterminated run
/// consumes the rest of the text: SQLite refuses it when it prepares, which
/// is the right place for a syntax error to be reported from.
fn skip_quoted(bytes: &[u8], start: usize) -> usize {
    let open = bytes[start];
    let close = if open == b'[' { b']' } else { open };
    let doubles = open != b'[';
    let mut at = start + 1;
    while at < bytes.len() {
        if bytes[at] == close {
            if doubles && bytes.get(at + 1) == Some(&close) {
                at += 2;
                continue;
            }
            return at + 1;
        }
        at += 1;
    }
    at
}

/// The offset just past the identifier characters starting at `from`.
fn ident_end(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < bytes.len()
        && (bytes[at].is_ascii_alphanumeric() || matches!(bytes[at], b'_' | b'$'))
    {
        at += 1;
    }
    at
}

/// Replace each `$sessions` token with `n` named placeholders.
///
/// The one edit the kernel makes to a caller's SQL.  Everything outside the
/// given ranges is copied byte for byte.
fn expand_sessions(sql: &str, tokens: &[Range<usize>], count: usize) -> String {
    if tokens.is_empty() {
        return sql.to_string();
    }
    let slots = (0..count).map(session_slot).collect::<Vec<_>>().join(", ");
    let slots = format!("({slots})");

    let mut out = String::with_capacity(sql.len() + tokens.len() * slots.len());
    let mut cursor = 0;
    for token in tokens {
        out.push_str(&sql[cursor..token.start]);
        out.push_str(&slots);
        cursor = token.end;
    }
    out.push_str(&sql[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A plan for `sql` over one stream, with everything else default.
    fn plan_of(sql: &str) -> KnlResult<QueryPlan> {
        plan(sql, QueryParams::None, &QueryOpts::default(), "s-1")
    }

    /// A plan for `sql` over the given set of streams.
    fn plan_over(sql: &str, sessions: &[&str]) -> KnlResult<QueryPlan> {
        let opts = QueryOpts {
            sessions: Some(sessions.iter().map(|s| (*s).to_string()).collect()),
            ..QueryOpts::default()
        };
        plan(sql, QueryParams::None, &opts, "s-1")
    }

    /// A read is a read: the two statements that only read are accepted, in
    /// any casing and after any amount of leading noise.
    #[test]
    fn a_select_or_with_is_a_query_whatever_it_is_dressed_in() {
        for sql in [
            "SELECT 1",
            "select 1",
            "  \n\t select 1",
            "-- a comment first\nSELECT 1",
            "/* and a block one */ WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT 1;",
            "SELECT 1;  -- trailing comment\n",
        ] {
            plan_of(sql).unwrap_or_else(|e| panic!("{sql:?} must be a query: {e}"));
        }
    }

    /// Everything else is refused before the connection is reached, and the
    /// refusal is the caller's class: the arguments did not hold up.
    #[test]
    fn anything_that_is_not_a_read_is_refused_as_validation() {
        for sql in [
            "INSERT INTO events (stream) VALUES ('x')",
            "UPDATE events SET kind = 'x'",
            "DELETE FROM events",
            "DROP TABLE events",
            "PRAGMA table_info(events)",
            "ATTACH DATABASE '/tmp/other.db' AS other",
            "BEGIN",
            "VACUUM",
        ] {
            let err = plan_of(sql).expect_err("a statement that is not a read must be refused");
            assert_eq!(err.kind(), KnlError::VALIDATION, "{sql:?}: {err}");
            assert!(
                err.reason().contains("SELECT or WITH"),
                "{sql:?}: {}",
                err.reason()
            );
        }

        let err = plan_of("   ").expect_err("empty SQL must be refused");
        assert_eq!(err.kind(), KnlError::VALIDATION);
    }

    /// A second statement is refused whole, rather than the first being run
    /// and the rest quietly dropped.
    #[test]
    fn a_second_statement_is_refused() {
        for sql in [
            "SELECT 1; DELETE FROM events",
            "SELECT 1;SELECT 2",
            "SELECT 1; -- a comment\n DROP TABLE events",
        ] {
            let err = plan_of(sql).expect_err("a second statement must be refused");
            assert_eq!(err.kind(), KnlError::VALIDATION, "{sql:?}: {err}");
            assert!(err.reason().contains("one statement"), "{}", err.reason());
        }
    }

    /// A `;` inside a literal or a comment is text, not the end of the
    /// statement — the scanner tells the three apart.
    #[test]
    fn a_semicolon_in_a_literal_or_a_comment_is_not_a_statement_boundary() {
        for sql in [
            r#"SELECT * FROM events WHERE kind = 'a;b'"#,
            r#"SELECT * FROM events WHERE kind = 'it''s;fine'"#,
            r#"SELECT "we;ird" FROM events"#,
            "SELECT 1 -- ; not a boundary\n",
            "SELECT /* ; */ 1",
        ] {
            plan_of(sql).unwrap_or_else(|e| panic!("{sql:?} must be one statement: {e}"));
        }
    }

    /// `$sessions` becomes one placeholder per id, and the ids are bound
    /// rather than written into the text.
    #[test]
    fn sessions_expands_to_one_named_placeholder_per_id() {
        let plan = plan_over(
            "SELECT * FROM events WHERE stream IN $sessions ORDER BY seq",
            &["stream-one", "stream-two"],
        )
        .expect("plan");
        assert_eq!(
            plan.sql,
            "SELECT * FROM events WHERE stream IN (:knl_sessions_0, :knl_sessions_1) ORDER BY seq"
        );
        assert_eq!(plan.sessions, ["stream-one", "stream-two"]);
        for id in &plan.sessions {
            assert!(
                !plan.sql.contains(id.as_str()),
                "an id is bound, never written into the SQL: {}",
                plan.sql
            );
        }

        // Omitted, the set is the session's own stream — so the same SQL
        // reads one stream by default.
        let plan = plan_of("SELECT * FROM events WHERE stream IN $sessions").expect("plan");
        assert_eq!(
            plan.sql,
            "SELECT * FROM events WHERE stream IN (:knl_sessions_0)"
        );
        assert_eq!(plan.sessions, ["s-1"]);
        assert_eq!(plan.stream, "s-1");
    }

    /// The rewrite touches the token and nothing else: an occurrence inside a
    /// literal, an identifier or a comment is left exactly as written, and so
    /// is a longer name that starts the same way.
    #[test]
    fn only_the_token_itself_is_rewritten() {
        for sql in [
            r#"SELECT '$sessions' AS literal"#,
            r#"SELECT "$sessions" FROM events"#,
            "SELECT 1 -- $sessions in a comment\n",
            "SELECT /* $sessions */ 1",
            "SELECT $sessions2",
        ] {
            let plan = plan_over(sql, &["a", "b"]).expect("plan");
            assert_eq!(plan.sql, sql, "the text must be untouched: {sql:?}");
        }

        // Two occurrences are both expanded, and the rest of the statement is
        // copied byte for byte.
        let plan = plan_over(
            "SELECT * FROM events WHERE stream IN $sessions UNION \
             SELECT * FROM events WHERE stream IN $sessions",
            &["a"],
        )
        .expect("plan");
        assert_eq!(
            plan.sql,
            "SELECT * FROM events WHERE stream IN (:knl_sessions_0) UNION \
             SELECT * FROM events WHERE stream IN (:knl_sessions_0)"
        );
    }

    /// An empty set is refused: it is a mistake in the caller's own code, and
    /// `IN ()` is not SQL anyway.
    #[test]
    fn an_empty_session_set_is_refused() {
        let err = plan_over("SELECT 1", &[]).expect_err("an empty set must be refused");
        assert_eq!(err.kind(), KnlError::VALIDATION);
        assert!(err.reason().contains("sessions"), "{}", err.reason());
    }

    /// A zero timeout is refused rather than read as "no deadline".
    #[test]
    fn a_zero_timeout_is_refused() {
        let opts = QueryOpts {
            timeout_ms: 0,
            ..QueryOpts::default()
        };
        let err = plan("SELECT 1", QueryParams::None, &opts, "s-1")
            .expect_err("a zero timeout must be refused");
        assert_eq!(err.kind(), KnlError::VALIDATION);
        assert!(err.reason().contains("timeout_ms"), "{}", err.reason());
    }

    /// The caller's own values ride along untouched; the plan carries them
    /// for the backend to bind.
    #[test]
    fn the_plan_carries_the_callers_values_for_binding() {
        let opts = QueryOpts::default();
        let plan = plan(
            "SELECT * FROM events WHERE kind = ?",
            QueryParams::Positional(vec![json!("note")]),
            &opts,
            "s-1",
        )
        .expect("plan");
        assert_eq!(plan.params, QueryParams::Positional(vec![json!("note")]));
        assert_eq!(plan.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(plan.limit, DEFAULT_LIMIT);
    }
}
