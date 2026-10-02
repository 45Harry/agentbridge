# ARCHITECTURE.md — agentbridge system design

This document draws the system twice: a high level design (HLD) of the parts
and how data moves between them, and a low level design (LLD) of the modules,
types, and the exact steps each command runs. `DESIGN.md` explains *why* the
design is what it is; this file shows *what* the code does today.

Diagrams are Mermaid and render on GitHub and in most Markdown previews.

---

# Part 1: High level design (HLD)

## 1.1 What the system does

Every AI coding tool on a machine keeps its own sessions in its own store and
shows only a slice of them (scoped to the current directory). agentbridge reads
every store, converts each session once into every other tool's native format,
and places it where that tool's own picker will find it. Turns added in any copy
flow back so every tool sees them.

## 1.2 System context

```mermaid
flowchart LR
    user([Developer])

    subgraph tools["AI coding tools (each keeps its own store)"]
        cc["Claude Code<br/>~/.claude/projects/*.jsonl"]
        cx["Codex CLI<br/>~/.codex/sessions/*.jsonl<br/>+ state_5.sqlite"]
        oc["OpenCode<br/>~/.local/share/opencode/opencode.db"]
        ag["Antigravity (agy)<br/>~/.gemini/antigravity*/<br/>conversations/*.db"]
    end

    ab["agentbridge<br/>(single Rust binary)"]
    data[("~/.agentbridge<br/>cache · overlay · manifest")]
    shell["Shell rc hook<br/>sync --changed"]

    user -->|"CLI / dashboard"| ab
    shell -->|"each new shell"| ab
    ab -->|"read only scan + load"| cc & cx & oc & ag
    ab -->|"hardlink converted copies"| cc & cx
    ab -->|"INSERT tagged rows"| oc & cx
    ab -->|"write conversation db + summary row"| ag
    ab <--> data
    user -->|"uses natively"| tools
```

No network calls, no telemetry. Everything is local files and local SQLite.

## 1.3 Layered architecture

```mermaid
flowchart TB
    subgraph bin["Binary (UI) — src/main.rs, tui.rs, dashboard.rs"]
        cli["clap CLI<br/>ls · index · init · sync · pull · status · unsync<br/>resume · start · inject · info · auto · tui"]
        dash["Dashboard (ratatui)"]
        tui["Conflict screen<br/>RatatuiConflictResolver"]
    end

    subgraph lib["Library — src/lib.rs (no UI dependencies)"]
        direction TB
        orch["Orchestration<br/>sync.rs · auto.rs"]
        disc["Discovery<br/>index.rs"]
        xform["Transform<br/>convert.rs · label.rs · inject.rs"]
        writers["Database writers<br/>opencode_write.rs · codex_write.rs · antigravity_write.rs"]
        core["Core contracts<br/>connector.rs (Connector, Registry) · model.rs"]
        conns["Read connectors — src/connectors/<br/>claude_code · codex_cli · opencode · antigravity"]
    end

    stores[("Tool stores")]
    abdata[("~/.agentbridge")]

    cli --> orch
    cli --> disc
    cli --> xform
    dash --> orch
    tui -. "implements ConflictResolver" .-> orch
    orch --> disc
    orch --> xform
    orch --> writers
    disc --> core
    xform --> core
    conns --> core
    core --> conns
    conns -->|"read only"| stores
    writers -->|"gated writes"| stores
    orch -->|"hardlinks"| stores
    orch <--> abdata
```

The library never imports UI code. `sync.rs` reaches the conflict screen only
through the `ConflictResolver` trait, which the binary implements.

## 1.4 Core data flow: index in place, derive once, link many

```mermaid
flowchart LR
    src["Source session<br/>(in its own tool, never modified)"]
    idx["Index entry<br/>metadata + pointer only"]
    norm["Normalized Session<br/>model.rs"]
    ovl["+ overlay turns<br/>+ overlay title"]
    lbl["+ cross tool label<br/>label::apply"]
    cache[("cache/&lt;target&gt;/...<br/>one artifact per target")]
    d1["Claude Code dir A"]
    d2["Claude Code dir B"]
    d3["Codex rollout path"]
    oc["OpenCode row"]
    ag["agy conversation db"]

    src -->|"Connector::scan"| idx
    idx -->|"Connector::load"| norm
    norm --> ovl --> lbl
    lbl -->|"convert"| cache
    cache -->|"hardlink (same inode)"| d1 & d2 & d3
    lbl -->|"opencode_write"| oc
    lbl -->|"antigravity_write"| ag
```

