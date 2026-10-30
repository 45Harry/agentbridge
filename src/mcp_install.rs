//! `agentbridge mcp install` / `uninstall` (spec 0001, steps 6 to 8).
//!
//! Adds one `agentbridge` entry to each detected tool's MCP settings file, so
//! the tool starts `agentbridge mcp` as a child process. Every other byte of
//! the file is kept: comments, spacing and key order, in the three JSON style
//! files (`jsonc-parser`'s CST) and in Codex's TOML (`toml_edit`).
//!
//! Gates, the same as every other writer into a tool's files:
//! - a tool that is open is skipped and named, so install can be run again;
//! - each file is backed up next to itself before it is changed;
//! - the edit is checked (the file is read again just before writing and the
//!   tool is skipped if it changed), written to a temp file in the same folder
//!   and renamed over the target; a symlinked file stays a symlink;
//! - every entry added gets a manifest row of kind `mcp-config`, written only
//!   after the edit succeeds; nothing without a row is ever removed;
//! - `--dry-run` writes nothing.
//!
//! Before any entry is added, install moves the machine off copy based sync:
//! `pull` (stopping if a conflict needs a decision), then the shell hook and
//! every session copy are removed. Recovered turns in the overlay stay.

use crate::connector::Registry;
use crate::sync::{LinkRecord, KIND_MCP_CONFIG};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// The key agentbridge's entry lives under in every settings file.
pub const ENTRY_KEY: &str = "agentbridge";

/// Tools install knows, in the order it reports them.
pub const TOOLS: [&str; 4] = ["claude-code", "codex-cli", "opencode", "antigravity"];

/// Is this tool running? Injected so tests never depend on what happens to be
/// open on the machine running them.
pub type OpenCheck = dyn Fn(&str) -> bool;

/// The real check: a process of that tool is running.
pub fn tool_is_open(tool: &str) -> bool {
    if cfg!(test) {
        return false;
    }
    match tool {
        "claude-code" => process_running("claude"),
        "codex-cli" => crate::codex_write::is_codex_running(),
        "opencode" => crate::opencode_write::is_opencode_running(),
        "antigravity" => crate::antigravity_write::is_antigravity_running(),
        _ => false,
    }
}

fn process_running(name: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", name])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

pub fn display_name(tool: &str) -> &'static str {
    match tool {
        "claude-code" => "Claude Code",
        "codex-cli" => "Codex",
        "opencode" => "OpenCode",
        "antigravity" => "Antigravity",
        _ => "unknown tool",
    }
}

// ---------------------------------------------------------------------------
// Where each tool keeps its MCP settings

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var(var).ok().filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The settings file install edits for `tool`. Env overrides are read on
/// every call.
pub fn settings_path(tool: &str) -> PathBuf {
    match tool {
        "claude-code" => env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(home).join(".claude.json"),
        "codex-cli" => env_dir("CODEX_HOME").unwrap_or_else(|| home().join(".codex")).join("config.toml"),
        "opencode" => {
            let dir = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config")).join("opencode");
            let jsonc = dir.join("opencode.jsonc");
            if jsonc.exists() || fs::symlink_metadata(&jsonc).is_ok() { jsonc } else { dir.join("opencode.json") }
        }
        // What agy reads (seen in agy 1.2.17: its binary names
        // `~/.gemini/config/mcp_config.json`, and a live run started the
        // server listed there). The IDE's `~/.gemini/antigravity/
        // mcp_config.json` is a symlink to it on a default install.
        // `ANTIGRAVITY_HOME` names the CLI's data folder, not this one.
        "antigravity" => home().join(".gemini/config/mcp_config.json"),
        _ => PathBuf::new(),
    }
}

/// The value agentbridge writes under its key for `tool`, as JSON (Codex's
/// TOML table is stored as its JSON equivalent).
pub fn entry_for(tool: &str, exe: &Path) -> Value {
    let cmd = exe.to_string_lossy().to_string();
    let mut entry = match tool {
        "claude-code" => json!({ "type": "stdio", "command": cmd, "args": ["mcp"] }),
        "codex-cli" => json!({ "command": cmd, "args": ["mcp"] }),
        "opencode" => json!({ "type": "local", "command": [cmd, "mcp"], "enabled": true }),
        _ => json!({ "command": cmd, "args": ["mcp"] }),
    };
    // A tool may start its MCP servers with a trimmed environment (Codex
    // does: seen live, `CLAUDE_CONFIG_DIR` did not reach the server and every
    // Claude Code session vanished from Codex). The store locations set when
    // install runs are written into the entry, so the server sees the same
    // stores whichever tool starts it.
    let env = store_env();
    if !env.is_empty() {
        let key = if tool == "opencode" { "environment" } else { "env" };
        entry[key] = Value::Object(env);
    }
    entry
}

/// The variables that move a tool's store, as set now.
pub const STORE_VARS: [&str; 5] = ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "XDG_DATA_HOME", "ANTIGRAVITY_HOME", "AGENTBRIDGE_DATA_DIR"];

fn store_env() -> serde_json::Map<String, Value> {
    STORE_VARS
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), Value::String(v))))
        .collect()
}

/// How a settings file is edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// JSON or JSONC; the entry goes under this top level object.
    Json(&'static str),
    /// Codex's TOML; the entry is `[mcp_servers.agentbridge]`.
    Toml,
}

