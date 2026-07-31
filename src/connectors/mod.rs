//! Registration point for every connector. This is the **one** file (besides
//! the new connector's own file) that changes when a provider is added — see
//! `crate::connector` for the interface contract.

use crate::connector::{Connector, Registry};

pub(crate) mod claude_code;
pub(crate) mod codex_cli;

pub fn all() -> Registry {
    let connectors: Vec<Box<dyn Connector>> = vec![
        Box::new(claude_code::ClaudeCodeConnector::new()),
        Box::new(codex_cli::CodexCliConnector::new()),
    ];
    Registry::new(connectors)
}

#[cfg(test)]
pub(crate) fn all_for_testing(fixture_root: &std::path::Path) -> Registry {
    let mut connectors: Vec<Box<dyn Connector>> = vec![];

    let cc_root = fixture_root.join("claude-code");
    if cc_root.exists() {
        connectors.push(Box::new(claude_code::TestClaudeCode::new(cc_root)));
    }

    let cx_root = fixture_root.join("codex-cli");
    if cx_root.exists() {
        connectors.push(Box::new(codex_cli::TestCodexCli::new(cx_root)));
    }

    Registry::new(connectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn fixture_root() -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests");
        p.push("fixtures");
        p
    }

    #[test]
    fn test_claude_code_scan_finds_all_sessions() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let results: Vec<_> = cc.scan().unwrap().filter_map(|r| r.ok()).collect();

        let names: std::collections::BTreeSet<&str> = results
            .iter()
            .map(|r| r.id.as_str())
            .collect();

        assert!(names.contains("normal-multi-turn"), "should find normal-multi-turn");
        assert!(names.contains("tool-calls-large-output"), "should find tool-calls-large-output");
        assert!(names.contains("compacted-session"), "should find compacted-session");
        assert!(names.contains("embedded-secret"), "should find embedded-secret");
        assert!(names.contains("legacy-timestamp"), "should find legacy-timestamp");
        assert!(names.contains("non-utf8"), "should find non-utf8");
        assert!(names.contains("truncated-final-line"), "should find truncated-final-line");
    }

    #[test]
    fn test_claude_code_scan_skips_empty_file() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let results: Vec<_> = cc.scan().unwrap().filter_map(|r| r.ok()).collect();
        let has_empty = results.iter().any(|r| r.id == "empty-file");
        assert!(!has_empty, "empty file should be skipped");
    }

    #[test]
    fn test_claude_code_load_normal_session() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("normal-multi-turn").unwrap();

        assert_eq!(session.id, "normal-multi-turn");
        assert_eq!(session.provider, "claude-code");
        assert!(session.messages.len() >= 5, "should have at least 5 messages, got {}", session.messages.len());

        let first_user = &session.messages[0];
        assert_eq!(first_user.role, Role::User);

        let last_msg = session.messages.last().unwrap();
        assert_eq!(last_msg.role, Role::Tool);
    }

    #[test]
    fn test_claude_code_load_tool_calls() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("tool-calls-large-output").unwrap();

        let tool_msgs: Vec<_> = session.messages.iter().filter(|m| m.tool_name.is_some()).collect();
        assert!(tool_msgs.len() >= 2, "should have tool calls, got {}", tool_msgs.len());

        let tool_result = session.messages.iter().find(|m| m.role == Role::Tool);
        assert!(tool_result.is_some(), "should have tool result messages");
    }

    #[test]
    fn test_claude_code_load_legacy_timestamps() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("legacy-timestamp").unwrap();

        assert!(session.started_at.is_some(), "should parse epoch timestamp");
        assert!(session.messages.len() >= 2, "should have messages, got {}", session.messages.len());
    }

    #[test]
    fn test_claude_code_load_truncated_handles_gracefully() {
        let registry = all_for_testing(&fixture_root());
        let cc = registry.by_id("claude-code").unwrap();
        let session = cc.load("truncated-final-line").unwrap();

        assert!(session.messages.len() >= 2, "should parse messages before truncation, got {}",
            session.messages.len());
    }

    #[test]
    fn test_codex_cli_scan_finds_sessions() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let results: Vec<_> = cx.scan().unwrap().filter_map(|r| r.ok()).collect();

        assert!(results.len() >= 5, "should find at least 5 codex sessions, got {}", results.len());

        let has_tool_session = results.iter().any(|r| r.title.as_deref() == Some("Refactor database layer"));
        assert!(has_tool_session, "should find the refactor session");
    }

    #[test]
    fn test_codex_cli_load_session() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let session = cx.load("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap();

        assert_eq!(session.provider, "codex-cli");
        assert!(session.messages.len() >= 3, "should have messages, got {}", session.messages.len());
    }

    #[test]
    fn test_codex_cli_load_legacy_timestamps() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let session = cx.load("cccccccc-cccc-cccc-cccc-cccccccccccc").unwrap();

        assert!(session.started_at.is_some(), "should parse epoch timestamp");
        assert!(session.messages.len() >= 1, "should have messages");
    }

    #[test]
    fn test_codex_cli_scan_skips_empty() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let results: Vec<_> = cx.scan().unwrap().filter_map(|r| r.ok()).collect();

        let has_empty = results.iter().any(|r| r.id == "empty-session");
        assert!(!has_empty, "empty file should be skipped");
    }

    #[test]
    fn test_codex_cli_load_not_found() {
        let registry = all_for_testing(&fixture_root());
        let cx = registry.by_id("codex-cli").unwrap();
        let result = cx.load("nonexistent-id-12345");
        assert!(result.is_err(), "should error on unknown id");
    }

    #[test]
    fn test_detect_on_fixtures() {
        let registry = all_for_testing(&fixture_root());
        let detected: Vec<&str> = registry.detected().map(|c| c.id()).collect();
        assert!(detected.contains(&"claude-code"), "claude-code should be detected");
        assert!(detected.contains(&"codex-cli"), "codex-cli should be detected");
    }

    #[test]
    fn test_scan_err_does_not_abort_whole_scan() {
        use crate::connector::Connector;
        let cx_root = fixture_root().join("codex-cli");
        let connector = codex_cli::TestCodexCli::new(cx_root);
        let results: Vec<_> = connector.scan().unwrap().collect();
        let err_count = results.iter().filter(|r| r.is_err()).count();
        let ok_count = results.iter().filter(|r| r.is_ok()).count();
        assert!(ok_count >= 5, "should have successes despite any errors: {}", ok_count);
        assert_eq!(err_count, 0, "should have no errors in scan");
    }
}
