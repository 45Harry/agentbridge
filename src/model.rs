//! Provider-agnostic normalized data model.
//!
//! Every connector translates its provider's native format into these types.
//! Nothing outside `src/connectors/` should need to know a provider-specific
//! shape once data has passed through `Connector::load`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A project is identified by its canonical (symlink-resolved, trailing-slash
/// stripped) absolute path. `git_root`/`git_remote` are used to unify sessions
/// recorded from different worktrees or renamed clones of the same repo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    /// Stable id: derived from `git_remote` when present, else the canonical path.
    /// Never derived from any provider's encoded-directory-name scheme.
    pub id: String,
    pub canonical_path: PathBuf,
    pub git_remote: Option<String>,
    pub git_root: Option<PathBuf>,
    /// Other paths (worktrees, symlinks, case variants, renamed dirs) known to
    /// resolve to this same project.
    pub aliases: Vec<PathBuf>,
}

/// Cheap, list-view metadata about a session — what `Connector::scan()` yields.
/// Must be derivable without reading a session's full body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawSession {
    pub id: String,
    pub provider: String,
    /// The project path exactly as read from *inside* the transcript/record
    /// (never reconstructed from an encoded directory name — see hard
    /// constraint in the project spec / CLAUDE-facing prompt).
    pub project_path: Option<PathBuf>,
    pub started_at: Option<DateTime<Utc>>,
    /// True last-event timestamp read from *inside* the file, not file mtime.
    pub last_event_at: Option<DateTime<Utc>>,
    pub title: Option<String>,
    pub source_path: PathBuf,
    /// False when the file is missing/corrupt but sidecar metadata let us
    /// still produce this record. `Connector::load` will error if called on
    /// an id with `body_available: false`.
    pub body_available: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TokenTotals {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub total: Option<u64>,
}

/// A fully loaded, normalized session — `Connector::load()`'s return type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    pub provider: String,
    pub project_id: String,
    pub started_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub title: Option<String>,
    pub token_totals: TokenTotals,
    pub source_path: PathBuf,
    /// The raw provider payload retained verbatim, for round-tripping and for
    /// features that need provider-specific fields the normalized model
    /// doesn't capture. Must already be redacted (see `crate::redact`) before
    /// this value is persisted or sent anywhere.
    pub raw_payload: serde_json::Value,
    pub body_available: bool,
    pub messages: Vec<Message>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub session_id: String,
    /// 0-based position within the session, stable and used as the
    /// provenance unit for `Fact`.
    pub ordinal: u64,
    pub role: Role,
    pub timestamp: Option<DateTime<Utc>>,
    pub text: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_result: Option<serde_json::Value>,
    /// The ordinal of the message this one replies to/continues from, where
    /// the provider exposes an explicit parent link (e.g. Claude Code's
    /// `parentUuid`). `None` when the provider is strictly linear or the
    /// link is unknown.
    pub parent_ordinal: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    FileTouched,
    CommandRun,
    GitState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    pub session_id: String,
    pub kind: ArtifactKind,
    /// File path for `FileTouched`, the literal command line for
    /// `CommandRun`, the branch name for `GitState`.
    pub path_or_command: String,
    /// SHA for `GitState`, exit code for `CommandRun`, etc.
    pub detail: Option<String>,
}

/// One provenance pointer: which message a distilled fact came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    pub session_id: String,
    pub message_ordinal: u64,
}

/// A distilled, durable claim about a project, produced by `agentbridge brief`.
/// Every `Fact` must carry at least one `Provenance` entry — an unattributable
/// fact is a bug (see project spec, Milestone 3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fact {
    pub id: String,
    pub project_id: String,
    pub text: String,
    pub tags: Vec<String>,
    pub provenance: Vec<Provenance>,
    pub created_at: DateTime<Utc>,
}