fn format_of(tool: &str) -> Format {
    match tool {
        "codex-cli" => Format::Toml,
        "opencode" => Format::Json("mcp"),
        _ => Format::Json("mcpServers"),
    }
}

const TOML_PARENT: &str = "mcp_servers";

// ---------------------------------------------------------------------------
// Editors: pure text in, text out

fn parse_jsonc(text: &str) -> Result<jsonc_parser::cst::CstRootNode, String> {
    jsonc_parser::cst::CstRootNode::parse(text, &jsonc_parser::ParseOptions::default())
        .map_err(|e| format!("not valid JSON: {}", e))
}

fn to_cst(v: &Value) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue as C;
    match v {
        Value::Null => C::Null,
        Value::Bool(b) => C::Bool(*b),
        Value::Number(n) => C::Number(n.to_string()),
        Value::String(s) => C::String(s.clone()),
        Value::Array(a) => C::Array(a.iter().map(to_cst).collect()),
        Value::Object(o) => C::Object(o.iter().map(|(k, v)| (k.clone(), to_cst(v))).collect()),
    }
}

fn toml_to_json_value(v: &toml_edit::Value) -> Value {
    use toml_edit::Value as T;
    match v {
        T::String(s) => json!(s.value()),
        T::Integer(i) => json!(i.value()),
        T::Float(f) => json!(f.value()),
        T::Boolean(b) => json!(b.value()),
        T::Datetime(d) => json!(d.value().to_string()),
        T::Array(a) => Value::Array(a.iter().map(toml_to_json_value).collect()),
        T::InlineTable(t) => Value::Object(t.iter().map(|(k, v)| (k.to_string(), toml_to_json_value(v))).collect()),
    }
}

fn toml_to_json(item: &toml_edit::Item) -> Option<Value> {
    match item {
        toml_edit::Item::None => None,
        toml_edit::Item::Value(v) => Some(toml_to_json_value(v)),
        toml_edit::Item::Table(t) => Some(Value::Object(
            t.iter().filter_map(|(k, v)| Some((k.to_string(), toml_to_json(v)?))).collect(),
        )),
        toml_edit::Item::ArrayOfTables(a) => Some(Value::Array(
            a.iter()
                .map(|t| Value::Object(t.iter().filter_map(|(k, v)| Some((k.to_string(), toml_to_json(v)?))).collect()))
                .collect(),
        )),
    }
}

/// The value currently under agentbridge's key, if any.
fn current_entry(format: Format, text: &str) -> Result<Option<Value>, String> {
    match format {
        Format::Json(parent) => {
            if text.trim().is_empty() {
                return Ok(None);
            }
            let root = parse_jsonc(text)?;
            let Some(obj) = root.object_value() else {
                return Err("the file is not a JSON object".to_string());
            };
            Ok(obj
                .get(parent)
                .and_then(|p| p.object_value())
                .and_then(|o| o.get(ENTRY_KEY))
                .and_then(|e| e.value())
                .and_then(|v| v.to_serde_value()))
        }
        Format::Toml => {
            let doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("not valid TOML: {}", e))?;
            Ok(doc.get(TOML_PARENT).and_then(|p| p.get(ENTRY_KEY)).and_then(toml_to_json))
        }
    }
}

/// `text` with agentbridge's entry set to `value`, and whether the parent
/// object had to be created.
fn with_entry(format: Format, text: &str, value: &Value) -> Result<(String, bool), String> {
    match format {
        Format::Json(parent) => {
            let root = parse_jsonc(if text.trim().is_empty() { "{}" } else { text })?;
            let obj = root.object_value().ok_or("the file is not a JSON object")?;
            let created_parent = obj.get(parent).is_none();
            let parent_obj = match obj.get(parent) {
                Some(p) => p.object_value().ok_or_else(|| format!("`{}` is not an object; add the entry by hand", parent))?,
                None => obj.object_value_or_set(parent),
            };
            match parent_obj.get(ENTRY_KEY) {
                Some(prop) => prop.set_value(to_cst(value)),
                None => {
                    parent_obj.append(ENTRY_KEY, to_cst(value));
                }
            }
            let mut out = root.to_string();
            if text.trim().is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            Ok((out, created_parent))
        }
        Format::Toml => {
            let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("not valid TOML: {}", e))?;
            let created_parent = !doc.contains_key(TOML_PARENT);
            if created_parent {
                let mut t = toml_edit::Table::new();
                // Only `[mcp_servers.agentbridge]` is written; no bare
                // `[mcp_servers]` header.
                t.set_implicit(true);
                doc.insert(TOML_PARENT, toml_edit::Item::Table(t));
            }
            let servers = doc[TOML_PARENT]
                .as_table_mut()
                .ok_or("`mcp_servers` is not a table; add the entry by hand")?;
            let mut t = toml_edit::Table::new();
            let obj = value.as_object().ok_or("entry must be an object")?;
            // `command` before `args`, as Codex's own docs write it; JSON
            // objects here sort their keys.
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort_by_key(|k| (k.as_str() != "command", k.as_str() != "args", k.as_str()));
            for (k, v) in keys.into_iter().map(|k| (k, &obj[k])) {
                let item = match v {
                    Value::String(s) => toml_edit::value(s.as_str()),
                    Value::Bool(b) => toml_edit::value(*b),
                    Value::Array(a) => {
                        let mut arr = toml_edit::Array::new();
                        for x in a {
                            arr.push(x.as_str().unwrap_or_default());
                        }
                        toml_edit::value(arr)
                    }
                    Value::Object(o) => {
                        let mut t = toml_edit::InlineTable::new();
                        for (ek, ev) in o {
                            t.insert(ek, ev.as_str().unwrap_or_default().into());
                        }
                        toml_edit::value(t)
                    }
                    _ => return Err(format!("cannot write `{}` to TOML", k)),
                };
                t.insert(k, item);
            }
            servers.insert(ENTRY_KEY, toml_edit::Item::Table(t));
            Ok((doc.to_string(), created_parent))
        }
    }
}

