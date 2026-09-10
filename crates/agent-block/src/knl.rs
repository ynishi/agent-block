//! `agent-block knl` — read the kernel's log from another process.
//!
//! A run writes its facts to a session in the project's kernel database, and
//! everything that reads them back so far is Lua running inside the host: a
//! view, a fold, `knl.export`. That is the wrong shape for the reader that
//! wants them most — a job manager, a test, a person after the fact — because
//! it is not the process that wrote them and has no script to run. This
//! subcommand is the door for that reader: a session id in, JSON Lines out,
//! one record per line, and nothing to install.
//!
//! ```text
//! agent-block knl export --session <ID> --as events|messages [--store <PATH>]
//! ```
//!
//! Two forms, and the difference is who the answer is for:
//!
//! - `--as events` is the log as it is stored, upcast to today's shape on the
//!   way out — the whole record, every kind, nothing folded away. What a
//!   reader that means to do its own accounting wants;
//! - `--as messages` is the conversation the log holds, folded
//!   ([`messages_of`]): the four kinds a turn is made of, each as one record
//!   carrying where in the log it came from.
//!
//! # The store is the door
//!
//! The events come out through [`SqliteEventStore`] and the kernel's own
//! [`EventStore::read`], never `eventsdb` directly, because the read a caller
//! wants is the *kernel's*: the upcaster chain runs down there, so a stream
//! written under an older schema version reads back in the current shape. A
//! reader going straight at the table would get the bytes and none of that.
//!
//! # Exit codes
//!
//! `0` and the records on stdout, or a one-line `error:` on stderr and `1` —
//! the binary's ordinary failure path. A session that is not in the store is
//! that failure and not an empty answer: a reader asking for a run by id and
//! silently getting nothing back cannot tell a finished run from a typo.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Map, Value};

use agent_block_core::bridge::config::knl_path;
use agent_block_core::knl::event::{
    kind_of, seq_of, FIELD_BEAT, FIELD_DATA, FIELD_EPOCH_MS, FIELD_KIND, FIELD_META, FIELD_SEQ,
};
use agent_block_core::knl::{EventStore, Logs, SqliteEventStore};

/// How many events are read per round trip.
///
/// The read is paged rather than asked for `usize::MAX` because a session's
/// log has no bound: a long-running job's stream is as big as it got, and one
/// read of the whole of it would hold all of it twice. Large enough that an
/// ordinary run is one round trip.
const PAGE: usize = 512;

/// `agent-block knl` arguments.
#[derive(Debug, Args)]
pub struct KnlArgs {
    #[command(subcommand)]
    pub command: KnlCommand,
}

/// The verbs of `agent-block knl`.
#[derive(Debug, Subcommand)]
pub enum KnlCommand {
    /// Print one session's log as JSON Lines, one record per line.
    ///
    /// ```text
    /// agent-block knl export --session s-7 --as messages
    /// ```
    Export(ExportArgs),
}

/// `agent-block knl export` arguments.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// The session to read: the stream id `knl.open` minted for it, which is
    /// what `knl.views.sessions` and a run's own `s:id()` report.
    #[arg(long, value_name = "ID")]
    pub session: String,

    /// The kernel database to read it from.
    ///
    /// Defaults to the project's own — the file `knl.open{}` writes to when a
    /// script names no `store`, resolved from the same `-p/--project` root
    /// (and `AGENT_BLOCK_KNL_PATH` when that is set), so a reader that knows
    /// which project a run belonged to does not have to know where its
    /// database lives.
    #[arg(long, value_name = "PATH")]
    pub store: Option<PathBuf>,

    /// Which form to print: the stored events, or the conversation they hold.
    #[arg(long = "as", value_name = "FORM")]
    pub form: Form,
}

/// What `--as` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Form {
    /// Every stored event, upcast to the current shape, verbatim.
    Events,
    /// The conversation the log holds: the four kinds a turn is made of, one
    /// message record each, the rest skipped.
    Messages,
}

/// Run `agent-block knl <verb>`.
pub async fn run(args: KnlArgs, project: &Path) -> anyhow::Result<()> {
    match args.command {
        KnlCommand::Export(export) => self::export(export, project).await,
    }
}

