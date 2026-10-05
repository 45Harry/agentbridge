//! Materialization: make every indexed session visible in one directory, for
//! every detected tool, without duplicating bodies.
//!
//! Implements `DESIGN.md` §4:
//!   Rule 2 — one derived artifact per (session, target format, directory),
//!            kept in `~/.agentbridge/cache`.
//!   Rule 3 — directory presence via hardlink to that single artifact.
//!
//! Every file created is recorded in a manifest with its inode, so `unsync`
//! removes exactly what agentbridge added and nothing a tool has since
//! replaced.

use crate::connector::Registry;
use crate::convert::{ClaudeCodeConverter, CodexCliConverter, SessionConverter};
use crate::index::{discover, Index};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// One file agentbridge created, and enough identity to remove it safely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRecord {
    pub dest: PathBuf,
    pub cache: PathBuf,
    pub session_id: String,
    /// Provider the session came from.
    pub source_provider: String,
    /// Provider whose store it was materialized into.
    pub target_provider: String,
    pub project: PathBuf,
    /// Inode at creation time. Removal requires a match, so a file a tool has
    /// since rewritten is never deleted by us.
    pub inode: u64,
    /// How many messages agentbridge wrote into this file. Anything beyond
    /// this on a later read was appended by the tool itself and is new work
    /// to pull back. Defaults to 0 for manifests written before write-back
    /// existed.
    #[serde(default)]
    pub message_count: usize,
    /// Title agentbridge last wrote into this copy (`None` when the source
    /// session had none). Lets `pull_back` tell "the tool renamed it" apart
    /// from "we never had a title to begin with" — the title equivalent of
    /// `message_count`. Defaults to `None` for manifests written before
    /// title tracking existed, which costs at most one spurious "renamed"
    /// report on the first pull after upgrading.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Default)]
pub struct PullReport {
    /// (session id, number of new messages recovered)
    pub pulled: Vec<(String, usize)>,
    /// (session id, new title) recovered from a rename made in a non-native
    /// copy. Independent of `pulled` — a rename with no new turns still ends
    /// up here.
    pub renamed: Vec<(String, String)>,
    /// (session id, providers with new work, choice made) for every session
    /// where more than one tool contributed new turns/renames since the last
    /// pull. A session with new work from exactly one tool is folded in
    /// directly and never appears here — it was never a conflict.
    pub conflicts: Vec<(String, Vec<String>, ConflictChoice)>,
    pub errors: Vec<String>,
}

/// What to do with a session where more than one tool has new write-back
/// work since the last pull. `pull_back` cannot pick a winner on its own —
/// dropping a tool's turns silently would violate the "never lose recovered
/// work" invariant `unsync` already relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictChoice {
    /// Keep every tool's new work — today's only behavior, unchanged.
    MergeAll,
    /// Keep only the named provider's new work; the other tool(s)'
    /// contribution for this pull is discarded (marked seen, never written
    /// to the overlay).
    KeepOnly(String),
    /// Decide nothing this round — leave the manifest untouched so the same
    /// new work is offered again on the next pull.
    Skip,
}

/// One tool's contribution to a conflicting session, handed to a
/// `ConflictResolver` so it can show the operator what each side actually
/// contains — not just the tool's name.
#[derive(Debug, Clone)]
pub struct ConflictItem {
    pub provider: String,
    pub new_messages: Vec<crate::model::Message>,
    pub new_title: Option<String>,
}

/// Asked once per conflicting session during `pull_back_with`. The library
/// stays free of any UI dependency; `main.rs` supplies a ratatui-backed
/// full-screen resolver for interactive terminals, and callers that must
/// never block (dry-run, `auto watch`) use `AutoMerge`.
pub trait ConflictResolver {
    /// `items` lists every target provider with new work for this session,
    /// in manifest order, each carrying the actual turns/rename it
    /// contributed since the last pull.
    fn resolve(&mut self, session_id: &str, items: &[ConflictItem]) -> ConflictChoice;
}

/// Default resolver: merge every tool's new work, exactly as `pull_back`
/// behaved before this feature existed. Used for dry-run, `auto watch`, and
/// any non-interactive invocation.
pub struct AutoMerge;

impl ConflictResolver for AutoMerge {
    fn resolve(&mut self, _session_id: &str, _items: &[ConflictItem]) -> ConflictChoice {
        ConflictChoice::MergeAll
    }
}

fn overlay_dir() -> PathBuf {
    data_dir().join("overlay")
}

fn overlay_path(session_id: &str) -> PathBuf {
    overlay_dir().join(format!("{}.jsonl", session_id))
}

/// Turns that exist only because a tool appended them to a materialized copy.
///
/// The source file belongs to another tool and is never modified (invariant
/// 2), so this overlay is the durable home for that new work — it is not a
/// duplicate of anything, it is the only copy that survives `unsync`.
pub fn overlay_messages(session_id: &str) -> Vec<crate::model::Message> {
    let Ok(content) = fs::read_to_string(overlay_path(session_id)) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Identity of a message for dedup: a turn is the same turn if its role, time
/// and text match. Ordinals are not usable — they are reassigned per file.
fn message_key(m: &crate::model::Message) -> String {
    format!(
        "{:?}|{}|{}|{}",
        m.role,
        m.timestamp.map(|t| t.timestamp_millis()).unwrap_or(0),
        m.text.as_deref().unwrap_or(""),
        m.tool_name.as_deref().unwrap_or(""),
    )
}

fn overlay_title_path(session_id: &str) -> PathBuf {
    overlay_dir().join(format!("{}.title", session_id))
}

/// A rename recovered from a non-native copy (write-back). Mirrors
/// `overlay_messages` but for the one-value title case: `pull_back` writes it
/// here when a materialized copy's title no longer matches what agentbridge
/// last wrote, and `fold_overlay` applies it on top of the native title
/// before the next sync re-materializes every copy.
pub fn overlay_title(session_id: &str) -> Option<String> {
    fs::read_to_string(overlay_title_path(session_id))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn set_overlay_title(session_id: &str, title: &str) -> std::io::Result<()> {
    fs::create_dir_all(overlay_dir())?;
    fs::write(overlay_title_path(session_id), title)
}

fn append_overlay(session_id: &str, messages: &[crate::model::Message]) -> std::io::Result<usize> {
    if messages.is_empty() {
        return Ok(0);
    }
    fs::create_dir_all(overlay_dir())?;

    // Never append a turn already recorded — pulls can overlap when the same
    // session is materialized into several tools.
    let existing: std::collections::HashSet<String> =
        overlay_messages(session_id).iter().map(message_key).collect();

    let mut body = fs::read_to_string(overlay_path(session_id)).unwrap_or_default();
    let mut added = 0;
    for m in messages {
        if existing.contains(&message_key(m)) {
            continue;
        }
        body.push_str(&serde_json::to_string(m).unwrap_or_default());
        body.push('\n');
        added += 1;
    }
    if added > 0 {
        fs::write(overlay_path(session_id), body)?;
    }
    Ok(added)
}

fn load_materialized(target: &str, path: &Path, id: &str) -> Option<crate::model::Session> {
    match target {
        "claude-code" => crate::connectors::claude_code::load_file(path, id).ok(),
        "codex-cli" => {
            let mut s = crate::connectors::codex_cli::load_file(path, id).ok()?;
            // A rollout carries no title; a name given inside Codex lives in
            // Codex's name list, keyed by the id the copy has there.
            if let Some(home) = crate::connectors::codex_cli::config_home() {
                let thread = crate::convert::ClaudeCodeConverter::session_uuid(id);
                s.title = crate::codex_write::renamed_in_codex(&home, &thread);
            }
            Some(s)
        }
        "opencode" => crate::connectors::opencode::load_from_db(path, id).ok(),
        // Antigravity's `dest` is the conversation body agentbridge wrote; the
        // title lives in the separate summaries index, so it is layered on
        // here or a rename made inside agy would never be seen.
        "antigravity" => {
            let mut s = crate::antigravity_write::load_written(path, id).ok()?;
            s.title = crate::antigravity_write::written_title(path, id);
            Some(s)
        }
        _ => None,
    }
}

/// What agentbridge believes about each materialized file vs what is actually
/// on disk now. Exposes drift — the basis of write-back and the first thing to
/// look at when a pull recovers nothing.
pub struct StatusRow {
    pub session_id: String,
    pub target_provider: String,
    pub dest: PathBuf,
    pub exists: bool,
    /// Messages agentbridge wrote.
    pub expected: usize,
    /// Messages the reader finds now; `None` when the file cannot be read.
    pub actual: Option<usize>,
}

impl StatusRow {
    /// New turns waiting to be pulled back.
    pub fn drift(&self) -> i64 {
        self.actual.unwrap_or(self.expected) as i64 - self.expected as i64
    }
}

/// The id a materialized copy is addressed by in its target tool.
///
/// Most targets keep the source's own id, but the tools agentbridge writes
/// rows/databases into (OpenCode, Antigravity) require a native-shaped id, so
/// the derived one is stashed in `LinkRecord::cache` at write time and has to
/// be used to read the copy back.
fn materialized_id(r: &LinkRecord) -> String {
    match r.target_provider.as_str() {
        "opencode" | "antigravity" => r.cache.to_string_lossy().to_string(),
        _ => r.session_id.clone(),
    }
}

pub fn status() -> Vec<StatusRow> {
    read_manifest()
        .into_iter()
        .map(|r| {
            let exists = r.dest.exists();
            let actual = if exists {
                let id = materialized_id(&r);
                load_materialized(&r.target_provider, &r.dest, &id)
                    .map(|s| s.messages.len())
            } else {
                None
            };
            StatusRow {
                session_id: r.session_id,
                target_provider: r.target_provider,
                dest: r.dest,
                exists,
                expected: r.message_count,
                actual,
            }
        })
        .collect()
}

/// Write-back: recover turns a tool appended to a materialized session so
/// every other tool can see them (DESIGN.md §6). Uses `AutoMerge`: every
/// tool's new work is kept, matching every version of this function before
/// conflict resolution existed. Use `pull_back_with` to ask the operator
/// instead when two or more tools both have new work for the same session.
///
/// Reads only files agentbridge itself created; source sessions are untouched.
pub fn pull_back(dry_run: bool) -> PullReport {
    pull_back_with(dry_run, &mut AutoMerge)
}

/// New work recovered from one materialized copy, not yet applied.
struct Pending {
    manifest_idx: usize,
    new_messages: Vec<crate::model::Message>,
    /// Turns the copy gained since the last pull, including bookkeeping turns
    /// that were dropped from `new_messages`.
    seen: usize,
    new_title: Option<String>,
}

/// Same as `pull_back`, but lets the caller decide what happens when more
/// than one tool has new work for the same session since the last pull —
/// `resolver` is asked once per such session. A session with new work from
/// exactly one tool is never a conflict and is folded in directly, same as
/// always.
pub fn pull_back_with(dry_run: bool, resolver: &mut dyn ConflictResolver) -> PullReport {
    let mut report = PullReport::default();
    let _lock = if dry_run {
        None
    } else {
        match acquire_run_lock() {
            Some(lock) => Some(lock),
            None => {
                report.errors.push(BUSY.to_string());
                return report;
            }
        }
    };
    // Repeated rows for one copy would each report the same new turn, and
    // read as a conflict of a tool against itself.
    let rows_on_disk = read_manifest_raw().len();
    let mut manifest = read_manifest();
    let mut changed = manifest.len() != rows_on_disk;

    // Pass 1: read every materialized copy once and record what is new since
    // the last pull. Two or more entries for the same session id here means
    // two tools independently contributed new work — a conflict this
    // function cannot resolve by itself.
    let mut pending_by_session: std::collections::BTreeMap<String, Vec<Pending>> =
        std::collections::BTreeMap::new();

    for (idx, rec) in manifest.iter().enumerate() {
        if !rec.dest.exists() {
            continue;
        }
        // OpenCode rows and Antigravity conversations are addressed by the
        // native-shaped id agentbridge created; for file targets the source
        // session id names the materialized file.
        let id = materialized_id(rec);
        let Some(session) = load_materialized(&rec.target_provider, &rec.dest, &id) else {
            continue;
        };

        let new_title = session
            .title
            .as_ref()
            .filter(|t| rec.title.as_deref() != Some(t.as_str()))
            .cloned();
        let new_messages = if session.messages.len() > rec.message_count {
            session.messages[rec.message_count..].to_vec()
        } else {
            Vec::new()
        };
        // How far the copy has grown, bookkeeping included. The manifest count
        // moves by this, so turns left behind below are not offered again.
        let seen = new_messages.len();
        // A tool resuming a session writes its own bookkeeping as turns
        // (`<environment_context>…`, `<skills_instructions>…`). Those are not
        // things anyone said and must not travel to the other tools.
        let new_messages: Vec<crate::model::Message> = new_messages
            .into_iter()
            .filter(|m| !m.text.as_deref().is_some_and(crate::label::is_bookkeeping))
            // Nor does a turn that says nothing: no text and no tool use
            // (Claude Code writes one ahead of its answer).
            .filter(|m| {
                m.text.as_deref().is_some_and(|t| !t.trim().is_empty())
                    || m.tool_name.is_some()
                    || m.tool_input.is_some()
                    || m.tool_result.is_some()
            })
            .collect();

        if new_title.is_none() && seen == 0 {
            continue;
        }

        pending_by_session.entry(rec.session_id.clone()).or_default().push(Pending {
            manifest_idx: idx,
            new_messages,
            seen,
            new_title,
        });
    }

    // Pass 2: apply. Ask the resolver only when a session actually has more
    // than one contributing tool; a single contributor is applied exactly as
    // `pull_back` always has, with no prompt.
    for (session_id, pendings) in pending_by_session {
        let providers: Vec<String> =
            pendings.iter().map(|p| manifest[p.manifest_idx].target_provider.clone()).collect();

        let choice = if pendings.len() > 1 {
            let items: Vec<ConflictItem> = pendings
                .iter()
                .map(|p| ConflictItem {
                    provider: manifest[p.manifest_idx].target_provider.clone(),
                    new_messages: p.new_messages.clone(),
                    new_title: p.new_title.clone(),
                })
                .collect();
            let choice = resolver.resolve(&session_id, &items);
            report.conflicts.push((session_id.clone(), providers.clone(), choice.clone()));
            choice
        } else {
            ConflictChoice::MergeAll
        };

        if choice == ConflictChoice::Skip {
            // Leave every contributing manifest record untouched so the same
            // new work is offered again on the next pull.
            continue;
        }

        for pending in &pendings {
            let keep = match &choice {
                ConflictChoice::MergeAll => true,
                ConflictChoice::KeepOnly(p) => manifest[pending.manifest_idx].target_provider == *p,
                ConflictChoice::Skip => unreachable!("handled above"),
            };

            if let Some(new_title) = &pending.new_title {
                if keep {
                    if dry_run {
                        report.renamed.push((session_id.clone(), new_title.clone()));
                    } else if let Err(e) = set_overlay_title(&session_id, new_title) {
                        report.errors.push(format!("overlay title {}: {}", session_id, e));
                    } else {
                        report.renamed.push((session_id.clone(), new_title.clone()));
                    }
                }
                if !dry_run {
                    manifest[pending.manifest_idx].title = Some(new_title.clone());
                    changed = true;
                }
            }

            if pending.seen == 0 {
                continue;
            }
            let new_count = manifest[pending.manifest_idx].message_count + pending.seen;

            if keep {
                if pending.new_messages.is_empty() {
                    // Only bookkeeping arrived: nothing to carry, but the
                    // count below still moves past it.
                } else if dry_run {
                    report.pulled.push((session_id.clone(), pending.new_messages.len()));
                } else {
                    match append_overlay(&session_id, &pending.new_messages) {
                        Ok(0) => {}
                        Ok(n) => report.pulled.push((session_id.clone(), n)),
                        Err(e) => {
                            report.errors.push(format!("overlay {}: {}", session_id, e));
                            continue;
                        }
                    }
                }
            }
            if !dry_run {
                manifest[pending.manifest_idx].message_count = new_count;
                changed = true;
            }
        }
    }

    if changed && !dry_run {
        let body: String = manifest
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default() + "\n")
            .collect();
        let _ = fs::write(manifest_path(), body);
    }

    report
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub created: Vec<LinkRecord>,
    /// Already present and identical — no work done (idempotency).
    pub unchanged: usize,
    /// Skipped because the session is already native to that tool *in this
    /// directory*; agentbridge must not touch a tool's own sessions.
    pub skipped_native: usize,
    /// Skipped because the session is itself a copy agentbridge made (it says
    /// so in its own title or first record), whatever the manifest holds.
    pub skipped_copies: usize,
    /// Native origin files updated in place because the session opted into
    /// merge-back (`agentbridge resume --merge`); turns pulled from other
    /// tools' copies were appended to the tool's own session file.
    pub merged_native: usize,
    /// Codex `threads` rows inserted so the materialized rollouts show up in
    /// `codex /resume` (CONNECTORS.md §2).
    pub codex_indexed: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Default)]