/// `text` without agentbridge's entry (and without the parent object when
/// `drop_empty_parent` and nothing else is in it), plus whether the document
/// holds nothing at all afterwards.
fn without_entry(format: Format, text: &str, drop_empty_parent: bool) -> Result<(String, bool), String> {
    match format {
        Format::Json(parent) => {
            let root = parse_jsonc(text)?;
            let obj = root.object_value().ok_or("the file is not a JSON object")?;
            if let Some(parent_prop) = obj.get(parent) {
                if let Some(parent_obj) = parent_prop.object_value()
                    && let Some(prop) = parent_obj.get(ENTRY_KEY)
                {
                    prop.remove();
                    if drop_empty_parent && parent_obj.properties().is_empty() {
                        parent_prop.remove();
                    }
                }
            }
            let empty = obj.properties().is_empty();
            Ok((root.to_string(), empty))
        }
        Format::Toml => {
            let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("not valid TOML: {}", e))?;
            let mut parent_empty = false;
            if let Some(servers) = doc.get_mut(TOML_PARENT).and_then(|p| p.as_table_like_mut()) {
                servers.remove(ENTRY_KEY);
                parent_empty = servers.is_empty();
            }
            if drop_empty_parent && parent_empty {
                doc.remove(TOML_PARENT);
            }
            let empty = doc.is_empty();
            Ok((doc.to_string(), empty))
        }
    }
}

/// How an entry is shown in `--dry-run`.
fn render_entry(tool: &str, value: &Value) -> String {
    match format_of(tool) {
        Format::Toml => with_entry(Format::Toml, "", value).map(|(t, _)| t.trim_end().to_string()).unwrap_or_default(),
        Format::Json(parent) => format!("\"{}\": {{ \"{}\": {} }}", parent, ENTRY_KEY, value),
    }
}

// ---------------------------------------------------------------------------
// Files

/// A settings file, with any symlink resolved.
struct Resolved {
    /// The path the tool reads.
    link: PathBuf,
    /// The file edits land on (the end of the symlink chain).
    target: PathBuf,
    exists: bool,
}

fn resolve(path: &Path) -> Result<Resolved, String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => match fs::canonicalize(path) {
            Ok(target) => Ok(Resolved { link: path.to_path_buf(), exists: target.is_file(), target }),
            Err(_) => Err(format!(
                "{} is a symlink to a file that does not exist; skipped (create its target, then run install again)",
                path.display()
            )),
        },
        Ok(m) if m.is_file() => Ok(Resolved { link: path.to_path_buf(), target: path.to_path_buf(), exists: true }),
        Ok(_) => Err(format!("{} is not a file; skipped", path.display())),
        Err(_) => Ok(Resolved { link: path.to_path_buf(), target: path.to_path_buf(), exists: false }),
    }
}

/// Copy a settings file next to itself before agentbridge changes it:
/// `<name>.agentbridge-backup-<UTC stamp>.<ext>`. Backups are never deleted
/// by agentbridge.
pub fn backup_settings(path: &Path) -> std::io::Result<PathBuf> {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3f");
    let mut dest = path.with_file_name(format!("{}.agentbridge-backup-{}{}", stem, stamp, ext));
    let mut n = 1;
    while dest.exists() {
        dest = path.with_file_name(format!("{}.agentbridge-backup-{}-{}{}", stem, stamp, n, ext));
        n += 1;
    }
    fs::copy(path, &dest)?;
    Ok(dest)
}

/// Write `new_text` over `r.target` atomically, but only if the file still
/// holds `old_text` (`None`: still does not exist). The temp file sits in the
/// same folder so the rename never crosses a filesystem, and takes the
/// original's permissions (`.claude.json` is private to its owner).
fn write_checked(r: &Resolved, old_text: Option<&str>, new_text: &str) -> Result<(), String> {
    let now = match fs::read(&r.target) {
        Ok(b) => Some(String::from_utf8(b).map_err(|_| "the file is not UTF-8".to_string())?),
        Err(_) => None,
    };
    if now.as_deref() != old_text {
        return Err(format!(
            "{} changed while agentbridge was editing it; nothing written (run the command again)",
            r.target.display()
        ));
    }
    let dir = r.target.parent().ok_or("settings file has no folder")?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    let name = r.target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!(".{}.agentbridge-tmp", name));
    fs::write(&tmp, new_text).map_err(|e| format!("cannot write {}: {}", tmp.display(), e))?;
    if let Ok(meta) = fs::metadata(&r.target) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = fs::rename(&tmp, &r.target) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {}", r.target.display(), e));
    }
    if r.link != r.target && fs::canonicalize(&r.link).ok().as_deref() != Some(r.target.as_path()) {
        return Err(format!("{} no longer points at {} after the write", r.link.display(), r.target.display()));
    }
    Ok(())
}