/// `agent-block knl export`: read one session and print it.
async fn export(args: ExportArgs, project: &Path) -> anyhow::Result<()> {
    let path = match args.store {
        Some(path) => path,
        None => knl_path(project).map_err(|reason| {
            anyhow::anyhow!("the project's kernel database could not be located: {reason}")
        })?,
    };
    // Checked before opening, because opening *creates*: a mistyped path
    // would otherwise leave an empty database behind and report the session
    // missing from it, which is true and useless.
    if !path.exists() {
        anyhow::bail!("no kernel database at '{}'", path.display());
    }

    // The logs outlive the store and are shut down either way: the reader
    // threads the open started have to be joined before the process leaves,
    // and a failed read is no reason to leave them running.
    let logs = Logs::new();
    let printed = read_and_print(&path, &args.session, args.form, &logs).await;
    for failure in logs.shutdown().await {
        tracing::warn!(error = %failure, "knl export: a log did not close cleanly");
    }
    printed
}

/// Open the stream, read it whole, and write the chosen form to stdout.
async fn read_and_print(path: &Path, session: &str, form: Form, logs: &Logs) -> anyhow::Result<()> {
    let store = SqliteEventStore::open(path, session, logs)
        .await
        .with_context(|| format!("opening the kernel database at '{}'", path.display()))?;

    let events = read_whole(&store)
        .await
        .with_context(|| format!("reading the session '{session}'"))?;
    if events.is_empty() {
        anyhow::bail!(
            "no session '{session}' in '{}' (a session with no events is a session that was \
             never opened)",
            path.display()
        );
    }

    let records = match form {
        Form::Events => events,
        Form::Messages => messages_of(&events),
    };

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for record in &records {
        let line = serde_json::to_string(record).context("rendering a record as JSON")?;
        writeln!(out, "{line}").context("writing to stdout")?;
    }
    out.flush().context("writing to stdout")
}

/// Every event of the stream, in `seq` order, read a page at a time.
async fn read_whole(store: &SqliteEventStore) -> agent_block_core::knl::KnlResult<Vec<Value>> {
    let mut from = 0u64;
    let mut all: Vec<Value> = Vec::new();
    loop {
        let page = store.read(from, PAGE).await?;
        let Some(last) = page.last() else {
            break;
        };
        let next = seq_of(last).saturating_add(1);
        let was_full = page.len() >= PAGE;
        all.extend(page);
        // `next <= from` cannot happen on a well-ordered stream and is
        // checked anyway: a page that did not advance the cursor would make
        // this loop forever rather than answer.
        if !was_full || next <= from {
            break;
        }
        from = next;
    }
    Ok(all)
}

/// Fold a session's events into the `messages` form.
///
/// **This mirrors the Lua kernel's `knl.export(session, { as = "messages" })`,
/// and the two must move together.** Same rules, same record shape; a change
/// on either side without the other is a reader getting one answer from
/// inside the host and a different one from the command line. A test holding
/// the two against each other is the next round's; until it exists, this doc
/// is the contract.
///
/// One record per event, rather than the batching
/// `crates/agent-block-core/blocks/lib/knl/init.lua`'s `fold` does for a
/// provider request. A request has to be a legal conversation; this is a
/// *reading*, so what a reader wants is where in the log each part came from
/// — hence `beat` / `seq` / `epoch_ms` / `kind` on every record, and no
/// grouping that would lose them.
///
/// Four kinds map, and the rest — `tool_call`'s own `llm_request`,
/// `llm_call_failed`, `session_*`, `budget_*`, a caller's own kinds — are not
/// part of the conversation and are skipped:
///
/// | kind | `role` | `content` |
/// |---|---|---|
/// | `msg_user` | `user` | `data.content`, verbatim |
/// | `llm_response` | `assistant` | `data.content`, verbatim, plus `usage` / `stop_reason` |
/// | `tool_call` | `assistant` | a `tool_use` block: `id` / `name` / `input` |
/// | `tool_result` | `user` | a `tool_result` block, the result uncut |
///
/// The two block forms are one block and not a list of one, which is the Lua
/// side's shape: an entry is one event, so there is never a second block to
/// hold. Nothing is cut — a `tool_result`'s content is the whole of what was
/// recorded ([`result_text`]); what caps a tool's answer is the policy that
/// acts on the handler's return, before the log.
///
/// `beat` is `meta.beat` — the caller's own correlation label — and is left
/// off a record whose event carried none, because "no beat" and "the beat was
/// null" are not the same fact.
fn messages_of(events: &[Value]) -> Vec<Value> {
    events.iter().filter_map(message_of).collect()
}

