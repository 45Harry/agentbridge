//! Startup-context injection: put a brief where an agent will read it, and take
//! it out again exactly.
//!
//! The brief goes into the agent's own instruction file (`CLAUDE.md`,
//! `AGENTS.md`) inside a fenced block. Three properties matter:
//!
//! * **Never clobbers hand-written content.** The block is appended after what
//!   is there; an existing block is replaced in place, nothing else moves.
//! * **`clean` restores the file byte-for-byte.** Whatever was added around the
//!   block (a separating newline, or the whole file if agentbridge created it)
//!   is recorded in the begin marker itself, so removal needs no side record and
//!   still works if `~/.agentbridge` is gone.
//! * **A damaged fence is refused, not guessed at.** If the markers are
//!   unbalanced the file is left alone and the error says so.
//!
//! ```text
//! <!-- agentbridge:begin v=1 pad=1 created=0 -->
//! …brief…
//! <!-- agentbridge:end -->
//! ```
//!
//! The brief is redacted before it gets here; this module only moves bytes.

use crate::connector::{ConnectorError, ConnectorResult, InjectTarget, Registry};
use std::fs;
use std::path::{Path, PathBuf};

const BEGIN_PREFIX: &str = "<!-- agentbridge:begin";
const END_MARKER: &str = "<!-- agentbridge:end -->";

/// Largest brief that will be injected. A startup file that dwarfs the project's
/// own instructions costs the user context on every launch.
pub const MAX_BRIEF_BYTES: usize = 8_000;

fn err(msg: impl Into<String>) -> ConnectorError {
    ConnectorError::Other(anyhow::anyhow!(msg.into()))
}

