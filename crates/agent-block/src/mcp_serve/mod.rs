//! `agent-block mcp` — serve registered blocks to an MCP client over stdio.
//!
//! The CLI already runs one block per process, which is enough for a human at a
//! shell but not for an agent on the other side of a conversation: a Bash
//! invocation is not in any tool list, so a model has no way to discover that a
//! block exists, and its output arrives as whatever the process printed. This
//! mode puts the same blocks where an MCP client already looks — one `run_block`
//! tool whose `block` argument enumerates what is registered, and resources
//! carrying the authoring contract and the block sources.
//!
//! The reason to route work here rather than let the caller do it: a block runs
//! against its own model with its own credentials, so its turns never enter the
//! caller's context. A caller strong at planning and review can hand the loop
//! off and get back one value.
//!
//! # Surface
//!
//! | Kind | Name | Meaning |
//! |---|---|---|
//! | tool | `run_block` | run one registered block, return what it returned |
//! | resource | `agent-block://guide` | how to write a block for this surface |
//! | resource | `agent-block://blocks` | the registry, as JSON |
//! | resource | `agent-block://blocks/<name>` | one block's source |
//!
//! # stdout
//!
//! stdio transport owns stdout, so this mode routes tracing to stderr. MCP
//! clients surface a server's stderr as its log, which is where the `ab.obs`
//! lines land.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_block_core::host::{PromptSource, ScriptSource};
use agent_block_core::{run_capture, BlockConfig};
use clap::Args;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo, Tool,
    },
    service::{MaybeSendFuture, RequestContext},
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};

/// Authoring contract, served verbatim as `agent-block://guide`.
const GUIDE: &str = include_str!("guide.md");

const GUIDE_URI: &str = "agent-block://guide";
const BLOCKS_URI: &str = "agent-block://blocks";
const BLOCK_URI_PREFIX: &str = "agent-block://blocks/";

// Declared once and used by both `resources/list` and `resources/read`. Held
// together because a listing that advertises one type while the read returns
// another is worse than declaring nothing: a client routing on the content's
// type gets the wrong answer and has no reason to doubt it.
const GUIDE_MIME: &str = "text/markdown";
const BLOCKS_MIME: &str = "application/json";
const BLOCK_MIME: &str = "text/x-lua";

/// `agent-block mcp` arguments.
#[derive(Debug, Args)]
pub struct McpArgs {
    /// Directory of `.lua` blocks to expose. Repeatable. Only `.lua` files
    /// directly inside these directories are callable.
    #[arg(long = "block-dir", value_name = "DIR", required = true)]
    pub block_dirs: Vec<PathBuf>,
}

/// One callable block: a `.lua` file in a registered directory.
#[derive(Debug, Clone)]
struct Block {
    /// File stem. The value the caller passes as `block`.
    name: String,
    path: PathBuf,
    /// The script's leading `--` comment lines, which is what the author wrote
    /// to describe it. Empty when the file opens with code.
    doc: String,
}

/// Scan the registered directories for blocks.
///
/// Called per request rather than once at startup so that dropping a `.lua`
/// file into a block directory makes it callable without a restart — the same
/// immediacy the CLI has, where a new script is simply a new `-s` argument.
///
/// Later directories do not shadow earlier ones: a duplicate name is skipped
/// and logged, because silently preferring one of two files that share a name
/// is the kind of thing that is only noticed once it has already run the wrong
/// one.
fn scan_blocks(dirs: &[PathBuf]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "block dir unreadable; skipped");
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("lua") {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };

            if let Some(existing) = blocks.iter().find(|b| b.name == name) {
                tracing::warn!(
                    block = name,
                    kept = %existing.path.display(),
                    skipped = %path.display(),
                    "duplicate block name; later directory ignored"
                );
                continue;
            }

            let doc = std::fs::read_to_string(&path)
                .map(|src| leading_doc(&src))
                .unwrap_or_default();

            blocks.push(Block {
                name: name.to_string(),
                path,
                doc,
            });
        }
    }

    blocks.sort_by(|a, b| a.name.cmp(&b.name));
    blocks
}

