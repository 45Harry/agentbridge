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
}

#[derive(Debug, Default)]
pub struct PullReport {
    /// (session id, number of new messages recovered)
    pub pulled: Vec<(String, usize)>,
    pub errors: Vec<String>,
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
        "codex-cli" => crate::connectors::codex_cli::load_file(path, id).ok(),
        "opencode" => crate::connectors::opencode::load_from_db(path, id).ok(),
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

pub fn status() -> Vec<StatusRow> {
    read_manifest()
        .into_iter()
        .map(|r| {
            let exists = r.dest.exists();
            let actual = if exists {
                let id = if r.target_provider == "opencode" {
                    r.cache.to_string_lossy().to_string()
                } else {
                    r.session_id.clone()
                };
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
/// every other tool can see them (DESIGN.md §6).
///
/// Reads only files agentbridge itself created; source sessions are untouched.
pub fn pull_back(dry_run: bool) -> PullReport {
    let mut report = PullReport::default();
    let mut manifest = read_manifest();
    let mut changed = false;

    for rec in manifest.iter_mut() {
        if !rec.dest.exists() {
            continue;
        }
        // OpenCode rows are addressed by the ses_... id agentbridge created;
        // for file targets the source session id names the materialized file.
        let id = if rec.target_provider == "opencode" {
            rec.cache.to_string_lossy().to_string()
        } else {
            rec.session_id.clone()
        };
        let Some(session) = load_materialized(&rec.target_provider, &rec.dest, &id)
        else {
            continue;
        };

        // Anything past what we wrote is the tool's own new work.
        if session.messages.len() <= rec.message_count {
            continue;
        }
        let new = &session.messages[rec.message_count..];

        if dry_run {
            report.pulled.push((rec.session_id.clone(), new.len()));
            continue;
        }

        match append_overlay(&rec.session_id, new) {
            Ok(0) => {}
            Ok(n) => {
                report.pulled.push((rec.session_id.clone(), n));
                rec.message_count = session.messages.len();
                changed = true;
            }
            Err(e) => report
                .errors
                .push(format!("overlay {}: {}", rec.session_id, e)),
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

/// agentbridge's own data dir. Overridable for tests and for operators who
/// keep state elsewhere.
pub fn data_dir() -> PathBuf {
    std::env::var("AGENTBRIDGE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".agentbridge"))
}

fn cache_root(target: &str) -> PathBuf {
    data_dir().join("cache").join(target)
}

fn manifest_path() -> PathBuf {
    data_dir().join("manifest.jsonl")
}

/// Where a target tool keeps its sessions.
fn live_root(target: &str) -> Option<PathBuf> {
    match target {
        "claude-code" => Some(home().join(".claude").join("projects")),
        "codex-cli" => Some(home().join(".codex")),
        // OpenCode stores rows in SQLite, not files — it cannot be hardlinked
        // into and is handled separately (DESIGN.md §5).
        _ => None,
    }
}

/// OpenCode's database, when present.
fn opencode_db() -> Option<PathBuf> {
    let p = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
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

pub fn read_manifest() -> Vec<LinkRecord> {
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

/// Make every session on the machine visible in `project` for every detected
/// file-based tool.
///
/// `dry_run` plans without touching the filesystem.
pub fn sync_into(registry: &Registry, project: &Path, dry_run: bool) -> SyncReport {
    let index: Index = discover(registry);
    let mut report = SyncReport::default();

    // OpenCode has no live_root (rows, not files) but is still a valid target.
    let targets: Vec<String> = registry
        .detected()
        .map(|c| c.id().to_string())
        .filter(|id| live_root(id).is_some() || (id == "opencode" && opencode_db().is_some()))
        .collect();

    // Back up OpenCode's database once per run, before any INSERT.
    let mut opencode_backed_up = false;
    // Same for Codex's index (`state_5.sqlite`).
    let mut codex_backed_up = false;

    let mut manifest = read_manifest();
    let mut manifest_dirty = false;
    let already: Vec<(String, PathBuf)> = manifest
        .iter()
        .map(|r| (r.session_id.clone(), r.dest.clone()))
        .collect();

    // Loop prevention (DESIGN.md §6). Sync writes into the tools' own stores,
    // so the next discovery pass sees those files as ordinary sessions. Left
    // unguarded they get re-materialized on every run and the session count
    // multiplies. A file agentbridge created is never a source.
    let generated: std::collections::HashSet<PathBuf> =
        manifest.iter().map(|r| r.dest.clone()).collect();

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
                if generated.contains(&kept.source_path) && !generated.contains(&e.source_path) {
                    entries[i] = e;
                }
            }
        }
    }

    for entry in entries {
        if generated.contains(&entry.source_path) {
            continue;
        }
        for target in &targets {
            // A session already native to this tool *in this directory* is
            // left strictly alone (invariant 2).
            if &entry.provider == target && entry.project_path.as_deref() == Some(project) {
                report.skipped_native += 1;
                continue;
            }

            let is_opencode = target == "opencode";
            let is_codex = target == "codex-cli";
            if !is_opencode && (live_root(target).is_none() || converter_for(target).is_none()) {
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
            let overlay = overlay_messages(&entry.id);
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

            if dry_run {
                report.created.push(LinkRecord {
                    dest: if is_opencode {
                        opencode_db().unwrap_or_default()
                    } else {
                        live_root(target).unwrap_or_default().join("(planned)")
                    },
                    cache: cache_root(target).join("(planned)"),
                    session_id: entry.id.clone(),
                    source_provider: entry.provider.clone(),
                    target_provider: target.clone(),
                    project: project.to_path_buf(),
                    inode: 0,
                    message_count: session.messages.len(),
                });
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
                    report.created.push(LinkRecord {
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
                    });
                }
                continue;
            }

            // Rule 2: derive once into the cache.
            let cache = cache_root(target);
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
                    // The artifact was just re-converted and the live copy shares
                    // its inode (hardlink), so the copy *is* refreshed even though
                    // nothing is linked. The manifest count must follow, or the
                    // next pull reads agentbridge's own refresh as tool drift.
                    if let Some(row) = manifest
                        .iter_mut()
                        .find(|r| r.session_id == entry.id && r.dest == dest)
                    {
                        row.message_count = session.messages.len();
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

                // Rule 3: presence by hardlink, not copy.
                match link_or_copy(artifact, &dest) {
                    Ok(inode) => {
                        if is_codex {
                            ensure_codex_row(&mut report, &mut codex_backed_up, &session, &dest, std::slice::from_ref(dir));
                        }
                        report.created.push(LinkRecord {
                            dest,
                            cache: artifact.clone(),
                            session_id: entry.id.clone(),
                            source_provider: entry.provider.clone(),
                            target_provider: target.clone(),
                            project: project.to_path_buf(),
                            inode,
                            message_count: session.messages.len(),
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
fn ensure_codex_row(
    report: &mut SyncReport,
    backed_up: &mut bool,
    session: &crate::model::Session,
    rollout_path: &Path,
    dirs: &[String],
) {
    let Some(db) = crate::codex_write::state_db() else {
        return;
    };
    if let Err(e) = crate::codex_write::ensure_safe_to_write() {
        report.errors.push(e.to_string());
        return;
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
                    return;
                }
            }
        }
    }
    match crate::codex_write::ensure_thread_rows(&db, session, rollout_path, dirs) {
        Ok(r) => report.codex_indexed += r.inserted,
        Err(e) => report.errors.push(e.to_string()),
    }
}

/// Remove exactly the files agentbridge created. A destination whose inode
/// no longer matches the manifest belongs to something else now and is kept.
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

    for r in &records {
        if r.target_provider == "opencode" {
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
        };
        append_manifest(&[rec]).unwrap();

        let report = unsync(false);

        assert!(foreign.exists(), "must not delete a file it did not create");
        assert_eq!(report.removed.len(), 0);
        assert_eq!(report.kept_foreign.len(), 1);
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
            !cache_root("claude-code").exists(),
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
        let (oc_id, written) =
            crate::opencode_write::write_session(&db, &source, "/tmp/some-project").unwrap();
        append_manifest(&[LinkRecord {
            dest: db.clone(),
            cache: PathBuf::from(&oc_id),
            session_id: source.id.clone(),
            source_provider: "claude-code".into(),
            target_provider: "opencode".into(),
            project: PathBuf::from("/tmp/some-project"),
            inode: 0,
            message_count: written,
        }])
        .unwrap();

        assert!(pull_back(false).pulled.is_empty(), "no new turns yet");

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

        let overlay = overlay_messages(&source.id);
        assert!(
            overlay
                .iter()
                .any(|m| m.text.as_deref() == Some("CONTINUED IN OPENCODE")),
            "overlay must hold the opencode turn"
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
            !cache_root("claude-code").exists(),
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
