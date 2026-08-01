use agentbridge::connector::Connector;
use agentbridge::connectors;
use agentbridge::convert::{ClaudeCodeConverter, CodexCliConverter, SessionConverter};
use agentbridge::model::Session;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "agentbridge", version, about = "Cross-tool session & memory bridge for AI coding agents")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List sessions from all providers
    #[command(name = "ls")]
    List {
        /// Filter by project path (substring match)
        #[arg(long)]
        project: Option<String>,

        /// Filter by provider
        #[arg(long)]
        provider: Option<String>,
    },

    /// Index sessions from all providers
    #[command(name = "index")]
    Index {
        /// Provider to index (default: all detected)
        #[arg(long)]
        provider: Option<String>,
    },

    /// Start an agent with cross-tool context injected
    #[command(name = "start")]
    Start {
        /// Target provider (claude-code, codex-cli, opencode)
        provider: String,

        /// Passthrough args to the agent
        #[arg(last = true)]
        passthrough: Vec<String>,

        /// Dry run (show what would be injected without writing)
        #[arg(long)]
        dry_run: bool,
    },

    /// Discover every agent session on this machine (read-only)
    #[command(name = "init")]
    Init,

    /// Make every session on the machine visible in a directory, for every
    /// detected tool
    #[command(name = "sync")]
    Sync {
        /// Directory to surface sessions in (defaults to cwd)
        #[arg(long)]
        project: Option<String>,

        /// Show what would happen without writing anything
        #[arg(long)]
        dry_run: bool,
    },

    /// Recover turns other tools appended to synced sessions
    #[command(name = "pull")]
    Pull {
        /// Show what would be recovered without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Keep sessions synced automatically
    #[command(name = "auto")]
    Auto {
        #[command(subcommand)]
        action: AutoAction,
    },

    /// Show drift between what agentbridge wrote and what is on disk now
    #[command(name = "status")]
    Status,

    /// Remove exactly the files agentbridge created
    #[command(name = "unsync")]
    Unsync {
        /// Show what would be removed without removing it
        #[arg(long)]
        dry_run: bool,
    },

    /// Resume a session across tools
    #[command(name = "resume")]
    Resume {
        /// Session ID to resume
        session_id: String,

        /// Target provider to resume in
        target: String,

        /// Project path override
        #[arg(long)]
        project: Option<String>,

        /// Dry run (show what would happen without doing it)
        #[arg(long)]
        dry_run: bool,
    },

    /// Inject session context into an agent's startup
    #[command(name = "inject")]
    Inject {
        /// Target provider
        provider: String,

        /// Session IDs to include
        #[arg(required = true)]
        session_ids: Vec<String>,

        /// Dry run
        #[arg(long)]
        dry_run: bool,
    },

    /// Show information about detected connectors
    #[command(name = "info")]
    Info,
}

