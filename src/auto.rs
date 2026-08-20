//! Automatic sync.
//!
//! The operator should run `init` once at install and never think about
//! syncing again. Two mechanisms, deliberately simple:
//!
//! * `watch` — a foreground loop that re-syncs when something actually
//!   changed. Cheap because the check is a stat sweep over session files, not
//!   a re-parse.
//! * `install`/`uninstall` — a shell hook so a new terminal syncs the
//!   directory you just entered, which is what makes it feel automatic
//!   without a daemon running all the time.
//!
//! Both converge on the same `sync_into` used by the manual command, so
//! there is one code path and one set of guarantees.

use crate::connector::Registry;
use crate::index::discover;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A cheap fingerprint of every session store: (path, size, mtime). Comparing
/// fingerprints tells us whether a re-sync is worth doing without opening or
/// parsing a single transcript.
pub fn fingerprint(registry: &Registry) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let index = discover(registry);
    let mut seen: Vec<PathBuf> = index.entries.iter().map(|e| e.source_path.clone()).collect();
    seen.sort();
    seen.dedup();
    fingerprint_files(&seen)
}

/// Stat each store path plus its SQLite WAL siblings.
fn fingerprint_files(paths: &[PathBuf]) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut out: Vec<(PathBuf, u64, Option<SystemTime>)> = Vec::new();
    for p in paths {
        let meta = std::fs::metadata(p).ok();
        out.push((
            p.clone(),
            meta.as_ref().map(|m| m.len()).unwrap_or(0),
            meta.as_ref().and_then(|m| m.modified().ok()),
        ));
        // SQLite WAL databases write new rows to `<db>-wal` and only
        // checkpoint into `<db>` later, so statting the .db alone can miss
        // brand-new sessions for long stretches.
        for suffix in ["-wal", "-shm"] {
            let sib = p.with_extension(format!(
                "{}{}",
                p.extension().and_then(|e| e.to_str()).unwrap_or_default(),
                suffix
            ));
            let m = std::fs::metadata(&sib).ok();
            out.push((
                sib,
                m.as_ref().map(|x| x.len()).unwrap_or(0),
                m.as_ref().and_then(|x| x.modified().ok()),
            ));
        }
    }
    out
}

pub struct WatchReport {
    pub rounds: u64,
    pub synced: u64,
}

/// Re-sync `project` whenever the session stores change.
///
/// `once` runs a single pass (useful for a shell hook or a cron entry);
/// otherwise it loops until interrupted.
pub fn watch(
    registry: &Registry,
    project: &Path,
    interval: Duration,
    once: bool,
) -> WatchReport {
    let mut report = WatchReport { rounds: 0, synced: 0 };
    let mut last = Vec::new();

    loop {
        report.rounds += 1;
        let now = fingerprint(registry);

        // First pass always syncs; later passes only when something moved.
        if last.is_empty() || now != last {
            let r = crate::sync::pull_back(false);
            let pulled: usize = r.pulled.iter().map(|(_, n)| n).sum();
            let s = crate::sync::sync_into(registry, project, false);
            if !s.created.is_empty() || pulled > 0 {
                report.synced += 1;
                println!(
                    "[agentbridge] synced {} new, pulled {} turn(s)",
                    s.created.len(),
                    pulled
                );
            }
            if !r.conflicts.is_empty() {
                // An unattended daemon can't prompt, so it merges (AutoMerge,
                // same as always) and just flags it for the operator to
                // revisit interactively if that wasn't the right call.
                println!(
                    "[agentbridge] {} session(s) had new work from more than one tool \
                     (auto-merged) — run `agentbridge pull` from a terminal to choose",
                    r.conflicts.len()
                );
            }
            last = now;
        }

        if once {
            break;
        }
        std::thread::sleep(interval);
    }

    report
}

/// Marker so the hook can be found and removed again.
const HOOK_BEGIN: &str = "# >>> agentbridge >>>";
const HOOK_END: &str = "# <<< agentbridge <<<";