pub struct UnsyncReport {
    pub removed: Vec<PathBuf>,
    /// Left alone because the inode no longer matches what we created.
    pub kept_foreign: Vec<PathBuf>,
    pub missing: usize,
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Held by the one run allowed to rewrite the manifest. Released on drop.
pub struct RunLock(PathBuf);

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A lock older than this belongs to a run that died.
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// What a run reports when it stands down for another.
const BUSY: &str = "another agentbridge sync is running; skipped (that run covers it)";

/// One writer at a time. The shell hook starts a `sync` per new shell and a
/// tool's own startup shells count, so several runs begin together; each read
/// the manifest before the others wrote it and took their copies for
/// sessions. `None` means another run holds the lock.
fn acquire_run_lock() -> Option<RunLock> {
    let dir = data_dir();
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join("sync.lock");
    for _ in 0..2 {
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", std::process::id());
                return Some(RunLock(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                if !stale {
                    return None;
                }
                let _ = fs::remove_file(&path);
            }
            Err(_) => return None,
        }
    }
    None
}

/// agentbridge's own data dir. Overridable for tests and for operators who
/// keep state elsewhere.
pub fn data_dir() -> PathBuf {
    std::env::var("AGENTBRIDGE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".agentbridge"))
}

/// Where a converted session is written before it is moved into the target
/// tool's own store. Nothing stays here: agentbridge keeps no copy of any
/// session. A tool cannot read another tool's format, so one converted file
/// per session has to exist, and it lives in that tool's store and nowhere
/// else. (An earlier design kept every artifact in `~/.agentbridge/cache` and
/// hardlinked it out; leftovers there grew to 28 GB on a real machine.)
fn staging_root(target: &str) -> PathBuf {
    staging_dir().join(target)
}

/// This run's staging folder. Per process, so two runs never share files.
fn staging_dir() -> PathBuf {
    data_dir().join("staging").join(std::process::id().to_string())
}

/// True when both files hold the same bytes.
fn same_content(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) if x.len() == y.len() => fs::read(a).ok() == fs::read(b).ok(),
        _ => false,
    }
}

fn manifest_path() -> PathBuf {
    data_dir().join("manifest.jsonl")
}

/// Per-session opt-in to merge-back: when a session carries a marker, turns
/// pulled from *other* tools' copies are also appended to the session's own
/// native file during sync. Without a marker, invariant 2 applies and the
/// origin file is never touched. The marker is set interactively by
/// `agentbridge resume` when the user chooses merge across tools.
fn merge_marker_path(session_id: &str) -> PathBuf {
    data_dir().join("merge").join(session_id)
}

pub fn set_merge(session_id: &str) -> std::io::Result<()> {
    let p = merge_marker_path(session_id);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&p, b"merge\n")
}

pub fn is_merge(session_id: &str) -> bool {
    merge_marker_path(session_id).exists()
}

pub fn clear_merge(session_id: &str) {
    let _ = fs::remove_file(merge_marker_path(session_id));
}

/// Where a target tool keeps its sessions. Must track the same
/// `CLAUDE_CONFIG_DIR`/`CODEX_HOME` overrides the read connectors use
/// (`connectors::claude_code::config_root`, `connectors::codex_cli::config_home`)
/// — otherwise materialized copies land in the default `~/.claude` or
/// `~/.codex` even when the real tool has been redirected elsewhere, and the
/// tool never sees them.
fn live_root(target: &str) -> Option<PathBuf> {
    match target {
        "claude-code" => crate::connectors::claude_code::write_root(),
        "codex-cli" => crate::connectors::codex_cli::config_home(),
        // OpenCode stores rows in SQLite, not files — it cannot be hardlinked
        // into and is handled separately (DESIGN.md §5). Antigravity is the
        // same: one SQLite body per conversation plus an index row, so it is
        // written by `antigravity_write`, not linked.
        _ => None,
    }
}

/// Public accessor for `resume`'s claude-code target: the same redirected
/// root sync materializes into (honors `CLAUDE_CONFIG_DIR`).
pub fn claude_live_root() -> Option<PathBuf> {
    live_root("claude-code")
}

/// OpenCode's database, when present.
fn opencode_db() -> Option<PathBuf> {    let p = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("opencode").join("opencode.db")
    } else {
        home().join(".local").join("share").join("opencode").join("opencode.db")
    };
    p.exists().then_some(p)
}

fn converter_for(target: &str) -> Option<Box<dyn SessionConverter>> {
    match target {
        "claude-code" => Some(Box::new(ClaudeCodeConverter::new())),
        "codex-cli" => Some(Box::new(CodexCliConverter::new())),
        _ => None,
    }
}

/// Link `src` to `dest`, falling back to a copy when a hardlink is impossible
/// (different filesystem). Returns the inode actually created.
fn link_or_copy(src: &Path, dest: &Path) -> std::io::Result<u64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    // Already the same inode: the hardlink is in place, nothing to do. Falling
    // through to copy here would truncate the file we are copying *from*.
    if let (Ok(a), Ok(b)) = (fs::metadata(src), fs::metadata(dest))
        && a.ino() == b.ino() {
            return Ok(a.ino());
        }

    match fs::hard_link(src, dest) {
        Ok(()) => {}
        Err(_) if dest.exists() => {
            // `hard_link` fails when the destination exists. Replace it via a
            // temp file + rename so the swap is atomic: a bare `fs::copy` onto
            // an existing path truncates it first, and if src and dest are the
            // same inode that destroys the content before it is read.
            let tmp = dest.with_extension("agentbridge-tmp");
            fs::copy(src, &tmp)?;
            fs::rename(&tmp, dest)?;
        }
        Err(e) => {
            // Cross-device or unsupported: a real copy, at the cost of bytes.
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(e);
            }
            fs::copy(src, dest)?;
        }
    }
    Ok(fs::metadata(dest)?.ino())
}

/// The inode of a file agentbridge wrote directly (rather than hardlinked).
/// Recorded so `unsync` can refuse to delete a path the tool has since
/// replaced with a file of its own. `0` when unreadable, which `unsync` treats
/// as "no match" and therefore leaves alone.
fn inode_of(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.ino()).unwrap_or(0)
}

/// The manifest as `status`, `pull` and `unsync` should see it: one row per
/// session written to one place.
pub fn read_manifest() -> Vec<LinkRecord> {
    dedup_manifest(read_manifest_raw())
}

/// Every row on disk, repeats included. Callers that rewrite the manifest use
/// this to tell whether collapsing the repeats changed anything.
fn read_manifest_raw() -> Vec<LinkRecord> {
    let path = manifest_path();
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn append_manifest(records: &[LinkRecord]) -> std::io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let path = manifest_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Two source sessions can carry the same id (e.g. a genuine Codex rollout
    // and a Claude copy of it), and both materialize into the same dest. The
    // last write wins on disk, so earlier rows describing the same dest would
    // report stale counts as drift on the next pull. Keep only the last row
    // per dest.
    //
    // OpenCode is the exception: its dest is one database holding every
    // session, so the row id (kept in `cache`) is what makes an entry unique.
    // Keyed on dest alone a whole run's OpenCode rows collapse into a single
    // manifest entry and `status`/`pull` lose sight of every session but one.
    // Antigravity needs no such exception: each conversation is its own file,
    // so its dest is already unique per row.
    let key = |r: &LinkRecord| -> (PathBuf, Option<PathBuf>) {
        if r.target_provider == "opencode" {
            (r.dest.clone(), Some(r.cache.clone()))
        } else {
            (r.dest.clone(), None)
        }
    };
    let mut seen: std::collections::HashSet<(PathBuf, Option<PathBuf>)> =
        std::collections::HashSet::new();
    let mut kept: Vec<&LinkRecord> = Vec::new();
    for r in records {
        let k = key(r);
        if seen.contains(&k) {
            kept.retain(|x| key(x) != k);
        }
        seen.insert(k);
        kept.push(r);
    }
    let mut existing = fs::read_to_string(&path).unwrap_or_default();
    for r in kept {
        existing.push_str(&serde_json::to_string(r).unwrap_or_default());
        existing.push('\n');
    }
    fs::write(&path, existing)
}

/// Drop repeated manifest rows, keeping the newest of each.
///
/// Two rows are the same entry when they describe the same session written to
/// the same place: `dest`, plus the row id (`cache`) for OpenCode, whose
/// `dest` is one database shared by every session. Rows for different
/// sessions that share a `dest` are left alone — `append_manifest` owns that
/// rule.
fn dedup_manifest(rows: Vec<LinkRecord>) -> Vec<LinkRecord> {
    let key = |r: &LinkRecord| {
        let row_id = (r.target_provider == "opencode").then(|| r.cache.clone());
        (r.session_id.clone(), r.dest.clone(), row_id)
    };
    let mut last: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        last.insert(key(r), i);
    }
    rows.into_iter()
        .enumerate()
        .filter(|(i, r)| last.get(&key(r)) == Some(i))
        .map(|(_, r)| r)
        .collect()
}

/// Make every session on the machine visible in `project` for every detected
/// file-based tool.
///
/// `dry_run` plans without touching the filesystem.
/// Fold turns other tools appended into a session (write-back). Dedup by turn
/// identity: a source session may already contain them if the user also
/// continued it in its own tool.
fn fold_overlay(session: &mut crate::model::Session, session_id: &str) {
    let overlay = overlay_messages(session_id);
    if !overlay.is_empty() {
        let have: std::collections::HashSet<String> =
            session.messages.iter().map(message_key).collect();
        for m in overlay {
            if !have.contains(&message_key(&m)) {
                session.messages.push(m);
            }
        }
        session
            .messages
            .sort_by_key(|m| m.timestamp.map(|t| t.timestamp_millis()).unwrap_or(0));
        for (i, m) in session.messages.iter_mut().enumerate() {
            m.ordinal = i as u64;
        }
    }

    // A rename made in a non-native copy overrides the native title the same
    // way appended turns override the native message list — the native file
    // itself is never touched (invariant 2), so this overlay is the only
    // record of the rename until/unless the session opts into merge-back.
    if let Some(t) = overlay_title(session_id) {
        session.title = Some(t);
    }
}

/// Write turns pulled from other tools back into the session's own native
/// file (opt-in merge-back, `resume --merge`). The claude-code variant
/// re-converts directly into the live root — the converter derives the same
/// `<encoded-project>/<uuid>.jsonl` path the native file lives at, so the
/// file is refreshed in place. The codex variant re-converts to cache and
/// copies the artifact over the native rollout, whose own filename derives
/// from the same session data and therefore matches.
fn merge_back_native(
    registry: &Registry,
    entry: &crate::index::IndexEntry,
) -> Result<(), String> {
    let Some(source) = registry.by_id(&entry.provider) else {
        return Err(format!("merge-back: no connector for {}", entry.provider));
    };
    let mut session = match source.load(&entry.id) {
        Ok(s) => s,
        Err(e) => return Err(format!("merge-back load {}: {}", entry.id, e)),
    };
    fold_overlay(&mut session, &entry.id);

    match entry.provider.as_str() {
        "claude-code" => {
            let Some(live) = live_root("claude-code") else {
                return Err("merge-back: no claude-code live root".to_string());
            };
            ClaudeCodeConverter::new()
                .convert(&session, &live)
                .map(|_| ())
        }
        "codex-cli" => {
            let Some(live) = live_root("codex-cli") else {
                return Err("merge-back: no codex-cli live root".to_string());
            };
            let cache = staging_root("codex-cli");
            let _staged = StagingGuard;
            let dir = session.project_path().unwrap_or_default();
            let artifact = CodexCliConverter::new()
                .convert_multi(&session, &cache, std::slice::from_ref(&dir))?
                .into_iter()
                .next()
                .ok_or_else(|| "merge-back: codex produced no artifact".to_string())?;
            let rel = artifact.strip_prefix(&cache).unwrap_or(&artifact);
            let dest = live.join(rel);
            link_or_copy(&artifact, &dest).map_err(|e| format!("merge-back write: {}", e))?;
            Ok(())
        }
        // Antigravity is deliberately absent: rewriting a native agy body with
        // the minimal protobuf encoder in `antigravity_write` would drop every
        // field agentbridge does not decode. The caller excludes it before
        // reaching here; this arm is the backstop.
        other => Err(format!("merge-back not supported for {}", other)),
    }
}

/// Delete the folder older builds kept every converted session in. Copies
/// already in the tools' stores are their own files and are not affected;
/// this only frees what nothing else points at. Returns true if it was there.
pub fn drop_legacy_cache() -> bool {
    let cache = data_dir().join("cache");
    cache.exists() && fs::remove_dir_all(cache).is_ok()
}

/// Removes this run's staging folder when it goes out of scope, so no
/// converted file outlives the run that made it.
struct StagingGuard;

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(staging_dir());
        // The parent goes too once no other run is using it.
        let _ = fs::remove_dir(data_dir().join("staging"));
    }
}

pub fn sync_into(registry: &Registry, project: &Path, dry_run: bool) -> SyncReport {
    sync_into_since(registry, project, dry_run, None)
}

/// When a session last changed, as far as a cheap look can tell: its own last
/// event, or the time its file was last written. Tools append to a transcript
/// long after the first record a scan reads, so the file time is what catches
/// a session that is still being worked on.
fn changed_at(e: &crate::index::IndexEntry) -> Option<chrono::DateTime<chrono::Utc>> {
    // OpenCode's file is one database shared by every session; its time says
    // nothing about any one of them.
    let file = (e.provider != "opencode")
        .then(|| fs::metadata(&e.source_path).ok()?.modified().ok())
        .flatten()
        .map(chrono::DateTime::<chrono::Utc>::from);
    [e.last_event_at, e.started_at, file].into_iter().flatten().max()
}

/// The state `sync --changed` keeps between runs: when the last one started,
/// and a fingerprint of every session store as it finished.
fn sync_state_path() -> PathBuf {
    data_dir().join("last-sync.json")
}

pub fn last_sync_state() -> Option<(chrono::DateTime<chrono::Utc>, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(sync_state_path()).ok()?).ok()?;
    let at = chrono::DateTime::parse_from_rfc3339(v.get("at")?.as_str()?).ok()?;
    Some((at.with_timezone(&chrono::Utc), v.get("fingerprint")?.as_str()?.to_string()))
}

pub fn record_sync_state(at: chrono::DateTime<chrono::Utc>, fingerprint: &str) -> std::io::Result<()> {
    fs::create_dir_all(data_dir())?;
    fs::write(
        sync_state_path(),
        serde_json::json!({ "at": at.to_rfc3339(), "fingerprint": fingerprint }).to_string(),
    )
}

/// Rename OpenCode rows still under an old id shape and point `manifest` at
/// the new ids. Returns how many rows moved. Does nothing while OpenCode is
/// open.
fn rename_opencode_rows(manifest: &mut [LinkRecord]) -> usize {
    let Some(db) = opencode_db().filter(|db| db.is_file()) else {
        return 0;
    };
    if crate::opencode_write::ensure_safe_to_write().is_err() {
        return 0;
    }
    let moves = crate::opencode_write::migrate_ids(&db).unwrap_or_default();
    for (old, new) in &moves {
        for r in manifest
            .iter_mut()
            .filter(|r| r.target_provider == "opencode" && r.cache.as_os_str() == old.as_str())
        {
            r.cache = PathBuf::from(new);
        }
    }
    moves.len()
}

/// The same rename, for a caller that is not about to sync. `sync --changed`
/// stops early when nothing is new, and old rows must not wait for a new
/// session before they become usable.
pub fn migrate_opencode_rows() -> usize {
    let Some(_lock) = acquire_run_lock() else {
        return 0;
    };
    let mut manifest = read_manifest();
    let moved = rename_opencode_rows(&mut manifest);
    if moved > 0 {
        let body: String = manifest
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default() + "\n")
            .collect();
        let _ = fs::write(manifest_path(), body);
    }
    moved
}

