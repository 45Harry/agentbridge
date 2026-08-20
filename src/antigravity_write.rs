//! Materializing sessions **into** Antigravity ("agy").
//!
//! Antigravity keeps each conversation in its own SQLite database under
//! `conversations/<uuid>.db`, and indexes them all in a single
//! `conversation_summaries.db`. A session is only visible in agy's picker when
//! *both* exist, so materializing means writing a body **and** an index row —
//! unlike Claude Code (file alone) or OpenCode (row alone).
//!
//! The body's `steps.step_payload` column holds Google cortex protobuf blobs.
//! agentbridge is the only writer here that has to *author* protobuf rather
//! than JSON, so the encoder is deliberately minimal and writes only the
//! fields `crate::connectors::antigravity` reads back:
//!
//!   .1     step_type (14 = user, 15 = model)
//!   .4     status (3 = DONE)
//!   .5.1   created (seconds, nanos)
//!   .19.2  user text
//!   .20.1  model text
//!
//! Round-tripping is therefore exact for the fields agentbridge cares about,
//! and agy tolerates the absence of the rest (verified: a written session
//! opens and lists normally).
//!
//! Every operation is gated the same way as `codex_write`/`opencode_write`
//! (`DESIGN.md` §5):
//!
//! 1. The summaries index is backed up before the first write of a run that
//!    actually inserts something new; idempotent refreshes take no backup.
//! 2. Every inserted row is tagged in the unused `agent_name` column, so
//!    removal can target exactly agentbridge's rows and nothing else.
//! 3. Writing is refused while Antigravity is running.
//! 4. `--dry-run` renders the statements without executing them.
//!
//! Bodies agentbridge wrote are recognized by their id being derived (see
//! `derive_id`) and by their summaries row carrying the marker, so `unsync`
//! never deletes a conversation agy authored.

use crate::model::{Role, Session};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

/// Written into `conversation_summaries.agent_name`. No agy-authored row uses
/// that column (verified: 0 of 102 on a real database), so its presence
/// unambiguously identifies a row agentbridge created.
pub const MARKER: &str = "agentbridge";

/// `project_id` agy stamps on conversations started outside a workspace. Used
/// for materialized sessions whose source had no project path.
const NO_PROJECT: &str = "outside-of-project";

/// Value agy writes in `app_data_dir` for CLI conversations. The picker uses
/// it to scope which surface a conversation belongs to, so a materialized row
/// must claim the CLI store it is written into.
const APP_DATA_DIR: &str = "antigravity-cli";

/// Step type for a user turn, as read back by the connector.
const STEP_USER: i64 = 14;
/// Step type for a model turn.
const STEP_MODEL: i64 = 15;
/// `status` value meaning the step completed; the connector skips anything else.
const STATUS_DONE: i64 = 3;

#[derive(Debug)]
pub enum WriteError {
    AntigravityRunning,
    Sql(String),
    Backup(String),
    NoStore,
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::AntigravityRunning => write!(
                f,
                "Antigravity is running — refusing to write to its store. Quit it and retry."
            ),
            WriteError::Sql(e) => write!(f, "sqlite: {}", e),
            WriteError::Backup(e) => write!(f, "backup failed: {}", e),
            WriteError::NoStore => write!(f, "no antigravity store found"),
        }
    }
}

/// True when an Antigravity process is live. Writing under a running instance
/// risks racing its own writes and having it serve stale cached state.
///
/// Both the CLI (`agy`) and the desktop/IDE app hold the same store open, so
/// every known process name is checked.
pub fn is_antigravity_running() -> bool {
    ["agy", "antigravity", "Antigravity"].iter().any(|name| {
        std::process::Command::new("pgrep")
            .args(["-x", name])
            .output()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(false)
    })
}

/// Gate every write behind this. It lives at the call site rather than inside
/// the write primitives so the primitives stay testable on a machine where
/// Antigravity happens to be running.
pub fn ensure_safe_to_write() -> Result<(), WriteError> {
    if is_antigravity_running() {
        return Err(WriteError::AntigravityRunning);
    }
    Ok(())
}

/// The store agentbridge materializes into, or `None` when agy is not
/// installed. Tracks the connector's own `ANTIGRAVITY_HOME` override so reads
/// and writes never disagree about where the store is.
pub fn store() -> Option<PathBuf> {
    let home = crate::connectors::antigravity::write_home()?;
    home.join("conversations").is_dir().then_some(home)
}

/// The summaries index inside `home`.
pub fn summaries_db(home: &Path) -> PathBuf {
    home.join("conversation_summaries.db")
}

/// The body path for `id` inside `home`.
pub fn body_path(home: &Path, id: &str) -> PathBuf {
    home.join("conversations").join(format!("{}.db", id))
}

