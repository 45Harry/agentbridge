use agentbridge::connectors;
use agentbridge::convert::{ClaudeCodeConverter, CodexCliConverter, SessionConverter};
use agentbridge::model::Session;
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

mod dashboard;
mod tui;

#[derive(Parser)]
#[command(name = "agentbridge", version, about = "Cross-tool session & memory bridge for AI coding agents")]
struct Cli {
    /// Write copies without redacting secrets (API keys, tokens, passwords).
    /// Interactive commands only: refused for `auto watch`.
    #[arg(long, global = true)]
    no_redact: bool,

    /// Open the interactive dashboard when omitted
    #[command(subcommand)]
    command: Option<Commands>,
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

    /// Build or refresh the search index (incremental; safe to run any time)
    #[command(name = "index")]
    Index {
        /// Only this provider (default: all detected)
        #[arg(long)]
        provider: Option<String>,

        /// Throw the index away and rebuild it from scratch
        #[arg(long)]
        rebuild: bool,
    },

    /// Search every tool's session history
    #[command(name = "search")]
    Search {
        /// Words to look for (all must appear in the same message)
        #[arg(required = true)]
        query: Vec<String>,

        /// Only sessions under this directory
        #[arg(long)]
        project: Option<String>,

        /// Only this provider
        #[arg(long)]
        provider: Option<String>,

        /// Only messages newer than this: 90m, 12h, 7d, 2w or YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,

        /// Most results to show
        #[arg(long, default_value = "10")]
        limit: usize,
    },

    /// Print a compact, cited summary of a project's history across all tools
    #[command(name = "brief")]
    Brief {
        /// Project directory (defaults to cwd)
        #[arg(long)]
        project: Option<String>,

        /// Only sessions newer than this: 90m, 12h, 7d, 2w or YYYY-MM-DD
        #[arg(long)]
        since: Option<String>,

        /// Token budget for the brief (measured on the final text)
        #[arg(long, default_value = "2000")]
        budget: usize,

        /// Use the index as it is instead of refreshing it first
        #[arg(long)]
        no_refresh: bool,

        /// Condense the brief with a model you choose: a command that reads a
        /// prompt on stdin and writes the answer on stdout (e.g. "claude -p").
        /// Opt-in. agentbridge makes no network call itself; the redacted brief
        /// is handed to this command. A summary that is not faithfully cited is
        /// discarded in favour of the plain brief.
        #[arg(long)]
        llm_cmd: Option<String>,
    },

    /// Run the MCP server on stdio, so an agent can search and summarise your
    /// other tools' sessions (tools: search_history, get_brief, get_session,
    /// record_fact)
    #[command(name = "mcp")]
    Mcp,

    /// Record a durable fact about a project; it appears in its brief
    #[command(name = "fact")]
    Fact {
        /// The fact, in one or two sentences
        #[arg(required = true)]
        text: Vec<String>,

        /// Project directory (defaults to cwd)
        #[arg(long)]
        project: Option<String>,

        /// Tag (repeatable)
        #[arg(long = "tag")]
        tags: Vec<String>,
    },

    /// Start an agent with context from your other tools' sessions injected
    #[command(name = "start")]
    Start {
        /// Agent to start (claude-code, codex-cli, opencode, antigravity)
        provider: String,

        /// Arguments passed through to the agent
        #[arg(last = true)]
        passthrough: Vec<String>,

        /// Project directory (defaults to cwd)
        #[arg(long)]
        project: Option<String>,

        /// Token budget for the injected brief
        #[arg(long, default_value = "1500")]
        budget: usize,

        /// Use the index as it is instead of refreshing it first
        #[arg(long)]
        no_refresh: bool,

        /// Inject the context but do not launch the agent
        #[arg(long)]
        no_launch: bool,

        /// Show what would be injected without writing or launching
        #[arg(long)]
        dry_run: bool,
    },

    /// Discover every agent session on this machine (read-only)
    #[command(name = "init")]
    Init,

    /// Open the interactive dashboard TUI (same as running bare `agentbridge`)
    #[command(name = "tui", visible_alias = "dashboard")]
    Tui,

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

        /// Sync every project directory your sessions have ever used, not just
        /// one. Slow on a big history, and OpenCode gets a copy of each session
        /// per project it knows (database growth); needs --dry-run or --yes.
        #[arg(long, conflicts_with = "project")]
        all_known: bool,

