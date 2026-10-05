//! Secret redaction (SPEC.md §3).
//!
//! Transcripts contain API keys, tokens and `.env` dumps. agentbridge copies
//! whole transcripts into other tools' stores, so every derived copy goes
//! through a `Redactor` first. Originals are never touched: redaction applies
//! to what agentbridge *writes* (cache, other tools' files and rows, the
//! overlay, injected briefs), not to what it reads.
//!
//! Two properties matter more than coverage:
//!
//! * **Fail closed.** A rules file that cannot be read or parsed is an error
//!   the caller must treat as "write nothing" — never "carry on unredacted".
//!   The only way to skip redaction is the explicit, per-process
//!   [`disable_for_this_process`] opt-out, which unattended paths never call.
//! * **Idempotent.** `redact(redact(x)) == redact(x)`. Write-back compares
//!   turns by their text, so a copy that redacted differently on a second
//!   pass would show up as new work on every pull.
//!
//! The `regex` crate matches in linear time, so a hostile transcript cannot
//! stall a sync with a pathological pattern.

use crate::model::{Message, Session};
use regex::{Captures, Regex, RegexSet};
use serde_json::Value;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Every replacement starts with this, which is how a second pass recognises
/// text it already redacted and leaves it alone.
const TAG_OPEN: &str = "[REDACTED";

/// File in the data dir holding user-defined rules (see [`parse_user_rules`]).
pub const USER_RULES_FILE: &str = "redact.rules";

static DISABLED: AtomicBool = AtomicBool::new(false);

/// Explicit opt-out for one process (`--no-redact`). Only interactive commands
/// may call this; the shell hook and `auto watch` never do.
pub fn disable_for_this_process() {
    DISABLED.store(true, Ordering::SeqCst);
}

/// Tests that need the opt-out flip it here and must reset it before the
/// sandbox lock is released.
#[cfg(test)]
pub(crate) fn set_disabled_for_test(disabled: bool) {
    DISABLED.store(disabled, Ordering::SeqCst);
}

pub fn is_disabled() -> bool {
    DISABLED.load(Ordering::SeqCst)
}