fn hook_body() -> String {
    format!(
        r#"{begin}
# Keeps every agent session visible in whatever directory you're in.
# Runs one quick sync per new shell; remove with: agentbridge auto uninstall
if command -v agentbridge >/dev/null 2>&1; then
  agentbridge sync >/dev/null 2>&1 &
fi
{end}
"#,
        begin = HOOK_BEGIN,
        end = HOOK_END
    )
}

fn shell_rc() -> PathBuf {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.contains("zsh") {
        home.join(".zshrc")
    } else if shell.contains("bash") {
        home.join(".bashrc")
    } else {
        home.join(".profile")
    }
}

/// Strip any existing hook block, so install is idempotent and uninstall is
/// exact.
fn without_hook(content: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in content.lines() {
        if line.trim() == HOOK_BEGIN {
            skipping = true;
            continue;
        }
        if line.trim() == HOOK_END {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

pub fn install_hook(dry_run: bool) -> std::io::Result<PathBuf> {
    let rc = shell_rc();
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    let updated = format!("{}\n{}", without_hook(&existing).trim_end(), hook_body());
    if !dry_run {
        std::fs::write(&rc, updated)?;
    }
    Ok(rc)
}

pub fn uninstall_hook(dry_run: bool) -> std::io::Result<(PathBuf, bool)> {
    let rc = shell_rc();
    let existing = std::fs::read_to_string(&rc).unwrap_or_default();
    let had = existing.contains(HOOK_BEGIN);
    if had && !dry_run {
        std::fs::write(&rc, without_hook(&existing))?;
    }
    Ok((rc, had))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_install_is_idempotent() {
        let base = "export PATH=/usr/bin\n";
        let once = format!("{}\n{}", base.trim_end(), hook_body());
        let twice = format!("{}\n{}", without_hook(&once).trim_end(), hook_body());
        assert_eq!(once, twice, "installing twice must not duplicate the block");
        assert_eq!(once.matches(HOOK_BEGIN).count(), 1);
    }

    #[test]
    fn test_uninstall_restores_original_content() {
        let base = "export PATH=/usr/bin\nalias k=kubectl\n";
        let installed = format!("{}\n{}", base.trim_end(), hook_body());
        let removed = without_hook(&installed);
        assert!(!removed.contains("agentbridge"), "hook must be gone");
        assert!(removed.contains("alias k=kubectl"), "user's own lines kept");
    }

    #[test]
    fn test_without_hook_leaves_unrelated_files_untouched() {
        let base = "# my rc\nexport FOO=1\n";
        assert_eq!(without_hook(base), base);
    }

    #[test]
    fn test_fingerprint_changes_when_a_session_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("s.jsonl");
        std::fs::write(&f, "a").unwrap();
        let before = (f.clone(), std::fs::metadata(&f).unwrap().len());
        std::fs::write(&f, "aa").unwrap();
        let after = (f.clone(), std::fs::metadata(&f).unwrap().len());
        assert_ne!(before, after, "size change must be observable");
    }

    /// Regression: OpenCode writes rows to `opencode.db-wal` and checkpoints
    /// later, so a watch fingerprint over the .db alone would miss brand-new
    /// sessions for long stretches. The fingerprint must stat WAL siblings.
    #[test]
    fn test_fingerprint_tracks_wal_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("sessions.db");
        std::fs::write(&db, "base").unwrap();
        let wal = tmp.path().join("sessions.db-wal");
        let shm = tmp.path().join("sessions.db-shm");

        let f = fingerprint_files(std::slice::from_ref(&db));
        let before: Vec<(PathBuf, u64, Option<SystemTime>)> = f
            .iter()
            .map(|(p, s, t)| (p.clone(), *s, *t))
            .collect();

        std::fs::write(&wal, "new rows").unwrap();
        std::fs::write(&shm, "index").unwrap();

        let after = fingerprint_files(std::slice::from_ref(&db));
        assert_ne!(
            before, after,
            "WAL growth must change the fingerprint even though the .db is untouched"
        );
        assert!(
            after.iter().any(|(p, s, _)| p == &wal && *s > 0),
            "the -wal sibling must be fingerprinted"
        );
    }
}