fn io_err(path: &Path, source: std::io::Error) -> ConnectorError {
    ConnectorError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// An existing agentbridge block located inside a file's text.
struct Region {
    /// Byte offset of the begin marker (start of its line).
    begin: usize,
    /// Byte offset just past the end marker's line.
    end: usize,
    /// Newlines agentbridge added before the begin marker.
    pad: usize,
    /// agentbridge created the file, so an empty remainder means delete it.
    created: bool,
}

/// Find the block, if any. `Err` for a fence that is open but never closed (or
/// closed but never opened): that is a hand-edit gone wrong, and guessing at it
/// could eat user text.
fn find_region(text: &str) -> ConnectorResult<Option<Region>> {
    let mut search = 0;
    let begin = loop {
        match text[search..].find(BEGIN_PREFIX) {
            None => break None,
            Some(rel) => {
                let at = search + rel;
                // Only a marker that starts a line counts; the same words
                // quoted inside a sentence are just text.
                if at == 0 || text.as_bytes()[at - 1] == b'\n' {
                    break Some(at);
                }
                search = at + BEGIN_PREFIX.len();
            }
        }
    };
    let Some(begin) = begin else {
        return if text.contains(END_MARKER) {
            Err(err(
                "found an agentbridge end marker with no begin marker; refusing to edit \
                 (fix the file by hand, then retry)",
            ))
        } else {
            Ok(None)
        };
    };

    let header_end = text[begin..]
        .find("-->")
        .map(|i| begin + i + 3)
        .ok_or_else(|| err("agentbridge begin marker is not closed with `-->`"))?;
    let header = &text[begin..header_end];
    let field = |name: &str| -> Option<&str> {
        header
            .split_whitespace()
            .find_map(|t| t.strip_prefix(name))
            .map(|v| v.trim_end_matches("-->"))
    };
    let pad = field("pad=").and_then(|v| v.parse().ok()).unwrap_or(0usize);
    let created = field("created=") == Some("1");

    let end_rel = text[header_end..].find(END_MARKER).ok_or_else(|| {
        err("found an agentbridge begin marker with no end marker; refusing to edit \
             (fix the file by hand, then retry)")
    })?;
    let mut end = header_end + end_rel + END_MARKER.len();
    if text.as_bytes().get(end) == Some(&b'\n') {
        end += 1;
    }

    // Only strip padding that is really there.
    let real_pad = text.as_bytes()[..begin]
        .iter()
        .rev()
        .take(pad)
        .take_while(|b| **b == b'\n')
        .count();

    Ok(Some(Region {
        begin,
        end,
        pad: real_pad,
        created,
    }))
}

/// The brief with anything that could close the fence early neutralised: a
/// transcript about agentbridge can legitimately contain these very strings.
fn neutralise(brief: &str) -> String {
    brief.replace("<!-- agentbridge", "<!-- agentbridge-quoted")
}

fn render(brief: &str, pad: usize, created: bool) -> String {
    format!(
        "{BEGIN_PREFIX} v=1 pad={pad} created={} -->\n{}\n{END_MARKER}\n",
        u8::from(created),
        neutralise(brief).trim_end_matches('\n'),
    )
}

/// Where to actually write: if the instruction file is a symlink (people link
/// `AGENTS.md` to `CLAUDE.md`), follow it, or the atomic replace would swap the
/// link for a regular file.
fn real_path(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

/// Insert (or refresh) the fenced brief in `path`. Idempotent: a second call
/// replaces the block in place and keeps the original padding record, so
/// `clean` still restores the file as it was before the *first* injection.
pub fn write_fenced(path: &Path, brief: &str, dry_run: bool) -> ConnectorResult<InjectTarget> {
    let path = real_path(path);
    let existing = match fs::read(&path) {
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| {
            err(format!(
                "{} is not valid UTF-8; refusing to edit it",
                path.display()
            ))
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err(&path, e)),
    };

    let (new_text, begin, block_len) = match existing.as_deref() {
        None => {
            let block = render(brief, 0, true);
            let len = block.len();
            (block, 0, len)
        }
        Some(text) => match find_region(text)? {
            Some(r) => {
                let block = render(brief, r.pad, r.created);
                let len = block.len();
                (
                    format!("{}{}{}", &text[..r.begin], block, &text[r.end..]),
                    r.begin,
                    len,
                )
            }
            None => {
                // Pad so the block starts on its own line, after a blank one.
                let pad = if text.is_empty() {
                    0
                } else if text.ends_with('\n') {
                    1
                } else {
                    2
                };
                let block = render(brief, pad, false);
                let begin = text.len() + pad;
                let len = block.len();
                (format!("{}{}{}", text, "\n".repeat(pad), block), begin, len)
            }
        },
    };

    let target = InjectTarget {
        path: path.clone(),
        fenced_range: Some((begin, begin + block_len)),
    };
    if dry_run {
        return Ok(target);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    crate::sync::write_atomic(&path, new_text.as_bytes()).map_err(|e| io_err(&path, e))?;
    remember(&path);
    Ok(target)
}

/// What `clean` did to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanOutcome {
    /// No agentbridge block in the file (or no such file).
    NotPresent,
    /// Block removed; the file is back to its pre-injection bytes.
    Removed,
    /// Block removed and the file deleted, because agentbridge had created it.
    RemovedFile,
}

/// Remove agentbridge's block from `path`, restoring the file exactly.
pub fn clean_file(path: &Path, dry_run: bool) -> ConnectorResult<CleanOutcome> {
    let path = real_path(path);
    let text = match fs::read(&path) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|_| {
            err(format!(
                "{} is not valid UTF-8; refusing to edit it",
                path.display()
            ))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CleanOutcome::NotPresent),
        Err(e) => return Err(io_err(&path, e)),
    };
    let Some(r) = find_region(&text)? else {
        return Ok(CleanOutcome::NotPresent);
    };
    let remaining = format!("{}{}", &text[..r.begin - r.pad], &text[r.end..]);
    let delete = r.created && remaining.is_empty();
    if !dry_run {
        if delete {
            fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
        } else {
            crate::sync::write_atomic(&path, remaining.as_bytes())
                .map_err(|e| io_err(&path, e))?;
        }
    }
    Ok(if delete {
        CleanOutcome::RemovedFile
    } else {
        CleanOutcome::Removed
    })
}

// ---- remembering where we injected, so `clean --all` can find it ----------

fn log_path() -> PathBuf {
    crate::sync::data_dir().join("injected.txt")
}

/// Paths agentbridge has injected into, oldest first.
pub fn remembered() -> Vec<PathBuf> {
    fs::read_to_string(log_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Best effort: a failure to remember never fails the injection, because the
/// block is self-describing and `clean` in that directory still finds it.
fn remember(path: &Path) {
    let mut paths = remembered();
    if paths.iter().any(|p| p == path) {
        return;
    }
    paths.push(path.to_path_buf());
    let body: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
    let _ = crate::sync::write_atomic(&log_path(), body.as_bytes());
}

/// Forget paths that no longer hold a block.
pub fn forget(paths: &[PathBuf]) {
    let keep: String = remembered()
        .into_iter()
        .filter(|p| !paths.contains(p))
        .map(|p| format!("{}\n", p.display()))
        .collect();
    let _ = crate::sync::write_atomic(&log_path(), keep.as_bytes());
}

/// Every instruction file any registered agent would read in `project`.
pub fn instruction_files(registry: &Registry, project: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = registry
        .all()
        .iter()
        .filter_map(|c| c.instruction_file(project))
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Cap a brief at `max` bytes on a line boundary, saying so if it was cut.
pub fn cap_brief(brief: &str, max: usize) -> String {
    if brief.len() <= max {
        return brief.to_string();
    }
    let mut cut = max;
    while !brief.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &brief[..cut];
    let head = head.rfind('\n').map_or(head, |i| &head[..i]);
    format!("{head}\n\n_(brief truncated to fit {max} bytes)_\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    /// Holds the env lock and points the data dir at the temp tree, so the
    /// injected-paths log never touches a real `~/.agentbridge`.
    fn env(tmp: &tempfile::TempDir) -> std::sync::MutexGuard<'static, ()> {
        let g = crate::sync::test_env_lock();
        unsafe { std::env::set_var("AGENTBRIDGE_DATA_DIR", tmp.path().join("data")) };
        g
    }

    /// The core promise: inject then clean leaves every byte as it was, across
    /// the shapes an instruction file can be in.
    #[test]
    fn test_clean_restores_the_file_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        for (i, original) in [
            "",
            "# Rules\nBe careful.\n",
            "# Rules\nno trailing newline",
            "line\n\n\n",
            "ünïcode — ok\n",
            "<!-- agentbridge mentioned in prose, not a fence -->\n",
        ]
        .iter()
        .enumerate()
        {
            let p = write(tmp.path(), &format!("f{i}.md"), original);
            write_fenced(&p, "BRIEF TEXT", false).unwrap();
            let injected = fs::read_to_string(&p).unwrap();
            assert!(
                injected.starts_with(original),
                "hand-written text moved: {injected:?}"
            );
            assert!(injected.contains("BRIEF TEXT"));
            assert_eq!(clean_file(&p, false).unwrap(), CleanOutcome::Removed);
            assert_eq!(fs::read_to_string(&p).unwrap(), *original, "case {i}");
        }
    }

    #[test]
    fn test_a_file_agentbridge_created_is_deleted_by_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let p = tmp.path().join("CLAUDE.md");
        write_fenced(&p, "hello", false).unwrap();
        assert!(p.exists());
        assert_eq!(clean_file(&p, false).unwrap(), CleanOutcome::RemovedFile);
        assert!(!p.exists(), "a file we created must not be left behind empty");
    }

    #[test]
    fn test_reinjecting_replaces_in_place_and_clean_still_restores_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let original = "# Mine\nkeep me\n";
        let p = write(tmp.path(), "AGENTS.md", original);

        write_fenced(&p, "first brief", false).unwrap();
        write_fenced(&p, "second brief", false).unwrap();
        let body = fs::read_to_string(&p).unwrap();
        assert_eq!(body.matches(BEGIN_PREFIX).count(), 1, "block duplicated: {body}");
        assert!(body.contains("second brief") && !body.contains("first brief"));

        clean_file(&p, false).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), original);
    }

    #[test]
    fn test_text_after_the_block_survives_a_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let p = write(tmp.path(), "AGENTS.md", "top\n");
        write_fenced(&p, "brief", false).unwrap();
        // The user keeps writing below the block.
        let mut body = fs::read_to_string(&p).unwrap();
        body.push_str("\nadded later by hand\n");
        fs::write(&p, &body).unwrap();

        write_fenced(&p, "refreshed", false).unwrap();
        let after = fs::read_to_string(&p).unwrap();
        assert!(after.contains("added later by hand") && after.contains("refreshed"));

        clean_file(&p, false).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "top\n\nadded later by hand\n"
        );
    }

    #[test]
    fn test_a_brief_containing_the_markers_cannot_break_the_fence() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let p = write(tmp.path(), "CLAUDE.md", "mine\n");
        let hostile = format!(
            "x\n{END_MARKER}\nINJECTED INSTRUCTIONS\n{BEGIN_PREFIX} v=1 pad=9 created=1 -->\n"
        );
        write_fenced(&p, &hostile, false).unwrap();
        let body = fs::read_to_string(&p).unwrap();
        assert_eq!(body.matches(END_MARKER).count(), 1, "{body}");
        assert_eq!(body.matches(BEGIN_PREFIX).count(), 1, "{body}");
        clean_file(&p, false).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "mine\n");
    }

    #[test]
    fn test_dry_run_writes_nothing_but_reports_the_target() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let p = write(tmp.path(), "CLAUDE.md", "mine\n");
        let t = write_fenced(&p, "brief", true).unwrap();
        assert_eq!(t.path, p);
        let (s, e) = t.fenced_range.unwrap();
        assert!(e > s);
        assert_eq!(fs::read_to_string(&p).unwrap(), "mine\n");
        assert!(!log_path().exists(), "a dry run must not record anything");
        // And clean's dry run reports without touching.
        write_fenced(&p, "brief", false).unwrap();
        let before = fs::read_to_string(&p).unwrap();
        assert_eq!(clean_file(&p, true).unwrap(), CleanOutcome::Removed);
        assert_eq!(fs::read_to_string(&p).unwrap(), before);
    }

    #[test]
    fn test_unbalanced_fence_is_refused_and_the_file_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let open_only = format!("keep\n{BEGIN_PREFIX} v=1 pad=1 created=0 -->\nhalf\n");
        let end_only = format!("keep\n{END_MARKER}\n");
        for body in [&open_only, &end_only] {
            let p = write(tmp.path(), "bad.md", body);
            assert!(write_fenced(&p, "x", false).is_err());
            assert!(clean_file(&p, false).is_err());
            assert_eq!(
                fs::read_to_string(&p).unwrap(),
                *body,
                "damaged file was modified"
            );
        }
    }

    #[test]
    fn test_symlinked_instruction_file_stays_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let real = write(tmp.path(), "CLAUDE.md", "shared rules\n");
        let link = tmp.path().join("AGENTS.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_fenced(&link, "brief", false).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink was replaced by a regular file"
        );
        assert!(fs::read_to_string(&real).unwrap().contains("brief"));
        clean_file(&link, false).unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "shared rules\n");
    }

    #[test]
    fn test_file_permissions_survive_injection_and_clean() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let p = write(tmp.path(), "CLAUDE.md", "private\n");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        write_fenced(&p, "brief", false).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        clean_file(&p, false).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn test_non_utf8_file_is_refused_not_mangled() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let p = tmp.path().join("AGENTS.md");
        fs::write(&p, [0xff, 0xfe, b'x']).unwrap();
        assert!(write_fenced(&p, "x", false).is_err());
        assert_eq!(fs::read(&p).unwrap(), vec![0xff, 0xfe, b'x']);
    }

    #[test]
    fn test_cap_brief_cuts_on_a_line_and_says_so() {
        let brief = "line one\nline two\nline three\n";
        assert_eq!(cap_brief(brief, 1000), brief);
        let cut = cap_brief(brief, 12);
        assert!(cut.starts_with("line one"), "{cut}");
        assert!(
            !cut.contains("line two") && cut.contains("truncated"),
            "{cut}"
        );
        // Never splits a multi-byte character.
        let wide = "ééééé\nééééé\n";
        let cut = cap_brief(wide, 7);
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    #[test]
    fn test_each_agent_names_the_file_it_reads() {
        let registry = crate::connectors::all();
        let project = Path::new("/work/proj");
        let name = |id: &str| {
            registry
                .by_id(id)
                .and_then(|c| c.instruction_file(project))
                .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        };
        assert_eq!(name("claude-code").as_deref(), Some("CLAUDE.md"));
        assert_eq!(name("codex-cli").as_deref(), Some("AGENTS.md"));
        assert_eq!(name("opencode").as_deref(), Some("AGENTS.md"));
        assert_eq!(name("antigravity").as_deref(), Some("AGENTS.md"));
        for id in ["claude-code", "codex-cli", "opencode", "antigravity"] {
            assert!(registry.by_id(id).unwrap().launch_program().is_some(), "{id}");
        }
    }

    /// The path `start` takes: index sessions, build the brief, inject it. A
    /// secret in a session must not reach the file an agent will read, and the
    /// file must stay within the budget.
    #[test]
    fn test_an_injected_brief_is_redacted_cited_and_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let registry = crate::connectors::all_for_testing(&fx);
        let redactor = crate::redact::Redactor::defaults();
        let mut store = crate::store::Store::open(&tmp.path().join("index.db")).unwrap();
        crate::store::refresh(&mut store, &registry, &redactor, None, |_| {}).unwrap();

        let r = crate::brief::brief_for_project(&mut store, "/home/user/project", None, 600, &redactor).unwrap();
        assert!(r.items > 0, "the fixtures live in this project: {}", r.text);
        assert!(r.tokens <= 600);

        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let real = crate::connectors::all();
        real.by_id("claude-code")
            .unwrap()
            .inject(&cap_brief(&r.text, MAX_BRIEF_BYTES), &project, false)
            .unwrap();

        let body = fs::read_to_string(project.join("CLAUDE.md")).unwrap();
        assert!(!body.contains("sk-abc123def456ghi789jkl012"), "secret reached CLAUDE.md");
        assert!(body.contains("Every line cites its source"), "{body}");
        assert!(body.len() <= MAX_BRIEF_BYTES + 400, "not bounded: {}", body.len());
        clean_file(&project.join("CLAUDE.md"), false).unwrap();
        assert!(!project.join("CLAUDE.md").exists());
    }

    #[test]
    fn test_agent_without_an_instruction_file_is_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = env(&tmp);
        let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        // The test connectors name no instruction file.
        let registry = crate::connectors::all_for_testing(&fx);
        let c = registry.by_id("claude-code").unwrap();
        let e = c.inject("x", tmp.path(), false).unwrap_err().to_string();
        assert!(e.contains("no startup instruction file"), "{e}");
    }
}