#[derive(Debug, thiserror::Error)]
pub enum RedactError {
    #[error("cannot read redaction rules {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} line {line}: {message}")]
    BadRule {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

#[derive(Clone)]
struct Rule {
    name: String,
    re: Regex,
    /// Index of the capture group holding the secret itself; `0` replaces the
    /// whole match. Rules keep the surrounding text (`KEY=`, `Bearer `) so a
    /// redacted transcript still reads sensibly.
    secret_group: usize,
}

/// Default ruleset: `(name, pattern)`. A named group `secret` marks the part to
/// replace; without one the whole match goes.
const DEFAULT_RULES: &[(&str, &str)] = &[
    (
        "private-key",
        // An unterminated block (a truncated transcript) redacts to the end
        // of the text rather than leaking the half that is there.
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?:[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|[\s\S]*)",
    ),
    (
        "aws-access-key-id",
        r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|AIPA|ANPA|ANVA)[A-Z0-9]{16}\b",
    ),
    (
        "aws-secret-key",
        r#"(?i)aws_?secret_?access_?key["']?\s*[:=]\s*["']?(?P<secret>[A-Za-z0-9/+=]{40})"#,
    ),
    (
        "github-token",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b",
    ),
    ("gitlab-token", r"\bglpat-[A-Za-z0-9_\-]{20,}"),
    // OpenAI (`sk-`, `sk-proj-`), Anthropic (`sk-ant-`) and look-alikes.
    ("sk-key", r"\bsk-[A-Za-z0-9_\-]{20,}"),
    ("stripe-key", r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}"),
    ("slack-token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
    ("google-api-key", r"\bAIza[0-9A-Za-z_\-]{35}"),
    ("npm-token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    ("huggingface-token", r"\bhf_[A-Za-z0-9]{30,}"),
    (
        "sendgrid-key",
        r"\bSG\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}",
    ),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
    ),
    (
        "bearer-token",
        r"(?i)\bbearer\s+(?P<secret>[A-Za-z0-9\-._~+/]{16,}=*)",
    ),
    (
        "authorization-header",
        r#"(?i)\bauthorization["']?\s*[:=]\s*["']?(?:basic|digest|token|bearer)?\s*(?P<secret>[A-Za-z0-9\-._~+/=]{8,})"#,
    ),
    (
        // `scheme://user:PASSWORD@host`; only the password is replaced.
        "url-password",
        r#"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s:/@"'<>]+:(?P<secret>[^\s@/"'<>]+)@"#,
    ),
    (
        // Upper-case env style (`export DB_PASSWORD=...`). Case-sensitive on
        // purpose: `max_tokens = 4096` is configuration, not a credential.
        "env-secret",
        r#"(?m)\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API_?KEY|PRIVATE_KEY|ACCESS_KEY|CREDENTIAL)[A-Z0-9_]*\s*[=:]\s*["']?(?P<secret>[^\s"']{4,})"#,
    ),
    (
        // `password: hunter2`, `"api_key": "..."`, `jwt_secret = "..."` in
        // config, YAML and JSON. The key may carry a prefix (`db_password`,
        // `jwt_secret`) but must *end* in the keyword, so `password_hash` and
        // `secret_count` are left alone. Bare `token` is excluded for the same
        // reason as above. The value
        // must be 6+ characters so prose like `secret: true` survives; a
        // 4-5 character password in free text is a known miss (SECURITY.md).
        "credential-assignment",
        r#"(?i)\b[\w.\-]*(?:password|passwd|pwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token)["']?\s*[:=]\s*["']?(?P<secret>[^\s"',;]{6,})"#,
    ),
];

/// JSON object keys whose string value is always a secret, whatever it looks
/// like. Compared after lower-casing and dropping `-`/`_`.
fn is_sensitive_key(key: &str) -> bool {
    let k: String = key
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(|c| c.to_lowercase())
        .collect();
    const EXACT: &[&str] = &["authorization", "cookie", "setcookie", "token"];
    const CONTAINS: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "apikey",
        "privatekey",
        "accesskey",
        "credential",
    ];
    // `access_token`, `id_token`, `refresh_token` — but not `max_tokens`,
    // which is a count and never a string anyway.
    EXACT.contains(&k.as_str()) || k.ends_with("token") || CONTAINS.iter().any(|c| k.contains(c))
}

fn compile(name: &str, pattern: &str) -> Result<Rule, String> {
    let re = Regex::new(pattern).map_err(|e| e.to_string())?;
    let secret_group = re
        .capture_names()
        .position(|n| n == Some("secret"))
        .unwrap_or(0);
    Ok(Rule {
        name: name.to_string(),
        re,
        secret_group,
    })
}

#[derive(Clone)]
pub struct Redactor {
    enabled: bool,
    rules: Vec<Rule>,
    /// One pass over the text decides whether any rule can match at all; most
    /// text has no secret and never reaches the per-rule replacement.
    set: RegexSet,
}

fn default_redactor() -> &'static Redactor {
    static DEFAULT: OnceLock<Redactor> = OnceLock::new();
    DEFAULT.get_or_init(|| {
        let rules: Vec<Rule> = DEFAULT_RULES
            .iter()
            .map(|(n, p)| compile(n, p).unwrap_or_else(|e| panic!("default rule {n}: {e}")))
            .collect();
        Redactor::from_rules(rules, true)
    })
}

impl Redactor {
    fn from_rules(rules: Vec<Rule>, enabled: bool) -> Self {
        let set = RegexSet::new(rules.iter().map(|r| r.re.as_str()))
            .unwrap_or_else(|e| panic!("redaction rule set: {e}"));
        Self {
            enabled,
            rules,
            set,
        }
    }