        /// Confirm `--all-known` (it is not asked interactively)
        #[arg(long)]
        yes: bool,
    },

    /// Recover turns other tools appended to synced sessions
    #[command(name = "pull")]
    Pull {
        /// Show what would be recovered without writing
        #[arg(long)]
        dry_run: bool,

        /// Skip the interactive prompt when a session has new work from more
        /// than one tool — keep every tool's contribution, same as before
        /// this prompt existed. Implied by --dry-run and by a non-TTY stdin.
        #[arg(long)]
        auto_merge: bool,
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

        /// Instead, remove generated files the manifest no longer tracks
        /// (found by the marker inside them). Files with turns added since
        /// agentbridge wrote them are kept.
        #[arg(long)]
        orphans: bool,
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

        /// Merge turns back into the origin file (skip the prompt)
        #[arg(long, conflicts_with = "copy")]
        merge: bool,

        /// Copy only — leave the origin file untouched (skip the prompt)
        #[arg(long, conflicts_with = "merge")]
        copy: bool,
    },

    /// Inject chosen sessions' context into an agent's startup file
    #[command(name = "inject")]
    Inject {
        /// Target provider
        provider: String,

        /// Session IDs to include
        #[arg(required = true)]
        session_ids: Vec<String>,

        /// Project directory (defaults to cwd)
        #[arg(long)]
        project: Option<String>,

        /// Dry run
        #[arg(long)]
        dry_run: bool,
    },

    /// Remove the context agentbridge injected, restoring the files exactly
    #[command(name = "clean")]
    Clean {
        /// Project directory (defaults to cwd)
        #[arg(long)]
        project: Option<String>,

        /// Also clean every project agentbridge has ever injected into
        #[arg(long)]
        all: bool,

        /// Show what would be removed without changing anything
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

    if cli.no_redact {
        // The shell hook and the watch daemon run unattended; nobody is there
        // to have meant this, so they can never skip redaction.
        if matches!(
            cli.command,
            Some(Commands::Auto { action: AutoAction::Watch { .. } })
        ) {
            eprintln!("--no-redact is not allowed with `auto watch` (it runs unattended)");
            std::process::exit(2);
        }
        agentbridge::redact::disable_for_this_process();
        eprintln!("warning: --no-redact: secrets in these sessions will be copied as-is");
    }

    match cli.command {
        None => {
            // Bare `agentbridge` = the dashboard, but only where a TUI can
            // actually render; a piped/cron run must never enter full-screen
            // mode. Non-TTY falls back to the static help, like a missing
            // subcommand used to.
            if std::io::stdin().is_terminal() {
                let mut dash = crate::dashboard::Dashboard::new();
                if let Err(e) = dash.run() {
                    eprintln!("dashboard: {}", e);
                    std::process::exit(1);
                }
            } else {
                use clap::CommandFactory;
                let _ = Cli::command().print_help();
            }
        }
        Some(Commands::List { project, provider }) => cmd_list(&registry, project, provider),
        Some(Commands::Index { provider, rebuild }) => cmd_index(&registry, provider.as_deref(), rebuild),
        Some(Commands::Search { query, project, provider, since, limit }) => {
            cmd_search(&query.join(" "), project.as_deref(), provider, since.as_deref(), limit)
        }
        Some(Commands::Brief { project, since, budget, no_refresh, llm_cmd }) => {
            cmd_brief(&registry, project.as_deref(), since.as_deref(), budget, no_refresh, llm_cmd.as_deref())
        }
        Some(Commands::Fact { text, project, tags }) => cmd_fact(&text.join(" "), project.as_deref(), tags),
        Some(Commands::Mcp) => cmd_mcp(),
        Some(Commands::Start { provider, passthrough, project, budget, no_refresh, no_launch, dry_run }) => {
            cmd_start(&registry, &provider, &passthrough, project.as_deref(), budget, no_refresh, no_launch, dry_run)
        }
        Some(Commands::Resume { session_id, target, project, dry_run, merge, copy }) => {
            cmd_resume(&registry, &session_id, &target, project.as_deref(), dry_run, merge, copy)
        }
        Some(Commands::Inject { provider, session_ids, project, dry_run }) => {
            cmd_inject(&registry, &provider, &session_ids, project.as_deref(), dry_run)
        }
        Some(Commands::Clean { project, all, dry_run }) => {
            cmd_clean(&registry, project.as_deref(), all, dry_run)
        }
        Some(Commands::Init) => cmd_init(&registry),
        Some(Commands::Tui) => {
            if std::io::stdin().is_terminal() {
                let mut dash = crate::dashboard::Dashboard::new();
                if let Err(e) = dash.run() {
                    eprintln!("dashboard: {}", e);
                    std::process::exit(1);
                }
            } else {
                eprintln!("the dashboard needs a terminal — run it from a shell");
                std::process::exit(1);
            }
        }
        Some(Commands::Sync { project, dry_run, all_known, yes }) => {
            cmd_sync(&registry, project.as_deref(), dry_run, all_known, yes)
        }
        Some(Commands::Pull { dry_run, auto_merge }) => cmd_pull(dry_run, auto_merge),
        Some(Commands::Auto { action }) => cmd_auto(&registry, action),
        Some(Commands::Status) => cmd_status(&registry),
        Some(Commands::Unsync { dry_run, orphans }) => cmd_unsync(&registry, dry_run, orphans),
        Some(Commands::Info) => cmd_info(&registry),
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

fn open_index_or_exit() -> agentbridge::store::Store {
    match agentbridge::store::Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agentbridge: {e}");
            std::process::exit(1);
        }
    }
}

