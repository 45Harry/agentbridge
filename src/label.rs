//! Cross-tool session labels.
//!
//! A materialized copy of a session is the *same conversation* as its origin,
//! but each tool shows it under its own picker with only a short name. That
//! makes a session impossible to correlate: the same conversation appears in
//! Claude Code, Codex, OpenCode and Antigravity with nothing tying the four
//! rows together, and each picker's date column shows when the copy was
//! written rather than when the conversation happened.
//!
//! The label puts the identity in the one field every tool displays — the
//! title:
//!
//! ```text
//! claude-code · Wire up the bridge · 2026-08-19 10:00 · aaaaaaaa
//! └ origin tool  └ session name      └ session start   └ session id
//! ```
//!
//! The id field is the part of the id that tells sessions apart: the first 8
//! characters, except for OpenCode's `ses_…` ids, whose start is time-ordered
//! and shared by neighbouring sessions — those give their last 8.
//!
//! Three properties this has to hold, all of them learned the hard way:
//!
//! 1. **The timestamp is the session's own start, never `now`.** A label built
//!    from sync time would change on every run, and `pull_back` compares the
//!    title it wrote against the title it reads back — a moving label reports
//!    every session as renamed on every pull (see `DECISIONS.md`, the
//!    705-false-rename regression).
//! 2. **UTC, not local time.** Local time makes the label depend on the
//!    machine's timezone, so syncing from a laptop that changed zones would
//!    look like a mass rename.
//! 3. **Labeling is idempotent.** `apply` strips any label already present
//!    before building a new one, so a label that leaks back into a session's
//!    title (possible for manifests written before this existed) is rebuilt
//!    rather than nested.
//!
//! And one rule about the name field: **a name the tool already has is kept
//! verbatim.** Many tools let you name a session (`claude -n`, an in-session
//! rename, agy's `title` column); that name is the user's, so it is never
//! truncated or reworded. Only a session with no name at all gets one derived
//! from its opening message. A tool's own placeholder (`New session - <time>`
//! in OpenCode) counts as no name, and the opening message is the first thing
//! a person wrote — tool wrapper blocks and agentbridge's own placeholder turn
//! are skipped.

use crate::model::{Role, Session};

/// Separator between label fields. A middle dot reads as punctuation rather
/// than structure in a picker row, and is vanishingly rare in real titles —
/// but `parse` never assumes that: a name containing the separator is
/// reassembled from the middle fields.
const SEP: &str = " · ";

/// `%Y-%m-%d %H:%M` — minute precision. Seconds add width without helping a
/// human correlate two rows, and the value is fixed for the session's
/// lifetime either way.
const STAMP: &str = "%Y-%m-%d %H:%M";

/// How much of the session id the label carries. Eight hex characters
/// distinguish every session on a real machine (24 antigravity + ~9k claude
/// sessions on the operator's own disk) while staying readable.
const ID_LEN: usize = 8;

/// Cap on a name agentbridge *derives* for an unnamed session. A name the tool
/// already had is never capped — see `display_name`. The metadata fields are
/// never truncated either: a clipped id or date would defeat the entire point
/// of the label.
const NAME_MAX: usize = 48;

/// The provider ids a label may begin with. Used by `parse` to tell a real
/// label from a title that merely contains the separator.
const PROVIDERS: &[&str] = &["claude-code", "codex-cli", "opencode", "antigravity"];

/// A label split back into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label<'a> {
    pub provider: &'a str,
    pub name: &'a str,
    pub stamp: &'a str,
    pub id: &'a str,
}

/// Recognize a label agentbridge wrote. Returns `None` for any title that is
/// not one, including titles that happen to contain the separator.
///
/// A title qualifies only when all four fields are present and well formed:
/// a known provider id, a stamp matching `STAMP`, and an id of `ID_LEN`
/// alphanumerics. Anything less is a user's own title and must survive
/// untouched.
pub fn parse(title: &str) -> Option<Label<'_>> {
    let parts: Vec<&str> = title.split(SEP).collect();
    if parts.len() < 4 {
        return None;
    }
    let provider = parts[0];
    if !PROVIDERS.contains(&provider) {
        return None;
    }
    let stamp = parts[parts.len() - 2];
    let id = parts[parts.len() - 1];
    if !is_stamp(stamp) || !is_short_id(id) {
        return None;
    }
    // Everything between the provider and the stamp is the name, so a name
    // containing the separator round-trips.
    let name_start = title.len() - id.len() - SEP.len() - stamp.len() - SEP.len();
    let name = &title[provider.len() + SEP.len()..name_start];
    Some(Label {
        provider,
        name,
        stamp,
        id,
    })
}