/// The leading `--` comment block of a Lua source, with the markers stripped.
///
/// This is the block author's own description, and it is the only thing the
/// caller sees before deciding to run it — so it is lifted into the tool
/// description rather than asking authors to repeat themselves in a manifest.
fn leading_doc(src: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() && lines.is_empty() {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("--") else {
            break;
        };
        lines.push(rest.trim_start_matches('-').trim());
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Description shown for the `run_block` tool, listing what is registered.
///
/// The names are in the input schema as an enum, but a bare enum says nothing
/// about what each block does; the authors' own header comments do.
fn run_block_description(blocks: &[Block]) -> String {
    let mut out = String::from(
        "Run one registered agent-block Lua block and return the JSON string it returned. \
         The block runs against its own model with its own credentials, so its LLM turns do \
         not enter this conversation — only its return value does.",
    );

    if blocks.is_empty() {
        out.push_str("\n\nNo blocks are registered.");
        return out;
    }

    out.push_str("\n\nRegistered blocks:\n");
    for b in blocks {
        let summary = b.doc.lines().next().unwrap_or("").trim();
        if summary.is_empty() {
            out.push_str(&format!("- {}\n", b.name));
        } else {
            out.push_str(&format!("- {}: {}\n", b.name, summary));
        }
    }
    out.push_str("\nRead agent-block://blocks/<name> for a block's full source.");
    out
}

#[derive(Clone)]
struct BlockServer {
    block_dirs: Arc<Vec<PathBuf>>,
    project_root: PathBuf,
    mcp_rpc_timeout: Duration,
}

impl BlockServer {
    fn blocks(&self) -> Vec<Block> {
        scan_blocks(&self.block_dirs)
    }

    /// Run one block to completion and hand back what it returned.
    ///
    /// Failure is split deliberately. A block that could not be run at all —
    /// unknown name, Lua error, cancelled — is a failed tool call. A block that
    /// ran and reported a negative outcome returns normally with that fact in
    /// its JSON, because only the block knows which of the two happened.
    async fn run_block(
        &self,
        params: &CallToolRequestParams,
    ) -> Result<CallToolResponse, McpError> {
        let args = params.arguments.clone().unwrap_or_default();

        let name = args
            .get("block")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::invalid_params("`block` is required", None))?;

        let blocks = self.blocks();
        let block = blocks.iter().find(|b| b.name == name).ok_or_else(|| {
            let known: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
            McpError::invalid_params(
                format!("unknown block '{name}'; registered: [{}]", known.join(", ")),
                None,
            )
        })?;

        let mut builder = BlockConfig::builder(
            ScriptSource::Path(block.path.clone()),
            self.project_root.clone(),
        )
        .mcp_rpc_timeout(self.mcp_rpc_timeout);
        if let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) {
            builder = builder.prompt(PromptSource::Inline(prompt.to_string()));
        }
        if let Some(context) = args.get("context").and_then(|v| v.as_str()) {
            builder = builder.context(PromptSource::Inline(context.to_string()));
        }

        tracing::info!(block = %block.name, path = %block.path.display(), "run_block: start");

        match run_capture(builder.build()).await {
            Ok(value) => {
                tracing::info!(block = %block.name, bytes = value.len(), "run_block: ok");
                // An empty return is legal but reads as a broken call to a
                // model looking at an empty tool result, so say which it was.
                let text = if value.is_empty() {
                    format!("block '{}' completed and returned no value", block.name)
                } else {
                    value
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into())
            }
            Err(e) => {
                tracing::warn!(block = %block.name, error = %e, "run_block: failed");
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "block '{}' failed: {e}",
                    block.name
                ))])
                .into())
            }
        }
    }
}

