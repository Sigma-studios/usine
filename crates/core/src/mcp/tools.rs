//! The six tools the MCP surface exposes, and their dispatch.
//!
//! Reads go straight to the [`Store`] (redb allows concurrent readers inside
//! the process, and the handle is a cheap `Arc` clone). Writes go through
//! [`ExecutorCommand`] exactly like the UI's own CRUD, so the executor persists
//! them and echoes an event the open board applies — a card created here shows
//! up without a restart.
//!
//! Deliberately absent: anything that starts an agent. The socket can add work
//! to the board, not execute code.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use uuid::Uuid;

use super::Ctx;
use crate::domain::config::ProjectConfig;
use crate::domain::model::{Card, Project};
use crate::infra::persistence::Store;
use crate::ExecutorCommand;

/// How long a create tool waits for the executor to persist its record before
/// answering. The command is dispatched inline and in order, so this is a
/// handful of milliseconds in practice; the ceiling only exists so a wedged
/// executor can't hang the client.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
const CONFIRM_POLL: Duration = Duration::from_millis(25);

/// The `tools/list` payload. Static — the schemas describe the tool contract,
/// which is deliberately narrower and flatter than the domain model.
pub fn list() -> Value {
    json!([
        {
            "name": "list_projects",
            "description": "List the projects on the Usine board.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
        {
            "name": "list_cards",
            "description": "List cards with their status and column, newest activity first. \
                            Omit `project` for every card on the board.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project id (or id prefix) or exact name." },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "get_card",
            "description": "Full detail for one card: description, state, branch, PR, provider.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "card": { "type": "string", "description": "Card id (or id prefix) or exact title." },
                },
                "required": ["card"],
                "additionalProperties": false,
            },
        },
        {
            "name": "get_plan",
            "description": "The plan an agent produced for a card, or null if it has none yet.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "card": { "type": "string", "description": "Card id (or id prefix) or exact title." },
                },
                "required": ["card"],
                "additionalProperties": false,
            },
        },
        {
            "name": "create_project",
            "description": "Add a local git repository to the board as a project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the git repository." },
                    "name": { "type": "string", "description": "Display name. Defaults to the directory name." },
                },
                "required": ["path"],
                "additionalProperties": false,
            },
        },
        {
            "name": "create_card",
            "description": "File a new card in a project's starting block. The card is created \
                            but NOT started — the user starts it from the board.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project id (or id prefix) or exact name." },
                    "title": { "type": "string" },
                    "description": { "type": "string", "description": "The task prompt handed to the agent." },
                },
                "required": ["project", "title"],
                "additionalProperties": false,
            },
        },
    ])
}

