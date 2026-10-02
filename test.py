#!/usr/bin/env python3
"""Live cross tool check for agentbridge.

This drives the REAL tools on this machine, against their real session stores:

    1. create   start a new session in each tool and give it a codename
    2. sync     run `agentbridge sync` in the test folder
    3. names    every copy of a session carries one identical label and date
    4. access   open each session from every OTHER tool and ask for the code
                word, which only the first tool was ever told
    5. sync     pull the new turns back and republish them
    6. write    each question asked in step 4 now shows in the other tools
    7. repeat   one more sync changes nothing

It costs real model calls (one per session, one per question), so it is not
part of `cargo test`. Run it by hand:

    python3 test.py                 # every tool, every pair
    python3 test.py --quick         # each session is opened from one other tool
    python3 test.py --tools claude,codex
    python3 test.py --keep          # leave the test sessions for a look

When it finishes it removes what it made: the copies agentbridge wrote for
the test folder, the sessions it started in each tool, and the folder itself.
Pass `--keep` to leave everything in place and look at it in the tools.

OpenCode needs a model it can reach. If your default one is not available, set
one, for example `AB_TEST_OPENCODE_MODEL=openrouter/deepseek/deepseek-v4-flash`.

Needs Python 3.9 or newer and nothing outside the standard library.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import random
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

HOME = Path.home()

# Short name -> the provider id agentbridge uses.
PROVIDER = {
    "claude": "claude-code",
    "codex": "codex-cli",
    "opencode": "opencode",
    "agy": "antigravity",
}
BINARY = {"claude": "claude", "codex": "codex", "opencode": "opencode", "agy": "agy"}

# Optional model per tool, e.g. AB_TEST_CLAUDE_MODEL=haiku. Unset means the
# tool's own default, except Claude Code, where a cheap model is the default.
MODEL = {
    "claude": os.environ.get("AB_TEST_CLAUDE_MODEL", "haiku"),
    "codex": os.environ.get("AB_TEST_CODEX_MODEL", ""),
    "opencode": os.environ.get("AB_TEST_OPENCODE_MODEL", ""),
    "agy": os.environ.get("AB_TEST_AGY_MODEL", ""),
}

WORDS = ["KIWI", "MANGO", "PAPAYA", "GUAVA", "QUINCE", "LYCHEE", "KUMQUAT", "PERSIMMON"]
LABEL = re.compile(r"^(?P<provider>[a-z-]+) · (?P<name>.+) · (?P<stamp>\d{4}-\d\d-\d\d \d\d:\d\d) · (?P<id>\S{8})$")


# --------------------------------------------------------------------------
# where things live
# --------------------------------------------------------------------------

def data_dir() -> Path:
    return Path(os.environ.get("AGENTBRIDGE_DATA_DIR", HOME / ".agentbridge"))


def claude_projects() -> Path:
    return Path(os.environ.get("CLAUDE_CONFIG_DIR", HOME / ".claude")) / "projects"


def codex_home() -> Path:
    return Path(os.environ.get("CODEX_HOME", HOME / ".codex"))


def opencode_db() -> Path:
    base = Path(os.environ.get("XDG_DATA_HOME", HOME / ".local" / "share"))
    return base / "opencode" / "opencode.db"


def agy_home() -> Path:
    return HOME / ".gemini" / "antigravity-cli"


# --------------------------------------------------------------------------
# small helpers
# --------------------------------------------------------------------------

def run(cmd: list[str], cwd: Path, timeout: int) -> tuple[int, str]:
    """Run a command, return (exit code, stdout+stderr). Never hangs: the whole
    process group is stopped when the time is up."""
    # PWD has to follow the working folder. Tools that read it (OpenCode does)
    # otherwise think they are in the folder this script was started from, and
    # `opencode run -s` then answers but never exits.
    env = dict(os.environ, PWD=str(cwd))
    proc = subprocess.Popen(
        cmd,
        cwd=str(cwd),
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        errors="replace",
        start_new_session=True,
    )
    try:
        out, _ = proc.communicate(timeout=timeout)
        return proc.returncode, out
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            out, _ = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            out, _ = proc.communicate()
        return 124, (out or "") + f"\n[stopped after {timeout}s]"


def read_db(path: Path, sql: str, args: tuple = ()) -> list[tuple]:
    """Read another tool's SQLite database without ever writing to it."""
    if not path.is_file():
        return []
    for mode in ("mode=ro", "immutable=1"):
        try:
            conn = sqlite3.connect(f"file:{path}?{mode}", uri=True, timeout=5)
            try:
                return conn.execute(sql, args).fetchall()
            finally:
                conn.close()
        except sqlite3.Error:
            continue
    return []