fn redactor_or_exit() -> agentbridge::redact::Redactor {
    match agentbridge::redact::Redactor::load() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("redaction: {e} — nothing was indexed");
            std::process::exit(2);
        }
    }
}

/// Bring the index up to date, telling the operator when it is the slow first
/// build. Progress goes to stderr so stdout stays clean for the brief or search.
fn refresh_index(
    registry: &agentbridge::connector::Registry,
    store: &mut agentbridge::store::Store,
    provider: Option<&str>,
) -> agentbridge::store::RefreshStats {
    let redactor = redactor_or_exit();
    let first = store.counts().map(|(s, _)| s == 0).unwrap_or(false);
    if first {
        eprintln!("Building the index (one time; later runs only read what changed)…");
    }
    let interactive = std::io::stderr().is_terminal();
    let stats = agentbridge::store::refresh(store, registry, &redactor, provider, |st| {
        if interactive && first {
            eprint!("\r  {} indexed, {} unchanged…", st.indexed, st.unchanged);
        }
    });
    if interactive && first {
        eprintln!();
    }
    match stats {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agentbridge: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_index(registry: &agentbridge::connector::Registry, provider: Option<&str>, rebuild: bool) {
    if let Some(p) = provider
        && registry.by_id(p).is_none()
    {
        eprintln!("Unknown provider: {p}");
        std::process::exit(2);
    }
    if rebuild {
        let db = agentbridge::store::default_path();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", db.display(), suffix));
        }
    }
    let mut store = open_index_or_exit();
    let stats = refresh_index(registry, &mut store, provider);
    let (sessions, messages) = store.counts().unwrap_or((0, 0));
    println!(
        "Index: {} session(s), {} message(s)  [{} indexed now, {} unchanged, {} removed, {} agentbridge copies skipped]",
        sessions, messages, stats.indexed, stats.unchanged, stats.removed, stats.skipped_copies
    );
    for e in stats.errors.iter().take(10) {
        eprintln!("  ! {e}");
    }
    if stats.errors.len() > 10 {
        eprintln!("  … and {} more", stats.errors.len() - 10);
    }
}

fn parse_since_or_exit(since: Option<&str>) -> Option<i64> {
    since.map(|t| {
        agentbridge::brief::parse_since(t, chrono::Utc::now().timestamp()).unwrap_or_else(|e| {
            eprintln!("--since: {e}");
            std::process::exit(2);
        })
    })
}

fn cmd_search(query: &str, project: Option<&str>, provider: Option<String>, since: Option<&str>, limit: usize) {
    let store = open_index_or_exit();
    if store.counts().map(|(s, _)| s == 0).unwrap_or(true) {
        eprintln!("The index is empty. Run `agentbridge index` first.");
        std::process::exit(1);
    }
    let filter = agentbridge::store::SearchFilter {
        project: project.map(|p| {
            std::fs::canonicalize(p).map(|d| d.to_string_lossy().to_string()).unwrap_or_else(|_| p.to_string())
        }),
        provider,
        since: parse_since_or_exit(since),
        limit,
    };
    let hits = store.search(query, &filter).unwrap_or_else(|e| {
        eprintln!("agentbridge: {e}");
        std::process::exit(1);
    });
    if hits.is_empty() {
        println!("No matches.");
        return;
    }
    for h in &hits {
        let when = h
            .ts
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let short: String = h.sid.chars().take(8).collect();
        println!(
            "[{}:{}#{}] {} · {} · {}",
            h.provider,
            short,
            h.ordinal,
            h.role,
            when,
            h.project.as_deref().unwrap_or("(no project)")
        );
        println!("    {}", h.snippet.replace('\n', " "));
    }
}

fn cmd_brief(
    registry: &agentbridge::connector::Registry,
    project: Option<&str>,
    since: Option<&str>,
    budget: usize,
    no_refresh: bool,
    llm_cmd: Option<&str>,
) {
    let project = resolve_project(project);
    let mut store = open_index_or_exit();
    if !no_refresh {
        refresh_index(registry, &mut store, None);
    }
    let redactor = redactor_or_exit();
    let since = parse_since_or_exit(since);
    match agentbridge::brief::brief_for_project(
        &mut store,
        &project.to_string_lossy(),
        since,
        budget,
        &redactor,
    ) {
        Ok(r) => {
            let mut text = r.text.clone();
            if let Some(cmd) = llm_cmd.filter(|_| r.items > 0) {
                let provider = agentbridge::llm::CommandProvider::new(cmd, std::time::Duration::from_secs(120))
                    .unwrap_or_else(|e| {
                        eprintln!("--llm-cmd: {e}");
                        std::process::exit(2);
                    });
                eprintln!(
                    "Sending the redacted brief ({} tokens) to `{}`. agentbridge opens no \
                     connection itself; what that command does with the text is up to you.",
                    r.tokens,
                    provider.program()
                );
                let summary = agentbridge::llm::summarise(&provider, &redactor, &r.text, budget);
                match &summary.fallback_reason {
                    None => eprintln!("(condensed by the model; every line checked against the brief's citations)"),
                    Some(why) => eprintln!("(model output not used: {why}; showing the plain brief)"),
                }
                text = summary.text;
            }
            print!("{text}");
            eprintln!(
                "({} session(s), {} line(s), {} of {} tokens{})",
                r.sessions,
                r.items,
                agentbridge::brief::count_tokens(&text),
                budget,
                if r.cached { ", cached" } else { "" }
            );
        }
        Err(e) => {
            eprintln!("agentbridge: {e}");
            std::process::exit(2);
        }
    }
}

/// stdout is the protocol channel here: nothing but JSON-RPC may be written to
/// it. Everything for a human goes to stderr.
fn cmd_mcp() {
    let store = open_index_or_exit();
    let redactor = redactor_or_exit();

    // Keep the index fresh without making any tool call wait for a scan: a
    // background thread with its own connection (SQLite WAL lets it write while
    // the server reads), once now and then every five minutes.
    std::thread::spawn(|| {
        loop {
            let registry = agentbridge::connectors::all();
            if let (Ok(mut store), Ok(redactor)) = (
                agentbridge::store::Store::open_default(),
                agentbridge::redact::Redactor::load(),
            ) {
                match agentbridge::store::refresh(&mut store, &registry, &redactor, None, |_| {}) {
                    Ok(s) => eprintln!(
                        "agentbridge mcp: index refreshed ({} indexed, {} unchanged)",
                        s.indexed, s.unchanged
                    ),
                    Err(e) => eprintln!("agentbridge mcp: index refresh failed: {e}"),
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(300));
        }
    });

    let project = std::env::current_dir().unwrap_or_default();
    let mut server = agentbridge::mcp::Server::new(store, redactor, project);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = server.serve(stdin.lock(), &mut stdout) {
        eprintln!("agentbridge mcp: {e}");
        std::process::exit(1);
    }
}

fn cmd_fact(text: &str, project: Option<&str>, tags: Vec<String>) {
    let project = resolve_project(project);
    let mut store = open_index_or_exit();
    let redactor = redactor_or_exit();
    match store.add_fact(Some(&project.to_string_lossy()), text, &tags, &redactor) {
        Ok(id) => println!("Recorded fact {id} for {}", project.display()),
        Err(e) => {
            eprintln!("agentbridge: {e}");
            std::process::exit(1);
        }
    }
}

/// Resolve `--project` (or cwd) to an absolute, symlink-free directory.
fn resolve_project(project: Option<&str>) -> PathBuf {
    match project {
        Some(p) => match std::fs::canonicalize(p) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("--project '{}' could not be resolved: {}", p, e);
                std::process::exit(2);
            }
        },
        None => std::env::current_dir().unwrap_or_default(),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_start(
    registry: &agentbridge::connector::Registry,
    provider: &str,
    passthrough: &[String],
    project: Option<&str>,
    budget: usize,
    no_refresh: bool,
    no_launch: bool,
    dry_run: bool,
) {
    let Some(connector) = registry.by_id(provider) else {
        eprintln!(
            "Unknown provider: {}. Use: {}",
            provider,
            registry.all().iter().map(|c| c.id()).collect::<Vec<_>>().join(", ")
        );
        std::process::exit(2);
    };
    let project = resolve_project(project);

    let mut store = open_index_or_exit();
    if !no_refresh {
        refresh_index(registry, &mut store, None);
    }
    let redactor = redactor_or_exit();
    let result = agentbridge::brief::brief_for_project(
        &mut store,
        &project.to_string_lossy(),
        None,
        budget,
        &redactor,
    )
    .unwrap_or_else(|e| {
        eprintln!("agentbridge: {e}");
        std::process::exit(2);
    });

    if result.items == 0 {
        println!(
            "Nothing indexed for {} yet; nothing to inject for {}.",
            project.display(),
            connector.display_name()
        );
    } else {
        let brief = agentbridge::inject::cap_brief(&result.text, agentbridge::inject::MAX_BRIEF_BYTES);
        if dry_run {
            println!(
                "[dry-run] Would inject a {}-token brief from {} session(s):",
                result.tokens, result.sessions
            );
            println!("{}", brief);
        }
        match connector.inject(&brief, &project, dry_run) {
            Ok(t) if dry_run => println!("Would write to: {}", t.path.display()),
            Ok(t) => {
                println!(
                    "Injected a {}-token brief from {} session(s) into {}",
                    result.tokens,
                    result.sessions,
                    t.path.display()
                );
                println!("Remove it again with: agentbridge clean");
            }
            Err(e) => {
                eprintln!("Could not inject into {}: {}", connector.display_name(), e);
                std::process::exit(1);
            }
        }
    }

    if dry_run || no_launch {
        return;
    }
    let Some(program) = connector.launch_program() else {
        eprintln!("{} cannot be launched from agentbridge.", connector.display_name());
        std::process::exit(1);
    };
    match std::process::Command::new(program)
        .args(passthrough)
        .current_dir(&project)
        .status()
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!(
                "Could not launch `{}`: {} (the context above is already in place; \
                 start it yourself, then `agentbridge clean` when done)",
                program, e
            );
            std::process::exit(127);
        }
    }
}