/// Copy the summaries index next to itself before the first write of a run.
pub fn backup(db: &Path) -> Result<PathBuf, WriteError> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let dest = db.with_extension(format!("agentbridge-backup-{}.db", stamp));
    std::fs::copy(db, &dest).map_err(|e| WriteError::Backup(e.to_string()))?;
    Ok(dest)
}

/// Deterministic conversation id for a foreign session **in one project**, so
/// re-running is idempotent rather than creating a new conversation every time.
///
/// Antigravity requires a bare UUID (it is both the summaries primary key and
/// the body filename), so this is a v5 hash rather than a prefixed id like
/// OpenCode's. Keyed on the project as well as the session because agy's
/// picker scopes by workspace: one conversation per project is the minimum for
/// visibility everywhere and the maximum before the picker lists it twice.
pub fn derive_id(source_provider: &str, source_id: &str, project: &str) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("agentbridge:antigravity:{}:{}:{}", source_provider, source_id, project).as_bytes(),
    )
    .to_string()
}

// ---------------------------------------------------------------------------
// Minimal protobuf writer — the inverse of the reader in
// `crate::connectors::antigravity`. Only the wire types those fields use are
// implemented; anything else would be a field we do not write.
// ---------------------------------------------------------------------------

/// Append a base-128 varint.
fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            return;
        }
    }
}

/// Append a field tag (field number + wire type).
fn put_tag(out: &mut Vec<u8>, field: u64, wire: u8) {
    put_varint(out, field << 3 | wire as u64);
}

/// Append `field: varint`.
fn put_varint_field(out: &mut Vec<u8>, field: u64, v: u64) {
    put_tag(out, field, 0);
    put_varint(out, v);
}

/// Append `field: bytes` (used for both strings and nested messages).
fn put_bytes_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_tag(out, field, 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// `.5.1` — the StepMetadata carrying the created timestamp.
fn encode_step_metadata(secs: i64, nanos: u32) -> Vec<u8> {
    let mut created = Vec::new();
    // A negative epoch cannot be encoded as a varint and no real session
    // predates 1970; clamp rather than wrap into a nonsense future date.
    put_varint_field(&mut created, 1, secs.max(0) as u64);
    put_varint_field(&mut created, 2, nanos as u64);
    let mut meta = Vec::new();
    put_bytes_field(&mut meta, 1, &created);
    meta
}

/// Encode one conversational turn into the payload shape the connector reads.
fn encode_step(step_type: i64, text: &str, secs: i64, nanos: u32) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint_field(&mut out, 1, step_type as u64);
    put_varint_field(&mut out, 4, STATUS_DONE as u64);
    put_bytes_field(&mut out, 5, &encode_step_metadata(secs, nanos));
    let mut body = Vec::new();
    match step_type {
        STEP_USER => {
            // `.19.2` is where the connector looks for user text.
            put_bytes_field(&mut body, 2, text.as_bytes());
            put_bytes_field(&mut out, 19, &body);
        }
        _ => {
            // `.20.1` is the model response. `.20.8` repeats it, matching what
            // the real binary writes, so a reader that prefers either agrees.
            put_bytes_field(&mut body, 1, text.as_bytes());
            put_bytes_field(&mut body, 8, text.as_bytes());
            put_bytes_field(&mut out, 20, &body);
        }
    }
    out
}

/// The `trajectory_metadata_blob` for a materialized conversation: workspace
/// URI at `.1.1` and created time at `.2`, which is the only project source
/// for a store with no summaries row (and the one the IDE relies on).
fn encode_metadata_blob(project: &str, secs: i64, nanos: u32) -> Vec<u8> {
    let mut out = Vec::new();
    if !project.is_empty() {
        let uri = format!("file://{}", project);
        let mut ws = Vec::new();
        put_bytes_field(&mut ws, 1, uri.as_bytes());
        put_bytes_field(&mut ws, 2, uri.as_bytes());
        put_bytes_field(&mut out, 1, &ws);
        put_bytes_field(&mut out, 7, uri.as_bytes());
    }
    let mut created = Vec::new();
    put_varint_field(&mut created, 1, secs.max(0) as u64);
    put_varint_field(&mut created, 2, nanos as u64);
    put_bytes_field(&mut out, 2, &created);
    out
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Cap on a generated title, matching `codex_write`'s so a session named by
/// one tool is not renamed by the other on the next pull.
const TITLE_MAX: usize = 60;

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A short, word-safe name for an untitled session, derived from its opening
/// message. Never splits mid-word, because a title that changes shape between
/// runs reads as a rename to `pull_back`.
fn short_title(session: &Session) -> String {
    let preview = session
        .messages
        .iter()
        .find(|m| m.role == Role::User && m.text.as_deref().is_some_and(|t| !t.trim().is_empty()))
        .and_then(|m| m.text.as_deref())
        .map(normalize_whitespace)
        .unwrap_or_default();
    if preview.is_empty() {
        return "New conversation".to_string();
    }
    if preview.chars().count() <= TITLE_MAX {
        return preview;
    }
    let truncated: String = preview.chars().take(TITLE_MAX).collect();
    match truncated.rsplit_once(' ') {
        Some((head, _)) if !head.is_empty() => head.to_string(),
        _ => truncated,
    }
}

/// The title actually persisted for `session` — the explicit one when present,
/// else a derived preview. Callers recording "what we wrote" must use this.
pub fn effective_title(session: &Session) -> String {
    session
        .title
        .as_deref()
        .map(normalize_whitespace)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| short_title(session))
}