impl ServerHandler for BlockServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(
            "Runs agent-block Lua blocks. Read agent-block://guide before writing one; \
             agent-block://blocks lists what is registered.",
        )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_ {
        let blocks = self.blocks();
        let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();

        // `block` is an enum of the registered names rather than a free path:
        // the callable set is then exactly the registered set, and the model
        // can see it without a second round-trip.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "block": {
                    "type": "string",
                    "description": "Name of a registered block",
                    "enum": names,
                },
                "prompt": {
                    "type": "string",
                    "description": "Passed to the block as the `_PROMPT` Lua global",
                },
                "context": {
                    "type": "string",
                    "description": "Passed to the block as the `_CONTEXT` Lua global; \
                                    blocks conventionally use it as the system prompt",
                },
            },
            "required": ["block"]
        });

        let tool = Tool::new(
            "run_block",
            run_block_description(&blocks),
            Arc::new(schema.as_object().cloned().unwrap_or_default()),
        );
        std::future::ready(Ok(ListToolsResult::with_all_items(vec![tool])))
    }

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match params.name.as_ref() {
            "run_block" => self.run_block(&params).await,
            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + MaybeSendFuture + '_ {
        let mut resources = vec![
            Resource::new(GUIDE_URI, "agent-block block authoring guide")
                .with_description("How to write a block for this server, and what it returns")
                .with_mime_type(GUIDE_MIME),
            Resource::new(BLOCKS_URI, "registered blocks")
                .with_description("Every registered block with its path and header comment")
                .with_mime_type(BLOCKS_MIME),
        ];

        for b in self.blocks() {
            resources.push(
                Resource::new(format!("{BLOCK_URI_PREFIX}{}", b.name), b.name.clone())
                    .with_description(if b.doc.is_empty() {
                        format!("Lua source of block '{}'", b.name)
                    } else {
                        b.doc.lines().next().unwrap_or("").to_string()
                    })
                    .with_mime_type(BLOCK_MIME),
            );
        }

        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, McpError>> + MaybeSendFuture + '_ {
        let uri = request.uri.clone();
        std::future::ready(self.read_uri(&uri).map(|(text, mime)| {
            ReadResourceResult::new(vec![
                ResourceContents::text(text, uri.clone()).with_mime_type(mime)
            ])
            .into()
        }))
    }
}

impl BlockServer {
    /// Resolve a resource URI to its content and the type that content is,
    /// which must be the type `resources/list` advertised for the same URI.
    fn read_uri(&self, uri: &str) -> Result<(String, &'static str), McpError> {
        if uri == GUIDE_URI {
            return Ok((GUIDE.to_string(), GUIDE_MIME));
        }

        if uri == BLOCKS_URI {
            let listing: Vec<serde_json::Value> = self
                .blocks()
                .into_iter()
                .map(|b| {
                    serde_json::json!({
                        "name": b.name,
                        "path": b.path.display().to_string(),
                        "doc": b.doc,
                        "uri": format!("{BLOCK_URI_PREFIX}{}", b.name),
                    })
                })
                .collect();
            return serde_json::to_string_pretty(&serde_json::json!({ "blocks": listing }))
                .map(|json| (json, BLOCKS_MIME))
                .map_err(|e| McpError::internal_error(format!("serialize blocks: {e}"), None));
        }

        if let Some(name) = uri.strip_prefix(BLOCK_URI_PREFIX) {
            // Resolved through the registry, never by joining the name onto a
            // path: a name that was not scanned has no source to read here.
            let blocks = self.blocks();
            let block = blocks
                .iter()
                .find(|b| b.name == name)
                .ok_or_else(|| McpError::invalid_params(format!("unknown block: {name}"), None))?;
            return std::fs::read_to_string(&block.path)
                .map(|src| (src, BLOCK_MIME))
                .map_err(|e| {
                    McpError::internal_error(format!("read {}: {e}", block.path.display()), None)
                });
        }

        Err(McpError::invalid_params(
            format!("unknown resource uri: {uri}"),
            None,
        ))
    }
}