def tail(text: str, lines: int = 3) -> str:
    kept = [l for l in text.strip().splitlines() if l.strip()]
    return " | ".join(kept[-lines:])[:300]


class Report:
    def __init__(self) -> None:
        self.rows: list[tuple[str, str, str, str]] = []

    def add(self, status: str, step: str, subject: str, detail: str = "") -> None:
        self.rows.append((status, step, subject, detail))
        print(f"  [{status:4}] {step:8} {subject:32} {detail}", flush=True)

    def count(self, status: str) -> int:
        return sum(1 for r in self.rows if r[0] == status)


# --------------------------------------------------------------------------
# the four tools
# --------------------------------------------------------------------------

class Tool:
    def __init__(self, name: str, folder: Path, timeout: int) -> None:
        self.name = name
        self.provider = PROVIDER[name]
        self.folder = folder
        self.timeout = timeout
        self.can_answer = True

    def available(self) -> bool:
        return shutil.which(BINARY[self.name]) is not None

    # -- ids of the sessions this tool has for the test folder -------------
    def known_ids(self) -> set[str]:
        if self.name == "claude":
            return {p.stem for p in claude_projects().glob("*/*.jsonl")}
        if self.name == "codex":
            ids = set()
            for p in (codex_home() / "sessions").rglob("*.jsonl"):
                m = re.search(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})", p.name)
                if m:
                    ids.add(m.group(1))
            return ids
        if self.name == "opencode":
            return {r[0] for r in read_db(opencode_db(), "select id from session")}
        return {p.stem for p in (agy_home() / "conversations").glob("*.db")}

    # -- command lines ------------------------------------------------------
    def _cmd(self, prompt: str, resume: str | None) -> list[str]:
        model = MODEL[self.name]
        if self.name == "claude":
            cmd = ["claude"]
            if resume:
                cmd += ["--resume", resume]
            cmd += ["-p", prompt, "--output-format", "json"]
            return cmd + (["--model", model] if model else [])
        if self.name == "codex":
            cmd = ["codex", "exec", "--skip-git-repo-check", "-s", "read-only"]
            cmd += ["-m", model] if model else []
            return cmd + (["resume", resume, prompt] if resume else [prompt])
        if self.name == "opencode":
            cmd = ["opencode", "run"]
            cmd += ["-m", model] if model else []
            cmd += ["-s", resume] if resume else []
            return cmd + [prompt]
        cmd = ["agy"]
        cmd += ["--conversation", resume] if resume else []
        cmd += ["-p", prompt]
        return cmd + (["--model", model] if model else [])

    def _answer(self, out: str) -> str:
        if self.name == "claude":
            for line in reversed(out.strip().splitlines()):
                try:
                    return str(json.loads(line).get("result", ""))
                except (ValueError, AttributeError):
                    continue
        return out

    def create(self, prompt: str) -> tuple[str | None, int, str]:
        """Start a new session. Returns (session id, exit code, output)."""
        before = self.known_ids()
        code, out = run(self._cmd(prompt, None), self.folder, self.timeout)
        sid = None
        if self.name == "claude":
            for line in reversed(out.strip().splitlines()):
                try:
                    sid = json.loads(line).get("session_id")
                    break
                except (ValueError, AttributeError):
                    continue
        elif self.name == "codex":
            m = re.search(r"session id: ([0-9a-f-]{36})", out)
            sid = m.group(1) if m else None
        if not sid:
            new = sorted(self.known_ids() - before)
            if self.name == "opencode" and new:
                # The newest session OpenCode made for this folder.
                rows = read_db(
                    opencode_db(),
                    "select id from session where directory = ? order by time_created desc",
                    (str(self.folder),),
                )
                new = [r[0] for r in rows if r[0] in new] or new
            sid = new[0] if new else None
        return sid, code, self._answer(out)

    def ask(self, session_id: str, prompt: str) -> tuple[int, str]:
        """Continue an existing session with one question."""
        code, out = run(self._cmd(prompt, session_id), self.folder, self.timeout)
        return code, self._answer(out)


