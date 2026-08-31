//! The MCP surface: protocol handshake, tool contract, and the two create
//! tools' route onto the executor's command channel.
//!
//! The executor itself is not spawned — the tests stand in for its persistence
//! handlers by draining the command channel and applying `AddProject` /
//! `CreateCard` to the store. That is the point as much as a convenience: if a
//! create tool ever wrote to the store directly, the drained channel would be
//! empty and these tests would fail, which is exactly the regression to catch
//! (a direct write would never reach the open board's reducer).
#![cfg(all(feature = "mcp", unix))]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt;
use serde_json::{json, Value};
use usine_core::mcp::{handle_line, serve, socket_path, Ctx};
use usine_core::{Card, CardState, DesignSub, ExecutorCommand, Project, ProjectConfig, Store};

/// A `Ctx` whose command channel is drained by a task that applies the
/// persistence commands, standing in for the executor's own handlers.
fn ctx(store: Store) -> Ctx {
    let (tx, mut rx) = mpsc::unbounded::<ExecutorCommand>();
    let sink = store.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.next().await {
            match cmd {
                ExecutorCommand::CreateCard { card } => {
                    sink.upsert_card(&card).unwrap();
                }
                ExecutorCommand::AddProject { project } => {
                    sink.upsert_project(&project).unwrap();
                }
                other => panic!("MCP sent an unexpected command: {other:?}"),
            }
        }
    });
    Ctx { store, cmd_tx: tx }
}

async fn rpc(ctx: &Ctx, method: &str, params: Value) -> Value {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let out = handle_line(&line.to_string(), ctx)
        .await
        .expect("a request with an id must be answered");
    serde_json::from_str(&out).expect("response is JSON")
}

/// Call a tool and return `(parsed result, is_error)`.
async fn call(ctx: &Ctx, name: &str, args: Value) -> (Value, bool) {
    let resp = rpc(
        ctx,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    )
    .await;
    let result = &resp["result"];
    let text = result["content"][0]["text"]
        .as_str()
        .expect("text content")
        .to_string();
    let is_error = result["isError"].as_bool().unwrap();
    let parsed = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (parsed, is_error)
}

