//! Kokoro's MCP endpoint.
//!
//! The tools live here, next to the state they touch, and are served over a
//! loopback Unix socket as newline-delimited JSON-RPC. `kokoro-mcp` carries the
//! same messages between an MCP client's stdio and this socket, so the tool
//! surface has exactly one definition.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter as _, Manager as _};
use tauri_runtime_cef::CefRuntime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::commands::{
    canvas::CanvasChannel,
    editing::{self, TranslationUpdate},
    lifecycle,
    output::export_to_path,
    processing::{self, JobChannel, Processing},
    project::{CurrentProject, Project, ProjectLibrary},
};

/// Latest protocol revision this endpoint answers.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Tells the renderer to drop its cached page data after an external edit.
const CHANGED_EVENT: &str = "kokoro://project-changed";

pub(crate) fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KOKORO_MCP_SOCKET") {
        return Ok(path.into());
    }
    let home = dirs::home_dir().context("the home directory is unavailable")?;
    Ok(home.join(".koharu").join("mcp.sock"))
}

pub(crate) fn serve(handle: AppHandle<CefRuntime>) {
    tokio::spawn(async move {
        if let Err(error) = run(handle).await {
            tracing::warn!(%error, "Kokoro's MCP endpoint stopped");
        }
    });
}

async fn run(handle: AppHandle<CefRuntime>) -> Result<()> {
    let path = socket_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // A stale socket from a previous run would make `bind` fail.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to bind {}", path.display()))?;
    tracing::info!(path = %path.display(), "Kokoro MCP endpoint ready");

    loop {
        let (stream, _) = listener.accept().await?;
        let handle = handle.clone();
        tokio::spawn(async move {
            if let Err(error) = connection(&handle, stream).await {
                tracing::debug!(%error, "MCP connection closed");
            }
        });
    }
}

async fn connection(handle: &AppHandle<CefRuntime>, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(request) => request,
            Err(error) => {
                send(&mut write, error_response(Value::Null, -32700, &error.to_string())).await?;
                continue;
            }
        };
        // A message without an id is a notification and takes no reply.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let response = match dispatch(handle, &method, params).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => error_response(id, -32603, &format!("{error:#}")),
        };
        send(&mut write, response).await?;
    }
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, message: Value) -> Result<()> {
    let mut line = serde_json::to_string(&message)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;
    Ok(())
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn dispatch(handle: &AppHandle<CefRuntime>, method: &str, params: Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "kokoro", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .context("tools/call needs a tool name")?
                .to_owned();
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            let (text, is_error) = match call(handle, &name, arguments).await {
                Ok(text) => (text, false),
                Err(error) => (format!("{error:#}"), true),
            };
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": is_error,
            }))
        }
        other => anyhow::bail!("unknown method {other}"),
    }
}

/// Command errors are `Display` but not `std::error::Error`, so unwrap them here.
fn command<T>(result: std::result::Result<T, crate::commands::Error>) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("{error}"))
}

async fn call(handle: &AppHandle<CefRuntime>, name: &str, arguments: Value) -> Result<String> {
    let project = || handle.state::<CurrentProject>();
    let text = match name {
        "list_projects" => {
            let library = handle.state::<ProjectLibrary>();
            serde_json::to_string_pretty(&library.list()?)?
        }
        "open_project" => {
            let name = string_argument(&arguments, "name")?;
            command(lifecycle::open_project(name, handle.clone()).await)?;
            "opened".to_owned()
        }
        "close_project" => {
            command(lifecycle::close_project(handle.clone()).await)?;
            "closed".to_owned()
        }
        "get_pages" => serde_json::to_string_pretty(&command(lifecycle::get_pages(project()).await)?)?,
        "get_page" => {
            let current = project();
            let guard = current.project.lock().await;
            let opened = guard.as_ref().context("no project is open")?;
            let snapshot = opened.snapshot();
            let page = match arguments.get("page").and_then(Value::as_str) {
                Some(id) => serde_json::from_value::<koharu_scene::EntityId>(Value::String(
                    id.to_owned(),
                ))?,
                None => opened
                    .active_page()
                    .context("no page is selected; pass a page id")?,
            };
            serde_json::to_string_pretty(&Project::page(&snapshot, page)?)?
        }
        "set_translation" => {
            let layer = entity_argument(&arguments, "layer")?;
            let text = arguments.get("text").and_then(Value::as_str).map(str::to_owned);
            command(
                editing::set_translation(
                    layer,
                    text,
                    handle.state(),
                    project(),
                    handle.state::<CanvasChannel>(),
                )
                .await,
            )?;
            "updated".to_owned()
        }
        "set_source_text" => {
            let layer = entity_argument(&arguments, "layer")?;
            let text = string_argument(&arguments, "text")?;
            command(
                editing::set_source_text(
                    layer,
                    text,
                    handle.state(),
                    project(),
                    handle.state::<CanvasChannel>(),
                )
                .await,
            )?;
            "updated".to_owned()
        }
        "set_translations" => {
            let updates: Vec<TranslationUpdate> = serde_json::from_value(
                arguments
                    .get("updates")
                    .cloned()
                    .context("set_translations needs an updates array")?,
            )?;
            let count = updates.len();
            command(
                editing::set_translations(
                    updates,
                    handle.state(),
                    project(),
                    handle.state::<CanvasChannel>(),
                )
                .await,
            )?;
            format!("updated {count} layers")
        }
        "get_glossary" => {
            let glossary = koharu_translator::Glossary::load()?;
            serde_json::to_string_pretty(&*glossary.read()?)?
        }
        "set_glossary" => {
            let entries: Vec<koharu_translator::Term> = serde_json::from_value(
                arguments
                    .get("entries")
                    .cloned()
                    .context("set_glossary needs an entries array")?,
            )?;
            let glossary = koharu_translator::Glossary::load()?;
            let saved = koharu_translator::Glossary { entries };
            {
                let mut current = glossary.write()?;
                *current = saved.clone();
                current.save()?;
            }
            serde_json::to_string_pretty(&saved)?
        }
        "apply_glossary" => {
            let changed = command(
                editing::apply_glossary(
                    handle.state(),
                    project(),
                    handle.state::<CanvasChannel>(),
                )
                .await,
            )?;
            format!("rewrote {changed} layers")
        }
        "process" => {
            let scope = serde_json::from_value(arguments.get("scope").cloned().unwrap_or(json!({
                "scope": "project"
            })))?;
            let operation = serde_json::from_value(
                arguments
                    .get("operation")
                    .cloned()
                    .unwrap_or(json!({ "operation": "full" })),
            )?;
            let job = command(
                processing::process(
                    handle.clone(),
                    scope,
                    operation,
                    project(),
                    handle.state::<Processing>(),
                    handle.state::<JobChannel>(),
                )
                .await,
            )?;
            serde_json::to_string(&job)?
        }
        "export" => {
            let destination = PathBuf::from(string_argument(&arguments, "destination")?);
            let format = serde_json::from_value(
                arguments
                    .get("format")
                    .cloned()
                    .unwrap_or(json!("png")),
            )?;
            command(
                export_to_path(
                    destination.clone(),
                    format,
                    project(),
                    handle.state(),
                )
                .await,
            )?;
            format!("exported to {}", destination.display())
        }
        "undo" => {
            command(
                editing::undo(
                    handle.state(),
                    project(),
                    handle.state::<CanvasChannel>(),
                )
                .await,
            )?;
            "undone".to_owned()
        }
        "redo" => {
            command(
                editing::redo(
                    handle.state(),
                    project(),
                    handle.state::<CanvasChannel>(),
                )
                .await,
            )?;
            "redone".to_owned()
        }
        other => anyhow::bail!("unknown tool {other}"),
    };
    if changes_project(name) {
        let _ = handle.emit(CHANGED_EVENT, ());
    }
    Ok(text)
}

