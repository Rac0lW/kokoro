//! Koharu's Tauri-managed application state, commands, and lifecycle.

mod app;
mod commands;
#[cfg(unix)]
mod mcp;

pub use app::run;
pub use commands::bindings;