/// Serve the registered blocks over stdio until the client disconnects.
pub async fn serve(
    args: McpArgs,
    project_root: &Path,
    mcp_rpc_timeout: Duration,
) -> anyhow::Result<()> {
    let block_dirs: Vec<PathBuf> = args.block_dirs;

    for dir in &block_dirs {
        if !dir.is_dir() {
            anyhow::bail!("--block-dir '{}' is not a directory", dir.display());
        }
    }

    let server = BlockServer {
        block_dirs: Arc::new(block_dirs),
        project_root: project_root.to_path_buf(),
        mcp_rpc_timeout,
    };

    let found = server.blocks();
    tracing::info!(
        blocks = found.len(),
        names = %found.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(","),
        "agent-block mcp: serving on stdio"
    );

    let transport = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let running = server
        .serve(transport)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start the stdio MCP server: {e}"))?;

    running
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server stopped with an error: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_doc_takes_the_header_comment_and_stops_at_code() {
        let src = "-- Summarize a document.\n--\n-- Returns { ok, text }.\nlocal agent = require(\"agent\")\n-- not part of the header\n";
        assert_eq!(
            leading_doc(src),
            "Summarize a document.\n\nReturns { ok, text }."
        );
    }

    #[test]
    fn leading_doc_is_empty_when_the_file_opens_with_code() {
        assert_eq!(leading_doc("local x = 1\n-- later\n"), "");
    }

    #[test]
    fn scan_finds_lua_files_and_ignores_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.lua"), "-- second\nreturn \"\"").unwrap();
        std::fs::write(dir.path().join("a.lua"), "-- first\nreturn \"\"").unwrap();
        std::fs::write(dir.path().join("notes.md"), "not a block").unwrap();

        let blocks = scan_blocks(&[dir.path().to_path_buf()]);

        let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(blocks[0].doc, "first");
    }

    #[test]
    fn a_duplicate_name_keeps_the_first_directory() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("dup.lua"), "-- kept\nreturn \"\"").unwrap();
        std::fs::write(second.path().join("dup.lua"), "-- shadowed\nreturn \"\"").unwrap();

        let blocks = scan_blocks(&[first.path().to_path_buf(), second.path().to_path_buf()]);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].doc, "kept");
    }

    #[test]
    fn unknown_uris_are_rejected_rather_than_resolved_against_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.lua"), "-- real\nreturn \"\"").unwrap();
        let server = BlockServer {
            block_dirs: Arc::new(vec![dir.path().to_path_buf()]),
            project_root: dir.path().to_path_buf(),
            mcp_rpc_timeout: Duration::from_secs(30),
        };

        assert!(server
            .read_uri("agent-block://blocks/../../etc/passwd")
            .is_err());
        assert!(server.read_uri("file:///etc/passwd").is_err());
        assert!(server.read_uri("agent-block://blocks/real").is_ok());
    }

    /// The type a read returns is the type the listing promised. Observed
    /// live: every read came back `text/plain` while the listing advertised
    /// markdown / json / lua, because the content carried no type of its own.
    #[test]
    fn read_returns_the_type_the_listing_advertised() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.lua"), "-- real\nreturn \"\"").unwrap();
        let server = BlockServer {
            block_dirs: Arc::new(vec![dir.path().to_path_buf()]),
            project_root: dir.path().to_path_buf(),
            mcp_rpc_timeout: Duration::from_secs(30),
        };

        assert_eq!(server.read_uri(GUIDE_URI).unwrap().1, GUIDE_MIME);
        assert_eq!(server.read_uri(BLOCKS_URI).unwrap().1, BLOCKS_MIME);
        assert_eq!(
            server.read_uri("agent-block://blocks/real").unwrap().1,
            BLOCK_MIME
        );
    }

    /// The guide teaches the block contract, so a wrong field name in its
    /// example is the worst place for one. `agent.run` returns `content`; the
    /// first draft said `result.text`, and the smoke block written from it
    /// returned nothing.
    #[test]
    fn the_guide_example_uses_the_field_agent_run_actually_returns() {
        assert!(
            GUIDE.contains("text = result.content"),
            "the guide's example must read the real field"
        );
        assert!(
            !GUIDE.contains("result.text"),
            "the guide must not reference a field agent.run does not return"
        );
    }
}