# --------------------------------------------------------------------------
# agentbridge
# --------------------------------------------------------------------------

def sync(folder: Path, timeout: int) -> tuple[int, str]:
    """`agentbridge sync`, waiting out another run that holds the lock."""
    out = ""
    for _ in range(12):
        code, out = run(["agentbridge", "sync", "--project", str(folder)], folder, timeout)
        if "another agentbridge sync is running" not in out:
            return code, out
        time.sleep(10)
    return 1, out


# agentbridge refuses to touch Codex's index while Codex is open. That is the
# guard working, so it is a warning here, not a failure.
EXPECTED_NOTICE = "Codex is running"


def sync_step(report: "Report", folder: Path, timeout: int) -> tuple[bool, str]:
    """Run sync and report it. Any `!` line agentbridge prints is surfaced once;
    one that is not a known guard fails the step."""
    rc, out = sync(folder, timeout)
    notices = list(dict.fromkeys(l.strip() for l in out.splitlines() if l.strip().startswith("!")))
    counts = " ".join(
        l.strip() for l in out.splitlines() if re.match(r"\s*(pulled|created|unchanged|skipped)\b", l)
    )
    ok = rc == 0
    for notice in notices:
        if EXPECTED_NOTICE in notice:
            report.add("WARN", "sync", "agentbridge", notice[:150])
        else:
            ok = False
            report.add("FAIL", "sync", "agentbridge", notice[:200])
    report.add("PASS" if ok else "FAIL", "sync", "agentbridge", re.sub(r"\s+", " ", counts) or tail(out))
    return ok, out


def manifest_rows(folder: Path) -> list[dict]:
    path = data_dir() / "manifest.jsonl"
    if not path.is_file():
        return []
    rows = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("project") == str(folder):
            rows.append(row)
    return rows


def copy_of(folder: Path, origin_id: str, target: str) -> dict | None:
    """The copy of `origin_id` that agentbridge wrote into `target` for the
    test folder, straight from agentbridge's own manifest."""
    for row in manifest_rows(folder):
        if row["session_id"] == origin_id and row["target_provider"] == PROVIDER[target]:
            return row
    return None


def copy_id(row: dict) -> str:
    """The id the copy is addressed by inside its own tool."""
    target = row["target_provider"]
    if target in ("opencode", "antigravity"):
        return row["cache"]
    dest = Path(row["dest"])
    if target == "codex-cli":
        try:
            first = json.loads(dest.read_text(errors="replace").splitlines()[0])
            return first["payload"]["id"]
        except (OSError, ValueError, KeyError, IndexError):
            pass
    return dest.stem


def copy_text(row: dict) -> str:
    """Everything the copy holds, as text, to look for a marker in."""
    target = row["target_provider"]
    dest = Path(row["dest"])
    if target == "opencode":
        parts = read_db(dest, "select data from part where session_id = ?", (row["cache"],))
        return "\n".join(p[0] for p in parts)
    if target == "antigravity":
        blob = b""
        for path in (dest, Path(str(dest) + "-wal")):
            if path.is_file():
                blob += path.read_bytes()
        return blob.decode("latin-1")
    try:
        return dest.read_text(errors="replace")
    except OSError:
        return ""