Three rules hold it together (full reasoning in `DESIGN.md` §4):

1. Never copy a session body into agentbridge. The index is metadata plus a pointer.
2. One derived artifact per session and target format, kept in `~/.agentbridge/cache`.
3. Presence in a directory is a hardlink to that artifact, not a copy.

OpenCode and Antigravity have no linkable files, so they get gated database writes instead.

## 1.5 The sync loop (write-back)

```mermaid
sequenceDiagram
    autonumber
    participant U as Developer
    participant A as Tool A (origin)
    participant AB as agentbridge
    participant B as Tool B (copy)
    participant D as ~/.agentbridge

    U->>A: works in a session
    AB->>A: scan + load (read only)
    AB->>D: convert into cache, record in manifest
    AB->>B: hardlink or insert copy
    U->>B: resumes the copy, adds turns
    AB->>B: pull: read copy, turns past message_count are new
    AB->>D: append new turns to overlay/<id>.jsonl
    Note over A: origin file is never touched
    AB->>D: re-sync: origin + overlay → refreshed cache
    Note over AB,B: every hardlinked copy updates at once (same inode)
```

`agentbridge resume --merge` is the only opt-in exception: it sets a merge
marker so overlay turns are also written back into the origin's own file
(Claude Code and Codex only, never OpenCode or Antigravity).

## 1.6 Triggers

```mermaid
flowchart LR
    manual["agentbridge sync / pull"] --> run
    hook["Shell hook<br/>sync --changed"] --> fp{"fingerprint<br/>changed?"}
    fp -- no --> stop["exit: nothing new"]
    fp -- yes --> since["sync only sessions<br/>changed since last run"] --> run
    watch["auto watch<br/>(poll every N s)"] --> fp2{"fingerprint<br/>changed?"}
    fp2 -- yes --> run
    fp2 -- no --> watch
    run["pull_back → sync_into(_since)"] --> lock{"sync.lock<br/>free?"}
    lock -- no --> busy["stand down<br/>(other run covers it)"]
    lock -- yes --> work["materialize"]
```

## 1.7 Storage layout owned by agentbridge

```text
~/.agentbridge/                (or $AGENTBRIDGE_DATA_DIR)
├── cache/
│   ├── claude-code/...        converted Claude Code JSONL (hardlink source)
│   └── codex-cli/...          converted Codex rollouts (hardlink source)
├── overlay/
│   ├── <session-id>.jsonl     turns recovered from other tools' copies
│   └── <session-id>.title     rename recovered from another tool
├── merge/<session-id>         opt-in marker for merge-back (resume --merge)
├── manifest.jsonl             one LinkRecord per thing agentbridge created
├── last-sync.json             { at, fingerprint } for sync --changed
└── sync.lock                  single-writer lock (stale after 30 min)
```

## 1.8 Non-functional guarantees

| Concern | How it is met |
|---|---|
| Safety of tool data | Origin files never modified; DB writes back up first, tag rows, refuse while the tool runs |
| Storage | Body stored once in cache; extra directories cost one inode entry |
| Idempotency | UUID v5 ids, paths from the session's own start time, manifest dedup |
| Reversibility | `unsync` removes exactly the manifest rows, and only when the inode still matches |
| Concurrency | `sync.lock` in the data dir; a busy run stands down |
| Loop prevention | Manifest plus self-declared copy labels (`label::is_copy`) |
| Every write previewable | `--dry-run` on every writing command |
| Privacy | No network, no telemetry |

---

# Part 2: Low level design (LLD)

## 2.1 Module map and dependencies

