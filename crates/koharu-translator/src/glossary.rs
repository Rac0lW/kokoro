//! Project term list applied to every translation request.
//!
//! Entries are consulted twice: the translation prompt is told to render a
//! source term exactly as written here, and the `apply_glossary` operation
//! rewrites existing translations that predate an entry.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Term list stored in the shared configuration file under `[glossary]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, specta::Type)]
#[serde(default)]
pub struct Glossary {
    pub entries: Vec<Term>,
}

impl Glossary {
    pub fn load() -> Result<koharu_config::Config<Self>> {
        koharu_config::load("glossary")
    }
}

/// One source term and the translation it must always receive.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, specta::Type)]
pub struct Term {
    pub source: String,
    pub target: String,
}