#[derive(Subcommand)]
enum AutoAction {
    /// Add a shell hook so every new terminal syncs automatically
    Install {
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the shell hook
    Uninstall {
        #[arg(long)]
        dry_run: bool,
    },
    /// Watch for changes and re-sync as they happen
    Watch {
        /// Directory to keep synced (defaults to cwd)
        #[arg(long)]
        project: Option<String>,
        /// Seconds between checks
        #[arg(long, default_value = "30")]
        interval: u64,
        /// Run a single pass and exit
        #[arg(long)]
        once: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let registry = connectors::all();

    match cli.command {
        Commands::List { project, provider } => cmd_list(&registry, project, provider),
        Commands::Index { provider } => cmd_index(&registry, provider),
        Commands::Start { provider, passthrough, dry_run } => {
            cmd_start(&registry, &provider, &passthrough, dry_run)
        }
        Commands::Resume { session_id, target, project, dry_run } => {
            cmd_resume(&registry, &session_id, &target, project.as_deref(), dry_run)
        }
        Commands::Inject { provider, session_ids, dry_run } => {
            cmd_inject(&registry, &provider, &session_ids, dry_run)
        }
        Commands::Init => cmd_init(&registry),
        Commands::Sync { project, dry_run } => cmd_sync(&registry, project.as_deref(), dry_run),
        Commands::Pull { dry_run } => cmd_pull(dry_run),
        Commands::Auto { action } => cmd_auto(&registry, action),
        Commands::Status => cmd_status(),
        Commands::Unsync { dry_run } => cmd_unsync(dry_run),
        Commands::Info => cmd_info(&registry),
    }
}

fn cmd_info(registry: &agentbridge::connector::Registry) {
    println!("agentbridge v{}", env!("CARGO_PKG_VERSION"));
    println!("Connectors registered: {}", registry.all().len());
    println!();
    for c in registry.all() {
        let detected = if c.detect() { "✓" } else { "✗" };
        println!("  {} {} ({})", detected, c.display_name(), c.id());
        for root in c.roots() {
            println!("         {}", root.display());
        }
    }
}

fn cmd_list(registry: &agentbridge::connector::Registry, project: Option<String>, provider: Option<String>) {
    let connectors: Vec<_> = match provider {
        Some(ref p) => {
            vec![registry.by_id(p)]
        }
        None => registry.all().iter().map(|c| Some(c.as_ref())).collect(),
    };

    for c_opt in connectors {
        let c = match c_opt {
            Some(c) => c,
            None => continue,
        };
        if !c.detect() {
            continue;
        }
        println!("[{}]", c.display_name());
        let scan = match c.scan() {
            Ok(s) => s,
            Err(_) => {
                println!("  (scan failed)");
                continue;
            }
        };
        let mut count = 0;
        for result in scan {
            let raw = match result {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let Some(ref proj_filter) = project {
                if let Some(ref pp) = raw.project_path {
                    if !pp.to_string_lossy().contains(proj_filter) {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            count += 1;
            let title = raw.title.as_deref().unwrap_or("(untitled)");
            let proj = raw
                .project_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "(unknown)".to_string());
            let started = raw
                .started_at
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "(unknown)".to_string());
            println!("  {} | {} | {} | {}", raw.id, proj, started, title);
        }
        if count == 0 {
            println!("  (no sessions)");
        }
        println!();
    }
}

fn cmd_index(registry: &agentbridge::connector::Registry, provider: Option<String>) {
    let connectors: Vec<&dyn Connector> = match provider {
        Some(ref p) => {
            registry.by_id(p).map(|c| vec![c]).unwrap_or_default()
        }
        None => registry.detected().collect(),
    };

    let mut total = 0;
    for c in &connectors {
        println!("Indexing {}...", c.display_name());
        let scan = match c.scan() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  Error scanning {}: {}", c.id(), e);
                continue;
            }
        };
        let mut count = 0;
        for result in scan {
            let raw = match result {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("  Error on session: {}", e);
                    continue;
                }
            };
            if raw.body_available {
                match c.load(&raw.id) {
                    Ok(session) => {
                        count += 1;
                        println!("  ✓ {} ({} msgs)", raw.id, session.messages.len());
                    }
                    Err(e) => {
                        eprintln!("  ✗ {} load failed: {}", raw.id, e);
                    }
                }
            } else {
                println!("  - {} (metadata only)", raw.id);
            }
        }
        println!("  → {} sessions from {}\n", count, c.display_name());
        total += count;
    }
    println!("Indexed {} sessions total.", total);
}

fn cmd_start(
    registry: &agentbridge::connector::Registry,
    provider: &str,
    _passthrough: &[String],
    dry_run: bool,
) {
    let connector = match registry.by_id(provider) {
        Some(c) => c,
        None => {
            eprintln!("Unknown provider: {}. Use: {}", provider,
                registry.all().iter().map(|c| c.id()).collect::<Vec<_>>().join(", "));
            return;
        }
    };

    let mut all_sessions: Vec<Session> = Vec::new();
    for c in registry.detected() {
        let scan = match c.scan() {
            Ok(s) => s,
            Err(_) => continue,
        };
        for result in scan {
            let raw = match result {
                Ok(r) => r,
                Err(_) => continue,
            };
            if raw.body_available {
                if let Ok(session) = c.load(&raw.id) {
                    all_sessions.push(session);
                }
            }
        }
    }

    all_sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));

    let brief = agentbridge::convert::build_cross_tool_brief(&all_sessions);

    if dry_run {
        println!("[dry-run] Would inject brief into {}:", provider);
        println!("{}", brief);
        println!();
        match connector.inject(&brief, true) {
            Ok(target) => {
                println!("Would write to: {}", target.path.display());
            }
            Err(e) => {
                println!("Inject target resolution: {} (expected during dry-run)", e);
            }
        }
        return;
    }

    match connector.inject(&brief, false) {
        Ok(target) => {
            println!("Injected brief into {} at {}", provider, target.path.display());
            if let Some((start, end)) = target.fenced_range {
                println!("  Fenced block: bytes [{}, {})", start, end);
            }
        }
        Err(e) => {
            eprintln!("Failed to inject into {}: {}", provider, e);
        }
    }
}