/// `sync_into`, limited to sessions that changed at or after `since`.
///
/// This is what makes an automatic sync cheap: a machine holds thousands of
/// old sessions, and re-publishing all of them every time one new session
/// appears costs minutes. `None` means every session, as `sync_into` does.
pub fn sync_into_since(
    registry: &Registry,
    project: &Path,
    dry_run: bool,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> SyncReport {
    let mut report = SyncReport::default();
    let _lock = if dry_run {
        None
    } else {
        match acquire_run_lock() {
            Some(lock) => Some(lock),
            None => {
                report.errors.push(BUSY.to_string());
                return report;
            }
        }
    };
    let _staged = StagingGuard;
    if !dry_run {
        drop_legacy_cache();
    }
    let mut index: Index = discover(registry);
    if let Some(since) = since {
        // Dropped before anything is opened: judging whether an entry is a
        // copy means reading its file, and that is the cost being avoided.
        index.entries.retain(|e| changed_at(e).is_some_and(|t| t >= since));
    }

    // OpenCode and Antigravity have no live_root (rows/per-conversation
    // databases, not linkable files) but are still valid targets.
    let targets: Vec<String> = registry
        .detected()
        .map(|c| c.id().to_string())
        .filter(|id| {
            live_root(id).is_some()
                || (id == "opencode" && opencode_db().is_some())
                || (id == "antigravity" && crate::antigravity_write::store().is_some())
        })
        .collect();

    // Back up OpenCode's database once per run, before any INSERT.
    let mut opencode_backed_up = false;
    // Same for Codex's index (`state_5.sqlite`).
    let mut codex_backed_up = false;
    // Same for Antigravity's summaries index.
    let mut antigravity_backed_up = false;

    // Manifests written before the OpenCode branch refreshed its rows in
    // place hold one extra copy of each row per past sync. `read_manifest`
    // collapses them; rewrite the file when that removed anything.
    let rows_on_disk = read_manifest_raw().len();
    let mut manifest = read_manifest();
    let mut manifest_dirty = manifest.len() != rows_on_disk;
    // Rows written before OpenCode copies took OpenCode's own id shape are
    // renamed in place, and the manifest follows them. Until then OpenCode's
    // free models refuse those sessions.
    if !dry_run && rename_opencode_rows(&mut manifest) > 0 {
        manifest_dirty = true;
    }
    let already: Vec<(String, PathBuf)> = manifest
        .iter()
        .map(|r| (r.session_id.clone(), r.dest.clone()))
        .collect();

    // Loop prevention (DESIGN.md §6). Sync writes into the tools' own stores,
    // so the next discovery pass sees those files as ordinary sessions. Left
    // unguarded they get re-materialized on every run and the session count
    // multiplies. A file agentbridge created is never a source.
    //
    // OpenCode is keyed differently: every row agentbridge writes there has
    // the *database* as its `dest`, and every session OpenCode owns has that
    // same database as its source path. Matching on the path would mark all
    // of OpenCode's own sessions as generated the moment one row is written
    // (found live: a new OpenCode session never reached another tool). The
    // row id is what identifies agentbridge's output there — from the
    // manifest, and from the marker in the row itself.
    let generated: std::collections::HashSet<PathBuf> = manifest
        .iter()
        .filter(|r| r.target_provider != "opencode")
        .map(|r| r.dest.clone())
        .collect();
    let mut generated_opencode: std::collections::HashSet<String> = manifest
        .iter()
        .filter(|r| r.target_provider == "opencode")
        .map(|r| r.cache.to_string_lossy().to_string())
        .collect();
    if let Some(db) = opencode_db() {
        generated_opencode.extend(crate::opencode_write::written_ids(&db));
    }
    let in_manifest = |e: &crate::index::IndexEntry| {
        if e.provider == "opencode" {
            generated_opencode.contains(&e.id)
        } else {
            generated.contains(&e.source_path)
        }
    };
    // The manifest is not the only witness. A copy made by a run this one
    // raced, or tracked by a manifest since lost, still says it is a copy in
    // its own title or first record. Judged on the entry's *own* file: a copy
    // can share its origin's id, so loading by id could read the wrong one.
    let self_declared: std::collections::HashSet<(String, String, PathBuf)> = index
        .entries
        .iter()
        .filter(|e| !in_manifest(e))
        .filter(|e| {
            let own = match e.provider.as_str() {
                "claude-code" | "codex-cli" => load_materialized(&e.provider, &e.source_path, &e.id),
                _ => registry.by_id(&e.provider).and_then(|c| c.load(&e.id).ok()),
            };
            own.is_some_and(|s| crate::label::is_copy(&s))
        })
        .map(|e| (e.provider.clone(), e.id.clone(), e.source_path.clone()))
        .collect();
    let declares_copy = |e: &crate::index::IndexEntry| {
        self_declared.contains(&(e.provider.clone(), e.id.clone(), e.source_path.clone()))
    };
    let is_generated = |e: &crate::index::IndexEntry| in_manifest(e) || declares_copy(e);

    // Two index entries can carry the same (provider, id) — e.g. a generated
    // file sits alongside its own source. They resolve to one destination, so
    // the second pass would overwrite the first. Prefer a real source over a
    // generated copy: a session whose only claude file is agentbridge's own
    // materialization must still be loaded from its real store, or it would
    // be skipped entirely (loop prevention) and never re-indexed.
    let mut seen: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut entries: Vec<&crate::index::IndexEntry> = Vec::new();
    for e in &index.entries {
        let key = (e.provider.clone(), e.id.clone());
        match seen.get(&key) {
            None => {
                seen.insert(key, entries.len());
                entries.push(e);
            }
            Some(&i) => {
                let kept = entries[i];
                if is_generated(kept) && !is_generated(e) {
                    entries[i] = e;
                }
            }
        }
    }

    for entry in entries {
        if is_generated(entry) {
            if declares_copy(entry) {
                report.skipped_copies += 1;
            }
            continue;
        }
        for target in &targets {
            // A session already native to this tool *in this directory* is
            // left strictly alone (invariant 2) — unless the user opted the
            // session into merge-back (`resume --merge`), in which case turns
            // pulled from other tools' copies are appended to the tool's own
            // file instead.
            if &entry.provider == target && entry.project_path.as_deref() == Some(project) {
                if !dry_run
                    && is_merge(&entry.id)
                    && !overlay_messages(&entry.id).is_empty()
                    && entry.provider != "opencode"
                    // Antigravity is excluded for a stronger reason than
                    // OpenCode's: a real agy body carries protobuf fields
                    // agentbridge does not decode (tool calls, reasoning,
                    // gen_metadata blobs). Rewriting one with the minimal
                    // encoder in `antigravity_write` would silently destroy
                    // everything outside the handful of fields we understand,
                    // so recovered turns stay in the overlay instead.
                    && entry.provider != "antigravity"
                {
                    match merge_back_native(registry, entry) {
                        Ok(()) => report.merged_native += 1,
                        Err(e) => report.errors.push(e),
                    }
                } else {
                    report.skipped_native += 1;
                }
                continue;
            }

            let is_opencode = target == "opencode";
            let is_codex = target == "codex-cli";
            let is_antigravity = target == "antigravity";
            if !is_opencode
                && !is_antigravity
                && (live_root(target).is_none() || converter_for(target).is_none())
            {
                continue;
            }

            // Load through the owning connector, then re-home the session into
            // the requested directory so the target tool scopes it here.
            let Some(source) = registry.by_id(&entry.provider) else {
                continue;
            };
            let mut session = match source.load(&entry.id) {
                Ok(s) => s,
                Err(e) => {
                    report.errors.push(format!("load {}: {}", entry.id, e));
                    continue;
                }
            };
            session.project_id = project.to_string_lossy().to_string();

            // Fold in turns other tools appended (write-back). Dedup by turn
            // identity: a source session may already contain them if the user
            // also continued it in its own tool.
            fold_overlay(&mut session, &entry.id);

            // Stamp the cross-tool label into the title every target picker
            // shows: origin tool, name, the session's own start time, and the
            // first 8 characters of its id. Applied after `fold_overlay` so a
            // rename recovered from another tool is what gets labeled, and
            // before every write so all four copies carry the same string.
            //
            // `session.provider`/`session.id` are still the *origin's* here
            // (only `project_id` was re-homed above), which is exactly what
            // makes the label identical in every tool.
            crate::label::apply(&mut session);

            if dry_run {
                report.created.push(LinkRecord {
                    dest: if is_opencode {
                        opencode_db().unwrap_or_default()
                    } else if is_antigravity {
                        crate::antigravity_write::store()
                            .unwrap_or_default()
                            .join("conversations")
                            .join("(planned)")
                    } else {
                        live_root(target).unwrap_or_default().join("(planned)")
                    },
                    cache: staging_root(target).join("(planned)"),
                    session_id: entry.id.clone(),
                    source_provider: entry.provider.clone(),
                    target_provider: target.clone(),
                    project: project.to_path_buf(),
                    inode: 0,
                    message_count: session.messages.len(),
                    title: session.title.clone(),
                });
                continue;
            }

            // Antigravity: one SQLite body per conversation plus a summaries
            // row. Neither is linkable, so this writes both directly, the same
            // way OpenCode INSERTs rows.
            if is_antigravity {
                let Some(home) = crate::antigravity_write::store() else {
                    continue;
                };
                if let Err(e) = crate::antigravity_write::ensure_safe_to_write() {
                    report.errors.push(e.to_string());
                    continue;
                }
                let dirs = target_dirs(project, &session);
                if !antigravity_backed_up
                    && crate::antigravity_write::summaries_db(&home).is_file()
                    && crate::antigravity_write::will_insert(&home, &session, &dirs)
                {
                    // A run that would only refresh conversations agentbridge
                    // already wrote needs no backup; one that inserts does.
                    match crate::antigravity_write::backup(
                        &crate::antigravity_write::summaries_db(&home),
                    ) {
                        Ok(_) => antigravity_backed_up = true,
                        Err(e) => {
                            report.errors.push(e.to_string());
                            continue;
                        }
                    }
                }
                let (rows, errors) =
                    crate::antigravity_write::write_sessions(&home, &session, &dirs);
                for e in errors {
                    report.errors.push(format!("antigravity {}", e));
                }
                for row in rows {
                    let record = LinkRecord {
                        // The body is the artifact a pull re-reads, so it is
                        // the `dest`; `unsync` removes it and its index row
                        // together by the marker.
                        dest: row.body.clone(),
                        cache: PathBuf::from(&row.id),
                        session_id: entry.id.clone(),
                        source_provider: entry.provider.clone(),
                        target_provider: target.clone(),
                        project: PathBuf::from(row.directory),
                        inode: inode_of(&row.body),
                        message_count: row.messages,
                        // The title actually persisted, not session.title:
                        // an untitled session gets a derived name, and that
                        // text is what a later pull reads back. Recording the
                        // source's `None` here would make every untitled
                        // session look renamed on the next pull.
                        title: Some(row.title),
                    };
                    // A re-sync rewrites the same conversation in place, so the
                    // manifest row must be *updated*, not appended — otherwise
                    // every run adds another row per conversation and `pull`
                    // reads one session as several tools' worth of new work
                    // (observed as "antigravity+antigravity+antigravity" in a
                    // conflict report). The file targets do the same thing via
                    // their `unchanged` branch below.
                    if let Some(existing) = manifest
                        .iter_mut()
                        .find(|r| r.session_id == record.session_id && r.dest == record.dest)
                    {
                        *existing = record;
                        manifest_dirty = true;
                        report.unchanged += 1;
                    } else {
                        report.created.push(record);
                    }
                }
                continue;
            }

            // OpenCode: rows, not files — INSERT instead of link (DESIGN.md §5).
            if is_opencode {
                let Some(db) = opencode_db() else { continue };
                if let Err(e) = crate::opencode_write::ensure_safe_to_write() {
                    report.errors.push(e.to_string());
                    continue;
                }
                // The sync directory, plus `$HOME` — which resolves to the
                // `global` project and so carries the session into every
                // folder that is not a project worktree of its own.
                let dirs = target_dirs(project, &session);
                if !opencode_backed_up
                    && crate::opencode_write::will_insert(&db, &session, &dirs)
                {
                    // A run that would only refresh rows agentbridge already
                    // wrote needs no backup; one that inserts a new row does.
                    match crate::opencode_write::backup(&db) {
                        Ok(_) => opencode_backed_up = true,
                        Err(e) => {
                            report.errors.push(e.to_string());
                            continue;
                        }
                    }
                }
                let (rows, errors) =
                    crate::opencode_write::write_sessions(&db, &session, &dirs);
                for e in errors {
                    report.errors.push(format!("opencode {}", e));
                }
                for row in rows {
                    let record = LinkRecord {
                        dest: db.clone(),
                        cache: PathBuf::from(row.id),
                        session_id: entry.id.clone(),
                        source_provider: entry.provider.clone(),
                        target_provider: target.clone(),
                        project: PathBuf::from(row.directory),
                        inode: 0,
                        // True row count, not session.messages.len(): a
                        // placeholder turn may have been inserted.
                        message_count: row.messages,
                        // The title actually persisted, not session.title —
                        // OpenCode always writes something (falling back to
                        // a derived name for an untitled session), and that
                        // fallback text is what a later pull will read back.
                        // Recording the source's often-`None` title here
                        // would make every untitled session look renamed on
                        // the next pull (see RowWritten::title).
                        title: Some(row.title),
                    };
                    // A re-sync rewrites the same row in place, so its
                    // manifest entry is updated, not appended — the same rule
                    // the antigravity branch follows, for the same reason: an
                    // appended copy per run made `pull` report one typed turn
                    // as several and a conflict of OpenCode against itself.
                    if let Some(existing) = manifest.iter_mut().find(|r| {
                        r.target_provider == "opencode"
                            && r.dest == record.dest
                            && r.cache == record.cache
                    }) {
                        *existing = record;
                        manifest_dirty = true;
                        report.unchanged += 1;
                    } else {
                        report.created.push(record);
                    }
                }
                continue;
            }

            // Convert once, into this run's staging folder. Each file is moved
            // on into the tool's store below and the folder is emptied.
            let cache = staging_root(target);
            let live = live_root(target).unwrap_or_default();
            let converter = match converter_for(target) { Some(c) => c, None => continue };
            // Codex scopes the resume picker to the rollout's session_meta
            // cwd, so one file per target directory: the sync project and the
            // home directory (CONNECTORS.md §2).
            let dirs = target_dirs(project, &session);
            let artifacts = match converter.convert_multi(&session, &cache, &dirs) {
                Ok(p) => p,
                Err(e) => {
                    report.errors.push(format!("convert {}: {}", entry.id, e));
                    continue;
                }
            };

            for (dir, artifact) in dirs.iter().zip(&artifacts) {
                // A directory whose variant would only duplicate what the
                // target tool already lists there natively gets none: e.g. a
                // codex session the user started in $HOME already has its own
                // rollout file, so our copy would show up twice in /resume.
                // `codex exec` sessions carry source `exec` and never appear
                // in the picker, so they still get a variant.
                if *dir != session.project_path().unwrap_or_default()
                    && is_codex
                    && index.entries.iter().any(|e| {
                        e.provider == entry.provider
                            && e.id == entry.id
                            && e.project_path.as_deref() == Some(Path::new(dir.as_str()))
                            && interactive_source(e.source.as_deref())
                            && !generated.contains(&e.source_path)
                    })
                {
                    if let Some(db) = crate::codex_write::state_db()
                        && crate::codex_write::ensure_safe_to_write().is_ok()
                    {
                        let _ = crate::codex_write::remove_thread_rows_for(
                            &db,
                            &session,
                            std::slice::from_ref(dir),
                        );
                    }
                    // A variant materialized before the native-visible check
                    // existed must not survive next to the tool's own file.
                    let rel = artifact.strip_prefix(&cache).unwrap_or(artifact);
                    let stale = live.join(rel);
                    if stale.exists() {
                        let _ = fs::remove_file(&stale);
                    }
                    if let Some(pos) = manifest
                        .iter()
                        .position(|r| r.session_id == entry.id && r.dest == stale)
                    {
                        manifest.remove(pos);
                        manifest_dirty = true;
                    }
                    continue;
                }
                // Mirror the artifact's layout under the tool's live root.
                let rel = artifact.strip_prefix(&cache).unwrap_or(artifact);
                let dest = live.join(rel);

                if already.iter().any(|(id, d)| id == &entry.id && d == &dest) && dest.exists() {
                    // Already in the tool's store. Replace it only when the
                    // session really changed, so an idle sync rewrites nothing.
                    // The manifest count must follow, or the next pull reads
                    // agentbridge's own refresh as tool drift.
                    //
                    // Title tracks session.title here, unlike the OpenCode branch
                    // below: `load_materialized` for codex-cli reads the rollout
                    // *file*, which never carries a title in the modern format —
                    // never `ensure_codex_row`'s DB-side fallback, which pull_back
                    // can never observe through that read path anyway.
                    if !same_content(artifact, &dest)
                        && let Err(e) = link_or_copy(artifact, &dest)
                    {
                        report.errors.push(format!("refresh {}: {}", entry.id, e));
                        continue;
                    }
                    if let Some(row) = manifest
                        .iter_mut()
                        .find(|r| r.session_id == entry.id && r.dest == dest)
                    {
                        row.message_count = session.messages.len();
                        row.title = session.title.clone();
                        // The file may be a new one now; `unsync` checks this.
                        row.inode = inode_of(&dest);
                        row.cache = dest.clone();
                        manifest_dirty = true;
                    }
                    // A materialized rollout still needs its `threads` row for
                    // `codex /resume`; retry in case the previous run was refused
                    // while Codex was open.
                    if is_codex {
                        ensure_codex_row(&mut report, &mut codex_backed_up, &session, &dest, std::slice::from_ref(dir));
                    }
                    report.unchanged += 1;
                    continue;
                }

                // Invariant 2, enforced where the write happens: a file already
                // at `dest` that agentbridge did not make is a session the tool
                // owns. That can only be reached when a copy sharing its
                // origin's id is mistaken for a source, and then the
                // destination *is* the original. Leave it alone.
                if dest.exists()
                    && !generated.contains(&dest)
                    && load_materialized(target, &dest, &entry.id)
                        .is_some_and(|s| !crate::label::is_copy(&s))
                {
                    report.skipped_native += 1;
                    continue;
                }

                // Into the tool's store. The staged file is removed when the
                // run ends, leaving this as the only one.
                match link_or_copy(artifact, &dest) {
                    Ok(inode) => {
                        if is_codex {
                            ensure_codex_row(&mut report, &mut codex_backed_up, &session, &dest, std::slice::from_ref(dir));
                        }
                        report.created.push(LinkRecord {
                            cache: dest.clone(),
                            dest,
                            session_id: entry.id.clone(),
                            source_provider: entry.provider.clone(),
                            target_provider: target.clone(),
                            project: project.to_path_buf(),
                            inode,
                            message_count: session.messages.len(),
                            title: session.title.clone(),
                        });
                    }
                    Err(e) => report.errors.push(format!("link {}: {}", entry.id, e)),
                }
            }
        }
    }

    if !dry_run {
        // Rewrite refreshed rows before appending newly created ones, so the
        // append never clobbers the update.
        if manifest_dirty {
            let body: String = manifest
                .iter()
                .map(|r| serde_json::to_string(r).unwrap_or_default() + "\n")
                .collect();
            if let Err(e) = fs::write(manifest_path(), body) {
                report.errors.push(format!("manifest update failed: {}", e));
            }
        }
        if let Err(e) = append_manifest(&report.created) {
            report.errors.push(format!("manifest write failed: {}", e));
        }
    }

    report
}

/// The directories one sync pass makes `session` visible in. Every tool here
/// scopes its picker to the directory you launch it from, so presence has to
/// be materialized per directory: the directory being synced, plus `$HOME` —
/// the one directory every tool can be started from, and (for OpenCode) the
/// `global` project that covers any folder that is not a worktree of its own.
fn target_dirs(project: &Path, session: &crate::model::Session) -> Vec<String> {
    let mut dirs = vec![
        session
            .project_path()
            .unwrap_or_else(|| project.to_string_lossy().to_string()),
    ];
    if let Ok(home) = std::env::var("HOME") {
        let home = home.trim_end_matches('/').to_string();
        if !dirs.contains(&home) {
            dirs.push(home);
        }
    }
    dirs
}

/// Sources the Codex resume picker lists (INTERACTIVE_SESSION_SOURCES in the
/// app-server, verified in codex 0.146 source). `exec` rollouts are indexed
/// but never listed.
fn interactive_source(source: Option<&str>) -> bool {
    matches!(source, Some("cli" | "vscode" | "atlas" | "chatgpt"))
}

/// Give a materialized Codex rollout its `threads` rows so `codex /resume`
/// lists it (CONNECTORS.md §2). Guarded and backed up once per run, exactly
/// like the OpenCode path. Silently skipped when Codex has never run here
/// (no `state_5.sqlite` to index into) or while Codex is open.
/// Returns the title actually persisted into the `threads` row(s) touched,
/// when the write happened at all — `None` when skipped (no state_5.sqlite,
/// Codex running, or an error, all already reported). Callers must record
/// this as `LinkRecord.title`, not `session.title` (see
/// `ThreadRowReport::title`).
fn ensure_codex_row(
    report: &mut SyncReport,
    backed_up: &mut bool,
    session: &crate::model::Session,
    rollout_path: &Path,
    dirs: &[String],
) -> Option<String> {
    // The name Codex shows. Done before the index guard on purpose: this is
    // an append to a file Codex itself appends to, so it is safe while Codex
    // is open, and waiting would leave the session to be named by Codex.
    if let (Some(home), Some(label)) =
        (crate::connectors::codex_cli::config_home(), session.title.as_deref())
    {
        let thread = crate::convert::ClaudeCodeConverter::session_uuid(&session.id);
        crate::codex_write::ensure_thread_name(&home, &thread, label);
    }
    let db = crate::codex_write::state_db()?;
    if let Err(e) = crate::codex_write::ensure_safe_to_write() {
        report.errors.push(e.to_string());
        return None;
    }
    if !*backed_up {
        // Refreshing rows agentbridge already wrote is idempotent; only an
        // INSERT of a new row justifies the one-per-run database backup.
        let will_insert = dirs.iter().any(|d| {
            let sid = crate::codex_write::session_uuid_for_dir(&session.id, d);
            crate::codex_write::thread_row_exists(&db, &sid)
                .map(|e| !e)
                .unwrap_or(true)
        });
        if will_insert {
            match crate::codex_write::backup(&db) {
                Ok(_) => *backed_up = true,
                Err(e) => {
                    report.errors.push(e.to_string());
                    return None;
                }
            }
        }
    }
    match crate::codex_write::ensure_thread_rows(&db, session, rollout_path, dirs) {
        Ok(r) => {
            report.codex_indexed += r.inserted;
            Some(r.title)
        }
        Err(e) => {
            report.errors.push(e.to_string());
            None
        }
    }
}

/// Remove exactly the files agentbridge created. A destination whose inode
/// no longer matches the manifest belongs to something else now and is kept.
/// Which copies a partial `unsync` takes away. Empty means everything.
#[derive(Debug, Default, Clone)]
pub struct UnsyncFilter {
    /// Copies written by a sync of this folder.
    pub project: Option<PathBuf>,
    /// Copies of these sessions, wherever they were written.
    pub sessions: Vec<String>,
}

impl UnsyncFilter {
    fn is_empty(&self) -> bool {
        self.project.is_none() && self.sessions.is_empty()
    }

    fn matches(&self, r: &LinkRecord) -> bool {
        self.project.as_deref() == Some(r.project.as_path())
            || self.sessions.contains(&r.session_id)
    }
}

/// `unsync` for part of the machine: only the copies `filter` names are
/// removed, one by one, and every other manifest row is kept. A copy whose
/// tool is open is left in the manifest for a later run.
pub fn unsync_matching(dry_run: bool, filter: &UnsyncFilter) -> UnsyncReport {
    if filter.is_empty() {
        return unsync(dry_run);
    }
    let mut report = UnsyncReport::default();
    // This rewrites the manifest, so it waits its turn behind a running sync
    // rather than racing it. After two minutes it goes ahead: a lock that old
    // is far more likely a run that died than one still working.
    let _lock = if dry_run {
        None
    } else {
        let mut lock = acquire_run_lock();
        for _ in 0..240 {
            if lock.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            lock = acquire_run_lock();
        }
        lock
    };
    let records = read_manifest();
    let mut keep: Vec<LinkRecord> = Vec::new();

    for r in records {
        if !filter.matches(&r) {
            keep.push(r);
            continue;
        }
        if dry_run {
            report.removed.push(r.dest.clone());
            continue;
        }
        let id = materialized_id(&r);
        let removed = match r.target_provider.as_str() {
            "opencode" => {
                crate::opencode_write::ensure_safe_to_write().is_ok()
                    && crate::opencode_write::remove_one(&r.dest, &id).is_ok()
            }
            "antigravity" => {
                crate::antigravity_write::ensure_safe_to_write().is_ok()
                    && crate::antigravity_write::store()
                        .is_some_and(|home| crate::antigravity_write::remove_one(&home, &id).is_ok())
            }
            _ => match fs::metadata(&r.dest) {
                Err(_) => {
                    report.missing += 1;
                    continue;
                }
                Ok(meta) if meta.ino() != r.inode => {
                    report.kept_foreign.push(r.dest.clone());
                    keep.push(r);
                    continue;
                }
                Ok(_) => {
                    if r.target_provider == "codex-cli"
                        && let Some(db) = crate::codex_write::state_db()
                        && crate::codex_write::ensure_safe_to_write().is_ok()
                    {
                        let _ = crate::codex_write::remove_rows_for_path(&db, &r.dest);
                    }
                    let gone = fs::remove_file(&r.dest).is_ok();
                    if gone
                        && let Some(parent) = r.dest.parent()
                        && fs::read_dir(parent).map(|mut d| d.next().is_none()).unwrap_or(false)
                    {
                        let _ = fs::remove_dir(parent);
                    }
                    gone
                }
            },
        };
        if removed {
            report.removed.push(r.dest.clone());
        } else {
            report.kept_foreign.push(r.dest.clone());
            keep.push(r);
        }
    }

    if !dry_run {
        let body: String = keep
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default() + "\n")
            .collect();
        let _ = fs::write(manifest_path(), body);
    }
    report
}