fn cmd_clean(
    registry: &agentbridge::connector::Registry,
    project: Option<&str>,
    all: bool,
    dry_run: bool,
) {
    use agentbridge::inject::{clean_file, CleanOutcome};
    let project = resolve_project(project);
    let mut files = agentbridge::inject::instruction_files(registry, &project);
    if all {
        files.extend(agentbridge::inject::remembered());
    }
    files.sort();
    files.dedup();

    let mut done = Vec::new();
    let mut failed = false;
    let mut touched = 0;
    for f in &files {
        match clean_file(f, dry_run) {
            Ok(CleanOutcome::NotPresent) => done.push(f.clone()),
            Ok(outcome) => {
                touched += 1;
                let verb = match (outcome, dry_run) {
                    (CleanOutcome::RemovedFile, true) => "would delete (agentbridge created it)",
                    (CleanOutcome::RemovedFile, false) => "deleted (agentbridge created it)",
                    (_, true) => "would clean",
                    (_, false) => "cleaned",
                };
                println!("{} {}", verb, f.display());
                done.push(f.clone());
            }
            Err(e) => {
                failed = true;
                eprintln!("  ! {}: {}", f.display(), e);
            }
        }
    }
    if !dry_run {
        agentbridge::inject::forget(&done);
    }
    if touched == 0 && !failed {
        println!("Nothing to clean{}.", if all { "" } else { " in this project" });
    }
    if failed {
        std::process::exit(1);
    }
}

