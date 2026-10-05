//! The connector abstraction. Every supported agent (Claude Code, Codex CLI,
//! OpenCode, Antigravity CLI, ...) implements `Connector`. Adding a new agent
//! must require exactly one new file under `src/connectors/` plus one
//! registration line in `src/connectors/mod.rs::all()` — nothing in this file,
//! or in any core module, should need to change.

use crate::model::{RawSession, Session};
use std::path::{Path, PathBuf};

pub type ConnectorResult<T> = Result<T, ConnectorError>;

#[derive(Debug, thiserror::Error)]
pub enum ConnectorError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse session {id} from {path}: {message}")]
    Parse {
        id: String,
        path: PathBuf,
        message: String,
    },
    #[error("session {0} not found")]
    NotFound(String),
    #[error("session {0} has no body available (metadata-only record)")]
    BodyUnavailable(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Where an injected brief was (or, under `--dry-run`, would be) written.
#[derive(Debug, Clone)]
pub struct InjectTarget {
    /// Absolute path of the file the brief was written into (or would be,
    /// under `--dry-run`).
    pub path: PathBuf,
    /// Byte range `[start, end)` within `path` occupied by agentbridge's
    /// fenced block, after injection — used by `clean` to remove exactly
    /// what was added and nothing the user wrote by hand.
    pub fenced_range: Option<(usize, usize)>,
}

/// A stream of cheap session-metadata records. Boxed so each connector can
/// return whatever iterator shape (file walk, DB cursor) is natural for it.
/// Implementations must be lazy — no full-body reads here — and must degrade
/// a single unreadable/corrupt session into an `Err` item rather than
/// aborting the whole scan.
pub type SessionStream<'a> = Box<dyn Iterator<Item = ConnectorResult<RawSession>> + 'a>;

pub trait Connector: Send + Sync {
    /// Stable identifier, e.g. `"claude-code"`, `"codex-cli"`. Used as the
    /// `provider` field on every `Session`/`Message` and as the CLI's
    /// `--provider` value — must never change once shipped.
    fn id(&self) -> &'static str;

    /// Human-readable name for CLI output.
    fn display_name(&self) -> &'static str {
        self.id()
    }

    /// Is this agent's storage present on this machine? Should be cheap
    /// (existence checks only) — no parsing.
    fn detect(&self) -> bool;

    /// Directories this connector will scan. Must honor the provider's own
    /// environment-variable override (e.g. `CLAUDE_CONFIG_DIR`, `CODEX_HOME`)
    /// before falling back to the documented default location.
    fn roots(&self) -> Vec<PathBuf>;

    /// Enumerate sessions under `roots()`. Read-only, streaming, and
    /// crash/lock-tolerant per the project's hard constraints: never blocks
    /// on another process's lock, never panics on a truncated final line or
    /// non-UTF-8 bytes, and yields a metadata-only `RawSession` with
    /// `body_available: false` rather than dropping a session whose body is
    /// missing/corrupt but whose sidecar metadata survives.
    fn scan(&self) -> ConnectorResult<SessionStream<'_>>;

    /// Fully load and normalize one session by id. May re-read the file(s)
    /// named in the corresponding `RawSession::source_path`.
    fn load(&self, id: &str) -> ConnectorResult<Session>;

    /// The exact argv needed to relaunch this session in its native agent
    /// (e.g. `["claude", "--resume", "<id>"]`), or `None` if this provider
    /// has no native resume mechanism agentbridge can drive.
    fn resume_cmd(&self, session: &Session) -> Option<Vec<String>>;

    /// The file in `project` this agent reads as startup instructions (e.g.
    /// `CLAUDE.md`, `AGENTS.md`), or `None` if agentbridge cannot inject
    /// context into it. Only names the file: the fenced, reversible write is
    /// the same for every agent and lives in `crate::inject`.
    fn instruction_file(&self, _project: &Path) -> Option<PathBuf> {
        None
    }

    /// The program that launches this agent, for `agentbridge start`.
    fn launch_program(&self) -> Option<&'static str> {
        None
    }

    /// Write `brief` into `project`'s instruction file for this agent, between
    /// begin/end fence markers so `agentbridge clean` can remove exactly what
    /// was added and nothing the user wrote by hand. With `dry_run`, computes
    /// and returns the target without writing.
    fn inject(&self, brief: &str, project: &Path, dry_run: bool) -> ConnectorResult<InjectTarget> {
        let path = self.instruction_file(project).ok_or_else(|| {
            ConnectorError::Other(anyhow::anyhow!(
                "{} has no startup instruction file agentbridge can inject into",
                self.display_name()
            ))
        })?;
        crate::inject::write_fenced(&path, brief, dry_run)
    }
}

/// The connector registry. `crate::connectors::all()` is the single place
/// that lists every known connector; this type only holds and looks them up.
pub struct Registry {
    connectors: Vec<Box<dyn Connector>>,
}

impl Registry {
    pub fn new(connectors: Vec<Box<dyn Connector>>) -> Self {
        Self { connectors }
    }

    pub fn all(&self) -> &[Box<dyn Connector>] {
        &self.connectors
    }

    pub fn detected(&self) -> impl Iterator<Item = &dyn Connector> {
        self.connectors
            .iter()
            .map(|c| c.as_ref())
            .filter(|c| c.detect())
    }

    pub fn by_id(&self, id: &str) -> Option<&dyn Connector> {
        self.connectors
            .iter()
            .map(|c| c.as_ref())
            .find(|c| c.id() == id)
    }
}