pub fn unsync(dry_run: bool) -> UnsyncReport {
    let mut report = UnsyncReport::default();
    let records = read_manifest();

    // OpenCode rows are removed by their marker, not by path.
    if !dry_run && records.iter().any(|r| r.target_provider == "opencode")
        && let Some(db) = opencode_db() {
            match crate::opencode_write::ensure_safe_to_write() {
                Ok(()) => {
                    let _ = crate::opencode_write::remove_all(&db);
                }
                Err(_) => {
                    // Leave them; the operator is told to quit OpenCode.
                }
            }
        }

    // Codex `threads` rows agentbridge inserted are removed by their marker.
    if !dry_run
        && records.iter().any(|r| r.target_provider == "codex-cli")
        && let Some(db) = crate::codex_write::state_db() {
            match crate::codex_write::ensure_safe_to_write() {
                Ok(()) => {
                    let _ = crate::codex_write::remove_all(&db);
                }
                Err(_) => {
                    // Leave them; the operator is told to quit Codex.
                }
            }
        }

    // Antigravity conversations agentbridge wrote are removed by their marker,
    // which takes the body and its summaries row together — an orphaned body
    // would still be found by a filesystem scan.
    if !dry_run
        && records.iter().any(|r| r.target_provider == "antigravity")
        && let Some(home) = crate::antigravity_write::store() {
            match crate::antigravity_write::ensure_safe_to_write() {
                Ok(()) => {
                    let _ = crate::antigravity_write::remove_all(&home);
                }
                Err(_) => {
                    // Leave them; the operator is told to quit Antigravity.
                }
            }
        }

    for r in &records {
        if r.target_provider == "opencode" {
            report.removed.push(r.dest.clone());
            continue;
        }
        // Antigravity bodies were already deleted with their index rows above,
        // by marker rather than by path. Counting them as missing here would
        // report a successful teardown as a failure.
        if r.target_provider == "antigravity" {
            report.removed.push(r.dest.clone());
            continue;
        }
        let Ok(meta) = fs::metadata(&r.dest) else {
            report.missing += 1;
            continue;
        };
        if meta.ino() != r.inode {
            report.kept_foreign.push(r.dest.clone());
            continue;
        }
        if dry_run {
            report.removed.push(r.dest.clone());
            continue;
        }
        match fs::remove_file(&r.dest) {
            Ok(()) => {
                // Prune the directory we created for this project, but only
                // if it is now empty — never remove a dir holding a tool's
                // own sessions.
                if let Some(parent) = r.dest.parent() {
                    let empty = fs::read_dir(parent).map(|mut d| d.next().is_none());
                    if empty.unwrap_or(false) {
                        let _ = fs::remove_dir(parent);
                    }
                }
                report.removed.push(r.dest.clone());
            }
            Err(_) => report.missing += 1,
        }
    }

    if !dry_run {
        // The cache holds only derived artifacts (DESIGN.md Rule 2) — once
        // nothing links to them they are pure waste, so drop them with the
        // links. Source sessions are never in here.
        if report.kept_foreign.is_empty() {
            let _ = fs::remove_dir_all(data_dir().join("cache"));
        }

        // Keep only the records we could not remove.
        let keep: Vec<&LinkRecord> = records
            .iter()
            .filter(|r| report.kept_foreign.contains(&r.dest))
            .collect();
        let body: String = keep
            .iter()
            .map(|r| serde_json::to_string(r).unwrap_or_default() + "\n")
            .collect();
        let _ = fs::write(manifest_path(), body);
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::Connector;
    use rusqlite::{params, Connection};
    use std::path::Path;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    /// HOME / AGENTBRIDGE_DATA_DIR are process-global, so sandboxed tests
    /// must not run concurrently or they clobber each other's env.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point HOME and the data dir at a temp tree so tests never touch the
    /// operator's real `~/.claude` or `~/.codex`.
    struct Sandbox {
        _tmp: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Sandbox {
        fn new() -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("AGENTBRIDGE_DATA_DIR", tmp.path().join(".agentbridge"));
                // live_root() now honors these overrides the same way the
                // read connectors do (regression: it used to hardcode
                // ~/.claude and ~/.codex) — sandbox them too, or a sandboxed
                // test run under an operator's own overridden shell would
                // write into their real session store instead of tmp.
                std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path().join(".claude"));
                std::env::set_var("CODEX_HOME", tmp.path().join(".codex"));
                // Same hazard for antigravity: `antigravity_write::store()`
                // honors ANTIGRAVITY_HOME, so without this a sandboxed test
                // would write conversations into the operator's real
                // ~/.gemini store (HANDOFF §sandbox doctrine).
                std::env::set_var("ANTIGRAVITY_HOME", tmp.path().join(".gemini/antigravity-cli"));
                // And OpenCode: its database path honors XDG_DATA_HOME, so an
                // operator shell that exports it would send a sandboxed test
                // to their real database. Same location HOME alone gives.
                std::env::set_var("XDG_DATA_HOME", tmp.path().join(".local/share"));
            }
            Sandbox { _tmp: tmp, _guard: guard }
        }
    }

    #[test]
    fn test_link_or_copy_creates_hardlink_not_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("artifact.jsonl");
        fs::write(&src, "hello").unwrap();
        let dest = tmp.path().join("sub/dir/link.jsonl");

        let inode = link_or_copy(&src, &dest).unwrap();

        // Same inode => one physical copy, two names (DESIGN.md Rule 3).
        assert_eq!(inode, fs::metadata(&src).unwrap().ino());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");
    }

    /// Regression: linking a file onto itself must not destroy it. `fs::copy`
    /// truncates the destination before reading the source, so when both names
    /// are the same inode the content is lost. Observed as a 240-record
    /// session collapsing to a single line.
    #[test]
    fn test_relinking_same_inode_does_not_truncate() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("artifact.jsonl");
        fs::write(&src, "line1\nline2\nline3\n").unwrap();
        let dest = tmp.path().join("linked.jsonl");
        link_or_copy(&src, &dest).unwrap();

        // Sync again over the existing link — the common idempotent case.
        link_or_copy(&src, &dest).unwrap();

        assert_eq!(
            fs::read_to_string(&dest).unwrap(),
            "line1\nline2\nline3\n",
            "re-linking must preserve content"
        );
        assert_eq!(fs::read_to_string(&src).unwrap(), "line1\nline2\nline3\n");
    }

    /// Replacing a *different* existing file must swap it wholesale, never
    /// leave it truncated.
    #[test]
    fn test_relinking_over_different_file_replaces_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("new.jsonl");
        fs::write(&src, "NEW\n").unwrap();
        let dest = tmp.path().join("old.jsonl");
        fs::write(&dest, "OLD CONTENT THAT IS LONGER\n").unwrap();

        link_or_copy(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "NEW\n");
        assert!(
            !tmp.path().join("old.agentbridge-tmp").exists(),
            "temp file must not be left behind"
        );
    }

    #[test]
    fn test_unsync_keeps_files_it_did_not_create() {
        let _sb = Sandbox::new();
        let tmp = tempfile::tempdir().unwrap();

        let foreign = tmp.path().join("someone-elses.jsonl");
        fs::write(&foreign, "not ours").unwrap();

        // A manifest record claiming an inode that does not match reality.
        let rec = LinkRecord {
            dest: foreign.clone(),
            cache: tmp.path().join("cache.jsonl"),
            session_id: "s1".into(),
            source_provider: "codex-cli".into(),
            target_provider: "claude-code".into(),
            project: tmp.path().to_path_buf(),
            inode: 999_999_999,
            message_count: 0,
            title: None,
        };
        append_manifest(&[rec]).unwrap();

        let report = unsync(false);
        assert!(foreign.exists(), "must not delete a file it did not create");
        assert_eq!(report.removed.len(), 0);
        assert_eq!(report.kept_foreign.len(), 1);
    }

    /// Regression: `live_root` used to hardcode `~/.claude` and `~/.codex`,
    /// ignoring `CLAUDE_CONFIG_DIR`/`CODEX_HOME`. On a machine where either
    /// is redirected (e.g. a non-default Claude Code install), materialized
    /// copies silently went to the default path instead — invisible to the
    /// tool that actually reads from the override.
    #[test]
    fn test_live_root_honors_config_dir_overrides() {
        let _sb = Sandbox::new();
        let custom = tempfile::tempdir().unwrap();
        let claude_dir = custom.path().join("custom-claude-home");
        let codex_dir = custom.path().join("custom-codex-home");
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude_dir);
            std::env::set_var("CODEX_HOME", &codex_dir);
        }

        assert_eq!(
            live_root("claude-code"),
            Some(claude_dir.join("projects")),
            "must honor CLAUDE_CONFIG_DIR, not hardcode ~/.claude/projects"
        );
        assert_eq!(
            live_root("codex-cli"),
            Some(codex_dir),
            "must honor CODEX_HOME, not hardcode ~/.codex"
        );
    }

    const NATIVE_UUID: &str = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";

    /// A native Claude Code session file in the sandboxed live root, its cwd
    /// pointed at `project` so discovery treats it as native *there*.
    fn write_native_claude_session(root: &Path, project: &str) {
        let encoded = crate::convert::ClaudeCodeConverter::encode_project_dir(project);
        let dir = root.join(&encoded);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{}.jsonl", NATIVE_UUID));
        let body = format!(
            "{{\"uuid\":\"{}\",\"type\":\"conversation_start\",\"cwd\":\"{}\",\"timestamp\":\"2026-07-01T12:00:00Z\",\"title\":\"Merge test\"}}\n\
             {{\"uuid\":\"b2c3d4e5-f6a7-8901-bcde-f12345678901\",\"parentUuid\":\"{}\",\"type\":\"user_message\",\"cwd\":\"{}\",\"timestamp\":\"2026-07-01T12:00:05Z\",\"content\":\"first question\"}}\n",
            NATIVE_UUID, project, NATIVE_UUID, project
        );
        fs::write(file, body).unwrap();
    }

    fn native_registry(root: PathBuf) -> crate::connector::Registry {
        crate::connector::Registry::new(vec![Box::new(
            crate::connectors::claude_code::TestClaudeCode::new(root),
        )])
    }

    /// A registry holding a real antigravity connector rooted in the sandbox,
    /// so it is both a discovery source and a write target.
    fn agy_registry(claude_root: PathBuf) -> crate::connector::Registry {
        crate::connector::Registry::new(vec![
            Box::new(crate::connectors::claude_code::TestClaudeCode::new(claude_root)),
            Box::new(crate::connectors::antigravity::AntigravityConnector::new()),
        ])
    }

    /// Create the sandbox's antigravity store so `store()`/`detect()` see it.
    fn make_agy_store() -> PathBuf {
        let home = crate::antigravity_write::store()
            .unwrap_or_else(|| crate::connectors::antigravity::write_home().unwrap());
        fs::create_dir_all(home.join("conversations")).unwrap();
        home
    }

    /// Sync must materialize a Claude session **into** antigravity: a body and
    /// an index row, both discoverable by antigravity's own connector.
    #[test]
    fn test_sync_materializes_into_antigravity() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let agy_home = make_agy_store();
        let registry = agy_registry(live);

        let report = sync_into(&registry, Path::new("/tmp/merge-project"), false);

        let agy: Vec<&LinkRecord> = report
            .created
            .iter()
            .filter(|r| r.target_provider == "antigravity")
            .collect();
        assert!(!agy.is_empty(), "antigravity must be a sync target: {:?}", report.errors);
        for r in &agy {
            assert!(r.dest.is_file(), "body written at {}", r.dest.display());
            assert!(r.inode != 0, "inode recorded so unsync can verify ownership");
            assert!(r.title.is_some(), "title recorded for rename detection");
        }
        assert_eq!(
            crate::antigravity_write::count_written(&agy_home),
            agy.len(),
            "one marked conversation per manifest row"
        );

        // And agy's own reader finds them.
        let connector = crate::connectors::antigravity::AntigravityConnector::new();
        let ids: Vec<String> = connector
            .scan()
            .unwrap()
            .filter_map(|r| r.ok())
            .map(|r| r.id)
            .collect();
        for r in &agy {
            let id = r.cache.to_string_lossy().to_string();
            assert!(ids.contains(&id), "{} must be listed by the connector", id);
        }
    }

    /// Re-running must refresh, not accumulate conversations.
    #[test]
    fn test_sync_into_antigravity_is_idempotent() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let agy_home = make_agy_store();
        let registry = agy_registry(live);

        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let after_first = crate::antigravity_write::count_written(&agy_home);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let after_second = crate::antigravity_write::count_written(&agy_home);
        assert_eq!(after_first, after_second, "no duplicate conversations");
        assert!(after_first > 0);
    }

    /// A conversation agentbridge wrote into agy must not be re-materialized as
    /// if the user had authored it there — that is the loop that multiplies
    /// sessions on every run (DESIGN.md §6).
    #[test]
    fn test_sync_does_not_feed_on_conversations_it_wrote_into_antigravity() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let agy_home = make_agy_store();
        let registry = agy_registry(live);

        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let first = crate::antigravity_write::count_written(&agy_home);
        // Two more passes: a feedback loop would grow the count each time.
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        assert_eq!(
            crate::antigravity_write::count_written(&agy_home),
            first,
            "agentbridge's own antigravity conversations must never become sources"
        );
    }

    /// The cross-tool label must be what lands in every target's title, must
    /// be identical across targets for one session, and must not read as a
    /// rename on the next pull.
    #[test]
    fn test_label_is_written_to_every_target_and_is_not_a_false_rename() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        make_agy_store();
        let registry = agy_registry(live);

        let report = sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let native = report
            .created
            .iter()
            .filter(|r| r.session_id == NATIVE_UUID)
            .collect::<Vec<_>>();
        assert!(!native.is_empty(), "the session was materialized somewhere");

        let mut labels: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &native {
            let title = r.title.as_deref().expect("every row records a title");
            let l = crate::label::parse(title)
                .unwrap_or_else(|| panic!("{} title must be a label: {:?}", r.target_provider, title));
            // The fixture session carries an explicit name; it must survive
            // verbatim rather than being reworded or clipped.
            assert_eq!(l.name, "Merge test", "existing name kept as-is");
            assert!(NATIVE_UUID.starts_with(l.id), "label id prefixes the origin id");
            assert_eq!(l.provider, "claude-code", "label names the origin tool");
            labels.insert(title.to_string());
        }
        assert_eq!(
            labels.len(),
            1,
            "one session must carry one identical label into every tool: {:?}",
            labels
        );

        // The label is what agentbridge wrote, so reading it back is not a
        // rename — this is the 705-false-rename hazard.
        let pulled = pull_back(false);
        assert!(
            pulled.renamed.is_empty(),
            "labeling must not report renames: {:?}",
            pulled.renamed
        );
    }

    /// Regression: the antigravity branch always *appended* its manifest rows,
    /// so every re-sync added another row per conversation. `pull` then read a
    /// single session as several tools' worth of new work and reported a
    /// conflict against itself ("antigravity+antigravity+antigravity").
    #[test]
    fn test_resync_updates_antigravity_manifest_rows_instead_of_appending() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        make_agy_store();
        let registry = agy_registry(live);

        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let after_first = read_manifest().len();
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        assert_eq!(
            read_manifest().len(),
            after_first,
            "re-syncing must refresh manifest rows, not add more"
        );

        // And exactly one row per (session, dest).
        let rows = read_manifest();
        let mut seen: std::collections::HashSet<(String, PathBuf)> =
            std::collections::HashSet::new();
        for r in rows.iter().filter(|r| r.target_provider == "antigravity") {
            assert!(
                seen.insert((r.session_id.clone(), r.dest.clone())),
                "duplicate manifest row for {} -> {}",
                r.session_id,
                r.dest.display()
            );
        }
    }

    /// An empty OpenCode database at the sandboxed default path, with the
    /// schema the write path and the reader both need.
    fn make_opencode_db() -> PathBuf {
        let db = crate::connectors::opencode::default_db_path();
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                sandboxes TEXT NOT NULL);
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
                title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                metadata TEXT,
                FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL,
                FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL,
                FOREIGN KEY (message_id) REFERENCES message(id) ON DELETE CASCADE);
            INSERT INTO project VALUES ('global','/',0,0,'[]');
            "#,
        )
        .unwrap();
        db
    }

    /// A session OpenCode itself created: no agentbridge marker, one user turn.
    fn insert_native_opencode_session(db: &Path, id: &str, directory: &str, text: &str) {
        let conn = Connection::open(db).unwrap();
        let ts: i64 = 1_790_927_764_000;
        conn.execute(
            "INSERT INTO session (id, project_id, slug, directory, title, version, \
             time_created, time_updated) VALUES (?1,'global','s',?2,?3,'1.18.30',?4,?4)",
            params![id, directory, format!("New session - 2026-10-02T07:56:04.904Z"), ts],
        )
        .unwrap();
        append_opencode_user_turn(&conn, id, &format!("msg_{id}"), ts, text);
    }

    fn append_opencode_user_turn(conn: &Connection, session: &str, msg_id: &str, ts: i64, text: &str) {
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![msg_id, session, ts, serde_json::json!({"role":"user"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![
                format!("prt_{msg_id}"),
                msg_id,
                session,
                ts,
                serde_json::json!({"type":"text","text":text}).to_string()
            ],
        )
        .unwrap();
    }

    /// A registry with a Claude reader and a real OpenCode connector, both in
    /// the sandbox, so OpenCode is a discovery source and a write target.
    fn opencode_registry(claude_root: PathBuf) -> crate::connector::Registry {
        crate::connector::Registry::new(vec![
            Box::new(crate::connectors::claude_code::TestClaudeCode::new(claude_root)),
            Box::new(crate::connectors::opencode::OpenCodeConnector::new()),
        ])
    }

    /// Found live 2026-10-02 (real opencode 1.18.30 database, sandboxed): the
    /// OpenCode branch always *appended* its manifest rows, so every sync added
    /// another row per session. One turn typed in OpenCode was then reported
    /// as 5 turns and a conflict of OpenCode against itself
    /// ("opencode+opencode+opencode+opencode+opencode"). Same bug the
    /// antigravity branch had.
    #[test]
    fn test_resync_updates_opencode_manifest_rows_instead_of_appending() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let db = make_opencode_db();
        let registry = opencode_registry(live);

        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let after_first = read_manifest().len();
        assert!(
            read_manifest().iter().any(|r| r.target_provider == "opencode"),
            "opencode must be a sync target"
        );
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let rows = read_manifest();
        assert_eq!(rows.len(), after_first, "re-syncing must refresh manifest rows, not add more");

        // One real turn typed in OpenCode into one synced row.
        let row = rows.iter().find(|r| r.target_provider == "opencode").unwrap();
        let oc_id = row.cache.to_string_lossy().to_string();
        let conn = Connection::open(&db).unwrap();
        append_opencode_user_turn(&conn, &oc_id, "msg_zz_new", 1_799_000_000_000, "TYPED IN OPENCODE");
        drop(conn);

        let report = pull_back(false);
        let got: usize = report.pulled.iter().map(|(_, n)| n).sum();
        assert_eq!(got, 1, "one typed turn is one recovered turn: {:?}", report.pulled);
        assert!(
            report.conflicts.is_empty(),
            "one tool's work is never a conflict with itself: {:?}",
            report.conflicts
        );
    }

    /// A manifest already carrying the repeats (written by a build before the
    /// fix above) must not make the very next `pull` report a self-conflict.
    #[test]
    fn test_pull_ignores_repeated_manifest_rows_from_older_builds() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let db = make_opencode_db();
        let registry = opencode_registry(live);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);

        // Re-create the old damage: every row three more times.
        let rows = read_manifest();
        let body: String = (0..4)
            .flat_map(|_| rows.iter())
            .map(|r| serde_json::to_string(r).unwrap() + "\n")
            .collect();
        fs::write(manifest_path(), body).unwrap();
        assert_eq!(read_manifest_raw().len(), rows.len() * 4);

        let row = rows.iter().find(|r| r.target_provider == "opencode").unwrap();
        let conn = Connection::open(&db).unwrap();
        append_opencode_user_turn(
            &conn,
            &row.cache.to_string_lossy(),
            "msg_zz_new",
            1_799_000_000_000,
            "TYPED IN OPENCODE",
        );
        drop(conn);

        let report = pull_back(false);
        let got: usize = report.pulled.iter().map(|(_, n)| n).sum();
        assert_eq!(got, 1, "one typed turn is one recovered turn: {:?}", report.pulled);
        assert!(report.conflicts.is_empty(), "no self-conflict: {:?}", report.conflicts);
        assert_eq!(read_manifest_raw().len(), rows.len(), "and the repeats are gone from disk");
    }

    /// Found live 2026-10-02: every OpenCode manifest row has the database
    /// itself as its `dest`, and loop prevention skipped any source whose path
    /// is a known `dest`. So after the first sync that wrote one row into
    /// OpenCode, *every* session OpenCode owns was treated as agentbridge's
    /// own output, and a new OpenCode session never reached another tool.
    #[test]
    fn test_native_opencode_sessions_still_sync_after_agentbridge_wrote_into_opencode() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let db = make_opencode_db();
        let registry = opencode_registry(live);

        // First sync writes the Claude session into OpenCode.
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let written = crate::opencode_write::count_written(&db);
        assert!(written > 0);

        // The user then starts a session in OpenCode.
        insert_native_opencode_session(&db, "ses_native0001tailAAAA", "/tmp/other", "started in opencode");
        sync_into(&registry, Path::new("/tmp/merge-project"), false);

        assert!(
            read_manifest()
                .iter()
                .any(|r| r.session_id == "ses_native0001tailAAAA" && r.target_provider == "claude-code"),
            "a session created in OpenCode must be materialized into Claude Code"
        );

        // And the rows agentbridge wrote must still never become sources.
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        let rows = read_manifest();
        let ours = crate::opencode_write::written_ids(&db);
        assert!(!ours.is_empty());
        assert!(
            rows.iter().all(|r| !ours.contains(&r.session_id)),
            "agentbridge's own OpenCode rows must not be re-materialized"
        );
        let before = rows.len();
        sync_into(&registry, Path::new("/tmp/merge-project"), false);
        assert_eq!(read_manifest().len(), before, "and the manifest stays the same size");
    }

    /// Found on the operator's real machine 2026-10-02: the shell hook starts
    /// one `sync` per new shell, several at once, and a tool's own startup
    /// shells count. Each run read the manifest before another had written
    /// it, saw that run's copies as ordinary sessions, and copied the copies.
    /// One Claude session ended up in agy four times, twice labeled as if
    /// Codex had made it, and the real manifest reached 123k rows.
    ///
    /// The manifest cannot be the only thing that says "this is a copy". Here
    /// it is deleted outright; nothing may be copied again.
    #[test]
    fn test_copies_are_never_sources_even_when_the_manifest_is_lost() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let db = make_opencode_db();
        insert_native_opencode_session(&db, "ses_native0001tailAAAA", "/tmp/other", "started in opencode");
        let codex = live_root("codex-cli").unwrap();
        fs::create_dir_all(codex.join("sessions")).unwrap();
        let registry = crate::connector::Registry::new(vec![
            Box::new(crate::connectors::claude_code::TestClaudeCode::new(live.clone())),
            Box::new(crate::connectors::codex_cli::CodexCliConnector::new()),
            Box::new(crate::connectors::opencode::OpenCodeConnector::new()),
        ]);
        let count_files = |root: &Path| {
            walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                .count()
        };

        let first = sync_into(&registry, Path::new("/tmp/merge-project"), false);
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        let rows = read_manifest().len();
        let (claude_files, codex_files, oc_rows) = (
            count_files(&live),
            count_files(&codex.join("sessions")),
            crate::opencode_write::count_written(&db),
        );
        assert!(codex_files > 0 && oc_rows > 0, "both stores were written into");

        fs::remove_file(manifest_path()).unwrap();
        let second = sync_into(&registry, Path::new("/tmp/merge-project"), false);
        assert!(second.errors.is_empty(), "{:?}", second.errors);

        assert_eq!(count_files(&live), claude_files, "no copy of a copy in Claude Code");
        assert_eq!(count_files(&codex.join("sessions")), codex_files, "none in Codex");
        assert_eq!(crate::opencode_write::count_written(&db), oc_rows, "none in OpenCode");
        assert_eq!(read_manifest().len(), rows, "the same copies are tracked again, no more");
        for r in read_manifest() {
            let label = crate::label::parse(r.title.as_deref().unwrap_or_default())
                .unwrap_or_else(|| panic!("every copy is labeled: {:?}", r.title));
            assert_eq!(
                label.provider, r.source_provider,
                "a copy names the tool the session really came from: {:?}",
                r.title
            );
            assert!(
                ["claude-code", "opencode"].contains(&label.provider),
                "only the two tools that own a session here may be an origin: {:?}",
                r.title
            );
        }
    }

    /// Found on the operator's real machine 2026-10-02: a Claude session's own
    /// file (39 records, written by Claude Code) was replaced by a 13 line
    /// converted copy titled `codex-cli · …`. A Codex copy of that session
    /// keeps the session's id, so when a racing run took the copy for a Codex
    /// session and wrote it "into Claude Code", the destination was the
    /// original file itself. A tool's own session is never written over,
    /// whatever led there.
    #[test]
    fn test_a_tools_own_session_file_is_never_written_over() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let native = live
            .join(crate::convert::ClaudeCodeConverter::encode_project_dir("/tmp/merge-project"))
            .join(format!("{}.jsonl", NATIVE_UUID));
        let before = fs::read(&native).unwrap();

        // A Codex rollout for the same id and folder that no manifest knows
        // about and that carries no origin record: what an older build's copy
        // looks like to a run that raced it.
        let codex = live_root("codex-cli").unwrap();
        let day = codex.join("sessions/2026/07/01");
        fs::create_dir_all(&day).unwrap();
        fs::write(
            day.join(format!("rollout-2026-07-01T12-00-00-{}.jsonl", NATIVE_UUID)),
            format!(
                "{{\"timestamp\":\"2026-07-01T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"session_id\":\"{id}\",\"timestamp\":\"2026-07-01T12:00:00Z\",\"cwd\":\"/tmp/merge-project\",\"source\":\"cli\"}}}}\n\
                 {{\"timestamp\":\"2026-07-01T12:00:05Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"first question\"}}}}\n",
                id = NATIVE_UUID
            ),
        )
        .unwrap();
        let registry = crate::connector::Registry::new(vec![
            Box::new(crate::connectors::claude_code::TestClaudeCode::new(live.clone())),
            Box::new(crate::connectors::codex_cli::CodexCliConnector::new()),
        ]);

        sync_into(&registry, Path::new("/tmp/merge-project"), false);

        assert_eq!(
            fs::read(&native).unwrap(),
            before,
            "the session file Claude Code owns must be byte for byte untouched"
        );
    }

    /// `unsync --project` takes away what one folder's sync wrote and nothing
    /// else: the other folder's copies, the manifest rows for them, and every
    /// session a tool owns all stay.
    #[test]
    fn test_unsync_for_one_project_leaves_the_rest() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let db = make_opencode_db();
        insert_native_opencode_session(&db, "ses_native0001tailAAAA", "/tmp/other", "started in opencode");
        let registry = opencode_registry(live.clone());

        sync_into(&registry, Path::new("/tmp/project-a"), false);
        sync_into(&registry, Path::new("/tmp/project-b"), false);
        let before = read_manifest();
        let in_a = before.iter().filter(|r| r.project == Path::new("/tmp/project-a")).count();
        assert!(in_a > 0 && in_a < before.len(), "both folders have copies: {in_a} of {}", before.len());

        let report = unsync_matching(
            false,
            &UnsyncFilter { project: Some(PathBuf::from("/tmp/project-a")), sessions: vec![] },
        );
        assert_eq!(report.removed.len(), in_a, "exactly that folder's copies");

        let after = read_manifest();
        assert_eq!(after.len(), before.len() - in_a, "the other rows are kept");
        assert!(after.iter().all(|r| r.project != Path::new("/tmp/project-a")));
        for r in &after {
            if r.target_provider == "claude-code" {
                assert!(r.dest.exists(), "another folder's copy must survive: {}", r.dest.display());
            }
        }
        let conn = Connection::open(&db).unwrap();
        let native: i64 = conn
            .query_row("SELECT COUNT(*) FROM session WHERE id = 'ses_native0001tailAAAA'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(native, 1, "OpenCode's own session is untouched");

        // By session: every copy of one session, wherever it was written.
        let report = unsync_matching(
            false,
            &UnsyncFilter { project: None, sessions: vec![NATIVE_UUID.to_string()] },
        );
        assert!(!report.removed.is_empty());
        assert!(read_manifest().iter().all(|r| r.session_id != NATIVE_UUID));
    }

    /// A sync limited to what changed leaves older sessions alone. This is
    /// what lets an automatic sync run without re-publishing a machine's whole
    /// history every time.
    #[test]
    fn test_sync_since_only_touches_sessions_that_changed() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let db = make_opencode_db();
        let registry = opencode_registry(live);
        let project = Path::new("/tmp/elsewhere");

        // Nothing changed after this moment, so nothing is written.
        let later = chrono::Utc::now() + chrono::Duration::hours(1);
        let report = sync_into_since(&registry, project, false, Some(later));
        assert!(report.created.is_empty(), "an old session is left alone: {:?}", report.created);
        assert_eq!(crate::opencode_write::count_written(&db), 0);

        // The session file was written a moment ago, so a window that starts
        // before that takes it.
        let earlier = chrono::Utc::now() - chrono::Duration::hours(1);
        let report = sync_into_since(&registry, project, false, Some(earlier));
        assert!(!report.created.is_empty(), "a session changed since then is published");
    }

    #[test]
    fn test_sync_state_round_trips() {
        let _sb = Sandbox::new();
        assert!(last_sync_state().is_none(), "no state before the first run");
        let at = chrono::Utc::now();
        record_sync_state(at, "abc123").unwrap();
        let (read_at, fp) = last_sync_state().expect("state is read back");
        assert_eq!(fp, "abc123");
        assert_eq!(read_at.timestamp(), at.timestamp());
    }

    /// Copies written under the old id shape are renamed by the next sync, and
    /// the manifest keeps pointing at them, so `pull` still finds their turns.
    #[test]
    fn test_sync_renames_old_shaped_opencode_rows_and_the_manifest_follows() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let db = make_opencode_db();
        let registry = opencode_registry(live);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);

        // Rewind one row, and its manifest entry, to the old shape.
        let mut rows = read_manifest();
        let i = rows.iter().position(|r| r.target_provider == "opencode").unwrap();
        let good = rows[i].cache.to_string_lossy().to_string();
        let old = "ses_ab0123456789abcdef0123456789abcdef";
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        for sql in [
            "UPDATE session SET id = ?2 WHERE id = ?1",
            "UPDATE message SET session_id = ?2 WHERE session_id = ?1",
            "UPDATE part SET session_id = ?2 WHERE session_id = ?1",
        ] {
            conn.execute(sql, params![good, old]).unwrap();
        }
        drop(conn);
        rows[i].cache = PathBuf::from(old);
        let body: String = rows.iter().map(|r| serde_json::to_string(r).unwrap() + "\n").collect();
        fs::write(manifest_path(), body).unwrap();
        let before = rows.len();

        sync_into(&registry, Path::new("/tmp/merge-project"), false);

        let after = read_manifest();
        assert_eq!(after.len(), before, "renamed, not added again");
        assert!(after.iter().all(|r| r.cache.as_os_str() != old), "no manifest row is left on the old id");
        assert!(after.iter().any(|r| r.cache.as_os_str() == good.as_str()));
        let ids = crate::opencode_write::written_ids(&db);
        assert!(ids.contains(&good) && !ids.contains(&old.to_string()), "{ids:?}");
        assert!(pull_back(false).errors.is_empty());
    }

    /// agentbridge keeps no copy of any session. After a sync the only
    /// converted file is the one in the target tool's store, the folder older
    /// builds filled is gone, and a later sync still refreshes that file.
    #[test]
    fn test_sync_keeps_no_copy_in_agentbridges_own_folder() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        // What an older build left behind.
        let legacy = data_dir().join("cache/claude-code/old");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("leftover.jsonl"), "x").unwrap();

        let report = sync_into(&registry, Path::new("/tmp/some-project"), false);
        let link = first_claude_link(&report);

        assert!(!data_dir().join("cache").exists(), "the old cache folder is removed");
        assert!(!data_dir().join("staging").exists(), "nothing is left staged");
        assert_eq!(fs::metadata(&link.dest).unwrap().nlink(), 1, "the tool's file is the only one");

        // A turn recovered from another tool still reaches that file.
        append_claude_turn(&link.dest, &link.session_id, "ADDED ELSEWHERE");
        pull_back(false);
        sync_into(&registry, Path::new("/tmp/some-project"), false);
        assert!(fs::read_to_string(&link.dest).unwrap().contains("ADDED ELSEWHERE"));
        assert!(!data_dir().join("staging").exists());

        // And unsync still knows the file as its own after the refresh.
        let removed = unsync(false);
        assert!(removed.removed.contains(&link.dest) && removed.kept_foreign.is_empty());
    }

    /// Two syncs at once is the race above. The second one must stand down.
    #[test]
    fn test_a_second_sync_stands_down_while_one_is_running() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        make_opencode_db();
        let registry = opencode_registry(live);

        let held = acquire_run_lock().expect("first run takes the lock");
        let report = sync_into(&registry, Path::new("/tmp/merge-project"), false);
        assert!(report.created.is_empty(), "nothing is written while another run holds the lock");
        assert!(
            report.errors.iter().any(|e| e.contains("another agentbridge")),
            "and it says why: {:?}",
            report.errors
        );
        assert!(read_manifest().is_empty());
        let pulled = pull_back(false);
        assert!(pulled.errors.iter().any(|e| e.contains("another agentbridge")));

        drop(held);
        let report = sync_into(&registry, Path::new("/tmp/merge-project"), false);
        assert!(!report.created.is_empty(), "the lock is released when the run ends");
    }

    /// A run that died leaves its lock behind; it must not block sync forever.
    #[test]
    fn test_a_stale_lock_is_taken_over() {
        let _sb = Sandbox::new();
        fs::create_dir_all(data_dir()).unwrap();
        let lock = data_dir().join("sync.lock");
        fs::write(&lock, "999999").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
        assert!(acquire_run_lock().is_some(), "an hour old lock is stale");
    }

    /// Write-back out of antigravity: turns appended to the materialized
    /// conversation are recovered into the overlay, and a rename is detected.
    #[test]
    fn test_pull_back_recovers_turns_and_renames_from_antigravity() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        make_agy_store();
        let registry = agy_registry(live);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);

        let rec = read_manifest()
            .into_iter()
            .find(|r| r.target_provider == "antigravity")
            .expect("an antigravity row");
        let agy_id = rec.cache.to_string_lossy().to_string();

        // Simulate the user continuing the conversation inside agy, and
        // renaming it, by appending a step and updating the index.
        let before = crate::antigravity_write::load_written(&rec.dest, &agy_id).unwrap();
        append_agy_turn(&rec.dest, before.messages.len(), "a reply typed inside agy");
        rename_agy(&rec.dest, &agy_id, "Renamed inside agy");

        let report = pull_back(false);
        assert!(
            report.pulled.iter().any(|(sid, n)| sid == &rec.session_id && *n >= 1),
            "the appended turn must be recovered: {:?}",
            report.pulled
        );
        let recovered = overlay_messages(&rec.session_id);
        assert!(
            recovered.iter().any(|m| m.text.as_deref() == Some("a reply typed inside agy")),
            "recovered text lands in the overlay"
        );
        assert_eq!(
            overlay_title(&rec.session_id).as_deref(),
            Some("Renamed inside agy"),
            "the rename is recovered too"
        );
    }

    /// An untitled session gets a derived name in agy. Reading that back must
    /// not look like the user renamed it — the false-rename regression this
    /// project hit twice before.
    #[test]
    fn test_antigravity_fallback_title_is_not_a_false_rename() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        // A session with no title at all.
        let encoded = crate::convert::ClaudeCodeConverter::encode_project_dir("/tmp/untitled-proj");
        let dir = live.join(&encoded);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("c1d2e3f4-a5b6-7890-cdef-123456789abc.jsonl"),
            "{\"uuid\":\"c1d2e3f4-a5b6-7890-cdef-123456789abc\",\"type\":\"conversation_start\",\"cwd\":\"/tmp/untitled-proj\",\"timestamp\":\"2026-07-01T12:00:00Z\"}\n             {\"uuid\":\"d2e3f4a5-b6c7-8901-def1-23456789abcd\",\"type\":\"user_message\",\"cwd\":\"/tmp/untitled-proj\",\"timestamp\":\"2026-07-01T12:00:05Z\",\"content\":\"do the thing\"}\n",
        )
        .unwrap();
        make_agy_store();
        let registry = agy_registry(live);
        sync_into(&registry, Path::new("/tmp/untitled-proj"), false);

        let report = pull_back(false);
        assert!(
            report.renamed.is_empty(),
            "a derived title must not read as a rename: {:?}",
            report.renamed
        );
    }

    /// Teardown must remove the bodies *and* their index rows, leaving
    /// conversations agy authored alone.
    #[test]
    fn test_unsync_removes_antigravity_conversations_but_not_agy_own() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        write_native_claude_session(&live, "/tmp/merge-project");
        let agy_home = make_agy_store();
        let registry = agy_registry(live);
        sync_into(&registry, Path::new("/tmp/merge-project"), false);

        let written: Vec<PathBuf> = read_manifest()
            .into_iter()
            .filter(|r| r.target_provider == "antigravity")
            .map(|r| r.dest)
            .collect();
        assert!(!written.is_empty());

        // A conversation agy authored, which must survive.
        let native_body = agy_home
            .join("conversations")
            .join("75ce4071-a2a8-44d0-9958-6720905cc5e4.db");
        fs::write(&native_body, b"agy own").unwrap();

        unsync(false);

        for body in &written {
            assert!(!body.exists(), "{} must be removed", body.display());
        }
        assert!(native_body.exists(), "agy's own conversation is untouched");
        assert_eq!(
            crate::antigravity_write::count_written(&agy_home),
            0,
            "no marked index rows remain"
        );
    }

    /// Append a step to a materialized conversation the way agy would, so the
    /// pull path sees new work. Encodes the same protobuf shape the reader
    /// expects (`.19.2` for user text).
    fn append_agy_turn(body: &Path, idx: usize, text: &str) {
        let conn = rusqlite::Connection::open(body).unwrap();
        let mut payload = Vec::new();
        // .1 step_type = 14 (user)
        payload.extend([0x08, 14]);
        // .4 status = 3
        payload.extend([0x20, 3]);
        // .5.1 created
        let mut created = Vec::new();
        created.extend([0x08]);
        let secs: u64 = 1_785_400_000;
        let mut v = secs;
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            created.push(b);
            if v == 0 {
                break;
            }
        }
        created.extend([0x10, 0]);
        let mut meta = Vec::new();
        meta.push(0x0a);
        meta.push(created.len() as u8);
        meta.extend(&created);
        payload.push(0x2a);
        payload.push(meta.len() as u8);
        payload.extend(&meta);
        // .19 { .2 = text }
        let mut inner = vec![0x12, text.len() as u8];
        inner.extend(text.as_bytes());
        payload.push(0x9a);
        payload.push(0x01);
        payload.push(inner.len() as u8);
        payload.extend(&inner);

        conn.execute(
            "INSERT INTO steps (idx, step_type, status, step_payload, step_format) \
             VALUES (?1, 14, 3, ?2, 0)",
            rusqlite::params![idx as i64, payload],
        )
        .unwrap();
    }

    /// Rename a materialized conversation in agy's index, the way the app would.
    fn rename_agy(body: &Path, id: &str, title: &str) {
        let home = body.parent().unwrap().parent().unwrap();
        let conn =
            rusqlite::Connection::open(crate::antigravity_write::summaries_db(home)).unwrap();
        conn.execute(
            "UPDATE conversation_summaries SET title = ?1 WHERE conversation_id = ?2",
            rusqlite::params![title, id],
        )
        .unwrap();
    }

    #[test]
    fn test_merge_marker_roundtrip() {
        let _sb = Sandbox::new();
        assert!(!is_merge("some-session"));
        set_merge("some-session").unwrap();
        assert!(is_merge("some-session"));
        clear_merge("some-session");
        assert!(!is_merge("some-session"));
    }

    /// Merge-back: a session the user opted into (`resume --merge`) gets the
    /// turns pulled from other tools appended to its own native file during
    /// sync — even though invariant 2 would normally skip it.
    #[test]
    fn test_sync_merges_overlay_into_native_file_when_opted_in() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        let project = PathBuf::from("/tmp/merge-project");
        write_native_claude_session(&live, project.to_str().unwrap());
        let registry = native_registry(live.clone());

        // A turn another tool (e.g. opencode) appended to its copy.
        let overlay_msg = crate::model::Message {
            session_id: NATIVE_UUID.into(),
            ordinal: 0,
            role: crate::model::Role::User,
            timestamp: Some(chrono::DateTime::from_timestamp(1783000000, 0).unwrap()),
            text: Some("answered in opencode".into()),
            tool_name: None,
            tool_input: None,
            tool_result: None,
            parent_ordinal: None,
        };
        append_overlay(NATIVE_UUID, &[overlay_msg]).unwrap();
        set_merge(NATIVE_UUID).unwrap();

        let report = sync_into(&registry, &project, false);

        assert_eq!(report.merged_native, 1, "opted-in native session must be merged back");
        assert_eq!(report.skipped_native, 0);
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);

        let session = crate::connectors::claude_code::load_file(
            &live.join(crate::convert::ClaudeCodeConverter::encode_project_dir(project.to_str().unwrap()))
                .join(format!("{}.jsonl", NATIVE_UUID)),
            NATIVE_UUID,
        )
        .unwrap();
        assert!(
            session.messages.iter().any(|m| m.text.as_deref() == Some("answered in opencode")),
            "native file must now contain the turn pulled from the other tool"
        );
        assert!(
            session.messages.iter().any(|m| m.text.as_deref() == Some("first question")),
            "original turns must be preserved"
        );
    }

    /// Without the opt-in marker, invariant 2 still holds: the native file is
    /// never modified, even when other tools appended turns.
    #[test]
    fn test_sync_leaves_native_file_untouched_without_merge_marker() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        let project = PathBuf::from("/tmp/merge-project");
        write_native_claude_session(&live, project.to_str().unwrap());
        let registry = native_registry(live.clone());
        let native_file = live
            .join(crate::convert::ClaudeCodeConverter::encode_project_dir(project.to_str().unwrap()))
            .join(format!("{}.jsonl", NATIVE_UUID));
        let before = fs::read_to_string(&native_file).unwrap();

        let overlay_msg = crate::model::Message {
            session_id: NATIVE_UUID.into(),
            ordinal: 0,
            role: crate::model::Role::User,
            timestamp: Some(chrono::DateTime::from_timestamp(1783000000, 0).unwrap()),
            text: Some("from another tool".into()),
            tool_name: None,
            tool_input: None,
            tool_result: None,
            parent_ordinal: None,
        };
        append_overlay(NATIVE_UUID, &[overlay_msg]).unwrap();

        let report = sync_into(&registry, &project, false);

        assert_eq!(report.merged_native, 0);
        assert_eq!(report.skipped_native, 1, "without marker the session is still skipped");
        assert_eq!(
            fs::read_to_string(&native_file).unwrap(),
            before,
            "native file must remain byte-identical without the merge marker"
        );
    }

    /// Regression: a Claude-Code-native session synced while standing in a
    /// *different* directory must reach both that directory and `$HOME`
    /// (`target_dirs()`'s documented fallback, HANDOFF.md §6 item 1) — before
    /// `ClaudeCodeConverter::convert_multi` was implemented, the trait's
    /// default silently dropped every directory past the first.
    #[test]
    fn test_sync_materializes_claude_session_into_project_and_home() {
        let _sb = Sandbox::new();
        let live = live_root("claude-code").unwrap();
        let native_project = PathBuf::from("/tmp/merge-project");
        write_native_claude_session(&live, native_project.to_str().unwrap());
        let registry = native_registry(live);

        // Sync while standing somewhere other than the session's own project.
        let elsewhere = PathBuf::from("/tmp/some-other-project");
        let report = sync_into(&registry, &elsewhere, false);

        let claude_links: Vec<&LinkRecord> =
            report.created.iter().filter(|r| r.target_provider == "claude-code").collect();
        assert_eq!(claude_links.len(), 2, "must materialize into both the sync project and $HOME");
        let elsewhere_tag =
            crate::convert::ClaudeCodeConverter::encode_project_dir(elsewhere.to_str().unwrap());
        let home = std::env::var("HOME").unwrap();
        let home_tag = crate::convert::ClaudeCodeConverter::encode_project_dir(&home);
        assert!(
            claude_links.iter().any(|r| r.dest.to_string_lossy().contains(&elsewhere_tag)),
            "missing a copy under the sync project"
        );
        assert!(
            claude_links.iter().any(|r| r.dest.to_string_lossy().contains(&home_tag)),
            "missing a copy under $HOME"
        );

        // The session's own native file must still be untouched (invariant 2)
        // — neither of the two new dests is the native path itself.
        let native_file = live_root("claude-code")
            .unwrap()
            .join(crate::convert::ClaudeCodeConverter::encode_project_dir(native_project.to_str().unwrap()))
            .join(format!("{}.jsonl", NATIVE_UUID));
        assert!(
            claude_links.iter().all(|r| r.dest != native_file),
            "must never write onto the session's own native path"
        );
    }

    #[test]
    fn test_sync_dry_run_writes_nothing() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");

        let report = sync_into(&registry, &project, true);

        assert!(!report.created.is_empty(), "dry run should still plan work");
        assert!(
            !manifest_path().exists(),
            "dry run must not write a manifest"
        );
        assert!(
            !staging_root("claude-code").exists(),
            "dry run must not materialize artifacts"
        );
    }

    #[test]
    fn test_sync_is_idempotent() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");

        let first = sync_into(&registry, &project, false);
        assert!(!first.created.is_empty(), "first sync creates links");

        let second = sync_into(&registry, &project, false);
        assert!(
            second.created.is_empty(),
            "second sync must create nothing (invariant 3), created {}",
            second.created.len()
        );
        assert!(second.unchanged > 0, "second sync should report unchanged");
    }

    /// Regression: sync writes into the tools' stores, so a second pass must
    /// not treat those files as new sessions and materialize them again.
    /// Without this guard the session count multiplies on every run.
    /// Simulate a tool appending a turn to a session agentbridge materialized.
    fn append_claude_turn(dest: &Path, sid: &str, text: &str) {
        let mut body = fs::read_to_string(dest).unwrap();
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&serde_json::json!({
            "parentUuid": serde_json::Value::Null,
            "isSidechain": false,
            "type": "user",
            "uuid": uuid::Uuid::new_v4().to_string(),
            "timestamp": "2026-08-01T00:00:00.000Z",
            "userType": "external",
            "entrypoint": "cli",
            "cwd": "/tmp/some-project",
            "sessionId": sid,
            "version": "2.1.220",
            "gitBranch": "",
            "message": { "role": "user", "content": text },
        }).to_string());
        body.push('\n');
        fs::write(dest, body).unwrap();
    }

    /// Simulate a turn appended in Codex's own rollout format (`event_msg`
    /// carrying a `payload.message` object — the shape `codex_cli.rs` reads
    /// a real turn from).
    fn append_codex_turn(dest: &Path, text: &str) {
        let mut body = fs::read_to_string(dest).unwrap();
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-08-01T00:00:01.000Z",
            "payload": { "message": { "role": "user", "content": text } },
        }).to_string());
        body.push('\n');
        fs::write(dest, body).unwrap();
    }

    /// Mirrors what a real in-session rename (or `-n/--name`) writes: a
    /// dedicated `custom-title` record, not a field on a turn.
    fn rename_claude_session(dest: &Path, sid: &str, title: &str) {
        let mut body = fs::read_to_string(dest).unwrap();
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&serde_json::json!({
            "type": "custom-title",
            "customTitle": title,
            "sessionId": sid,
        }).to_string());
        body.push('\n');
        fs::write(dest, body).unwrap();
    }

    fn first_claude_link(report: &SyncReport) -> LinkRecord {
        report
            .created
            .iter()
            .find(|r| r.target_provider == "claude-code")
            .expect("a claude-code link")
            .clone()
    }

    /// Core write-back: a turn a tool appended must be recovered into the
    /// overlay, since the source file belongs to another tool and is never
    /// modified.
    #[test]
    fn test_pull_back_recovers_turns_a_tool_appended() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");

        let synced = sync_into(&registry, &project, false);
        let link = first_claude_link(&synced);

        // Nothing new yet.
        assert!(pull_back(false).pulled.is_empty(), "no new turns to pull");

        append_claude_turn(&link.dest, &link.session_id, "CONTINUED IN ANOTHER TOOL");

        let report = pull_back(false);
        let got: usize = report.pulled.iter().map(|(_, n)| n).sum();
        assert_eq!(got, 1, "the appended turn must be recovered");

        let overlay = overlay_messages(&link.session_id);
        assert!(
            overlay.iter().any(|m| m.text.as_deref() == Some("CONTINUED IN ANOTHER TOOL")),
            "overlay must hold the new turn"
        );
    }

    /// Found live with real Codex 0.160: resuming a synced session there made
    /// Codex write its own bookkeeping as turns (`<environment_context>…`,
    /// `<skills_instructions>…`). Pull recovered them, and every other tool
    /// then showed them as things you had said. Only real turns travel.
    #[test]
    fn test_pull_back_leaves_a_tools_bookkeeping_turns_behind() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let synced = sync_into(&registry, Path::new("/tmp/some-project"), false);
        let link = first_claude_link(&synced);

        append_claude_turn(
            &link.dest,
            &link.session_id,
            "<environment_context>\n  <cwd>/tmp/some-project</cwd>\n</environment_context>",
        );
        append_claude_turn(&link.dest, &link.session_id, "   ");
        append_claude_turn(&link.dest, &link.session_id, "A REAL QUESTION");

        let report = pull_back(false);
        let got: usize = report.pulled.iter().map(|(_, n)| n).sum();
        assert_eq!(got, 1, "only the real turn is recovered: {:?}", report.pulled);
        let overlay = overlay_messages(&link.session_id);
        assert!(overlay.iter().any(|m| m.text.as_deref() == Some("A REAL QUESTION")));
        assert!(
            !overlay.iter().any(|m| m.text.as_deref().is_some_and(|t| t.contains("environment_context"))),
            "bookkeeping must not reach the overlay"
        );
        // Both were counted as seen, so the next pull has nothing left to do.
        assert!(pull_back(false).pulled.is_empty(), "and it is not offered again");
    }

    /// Pulling twice must not duplicate the same turn.
    #[test]
    fn test_pull_back_is_idempotent() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let link = first_claude_link(&synced);

        append_claude_turn(&link.dest, &link.session_id, "ONLY ONCE");
        pull_back(false);
        let after_first = overlay_messages(&link.session_id).len();

        pull_back(false);
        let after_second = overlay_messages(&link.session_id).len();

        assert_eq!(after_first, after_second, "re-pulling must not duplicate");
    }

    /// The point of write-back: work done in one tool reaches the others.
    #[test]
    fn test_recovered_turn_propagates_to_other_tools() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let link = first_claude_link(&synced);

        append_claude_turn(&link.dest, &link.session_id, "CROSS TOOL TURN");
        pull_back(false);

        // Re-materialize: the Codex copy of the same session must now carry it.
        unsync(false);
        let resynced = sync_into(&registry, &project, false);
        let codex = resynced
            .created
            .iter()
            .find(|r| r.target_provider == "codex-cli" && r.session_id == link.session_id)
            .expect("a codex link for the same session");

        let body = fs::read_to_string(&codex.dest).unwrap();
        assert!(
            body.contains("CROSS TOOL TURN"),
            "a turn added in one tool must appear in the other tool's copy"
        );
    }

    /// A resolver that returns a fixed choice and records every session it
    /// was asked to resolve, so a test can assert both the outcome and that
    /// the operator was (or wasn't) actually asked — without a real terminal.
    struct ScriptedResolver {
        choice: ConflictChoice,
        asked: Vec<(String, Vec<String>)>,
    }

    impl ScriptedResolver {
        fn new(choice: ConflictChoice) -> Self {
            Self { choice, asked: Vec::new() }
        }
    }

    impl ConflictResolver for ScriptedResolver {
        fn resolve(&mut self, session_id: &str, items: &[ConflictItem]) -> ConflictChoice {
            let providers: Vec<String> = items.iter().map(|i| i.provider.clone()).collect();
            self.asked.push((session_id.to_string(), providers));
            self.choice.clone()
        }
    }

    /// New work from exactly one tool must never be reported as a conflict —
    /// this is the ordinary write-back path every earlier pull_back test
    /// already exercises, and it must stay prompt-free.
    #[test]
    fn test_pull_back_single_tool_new_work_is_not_a_conflict() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let link = first_claude_link(&synced);

        append_claude_turn(&link.dest, &link.session_id, "ONLY CLAUDE TOUCHED THIS");

        let report = pull_back(false);
        assert!(
            report.conflicts.is_empty(),
            "a single contributing tool must never be reported as a conflict"
        );
        assert_eq!(report.pulled.iter().map(|(_, n)| n).sum::<usize>(), 1);
    }

    /// Two tools both appending new turns to the same session since the last
    /// pull is exactly the scenario the operator described: continue a
    /// session in Codex after it was worked on in Claude Code. The default
    /// resolver (`AutoMerge`, what plain `pull_back` uses) must keep both —
    /// unchanged behavior from before conflict detection existed — but it
    /// must still surface the conflict so the operator can see it happened.
    #[test]
    fn test_pull_back_two_tools_is_a_conflict_and_auto_merge_keeps_both() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let claude = first_claude_link(&synced);
        let codex = synced
            .created
            .iter()
            .find(|r| r.target_provider == "codex-cli" && r.session_id == claude.session_id)
            .expect("a codex-cli link for the same session")
            .clone();

        append_claude_turn(&claude.dest, &claude.session_id, "FROM CLAUDE CODE");
        append_codex_turn(&codex.dest, "FROM CODEX");

        let report = pull_back(false);

        assert_eq!(report.conflicts.len(), 1, "two contributing tools must be one conflict");
        let (id, providers, choice) = &report.conflicts[0];
        assert_eq!(id, &claude.session_id);
        assert_eq!(
            providers.iter().collect::<std::collections::BTreeSet<_>>(),
            ["claude-code".to_string(), "codex-cli".to_string()].iter().collect()
        );
        assert_eq!(choice, &ConflictChoice::MergeAll);

        let overlay = overlay_messages(&claude.session_id);
        assert!(overlay.iter().any(|m| m.text.as_deref() == Some("FROM CLAUDE CODE")));
        assert!(overlay.iter().any(|m| m.text.as_deref() == Some("FROM CODEX")));
    }

    /// "Continue the written-back session, or only the session from one
    /// agent, skipping the other" — the operator's own words for this
    /// feature. `KeepOnly` must keep exactly the chosen tool's new turns and
    /// discard the other's, permanently (not just this run — re-pulling must
    /// not re-offer the discarded turns).
    #[test]
    fn test_pull_back_keep_only_discards_the_other_tool() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let claude = first_claude_link(&synced);
        let codex = synced
            .created
            .iter()
            .find(|r| r.target_provider == "codex-cli" && r.session_id == claude.session_id)
            .expect("a codex-cli link for the same session")
            .clone();

        append_claude_turn(&claude.dest, &claude.session_id, "KEEP ME");
        append_codex_turn(&codex.dest, "DISCARD ME");

        let mut resolver = ScriptedResolver::new(ConflictChoice::KeepOnly("claude-code".to_string()));
        let report = pull_back_with(false, &mut resolver);

        assert_eq!(resolver.asked.len(), 1, "the resolver must be asked exactly once");

        let overlay = overlay_messages(&claude.session_id);
        assert!(overlay.iter().any(|m| m.text.as_deref() == Some("KEEP ME")));
        assert!(
            !overlay.iter().any(|m| m.text.as_deref() == Some("DISCARD ME")),
            "the non-chosen tool's new turn must not reach the overlay"
        );
        assert_eq!(report.pulled, vec![(claude.session_id.clone(), 1)]);

        // Re-pulling must not re-offer the discarded turn as a conflict — it
        // was a deliberate, permanent choice, not a deferral.
        let mut resolver2 = ScriptedResolver::new(ConflictChoice::MergeAll);
        let second = pull_back_with(false, &mut resolver2);
        assert!(second.conflicts.is_empty(), "the discarded turn must not resurface");
        assert!(second.pulled.is_empty());
    }

    /// `Skip` must decide nothing — the same conflict is offered again next
    /// time, unlike `KeepOnly` which is a permanent choice for that batch of
    /// turns.
    #[test]
    fn test_pull_back_skip_leaves_manifest_untouched_and_reasks() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let claude = first_claude_link(&synced);
        let codex = synced
            .created
            .iter()
            .find(|r| r.target_provider == "codex-cli" && r.session_id == claude.session_id)
            .expect("a codex-cli link for the same session")
            .clone();

        append_claude_turn(&claude.dest, &claude.session_id, "FROM CLAUDE CODE");
        append_codex_turn(&codex.dest, "FROM CODEX");

        let mut resolver = ScriptedResolver::new(ConflictChoice::Skip);
        let first = pull_back_with(false, &mut resolver);
        assert!(first.pulled.is_empty(), "skip must recover nothing this round");
        assert!(overlay_messages(&claude.session_id).is_empty());

        let mut resolver2 = ScriptedResolver::new(ConflictChoice::MergeAll);
        let second = pull_back_with(false, &mut resolver2);
        assert_eq!(
            resolver2.asked.len(),
            1,
            "a skipped conflict must be offered again on the next pull"
        );
        assert_eq!(second.conflicts[0].2, ConflictChoice::MergeAll);
        let overlay = overlay_messages(&claude.session_id);
        assert!(overlay.iter().any(|m| m.text.as_deref() == Some("FROM CLAUDE CODE")));
        assert!(overlay.iter().any(|m| m.text.as_deref() == Some("FROM CODEX")));
    }

    /// A rename made in a non-native copy must be recovered the same way an
    /// appended turn is — this is the fix for "renamed a session in one tool,
    /// the other tool never saw it."
    #[test]
    fn test_pull_back_recovers_a_rename() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");

        let synced = sync_into(&registry, &project, false);
        let link = first_claude_link(&synced);

        assert!(pull_back(false).renamed.is_empty(), "no rename yet");

        rename_claude_session(&link.dest, &link.session_id, "renamed-in-claude-code");

        let report = pull_back(false);
        assert_eq!(
            report.renamed,
            vec![(link.session_id.clone(), "renamed-in-claude-code".to_string())]
        );
        assert_eq!(
            overlay_title(&link.session_id).as_deref(),
            Some("renamed-in-claude-code")
        );

        // Pulling again with no further rename must not report it a second time.
        assert!(pull_back(false).renamed.is_empty(), "re-pulling must not repeat a rename");
    }

    /// The point of write-back for titles: a rename made in one tool reaches
    /// every other tool's copy on the next sync.
    #[test]
    fn test_recovered_rename_propagates_to_other_tools() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let link = first_claude_link(&synced);

        rename_claude_session(&link.dest, &link.session_id, "renamed-in-claude-code");
        pull_back(false);

        unsync(false);
        let resynced = sync_into(&registry, &project, false);
        let codex = resynced
            .created
            .iter()
            .find(|r| r.target_provider == "codex-cli" && r.session_id == link.session_id)
            .expect("a codex link for the same session");

        // Codex keeps title in its `threads` SQLite index, not the rollout
        // file body (that upsert has its own coverage in codex_write.rs) — so
        // the LinkRecord is the observable point here: it carries the title
        // that will be written into that index (`ensure_codex_row`).
        //
        // The written title is the cross-tool label, so the recovered rename
        // is its *name* field rather than the whole string.
        let written = codex.title.as_deref().expect("codex link carries a title");
        let parsed = crate::label::parse(written)
            .unwrap_or_else(|| panic!("codex title must be a label: {:?}", written));
        assert_eq!(parsed.name, "renamed-in-claude-code");
        // And the label still identifies the origin session, which is the
        // point of carrying it into other tools at all.
        assert!(
            link.session_id.starts_with(parsed.id),
            "label id {:?} must prefix the origin session id {:?}",
            parsed.id,
            link.session_id
        );
    }

    /// The overlay is the ONLY durable copy of turns added in a materialized
    /// session — `unsync` deletes the materialized files, so destroying the
    /// overlay with them would lose real work.
    #[test]
    fn test_unsync_never_destroys_recovered_work() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");
        let synced = sync_into(&registry, &project, false);
        let link = first_claude_link(&synced);

        append_claude_turn(&link.dest, &link.session_id, "PRECIOUS WORK");
        pull_back(false);
        assert!(!overlay_messages(&link.session_id).is_empty());

        unsync(false);

        let overlay = overlay_messages(&link.session_id);
        assert!(
            overlay.iter().any(|m| m.text.as_deref() == Some("PRECIOUS WORK")),
            "unsync must not delete recovered turns"
        );
    }
    /// Write-back for the SQLite target: a turn OpenCode appended to a session
    /// agentbridge wrote must be recoverable via `pull_back`.
    #[test]
    fn test_pull_back_recovers_turns_appended_in_opencode() {
        let _sb = Sandbox::new();
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                sandboxes TEXT NOT NULL);
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
                title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                metadata TEXT,
                FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL,
                FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL,
                FOREIGN KEY (message_id) REFERENCES message(id) ON DELETE CASCADE);
            INSERT INTO project VALUES ('global','/',0,0,'[]');
            "#,
        )
        .unwrap();
        drop(conn);

        // Materialize a fixture session into the sandbox database, exactly as
        // `sync` would, and record the manifest row `pull_back` keys on.
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let source = registry
            .by_id("claude-code")
            .expect("claude fixture")
            .load("normal-multi-turn")
            .expect("fixture loads");
        let (oc_id, written, written_title) =
            crate::opencode_write::write_session(&db, &source, "/tmp/some-project").unwrap();
        // Regression: recording anything other than the title actually
        // written (e.g. the source's raw, often-`None` title) here makes the
        // very first `pull_back` below misread OpenCode's own fallback text
        // as a rename nobody made (found live, outside the sandbox,
        // 2026-08-14 — see RowWritten::title).
        append_manifest(&[LinkRecord {
            dest: db.clone(),
            cache: PathBuf::from(&oc_id),
            session_id: source.id.clone(),
            source_provider: "claude-code".into(),
            target_provider: "opencode".into(),
            title: Some(written_title),
            project: PathBuf::from("/tmp/some-project"),
            inode: 0,
            message_count: written,
        }])
        .unwrap();

        let first = pull_back(false);
        assert!(first.pulled.is_empty(), "no new turns yet");
        assert!(
            first.renamed.is_empty(),
            "recording the title actually written must not look like a rename: {:?}",
            first.renamed
        );

        // OpenCode appended a turn of its own: rows addressed with its own
        // id scheme (sorts after agentbridge's `msg_0…`), a fresh timestamp.
        let conn = Connection::open(&db).unwrap();
        let ts: i64 = 1_784_316_034_000 + 5_000;
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES ('msg_f0001', ?1, ?2, ?2, ?3)",
            params![oc_id, ts, serde_json::json!({"role":"user"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES ('prt_f0001', 'msg_f0001', ?1, ?2, ?2, ?3)",
            params![
                oc_id,
                ts,
                serde_json::json!({"type":"text","text":"CONTINUED IN OPENCODE"}).to_string()
            ],
        )
        .unwrap();
        drop(conn);

        let report = pull_back(false);
        let got: usize = report.pulled.iter().map(|(_, n)| n).sum();
        assert_eq!(got, 1, "the opencode-appended turn must be recovered");
        assert!(
            report.renamed.is_empty(),
            "an appended turn is not a rename: {:?}",
            report.renamed
        );

        let overlay = overlay_messages(&source.id);
        assert!(
            overlay
                .iter()
                .any(|m| m.text.as_deref() == Some("CONTINUED IN OPENCODE")),
            "overlay must hold the opencode turn"
        );
    }

    /// The exact bug found live 2026-08-14: an *untitled* session gets some
    /// derived fallback text written into OpenCode (`write_session`'s
    /// `"{provider} session {id}"`), and that fallback is indistinguishable
    /// from a real title once round-tripped through the database. Recording
    /// the source's raw title (`None`) as `LinkRecord.title` instead of what
    /// was actually written made every untitled session look renamed on the
    /// very next pull — at machine scale, hundreds of false positives from a
    /// single `pull` run.
    #[test]
    fn test_untitled_session_fallback_title_is_not_a_false_rename() {
        let _sb = Sandbox::new();
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                sandboxes TEXT NOT NULL);
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
                title TEXT NOT NULL, version TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                metadata TEXT,
                FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE);
            CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL,
                FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE);
            CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                session_id TEXT NOT NULL, time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL, data TEXT NOT NULL,
                FOREIGN KEY (message_id) REFERENCES message(id) ON DELETE CASCADE);
            INSERT INTO project VALUES ('global','/',0,0,'[]');
            "#,
        )
        .unwrap();
        drop(conn);

        let registry = crate::connectors::all_for_testing(&fixture_root());
        let source = registry
            .by_id("claude-code")
            .expect("claude fixture")
            .load("tool-calls-large-output")
            .expect("fixture loads");
        assert!(source.title.is_none(), "fixture must be untitled for this test to mean anything");

        let (oc_id, written, written_title) =
            crate::opencode_write::write_session(&db, &source, "/tmp/some-project").unwrap();
        assert!(!written_title.is_empty(), "write_session must fall back to something");
        append_manifest(&[LinkRecord {
            dest: db.clone(),
            cache: PathBuf::from(&oc_id),
            session_id: source.id.clone(),
            source_provider: "claude-code".into(),
            target_provider: "opencode".into(),
            title: Some(written_title),
            project: PathBuf::from("/tmp/some-project"),
            inode: 0,
            message_count: written,
        }])
        .unwrap();

        let report = pull_back(false);
        assert!(
            report.renamed.is_empty(),
            "the fallback title round-tripping through OpenCode must not look like a rename: {:?}",
            report.renamed
        );
    }

    #[test]
    fn test_sync_does_not_feed_on_its_own_output() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");

        let first = sync_into(&registry, &project, false).created.len();
        assert!(first > 0);

        // Re-discovering now also sees everything sync just wrote.
        let second = sync_into(&registry, &project, false);
        assert!(
            second.created.is_empty(),
            "sync must ignore its own output; created {} on second pass",
            second.created.len()
        );

        let third = sync_into(&registry, &project, false);
        assert!(third.created.is_empty(), "and must stay stable on a third pass");
    }

    #[test]
    fn test_sync_then_unsync_round_trip_removes_everything() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");

        let synced = sync_into(&registry, &project, false);
        let created: Vec<PathBuf> = synced.created.iter().map(|r| r.dest.clone()).collect();
        assert!(!created.is_empty());
        for p in &created {
            assert!(p.exists(), "link should exist after sync: {}", p.display());
        }

        let report = unsync(false);
        assert!(
            !staging_root("claude-code").exists(),
            "unsync must drop the derived cache too"
        );
        assert_eq!(
            report.removed.len(),
            created.len(),
            "unsync must remove exactly what sync created (invariant 4)"
        );
        for p in &created {
            assert!(!p.exists(), "link should be gone: {}", p.display());
        }
        assert!(report.kept_foreign.is_empty());
    }

    /// Two source sessions can carry the same id and both materialize into the
    /// same dest; the manifest must keep only the last row per dest, or the
    /// earlier (stale) count reads as drift on the next pull.
    #[test]
    fn test_manifest_keeps_last_row_per_dest() {
        let _sb = Sandbox::new();
        let dest = PathBuf::from("/tmp/dest.jsonl");
        let stale = LinkRecord {
            dest: dest.clone(),
            cache: PathBuf::from("/tmp/c1"),
            session_id: "s".to_string(),
            source_provider: "claude-code".to_string(),
            target_provider: "claude-code".to_string(),
            project: PathBuf::from("/p"),
            inode: 0,
            message_count: 812,
            title: None,
        };
        let fresh = LinkRecord {
            dest,
            cache: PathBuf::from("/tmp/c2"),
            session_id: "s".to_string(),
            source_provider: "codex-cli".to_string(),
            target_provider: "claude-code".to_string(),
            project: PathBuf::from("/p"),
            inode: 0,
            message_count: 1202,
            title: None,
        };
        append_manifest(&[stale, fresh]).unwrap();

        let rows = read_manifest();
        assert_eq!(rows.len(), 1, "one row per dest");
        assert_eq!(rows[0].message_count, 1202, "must keep the last write");
    }

    /// The live copy shares the cache artifact's inode (hardlink), so a
    /// re-conversion refreshes it even when nothing is linked. The manifest
    /// count must follow, or pull reads agentbridge's own refresh as drift.
    #[test]
    fn test_resync_updates_manifest_count_after_refresh() {
        let _sb = Sandbox::new();
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let project = PathBuf::from("/tmp/some-project");

        let first = sync_into(&registry, &project, false);
        let row = first
            .created
            .iter()
            .find(|r| r.target_provider == "claude-code")
            .expect("claude target must be materialized");
        let before = row.message_count;
        let sid = row.session_id.clone();

        // A turn lands in the overlay (pulled back from another tool).
        let overlay = crate::sync::overlay_path(&sid);
        std::fs::create_dir_all(overlay.parent().unwrap()).unwrap();
        std::fs::write(
            &overlay,
            format!(
                "{}\n",
                serde_json::json!({
                    "session_id": "ses_new",
                    "ordinal": 9999,
                    "role": "user",
                    "timestamp": "2026-07-31T00:00:00Z",
                    "text": "CONTINUED ELSEWHERE",
                })
            ),
        )
        .unwrap();

        // Second pass must refresh the copy (shared inode) and update the row.
        let second = sync_into(&registry, &project, false);
        assert!(second.created.is_empty(), "nothing new linked");

        let rows = read_manifest();
        let row = rows
            .iter()
            .find(|r| r.target_provider == "claude-code" && r.session_id == sid)
            .expect("row must still exist");
        assert_eq!(
            row.message_count,
            before + 1,
            "refresh must advance the manifest count"
        );
    }
}