fn cmd_resume(
    registry: &agentbridge::connector::Registry,
    session_id: &str,
    target: &str,
    project: Option<&str>,
    dry_run: bool,
) {
    let source_session = find_session(registry, session_id);
    let mut session = match source_session {
        Some(s) => s,
        None => {
            eprintln!("Session '{}' not found in any provider.", session_id);
            return;
        }
    };

    // `--project` re-homes the session into another working directory. Both
    // Claude Code and Codex scope resume to the cwd you launch them from, so
    // without this you can only resume a session from the directory it was
    // originally created in.
    if let Some(p) = project {
        match std::fs::canonicalize(p) {
            Ok(abs) => {
                session.project_id = abs.to_string_lossy().to_string();
            }
            Err(e) => {
                eprintln!("--project '{}' could not be resolved: {}", p, e);
                return;
            }
        }
    }

    println!("Resuming session {} from {} into {}...", session.id, session.provider, target);

    let target_dirs = match target {
        "claude-code" => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            vec![PathBuf::from(&home).join(".claude").join("projects")]
        }
        "codex-cli" => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            vec![PathBuf::from(&home).join(".codex")]
        }
        "opencode" => {
            let data_dir = std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                    PathBuf::from(&home).join(".local").join("share").join("opencode")
                });
            vec![data_dir]
        }
        _ => {
            eprintln!("Unknown target provider: {}", target);
            return;
        }
    };

    let Some(target_dir) = target_dirs.first() else {
        eprintln!("Could not determine target directory for {}", target);
        return;
    };

    if dry_run {
        println!("[dry-run] Would copy session to {} directory", target);
        println!("  Source: {} (from {})", session.id, session.provider);
        println!("  Target: {} ({})", target, target_dir.display());
        println!("  Project path: {}", session.project_path().unwrap_or_else(|| "(none)".to_string()));
        println!("  Messages: {}", session.messages.len());
        return;
    }

    let result: Result<PathBuf, String> = match target {
        "claude-code" => {
            let converter = ClaudeCodeConverter::new();
            converter.convert(&session, target_dir)
        }
        "codex-cli" => {
            let converter = CodexCliConverter::new();
            converter.convert(&session, target_dir)
        }
        "opencode" => {
            let db = target_dir.join("opencode.db");
            match agentbridge::opencode_write::ensure_safe_to_write() {
                Err(e) => Err(e.to_string()),
                Ok(()) => {
                    let backed = agentbridge::opencode_write::backup(&db);
                    if let Err(e) = backed {
                        Err(format!("opencode backup failed: {}", e))
                    } else {
                        let dir = session.project_path().unwrap_or_default();
                        match agentbridge::opencode_write::write_session(&db, &session, &dir) {
                            Ok((id, _)) => Ok(PathBuf::from(id)),
                            Err(e) => Err(e.to_string()),
                        }
                    }
                }
            }
        }
        _ => unreachable!(),
    };

    match result {
        Ok(path) => {
            let prev_provider = &session.provider;
            println!("✓ Session '{}' (from {}) copied to {} format", session.id, prev_provider, target);
            println!("  → {}", path.display());
            // Derive the command from the file actually written — the target
            // id can differ from the source id (non-UUID ids get a fresh one).
            let cmd = match target {
                "claude-code" => ClaudeCodeConverter::new().resume_cmd(&path),
                "codex-cli" => CodexCliConverter::new().resume_cmd(&path),
                "opencode" => vec![
                    "opencode".to_string(),
                    "run".to_string(),
                    "--session".to_string(),
                    path.to_string_lossy().to_string(),
                ],
                _ => vec![],
            };
            if let Some(cwd) = session.project_path() {
                println!("  Run from {}:", cwd);
                println!("    {}", cmd.join(" "));
            } else {
                println!("  Run: {}", cmd.join(" "));
            }
        }
        Err(e) => {
            eprintln!("Failed to convert/resume session: {}", e);
        }
    }
}