def stored_title(row: dict) -> str | None:
    """The title the target tool itself now holds for the copy, where it can
    be read without driving the tool. None means it could not be read."""
    target = row["target_provider"]
    dest = Path(row["dest"])
    if target == "claude-code":
        title = None
        try:
            for line in dest.read_text(errors="replace").splitlines():
                try:
                    rec = json.loads(line)
                except ValueError:
                    continue
                if rec.get("type") == "custom-title":
                    title = rec.get("customTitle")
        except OSError:
            return None
        return title
    if target == "opencode":
        rows = read_db(dest, "select title from session where id = ?", (row["cache"],))
        return rows[0][0] if rows else None
    if target == "antigravity":
        rows = read_db(
            agy_home() / "conversation_summaries.db",
            "select title from conversation_summaries where conversation_id = ?",
            (row["cache"],),
        )
        return rows[0][0] if rows else None
    return None  # Codex keeps titles in an index that is not safe to read live


def cleanup(folder: Path, tools: dict, origins: dict, timeout: int) -> None:
    """Take away what this run made: agentbridge's copies for the test folder
    and of the test sessions, the sessions themselves, and the folder."""
    cmd = ["agentbridge", "unsync", "--project", str(folder)]
    for o in origins.values():
        cmd += ["--session", o["id"]]
    rc, out = run(cmd, folder, max(timeout, 300))
    print(f"  copies:   {tail(out, 2)}")

    for name, o in origins.items():
        sid, note = o["id"], "removed"
        try:
            if name == "claude":
                for p in claude_projects().glob(f"*/{sid}.jsonl"):
                    p.unlink()
            elif name == "codex":
                rc, out = run(["codex", "delete", sid, "--force"], folder, 60)
                for p in (codex_home() / "sessions").rglob(f"*{sid}*.jsonl"):
                    p.unlink()
            elif name == "opencode":
                rc, out = run(["opencode", "session", "delete", sid], folder, 60)
                if rc != 0:
                    note = f"`opencode session delete` failed: {tail(out, 1)}"
            else:  # agy has no delete command; the conversation is one file plus one index row
                for p in (agy_home() / "conversations").glob(f"{sid}.db*"):
                    p.unlink()
                index = agy_home() / "conversation_summaries.db"
                if index.is_file():
                    conn = sqlite3.connect(index, timeout=10)
                    conn.execute("delete from conversation_summaries where conversation_id = ?", (sid,))
                    conn.commit()
                    conn.close()
        except (OSError, sqlite3.Error) as e:
            note = f"could not remove: {e}"
        # What agentbridge recovered for this session is test data too.
        for p in (data_dir() / "overlay").glob(f"{sid}*"):
            p.unlink(missing_ok=True)
        print(f"  session:  {name:9} {sid}  {note}")

    # Claude Code keeps one folder per project; this one only ever held the test.
    encoded = re.sub(r"[^A-Za-z0-9]", "-", str(folder))
    shutil.rmtree(claude_projects() / encoded, ignore_errors=True)
    shutil.rmtree(folder, ignore_errors=True)
    print(f"  folder:   {folder} removed")