/// Held while install or uninstall edits settings files. Released on drop.
struct InstallLock(PathBuf);

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

const INSTALL_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn acquire_install_lock() -> Result<InstallLock, String> {
    let dir = crate::sync::data_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    let path = dir.join("mcp.lock");
    for _ in 0..2 {
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", std::process::id());
                return Ok(InstallLock(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > INSTALL_LOCK_STALE);
                if !stale {
                    break;
                }
                let _ = fs::remove_file(&path);
            }
            Err(e) => return Err(format!("cannot take {}: {}", path.display(), e)),
        }
    }
    Err("another `agentbridge mcp install` or `uninstall` is running; try again when it finishes".to_string())
}

// ---------------------------------------------------------------------------
// Reports

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The entry was added (or would be, under `--dry-run`).
    Added,
    /// agentbridge's own entry now points at a new binary path.
    Updated,
    /// Already in place; nothing to do.
    Unchanged,
    /// The entry was removed (uninstall).
    Removed,
    /// The tool is open; nothing written. Run the command again after
    /// closing it.
    SkippedOpen,
    /// An `agentbridge` entry agentbridge did not create; left as it is.
    SkippedForeign,
    /// agentbridge's entry was changed by hand since install; left as it is.
    SkippedEdited,
    /// The tool's session store is not on this machine.
    SkippedNotDetected,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub tool: String,
    pub file: PathBuf,
    pub action: Action,
    /// The entry (for `Added`/`Updated`) or why a tool was skipped.
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct MigrationReport {
    /// Turns recovered by the pull before copies were removed.
    pub pulled: usize,
    /// The shell rc the hook was (or would be) removed from.
    pub hook_removed: Option<PathBuf>,
    pub copies_removed: usize,
    pub copies_kept: usize,
    /// Databases backed up before copies were deleted from them.
    pub backups: Vec<PathBuf>,
    /// Why the migration (and so the install) stopped before deleting
    /// anything.
    pub stopped: Option<String>,
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub dry_run: bool,
    pub migration: MigrationReport,
    pub tools: Vec<ToolOutcome>,
    /// Why install stopped before adding any entry.
    pub stopped: Option<String>,
}

#[derive(Debug, Default)]
pub struct UninstallReport {
    pub dry_run: bool,
    pub removed: Vec<ToolOutcome>,
    pub kept: Vec<ToolOutcome>,
    pub stopped: Option<String>,
}

// ---------------------------------------------------------------------------
// Migration away from copies

/// Answers `Skip` to every conflict and remembers it, so a dry pull can tell
/// whether a real one would need a decision from you.
#[derive(Default)]
struct RecordConflicts(Vec<String>);

impl crate::sync::ConflictResolver for RecordConflicts {
    fn resolve(&mut self, session_id: &str, _items: &[crate::sync::ConflictItem]) -> crate::sync::ConflictChoice {
        self.0.push(session_id.to_string());
        crate::sync::ConflictChoice::Skip
    }
}

/// Tools whose copies live in a database that `unsync` can only change while
/// the tool is closed.
const DB_COPY_TOOLS: [&str; 3] = ["codex-cli", "opencode", "antigravity"];

fn migrate(dry_run: bool, is_open: &OpenCheck) -> MigrationReport {
    let mut report = MigrationReport::default();
    // Held from the dry pull until the last copy is gone, so the old shell
    // hook cannot start a sync in the middle.
    let _lock = if dry_run {
        None
    } else {
        match crate::sync::acquire_run_lock() {
            Some(l) => Some(l),
            None => {
                report.stopped = Some("another agentbridge sync is running; run install again in a minute".to_string());
                return report;
            }
        }
    };

    let mut recorder = RecordConflicts::default();
    let dry = crate::sync::pull_back_locked(true, &mut recorder);
    if !recorder.0.is_empty() {
        report.stopped = Some(format!(
            "{} session(s) have new work from more than one tool, which needs your decision. Run `agentbridge pull`, then `agentbridge mcp install` again. Nothing was deleted.",
            recorder.0.len()
        ));
        return report;
    }
    if let Some(e) = dry.errors.first() {
        report.stopped = Some(format!("pull failed ({}); nothing was deleted. Fix that, then run install again.", e));
        return report;
    }

    let copies = crate::sync::read_manifest();
    let blocking: Vec<&str> = DB_COPY_TOOLS
        .iter()
        .copied()
        .filter(|t| copies.iter().any(|r| r.target_provider == *t) && is_open(t))
        .collect();
    if !blocking.is_empty() {
        let names: Vec<&str> = blocking.iter().map(|t| display_name(t)).collect();
        report.stopped = Some(format!(
            "{} is open and holds copies agentbridge made; close it and run install again. Nothing was deleted.",
            names.join(" and ")
        ));
        return report;
    }

    if dry_run {
        report.pulled = dry.pulled.iter().map(|(_, n)| n).sum();
        report.hook_removed = crate::auto::uninstall_hook(true).ok().filter(|(_, had)| *had).map(|(rc, _)| rc);
        report.copies_removed = copies.len();
        return report;
    }

    let real = crate::sync::pull_back_locked(false, &mut crate::sync::AutoMerge);
    if let Some(e) = real.errors.first() {
        report.stopped = Some(format!("pull failed ({}); nothing was deleted. Fix that, then run install again.", e));
        return report;
    }
    report.pulled = real.pulled.iter().map(|(_, n)| n).sum();
    match crate::auto::uninstall_hook(false) {
        Ok((rc, true)) => report.hook_removed = Some(rc),
        Ok((_, false)) => {}
        Err(e) => {
            report.stopped = Some(format!("could not remove the shell hook: {}. Nothing else was deleted.", e));
            return report;
        }
    }
    // The copies inside tools' databases are deleted by row; back each
    // database up first, like every other write into one.
    for tool in DB_COPY_TOOLS {
        if !copies.iter().any(|r| r.target_provider == tool) {
            continue;
        }
        let db = match tool {
            "opencode" => Some(crate::connectors::opencode::default_db_path()).filter(|p| p.exists()),
            "codex-cli" => crate::codex_write::state_db(),
            _ => crate::antigravity_write::store().map(|h| crate::antigravity_write::summaries_db(&h)).filter(|p| p.exists()),
        };
        if let Some(db) = db {
            let done = match tool {
                "opencode" => crate::opencode_write::backup(&db).map_err(|e| e.to_string()),
                "codex-cli" => crate::codex_write::backup(&db).map_err(|e| e.to_string()),
                _ => crate::antigravity_write::backup(&db).map_err(|e| e.to_string()),
            };
            if let Err(e) = done {
                report.stopped = Some(format!("could not back up {} before removing copies ({}); nothing was deleted", db.display(), e));
                return report;
            }
            report.backups.push(db);
        }
    }
    let gone = crate::sync::unsync_sessions(false);
    report.copies_removed = gone.removed.len();
    report.copies_kept = gone.kept_foreign.len();
    report
}