fn cmd_inject(
    registry: &agentbridge::connector::Registry,
    provider: &str,
    session_ids: &[String],
    dry_run: bool,
) {
    let sessions: Vec<Session> = session_ids
        .iter()
        .filter_map(|id| find_session(registry, id))
        .collect();

    if sessions.is_empty() {
        eprintln!("No sessions found for the given IDs.");
        return;
    }

    let brief = agentbridge::convert::build_cross_tool_brief(&sessions);

    let connector = match registry.by_id(provider) {
        Some(c) => c,
        None => {
            eprintln!("Unknown provider: {}", provider);
            return;
        }
    };

    if dry_run {
        println!("[dry-run] Would inject brief into {}:", provider);
        println!("{}", brief);
        match connector.inject(&brief, true) {
            Ok(target) => {
                println!("Would write to: {}", target.path.display());
            }
            Err(e) => {
                println!("Inject target resolution: {}", e);
            }
        }
        return;
    }

    match connector.inject(&brief, false) {
        Ok(target) => {
            println!("✓ Injected brief into {} at {}", provider, target.path.display());
        }
        Err(e) => {
            eprintln!("Failed to inject: {}", e);
        }
    }
}

fn find_session(registry: &agentbridge::connector::Registry, session_id: &str) -> Option<Session> {
    for c in registry.all() {
        if !c.detect() {
            continue;
        }
        let scan = c.scan().ok()?;
        for result in scan {
            let raw = result.ok()?;
            if raw.id == session_id || raw.id.contains(session_id) || session_id.contains(&raw.id) {
                if raw.body_available {
                    return c.load(&raw.id).ok();
                }
            }
        }
    }
    None
}

/// Zero-config discovery: find every agent session on this machine.
/// Read-only — writes nothing anywhere (DESIGN.md §8).
fn cmd_init(registry: &agentbridge::connector::Registry) {
    println!("scanning…");
    let index = agentbridge::index::discover(registry);

    for c in registry.all() {
        let name = c.display_name();
        if !c.detect() {
            println!("  {:<14}— not detected", name);
            continue;
        }
        let n = index.entries.iter().filter(|e| e.provider == c.id()).count();
        let root = c
            .roots()
            .first()
            .map(|r| r.display().to_string())
            .unwrap_or_default();
        println!("  {:<14}{:>4} sessions   {}", name, n, root);
    }

    println!();
    println!(
        "indexed {} sessions across {} tools, {} project directories",
        index.entries.len(),
        index.by_provider().len(),
        index.project_dirs().len()
    );
    if !index.errors.is_empty() {
        println!("{} session(s) could not be read (skipped)", index.errors.len());
    }
}

fn cmd_sync(registry: &agentbridge::connector::Registry, project: Option<&str>, dry_run: bool) {
    let dir = match project {
        Some(p) => match std::fs::canonicalize(p) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("--project '{}' could not be resolved: {}", p, e);
                return;
            }
        },
        None => std::env::current_dir().unwrap_or_default(),
    };

    // Recover anything other tools appended before re-materializing, so the
    // refreshed copies carry it.
    let pulled = agentbridge::sync::pull_back(dry_run);
    let n: usize = pulled.pulled.iter().map(|(_, n)| n).sum();
    if n > 0 {
        println!(
            "  pulled    {} new turn(s) from {} session(s) worked on elsewhere",
            n,
            pulled.pulled.len()
        );
    }

    println!("Surfacing all machine sessions in {}", dir.display());
    let report = agentbridge::sync::sync_into(registry, &dir, dry_run);

    if dry_run {
        println!("[dry-run] would materialize {} session(s); nothing written", report.created.len());
    } else {
        println!("  created   {}", report.created.len());
        println!("  unchanged {}", report.unchanged);
    }
    if report.skipped_native > 0 {
        println!("  skipped   {} (already native here)", report.skipped_native);
    }
    for e in report.errors.iter().take(10) {
        eprintln!("  ! {}", e);
    }
    if !dry_run && !report.created.is_empty() {
        println!();
        println!("Open any tool here — its own session picker now lists them.");
        println!("Undo with: agentbridge unsync");
    }
}