```mermaid
flowchart TB
    main["main.rs<br/>clap Commands"]
    dashboard["dashboard.rs<br/>Dashboard::run"]
    tuirs["tui.rs<br/>RatatuiConflictResolver"]

    sync["sync.rs<br/>sync_into_since · pull_back_with<br/>status · unsync_matching<br/>manifest · overlay · RunLock"]
    auto["auto.rs<br/>fingerprint · watch<br/>install_hook / uninstall_hook"]
    index["index.rs<br/>discover → Index"]
    convert["convert.rs<br/>SessionConverter<br/>ClaudeCode / CodexCli / OpenCode<br/>build_cross_tool_brief"]
    label["label.rs<br/>build · apply · is_copy<br/>is_bookkeeping"]
    inject["inject.rs<br/>fenced brief blocks"]
    ocw["opencode_write.rs"]
    cxw["codex_write.rs"]
    agw["antigravity_write.rs"]
    connector["connector.rs<br/>Connector · Registry"]
    model["model.rs"]
    conns["connectors/mod.rs::all()"]
    c1["claude_code.rs"]
    c2["codex_cli.rs"]
    c3["opencode.rs"]
    c4["antigravity.rs"]

    main --> sync & auto & index & convert & label & inject & ocw & cxw & agw
    main --> dashboard & tuirs
    tuirs --> sync
    auto --> sync
    sync --> index & convert & label & ocw & cxw & agw
    index --> connector
    conns --> c1 & c2 & c3 & c4
    c1 & c2 & c3 & c4 --> connector
    connector --> model
    convert --> model
    label --> model
    ocw & cxw & agw --> model
```

## 2.2 Core types

```mermaid
classDiagram
    class Connector {
        <<trait>>
        +id() &str
        +display_name() &str
        +detect() bool
        +roots() Vec~PathBuf~
        +scan() SessionStream
        +load(id) Session
        +resume_cmd(session) Option~Vec~String~~
        +inject(brief, dry_run) InjectTarget
    }
    class Registry {
        -connectors: Vec~Box~dyn Connector~~
        +all()
        +detected()
        +by_id(id)
    }
    class RawSession {
        id, provider
        project_path, source_path
        started_at, last_event_at
        title, source
        body_available: bool
    }
    class IndexEntry {
        id, provider
        project_path, source_path
        started_at, last_event_at
        title, source
    }
    class Index {
        entries: Vec~IndexEntry~
        errors: Vec~String~
        +by_provider()
        +project_dirs()
        +find(id)
    }
    class Session {
        id, provider, project_id
        started_at, last_event_at
        model, title
        token_totals: TokenTotals
        source_path
        raw_payload: Value
        body_available
        messages: Vec~Message~
        artifacts: Vec~Artifact~
    }
    class Message {
        session_id, ordinal
        role: Role
        timestamp, text
        tool_name, tool_input, tool_result
        parent_ordinal
    }
    class Artifact {
        session_id
        kind: ArtifactKind
        path_or_command, detail
    }
    class LinkRecord {
        dest, cache
        session_id
        source_provider, target_provider
        project, inode
        message_count
        title
    }
    class SessionConverter {
        <<trait>>
        +convert(session, root)
        +convert_multi(session, cache, dirs)
    }
    class ConflictResolver {
        <<trait>>
        +resolve(session_id, items) ConflictChoice
    }
    class ConflictChoice {
        <<enum>>
        MergeAll
        KeepOnly(provider)
        Skip
    }

    Registry o-- Connector
    Connector ..> RawSession : scan yields
    Connector ..> Session : load returns
    RawSession ..> IndexEntry : discover maps
    Index o-- IndexEntry
    Session *-- Message
    Session *-- Artifact
    SessionConverter ..> Session
    ConflictResolver ..> ConflictChoice
    LinkRecord ..> Session : records one materialized copy
```

`ConflictResolver` has two implementations: `AutoMerge` (library, used by
dry-run and `auto watch`) and `RatatuiConflictResolver` (binary, interactive).

## 2.3 Connectors (read side)

| Connector `id()` | Store | Env override | Format |
|---|---|---|---|
| `claude-code` | `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` | `CLAUDE_CONFIG_DIR` | JSONL, one record per event |
| `codex-cli` | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `CODEX_HOME` | JSONL rollout, first record `session_meta` |
| `opencode` | `$XDG_DATA_HOME/opencode/opencode.db` | `XDG_DATA_HOME` | SQLite rows (`session`, `message`, `part`) |
| `antigravity` | `~/.gemini/antigravity*/conversations/*.db` | `ANTIGRAVITY_HOME` (write only) | SQLite + hand-written protobuf reader |

Contract highlights: `detect()` is existence checks only; `scan()` is lazy and
yields one `Err` per bad session instead of aborting; SQLite is opened
`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`; the project path comes from inside
the records, never from the encoded directory name.