// ---------------------------------------------------------------------------
// install / uninstall

fn mcp_rows() -> Vec<LinkRecord> {
    crate::sync::read_manifest_kind(KIND_MCP_CONFIG)
}

fn row_for<'r>(rows: &'r [LinkRecord], tool: &str, file: &Path) -> Option<&'r LinkRecord> {
    rows.iter().find(|r| r.target_provider == tool && r.dest == file && r.cache.as_os_str() == ENTRY_KEY)
}

/// The smallest span where `before` and `after` differ, widened until the
/// inserted text occurs exactly once in `after`: (removed, inserted).
fn span_diff(before: &str, after: &str) -> (String, String) {
    let floor = |s: &str, mut i: usize| {
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let mut p = before.bytes().zip(after.bytes()).take_while(|(a, b)| a == b).count();
    p = floor(before, p.min(before.len())).min(floor(after, p.min(after.len())));
    let max_s = before.len().min(after.len()) - p;
    let mut sfx = before.bytes().rev().zip(after.bytes().rev()).take(max_s).take_while(|(a, b)| a == b).count();
    loop {
        let (bs, as_) = (before.len() - sfx, after.len() - sfx);
        if before.is_char_boundary(bs) && after.is_char_boundary(as_) {
            let inserted = &after[p..as_];
            if (!inserted.is_empty() && after.matches(inserted).count() == 1) || (p == 0 && sfx == 0) {
                return (before[p..bs].to_string(), inserted.to_string());
            }
        }
        // Widen: one more byte of context on each side.
        if p > 0 {
            p = floor(after, p - 1).min(floor(before, p - 1));
        }
        sfx = sfx.saturating_sub(1);
    }
}

/// `text` with install's edit swapped back byte for byte, when the text it
/// inserted is still there exactly once.
fn undo_exact(text: &str, undo: Option<&Value>) -> Option<String> {
    let undo = undo?;
    let inserted = undo.get("inserted")?.as_str()?;
    let removed = undo.get("removed")?.as_str()?;
    (!inserted.is_empty() && text.matches(inserted).count() == 1).then(|| text.replacen(inserted, removed, 1))
}

/// What a settings file was like before install first touched it.
#[derive(Clone, Copy)]
struct Before {
    created_file: bool,
    created_parent: bool,
    blank: bool,
}

fn new_row(tool: &str, file: &Path, value: &Value, before: Before, undo: Option<Value>) -> LinkRecord {
    LinkRecord {
        dest: file.to_path_buf(),
        cache: PathBuf::from(ENTRY_KEY),
        session_id: String::new(),
        source_provider: String::new(),
        target_provider: tool.to_string(),
        project: std::env::current_dir().unwrap_or_default(),
        inode: 0,
        message_count: 0,
        title: None,
        kind: KIND_MCP_CONFIG.to_string(),
        entry: Some(value.clone()),
        created_file: before.created_file,
        created_parent: before.created_parent,
        blank_before: before.blank,
        undo,
    }
}

/// Add agentbridge's entry to every detected tool's settings file, after
/// moving the machine off copy based sync. `exe` is the binary the entry
/// runs (the one that ran install, canonicalized).
pub fn install(registry: &Registry, exe: &Path, dry_run: bool, is_open: &OpenCheck) -> InstallReport {
    let mut report = InstallReport { dry_run, ..Default::default() };
    let _lock = if dry_run {
        None
    } else {
        match acquire_install_lock() {
            Ok(l) => Some(l),
            Err(e) => {
                report.stopped = Some(e);
                return report;
            }
        }
    };

    report.migration = migrate(dry_run, is_open);
    if let Some(why) = &report.migration.stopped {
        report.stopped = Some(why.clone());
        return report;
    }

    let detected: Vec<&str> = registry.detected().map(|c| c.id()).collect();
    let mut rows = mcp_rows();
    for tool in TOOLS {
        let path = settings_path(tool);
        let outcome = install_one(tool, &path, exe, dry_run, detected.contains(&tool), is_open, &mut rows);
        report.tools.push(outcome);
    }
    report
}

fn install_one(
    tool: &str,
    path: &Path,
    exe: &Path,
    dry_run: bool,
    detected: bool,
    is_open: &OpenCheck,
    rows: &mut Vec<LinkRecord>,
) -> ToolOutcome {
    let out = |file: &Path, action: Action, detail: String| ToolOutcome {
        tool: tool.to_string(),
        file: file.to_path_buf(),
        action,
        detail,
    };
    if !detected {
        return out(path, Action::SkippedNotDetected, format!("{} is not on this machine", display_name(tool)));
    }
    if is_open(tool) {
        return out(path, Action::SkippedOpen, format!("{} is open; close it and run install again", display_name(tool)));
    }
    let r = match resolve(path) {
        Ok(r) => r,
        Err(e) => return out(path, Action::Failed, e),
    };
    let text = if r.exists {
        match fs::read_to_string(&r.target) {
            Ok(t) => Some(t),
            Err(e) => return out(&r.target, Action::Failed, format!("cannot read: {}", e)),
        }
    } else {
        None
    };
    let format = format_of(tool);
    let current = match current_entry(format, text.as_deref().unwrap_or("")) {
        Ok(c) => c,
        Err(e) => return out(&r.target, Action::Failed, e),
    };
    let desired = entry_for(tool, exe);
    let row = row_for(rows, tool, &r.target).cloned();

    let action = match (&current, &row) {
        (None, _) => Action::Added,
        (Some(c), Some(row)) if *c == desired && row.entry.as_ref() == Some(c) => Action::Unchanged,
        (Some(c), Some(row)) if row.entry.as_ref() == Some(c) => Action::Updated,
        (Some(_), Some(_)) => {
            return out(&r.target, Action::SkippedEdited, "agentbridge's entry was changed by hand; left as it is".to_string());
        }
        (Some(_), None) => {
            return out(
                &r.target,
                Action::SkippedForeign,
                "an `agentbridge` entry agentbridge did not add is already there; left as it is".to_string(),
            );
        }
    };
    let detail = render_entry(tool, &desired);
    if action == Action::Unchanged || dry_run {
        return out(&r.target, action, detail);
    }

    let (new_text, created_parent) = match with_entry(format, text.as_deref().unwrap_or(""), &desired) {
        Ok(x) => x,
        Err(e) => return out(&r.target, Action::Failed, e),
    };
    if r.exists
        && let Err(e) = backup_settings(&r.target)
    {
        return out(&r.target, Action::Failed, format!("backup failed, nothing written: {}", e));
    }
    if let Err(e) = write_checked(&r, text.as_deref(), &new_text) {
        return out(&r.target, Action::Failed, e);
    }
    // The row only after the edit is on disk. An update keeps the original
    // row's `created_*` facts: they describe the file before agentbridge.
    let before = match &row {
        Some(old) => Before { created_file: old.created_file, created_parent: old.created_parent, blank: old.blank_before },
        None => Before {
            created_file: !r.exists,
            created_parent,
            blank: r.exists && text.as_deref().is_some_and(|t| t.trim().is_empty()),
        },
    };
    // The exact edit, measured from the file as it was before agentbridge's
    // first install (an update first puts the old entry's bytes back).
    let old_text = text.as_deref().unwrap_or("");
    let pristine = match &row {
        Some(old) => undo_exact(old_text, old.undo.as_ref()),
        None => Some(old_text.to_string()),
    };
    let undo = pristine.map(|p| {
        let (removed, inserted) = span_diff(&p, &new_text);
        json!({ "removed": removed, "inserted": inserted })
    });
    rows.retain(|x| !(x.target_provider == tool && x.dest == r.target));
    rows.push(new_row(tool, &r.target, &desired, before, undo));
    if let Err(e) = crate::sync::write_manifest_kind(KIND_MCP_CONFIG, rows) {
        return out(&r.target, Action::Failed, format!("entry written but the manifest could not be updated: {}", e));
    }
    out(&r.target, action, detail)
}

/// Remove exactly the entries agentbridge added, as recorded in the
/// manifest. An entry changed by hand is kept and reported; a tool that is
/// open is kept for a later run.
pub fn uninstall(dry_run: bool, is_open: &OpenCheck) -> UninstallReport {
    let mut report = UninstallReport { dry_run, ..Default::default() };
    let rows = mcp_rows();
    if rows.is_empty() {
        return report;
    }
    let _lock = if dry_run {
        None
    } else {
        match acquire_install_lock() {
            Ok(l) => Some(l),
            Err(e) => {
                report.stopped = Some(e);
                return report;
            }
        }
    };
    let mut keep: Vec<LinkRecord> = Vec::new();
    for row in rows {
        let tool = row.target_provider.clone();
        let outcome = |action: Action, detail: &str| ToolOutcome {
            tool: tool.clone(),
            file: row.dest.clone(),
            action,
            detail: detail.to_string(),
        };
        if is_open(&tool) {
            report.kept.push(outcome(Action::SkippedOpen, &format!("{} is open; close it and run uninstall again", display_name(&tool))));
            keep.push(row);
            continue;
        }
        let text = match fs::read_to_string(&row.dest) {
            Ok(t) => t,
            // The file is gone: so is the entry. Drop the row.
            Err(_) => {
                report.removed.push(outcome(Action::Removed, "the settings file is gone"));
                continue;
            }
        };
        let format = format_of(&tool);
        let current = match current_entry(format, &text) {
            Ok(c) => c,
            Err(e) => {
                report.kept.push(outcome(Action::Failed, &e));
                keep.push(row);
                continue;
            }
        };
        match current {
            None => {
                report.removed.push(outcome(Action::Removed, "the entry was already gone"));
            }
            Some(c) if Some(&c) != row.entry.as_ref() => {
                report.kept.push(outcome(Action::SkippedEdited, "agentbridge's entry was changed by hand; left as it is"));
                keep.push(row);
            }
            Some(_) => {
                if dry_run {
                    report.removed.push(outcome(Action::Removed, ""));
                    keep.push(row);
                    continue;
                }
                let r = Resolved { link: row.dest.clone(), target: row.dest.clone(), exists: true };
                // Byte exact when install's own text is still there; else the
                // editor removes the entry (the tool rewrote the file since).
                let exact = undo_exact(&text, row.undo.as_ref())
                    .filter(|t| current_entry(format, t).ok().flatten().is_none())
                    .map(|t| {
                        let empty = t.trim().is_empty();
                        (t, empty)
                    });
                let result = exact.map(Ok).unwrap_or_else(|| without_entry(format, &text, row.created_parent)).and_then(|(new_text, empty)| {
                    backup_settings(&row.dest).map_err(|e| format!("backup failed, nothing written: {}", e))?;
                    if row.created_file && empty {
                        // The file did not exist before install and holds
                        // nothing else now: put it back the way it was.
                        let now = fs::read_to_string(&row.dest).ok();
                        if now.as_deref() != Some(text.as_str()) {
                            return Err(format!("{} changed while agentbridge was editing it; run uninstall again", row.dest.display()));
                        }
                        fs::remove_file(&row.dest).map_err(|e| format!("cannot remove {}: {}", row.dest.display(), e))
                    } else if row.blank_before && empty {
                        write_checked(&r, Some(&text), "")
                    } else {
                        write_checked(&r, Some(&text), &new_text)
                    }
                });
                match result {
                    Ok(()) => report.removed.push(outcome(Action::Removed, "")),
                    Err(e) => {
                        report.kept.push(outcome(Action::Failed, &e));
                        keep.push(row);
                    }
                }
            }
        }
    }
    if !dry_run && let Err(e) = crate::sync::write_manifest_kind(KIND_MCP_CONFIG, &keep) {
        report.stopped = Some(format!("the manifest could not be updated: {}", e));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/agentbridge/bin/agentbridge";

    fn value() -> Value {
        entry_for("claude-code", Path::new(EXE))
    }

    #[test]
    fn test_jsonc_edit_keeps_comments_and_undo_is_byte_identical() {
        let original = "{\n  // my servers\n  \"mcpServers\": {\n    \"other\": { \"command\": \"x\" }, // keep me\n  },\n  \"theme\": \"dark\",\n}\n";
        let f = Format::Json("mcpServers");
        let (added, created_parent) = with_entry(f, original, &value()).unwrap();
        assert!(!created_parent);
        assert!(added.contains("// my servers") && added.contains("// keep me"), "{added}");
        assert_eq!(current_entry(f, &added).unwrap(), Some(value()));
        let (back, empty) = without_entry(f, &added, created_parent).unwrap();
        assert!(!empty);
        assert_eq!(back, original);
    }

    /// Regression (the real `opencode.jsonc` on the operator's machine,
    /// 2026-10-30): a trailing comma followed by blank lines before `}`. The
    /// undo has to give those bytes back too.
    #[test]
    fn test_trailing_comma_and_blank_lines_survive_undo() {
        let original = "{\n  \"$schema\": \"https://opencode.ai/config.json\",\n \n\n}\n";
        let f = Format::Json("mcp");
        let v = entry_for("opencode", Path::new(EXE));
        let (added, _) = with_entry(f, original, &v).unwrap();
        let (removed, inserted) = span_diff(original, &added);
        let undo = json!({ "removed": removed, "inserted": inserted });
        assert_eq!(undo_exact(&added, Some(&undo)).as_deref(), Some(original));
    }

    #[test]
    fn test_span_diff_is_unique_and_reversible() {
        for (a, b) in [("", "{\"x\":1}\n"), ("{}\n", "{\"a\":{}}\n"), ("aXa", "aXXa"), ("é{}", "é{\"k\":1}")] {
            let (removed, inserted) = span_diff(a, b);
            assert_eq!(b.matches(inserted.as_str()).count(), 1, "{a:?} -> {b:?}");
            assert_eq!(b.replacen(&inserted, &removed, 1), a);
        }
    }

    #[test]
    fn test_json_without_parent_gets_one_and_loses_it_on_undo() {
        let original = "{\n  \"numStartups\": 3,\n  \"projects\": {}\n}\n";
        let f = Format::Json("mcpServers");
        let (added, created_parent) = with_entry(f, original, &value()).unwrap();
        assert!(created_parent);
        let parsed: Value = serde_json::from_str(&added).unwrap();
        assert_eq!(parsed["mcpServers"]["agentbridge"], value());
        assert_eq!(parsed["numStartups"], 3);
        let (back, _) = without_entry(f, &added, true).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn test_empty_json_file_is_created_as_an_object() {
        let f = Format::Json("mcp");
        let v = entry_for("opencode", Path::new(EXE));
        let (text, created_parent) = with_entry(f, "", &v).unwrap();
        assert!(created_parent);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, json!({ "mcp": { "agentbridge": v } }));
        let (_, empty) = without_entry(f, &text, true).unwrap();
        assert!(empty, "nothing else is in the file");
    }

    #[test]
    fn test_toml_edit_keeps_comments_and_undo_is_byte_identical() {
        let original = "# Codex settings\nmodel = \"gpt-5\" # pinned\n\n[mcp_servers.other]\ncommand = \"x\"\n\n[profiles.fast]\nmodel = \"mini\"\n";
        let v = entry_for("codex-cli", Path::new(EXE));
        let (added, created_parent) = with_entry(Format::Toml, original, &v).unwrap();
        assert!(!created_parent);
        assert!(added.contains("# Codex settings") && added.contains("# pinned"), "{added}");
        assert!(added.contains("[mcp_servers.agentbridge]"), "{added}");
        assert_eq!(current_entry(Format::Toml, &added).unwrap(), Some(v));
        let (back, _) = without_entry(Format::Toml, &added, created_parent).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn test_toml_without_parent_writes_only_the_entry_table() {
        let original = "model = \"gpt-5\"\n";
        let v = entry_for("codex-cli", Path::new(EXE));
        let (added, created_parent) = with_entry(Format::Toml, original, &v).unwrap();
        assert!(created_parent);
        assert!(!added.contains("[mcp_servers]\n"), "no bare parent header: {added}");
        assert!(added.contains("[mcp_servers.agentbridge]"));
        assert!(added.contains(&format!("command = \"{}\"", EXE)));
        assert!(added.contains("args = [\"mcp\"]"));
        let (back, _) = without_entry(Format::Toml, &added, true).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn test_backup_names_keep_the_extension_and_never_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("opencode.jsonc");
        fs::write(&file, "{}").unwrap();
        let a = backup_settings(&file).unwrap();
        let b = backup_settings(&file).unwrap();
        assert_ne!(a, b);
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.starts_with("opencode.agentbridge-backup-") && name.ends_with(".jsonc"), "{name}");
            assert_eq!(fs::read_to_string(p).unwrap(), "{}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinked_file_stays_a_symlink_and_dangling_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("config/mcp_config.json");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "{}").unwrap();
        let link = tmp.path().join("mcp_config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let r = resolve(&link).unwrap();
        assert_eq!(r.target, fs::canonicalize(&real).unwrap());
        write_checked(&r, Some("{}"), "{\"mcpServers\":{}}").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "{\"mcpServers\":{}}");

        let dangling = tmp.path().join("gone.json");
        std::os::unix::fs::symlink(tmp.path().join("missing.json"), &dangling).unwrap();
        assert!(resolve(&dangling).is_err());
    }

    /// Real data check: every real settings file on this machine takes the
    /// entry and gives it back byte for byte. Reads only; nothing is written.
    #[test]
    #[ignore = "reads the real settings files on this machine"]
    fn test_real_settings_files_round_trip_in_memory() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let mut files: Vec<(&str, PathBuf)> = TOOLS.iter().map(|t| (*t, settings_path(t))).collect();
        files.push(("claude-code", home.join(".claude.json")));
        let mut checked = 0;
        for (tool, path) in files {
            let Ok(text) = fs::read_to_string(&path) else {
                eprintln!("{}: {} not there", tool, path.display());
                continue;
            };
            let f = format_of(tool);
            let v = entry_for(tool, Path::new(EXE));
            assert_eq!(current_entry(f, &text).unwrap(), None, "{} already has an agentbridge entry", path.display());
            let (added, created_parent) = with_entry(f, &text, &v).unwrap_or_else(|e| panic!("{}: {}", path.display(), e));
            assert_eq!(current_entry(f, &added).unwrap(), Some(v.clone()), "{}", path.display());
            if let Format::Json(_) = f {
                // Plain JSON stays plain JSON a strict parser accepts.
                if serde_json::from_str::<Value>(&text).is_ok() {
                    serde_json::from_str::<Value>(&added).unwrap_or_else(|e| panic!("{} is no longer strict JSON: {}", path.display(), e));
                }
            }
            let (removed, inserted) = span_diff(&text, &added);
            let undo = json!({ "removed": removed, "inserted": inserted });
            assert_eq!(undo_exact(&added, Some(&undo)).as_deref(), Some(text.as_str()), "{} not byte identical after undo", path.display());
            let (back, _) = without_entry(f, &added, created_parent).unwrap();
            assert_eq!(current_entry(f, &back).unwrap(), None, "{}: the fallback removes the entry", path.display());
            eprintln!("{}: {} ({} bytes) ok, parent created: {}", tool, path.display(), text.len(), created_parent);
            checked += 1;
        }
        assert!(checked > 0);
    }

    #[test]
    fn test_write_checked_refuses_a_file_that_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(".claude.json");
        fs::write(&file, "{\"a\":2}").unwrap();
        let r = resolve(&file).unwrap();
        assert!(write_checked(&r, Some("{\"a\":1}"), "{}").is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"a\":2}");
    }
}