fn cmd_resume(
    registry: &agentbridge::connector::Registry,
    session_id: &str,
    target: &str,
    project: Option<&str>,
    dry_run: bool,
    merge: bool,
    copy: bool,
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

    let _lock = (!dry_run).then(lock_or_exit);

    // Cross-tool copies are redacted like everything else `sync` writes. A
    // resume into the session's own tool rewrites its native file, and
    // redacting that would replace text the user actually wrote.
    if session.provider != target {
        match agentbridge::redact::Redactor::load() {
            Ok(redactor) => {
                let n = redactor.session(&mut session);
                if n > 0 {
                    println!("  redacted {} secret(s) in the copy (original untouched)", n);
                }
            }
            Err(e) => {
                eprintln!("redaction: {} — nothing was written", e);
                std::process::exit(2);
            }
        }
    }

    println!("Resuming session {} from {} into {}...", session.id, session.provider, target);

    // Cross-tool access: the session originated in another tool. Ask what the
    // user wants done with the origin file instead of silently choosing for
    // them. `--merge`/`--copy` skip the prompt for scripts; non-TTY stdin
    // (pipes) defaults to copy, leaving any previous choice in place.
    if session.provider != target {
        let choice = if merge {
            Some(true)
        } else if copy {
            Some(false)
        } else if std::io::stdin().is_terminal() {
            use std::io::{BufRead, Write};
            println!(
                "This session is native to {}. When you continue it here, what should happen to the original {} file?",
                session.provider, session.provider
            );
            println!("  [m] Merge back — new turns also get written into the original file");
            println!("  [c] Copy only — origin file stays untouched (default)");
            print!("Choice (m/c) [c]: ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                Ok(_) => matches!(line.trim().to_lowercase().as_str(), "m" | "merge").then_some(true),
                Err(_) => None,
            }
        } else {
            None
        };

        match choice {
            Some(true) => {
                if let Err(e) = agentbridge::sync::set_merge(&session.id) {
                    eprintln!("  ! could not record merge choice: {}", e);
                } else {
                    println!(
                        "  Merge-back ON: new turns in {} will also update the original {} session file",
                        target, session.provider
                    );
                }
            }
            Some(false) => {
                agentbridge::sync::clear_merge(&session.id);
                println!(
                    "  Copy only: the original {} file stays untouched; new turns live in {}'s copy",
                    session.provider, target
                );
            }
            None => {
                println!(
                    "  Defaulting to copy-only (origin file untouched). Use --merge to opt in."
                );
            }
        }
    }

    // The copy `resume` writes into the target carries the same cross-tool
    // label `sync` stamps on every materialized copy (origin tool · name ·
    // the session's own start time · id). So a resumed conversation correlates
    // with its siblings in the other pickers, keeps an existing name/title
    // verbatim, and shows its original date — never the resume date. Native
    // resume into the session's own tool is excluded: that path (re)writes the
    // origin file itself, and a tool's own session titles are never rewritten.
    agentbridge::label::apply_for_resume(&mut session, target);

    let target_dirs = match target {
        "claude-code" => {
            let root = agentbridge::sync::claude_live_root().unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(&home).join(".claude").join("projects")
            });
            vec![root]
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
        "antigravity" => match agentbridge::antigravity_write::store() {
            Some(home) => vec![home],
            None => {
                eprintln!("Antigravity store not found (looked under ~/.gemini)");
                return;
            }
        },
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
            // Resuming into the session's own tool rewrites its native file,
            // which must stay unmarked; a cross-tool copy is ours and marked.
            let converter = if session.provider == target {
                ClaudeCodeConverter::unmarked()
            } else {
                ClaudeCodeConverter::new()
            };
            converter.convert(&session, target_dir)
        }
        "codex-cli" => {
            let converter = if session.provider == target {
                CodexCliConverter::unmarked()
            } else {
                CodexCliConverter::new()
            };
            let mut dirs = vec![session.project_path().unwrap_or_default()];
            if let Ok(home) = std::env::var("HOME") {
                let home = home.trim_end_matches('/').to_string();
                if !dirs.contains(&home) {
                    dirs.push(home);
                }
            }
            match converter.convert_multi(&session, target_dir, &dirs) {
                Err(e) => Err(e),
                Ok(written) => {
                    // Index each rollout in `state_5.sqlite` so it shows up in
                    // `codex /resume` from its own directory (CONNECTORS.md §2).
                    if let Some(db) = agentbridge::codex_write::state_db() {
                        match agentbridge::codex_write::ensure_safe_to_write() {
                            Err(e) => eprintln!("  ! {}", e),
                            Ok(()) => {
                                let will_insert = dirs.iter().any(|d| {
                                    let sid =
                                        agentbridge::codex_write::session_uuid_for_dir(&session.id, d);
                                    agentbridge::codex_write::thread_row_exists(&db, &sid)
                                        .map(|e| !e)
                                        .unwrap_or(true)
                                });
                                if will_insert {
                                    let _ = agentbridge::codex_write::backup(&db);
                                }
                                let mut inserted = 0usize;
                                for (dir, path) in dirs.iter().zip(&written) {
                                    if let Ok(r) = agentbridge::codex_write::ensure_thread_rows(
                                        &db, &session, path, std::slice::from_ref(dir),
                                    ) {
                                        inserted += r.inserted;
                                    } else {
                                        eprintln!("  ! codex threads row failed");
                                    }
                                }
                                if inserted > 0 {
                                    println!(
                                        "  indexed into codex /resume ({} new row(s))",
                                        inserted
                                    );
                                }
                            }
                        }
                    }
                    written
                        .into_iter()
                        .next()
                        .ok_or_else(|| "codex materialization produced no files".to_string())
                }
            }
        }
        "opencode" => {
            let db = target_dir.join("opencode.db");
            match agentbridge::opencode_write::ensure_safe_to_write() {
                Err(e) => Err(e.to_string()),
                Ok(()) => {
                    let dir = session.project_path().unwrap_or_default();
                    // The row is already ours: the write below only refreshes
                    // it, which needs no backup. A new row gets one first.
                    let exists = !agentbridge::opencode_write::will_insert(
                        &db,
                        &session,
                        std::slice::from_ref(&dir),
                    );
                    if !exists
                        && let Err(e) = agentbridge::opencode_write::backup(&db)
                    {
                        Err(format!("opencode backup failed: {}", e))
                    } else {
                        match agentbridge::opencode_write::write_session(&db, &session, &dir) {
                            Ok((id, _, _)) => Ok(PathBuf::from(id)),
                            Err(e) => Err(e.to_string()),
                        }
                    }
                }
            }
        }
        "antigravity" => match agentbridge::antigravity_write::ensure_safe_to_write() {
            Err(e) => Err(e.to_string()),
            Ok(()) => {
                let dir = session.project_path().unwrap_or_default();
                // The conversation is already ours: the write below only
                // refreshes it, which needs no backup. A new one gets one first.
                let index = agentbridge::antigravity_write::summaries_db(target_dir);
                if index.is_file()
                    && agentbridge::antigravity_write::will_insert(
                        target_dir,
                        &session,
                        std::slice::from_ref(&dir),
                    )
                    && let Err(e) = agentbridge::antigravity_write::backup(&index)
                {
                    eprintln!("  ! antigravity backup failed: {}", e);
                }
                agentbridge::antigravity_write::write_session(target_dir, &session, &dir)
                    .map(|row| row.body)
                    .map_err(|e| e.to_string())
            }
        },
        // Every target the CLI accepts must be handled above; `resume` parses
        // `target` from a fixed list, so anything else is a programming error
        // rather than bad input — but reporting it beats panicking on a user's
        // machine.
        other => Err(format!("resume into {} is not implemented", other)),
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
                // The conversation id is the body's filename stem.
                // `--conversation` per CONNECTORS.md §4 (verified agy 1.1.8).
                "antigravity" => vec![
                    "agy".to_string(),
                    "--conversation".to_string(),
                    path.file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
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
    project: Option<&str>,
    dry_run: bool,
) {
    let Some(connector) = registry.by_id(provider) else {
        eprintln!("Unknown provider: {}", provider);
        std::process::exit(2);
    };
    let project = resolve_project(project);
    let mut store = open_index_or_exit();
    refresh_index(registry, &mut store, None);

    let mut rows = Vec::new();
    for id in session_ids {
        match store.find_session(id) {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => eprintln!("  ! no indexed session matches `{id}`"),
            Err(e) => {
                eprintln!("agentbridge: {e}");
                std::process::exit(1);
            }
        }
    }
    if rows.is_empty() {
        eprintln!("No sessions found for the given IDs.");
        std::process::exit(1);
    }
    let redactor = redactor_or_exit();
    let result = agentbridge::brief::brief_for_sessions(&store, rows, 1500, &redactor)
        .unwrap_or_else(|e| {
            eprintln!("agentbridge: {e}");
            std::process::exit(2);
        });
    let brief = agentbridge::inject::cap_brief(&result.text, agentbridge::inject::MAX_BRIEF_BYTES);

    if dry_run {
        println!("[dry-run] Would inject a {}-token brief into {}:", result.tokens, provider);
        println!("{}", brief);
    }
    match connector.inject(&brief, &project, dry_run) {
        Ok(target) if dry_run => println!("Would write to: {}", target.path.display()),
        Ok(target) => {
            println!("Injected brief into {} at {}", provider, target.path.display());
            println!("Remove it again with: agentbridge clean");
        }
        Err(e) => {
            eprintln!("Failed to inject: {}", e);
            std::process::exit(1);
        }
    }
}