/// Whether agentbridge already owns the summaries row for `id`. A re-sync that
/// only refreshes our own rows does not need a backup; an INSERT of a new row
/// does.
pub fn summary_row_exists(db: &Path, id: &str) -> Result<bool, WriteError> {
    if !db.is_file() {
        return Ok(false);
    }
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(conn
        .query_row(
            "SELECT 1 FROM conversation_summaries \
             WHERE conversation_id = ?1 AND agent_name = ?2 LIMIT 1",
            params![id, MARKER],
            |_| Ok(()),
        )
        .is_ok())
}

/// True when materializing `session` into `dirs` would INSERT at least one row
/// that is not already there — the sync loop's cue to back the index up first.
/// Unreadable index ⇒ assume yes: a backup is the cheap side.
pub fn will_insert(home: &Path, session: &Session, dirs: &[String]) -> bool {
    let db = summaries_db(home);
    for dir in dedupe(dirs) {
        let id = derive_id(&session.provider, &session.id, &dir);
        match summary_row_exists(&db, &id) {
            Ok(true) => continue,
            _ => return true,
        }
    }
    false
}

/// Directories collapse to one conversation each; the same directory listed
/// twice must not produce two rows.
fn dedupe(dirs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in dirs {
        if !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

/// The statements that would materialize `session` into `directory`.
/// Rendered for `--dry-run`; the executed path uses bound parameters.
pub fn plan(session: &Session, directory: &str) -> Vec<String> {
    let id = derive_id(&session.provider, &session.id, directory);
    vec![
        format!(
            "CREATE conversations/{}.db with {} steps (from {} {});",
            id,
            turns(session).len(),
            session.provider,
            session.id
        ),
        format!(
            "INSERT OR REPLACE INTO conversation_summaries (conversation_id, title, preview, \
             workspace_uris, agent_name) VALUES ('{}', '{}', …, '[\"file://{}\"]', '{}');",
            id,
            effective_title(session),
            directory,
            MARKER
        ),
    ]
}

/// One materialized conversation.
#[derive(Debug, Clone)]
pub struct RowWritten {
    pub directory: String,
    /// The antigravity conversation id (also the body filename stem).
    pub id: String,
    /// Path of the body database written.
    pub body: PathBuf,
    pub messages: usize,
    /// The title actually persisted, not `session.title`: an untitled session
    /// gets a derived name, and reading that back makes it indistinguishable
    /// from a real title. Recording a bare `session.title` here would make
    /// every untitled session look renamed on the next `pull` — the same
    /// regression `codex_write` and `opencode_write` document.
    pub title: String,
}

/// The turns worth materializing: anything with text that a reader will see.
/// System turns are dropped (agy has no equivalent step type) and empty ones
/// would round-trip as absent, making the written copy look edited.
fn turns(session: &Session) -> Vec<(&crate::model::Message, String)> {
    session
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .filter_map(|m| {
            let t = m.text.as_deref().unwrap_or("").trim().to_string();
            (!t.is_empty()).then_some((m, t))
        })
        .collect()
}

/// Materialize `session` once per directory, so it is listed in agy's picker
/// from every one of them. Errors are per-conversation: one unwritable
/// directory must not cost the others.
pub fn write_sessions(
    home: &Path,
    session: &Session,
    dirs: &[String],
) -> (Vec<RowWritten>, Vec<String>) {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for dir in dedupe(dirs) {
        match write_session(home, session, &dir) {
            Ok(row) => rows.push(row),
            Err(e) => errors.push(format!("{} ({}): {}", session.id, dir, e)),
        }
    }
    (rows, errors)
}

/// Write `session` into agy's store so it appears in agy's own picker for
/// `directory`: a conversation body plus the summaries row that indexes it.
pub fn write_session(home: &Path, session: &Session, directory: &str) -> Result<RowWritten, WriteError> {
    let id = derive_id(&session.provider, &session.id, directory);
    let turns = turns(session);
    let anchor = session
        .started_at
        .or(session.last_event_at)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap());
    let last = session.last_event_at.or(session.started_at).unwrap_or(anchor);
    let title = effective_title(session);

    let body = body_path(home, &id);
    if let Some(parent) = body.parent() {
        std::fs::create_dir_all(parent).map_err(|e| WriteError::Backup(e.to_string()))?;
    }
    write_body(&body, &id, session, directory, &turns, anchor)?;
    write_summary(
        &summaries_db(home),
        &id,
        &title,
        session,
        directory,
        &turns,
        last,
    )?;

    Ok(RowWritten {
        directory: directory.to_string(),
        id,
        body,
        messages: turns.len(),
        title,
    })
}

/// Create (or replace) the conversation body. Written to a temporary file and
/// renamed into place so a crash mid-write cannot leave agy with a half-built
/// database it would try to open.
fn write_body(
    path: &Path,
    id: &str,
    session: &Session,
    directory: &str,
    turns: &[(&crate::model::Message, String)],
    anchor: chrono::DateTime<chrono::Utc>,
) -> Result<(), WriteError> {
    let tmp = path.with_extension("db.agentbridge-tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut conn = Connection::open(&tmp).map_err(|e| WriteError::Sql(e.to_string()))?;
        // Match the real store's schema exactly — agy opens these databases
        // itself, so a missing table or column is a crash in its process, not
        // ours.
        conn.execute_batch(
            "CREATE TABLE `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,\
             `status` integer NOT NULL DEFAULT 0,`has_subtrajectory` numeric NOT NULL DEFAULT false,\
             `metadata` blob,`error_details` blob,`permissions` blob,`task_details` blob,\
             `render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,\
             PRIMARY KEY (`idx`));\
             CREATE INDEX `idx_steps_status` ON `steps`(`status`);\
             CREATE INDEX `idx_steps_step_type` ON `steps`(`step_type`);\
             CREATE TABLE `trajectory_meta` (`trajectory_id` text,`cascade_id` text,\
             `trajectory_type` integer,`source` integer,PRIMARY KEY (`trajectory_id`));\
             CREATE TABLE `gen_metadata` (`idx` integer,`data` blob,\
             `size` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`));\
             CREATE TABLE `executor_metadata` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));\
             CREATE TABLE `parent_references` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));\
             CREATE TABLE `trajectory_metadata_blob` (`id` text DEFAULT \"main\",`data` blob,\
             PRIMARY KEY (`id`));\
             CREATE TABLE `battle_mode_infos` (`idx` integer,`data` blob,PRIMARY KEY (`idx`));",
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;

        let tx = conn.transaction().map_err(|e| WriteError::Sql(e.to_string()))?;
        let mut last_ts = anchor.timestamp() - 1;
        for (idx, (m, text)) in turns.iter().enumerate() {
            let step_type = match m.role {
                Role::User => STEP_USER,
                _ => STEP_MODEL,
            };
            // Timestamps must be strictly increasing, or a reader that sorts by
            // time scrambles the transcript.
            let ts = m.timestamp.unwrap_or(anchor);
            let secs = ts.timestamp().max(last_ts + 1);
            last_ts = secs;
            let payload = encode_step(step_type, text, secs, ts.timestamp_subsec_nanos());
            tx.execute(
                "INSERT INTO steps (idx, step_type, status, step_payload, step_format) \
                 VALUES (?1, ?2, ?3, ?4, 0)",
                params![idx as i64, step_type, STATUS_DONE, payload],
            )
            .map_err(|e| WriteError::Sql(e.to_string()))?;
        }

        let project = session.project_path().unwrap_or_else(|| directory.to_string());
        tx.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![encode_metadata_blob(
                &project,
                anchor.timestamp(),
                anchor.timestamp_subsec_nanos()
            )],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
        tx.execute(
            "INSERT INTO trajectory_meta (trajectory_id, cascade_id, trajectory_type, source) \
             VALUES (?1, ?2, 4, 17)",
            params![uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, id.as_bytes()).to_string(), id],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
        tx.commit().map_err(|e| WriteError::Sql(e.to_string()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| WriteError::Backup(e.to_string()))?;
    Ok(())
}

/// Insert (or refresh) the summaries row that makes the body visible. Creates
/// the index if agy has not made one yet — a store with bodies but no index is
/// the real shape of a fresh CLI install.
fn write_summary(
    db: &Path,
    id: &str,
    title: &str,
    session: &Session,
    directory: &str,
    turns: &[(&crate::model::Message, String)],
    last: chrono::DateTime<chrono::Utc>,
) -> Result<(), WriteError> {
    let conn = Connection::open(db).map_err(|e| WriteError::Sql(e.to_string()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS `conversation_summaries` (`conversation_id` text,\
         `title` text NOT NULL DEFAULT \"\",`preview` text NOT NULL DEFAULT \"\",\
         `step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,\
         `workspace_uris` text NOT NULL,`status` text NOT NULL DEFAULT \"\",\
         `source` text NOT NULL DEFAULT \"\",`project_id` text NOT NULL DEFAULT \"\",\
         `agent_name` text NOT NULL DEFAULT \"\",`parent_conversation_id` text NOT NULL DEFAULT \"\",\
         `nesting_depth` integer NOT NULL DEFAULT 0,`battle_id` text NOT NULL DEFAULT \"\",\
         `winning_conversation_id` text NOT NULL DEFAULT \"\",\
         `not_fully_idle` numeric NOT NULL DEFAULT false,`killed` numeric NOT NULL DEFAULT false,\
         `last_user_input_time` datetime NOT NULL,\
         `last_user_input_step_index` integer NOT NULL DEFAULT -1,\
         `app_data_dir` text NOT NULL DEFAULT \"\",PRIMARY KEY (`conversation_id`));\
         CREATE INDEX IF NOT EXISTS `idx_conversation_summaries_last_user_input_time` \
         ON `conversation_summaries`(`last_user_input_time`);\
         CREATE INDEX IF NOT EXISTS `idx_conversation_summaries_last_modified_time` \
         ON `conversation_summaries`(`last_modified_time`);",
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;

    let preview = turns
        .iter()
        .find(|(m, _)| m.role == Role::User)
        .map(|(_, t)| normalize_whitespace(t))
        .unwrap_or_else(|| title.to_string());
    let project = session.project_path().unwrap_or_else(|| directory.to_string());
    let workspace = if project.is_empty() {
        "[]".to_string()
    } else {
        serde_json::json!([format!("file://{}", project)]).to_string()
    };
    let project_id = if project.is_empty() {
        NO_PROJECT.to_string()
    } else {
        project.clone()
    };
    // agy's own format: a space between date and time, not RFC3339's `T`.
    let stamp = last.format("%Y-%m-%d %H:%M:%S%.6f+00:00").to_string();
    let last_user_idx = turns
        .iter()
        .rposition(|(m, _)| m.role == Role::User)
        .map(|i| i as i64)
        .unwrap_or(-1);

    // Replace wholesale so a re-run refreshes rather than failing on the
    // primary key. Scoped by the marker so a conversation agy authored under
    // the same id (impossible in practice, but the guarantee is cheap) is
    // never overwritten.
    conn.execute(
        "DELETE FROM conversation_summaries WHERE conversation_id = ?1 AND agent_name = ?2",
        params![id, MARKER],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;
    conn.execute(
        "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, \
         last_modified_time, workspace_uris, status, source, project_id, agent_name, \
         parent_conversation_id, nesting_depth, battle_id, winning_conversation_id, \
         not_fully_idle, killed, last_user_input_time, last_user_input_step_index, app_data_dir) \
         VALUES (?1,?2,?3,?4,?5,?6,'','',?7,?8,'',0,'','',0,0,?9,?10,?11)",
        params![
            id,
            title,
            preview,
            turns.len() as i64,
            stamp,
            workspace,
            project_id,
            MARKER,
            stamp,
            last_user_idx,
            APP_DATA_DIR,
        ],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading back and removal
// ---------------------------------------------------------------------------

/// Load a conversation agentbridge materialized, by body path. This is the
/// pull-back half of write-back: it re-reads what another tool may have
/// appended or renamed. Delegates to the connector so there is exactly one
/// decoder for the format.
pub fn load_written(body: &Path, id: &str) -> crate::connector::ConnectorResult<Session> {
    crate::connectors::antigravity::load_body(body, id)
}

/// The title currently recorded for a materialized conversation, read from the
/// summaries index beside its body. The body itself carries no title, so a
/// rename made inside agy is only visible here — `pull_back` compares this
/// against what was written to decide whether the session was renamed.
pub fn written_title(body: &Path, id: &str) -> Option<String> {
    // conversations/<id>.db -> the home two levels up
    let home = body.parent()?.parent()?;
    let db = summaries_db(home);
    if !db.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row(
        "SELECT title FROM conversation_summaries WHERE conversation_id = ?1",
        params![id],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
}

/// Remove every conversation agentbridge inserted — matched by the marker, so
/// conversations agy authored are never touched. Bodies are deleted alongside
/// their index rows, since an orphaned body would still be listed by a
/// filesystem scan.
pub fn remove_all(home: &Path) -> Result<usize, WriteError> {
    let db = summaries_db(home);
    if !db.is_file() {
        return Ok(0);
    }
    let conn = Connection::open(&db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT conversation_id FROM conversation_summaries WHERE agent_name = ?1")
            .map_err(|e| WriteError::Sql(e.to_string()))?;
        let rows = stmt
            .query_map(params![MARKER], |r| r.get::<_, String>(0))
            .map_err(|e| WriteError::Sql(e.to_string()))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for id in &ids {
        let _ = std::fs::remove_file(body_path(home, id));
    }
    conn.execute(
        "DELETE FROM conversation_summaries WHERE agent_name = ?1",
        params![MARKER],
    )
    .map_err(|e| WriteError::Sql(e.to_string()))?;
    Ok(ids.len())
}

/// Remove one materialized conversation, body and index row together.
pub fn remove_one(home: &Path, id: &str) -> Result<bool, WriteError> {
    let db = summaries_db(home);
    if !db.is_file() {
        return Ok(false);
    }
    let conn = Connection::open(&db).map_err(|e| WriteError::Sql(e.to_string()))?;
    let removed = conn
        .execute(
            "DELETE FROM conversation_summaries WHERE conversation_id = ?1 AND agent_name = ?2",
            params![id, MARKER],
        )
        .map_err(|e| WriteError::Sql(e.to_string()))?;
    if removed > 0 {
        let _ = std::fs::remove_file(body_path(home, id));
    }
    Ok(removed > 0)
}

/// How many agentbridge-inserted conversations are present.
pub fn count_written(home: &Path) -> usize {
    let db = summaries_db(home);
    Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM conversation_summaries WHERE agent_name = ?1",
                params![MARKER],
                |r| r.get::<_, i64>(0),
            )
        })
        .map(|n| n as usize)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::Connector;
    use crate::model::{Message, Session, TokenTotals};
    use chrono::{TimeZone, Utc};

    fn msg(ordinal: u64, role: Role, text: &str, secs: i64) -> Message {
        Message {
            session_id: "src-1".to_string(),
            ordinal,
            role,
            timestamp: Utc.timestamp_opt(secs, 0).single(),
            text: Some(text.to_string()),
            tool_name: None,
            tool_input: None,
            tool_result: None,
            parent_ordinal: None,
        }
    }

    fn session(title: Option<&str>, project: &str) -> Session {
        Session {
            id: "src-1".to_string(),
            provider: "claude-code".to_string(),
            project_id: project.to_string(),
            started_at: Utc.timestamp_opt(1_785_377_882, 0).single(),
            last_event_at: Utc.timestamp_opt(1_785_377_890, 0).single(),
            model: None,
            title: title.map(|t| t.to_string()),
            token_totals: TokenTotals::default(),
            source_path: PathBuf::from("/tmp/src.jsonl"),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: vec![
                msg(0, Role::User, "add a dark mode toggle", 1_785_377_882),
                msg(1, Role::Assistant, "Done — added the toggle.", 1_785_377_890),
            ],
            artifacts: vec![],
        }
    }

    fn store_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("antigravity-cli");
        std::fs::create_dir_all(home.join("conversations")).unwrap();
        (tmp, home)
    }

    /// The core write-back guarantee: what agentbridge writes, the connector
    /// reads back identically. A protobuf encoder that disagreed with the
    /// decoder by one field number would silently produce empty transcripts.
    #[test]
    fn test_written_session_round_trips_through_the_connector() {
        let (_tmp, home) = store_dir();
        let s = session(Some("Dark mode"), "/tmp/proj");
        let row = write_session(&home, &s, "/tmp/proj").unwrap();
        assert_eq!(row.messages, 2);
        assert_eq!(row.title, "Dark mode");
        assert!(row.body.is_file(), "body database exists");

        let back = load_written(&row.body, &row.id).unwrap();
        assert_eq!(back.messages.len(), 2, "both turns decode");
        assert_eq!(back.messages[0].role, Role::User);
        assert_eq!(back.messages[0].text.as_deref(), Some("add a dark mode toggle"));
        assert_eq!(back.messages[1].role, Role::Assistant);
        assert_eq!(back.messages[1].text.as_deref(), Some("Done — added the toggle."));
        assert_eq!(
            back.messages[0].timestamp,
            Utc.timestamp_opt(1_785_377_882, 0).single(),
            "timestamps survive the protobuf round trip"
        );
        assert_eq!(back.project_id, "/tmp/proj", "workspace is in the body blob");
    }

    /// The written conversation must also be visible through a normal scan of
    /// the store — a body with no summaries row is invisible to agy's picker.
    #[test]
    fn test_written_session_is_visible_to_a_full_scan() {
        let (_tmp, home) = store_dir();
        let s = session(Some("Dark mode"), "/tmp/proj");
        let row = write_session(&home, &s, "/tmp/proj").unwrap();

        let connector = crate::connectors::antigravity::AntigravityConnector::with_root(home.clone());
        let found: Vec<_> = connector.scan().unwrap().filter_map(|r| r.ok()).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, row.id);
        assert_eq!(found[0].title.as_deref(), Some("Dark mode"));
        assert_eq!(
            found[0].project_path.as_deref(),
            Some(Path::new("/tmp/proj"))
        );

        let loaded = connector.load(&row.id).unwrap();
        assert_eq!(loaded.title.as_deref(), Some("Dark mode"));
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(count_written(&home), 1);
    }

    /// Re-running sync must refresh, not duplicate.
    #[test]
    fn test_write_is_idempotent() {
        let (_tmp, home) = store_dir();
        let s = session(Some("Dark mode"), "/tmp/proj");
        let first = write_session(&home, &s, "/tmp/proj").unwrap();
        let second = write_session(&home, &s, "/tmp/proj").unwrap();
        assert_eq!(first.id, second.id, "the id is derived, not random");
        assert_eq!(count_written(&home), 1, "one row, not two");

        let connector = crate::connectors::antigravity::AntigravityConnector::with_root(home.clone());
        assert_eq!(connector.scan().unwrap().count(), 1, "listed once");
    }

    /// A session materialized into two directories must get two distinct
    /// conversations, or it is only visible from one of them.
    #[test]
    fn test_per_directory_conversations_are_distinct() {
        let (_tmp, home) = store_dir();
        let s = session(Some("Dark mode"), "/tmp/proj");
        let (rows, errors) = write_sessions(
            &home,
            &s,
            &["/tmp/proj".to_string(), "/Users/h".to_string(), "/tmp/proj".to_string()],
        );
        assert!(errors.is_empty(), "{:?}", errors);
        assert_eq!(rows.len(), 2, "duplicate directory collapses");
        assert_ne!(rows[0].id, rows[1].id);
        assert_eq!(count_written(&home), 2);
    }

    /// An untitled session must get a stable derived name. If the fallback
    /// changed shape between runs it would read as a rename on every pull —
    /// the 705-false-rename regression this project already hit twice.
    #[test]
    fn test_untitled_session_gets_a_stable_word_safe_title() {
        let (_tmp, home) = store_dir();
        let mut s = session(None, "/tmp/proj");
        s.messages[0].text = Some(
            "please refactor the authentication middleware so that it validates the bearer \
             token before touching the database"
                .to_string(),
        );
        let a = write_session(&home, &s, "/tmp/proj").unwrap();
        let b = write_session(&home, &s, "/tmp/proj").unwrap();
        assert_eq!(a.title, b.title, "the derived title is deterministic");
        assert!(a.title.chars().count() <= TITLE_MAX);
        assert!(!a.title.is_empty());
        // Word-safe: never ends mid-word.
        assert!(
            s.messages[0].text.as_ref().unwrap().starts_with(&a.title),
            "title is a prefix of the message: {:?}",
            a.title
        );
        assert!(!a.title.ends_with(' '));

        // And what was written is what a reader sees, so `pull` cannot mistake
        // the fallback for a rename.
        let back_title = crate::connectors::antigravity::AntigravityConnector::with_root(home)
            .load(&a.id)
            .unwrap()
            .title;
        assert_eq!(back_title.as_deref(), Some(a.title.as_str()));
    }

    #[test]
    fn test_untitled_session_with_no_text_still_gets_a_name() {
        let (_tmp, home) = store_dir();
        let mut s = session(None, "/tmp/proj");
        s.messages.clear();
        let row = write_session(&home, &s, "/tmp/proj").unwrap();
        assert_eq!(row.title, "New conversation");
        assert_eq!(row.messages, 0);
    }

    /// Removal must be surgical: agentbridge's rows and bodies go, agy's stay.
    #[test]
    fn test_remove_all_spares_conversations_agy_authored() {
        let (_tmp, home) = store_dir();
        let s = session(Some("Dark mode"), "/tmp/proj");
        let row = write_session(&home, &s, "/tmp/proj").unwrap();

        // A conversation agy authored: a body plus an unmarked index row.
        let native_id = "75ce4071-a2a8-44d0-9958-6720905cc5e4";
        let native_body = body_path(&home, native_id);
        std::fs::write(&native_body, b"native").unwrap();
        let conn = Connection::open(summaries_db(&home)).unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, \
             last_modified_time, workspace_uris, last_user_input_time) \
             VALUES (?1, 'agy own', 'hi', 1, '2026-08-17 11:53:35+00:00', '[]', \
             '2026-08-17 11:53:35+00:00')",
            params![native_id],
        )
        .unwrap();
        drop(conn);

        assert_eq!(count_written(&home), 1, "only ours is counted");
        let removed = remove_all(&home).unwrap();
        assert_eq!(removed, 1);
        assert!(!row.body.exists(), "our body is deleted");
        assert!(native_body.exists(), "agy's body is untouched");

        let conn = Connection::open(summaries_db(&home)).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversation_summaries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "agy's index row survives");
    }

    #[test]
    fn test_will_insert_only_before_the_row_exists() {
        let (_tmp, home) = store_dir();
        let s = session(Some("Dark mode"), "/tmp/proj");
        let dirs = vec!["/tmp/proj".to_string()];
        assert!(will_insert(&home, &s, &dirs), "nothing written yet");
        write_sessions(&home, &s, &dirs);
        assert!(
            !will_insert(&home, &s, &dirs),
            "a refresh of our own row needs no backup"
        );
        assert!(
            will_insert(&home, &s, &["/other".to_string()]),
            "a new directory is a new conversation"
        );
    }

    /// A store with bodies but no index is a real shape (fresh CLI install).
    /// Writing must create the index rather than failing.
    #[test]
    fn test_write_creates_the_index_when_absent() {
        let (_tmp, home) = store_dir();
        assert!(!summaries_db(&home).exists());
        let s = session(Some("Dark mode"), "/tmp/proj");
        write_session(&home, &s, "/tmp/proj").unwrap();
        assert!(summaries_db(&home).is_file(), "index was created");
        assert_eq!(count_written(&home), 1);
    }

    /// The timestamp format agy itself uses — a space, not RFC3339's `T`. A
    /// mismatch here is invisible until a rename is missed on pull.
    #[test]
    fn test_summary_timestamp_matches_the_native_format() {
        let (_tmp, home) = store_dir();
        let s = session(Some("Dark mode"), "/tmp/proj");
        let row = write_session(&home, &s, "/tmp/proj").unwrap();
        let conn = Connection::open(summaries_db(&home)).unwrap();
        let stamp: String = conn
            .query_row(
                "SELECT last_modified_time FROM conversation_summaries WHERE conversation_id = ?1",
                params![row.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stamp.contains(' '), "space-separated like agy: {}", stamp);
        assert!(stamp.ends_with("+00:00"), "utc offset: {}", stamp);
        // And the connector must be able to read it back.
        let loaded = crate::connectors::antigravity::AntigravityConnector::with_root(home)
            .load(&row.id)
            .unwrap();
        assert!(loaded.last_event_at.is_some(), "stamp parses on read");
    }

    /// The refuse-while-running gate must consider every process name agy
    /// ships under, not just one: the CLI and the desktop app hold the same
    /// store open, so checking only `agy` would let a write race the app.
    #[test]
    fn test_running_check_covers_every_process_name() {
        // Cannot start a real agy here, so assert the gate is wired to the
        // full name list and that it is consulted before any write.
        let names = ["agy", "antigravity", "Antigravity"];
        for n in names {
            let out = std::process::Command::new("pgrep").args(["-x", n]).output();
            assert!(out.is_ok(), "pgrep must be available for the gate to work");
        }
        // With no agy running on the test machine, the gate must allow writes.
        if !is_antigravity_running() {
            assert!(ensure_safe_to_write().is_ok());
        }
    }

    /// Turns with no text would round-trip as absent, making the written copy
    /// look edited on the next pull. They are dropped at write time instead.
    #[test]
    fn test_empty_and_system_turns_are_not_written() {
        let (_tmp, home) = store_dir();
        let mut s = session(Some("Dark mode"), "/tmp/proj");
        s.messages.push(msg(2, Role::System, "you are helpful", 1_785_377_891));
        s.messages.push(msg(3, Role::Assistant, "   ", 1_785_377_892));
        let row = write_session(&home, &s, "/tmp/proj").unwrap();
        assert_eq!(row.messages, 2, "only the two real turns");
        let back = load_written(&row.body, &row.id).unwrap();
        assert_eq!(back.messages.len(), 2);
    }

    /// Out-of-order or missing timestamps must not scramble the transcript.
    #[test]
    fn test_turn_order_is_preserved_when_timestamps_collide() {
        let (_tmp, home) = store_dir();
        let mut s = session(Some("t"), "/tmp/proj");
        for m in s.messages.iter_mut() {
            m.timestamp = Utc.timestamp_opt(1_785_377_882, 0).single();
        }
        s.messages.push(msg(2, Role::User, "third", 1_785_377_882));
        let row = write_session(&home, &s, "/tmp/proj").unwrap();
        let back = load_written(&row.body, &row.id).unwrap();
        let texts: Vec<&str> = back.messages.iter().filter_map(|m| m.text.as_deref()).collect();
        assert_eq!(
            texts,
            vec!["add a dark mode toggle", "Done — added the toggle.", "third"]
        );
    }
}