    /// The built-in rules only. Never reads the filesystem and cannot fail.
    pub fn defaults() -> Self {
        default_redactor().clone()
    }

    /// A pass-through. Only reachable through [`Redactor::load`] after
    /// [`disable_for_this_process`], or explicitly in tests.
    pub fn disabled() -> Self {
        Self::from_rules(Vec::new(), false)
    }

    /// Built-in rules plus the user's `redact.rules` from the data dir.
    ///
    /// An unreadable or invalid rules file is an error: callers must abort the
    /// write rather than fall back to the defaults, because the user asked for
    /// stricter redaction than the defaults give.
    pub fn load() -> Result<Self, RedactError> {
        if is_disabled() {
            return Ok(Self::disabled());
        }
        let path = crate::sync::data_dir().join(USER_RULES_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::defaults());
            }
            Err(source) => return Err(RedactError::Io { path, source }),
        };
        let user = parse_user_rules(&text, &path)?;
        let mut rules = default_redactor().rules.clone();
        rules.extend(user);
        Ok(Self::from_rules(rules, true))
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Redact one string. Returns the text unchanged (and `0`) when nothing
    /// matched.
    pub fn text(&self, input: &str) -> (String, usize) {
        if !self.enabled || input.is_empty() || !self.set.is_match(input) {
            return (input.to_string(), 0);
        }
        let hits = Cell::new(0usize);
        let mut out = input.to_string();
        for idx in self.set.matches(input).iter() {
            let rule = &self.rules[idx];
            let replaced = rule.re.replace_all(&out, |caps: &Captures| {
                let whole = &caps[0];
                let (start, end) = match caps.get(rule.secret_group) {
                    Some(m) => (
                        m.start() - caps.get(0).unwrap().start(),
                        m.end() - caps.get(0).unwrap().start(),
                    ),
                    None => (0, whole.len()),
                };
                // Already redacted by an earlier pass or an earlier rule.
                if whole[start..end].starts_with(TAG_OPEN) {
                    return whole.to_string();
                }
                hits.set(hits.get() + 1);
                format!(
                    "{}{}:{}]{}",
                    &whole[..start],
                    TAG_OPEN,
                    rule.name,
                    &whole[end..]
                )
            });
            out = replaced.into_owned();
        }
        (out, hits.get())
    }

    /// Redact every string inside a JSON value, in place. Values under a
    /// sensitive key (`password`, `authorization`, `*_token`, ...) are replaced
    /// outright, since a tool call's arguments are often structured and the
    /// secret has no recognisable shape.
    pub fn value(&self, v: &mut Value) -> usize {
        if !self.enabled {
            return 0;
        }
        match v {
            Value::String(s) => {
                let (out, n) = self.text(s);
                if n > 0 {
                    *s = out;
                }
                n
            }
            Value::Array(items) => items.iter_mut().map(|i| self.value(i)).sum(),
            Value::Object(map) => {
                let mut n = 0;
                for (k, val) in map.iter_mut() {
                    match val {
                        Value::String(s)
                            if !s.is_empty() && !s.starts_with(TAG_OPEN) && is_sensitive_key(k) =>
                        {
                            *s = format!("{}:{}]", TAG_OPEN, "sensitive-key");
                            n += 1;
                        }
                        _ => n += self.value(val),
                    }
                }
                n
            }
            _ => 0,
        }
    }

    /// Redact one turn's text and tool payloads, in place.
    pub fn message(&self, m: &mut Message) -> usize {
        if !self.enabled {
            return 0;
        }
        let mut n = 0;
        if let Some(t) = m.text.as_mut() {
            let (out, k) = self.text(t);
            if k > 0 {
                *t = out;
                n += k;
            }
        }
        if let Some(v) = m.tool_input.as_mut() {
            n += self.value(v);
        }
        if let Some(v) = m.tool_result.as_mut() {
            n += self.value(v);
        }
        n
    }

    /// Redact everything in a session that agentbridge could write out.
    /// Returns how many secrets were replaced.
    pub fn session(&self, s: &mut Session) -> usize {
        if !self.enabled {
            return 0;
        }
        let mut n = 0;
        if let Some(t) = s.title.as_mut() {
            let (out, k) = self.text(t);
            if k > 0 {
                *t = out;
                n += k;
            }
        }
        for m in &mut s.messages {
            n += self.message(m);
        }
        for a in &mut s.artifacts {
            let (out, k) = self.text(&a.path_or_command);
            if k > 0 {
                a.path_or_command = out;
                n += k;
            }
            if let Some(d) = a.detail.as_mut() {
                let (out, k) = self.text(d);
                if k > 0 {
                    *d = out;
                    n += k;
                }
            }
        }
        n += self.value(&mut s.raw_payload);
        n
    }
}