fn find_session(registry: &agentbridge::connector::Registry, session_id: &str) -> Option<Session> {
    for c in registry.all() {
        if !c.detect() {
            continue;
        }
        // A connector whose scan fails, or one corrupt session in the middle of
        // a scan, must not hide every session after it.
        let Ok(scan) = c.scan() else { continue };
        for raw in scan.filter_map(|r| r.ok()) {
            if (raw.id == session_id || raw.id.contains(session_id) || session_id.contains(&raw.id))
                && raw.body_available
                && let Ok(session) = c.load(&raw.id)
            {
                return Some(session);
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

fn cmd_sync(
    registry: &agentbridge::connector::Registry,
    project: Option<&str>,
    dry_run: bool,
    all_known: bool,
    yes: bool,
) {
    // Every directory a session was ever recorded in, that still exists.
    let known: Vec<PathBuf> = if all_known {
        agentbridge::index::discover(registry)
            .project_dirs()
            .into_iter()
            .filter(|d| d.is_dir())
            .collect()
    } else {
        Vec::new()
    };
    if all_known && !dry_run && !yes {
        eprintln!(
            "--all-known would sync {} directories. Each one re-reads your whole session history, \
             and OpenCode gets a copy of every session per project directory it knows, which grows \
             its database. Preview with --dry-run, or confirm with --yes.",
            known.len()
        );
        std::process::exit(2);
    }
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

    // One writer at a time: the hook, `auto watch` and manual runs all edit
    // the manifest. A dry run only reads, so it never waits.
    let _lock = (!dry_run).then(lock_or_exit);

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

    let report = if all_known {
        println!("Surfacing all machine sessions in {} known directories", known.len());
        let mut total = agentbridge::sync::SyncReport::default();
        for d in &known {
            total.absorb(agentbridge::sync::sync_into(registry, d, dry_run));
        }
        total
    } else {
        println!("Surfacing all machine sessions in {}", dir.display());
        agentbridge::sync::sync_into(registry, &dir, dry_run)
    };

    if dry_run {
        println!("[dry-run] would materialize {} session(s); nothing written", report.created.len());
    } else {
        println!("  created   {}", report.created.len());
        println!("  unchanged {}", report.unchanged);
        if report.codex_indexed > 0 {
            println!("  indexed   {} session(s) into codex /resume", report.codex_indexed);
        }
    }
    if report.sessions_redacted > 0 {
        println!(
            "  redacted  secrets in {} session(s) (copies only; originals untouched)",
            report.sessions_redacted
        );
    }
    if report.skipped_native > 0 {
        println!("  skipped   {} (already native here)", report.skipped_native);
    }
    if report.merged_native > 0 {
        println!(
            "  merged    {} (opt-in merge-back: origin files updated)",
            report.merged_native
        );
    }
    for e in report.errors.iter().take(10) {
        eprintln!("  ! {}", e);
    }
    // Fail closed, loudly: a redaction problem means nothing was written, and
    // a script has to be able to tell.
    if report.errors.iter().chain(pulled.errors.iter()).any(|e| e.starts_with("redaction:")) {
        std::process::exit(2);
    }
    if !dry_run && !report.created.is_empty() {
        println!();
        println!("Open any tool here — its own session picker now lists them.");
        println!("Undo with: agentbridge unsync");
    }
}

fn cmd_unsync(registry: &agentbridge::connector::Registry, dry_run: bool, orphans: bool) {
    let _lock = (!dry_run).then(lock_or_exit);
    let report = if orphans {
        agentbridge::sync::unsync_orphans(registry, dry_run)
    } else {
        agentbridge::sync::unsync(dry_run)
    };
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

fn cmd_pull(dry_run: bool, auto_merge: bool) {
    // Dry-run never blocks for input — it only previews. A non-TTY stdin
    // (piped, scripted, cron) can't answer a prompt either. `--auto-merge`
    // is the explicit opt-out for scripts that want today's behavior. The
    // full-screen picker (src/tui.rs) is only ever constructed here.
    let interactive = !dry_run && !auto_merge && std::io::stdin().is_terminal();
    let _lock = (!dry_run).then(lock_or_exit);

    let report = if interactive {
        agentbridge::sync::pull_back_with(dry_run, &mut crate::tui::RatatuiConflictResolver)
    } else {
        agentbridge::sync::pull_back(dry_run)
    };
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
    if !report.renamed.is_empty() {
        println!(
            "{} rename(s){}:",
            report.renamed.len(),
            if dry_run { " [dry-run]" } else { "" }
        );
        for (id, title) in &report.renamed {
            println!("  {:<40} -> {}", id, title);
        }
    }
    if !report.conflicts.is_empty() {
        println!(
            "{} session(s) had new work from more than one tool:",
            report.conflicts.len()
        );
        for (id, providers, choice) in &report.conflicts {
            use agentbridge::sync::ConflictChoice;
            let outcome = match choice {
                ConflictChoice::MergeAll => "merged".to_string(),
                ConflictChoice::KeepOnly(p) => format!("kept only {}", p),
                ConflictChoice::Skip => "skipped — will ask again next pull".to_string(),
            };
            println!("  {:<40} {} -> {}", id, providers.join("+"), outcome);
        }
        if !interactive {
            println!("  (non-interactive: use `agentbridge pull` from a terminal to choose)");
        }
    }
    for e in report.errors.iter().take(10) {
        eprintln!("  ! {}", e);
    }
    if !dry_run && (total > 0 || !report.renamed.is_empty()) {
        println!();
        println!("Run `agentbridge sync` to push these to every other tool.");
    }
}

fn cmd_status(registry: &agentbridge::connector::Registry) {
    let rows = agentbridge::sync::status();
    if rows.is_empty() {
        println!("Nothing synced. Run `agentbridge sync`.");
        print_orphans(registry);
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
    print_orphans(registry);
}

/// Generated files the manifest has lost track of, found by their marker.
fn print_orphans(registry: &agentbridge::connector::Registry) {
    let orphans = agentbridge::sync::orphans(registry);
    if orphans.is_empty() {
        return;
    }
    println!();
    println!("{} generated file(s) are not in the manifest:", orphans.len());
    for o in orphans.iter().take(20) {
        let state = match o.on_disk {
            Some(n) if n == o.expected => "unmodified".to_string(),
            Some(n) => format!("{} turn(s) added since", n.saturating_sub(o.expected)),
            None => "unreadable".to_string(),
        };
        println!(
            "  {} (from {} {}) — {}",
            o.path.display(),
            o.origin_provider,
            o.origin_id,
            state
        );
    }
    println!("Remove the unmodified ones with: agentbridge unsync --orphans");
}

/// Take the run lock or exit; waiting is bounded, never indefinite.
fn lock_or_exit() -> agentbridge::lock::RunLock {
    match agentbridge::lock::acquire_default() {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("agentbridge: {e}");
            // EX_TEMPFAIL: nothing was done, safe to retry.
            std::process::exit(75);
        }
    }
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
