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

/// How long the accept loop waits after a failure, doubling up to the cap.
const ACCEPT_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_millis(50);
const ACCEPT_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(5);

/// How long we keep swallowing a hung-up-on peer's leftovers before giving up.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    // Most accept errors are per-connection and harmless, but fd exhaustion
    // (EMFILE/ENFILE) is per-process and persists: retrying it flat out would
    // spin this thread at 100% CPU and bury the log. So back off, and only log
    // the first failure of a run.
    let mut backoff = ACCEPT_BACKOFF_MIN;
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => {
                backoff = ACCEPT_BACKOFF_MIN;
                pair
            }
            Err(e) => {
                if backoff == ACCEPT_BACKOFF_MIN {
                    tracing::warn!("mcp: accept failed ({e}) — backing off");
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
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
/// the file is stale, and the bind below renames straight over it.
async fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if path.exists() && UnixStream::connect(path).await.is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!(
                "{} is already served by another Usine instance",
                path.display()
            ),
        ));
    }
    // The socket is a write path onto the user's board, so it must never be
    // reachable by another local user — not even briefly. `bind` applies the
    // ambient umask, so binding straight onto `path` would leave a
    // world-connectable socket until the chmod lands. Bind a private temp name
    // instead, tighten it, and only then rename it into place; the rename is
    // atomic and also clears any stale file left by a crash.
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("mcp.sock"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp)?;
    // A socket we cannot lock down is not one we are willing to serve.
    if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::new(
            e.kind(),
            format!("could not restrict {} to owner-only: {e}", tmp.display()),
        ));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(listener)
}

/// Serve one client. Requests are handled strictly in order rather than
/// concurrently: with a surface this small the only slow tool is a create
/// waiting on the executor, and sequential handling is what lets a client read
/// back what it just wrote on the next line.
async fn handle_conn(stream: UnixStream, ctx: &Ctx) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    // One byte of slack over the cap plus one for the newline, so a line of
    // exactly `MAX_LINE` bytes still leaves budget and doesn't read as an
    // overflow.
    let budget = MAX_LINE + 2;
    let mut lines = BufReader::new(read).take(budget).lines();
    loop {
        let line = match lines.next_line().await? {
            Some(l) => l,
            None => return Ok(()),
        };
        // A line that ate the whole budget was cut short by the cap: `Take`
        // reports EOF mid-line, so the fragment above only *looks* like a
        // message. Say so and hang up — the rest of the oversized line is still
        // queued, and resuming mid-message would spray parse errors instead.
        if lines.get_ref().limit() == 0 {
            let response = jsonrpc::err(
                Value::Null,
                jsonrpc::PARSE_ERROR,
                format!("message exceeds the {MAX_LINE} byte limit; closing connection"),
            );
            write.write_all(response.to_string().as_bytes()).await?;
            write.write_all(b"\n").await?;
            write.flush().await?;
            // Half-close so the peer sees the end of the stream, then swallow
            // the tail of the oversized message. Dropping the socket with data
            // still queued on the receive side makes the kernel answer with a
            // reset, which throws away the explanation we just sent before the
            // peer can read it.
            write.shutdown().await?;
            drain(lines.into_inner().into_inner()).await;
            return Ok(());
        }
        // `take` caps the whole stream, so re-arm the budget after each line
        // rather than letting one connection run out of it mid-session.
        lines.get_mut().set_limit(budget);
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

/// Read and discard whatever the peer still has in flight, so the socket can
/// close gracefully instead of with a reset. Bounded in time as well as by EOF:
/// a peer that keeps talking after being hung up on gets dropped anyway.
async fn drain(mut read: impl AsyncReadExt + Unpin) {
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, async {
        let mut scratch = [0u8; 8 * 1024];
        while matches!(read.read(&mut scratch).await, Ok(n) if n > 0) {}
    })
    .await;
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