# --------------------------------------------------------------------------
# the test
# --------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description="Live cross tool check for agentbridge (real tools, real model calls).")
    ap.add_argument("--tools", default="claude,codex,opencode,agy", help="comma separated: claude,codex,opencode,agy")
    ap.add_argument("--quick", action="store_true", help="open each session from one other tool, not all of them")
    ap.add_argument("--timeout", type=int, default=240, help="seconds allowed per tool call (default 240)")
    ap.add_argument("--folder", help="test folder to use (default: a new one under ~/agentbridge-live-test)")
    ap.add_argument("--keep", action="store_true", help="leave the test sessions and copies in place")
    args = ap.parse_args()

    names = [t.strip() for t in args.tools.split(",") if t.strip()]
    unknown = [t for t in names if t not in PROVIDER]
    if unknown:
        ap.error(f"unknown tool(s): {', '.join(unknown)}")
    if shutil.which("agentbridge") is None:
        print("agentbridge is not on PATH. Run `cargo install --path .` first.")
        return 2

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%d-%H%M%S")
    folder = Path(args.folder) if args.folder else HOME / "agentbridge-live-test" / stamp
    folder.mkdir(parents=True, exist_ok=True)
    folder = folder.resolve()
    if not (folder / "README.md").exists():
        (folder / "README.md").write_text("Throwaway folder for the agentbridge live test.\n")
    if shutil.which("git") and not (folder / ".git").exists():
        # A real repository with one commit, so every tool treats the folder as
        # a project of its own rather than a catch-all one.
        run(["git", "init", "-q", "."], folder, 30)
        run(["git", "add", "README.md"], folder, 30)
        run(["git", "-c", "user.name=agentbridge-test", "-c", "user.email=test@example.invalid",
             "commit", "-q", "-m", "test folder"], folder, 30)

    nonce = "".join(random.choices("0123456789", k=4))
    report = Report()
    code, version = run(["agentbridge", "--version"], folder, 30)
    print(f"{version.strip()}  |  folder: {folder}\n")

    tools = {n: Tool(n, folder, args.timeout) for n in names}

    # 1. create -----------------------------------------------------------
    print("1. create a session in each tool")
    origins: dict[str, dict] = {}
    for name, tool in tools.items():
        if not tool.available():
            report.add("SKIP", "create", name, f"`{BINARY[name]}` is not installed")
            continue
        word = f"{random.choice(WORDS)}-{nonce}-{name.upper()}"
        prompt = (
            "Do not use any tools or run any commands. "
            f"My project codename is {word}. Remember it for later. Reply with exactly: noted."
        )
        started = dt.datetime.now(dt.timezone.utc)
        sid, rc, answer = tool.create(prompt)
        if not sid:
            tool.can_answer = False
            report.add("FAIL", "create", name, f"no new session appeared. {tail(answer)}")
            continue
        origins[name] = {"id": sid, "word": word, "started": started}
        if rc != 0:
            tool.can_answer = False
            report.add("WARN", "create", name, f"{sid}: the question was saved but {name} did not answer")
        else:
            report.add("PASS", "create", name, f"{sid}  codename {word}")

    if len(origins) < 2:
        print("\nFewer than two tools produced a session, so there is nothing to cross check.")
        return 1

    # 2. sync -------------------------------------------------------------
    print("\n2. agentbridge sync")
    ok, out = sync_step(report, folder, max(args.timeout, 600))
    if not manifest_rows(folder):
        print("\nThe first sync wrote nothing for this folder, so there is nothing to check.")
        return 1

    # 3. names and dates --------------------------------------------------
    print("\n3. one name and one date per session, in every tool")
    for name, o in origins.items():
        rows = [r for r in manifest_rows(folder) if r["session_id"] == o["id"]]
        targets = sorted({r["target_provider"] for r in rows})
        titles = {r.get("title") for r in rows}
        if not rows:
            report.add("FAIL", "names", name, "the session was not copied into any other tool")
            continue
        label = LABEL.match(next(iter(titles)) or "")
        if len(titles) != 1 or not label:
            report.add("FAIL", "names", name, f"copies disagree or are not labeled: {sorted(map(str, titles))}")
            continue
        when = dt.datetime.strptime(label["stamp"], "%Y-%m-%d %H:%M").replace(tzinfo=dt.timezone.utc)
        drift = abs((when - o["started"]).total_seconds())
        problems = []
        if label["provider"] != PROVIDER[name]:
            problems.append(f"origin shown as {label['provider']}")
        if drift > 300:
            problems.append(f"date {label['stamp']} UTC is {int(drift)}s from when the session started")
        if label["id"] not in o["id"]:
            problems.append(f"id {label['id']} is not part of {o['id']}")
        if problems:
            report.add("FAIL", "names", name, "; ".join(problems))
            continue
        report.add("PASS", "names", name, f"{next(iter(titles))}   in: {', '.join(targets)}")
        for row in rows:
            held = stored_title(row)
            if held is not None and held != row["title"]:
                status = "WARN" if row["target_provider"] == "antigravity" and not held else "FAIL"
                report.add(status, "names", f"{name} in {row['target_provider']}",
                           f"the tool holds {held!r}" + (" (agy clears titles it did not write)" if status == "WARN" else ""))

    # 4. access -----------------------------------------------------------
    print("\n4. open each session from the other tools and ask for its codename")
    asked: list[dict] = []
    order = list(tools)
    for name, o in origins.items():
        others = [t for t in order if t != name]
        if args.quick:
            start = (order.index(name) + 1) % len(order)
            rotated = order[start:] + order[:start]
            others = [t for t in rotated if t != name and t in tools and tools[t].available() and tools[t].can_answer][:1]
        for target in others:
            subject = f"{name} -> {target}"
            tool = tools[target]
            if not tool.available():
                report.add("SKIP", "access", subject, f"`{BINARY[target]}` is not installed")
                continue
            if not tool.can_answer:
                report.add("SKIP", "access", subject, f"{target} cannot answer on this machine")
                continue
            row = copy_of(folder, o["id"], target)
            if not row:
                report.add("FAIL", "access", subject, f"no copy of the session exists in {target}")
                continue
            marker = f"WB-{nonce}-{name}-via-{target}"
            prompt = (
                "Do not use any tools or run any commands. What is my project codename, which I "
                f"told you earlier in this conversation? Reply with only that. (reference {marker})"
            )
            rc, answer = tool.ask(copy_id(row), prompt)
            if o["word"] not in answer:
                # A model now and then declines or rambles. The history is what
                # is under test, so it gets one more chance to read it.
                rc, answer = tool.ask(copy_id(row), prompt)
            if o["word"] in answer:
                report.add("PASS", "access", subject, f"answered {o['word']}")
                asked.append({"origin": name, "via": target, "marker": marker})
            else:
                report.add("FAIL", "access", subject, f"expected {o['word']}, got: {tail(answer, 2)}")
                if marker in copy_text(row):
                    asked.append({"origin": name, "via": target, "marker": marker})

    # 5. sync again -------------------------------------------------------
    print("\n5. agentbridge sync (pull the new turns back, republish)")
    sync_step(report, folder, max(args.timeout, 600))

    # 6. write back -------------------------------------------------------
    print("\n6. each question asked in step 4 now shows in the other tools")
    for q in asked:
        o = origins[q["origin"]]
        overlay = data_dir() / "overlay" / f"{o['id']}.jsonl"
        pulled = overlay.is_file() and q["marker"] in overlay.read_text(errors="replace")
        subject = f"{q['origin']} via {q['via']}"
        if not pulled:
            report.add("FAIL", "write", subject, "the turn was not pulled back into agentbridge's overlay")
            continue
        seen, missing = [], []
        for target in tools:
            if target in (q["origin"], q["via"]):
                continue  # the original is never changed; the tool that asked already has it
            row = copy_of(folder, o["id"], target)
            if not row:
                continue
            (seen if q["marker"] in copy_text(row) else missing).append(target)
        if missing:
            report.add("FAIL", "write", subject, f"missing in: {', '.join(missing)}  (present in: {', '.join(seen) or 'none'})")
        else:
            report.add("PASS", "write", subject, f"pulled back, and present in: {', '.join(seen) or 'no other tool had a copy'}")

    # 7. repeat -----------------------------------------------------------
    print("\n7. one more sync changes nothing")
    ok, out = sync_step(report, folder, max(args.timeout, 600))
    created = re.search(r"created\s+(\d+)", out)
    pulled = re.search(r"pulled\s+(\d+)", out)
    if ok and created and created.group(1) == "0" and not pulled:
        report.add("PASS", "repeat", "agentbridge", "nothing new was created or pulled")
    else:
        report.add("FAIL", "repeat", "agentbridge", "a repeat sync still created, pulled, or reported something")

    # summary -------------------------------------------------------------
    print("\n" + "=" * 72)
    print(f"PASS {report.count('PASS')}   FAIL {report.count('FAIL')}   WARN {report.count('WARN')}   SKIP {report.count('SKIP')}")
    for status, step, subject, detail in report.rows:
        if status in ("FAIL", "WARN", "SKIP"):
            print(f"  {status:4} {step:8} {subject}: {detail}")
    failed = report.count("FAIL")
    if args.keep:
        print(f"\nTest sessions were left in place under: {folder}")
        ids = " ".join(f"--session {o['id']}" for o in origins.values())
        print(f"To remove the copies later: agentbridge unsync --project {folder} {ids}")
    else:
        print("\n8. clean up")
        cleanup(folder, tools, origins, args.timeout)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