/// The bare session name, with any label agentbridge previously wrote removed.
pub fn strip(title: &str) -> &str {
    match parse(title) {
        Some(l) => l.name,
        None => title,
    }
}

/// `2026-08-19 10:00` — exactly `STAMP`'s shape. Checked structurally rather
/// than by reparsing, so a label is recognized even if the value is odd.
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 16 {
        return false;
    }
    let digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15];
    let dashes = [4, 7];
    digits.iter().all(|&i| b[i].is_ascii_digit())
        && dashes.iter().all(|&i| b[i] == b'-')
        && b[10] == b' '
        && b[13] == b':'
}

/// The id field: `ID_LEN` characters drawn from what real session ids contain.
///
/// Not restricted to hex or to UUID shape. Claude Code derives a session id
/// from its filename stem, so an id can be any word-ish string (a real one on
/// the operator's disk: `renamed-in-claude-code`), and a UUID's first 8
/// characters can themselves include a `-`. Accepting `-`/`_` keeps those
/// labels parseable; the length check plus the provider and stamp checks are
/// what actually distinguish a label from a user's title.
fn is_short_id(s: &str) -> bool {
    s.chars().count() == ID_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Build the label for `session`, whose `id`/`provider` must still be the
/// **origin** session's — that is what makes the same label appear in every
/// tool. (`sync_into` re-homes `project_id` but deliberately leaves both of
/// these alone.)
pub fn build(session: &Session) -> String {
    let name = display_name(session);
    // A zero or absurd time is a missing one: a store that never recorded a
    // start yields the epoch, and a value in the wrong unit yields year 58579.
    // Shown as a date either would be a lie, and the second is not even
    // stamp-shaped, so the label would stop parsing.
    let real = |t: &chrono::DateTime<chrono::Utc>| {
        use chrono::Datelike;
        t.timestamp() > 0 && t.year() <= 9999
    };
    let stamp = session
        .started_at
        .filter(real)
        .or(session.last_event_at.filter(real))
        .map(|t| t.format(STAMP).to_string())
        // A session with no timestamp anywhere is rare but must not silently
        // produce a label that looks like a different shape.
        .unwrap_or_else(|| "0000-00-00 00:00".to_string());
    let id = short_id(&session.id);
    format!(
        "{}{}{}{}{}{}{}",
        session.provider, SEP, name, SEP, stamp, SEP, id
    )
}

/// The id field: the `ID_LEN` characters of `id` that tell sessions apart.
///
/// For most tools that is the start (a UUID's first 8 hex characters). An
/// OpenCode id is `ses_` followed by a time-ordered run and only then a random
/// tail, so its first 8 characters are the same for every session created
/// within a few minutes (`ses_f045`, `ses_f046` — found live, see the test).
/// Those ids contribute their tail instead.
fn short_id(id: &str) -> String {
    let len = id.chars().count();
    if id.starts_with("ses_") && len >= ID_LEN + "ses_".len() {
        id.chars().skip(len - ID_LEN).collect()
    } else {
        id.chars().take(ID_LEN).collect()
    }
}

/// Key agentbridge writes into a copy's own first record when the target
/// format has no title to carry a label (a Codex rollout).
pub const ORIGIN_KEY: &str = "agentbridge";

/// True when `session` is a copy agentbridge made of some other session.
///
/// This is what keeps sync from copying its own copies, and it must not
/// depend on the manifest: runs race each other (the shell hook starts
/// several at once) and a manifest can be lost. A copy says so itself, in
/// one of two ways:
///
/// - its title is a label naming an origin other than the session carrying
///   it — another tool, or the same tool under another id;
/// - its first record carries [`ORIGIN_KEY`], for a format with no title;
/// - for agy, its id. agy rebuilds its index when it starts and blanks the
///   title and marker agentbridge wrote (seen live: 108 of 109 rows), so
///   neither survives there. What does survive is the id: agentbridge names
///   the conversations it writes with a version 5 UUID
///   (`antigravity_write::derive_id`), and agy's own are version 4.
pub fn is_copy(session: &Session) -> bool {
    if session.raw_payload.get(ORIGIN_KEY).is_some() {
        return true;
    }
    if session.provider == "antigravity"
        && uuid::Uuid::parse_str(&session.id).is_ok_and(|u| u.get_version_num() == 5)
    {
        return true;
    }
    session
        .title
        .as_deref()
        .and_then(parse)
        .is_some_and(|l| l.provider != session.provider || l.id != short_id(&session.id))
}

/// Replace `session.title` with its label, idempotently.
///
/// Call this once per target immediately before writing, after write-back
/// overlays have been folded in — the label must describe the session as it
/// will be written, and must be what gets recorded as "the title we wrote".
pub fn apply(session: &mut Session) {
    session.title = Some(build(session));
}

/// The `resume` command's rule, kept in one place so it can be tested: an
/// on-demand cross-tool copy is labeled exactly like a synced one, but a
/// native resume into the session's own tool (re)writes the origin file, so
/// its user-chosen title must never be replaced by a label.
pub fn apply_for_resume(session: &mut Session, target: &str) {
    if session.provider != target {
        apply(session);
    }
}

/// The name portion: the session's own name, kept **exactly** as the tool
/// recorded it, with only any label agentbridge previously wrote removed.
///
/// A name the user or the tool already chose is never truncated or reworded —
/// it is the one field of the label that is not ours to invent, and clipping it
/// would both lose information and (because the clipped form is what gets
/// recorded as "the title we wrote") make the session look renamed. Only a
/// session that has *no* name gets one derived here, from a word-safe preview
/// of its opening message.
fn display_name(session: &Session) -> String {
    if let Some(t) = session.title.as_deref() {
        // Whitespace is collapsed so the name occupies one picker row; the
        // wording itself is untouched.
        let bare = normalize_whitespace(strip(t));
        // A bare-id placeholder (`"{provider} session {id}"`) is not a name a
        // user or tool chose: versions before labels wrote it for every
        // nameless session, so keeping it verbatim would list a bare id
        // forever. Treat such a session as unnamed and derive a real name
        // from the opening message.
        if !bare.is_empty()
            && !is_generic_fallback(session, &bare)
            && !is_tool_placeholder(&bare)
            && !is_opening_message(session, &bare)
        {
            return bare;
        }
    }
    // The first thing a person actually wrote: not a tool's wrapper block
    // (`<command-name>…`, `<environment_context>…`) and not the placeholder
    // turn agentbridge itself inserts.
    let preview = session
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.text.as_deref())
        // `opencode run "…"` records the prompt with its shell quotes; they
        // are not part of what was asked.
        .map(|t| normalize_whitespace(strip_wrappers(t).trim().trim_matches('"')))
        .find(|t| !t.is_empty() && t != CONTINUATION_PLACEHOLDER)
        .unwrap_or_default();
    if preview.is_empty() {
        "(untitled)".to_string()
    } else {
        // Derived, not given — so clipping it is ours to do.
        clip(&preview)
    }
}

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when `title` is agentbridge's own generic placeholder
/// (`"{provider} session {id}"`), which versions before labels wrote into
/// copies of nameless sessions. Such a placeholder looks exactly like a bare
/// id once round-tripped, so `display_name` skips it and derives a name from
/// the opening message. A real user title never matches: the tail must be
/// id-shaped and at least 8 chars, which user-chosen wording effectively
/// never is.
fn is_generic_fallback(session: &Session, title: &str) -> bool {
    let Some(rest) = title.strip_prefix(&format!("{} session ", session.provider)) else {
        return false;
    };
    rest.chars().count() >= ID_LEN
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// True when `title` is just the session's whole opening message. A tool with
/// no name for a session shows that text as a preview (agy's summaries do),
/// and a reader hands it through as the title. Nobody chose it, so it is
/// named like any other unnamed session: from the same text, clipped.
fn is_opening_message(session: &Session, title: &str) -> bool {
    session
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .and_then(|m| m.text.as_deref())
        .is_some_and(|t| normalize_whitespace(t) == title)
}

/// True when `title` is a placeholder a *tool* wrote for a session nobody
/// named. OpenCode's is `New session - <ISO timestamp>`; kept verbatim it puts
/// the date in the label twice and no name at all. Only the exact shape
/// matches, so a title a person typed that starts the same way is kept.
fn is_tool_placeholder(title: &str) -> bool {
    title
        .strip_prefix("New session - ")
        .is_some_and(|rest| chrono::DateTime::parse_from_rfc3339(rest).is_ok())
}

/// The user turn agentbridge inserts when a history would otherwise open with
/// an assistant turn (`opencode_write`). It is ours, so it never names a
/// session.
pub const CONTINUATION_PLACEHOLDER: &str = "(agentbridge: continuing a previous conversation)";

/// `text` with any leading tool wrapper blocks removed.
///
/// Tools record their own bookkeeping as user turns wrapped in tags —
/// `<command-name>/resume</command-name>`, `<environment_context>…`,
/// `<system-reminder>…`. A wrapper is recognized by a tag name containing `-`
/// or `_`, which those all have and ordinary markup a person types (`<div>`,
/// `<p>`) does not. An unclosed tag is left alone rather than guessed at.
fn strip_wrappers(text: &str) -> &str {
    let mut s = text;
    loop {
        let t = s.trim_start();
        let Some(rest) = t.strip_prefix('<') else {
            return t;
        };
        let name_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(rest.len());
        let name = &rest[..name_len];
        if !(name.contains('-') || name.contains('_')) {
            return t;
        }
        let Some(open_end) = rest.find('>') else {
            return t;
        };
        let close = format!("</{}>", name);
        let Some(pos) = rest[open_end..].find(&close) else {
            return t;
        };
        s = &rest[open_end + pos + close.len()..];
    }
}

/// True when `text` is nothing but tool wrapper blocks: a turn a tool wrote
/// for itself, not something a person or a model said.
pub fn is_bookkeeping(text: &str) -> bool {
    !text.trim().is_empty() && strip_wrappers(text).trim().is_empty()
}

/// Truncate on a word boundary. A name that changed shape between runs would
/// read as a rename, so this must be a pure function of its input.
fn clip(s: &str) -> String {
    if s.chars().count() <= NAME_MAX {
        return s.to_string();
    }
    let truncated: String = s.chars().take(NAME_MAX).collect();
    let cut = match truncated.rsplit_once(' ') {
        Some((head, _)) if !head.is_empty() => head.to_string(),
        _ => truncated,
    };
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, TokenTotals};
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;

    fn msg(role: Role, text: &str) -> Message {
        Message {
            session_id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
            ordinal: 0,
            role,
            timestamp: Utc.timestamp_opt(1_785_492_000, 0).single(),
            text: Some(text.to_string()),
            tool_name: None,
            tool_input: None,
            tool_result: None,
            parent_ordinal: None,
        }
    }

    fn session(provider: &str, title: Option<&str>) -> Session {
        Session {
            id: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
            provider: provider.to_string(),
            project_id: "/tmp/proj".to_string(),
            started_at: Utc.timestamp_opt(1_785_492_000, 0).single(),
            last_event_at: Utc.timestamp_opt(1_785_492_600, 0).single(),
            model: None,
            title: title.map(|t| t.to_string()),
            token_totals: TokenTotals::default(),
            source_path: PathBuf::from("/tmp/src.jsonl"),
            raw_payload: serde_json::Value::Null,
            body_available: true,
            messages: vec![msg(Role::User, "wire up the bridge")],
            artifacts: vec![],
        }
    }

    #[test]
    fn test_label_carries_agent_name_date_and_id() {
        let s = session("claude-code", Some("Wire up the bridge"));
        let label = build(&s);
        assert_eq!(
            label,
            "claude-code · Wire up the bridge · 2026-07-31 10:00 · aaaaaaaa"
        );
        let parsed = parse(&label).expect("own label must parse");
        assert_eq!(parsed.provider, "claude-code");
        assert_eq!(parsed.name, "Wire up the bridge");
        assert_eq!(parsed.stamp, "2026-07-31 10:00");
        assert_eq!(parsed.id, "aaaaaaaa");
    }

    /// The exact complaint found live 2026-09-09: pre-label agentbridge wrote
    /// the generic `"{provider} session {id}"` placeholder into nameless
    /// copies, and it round-tripped into their source titles. Keeping it
    /// verbatim would list a bare id in every picker forever, so the label
    /// must treat it as "no name" and derive a real one from the opening
    /// message instead.
    #[test]
    fn test_generic_placeholder_is_not_kept_as_a_name() {
        let s = session(
            "claude-code",
            Some("claude-code session aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
        );
        let label = build(&s);
        let parsed = parse(&label).expect("own label must parse");
        assert_eq!(
            parsed.name,
            "wire up the bridge",
            "the placeholder must be replaced by the message preview"
        );
    }

    /// Seen on the operator's real machine: `1970-01-01 00:00` for sessions
    /// whose store never recorded a start, and `+58579-08-17 12:37` for one
    /// read in the wrong unit. The second broke the label's own shape, so the
    /// next sync wrapped it in another label.
    #[test]
    fn test_zero_and_absurd_times_are_not_shown_as_dates() {
        let mut s = session("antigravity", Some("Name"));
        s.started_at = Utc.timestamp_opt(0, 0).single();
        assert!(build(&s).contains("2026-07-31 10:10"), "falls back to the last event: {}", build(&s));
        s.last_event_at = Utc.timestamp_opt(1_790_927_716_000, 0).single();
        let label = build(&s);
        assert!(label.contains("0000-00-00 00:00"), "no real time at all: {label}");
        assert!(parse(&label).is_some(), "and it is still a label: {label}");
    }

    /// Found live with agy 1.1.27: an untitled conversation's title is its
    /// preview, the whole first message, so the name ran to 131 characters
    /// while the same kind of session from other tools was clipped.
    #[test]
    fn test_a_title_that_is_only_the_opening_message_is_clipped_like_a_derived_name() {
        let long = "Do not use any tools. In one short sentence, why are timestamps \
                    usually stored in UTC? Also remember my project codename is LYCHEE.";
        let mut s = session("antigravity", Some(long));
        s.messages = vec![msg(Role::User, long)];
        let name = parse(&build(&s)).unwrap().name.to_string();
        assert!(name.ends_with('…') && name.chars().count() <= NAME_MAX + 1, "{name}");
        // A short opening message used as the title is unchanged either way.
        let mut short = session("antigravity", Some("say hi"));
        short.messages = vec![msg(Role::User, "say hi")];
        assert_eq!(parse(&build(&short)).unwrap().name, "say hi");
    }

    /// A copy is told apart from the session it was made from by its own
    /// title: the label names an origin that is not the session carrying it.
    #[test]
    fn test_a_copy_is_recognized_by_a_label_naming_another_session() {
        // A Claude session written into Codex or OpenCode: other tool.
        let mut copy = session("opencode", Some("claude-code · Fix login · 2026-07-31 10:00 · aaaaaaaa"));
        copy.id = "ses_ab0123456789abcdef0123456789abcdef".to_string();
        assert!(is_copy(&copy));
        // The same tool, another id (a per directory copy).
        let mut same_tool = session("claude-code", Some("claude-code · Fix login · 2026-07-31 10:00 · 99ba641d"));
        same_tool.id = "1b690202-f9de-5a45-8154-5ace4b5f0000".to_string();
        assert!(is_copy(&same_tool));
    }

    /// Found on the real agy store: after agy restarted, the copies agentbridge
    /// had written there had blank titles and no marker left, so the next
    /// sync would have copied all of them out again as agy's own sessions.
    #[test]
    fn test_an_agy_copy_is_recognized_by_its_id_when_agy_has_wiped_its_title() {
        let mut wiped = session("antigravity", None);
        wiped.id = crate::antigravity_write::derive_id("claude-code", "abc", "/tmp/p");
        assert!(is_copy(&wiped));
        let mut own = session("antigravity", None);
        own.id = "91fca480-790f-49e2-bc1a-27b28377e120".to_string();
        assert!(!is_copy(&own), "agy's own conversations are version 4");
    }

    /// A session a tool owns is never a copy: unlabeled, named by a person, or
    /// (after a merge back) labeled as itself.
    #[test]
    fn test_a_tools_own_session_is_not_a_copy() {
        assert!(!is_copy(&session("claude-code", None)));
        assert!(!is_copy(&session("claude-code", Some("Fix login"))));
        assert!(!is_copy(&session("claude-code", Some("docs · a note"))));
        assert!(!is_copy(&session(
            "claude-code",
            Some("claude-code · Fix login · 2026-07-31 10:00 · aaaaaaaa")
        )));
    }

    /// A Codex rollout holds no title, so the copy carries its origin in the
    /// first record instead, and the reader hands that through.
    #[test]
    fn test_a_copy_is_recognized_by_the_origin_recorded_in_its_payload() {
        let mut s = session("codex-cli", None);
        s.raw_payload = serde_json::json!({"agentbridge": {"source_provider": "claude-code"}});
        assert!(is_copy(&s));
    }

    /// Found live 2026-10-02 against opencode 1.18.30: a session nobody named
    /// carries OpenCode's own placeholder, `New session - <ISO timestamp>`.
    /// Kept verbatim it produced `opencode · New session - 2026-10-02T07:56:04.904Z
    /// · 2026-10-02 07:56 · ses_f046` — the date twice and no name at all.
    #[test]
    fn test_opencode_placeholder_title_is_not_kept_as_a_name() {
        let s = session("opencode", Some("New session - 2026-10-02T07:56:04.904Z"));
        assert_eq!(parse(&build(&s)).unwrap().name, "wire up the bridge");
    }

    /// Only the exact placeholder shape counts. A title a person typed that
    /// merely starts the same way is theirs and must be kept.
    #[test]
    fn test_real_title_starting_like_the_opencode_placeholder_is_kept() {
        for title in ["New session - notes", "New session", "New session - 2026 plan"] {
            let s = session("opencode", Some(title));
            assert_eq!(parse(&build(&s)).unwrap().name, title);
        }
    }

    /// Seen on the operator's real manifest: hundreds of derived names were
    /// `<command-name>/resume</command-name>…`, `<environment_context>…` or
    /// `(agentbridge: continuing a previous…`. None of those is what the
    /// session is about; the name must come from the first thing a person
    /// actually wrote.
    #[test]
    fn test_derived_name_skips_tool_wrappers_and_agentbridge_placeholder() {
        let mut s = session("claude-code", None);
        s.messages = vec![
            msg(Role::User, "(agentbridge: continuing a previous conversation)"),
            msg(
                Role::User,
                "<command-name>/resume</command-name>\n<command-message>resume</command-message>",
            ),
            msg(
                Role::User,
                "<environment_context>\n  <cwd>/tmp/x</cwd>\n</environment_context>",
            ),
            msg(Role::User, "fix the login redirect"),
        ];
        assert_eq!(parse(&build(&s)).unwrap().name, "fix the login redirect");
    }

    /// A wrapper block in front of real text is dropped; the text is the name.
    #[test]
    fn test_derived_name_drops_a_leading_wrapper_block() {
        let mut s = session("codex-cli", None);
        s.messages = vec![msg(
            Role::User,
            "<system-reminder>be brief</system-reminder>\nadd a retry to the uploader",
        )];
        assert_eq!(parse(&build(&s)).unwrap().name, "add a retry to the uploader");
    }

    /// Ordinary markup a person typed is their wording, not a tool wrapper, and
    /// an unclosed wrapper is left alone rather than guessed at.
    #[test]
    fn test_derived_name_keeps_text_that_is_not_a_tool_wrapper() {
        for text in ["<div>hello</div> center this", "<local-command-stdout> never closed"] {
            let mut s = session("claude-code", None);
            s.messages = vec![msg(Role::User, text)];
            assert_eq!(parse(&build(&s)).unwrap().name, text);
        }
    }

    /// Found live: `opencode run "…"` stores the prompt wrapped in the quotes
    /// it was typed with, so every derived name began with a stray `"`.
    #[test]
    fn test_derived_name_drops_quotes_wrapping_the_whole_prompt() {
        let mut s = session("opencode", None);
        s.messages = vec![msg(Role::User, "\"Reply with exactly: noted.\"")];
        assert_eq!(parse(&build(&s)).unwrap().name, "Reply with exactly: noted.");
    }

    /// When every message is a wrapper there is nothing to name it by.
    #[test]
    fn test_session_of_only_wrappers_is_untitled() {
        let mut s = session("claude-code", None);
        s.messages = vec![msg(Role::User, "<command-name>/clear</command-name>")];
        assert_eq!(parse(&build(&s)).unwrap().name, "(untitled)");
    }

    /// Found live 2026-10-02: OpenCode ids are `ses_` plus a time-ordered
    /// prefix, so the first 8 characters were `ses_f045` / `ses_f046` for every
    /// session made within the same few minutes — the id field told nothing
    /// apart. The random part is at the end.
    #[test]
    fn test_opencode_ids_use_the_distinguishing_tail() {
        let mut a = session("opencode", Some("A"));
        a.id = "ses_f04630257ffepyyOZrwjoHsBpm".to_string();
        let mut b = session("opencode", Some("B"));
        b.id = "ses_f04624513ffe2hRisW55eg5M31".to_string();
        let la = build(&a);
        let lb = build(&b);
        let (pa, pb) = (parse(&la).unwrap(), parse(&lb).unwrap());
        assert_eq!(pa.id, "wjoHsBpm");
        assert_eq!(pb.id, "55eg5M31");
        assert_ne!(pa.id, pb.id, "two sessions must not share an id field");
        // Still idempotent.
        let mut again = a.clone();
        again.title = Some(la.clone());
        apply(&mut again);
        assert_eq!(again.title.unwrap(), la);
    }

    /// Labels written before the tail rule carried `ses_f045`. They must still
    /// be recognized, or the old label would be nested inside the new one.
    #[test]
    fn test_old_opencode_prefix_label_is_still_stripped() {
        let mut s = session("opencode", None);
        s.id = "ses_f04630257ffepyyOZrwjoHsBpm".to_string();
        s.title = Some("opencode · Tidy the cache · 2026-07-31 10:00 · ses_f046".to_string());
        let label = build(&s);
        assert_eq!(label, "opencode · Tidy the cache · 2026-07-31 10:00 · wjoHsBpm");
    }

    /// A real title that merely *starts* like the placeholder (but is not
    /// id-shaped) is a user-chosen name and must be kept exactly.
    #[test]
    fn test_real_title_looking_like_a_placeholder_is_kept() {
        let s = session("claude-code", Some("claude-code session notes"));
        let label = build(&s);
        let parsed = parse(&label).expect("own label must parse");
        assert_eq!(parsed.name, "claude-code session notes");
    }

    /// The whole point: the same origin session labels identically no matter
    /// which tool it is being written into, so four picker rows correlate.
    #[test]
    fn test_label_is_identical_across_targets() {
        let s = session("codex-cli", Some("Shared work"));
        let first = build(&s);
        // `sync_into` re-homes project_id per target; the label must not move.
        let mut other = s.clone();
        other.project_id = "/Users/harry".to_string();
        assert_eq!(build(&other), first);
    }

    /// A moving label would report every session as renamed on every pull.
    #[test]
    fn test_label_is_stable_and_uses_session_start_not_now() {
        let s = session("opencode", Some("Stable"));
        let a = build(&s);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = build(&s);
        assert_eq!(a, b, "the label must not depend on the current time");
        assert!(a.contains("2026-07-31 10:00"), "session start: {}", a);
        assert!(
            !a.contains(&Utc::now().format("%Y-%m-%d").to_string()),
            "must never carry the sync date: {}",
            a
        );
    }

    /// Re-labeling must rebuild, never nest — the migration hazard for
    /// manifests written before labels existed.
    #[test]
    fn test_applying_a_label_twice_does_not_nest() {
        let mut s = session("antigravity", Some("Original"));
        apply(&mut s);
        let once = s.title.clone().unwrap();
        apply(&mut s);
        let twice = s.title.clone().unwrap();
        assert_eq!(once, twice, "labeling is idempotent");
        assert_eq!(
            twice.matches("antigravity").count(),
            1,
            "provider appears once: {}",
            twice
        );
        assert_eq!(parse(&twice).unwrap().name, "Original");
    }

    /// A rename made inside a tool arrives as the labeled title; the new label
    /// must be built around the *user's* name, not around the old label.
    #[test]
    fn test_rename_inside_a_tool_replaces_the_name_field() {
        let mut s = session("claude-code", Some("Before"));
        apply(&mut s);
        // The user renames the materialized copy; pull_back stores that title.
        s.title = Some("claude-code · After · 2026-07-31 10:00 · aaaaaaaa".to_string());
        apply(&mut s);
        assert_eq!(parse(s.title.as_deref().unwrap()).unwrap().name, "After");
    }

    #[test]
    fn test_untitled_session_falls_back_to_a_word_safe_preview() {
        let s = session("codex-cli", None);
        let label = build(&s);
        assert_eq!(parse(&label).unwrap().name, "wire up the bridge");
    }

    #[test]
    fn test_session_with_no_title_and_no_text_still_labels() {
        let mut s = session("codex-cli", None);
        s.messages.clear();
        let label = build(&s);
        let p = parse(&label).expect("must still be a valid label");
        assert_eq!(p.name, "(untitled)");
        assert_eq!(p.id, "aaaaaaaa");
    }

    /// A name the tool already has is the user's, so it survives verbatim
    /// however long it is — truncating it would lose information and, because
    /// the written title is what `pull_back` compares against, would also read
    /// as a rename.
    #[test]
    fn test_existing_session_name_is_never_truncated() {
        let long = "refactor the authentication middleware so that it validates \
                    the bearer token before touching the database at all";
        let s = session("claude-code", Some(long));
        let label = build(&s);
        let p = parse(&label).expect("a long label still parses");
        assert_eq!(p.name, long, "an existing name is kept exactly");
        assert!(!p.name.ends_with('…'), "never ellipsized: {}", p.name);
        assert_eq!(p.id, "aaaaaaaa", "id must never be cut");
        assert_eq!(p.stamp, "2026-07-31 10:00", "date must never be cut");
        assert_eq!(build(&s), label, "and it is deterministic");
    }

    /// Only a name agentbridge *derives* (from the opening message of a session
    /// that has no name) is clipped — that string is ours, not the user's.
    #[test]
    fn test_derived_name_for_an_unnamed_session_is_clipped() {
        let long = "refactor the authentication middleware so that it validates \
                    the bearer token before touching the database at all";
        let mut s = session("claude-code", None);
        s.messages = vec![msg(Role::User, long)];
        let label = build(&s);
        let p = parse(&label).expect("a clipped label still parses");
        assert!(p.name.chars().count() <= NAME_MAX + 1, "name: {}", p.name);
        assert!(p.name.ends_with('…'), "derived names are marked: {}", p.name);
        assert!(long.starts_with(p.name.trim_end_matches('…')), "word-safe prefix");
        assert_eq!(p.id, "aaaaaaaa");
        assert_eq!(build(&s), label, "and clipping is deterministic");
    }

    /// Whitespace is collapsed so the name occupies one picker row, but the
    /// wording is not otherwise touched.
    #[test]
    fn test_existing_name_only_has_whitespace_normalized() {
        let s = session("opencode", Some("  Fix   the\n bridge  "));
        assert_eq!(parse(&build(&s)).unwrap().name, "Fix the bridge");
    }

    /// A user's own title must never be mistaken for a label and mangled.
    #[test]
    fn test_user_titles_are_not_parsed_as_labels() {
        for title in [
            "just a name",
            // Contains the separator but is not a label.
            "docs · a note",
            "a · b · c · d",
            // Right shape, unknown provider.
            "kilo-code · x · 2026-07-31 10:00 · aaaaaaaa",
            // Known provider, malformed stamp.
            "claude-code · x · yesterday · aaaaaaaa",
            // Known provider, id too short.
            "claude-code · x · 2026-07-31 10:00 · aaa",
        ] {
            assert!(parse(title).is_none(), "must not parse: {}", title);
            assert_eq!(strip(title), title, "must survive untouched: {}", title);
        }
    }

    /// Session ids are not always UUIDs: Claude Code derives one from the
    /// filename stem, so an id can contain `-`. A stricter id check rejected
    /// those labels and `pull_back` then saw its own label as a foreign
    /// rename.
    #[test]
    fn test_non_uuid_session_ids_still_produce_parseable_labels() {
        for id in [
            "renamed-in-claude-code",
            "my_session_name",
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        ] {
            let mut s = session("claude-code", Some("Name"));
            s.id = id.to_string();
            let label = build(&s);
            let p = parse(&label)
                .unwrap_or_else(|| panic!("must parse for id {:?}: {:?}", id, label));
            assert_eq!(p.name, "Name");
            assert!(id.starts_with(p.id), "label id must prefix {:?}", id);
            // Still idempotent for these ids.
            let mut again = s.clone();
            again.title = Some(label.clone());
            apply(&mut again);
            assert_eq!(again.title.unwrap(), label);
        }
    }

    /// A name that itself contains the separator must round-trip.
    #[test]
    fn test_name_containing_the_separator_round_trips() {
        let s = session("opencode", Some("docs · a note"));
        let label = build(&s);
        assert_eq!(parse(&label).unwrap().name, "docs · a note");
        let mut again = s.clone();
        again.title = Some(label.clone());
        apply(&mut again);
        assert_eq!(again.title.unwrap(), label, "still idempotent");
    }

    /// A `resume` copy written into *another* tool is labeled exactly like a
    /// synced copy: the original name is kept, and the date is the session's
    /// own start — never the day the resume happened.
    #[test]
    fn test_resume_copy_across_tools_is_labeled_with_original_date_and_name() {
        let mut s = session("claude-code", Some("Payroll migration"));
        apply_for_resume(&mut s, "opencode");
        let label = parse(s.title.as_deref().unwrap()).expect("resumed copy is labeled");
        assert_eq!(label.provider, "claude-code");
        assert_eq!(label.name, "Payroll migration");
        assert_eq!(label.stamp, "2026-07-31 10:00", "original date, not the resume date");
        assert_eq!(label.id, "aaaaaaaa");
    }

    /// Resuming a session inside its own tool rewrites the origin file, whose
    /// title is the user's — `apply_for_resume` must leave it untouched.
    #[test]
    fn test_native_resume_never_relabels_the_origin() {
        let mut s = session("claude-code", Some("My Important Session"));
        apply_for_resume(&mut s, "claude-code");
        assert_eq!(s.title.as_deref(), Some("My Important Session"));
    }
}
