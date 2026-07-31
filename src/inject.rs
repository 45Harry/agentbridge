use crate::connector::{ConnectorError, ConnectorResult, InjectTarget, Registry};
use crate::convert::build_cross_tool_brief;
use crate::model::Session;
use std::path::PathBuf;

pub fn inject_brief(
    registry: &Registry,
    target_provider: &str,
    sessions: &[Session],
    dry_run: bool,
) -> ConnectorResult<InjectTarget> {
    let brief = build_cross_tool_brief(sessions);

    let connector = registry
        .by_id(target_provider)
        .ok_or_else(|| ConnectorError::Other(anyhow::anyhow!("unknown provider: {}", target_provider)))?;

    let fenced = format!(
        "\n<!-- agentbridge:brief -->\n{}\n<!-- /agentbridge:brief -->\n",
        brief
    );

    connector.inject(&fenced, dry_run)
}

pub fn agentbridge_start(
    registry: &Registry,
    target_provider: &str,
    project_path: Option<&str>,
    dry_run: bool,
) -> ConnectorResult<InjectTarget> {
    let all_sessions: Vec<Session> = registry
        .all()
        .iter()
        .filter_map(|c| {
            let scan = c.scan().ok()?;
            let mut sessions: Vec<Session> = scan
                .filter_map(|r| r.ok())
                .filter(|raw| {
                    if let Some(ref pp) = raw.project_path {
                        if let Some(target) = project_path {
                            return pp.to_string_lossy().contains(target);
                        }
                    }
                    project_path.is_none()
                })
                .filter_map(|raw| c.load(&raw.id).ok())
                .collect();
            sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            Some(sessions)
        })
        .flatten()
        .collect();

    let connector = registry
        .by_id(target_provider)
        .ok_or_else(|| ConnectorError::Other(anyhow::anyhow!("unknown provider: {}", target_provider)))?;

    let brief = build_cross_tool_brief(&all_sessions);

    connector.inject(&brief, dry_run)
}

pub fn write_claude_code_startup_brief(brief: &str, claude_project_dir: &PathBuf) -> ConnectorResult<PathBuf> {
    let claude_instructions = claude_project_dir.join("CLAUDE.md");
    let marker_begin = "<!-- agentbridge:brief -->";
    let marker_end = "<!-- /agentbridge:brief -->";
    let content = format!(
        "{}\n{}\n{}",
        marker_begin,
        brief,
        marker_end,
    );

    let mut existing = String::new();
    if claude_instructions.exists() {
        existing = std::fs::read_to_string(&claude_instructions)
            .map_err(|e| ConnectorError::Io {
                path: claude_instructions.clone(),
                source: e,
            })?;
    }

    let new_content = if let Some(start) = existing.find(marker_begin) {
        if let Some(end) = existing.find(marker_end) {
            let end = end + marker_end.len();
            format!("{}{}{}", &existing[..start], content, &existing[end..])
        } else {
            format!("{}\n\n{}", existing, content)
        }
    } else {
        format!("{}\n\n{}", existing, content)
    };

    std::fs::write(&claude_instructions, &new_content)
        .map_err(|e| ConnectorError::Io {
            path: claude_instructions.clone(),
            source: e,
        })?;

    Ok(claude_instructions)
}