/// Run one tool. `Err(String)` is a tool-level failure: it comes back as
/// `isError: true` with the message, so the calling model can correct itself,
/// rather than as a JSON-RPC error (which clients surface as a hard fault).
pub async fn call(ctx: &Ctx, name: &str, args: &Value) -> std::result::Result<Value, String> {
    match name {
        "list_projects" => list_projects(ctx),
        "list_cards" => list_cards(ctx, opt_str(args, "project")?),
        "get_card" => get_card(ctx, req_str(args, "card")?),
        "get_plan" => get_plan(ctx, req_str(args, "card")?),
        "create_project" => {
            create_project(ctx, req_str(args, "path")?, opt_str(args, "name")?).await
        }
        "create_card" => {
            create_card(
                ctx,
                req_str(args, "project")?,
                req_str(args, "title")?,
                opt_str(args, "description")?.unwrap_or_default(),
            )
            .await
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

fn list_projects(ctx: &Ctx) -> std::result::Result<Value, String> {
    let projects = ctx.store.list_projects().map_err(|e| e.to_string())?;
    let cards = ctx.store.list_cards().map_err(|e| e.to_string())?;
    let out: Vec<Value> = projects
        .iter()
        .map(|p| {
            let count = cards.iter().filter(|c| c.project_id == p.id).count();
            project_json(p, count)
        })
        .collect();
    Ok(json!({ "projects": out }))
}

fn list_cards(ctx: &Ctx, project: Option<String>) -> std::result::Result<Value, String> {
    let mut cards = match &project {
        Some(needle) => {
            let p = resolve_project(&ctx.store, needle)?;
            ctx.store
                .list_cards_for_project(p.id)
                .map_err(|e| e.to_string())?
        }
        None => ctx.store.list_cards().map_err(|e| e.to_string())?,
    };
    cards.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    let out: Vec<Value> = cards.iter().map(card_summary).collect();
    Ok(json!({ "cards": out }))
}

fn get_card(ctx: &Ctx, needle: String) -> std::result::Result<Value, String> {
    let card = resolve_card(&ctx.store, &needle)?;
    let mut v = card_summary(&card);
    let obj = v.as_object_mut().expect("card summary is an object");
    obj.insert("description".into(), json!(card.description));
    obj.insert("provider".into(), json!(card.config.provider.label()));
    obj.insert("branch".into(), json!(card.branch));
    obj.insert(
        "pr".into(),
        card.pr
            .as_ref()
            .map(|pr| json!({ "number": pr.number, "url": pr.url, "state": pr.state }))
            .unwrap_or(Value::Null),
    );
    obj.insert(
        "has_plan".into(),
        json!(ctx
            .store
            .get_plan(card.id)
            .map_err(|e| e.to_string())?
            .is_some()),
    );
    obj.insert("created_at".into(), json!(card.created_at));
    Ok(v)
}

fn get_plan(ctx: &Ctx, needle: String) -> std::result::Result<Value, String> {
    let card = resolve_card(&ctx.store, &needle)?;
    let plan = ctx.store.get_plan(card.id).map_err(|e| e.to_string())?;
    Ok(json!({ "card_id": card.id, "title": card.title, "plan": plan }))
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Mirrors `AppState::add_project`, with the checks the UI gets for free from a
/// human picking a folder in a file dialog spelled out instead.
async fn create_project(
    ctx: &Ctx,
    path: String,
    name: Option<String>,
) -> std::result::Result<Value, String> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!("`{}` is not an absolute path", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("`{}` is not an existing directory", path.display()));
    }
    if git2::Repository::open(&path).is_err() {
        return Err(format!("`{}` is not a git repository", path.display()));
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
    let existing = ctx.store.list_projects().map_err(|e| e.to_string())?;
    if let Some(dup) = existing
        .iter()
        .find(|p| p.path.canonicalize().unwrap_or_else(|_| p.path.clone()) == canonical)
    {
        return Err(format!(
            "already on the board as project `{}` ({})",
            dup.name, dup.id
        ));
    }

    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    let name = name.unwrap_or_else(|| {
        path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    });
    let config = ProjectConfig {
        base_branch: crate::infra::git::detect_base_branch(&path),
        ..Default::default()
    };
    let project = Project::new(name, path, config);
    let id = project.id;
    ctx.send(ExecutorCommand::AddProject {
        project: Box::new(project),
    })?;

    let confirmed = confirm(|| ctx.store.get_project(id).is_ok()).await;
    let mut out = json!({ "id": id, "confirmed": confirmed });
    if confirmed {
        let p = ctx.store.get_project(id).map_err(|e| e.to_string())?;
        out = project_json(&p, 0);
    }
    Ok(out)
}

/// Mirrors `AppState::create_card`: the card lands in the starting block with
/// the current global defaults. Nothing starts it.
async fn create_card(
    ctx: &Ctx,
    project: String,
    title: String,
    description: String,
) -> std::result::Result<Value, String> {
    let project = resolve_project(&ctx.store, &project)?;
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("`title` must not be blank".into());
    }
    let config = ctx
        .store
        .settings()
        .map_err(|e| e.to_string())?
        .new_card_config();
    let card = Card::new(project.id, title, description, config);
    let id = card.id;
    ctx.send(ExecutorCommand::CreateCard {
        card: Box::new(card),
    })?;

    let confirmed = confirm(|| ctx.store.get_card(id).is_ok()).await;
    let mut out = json!({ "id": id, "project_id": project.id, "confirmed": confirmed });
    if confirmed {
        let c = ctx.store.get_card(id).map_err(|e| e.to_string())?;
        out = card_summary(&c);
    }
    Ok(out)
}

/// Poll until the executor has persisted what we just sent, so a follow-up
/// `get_card` in the same agent turn can't miss it. Returns whether it landed.
async fn confirm(mut seen: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + CONFIRM_TIMEOUT;
    loop {
        if seen() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(CONFIRM_POLL).await;
    }
}

// ---------------------------------------------------------------------------
// Shaping
// ---------------------------------------------------------------------------

fn project_json(p: &Project, card_count: usize) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "path": p.path,
        "base_branch": p.config.effective_base_branch(),
        "card_count": card_count,
    })
}

