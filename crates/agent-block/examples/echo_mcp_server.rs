//! MCP echo harness — a reference server for exercising the rich MCP client.
//!
//! Provides tools / resources / prompts / logging / sampling so that every
//! capability path in the agent-block MCP bridge can be smoke-tested against
//! a real, independently-running server.
//!
//! # Usage
//!
//! ```sh
//! # stdio (default)
//! cargo run --example echo_mcp_server
//!
//! # HTTP on a fixed port
//! cargo run --example echo_mcp_server -- --transport http --port 8765
//!
//! # HTTP on an ephemeral port — prints ECHO_MCP_URL=http://127.0.0.1:<port>/mcp
//! cargo run --example echo_mcp_server -- --transport http --port 0
//!
//! # Enable periodic log notifications (5 × 1-second ticks after connect)
//! cargo run --example echo_mcp_server -- --transport http --port 0 --emit-logs
//!
//! # Ask the client to do a sampling round-trip immediately after connect
//! cargo run --example echo_mcp_server -- --transport http --port 0 --request-sampling
//! ```

use std::{
    future::Future,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

use clap::{Parser, ValueEnum};
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Content, CreateMessageRequestParams,
        GetPromptRequestParams, GetPromptResult, ListPromptsResult, ListResourcesResult,
        ListToolsResult, LoggingLevel, LoggingMessageNotificationParam, PaginatedRequestParams,
        ProgressNotificationParam, ProgressToken, Prompt, PromptArgument, PromptMessage,
        PromptMessageRole, RawResource, ReadResourceRequestParams, ReadResourceResult,
        ResourceContents, SamplingMessage, ServerCapabilities, ServerInfo, SetLevelRequestParams,
        Tool,
    },
    service::{MaybeSendFuture, RequestContext},
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use tokio_util::sync::CancellationToken;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

#[derive(Debug, Parser)]
#[command(name = "echo_mcp_server", about = "MCP echo harness for agent-block")]
struct Cli {
    /// Transport to use: stdio (default) or http.
    #[arg(long, default_value = "stdio")]
    transport: Transport,

    /// TCP port for HTTP mode. 0 means OS-assigned ephemeral port.
    #[arg(long, default_value = "8765")]
    port: u16,

    /// After a client connects, emit 5 LoggingMessageNotifications at 1-second
    /// intervals (level=info, logger="echo", data="tick N").
    #[arg(long)]
    emit_logs: bool,

    /// After a client connects, send one sampling/createMessage request to the
    /// client (prompt="say hi"). The response is logged; failures are ignored.
    #[arg(long)]
    request_sampling: bool,
}

// ── Server implementation ─────────────────────────────────────────────────────

/// Flags passed from the CLI into the server handler.
#[derive(Debug, Clone)]
struct Flags {
    emit_logs: bool,
    request_sampling: bool,
    /// Current log level as set by logging/setLevel (stored as u8 to allow
    /// atomic access without a Mutex).  Maps to `LoggingLevel` discriminants.
    log_level: Arc<AtomicU8>,
}

#[derive(Clone)]
struct EchoServer {
    flags: Flags,
}

impl EchoServer {
    fn new(flags: Flags) -> Self {
        Self { flags }
    }
}

impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .enable_logging()
                .build(),
        )
    }

    // ── tools/list ───────────────────────────────────────────────────────────

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + MaybeSendFuture + '_ {
        let tools = vec![
            Tool::new(
                "echo",
                "Return the input string unchanged",
                Arc::new(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "msg": { "type": "string", "description": "Message to echo" }
                    },
                    "required": ["msg"]
                })
                .as_object()
                .cloned()
                .unwrap_or_default()),
            ),
            Tool::new(
                "slow_echo",
                "Echo with incremental progress notifications (100 ms per step)",
                Arc::new(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "msg":   { "type": "string",  "description": "Message to echo" },
                        "steps": { "type": "integer", "description": "Number of progress steps (default 5)" }
                    },
                    "required": ["msg"]
                })
                .as_object()
                .cloned()
                .unwrap_or_default()),
            ),
        ];
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    // ── tools/call ───────────────────────────────────────────────────────────

    async fn call_tool(
        &self,
        params: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = params.arguments.clone().unwrap_or_default();

        match params.name.as_ref() {
            "echo" => {
                let msg = args
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // If --request-sampling was given, attempt a sampling round-trip.
                if self.flags.request_sampling {
                    let peer = ctx.peer.clone();
                    tokio::spawn(async move {
                        let req = CreateMessageRequestParams::new(
                            vec![SamplingMessage::user_text("say hi")],
                            256,
                        );
                        match peer.create_message(req).await {
                            Ok(resp) => {
                                eprintln!("[echo-harness] sampling response model={}", resp.model);
                            }
                            Err(e) => {
                                eprintln!("[echo-harness] sampling request failed: {e}");
                            }
                        }
                    });
                }

                // If --emit-logs was given, spawn a background task that fires
                // 5 log notifications at 1-second intervals.
                if self.flags.emit_logs {
                    let peer = ctx.peer.clone();
                    tokio::spawn(async move {
                        for n in 1u8..=5 {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            let _ = peer
                                .notify_logging_message(LoggingMessageNotificationParam {
                                    level: LoggingLevel::Info,
                                    logger: Some("echo".into()),
                                    data: serde_json::json!(format!("tick {n}")),
                                })
                                .await;
                        }
                    });
                }

                Ok(CallToolResult::success(vec![Content::text(msg)]))
            }

            "slow_echo" => {
                let msg = args
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let steps: u32 = args
                    .get("steps")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .unwrap_or(5)
                    .max(1);

                // Extract progress token from _meta if provided.
                // In rmcp 1.4.0, `_meta` is deserialized into `ctx.meta` (via
                // extensions), not `params.meta` which is always None after the
                // wire round-trip.
                let token_opt: Option<ProgressToken> = ctx.meta.get_progress_token();

                for step in 1..=steps {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                    if let Some(ref token) = token_opt {
                        let _ = ctx
                            .peer
                            .notify_progress(ProgressNotificationParam {
                                progress_token: ProgressToken(token.0.clone()),
                                progress: step as f64,
                                total: Some(steps as f64),
                                message: Some(format!("step {step}/{steps}")),
                            })
                            .await;
                    }
                }

                Ok(CallToolResult::success(vec![Content::text(msg)]))
            }

            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    // ── resources/list ───────────────────────────────────────────────────────

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + MaybeSendFuture + '_ {
        let resources = vec![
            rmcp::model::Resource::new(RawResource::new("text://hello", "hello"), None),
            rmcp::model::Resource::new(RawResource::new("text://note", "note"), None),
        ];
        std::future::ready(Ok(ListResourcesResult::with_all_items(resources)))
    }

    // ── resources/read ───────────────────────────────────────────────────────

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResult, McpError>> + MaybeSendFuture + '_ {
        let uri = request.uri.clone();
        let text = match uri.as_str() {
            "text://hello" => "hello world".to_string(),
            "text://note" => "a note".to_string(),
            other => {
                return std::future::ready(Err(McpError::invalid_params(
                    format!("unknown resource uri: {other}"),
                    None,
                )));
            }
        };
        std::future::ready(Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text, uri,
        )])))
    }

    // ── prompts/list ─────────────────────────────────────────────────────────

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListPromptsResult, McpError>> + MaybeSendFuture + '_ {
        let prompts = vec![Prompt::new(
            "greet",
            Some("Greeting prompt"),
            Some(vec![PromptArgument::new("name")
                .with_description("Name to greet")
                .with_required(true)]),
        )];
        std::future::ready(Ok(ListPromptsResult::with_all_items(prompts)))
    }

    // ── prompts/get ──────────────────────────────────────────────────────────

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<GetPromptResult, McpError>> + MaybeSendFuture + '_ {
        if request.name != "greet" {
            return std::future::ready(Err(McpError::invalid_params(
                format!("unknown prompt: {}", request.name),
                None,
            )));
        }

        let name = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("world")
            .to_string();

        let message = PromptMessage::new_text(PromptMessageRole::User, format!("hello, {name}"));
        std::future::ready(Ok(GetPromptResult::new(vec![message])))
    }

    // ── logging/setLevel ─────────────────────────────────────────────────────

    fn set_level(
        &self,
        params: SetLevelRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + MaybeSendFuture + '_ {
        self.flags
            .log_level
            .store(params.level as u8, Ordering::Relaxed);
        std::future::ready(Ok(()))
    }
}

// ── entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let flags = Flags {
        emit_logs: cli.emit_logs,
        request_sampling: cli.request_sampling,
        log_level: Arc::new(AtomicU8::new(LoggingLevel::Info as u8)),
    };

    match cli.transport {
        Transport::Stdio => run_stdio(flags).await,
        Transport::Http => run_http(flags, cli.port).await,
    }
}

async fn run_stdio(flags: Flags) {
    let server = EchoServer::new(flags);
    let transport = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    match server.serve(transport).await {
        Ok(running) => {
            if let Err(e) = running.waiting().await {
                eprintln!("[echo-harness] server error: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("[echo-harness] failed to start stdio server: {e}");
            std::process::exit(1);
        }
    }
}

async fn run_http(flags: Flags, port: u16) {
    let ct = CancellationToken::new();

    let config = StreamableHttpServerConfig::default()
        .with_sse_keep_alive(None)
        .with_cancellation_token(ct.child_token());

    let flags_clone = flags.clone();
    let service: StreamableHttpService<EchoServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(EchoServer::new(flags_clone.clone())),
            Default::default(),
            config,
        );

    let router = axum::Router::new().nest_service("/mcp", service);

    let addr = format!("127.0.0.1:{port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[echo-harness] failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };

    let bound_addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[echo-harness] local_addr failed: {e}");
            std::process::exit(1);
        }
    };

    // Print the URL so that callers (scripts / tests) can pick it up.
    println!("ECHO_MCP_URL=http://{bound_addr}/mcp");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await
        {
            eprintln!("[echo-harness] http server error: {e}");
        }
    });

    // Wait for SIGINT / SIGTERM.
    wait_for_signal().await;
    ct.cancel();
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