/// Parse a user rules file: one `name = regex` per line; blank lines and lines
/// starting with `#` are ignored. A named group `(?P<secret>...)` limits the
/// replacement to that group.
///
/// Any malformed line is an error. Silently skipping a rule the user wrote
/// would leave the thing they wanted redacted in the clear.
fn parse_user_rules(text: &str, path: &Path) -> Result<Vec<Rule>, RedactError> {
    let mut rules = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = |message: String| RedactError::BadRule {
            path: path.to_path_buf(),
            line: i + 1,
            message,
        };
        let Some((name, pattern)) = line.split_once('=') else {
            return Err(bad("expected `name = regex`".to_string()));
        };
        let name = name.trim();
        let pattern = pattern.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(bad(format!(
                "rule name `{name}` must be lowercase letters, digits and `-`"
            )));
        }
        if pattern.is_empty() {
            return Err(bad("empty pattern".to_string()));
        }
        rules.push(compile(name, pattern).map_err(|e| bad(format!("invalid regex: {e}")))?);
    }
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn r() -> Redactor {
        Redactor::defaults()
    }

    fn redacted(s: &str) -> String {
        r().text(s).0
    }

    fn assert_redacted(secret_input: &str, secret: &str) {
        let out = redacted(secret_input);
        assert!(
            !out.contains(secret),
            "secret survived: {out:?} (from {secret_input:?})"
        );
        assert!(out.contains(TAG_OPEN), "no tag in {out:?}");
    }

    // ---- positive cases: one per default rule -------------------------

    #[test]
    fn test_aws_access_key_id_is_redacted() {
        assert_redacted("key AKIAIOSFODNN7EXAMPLE here", "AKIAIOSFODNN7EXAMPLE");
    }

    #[test]
    fn test_aws_secret_key_keeps_the_name_drops_the_value() {
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let out = redacted(&format!("aws_secret_access_key = {secret}"));
        assert!(!out.contains(secret));
        assert!(out.contains("aws_secret_access_key"), "{out}");
    }

    #[test]
    fn test_private_key_block_is_redacted_whole() {
        let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nabcdEFGH1234";
        let pem = format!(
            "before\n-----BEGIN RSA PRIVATE KEY-----\n{body}\n-----END RSA PRIVATE KEY-----\nafter"
        );
        let out = redacted(&pem);
        assert!(
            !out.contains("MIIEvQ") && !out.contains("abcdEFGH1234"),
            "{out}"
        );
        assert!(
            out.starts_with("before\n") && out.ends_with("\nafter"),
            "{out}"
        );
    }

    #[test]
    fn test_truncated_private_key_block_redacts_to_the_end() {
        let out = redacted("x -----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\nmore");
        assert!(!out.contains("MIIEvQ") && !out.contains("more"), "{out}");
        assert!(out.starts_with("x "));
    }

    #[test]
    fn test_openai_and_anthropic_keys_are_redacted() {
        assert_redacted("sk-abc123def456ghi789jkl012", "sk-abc123def456ghi789jkl012");
        assert_redacted(
            "sk-proj-1111111111222222222233333333334444444444",
            "1111111111",
        );
        assert_redacted("sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv", "AbCdEfGhIjKlMn");
    }

    #[test]
    fn test_github_tokens_are_redacted() {
        let t = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        assert_redacted(&format!("token {t}"), &t);
        assert_redacted(
            "github_pat_11AAAAAAA0abcdefghijkl_mnopqrstuv",
            "abcdefghijkl",
        );
    }

    #[test]
    fn test_other_vendor_tokens_are_redacted() {
        assert_redacted("xoxb-123456789012-abcdefghij", "123456789012");
        assert_redacted(
            "AIzaSyA-1234567890abcdefghijklmnopqrstuv",
            "1234567890abcdef",
        );
        assert_redacted(&format!("npm_{}", "a".repeat(36)), &"a".repeat(36));
        assert_redacted(&format!("hf_{}", "b".repeat(34)), &"b".repeat(34));
        assert_redacted("glpat-abcdefghij0123456789", "abcdefghij0123456789");
        assert_redacted(&format!("sk_live_{}", "c".repeat(24)), &"c".repeat(24));
    }

    #[test]
    fn test_jwt_is_redacted() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fw";
        assert_redacted(&format!("cookie={jwt}"), "SflKxwRJSMeKKF2QT4fw");
    }

    #[test]
    fn test_bearer_token_keeps_the_scheme() {
        let out = redacted("Authorization: Bearer abcdef0123456789ABCDEF.xyz");
        assert!(!out.contains("abcdef0123456789"), "{out}");
        assert!(out.contains("Bearer"), "{out}");
    }

    #[test]
    fn test_basic_authorization_header_is_redacted() {
        assert_redacted("Authorization: Basic dXNlcjpwYXNzd29yZA==", "dXNlcjpwYXNz");
    }

    #[test]
    fn test_connection_string_password_only() {
        let out = redacted("DATABASE_URL=postgres://admin:s3cr3tP4ss@db.internal:5432/app");
        assert!(!out.contains("s3cr3tP4ss"), "{out}");
        assert!(
            out.contains("admin:") && out.contains("@db.internal:5432/app"),
            "{out}"
        );
    }

    #[test]
    fn test_env_dump_values_are_redacted_names_kept() {
        let dump = "STRIPE_SECRET=hunter2hunter2\nDB_PASSWORD=\"p@ssw0rd!\"\nHOME=/home/sumit\n";
        let out = redacted(dump);
        assert!(
            !out.contains("hunter2hunter2") && !out.contains("p@ssw0rd"),
            "{out}"
        );
        assert!(
            out.contains("STRIPE_SECRET=") && out.contains("DB_PASSWORD="),
            "{out}"
        );
        assert!(
            out.contains("HOME=/home/sumit"),
            "unrelated line changed: {out}"
        );
    }

    #[test]
    fn test_credential_assignment_in_config_text() {
        assert_redacted("password: hunter2", "hunter2");
        assert_redacted("{\"api_key\": \"abcd1234efgh\"}", "abcd1234efgh");
        assert_redacted("client-secret = zzzz9999", "zzzz9999");
        // Prefixed keys, as found in a real TOML tool result.
        assert_redacted(
            "jwt_secret = \"super-secret-jwt-key-12345\"",
            "super-secret-jwt-key",
        );
        assert_redacted("db_password: abc12345", "abc12345");
    }

    // ---- negative cases: ordinary text must survive -------------------

    #[test]
    fn test_ordinary_text_is_untouched() {
        for s in [
            "The bearer of bad news arrived.",
            "Use task-based risk-adjusted planning for the sk-learn migration",
            "commit 4b825dc642cb6eb9a060e54bf8d69288fbee4904 fixed it",
            "session 550e8400-e29b-41d4-a716-446655440000 resumed",
            "max_tokens = 4096 and input_tokens: 123456",
            "http://localhost:8080/api and git@github.com:org/repo.git",
            "ssh://deploy@host.example.com/srv",
            "set secret: true to enable",
            "the token count was high",
            "password_hash = abcdefgh",
            "secret_count = 12345678 and token_limit: 99999999",
            "fn main() { println!(\"hello\"); }",
        ] {
            let (out, n) = r().text(s);
            assert_eq!(out, s, "false positive on {s:?}");
            assert_eq!(n, 0);
        }
    }

    // ---- structural properties ----------------------------------------

    #[test]
    fn test_redaction_is_idempotent() {
        let input = "k=sk-abc123def456ghi789jkl012 DB_PASSWORD=hunter2hunter2 \
                     postgres://u:pw1234@h/db Authorization: Bearer abcdef0123456789ABCDEF \
                     -----BEGIN PRIVATE KEY-----\nxx\n-----END PRIVATE KEY-----";
        let once = redacted(input);
        let twice = redacted(&once);
        assert_eq!(once, twice, "second pass changed the text");
        assert_eq!(r().text(&once).1, 0, "second pass still counted hits");
    }

    #[test]
    fn test_count_reports_each_secret() {
        let (_, n) = r().text("AKIAIOSFODNN7EXAMPLE and sk-abc123def456ghi789jkl012");
        assert_eq!(n, 2);
    }

    #[test]
    fn test_json_values_are_walked_and_sensitive_keys_replaced() {
        let mut v = json!({
            "command": "curl -H 'Authorization: Bearer abcdef0123456789ABCDEF' x",
            "env": {"DB_PASSWORD": "zzz", "PATH": "/usr/bin"},
            "access_token": "opaque-value-with-no-shape",
            "max_tokens": 4096,
            "args": ["--key", "sk-abc123def456ghi789jkl012"],
        });
        let n = r().value(&mut v);
        let s = v.to_string();
        assert!(n >= 4, "expected several hits, got {n}: {s}");
        for leaked in ["abcdef0123456789", "zzz", "opaque-value", "sk-abc123"] {
            assert!(!s.contains(leaked), "{leaked} leaked: {s}");
        }
        assert!(s.contains("/usr/bin") && s.contains("4096"), "{s}");
        assert_eq!(r().value(&mut v), 0, "second pass must be a no-op");
    }

    #[test]
    fn test_session_redaction_covers_every_written_field() {
        use crate::model::{Artifact, ArtifactKind, Message, Role, TokenTotals};
        let secret = "sk-abc123def456ghi789jkl012";
        let msg = |text: &str| Message {
            session_id: "s".into(),
            ordinal: 0,
            role: Role::User,
            timestamp: None,
            text: Some(text.into()),
            tool_name: Some("bash".into()),
            tool_input: Some(json!({ "command": format!("echo {secret}") })),
            tool_result: Some(json!(format!("out {secret}"))),
            parent_ordinal: None,
        };
        let mut s = Session {
            id: "s".into(),
            provider: "claude-code".into(),
            project_id: "/p".into(),
            started_at: None,
            last_event_at: None,
            model: None,
            title: Some(format!("title {secret}")),
            token_totals: TokenTotals::default(),
            source_path: "/x".into(),
            raw_payload: json!({ "k": secret }),
            body_available: true,
            messages: vec![msg(&format!("use {secret}"))],
            artifacts: vec![Artifact {
                session_id: "s".into(),
                kind: ArtifactKind::CommandRun,
                path_or_command: format!("export K={secret}"),
                detail: Some(secret.into()),
            }],
        };
        let n = r().session(&mut s);
        assert!(n >= 7, "expected every field to hit, got {n}");
        let dump = serde_json::to_string(&s).unwrap();
        assert!(!dump.contains(secret), "secret survived in {dump}");
    }

    #[test]
    fn test_disabled_redactor_is_a_pass_through() {
        let d = Redactor::disabled();
        let input = "sk-abc123def456ghi789jkl012";
        assert_eq!(d.text(input), (input.to_string(), 0));
        assert!(!d.is_enabled());
    }

    #[test]
    fn test_default_rules_all_compile() {
        // `default_redactor` panics on a bad built-in rule; reaching here
        // means every one parsed.
        assert_eq!(Redactor::defaults().rules.len(), DEFAULT_RULES.len());
    }

    #[test]
    fn test_redaction_speed_on_clean_text_is_linear() {
        // 20 MB of ordinary transcript-like text must go through in well under
        // a few seconds even in a debug build; a regression to per-rule full
        // scans or backtracking would blow far past this.
        let line = "The quick brown fox jumps over the lazy dog; commit 4b825dc6 fixed it.\n";
        let big = line.repeat(20 * 1024 * 1024 / line.len());
        let start = std::time::Instant::now();
        let (out, n) = r().text(&big);
        let took = start.elapsed();
        eprintln!("redacted {} MB of clean text in {took:?}", big.len() / (1024 * 1024));
        assert_eq!(n, 0);
        assert_eq!(out.len(), big.len());
        assert!(took.as_secs() < 20, "20 MB took {took:?}");
    }

    #[test]
    fn test_redaction_speed_when_every_line_has_a_secret() {
        // Worst case: every line triggers several rules, so the per-rule
        // replacement path runs on the whole input.
        let line = "export API_KEY=sk-abc123def456ghi789jkl012 and Bearer abcdef0123456789ABCDEF done\n";
        let big = line.repeat(5 * 1024 * 1024 / line.len());
        let start = std::time::Instant::now();
        let (out, n) = r().text(&big);
        let took = start.elapsed();
        eprintln!("redacted {} MB of dense secrets ({n} hits) in {took:?}", big.len() / (1024 * 1024));
        assert!(n > 1000);
        assert!(!out.contains("sk-abc123"));
        assert!(took.as_secs() < 60, "5 MB of dense secrets took {took:?}");
    }

    // ---- user rules and fail-closed behaviour -------------------------

    #[test]
    fn test_user_rules_extend_the_defaults() {
        let rules = parse_user_rules(
            "# comment\n\nacme-id = ACME-[0-9]{6}\nbadge = badge=(?P<secret>[0-9]+)\n",
            Path::new("redact.rules"),
        )
        .unwrap();
        let mut all = default_redactor().rules.clone();
        all.extend(rules);
        let red = Redactor::from_rules(all, true);
        let (out, n) = red.text("id ACME-123456 and badge=998877 and AKIAIOSFODNN7EXAMPLE");
        assert_eq!(n, 3, "{out}");
        assert!(out.contains("badge=[REDACTED:badge]"), "{out}");
        assert!(!out.contains("ACME-123456"));
    }

    #[test]
    fn test_invalid_user_rules_are_errors_not_skipped() {
        for (bad, why) in [
            ("no-equals-sign", "missing ="),
            ("Bad Name = x+", "bad name"),
            ("x = (unclosed", "bad regex"),
            ("x =", "empty pattern"),
        ] {
            let e = parse_user_rules(bad, Path::new("redact.rules"));
            assert!(e.is_err(), "{why}: {bad:?} must be rejected");
        }
    }

    #[test]
    fn test_bad_rules_file_makes_load_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = crate::sync::test_env_lock();
        unsafe { std::env::set_var("AGENTBRIDGE_DATA_DIR", tmp.path()) };
        std::fs::write(tmp.path().join(USER_RULES_FILE), "oops = (unclosed\n").unwrap();
        assert!(
            Redactor::load().is_err(),
            "an invalid rules file must stop the write, not fall back to defaults"
        );
        // A good file loads and applies.
        std::fs::write(tmp.path().join(USER_RULES_FILE), "acme = ACME-[0-9]{4}\n").unwrap();
        let red = Redactor::load().unwrap();
        assert_eq!(red.text("ACME-1234").1, 1);
        // No file at all is fine: defaults only.
        std::fs::remove_file(tmp.path().join(USER_RULES_FILE)).unwrap();
        assert!(Redactor::load().is_ok());
    }
}