## 2.4 Write side: how each target receives a copy

```mermaid
flowchart TB
    s["Labeled Session<br/>(re-homed to project + $HOME)"]
    s --> t{target}

    t -->|claude-code| cc1["ClaudeCodeConverter.convert_multi<br/>→ cache/claude-code/..."]
    cc1 --> cc2["hardlink into<br/>~/.claude/projects/&lt;dir&gt;/&lt;uuid&gt;.jsonl<br/>(inode check, temp+rename if different)"]

    t -->|codex-cli| cx1["CodexCliConverter.convert_multi<br/>one rollout per target dir"]
    cx1 --> cx2["hardlink into ~/.codex/sessions/..."]
    cx2 --> cx3["codex_write::ensure_thread_rows<br/>INSERT threads row in state_5.sqlite"]

    t -->|opencode| oc1["opencode_write::write_sessions<br/>INSERT session/message/part rows<br/>(creates 'global' project if missing)"]

    t -->|antigravity| ag1["antigravity_write::write_sessions<br/>conversation .db body (protobuf)<br/>+ summaries row"]

    cc2 & cx3 & oc1 & ag1 --> m["LinkRecord → manifest.jsonl"]
```

Every database writer follows the same four gates before touching another
tool's live database:

```mermaid
flowchart LR
    a["ensure_safe_to_write()<br/>tool not running?"] -->|yes| b["will_insert()?<br/>new row needed"]
    a -->|no| x["error, skip target"]
    b -->|yes, first in run| c["backup(db)"]
    b -->|no, refresh only| d
    c --> d["write rows tagged<br/>MARKER = 'agentbridge'"]
    d --> e["--dry-run: plan() renders<br/>statements instead"]
```

Ids are deterministic: `derive_id(source_provider, source_id, project)` is a
UUID v5, so a re-sync rewrites the same row in place and the manifest row is
updated, not appended.

## 2.5 `sync` in detail (`sync::sync_into_since`)

```mermaid
flowchart TB
    start(["sync [--changed] [--dry-run]"]) --> ch{"--changed?"}
    ch -- yes --> fp{"fingerprint ==<br/>last-sync.json?"}
    fp -- yes --> done0(["Nothing new"])
    fp -- no --> since["since = last run time"]
    ch -- no --> allx["since = None"]
    since & allx --> pull["pull_back (AutoMerge)<br/>recover turns first"]
    pull --> lock{"acquire sync.lock"}
    lock -- busy --> done1(["stand down"])
    lock -- ok --> disc["index::discover(registry)"]
    disc --> filt["keep entries changed ≥ since"]
    filt --> tgts["targets = detected tools with<br/>live_root / opencode db / agy store"]
    tgts --> gen["build 'generated' set:<br/>manifest dests + OpenCode row ids<br/>+ self-declared copies (label::is_copy)"]
    gen --> dedup["dedup (provider, id),<br/>prefer real source over copy"]
    dedup --> loop{{"for each entry × target"}}
    loop --> isgen{"generated?"}
    isgen -- yes --> skipc["skip (loop prevention)"]
    isgen -- no --> native{"native to target<br/>in this dir?"}
    native -- yes --> merge{"merge marker<br/>+ overlay turns?"}
    merge -- yes --> mb["merge_back_native"]
    merge -- no --> skipn["skipped_native"]
    native -- no --> load["Connector::load"]
    load --> rehome["project_id = sync dir"]
    rehome --> fold["fold_overlay<br/>(dedup by turn, re-sort, re-ordinal,<br/>apply overlay title)"]
    fold --> lab["label::apply"]
    lab --> write["write per target (2.4)"]
    write --> rec["update or append LinkRecord"]
    rec --> loop
    loop -- finished --> save["write manifest.jsonl"]
    save --> st["record last-sync.json<br/>(fingerprint taken after run)"]
```

## 2.6 `pull` in detail (`sync::pull_back_with`)