fn seed(store: &Store) -> (Project, Card) {
    let project = Project::new("Demo", PathBuf::from("/tmp/demo"), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let settings = store.settings().unwrap();
    let mut card = Card::new(
        project.id,
        "Fix the widget",
        "make it work",
        settings.new_card_config(),
    );
    card.state = CardState::Designing(DesignSub::AwaitingApproval {
        plan: "step one\nstep two".into(),
    });
    store.upsert_card(&card).unwrap();
    store.save_plan(card.id, "step one\nstep two").unwrap();
    (project, card)
}

/// Connect once the server is really serving. A bare connect is not enough: a
/// socket whose listener has just closed still accepts one for a moment (the
/// very race `serve`'s own liveness probe exists for), so round-trip a `ping`
/// before handing the stream back.
async fn connect_ready(path: &std::path::Path) -> tokio::net::UnixStream {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    for _ in 0..200 {
        if let Ok(mut stream) = tokio::net::UnixStream::connect(path).await {
            let pinged = async {
                stream
                    .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"ping\"}\n")
                    .await
                    .ok()?;
                let mut line = String::new();
                BufReader::new(&mut stream)
                    .read_line(&mut line)
                    .await
                    .ok()?;
                (!line.is_empty()).then_some(())
            }
            .await;
            if pinged.is_some() {
                return stream;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no server answered on {}", path.display());
}

#[tokio::test]
async fn initialize_echoes_a_supported_protocol_and_advertises_tools() {
    let ctx = ctx(Store::open_in_memory().unwrap());
    let resp = rpc(
        &ctx,
        "initialize",
        json!({ "protocolVersion": "2024-11-05" }),
    )
    .await;
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert!(resp["result"]["capabilities"]["tools"].is_object());
    assert_eq!(resp["result"]["serverInfo"]["name"], "usine");

    // An unknown version is answered with ours rather than refused.
    let resp = rpc(
        &ctx,
        "initialize",
        json!({ "protocolVersion": "1999-01-01" }),
    )
    .await;
    assert_ne!(resp["result"]["protocolVersion"], "1999-01-01");
    assert!(resp["result"]["protocolVersion"].is_string());
}

#[tokio::test]
async fn tools_list_is_the_six_tools_with_object_schemas() {
    let ctx = ctx(Store::open_in_memory().unwrap());
    let resp = rpc(&ctx, "tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "list_projects",
            "list_cards",
            "get_card",
            "get_plan",
            "create_project",
            "create_card",
        ]
    );
    for t in &tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        assert!(t["description"].is_string());
    }
    // Nothing that could start an agent leaked into the surface.
    assert!(!names
        .iter()
        .any(|n| n.contains("start") || n.contains("run")));
}

#[tokio::test]
async fn notifications_get_no_response_and_unknown_methods_error() {
    let ctx = ctx(Store::open_in_memory().unwrap());
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert!(handle_line(&note.to_string(), &ctx).await.is_none());

    let resp = rpc(&ctx, "tools/nope", json!({})).await;
    assert_eq!(resp["error"]["code"], -32601);
    assert_eq!(resp["id"], 1);

    let bad = handle_line("{not json", &ctx).await.unwrap();
    let bad: Value = serde_json::from_str(&bad).unwrap();
    assert_eq!(bad["error"]["code"], -32700);
}

#[tokio::test]
async fn reads_report_the_board() {
    let store = Store::open_in_memory().unwrap();
    let (project, card) = seed(&store);
    let ctx = ctx(store);

    let (projects, err) = call(&ctx, "list_projects", json!({})).await;
    assert!(!err);
    assert_eq!(projects["projects"][0]["name"], "Demo");
    assert_eq!(projects["projects"][0]["card_count"], 1);

    let (cards, _) = call(&ctx, "list_cards", json!({ "project": "Demo" })).await;
    let first = &cards["cards"][0];
    assert_eq!(first["title"], "Fix the widget");
    assert_eq!(
        first["status"],
        CardState::Designing(DesignSub::AwaitingApproval {
            plan: String::new()
        })
        .status_label()
    );
    assert_eq!(first["column"], "Designing");
    assert_eq!(first["needs_attention"], true);

    // Reference by id prefix, and the detail fields `get_card` adds.
    let prefix = card.id.to_string()[..8].to_string();
    let (detail, _) = call(&ctx, "get_card", json!({ "card": prefix })).await;
    assert_eq!(detail["description"], "make it work");
    assert_eq!(detail["has_plan"], true);
    assert_eq!(detail["pr"], Value::Null);

    let (plan, _) = call(&ctx, "get_plan", json!({ "card": "Fix the widget" })).await;
    assert_eq!(plan["plan"], "step one\nstep two");
    assert_eq!(plan["card_id"], card.id.to_string());

    // An unfiltered listing sees the same card; an unknown project does not.
    let (all, _) = call(&ctx, "list_cards", json!({})).await;
    assert_eq!(all["cards"].as_array().unwrap().len(), 1);
    let (msg, err) = call(&ctx, "list_cards", json!({ "project": "nope" })).await;
    assert!(err, "{msg}");
    assert_eq!(project.id, card.project_id);
}

#[tokio::test]
async fn create_card_goes_through_the_command_channel() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = seed(&store);
    let ctx = ctx(store.clone());

    let (created, err) = call(
        &ctx,
        "create_card",
        json!({ "project": project.id.to_string(), "title": "  New work  ", "description": "do it" }),
    )
    .await;
    assert!(!err, "{created}");
    assert_eq!(created["title"], "New work", "title is trimmed");

    let id = created["id"].as_str().unwrap().parse().unwrap();
    let card = store
        .get_card(id)
        .expect("persisted via the executor channel");
    assert_eq!(card.state, CardState::StartingBlock, "created, not started");
    assert_eq!(card.description, "do it");
    assert_eq!(card.project_id, project.id);

    let (msg, err) = call(
        &ctx,
        "create_card",
        json!({ "project": project.id.to_string(), "title": "   " }),
    )
    .await;
    assert!(err, "a blank title is refused");
    assert!(msg.as_str().unwrap().contains("title"));
}