/// A `tool_result`'s payload as the block's content: a string verbatim, any
/// other value as JSON text.
///
/// The Lua side's `result_text`, in Rust. The envelope already carries the
/// rest, so this only has to answer "what did the tool say" in one type — and
/// it is the *whole* answer, however long.
fn result_text(result: Option<Value>) -> Value {
    match result {
        Some(Value::String(text)) => Value::String(text),
        Some(other) => Value::String(other.to_string()),
        // A `tool_result` carries a `result` by its own shape, so this is the
        // malformed record: encoded rather than dropped, for the same reason
        // the fold reads a missing `data` as an empty one.
        None => Value::String(Value::Null.to_string()),
    }
}

/// One event as one message record, or `None` when it is not part of the
/// conversation.
fn message_of(event: &Value) -> Option<Value> {
    let data = event.get(FIELD_DATA);
    let field = |name: &str| data.and_then(|data| data.get(name)).cloned();
    let content = || field("content").unwrap_or(Value::Null);

    let kind = kind_of(event);
    let mut record = match kind {
        "msg_user" => json!({ "role": "user", "content": content() }),
        "llm_response" => {
            let mut record = json!({ "role": "assistant", "content": content() });
            for name in ["usage", "stop_reason"] {
                if let (Some(value), Some(object)) = (field(name), record.as_object_mut()) {
                    object.insert(name.to_string(), value);
                }
            }
            record
        }
        "tool_call" => {
            let mut block = Map::new();
            block.insert("type".to_string(), json!("tool_use"));
            block.insert("id".to_string(), field("call_id").unwrap_or(Value::Null));
            block.insert("name".to_string(), field("name").unwrap_or(Value::Null));
            block.insert("input".to_string(), field("args").unwrap_or(Value::Null));
            json!({ "role": "assistant", "content": Value::Object(block) })
        }
        "tool_result" => {
            let mut block = Map::new();
            block.insert("type".to_string(), json!("tool_result"));
            block.insert(
                "tool_use_id".to_string(),
                field("call_id").unwrap_or(Value::Null),
            );
            block.insert("content".to_string(), result_text(field("result")));
            // Present only when the tool said it failed, which is the same
            // rule the request fold follows: an absent `is_error` is not a
            // claim that the call went well.
            if field("ok") == Some(Value::Bool(false)) {
                block.insert("is_error".to_string(), json!(true));
            }
            json!({ "role": "user", "content": Value::Object(block) })
        }
        _ => return None,
    };

    let object = record.as_object_mut()?;
    if let Some(beat) = event
        .get(FIELD_META)
        .and_then(|meta| meta.get(FIELD_BEAT))
        .cloned()
    {
        object.insert(FIELD_BEAT.to_string(), beat);
    }
    object.insert(FIELD_SEQ.to_string(), json!(seq_of(event)));
    if let Some(epoch_ms) = event.get(FIELD_EPOCH_MS).cloned() {
        object.insert(FIELD_EPOCH_MS.to_string(), epoch_ms);
    }
    object.insert(FIELD_KIND.to_string(), json!(kind));
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::messages_of;
    use serde_json::{json, Value};

    /// An event as the store hands one back: the kernel's coordinates at the
    /// top, the caller's labels under `meta`, the kind's own content under
    /// `data`.
    fn stored(seq: u64, kind: &str, beat: Option<&str>, data: Value) -> Value {
        json!({
            "kind": kind,
            "seq": seq,
            "epoch_ms": 1_700_000_000_000u64 + seq,
            "meta": match beat {
                Some(beat) => json!({ "beat": beat }),
                None => json!({}),
            },
            "data": data,
        })
    }

    /// The four kinds a turn is made of, each as one record carrying where in
    /// the log it came from.
    #[test]
    fn the_four_kinds_fold_to_the_messages_they_are() {
        let events = [
            stored(3, "msg_user", None, json!({ "content": "hi" })),
            stored(
                4,
                "llm_response",
                Some("b1"),
                json!({
                    "content": [{ "type": "text", "text": "on it" }],
                    "usage": { "input_tokens": 7, "output_tokens": 2 },
                    "stop_reason": "tool_use",
                }),
            ),
            stored(
                5,
                "tool_call",
                Some("b1"),
                json!({ "call_id": "c-1", "name": "sh", "args": { "cmd": "ls" } }),
            ),
            stored(
                6,
                "tool_result",
                Some("b1"),
                json!({ "call_id": "c-1", "ok": true, "result": { "out": "a\nb" } }),
            ),
        ];

        let messages = messages_of(&events);
        assert_eq!(
            messages,
            [
                json!({
                    "role": "user", "content": "hi",
                    "seq": 3, "epoch_ms": 1_700_000_000_003u64, "kind": "msg_user",
                }),
                json!({
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "on it" }],
                    "usage": { "input_tokens": 7, "output_tokens": 2 },
                    "stop_reason": "tool_use",
                    "beat": "b1", "seq": 4, "epoch_ms": 1_700_000_000_004u64,
                    "kind": "llm_response",
                }),
                json!({
                    "role": "assistant",
                    "content": {
                        "type": "tool_use", "id": "c-1", "name": "sh",
                        "input": { "cmd": "ls" },
                    },
                    "beat": "b1", "seq": 5, "epoch_ms": 1_700_000_000_005u64,
                    "kind": "tool_call",
                }),
                json!({
                    "role": "user",
                    "content": {
                        "type": "tool_result", "tool_use_id": "c-1",
                        // A result that is not a string is JSON text, which
                        // is the one type a message's content can be.
                        "content": "{\"out\":\"a\\nb\"}",
                    },
                    "beat": "b1", "seq": 6, "epoch_ms": 1_700_000_000_006u64,
                    "kind": "tool_result",
                }),
            ]
        );
    }

    /// A tool that failed says so on the block, and one that did not says
    /// nothing: an absent `is_error` is not a claim either way.
    #[test]
    fn a_failed_tool_result_is_marked_and_a_good_one_is_not() {
        let events = [
            stored(
                1,
                "tool_result",
                None,
                json!({ "call_id": "c-1", "ok": false, "result": "boom" }),
            ),
            stored(
                2,
                "tool_result",
                None,
                json!({ "call_id": "c-2", "ok": true, "result": "fine" }),
            ),
        ];

        let messages = messages_of(&events);
        assert_eq!(messages[0]["content"]["is_error"], json!(true));
        assert_eq!(
            messages[1]["content"].get("is_error"),
            None,
            "a call that went well carries no mark: {}",
            messages[1]
        );
    }

    /// A string result travels verbatim, however big: a reading of the log
    /// that cut a tool's answer short would be answering a different question
    /// than the one asked.
    #[test]
    fn a_tool_results_content_is_uncut() {
        let long = "x".repeat(50_000);
        let events = [stored(
            1,
            "tool_result",
            None,
            json!({ "call_id": "c-1", "ok": true, "result": long.clone() }),
        )];

        let messages = messages_of(&events);
        assert_eq!(messages[0]["content"]["content"], json!(long));
    }

    /// Everything that is not part of the conversation is skipped — the
    /// boundaries, the ledger, the call that never came off, and a caller's
    /// own kinds — rather than folded into a message with nothing in it.
    #[test]
    fn the_kinds_that_are_not_a_conversation_are_skipped() {
        let events = [
            stored(1, "session_opened", None, json!({ "owner": "u" })),
            stored(2, "budget_granted", None, json!({ "amount": 8 })),
            stored(3, "llm_request", None, json!({ "request": {} })),
            stored(4, "llm_call_failed", None, json!({ "error": "nope" })),
            stored(5, "note", None, json!({ "text": "mine" })),
            stored(6, "msg_user", None, json!({ "content": "hi" })),
            stored(7, "session_closed", None, json!({ "reason": "done" })),
        ];

        let messages = messages_of(&events);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0]["kind"], json!("msg_user"));
    }

    /// An event with no `data` folds like the rest instead of raising: a
    /// record that lost its content is still a record, and the reading says
    /// so with a null rather than stopping the export.
    #[test]
    fn an_event_with_no_data_still_folds() {
        let events = [json!({ "kind": "msg_user", "seq": 1, "meta": {} })];
        let messages = messages_of(&events);
        assert_eq!(
            messages,
            [json!({ "role": "user", "content": Value::Null, "seq": 1, "kind": "msg_user" })]
        );
    }
}