/// The agent-facing view of a card. Built by hand rather than serializing
/// `Card`, both to keep the payload small and so the tool contract doesn't move
/// every time the domain model gains a field.
fn card_summary(c: &Card) -> Value {
    json!({
        "id": c.id,
        "project_id": c.project_id,
        "title": c.title,
        "status": c.status_label(),
        "column": c.state.column().title(),
        "blocked": c.blocked,
        "needs_attention": c.needs_attention(),
        "updated_at": c.updated_at,
    })
}

// ---------------------------------------------------------------------------
// Reference resolution
// ---------------------------------------------------------------------------

/// Resolve a user-supplied reference against a set of records, trying, in
/// order: a full UUID, a case-insensitive exact name, then an id prefix of at
/// least four characters. Names come before prefixes so a title that happens to
/// look like hex can't be shadowed by an unrelated id; four is the prefix floor
/// because anything shorter is too collision-prone to be a deliberate
/// reference. Ambiguity is an error listing the candidates, never a silent pick.
fn resolve<T>(
    needle: &str,
    kind: &str,
    mut items: Vec<T>,
    id: impl Fn(&T) -> Uuid,
    label: impl Fn(&T) -> &str,
) -> std::result::Result<T, String> {
    let needle = needle.trim();
    if needle.is_empty() {
        return Err(format!("empty {kind} reference"));
    }
    if let Ok(uuid) = Uuid::parse_str(needle) {
        return items
            .into_iter()
            .find(|i| id(i) == uuid)
            .ok_or_else(|| format!("no {kind} with id {uuid}"));
    }

    let named: Vec<usize> = (0..items.len())
        .filter(|&i| label(&items[i]).eq_ignore_ascii_case(needle))
        .collect();
    let matched = if !named.is_empty() {
        named
    } else if needle.len() >= 4 {
        let lower = needle.to_lowercase();
        (0..items.len())
            .filter(|&i| id(&items[i]).to_string().starts_with(&lower))
            .collect()
    } else {
        Vec::new()
    };

    match matched.len() {
        0 => Err(format!("no {kind} matching `{needle}`")),
        1 => Ok(items.remove(matched[0])),
        _ => {
            let listed: Vec<String> = matched
                .iter()
                .take(5)
                .map(|&i| format!("{} {}", short_id(id(&items[i])), label(&items[i])))
                .collect();
            Err(format!(
                "`{needle}` matches {} {kind}s — use an id: {}",
                matched.len(),
                listed.join(", ")
            ))
        }
    }
}

fn short_id(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

fn resolve_project(store: &Store, needle: &str) -> std::result::Result<Project, String> {
    let items = store.list_projects().map_err(|e| e.to_string())?;
    resolve(needle, "project", items, |p| p.id, |p| p.name.as_str())
}

fn resolve_card(store: &Store, needle: &str) -> std::result::Result<Card, String> {
    let items = store.list_cards().map_err(|e| e.to_string())?;
    resolve(needle, "card", items, |c| c.id, |c| c.title.as_str())
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn req_str(args: &Value, key: &str) -> std::result::Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("`{key}` must be a string")),
        None => Err(format!("missing required argument `{key}`")),
    }
}

fn opt_str(args: &Value, key: &str) -> std::result::Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}