#[tokio::test]
async fn create_project_checks_the_path_before_adding_it() {
    let store = Store::open_in_memory().unwrap();
    let ctx = ctx(store.clone());

    let (msg, err) = call(&ctx, "create_project", json!({ "path": "relative/path" })).await;
    assert!(err && msg.as_str().unwrap().contains("absolute"));

    let plain = tempfile::tempdir().unwrap();
    let (msg, err) = call(
        &ctx,
        "create_project",
        json!({ "path": plain.path().to_string_lossy() }),
    )
    .await;
    assert!(err, "a directory that isn't a repo is refused");
    assert!(msg.as_str().unwrap().contains("git repository"));

    let repo = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo.path())
        .status()
        .unwrap()
        .success());
    let path = repo.path().to_string_lossy().to_string();
    let (added, err) = call(&ctx, "create_project", json!({ "path": &path })).await;
    assert!(!err, "{added}");
    assert!(!added["base_branch"].as_str().unwrap().is_empty());
    let id = added["id"].as_str().unwrap().parse().unwrap();
    assert!(store.get_project(id).is_ok());

    // The same repo a second time is a duplicate, not a second project.
    let (msg, err) = call(&ctx, "create_project", json!({ "path": &path })).await;
    assert!(err, "{msg}");
    assert!(msg.as_str().unwrap().contains("already on the board"));
}

#[tokio::test]
async fn an_ambiguous_reference_lists_the_candidates() {
    let store = Store::open_in_memory().unwrap();
    let (project, _) = seed(&store);
    let settings = store.settings().unwrap();
    let twin = Card::new(project.id, "Fix the widget", "", settings.new_card_config());
    store.upsert_card(&twin).unwrap();
    let ctx = ctx(store);

    let (msg, err) = call(&ctx, "get_card", json!({ "card": "Fix the widget" })).await;
    assert!(err);
    let msg = msg.as_str().unwrap();
    assert!(msg.contains("matches 2 card"), "{msg}");
    assert!(msg.contains(&twin.id.to_string()[..8]), "{msg}");
}

#[tokio::test]
async fn socket_round_trip_and_stale_socket_replacement() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let store = Store::open_in_memory().unwrap();
    seed(&store);

    // A stale socket file (nothing listening) must be replaced, not fatal.
    std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(std::fs::File::open(&path));

    let server = tokio::spawn(serve(path.clone(), ctx(store)));
    // Bound over the stale socket, and answering.
    let (read, mut write) = connect_ready(&path).await.into_split();
    let mut lines = BufReader::new(read).lines();
    for req in [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "list_projects", "arguments": {} } }),
    ] {
        write
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        write.flush().await.unwrap();
    }
    // One JSON object per line, in order.
    let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(first["id"], 1);
    let second: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(second["id"], 2);
    assert!(second["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Demo"));

    // A second server must not steal a live socket.
    let second_store = Store::open_in_memory().unwrap();
    let err = serve(path.clone(), ctx(second_store)).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

    server.abort();
}

/// The socket is a write path onto the board, so it must be owner-only from the
/// moment it is connectable — never briefly world-accessible under the umask.
#[tokio::test]
async fn the_socket_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let server = tokio::spawn(serve(path.clone(), ctx(Store::open_in_memory().unwrap())));
    drop(connect_ready(&path).await);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket mode is {mode:o}");
    // No leftover from the bind-then-rename dance.
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n != "mcp.sock")
        .collect();
    assert!(strays.is_empty(), "left behind {strays:?}");
    server.abort();
}

/// An oversized message is reported once and the connection closed, rather than
/// being served back as a truncated fragment that parses as garbage forever.
#[tokio::test]
async fn an_oversized_message_is_rejected_and_hangs_up() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.sock");
    let server = tokio::spawn(serve(path.clone(), ctx(Store::open_in_memory().unwrap())));
    let (read, mut write) = connect_ready(&path).await.into_split();
    let mut lines = BufReader::new(read).lines();

    let huge = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping",
                       "params": { "pad": "x".repeat(2 * 1024 * 1024) } });
    // The peer may hang up mid-write once it has seen enough; that is the point.
    let _ = write.write_all(format!("{huge}\n").as_bytes()).await;
    let _ = write.flush().await;

    let response: Value =
        serde_json::from_str(&lines.next_line().await.unwrap().expect("an answer")).unwrap();
    assert_eq!(response["error"]["code"], -32700);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("exceeds"));
    assert!(
        lines.next_line().await.unwrap().is_none(),
        "connection should be closed after an oversized message"
    );
    server.abort();
}

#[test]
fn the_demo_socket_is_distinct_from_the_real_one() {
    assert_ne!(socket_path(true), socket_path(false));
    assert!(socket_path(true).to_string_lossy().contains("demo"));
}
