//! A local MCP server over the board, served from inside the running app.
//!
//! It exists so an external agent — a terminal Claude Code session, say — can
//! see what is on the board and file work onto it without the user switching
//! apps. It has to live *in* the app process: redb takes an exclusive `flock`
//! on `usine.db`, so no second process can open the database while Usine is
//! running (the same constraint behind [`crate::CoreError::is_db_locked`]).
//!
//! The surface is deliberately small — read projects, cards, and plans; create
//! projects and cards. Nothing here can start an agent run, so reaching the
//! socket does not buy arbitrary code execution.
//!
//! Transport is newline-delimited JSON-RPC (MCP's stdio framing) over a Unix
//! socket at `<data_dir>/mcp.sock`, mode 0600. Clients get there through
//! `usine-cli mcp`, a stdio relay. Living under [`crate::infra::paths::data_dir`]
//! means a `USINE_DATA_DIR`-isolated instance automatically gets its own socket,
//! exactly as it gets its own database.
//!
//! The whole module is behind the `mcp` cargo feature (on by default);
//! `--no-default-features` compiles it, and tokio's net stack, out.

mod jsonrpc;
mod tools;

use std::path::{Path, PathBuf};

use futures::channel::mpsc::UnboundedSender;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::infra::persistence::Store;
use crate::ExecutorCommand;

/// Longest line we will read from a client. MCP messages are small; the cap
/// keeps a malformed or hostile peer from growing our buffer without bound.
const MAX_LINE: u64 = 1024 * 1024;

/// `<data_dir>/mcp.sock`, or `mcp-demo.sock` in demo mode — mirroring
/// [`crate::infra::paths::store_path`], so a demo run can never answer for the
/// real board.
pub fn socket_path(demo: bool) -> PathBuf {
    let name = if demo { "mcp-demo.sock" } else { "mcp.sock" };
    crate::infra::paths::data_dir().join(name)
}

/// What the tools act on: the store for reads, the executor's command channel
/// for writes. Sending on that channel is what keeps an open board in sync —
/// the executor persists and echoes an event the UI applies.
pub struct Ctx {
    pub store: Store,
    pub cmd_tx: UnboundedSender<ExecutorCommand>,
}

impl Ctx {
    fn send(&self, cmd: ExecutorCommand) -> std::result::Result<(), String> {
        self.cmd_tx
            .unbounded_send(cmd)
            .map_err(|_| "the executor is no longer accepting commands".to_string())
    }
}

/// Start the server on its own thread and runtime, and forget about it.
///
/// Own thread for the same reason the executor has one: nothing about a client
/// on this socket should be able to wedge the runtime that drives agent runs.
/// Every failure is logged and left there — an MCP that won't start must not
/// stop the app from running.
pub fn spawn(store: Store, cmd_tx: UnboundedSender<ExecutorCommand>, demo: bool) {
    let path = socket_path(demo);
    let spawned = std::thread::Builder::new()
        .name("usine-mcp".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!("mcp: could not build runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                if let Err(e) = serve(path, Ctx { store, cmd_tx }).await {
                    tracing::warn!("mcp: server stopped: {e}");
                }
            });
        });
    if let Err(e) = spawned {
        tracing::warn!("mcp: could not spawn thread: {e}");
    }
}

/// Bind `path` and serve until the future is dropped. Separated from [`spawn`]
/// so tests can drive a real socket without a thread.
pub async fn serve(path: PathBuf, ctx: Ctx) -> std::io::Result<()> {
    let listener = bind(&path).await?;
    tracing::info!("mcp: listening on {}", path.display());
    let ctx = std::sync::Arc::new(ctx);
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("mcp: accept failed: {e}");
                continue;
            }
        };
        let ctx = std::sync::Arc::clone(&ctx);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, &ctx).await {
                tracing::debug!("mcp: connection ended: {e}");
            }
        });
    }
}

/// Take the socket path, refusing to steal a live one.
///
/// A leftover socket file after a crash is ordinary and must not lock us out,
/// but a *connectable* one means another Usine instance sharing this data dir
/// already owns the surface — quietly rebinding would silently hijack its
/// clients. So we probe first: connect succeeds → back off; connect fails →
/// the file is stale, remove and bind.
async fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "{} is already served by another Usine instance",
                    path.display()
                ),
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    // Owner-only: the socket is a write path onto the user's board.
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

/// Serve one client. Requests are handled strictly in order rather than
/// concurrently: with a surface this small the only slow tool is a create
/// waiting on the executor, and sequential handling is what lets a client read
/// back what it just wrote on the next line.
async fn handle_conn(stream: UnixStream, ctx: &Ctx) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).take(MAX_LINE).lines();
    loop {
        let line = match lines.next_line().await? {
            Some(l) => l,
            None => return Ok(()),
        };
        // `take` caps the whole stream, so re-arm the budget after each line
        // rather than letting one connection run out of it mid-session.
        lines.get_mut().set_limit(MAX_LINE);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line, ctx).await {
            write.write_all(response.as_bytes()).await?;
            write.write_all(b"\n").await?;
            write.flush().await?;
        }
    }
}

/// The whole protocol, as a pure-ish function over one line. `None` means "say
/// nothing" (a notification). Kept socket-free so tests can drive it directly.
pub async fn handle_line(line: &str, ctx: &Ctx) -> Option<String> {
    let req = match jsonrpc::parse(line) {
        Ok(r) => r,
        Err(response) => return Some(response.to_string()),
    };
    // Notifications (`notifications/initialized`, `notifications/cancelled`, …)
    // carry no id and must never be answered.
    let id = req.id?;

    let response = match req.method.as_str() {
        "initialize" => jsonrpc::ok(id, initialize(&req.params)),
        "ping" => jsonrpc::ok(id, json!({})),
        "tools/list" => jsonrpc::ok(id, json!({ "tools": tools::list() })),
        "tools/call" => match req.params.get("name").and_then(Value::as_str) {
            Some(name) => {
                let args = req
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                jsonrpc::ok(id, tool_result(ctx, name, &args).await)
            }
            None => jsonrpc::err(id, jsonrpc::INVALID_PARAMS, "missing tool `name`"),
        },
        other => jsonrpc::err(
            id,
            jsonrpc::METHOD_NOT_FOUND,
            format!("unknown method `{other}`"),
        ),
    };
    Some(response.to_string())
}

fn initialize(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .and_then(|v| {
            jsonrpc::PROTOCOL_VERSIONS
                .iter()
                .find(|k| **k == v)
                .copied()
        })
        .unwrap_or_else(jsonrpc::latest_protocol);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "usine", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// A tool that fails answers `isError: true` with the reason in the content,
/// not a JSON-RPC error — the message is for the model to read and retry from
/// ("`foo` matches 2 cards — use an id"), not a transport fault.
async fn tool_result(ctx: &Ctx, name: &str, args: &Value) -> Value {
    match tools::call(ctx, name, args).await {
        Ok(value) => content(
            serde_json::to_string_pretty(&value).unwrap_or_else(|e| e.to_string()),
            false,
        ),
        Err(message) => content(message, true),
    }
}

fn content(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}