```mermaid
flowchart TB
    p0(["pull [--dry-run]"]) --> lk{"sync.lock"}
    lk -- busy --> pb(["stand down"])
    lk -- ok --> rm["read_manifest (deduped)"]
    rm --> p1{{"Pass 1: each LinkRecord"}}
    p1 --> ld["load_materialized(target, dest, id)"]
    ld --> diff["new_title = title ≠ rec.title<br/>new = messages[rec.message_count..]"]
    diff --> clean["drop bookkeeping turns<br/>(label::is_bookkeeping) and empty turns"]
    clean --> pend["pending_by_session[id].push"]
    pend --> p1
    p1 -- done --> p2{{"Pass 2: each session"}}
    p2 --> many{"> 1 tool with<br/>new work?"}
    many -- no --> all["MergeAll"]
    many -- yes --> res["resolver.resolve(id, items)"]
    res --> choice{"choice"}
    choice -- Skip --> p2
    choice -- "MergeAll / KeepOnly(p)" --> apply
    all --> apply["append_overlay(id, turns)<br/>set_overlay_title(id, title)"]
    apply --> bump["rec.message_count += seen<br/>rec.title = new title"]
    bump --> p2
    p2 -- done --> wm["rewrite manifest.jsonl"]
```

`message_count` moves by everything the copy gained, bookkeeping included, so a
dropped turn is never offered again.

## 2.7 `unsync` (`sync::unsync_matching`)

```mermaid
flowchart LR
    u0(["unsync [filter] [--dry-run]"]) --> u1["read manifest"]
    u1 --> u2{{"each matching LinkRecord"}}
    u2 --> u3{"target"}
    u3 -->|file target| u4{"inode still<br/>matches?"}
    u4 -- yes --> u5["remove link"]
    u4 -- no --> u6["kept_foreign<br/>(tool replaced it)"]
    u3 -->|opencode| u7["opencode_write::remove_one(row id)"]
    u3 -->|antigravity| u8["antigravity_write::remove_one(v5 id)"]
    u3 -->|codex rows| u9["codex_write::remove_rows_for_path"]
    u5 & u6 & u7 & u8 & u9 --> u2
    u2 -- done --> u10["rewrite manifest<br/>overlay is kept"]
```

The overlay survives `unsync`: recovered work is never thrown away.

## 2.8 Other commands

| Command | Library path |
|---|---|
| `init` / `index` / `ls` | `index::discover` → print by provider and project dir. Read only |
| `status` | `sync::status` → `StatusRow::drift()` = messages on disk minus `message_count` |
| `resume <id> --in <tool> [--merge]` | load → `label::apply_for_resume` → write into the target (same writers as sync) → optional `sync::set_merge` → print the target tool's resume command (from the converter's `resume_cmd`, or `opencode run --session` / `agy --conversation`) |
| `start` | load recent sessions for the project → `convert::build_cross_tool_brief` → `inject::agentbridge_start` |
| `inject` | `Connector::inject` with fenced begin/end markers (`inject.rs`) so it can be removed exactly |
| `auto install` / `uninstall` | `auto::install_hook` / `uninstall_hook` edit a fenced block in the shell rc |
| `auto watch` | `auto::watch`: poll fingerprint, then `pull_back` + `sync_into` |
| `tui` / no args | `dashboard::Dashboard::run` |

## 2.9 Change detection (`auto::fingerprint`)

Fingerprint = `(path, size, mtime)` for every connector root's session files,
plus the `-wal` sibling of each SQLite database (WAL writes do not change the
`.db` mtime). The `-shm` file is left out of the digest because reading a
database changes it. The digest is stored in `last-sync.json` after a run, so
the copies that run wrote do not count as a change next time.

## 2.10 Copy identification (`label.rs`)

A copy must say it is a copy even without the manifest:

- Titled formats: the title is a label `provider · name · start time · id[..8]`
  naming another origin. `label::parse` / `label::strip` read it.
- Codex (no title): the first record carries an `agentbridge` origin key
  (`label::ORIGIN_KEY`), passed through `raw_payload`.
- `label::is_copy(session)` checks both, judged on the entry's own file.
- Antigravity rows are recognised by their UUID v5 id
  (`antigravity_write::is_derived_id`), because agy blanks titles on rows it
  did not write when it rebuilds its index.

## 2.11 Known gaps

- Redaction (`src/redact.rs`) required by `DESIGN.md` invariant 6 is not built yet.
- Topic threading across directories (`DESIGN.md` §10) is undecided.
- Merge-back into Antigravity and OpenCode origins is deliberately unsupported.