fn cmd_unsync(dry_run: bool) {
    let report = agentbridge::sync::unsync(dry_run);
    if dry_run {
        println!("[dry-run] would remove {} file(s)", report.removed.len());
    } else {
        println!("removed {} file(s)", report.removed.len());
    }
    if !report.kept_foreign.is_empty() {
        println!(
            "kept {} file(s) that no longer match what agentbridge created",
            report.kept_foreign.len()
        );
    }
    if report.missing > 0 {
        println!("{} already gone", report.missing);
    }
}

fn cmd_pull(dry_run: bool) {
    let report = agentbridge::sync::pull_back(dry_run);
    let total: usize = report.pulled.iter().map(|(_, n)| n).sum();

    if report.pulled.is_empty() {
        println!("No new turns — nothing was continued in another tool.");
    } else if dry_run {
        println!("[dry-run] would recover {} turn(s):", total);
    } else {
        println!("Recovered {} turn(s):", total);
    }
    for (id, n) in &report.pulled {
        println!("  {:<40} +{} turn(s)", id, n);
    }
    for e in report.errors.iter().take(10) {
        eprintln!("  ! {}", e);
    }
    if !dry_run && total > 0 {
        println!();
        println!("Run `agentbridge sync` to push these to every other tool.");
    }
}

fn cmd_status() {
    let rows = agentbridge::sync::status();
    if rows.is_empty() {
        println!("Nothing synced. Run `agentbridge sync`.");
        return;
    }
    println!("{:<38} {:<12} {:>8} {:>8} {:>7}", "SESSION", "TARGET", "WROTE", "ON DISK", "NEW");
    let mut drifted = 0;
    for r in &rows {
        let actual = r.actual.map(|n| n.to_string()).unwrap_or_else(|| {
            if r.exists { "unreadable".into() } else { "gone".into() }
        });
        let d = r.drift();
        if d > 0 {
            drifted += 1;
        }
        println!(
            "{:<38} {:<12} {:>8} {:>8} {:>7}",
            &r.session_id[..r.session_id.len().min(36)],
            r.target_provider,
            r.expected,
            actual,
            if d > 0 { format!("+{}", d) } else { "-".to_string() }
        );
    }
    println!();
    println!("{} file(s) tracked, {} with new turns to pull", rows.len(), drifted);
}

fn cmd_auto(registry: &agentbridge::connector::Registry, action: AutoAction) {
    match action {
        AutoAction::Install { dry_run } => match agentbridge::auto::install_hook(dry_run) {
            Ok(rc) => {
                if dry_run {
                    println!("[dry-run] would add the agentbridge hook to {}", rc.display());
                } else {
                    println!("Installed the agentbridge hook in {}", rc.display());
                    println!("New terminals will sync automatically. Undo: agentbridge auto uninstall");
                }
            }
            Err(e) => eprintln!("Could not update your shell rc: {}", e),
        },
        AutoAction::Uninstall { dry_run } => match agentbridge::auto::uninstall_hook(dry_run) {
            Ok((rc, had)) => {
                if !had {
                    println!("No agentbridge hook found in {}", rc.display());
                } else if dry_run {
                    println!("[dry-run] would remove the hook from {}", rc.display());
                } else {
                    println!("Removed the agentbridge hook from {}", rc.display());
                }
            }
            Err(e) => eprintln!("Could not update your shell rc: {}", e),
        },
        AutoAction::Watch { project, interval, once } => {
            let dir = match project {
                Some(p) => std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p)),
                None => std::env::current_dir().unwrap_or_default(),
            };
            if once {
                agentbridge::auto::watch(registry, &dir, std::time::Duration::from_secs(interval), true);
            } else {
                println!("Watching for session changes in {} (every {}s). Ctrl-C to stop.", dir.display(), interval);
                agentbridge::auto::watch(registry, &dir, std::time::Duration::from_secs(interval), false);
            }
        }
    }
}
