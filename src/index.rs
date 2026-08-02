//! Discovery: find every agent session on the machine and index it.
//!
//! Per `DESIGN.md` Rule 1 this never copies a session body. An `IndexEntry` is
//! metadata plus a pointer at the file that already exists on disk; bodies are
//! streamed from that path on demand.

use crate::connector::Registry;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One discovered session. Cheap to build — connectors derive this from a
/// filename or the first record, without reading the whole transcript.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub id: String,
    pub provider: String,
    /// Working directory the session belongs to, read from *inside* the
    /// transcript. Never decoded from an encoded directory name.
    pub project_path: Option<PathBuf>,
    pub source_path: PathBuf,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_event_at: Option<chrono::DateTime<chrono::Utc>>,
    pub title: Option<String>,
    /// See `RawSession::source`.
    pub source: Option<String>,
}

/// Result of a discovery pass.
#[derive(Debug, Default)]
pub struct Index {
    pub entries: Vec<IndexEntry>,
    /// Sessions a connector reported but could not parse. Discovery is
    /// best-effort: one unreadable file must never abort the scan.
    pub errors: Vec<String>,
}

impl Index {
    /// Sessions grouped by provider, provider order stable for display.
    pub fn by_provider(&self) -> BTreeMap<&str, Vec<&IndexEntry>> {
        let mut out: BTreeMap<&str, Vec<&IndexEntry>> = BTreeMap::new();
        for e in &self.entries {
            out.entry(e.provider.as_str()).or_default().push(e);
        }
        out
    }

    /// Distinct project directories seen across all providers.
    pub fn project_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .entries
            .iter()
            .filter_map(|e| e.project_path.clone())
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }

    pub fn find(&self, id: &str) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

/// Scan every detected connector. Read-only: opens files for reading and
/// writes nothing anywhere.
pub fn discover(registry: &Registry) -> Index {
    let mut index = Index::default();

    for connector in registry.detected() {
        match connector.scan() {
            Ok(stream) => {
                for item in stream {
                    match item {
                        Ok(raw) => index.entries.push(IndexEntry {
                            id: raw.id,
                            provider: raw.provider,
                            project_path: raw.project_path,
                            source_path: raw.source_path,
                            started_at: raw.started_at,
                            last_event_at: raw.last_event_at,
                            title: raw.title,
                            source: raw.source,
                        }),
                        // A single malformed session is recorded and skipped;
                        // the rest of the scan continues.
                        Err(e) => index.errors.push(format!("{}: {}", connector.id(), e)),
                    }
                }
            }
            Err(e) => index
                .errors
                .push(format!("{}: scan failed: {}", connector.id(), e)),
        }
    }

    // Newest first — matches how every tool's own picker orders sessions.
    index
        .entries
        .sort_by_key(|e| std::cmp::Reverse(e.last_event_at));
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn test_discover_finds_sessions_from_all_providers() {
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let index = discover(&registry);

        assert!(!index.entries.is_empty(), "should discover fixture sessions");

        let providers = index.by_provider();
        assert!(
            providers.contains_key("claude-code"),
            "claude-code sessions should be discovered"
        );
        assert!(
            providers.contains_key("codex-cli"),
            "codex-cli sessions should be discovered"
        );
    }

    #[test]
    fn test_discover_records_project_dirs() {
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let index = discover(&registry);

        // Project paths come from inside the transcripts, so a session with no
        // cwd record simply has none — it must not be invented.
        let dirs = index.project_dirs();
        for d in &dirs {
            assert!(d.is_absolute(), "project dirs are absolute: {}", d.display());
        }
    }

    #[test]
    fn test_discover_is_sorted_newest_first() {
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let index = discover(&registry);

        let times: Vec<_> = index
            .entries
            .iter()
            .filter_map(|e| e.last_event_at)
            .collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(times, sorted, "entries must be newest-first");
    }

    #[test]
    fn test_bad_session_does_not_abort_discovery() {
        // The fixture set deliberately contains truncated / non-UTF-8 / empty
        // files. Discovery must still return the good ones.
        let registry = crate::connectors::all_for_testing(&fixture_root());
        let index = discover(&registry);
        assert!(
            index.entries.len() >= 2,
            "malformed fixtures must not abort the scan"
        );
    }
}
