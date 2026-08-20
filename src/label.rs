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
//! from its opening message.

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
    let stamp = session
        .started_at
        .or(session.last_event_at)
        .map(|t| t.format(STAMP).to_string())
        // A session with no timestamp anywhere is rare but must not silently
        // produce a label that looks like a different shape.
        .unwrap_or_else(|| "0000-00-00 00:00".to_string());
    let id: String = session.id.chars().take(ID_LEN).collect();
    format!(
        "{}{}{}{}{}{}{}",
        session.provider, SEP, name, SEP, stamp, SEP, id
    )
}

/// Replace `session.title` with its label, idempotently.
///
/// Call this once per target immediately before writing, after write-back
/// overlays have been folded in — the label must describe the session as it
/// will be written, and must be what gets recorded as "the title we wrote".
pub fn apply(session: &mut Session) {
    session.title = Some(build(session));
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
        if !bare.is_empty() {
            return bare;
        }
    }
    let preview = session
        .messages
        .iter()
        .find(|m| m.role == Role::User && m.text.as_deref().is_some_and(|t| !t.trim().is_empty()))
        .and_then(|m| m.text.as_deref())
        .map(normalize_whitespace)
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
}