/// Tools that leave the renderer's cached project data stale.
fn changes_project(tool: &str) -> bool {
    !matches!(tool, "list_projects" | "get_pages" | "get_page")
}

fn string_argument(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {key}"))
        .map(str::to_owned)
}

fn entity_argument(arguments: &Value, key: &str) -> Result<koharu_scene::EntityId> {
    let value = string_argument(arguments, key)?;
    Ok(serde_json::from_value(Value::String(value))?)
}

fn tools() -> Value {
    json!([
        {
            "name": "list_projects",
            "description": "List the projects in Kokoro's local library.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "open_project",
            "description": "Open one of the projects listed by list_projects.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "close_project",
            "description": "Close the open project.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_pages",
            "description": "List the pages of the open project with their sizes and layer counts.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_page",
            "description": "Read one page, including every layer's source text and translation. Defaults to the page selected in the editor.",
            "inputSchema": {
                "type": "object",
                "properties": { "page": { "type": "string", "description": "Page id from get_pages" } }
            }
        },
        {
            "name": "set_translation",
            "description": "Replace a text layer's translation. Pass null to clear it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "layer": { "type": "string", "description": "Text layer id from get_page" },
                    "text": { "type": ["string", "null"] }
                },
                "required": ["layer"]
            }
        },
        {
            "name": "set_source_text",
            "description": "Replace a text layer's source text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "layer": { "type": "string", "description": "Text layer id from get_page" },
                    "text": { "type": "string" }
                },
                "required": ["layer", "text"]
            }
        },
        {
            "name": "set_translations",
            "description": "Replace many translations in one undoable commit. Use this instead of calling set_translation in a loop when unifying terminology or rewriting a whole page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "updates": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "layer": { "type": "string", "description": "Text layer id from get_page" },
                                "text": { "type": ["string", "null"] }
                            },
                            "required": ["layer"]
                        }
                    }
                },
                "required": ["updates"]
            }
        },
        {
            "name": "get_glossary",
            "description": "Read the project term list that every translation request is told to honour.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "set_glossary",
            "description": "Replace the term list. Each entry is a source term and the translation it must always receive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "source": { "type": "string" },
                                "target": { "type": "string" }
                            },
                            "required": ["source", "target"]
                        }
                    }
                },
                "required": ["entries"]
            }
        },
        {
            "name": "apply_glossary",
            "description": "Rewrite existing translations so they match the term list, in one undoable commit.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "process",
            "description": "Run the pipeline. Defaults to a full run over the whole project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {
                        "description": "project, or pages with an id list",
                        "type": "object"
                    },
                    "operation": {
                        "description": "full, through a stage, only a stage, or a list of stages",
                        "type": "object"
                    }
                }
            }
        },
        {
            "name": "export",
            "description": "Export the open project to a directory (png, psd) or a .cbz file. Use an absolute path.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "destination": {
                        "type": "string",
                        "description": "Absolute directory for png/psd, or the .cbz file path"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["png", "psd", "cbz"],
                        "description": "Defaults to png"
                    }
                },
                "required": ["destination"]
            }
        },
        {
            "name": "undo",
            "description": "Undo the project's last edit.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "redo",
            "description": "Redo the project's last undone edit.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}
