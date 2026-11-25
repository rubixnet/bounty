#!/usr/bin/env python3
"""Capture and check Claude Code fixtures for the Traction daemon tests.

Capture (local only, never in CI; needs a logged-in `claude` on PATH):

    python3 scripts/claude_fixtures.py

Check the committed fixtures (no `claude` needed; safe in CI):

    python3 scripts/claude_fixtures.py --check

Capture drives the real `claude -p` CLI with the synthetic prompts below on a
fresh throwaway Git repo, with a fresh temp HOME and CLAUDE_CONFIG_DIR that hold
only the capture hooks and the Traction command files, so the developer's own
settings and transcripts are never read. A loopback listener records every hook
payload unchanged; the session transcripts are then copied, scrubbed of
machine-specific values and written to the fixture folder with a manifest.

Authentication: a temp CLAUDE_CONFIG_DIR has no login. Pass a token through
CLAUDE_CODE_OAUTH_TOKEN (from `claude setup-token`) or ANTHROPIC_API_KEY. These
are the only variables passed through from your environment.

Standard library only, Python 3.9+.
"""

from __future__ import annotations

import argparse
import datetime
import getpass
import http.server
import json
import os
import platform
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_OUT = REPO_ROOT / "crates" / "traction-daemon" / "tests" / "data" / "claude-code"

CANARY = "TRACTION-CANARY"
FIXTURE_GIT_NAME = "Traction Fixture"
FIXTURE_GIT_EMAIL = "fixture@example.invalid"

# The spec's hook set (never WorktreeCreate).
HOOKS = [
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "SubagentStop",
    "CwdChanged",
    "DirectoryAdded",
    "Stop",
    "StopFailure",
    "SessionEnd",
]
TOOL_HOOKS = {"PermissionRequest", "PreToolUse", "PostToolUse", "PostToolUseFailure"}

# Traction command files (spec "Installer": frontmatter with a description and
# `disable-model-invocation: true`, a marker comment, and a one-line body that
# names the Traction action with no instructions to the model). Ticket 06 owns
# the shipped text; the capture only needs files of the same shape.
COMMAND_MARKER = "<!-- traction:managed -->"
STOP_TRACK_BODY = "Traction: pause tracking for this session."
RESUME_TRACK_BODY = "Traction: resume tracking for this session."
BLOCK_REASON_PAUSED = "Traction tracking paused for this session."
BLOCK_REASON_RESUMED = "Traction tracking resumed for this session."


def command_file(description: str, body: str) -> str:
    return (
        "---\n"
        f"description: {description}\n"
        "disable-model-invocation: true\n"
        "---\n"
        f"{COMMAND_MARKER}\n"
        f"{body}\n"
    )


# ---------------------------------------------------------------- scenarios


class Scenario:
    def __init__(self, name, prompts, flags, simulated=False, note="", needs_canary=True):
        self.name = name
        self.prompts = prompts  # list of prompts, each a separate `claude -p` turn (resumed)
        self.flags = flags
        self.simulated = simulated
        self.note = note
        self.needs_canary = needs_canary


def c(name):
    return f"{CANARY}-{name}"


SCENARIOS = [
    Scenario(
        "normal-multi-tool",
        [
            f"Create a file named notes.txt containing exactly the line '{c('normal-multi-tool')}'. "
            "Then read it back with the Read tool, then run `ls` with Bash. Reply with one short sentence."
        ],
        ["--permission-mode", "acceptEdits", "--allowedTools", "Read,Write,Edit,Bash(ls:*),Bash(ls)"],
    ),
    Scenario(
        "tool-failure",
        [
            f"Use the Read tool to read the file missing-{c('tool-failure')}.txt (it does not exist). "
            "Then run the Bash command `false`. Report both failures in one sentence and do nothing else."
        ],
        ["--permission-mode", "acceptEdits", "--allowedTools", "Read,Bash(false)"],
    ),
    Scenario(
        "permission-request",
        [
            f"Run the Bash command `echo {c('permission-request')} > perm.txt`. "
            "If you are not allowed, say so in one sentence and stop."
        ],
        ["--permission-mode", "default"],
        note="Bash is not pre-allowed, so Claude Code must ask for permission; headless -p mode may deny instead of asking.",
    ),
    Scenario(
        "subagent",
        [
            "Use the Task tool to start one general-purpose subagent with this exact task: "
            f"'Read the file README.md and reply with the phrase {c('subagent')}.' "
            "Then tell me in one sentence what the subagent replied."
        ],
        ["--permission-mode", "acceptEdits", "--allowedTools", "Task,Agent,Read"],
    ),
    Scenario(
        "cwd-change",
        [
            f"Run these Bash commands one at a time: `cd sub`, then `pwd`, then `echo {c('cwd-change')}`. "
            "Reply with one short sentence."
        ],
        ["--permission-mode", "acceptEdits", "--allowedTools", "Bash(cd:*),Bash(cd),Bash(pwd),Bash(echo:*)"],
    ),
    Scenario(
        "interrupted-turn",
        [f"Run the Bash command `sleep 60 && echo {c('interrupted-turn')}` and wait for it to finish."],
        ["--permission-mode", "acceptEdits", "--allowedTools", "Bash(sleep:*)"],
        note="The capture sends SIGINT (CTRL_BREAK on Windows) to claude once the Bash PreToolUse hook arrives.",
    ),
    Scenario(
        "api-error-stop",
        [f"Reply with the phrase {c('api-error-stop')}."],
        [],
        simulated=True,
        note="ANTHROPIC_BASE_URL points at a local stub that answers every API call with HTTP 400 "
        "invalid_request_error, so the error is simulated while Claude Code's reaction to it is real.",
    ),
    Scenario(
        "stop-track",
        ["/stop-track", "/resume-track"],
        [],
        note="The listener answers UserPromptSubmit with the spec's block decision when the prompt is "
        "the literal command or the command file body. Each prompt is a separate `claude -p` call.",
        needs_canary=False,
    ),
]
SCENARIO_NAMES = [s.name for s in SCENARIOS]
CANARY_EXEMPT = {s.name for s in SCENARIOS if not s.needs_canary}

# ---------------------------------------------------------------- check


MANIFEST_KEYS = ["scenario", "claude_code_version", "captured_at", "os", "simulated"]

HOME_PATH_RE = re.compile(
    r"(/Users/(?!<)[^/\"\\\s]+|/home/(?!<)[^/\"\\\s]+|[A-Za-z]:(\\\\|\\|/)Users(\\\\|\\|/)(?!<)[^\\/\"\s]+|"
    r"-Users-(?!<)[A-Za-z0-9_.]+|-home-(?!<)[A-Za-z0-9_.]+)"
)
TEMP_PATH_RE = re.compile(
    r"(/private/var/folders/|/var/folders/|/private/tmp/|(?<![\w<])/tmp/|AppData(\\\\|\\|/)Local(\\\\|\\|/)Temp"
    r"|-private-var-folders-|-var-folders-)"
)
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@(?!example\.invalid\b)[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
ACCOUNT_KEYS_RE = re.compile(
    r"\"(accountUuid|account_uuid|organizationUuid|organization_uuid|emailAddress|oauthAccount|userID|user_id"
    r"|apiKey|api_key|accessToken|access_token|refreshToken|refresh_token)\"\s*:\s*\"(?!<)[^\"]+\""
)
# Claude Code's own per-user scratch folder for background-task output:
# `/private/tmp/claude-<uid>/...` on macOS, `/tmp/claude-<uid>/...` on Linux.
# The uid is machine-specific; the rest of the path (project slug, session ID,
# task ID) is already scrubbed or deliberately kept.
CLAUDE_SCRATCH_RE = re.compile(r"(?:/private)?/tmp/claude-\d+(?=[/\\])")
SECRET_VALUE_RE = re.compile(r"(sk-ant-[A-Za-z0-9_-]{8,}|Bearer\s+(?!<|\$)[A-Za-z0-9._-]{16,})")


def local_identity_res():
    """Patterns for this machine's own values (only meaningful where the check runs)."""
    out = []
    home = str(Path.home())
    if len(home) > 3:
        out.append(("home path", home))
    host = socket.gethostname().split(".")[0]
    if len(host) >= 4:
        out.append(("hostname", host))
    return out


def scan_text(text: str, where: str, errors: list):
    for label, rx in [
        ("home path", HOME_PATH_RE),
        ("temp path", TEMP_PATH_RE),
        ("email", EMAIL_RE),
        ("account identifier", ACCOUNT_KEYS_RE),
        ("secret", SECRET_VALUE_RE),
    ]:
        m = rx.search(text)
        if m:
            errors.append(f"{where}: leftover {label}: {m.group(0)[:60]!r}")
    for label, value in local_identity_res():
        if value in text:
            errors.append(f"{where}: leftover {label} of this machine")


def check(out: Path) -> int:
    errors = []
    if not out.is_dir():
        print(f"check: fixture folder {out} is missing", file=sys.stderr)
        return 1
    for name in SCENARIO_NAMES:
        d = out / name
        if not d.is_dir():
            errors.append(f"{name}: scenario folder missing")
            continue
        texts = []
        mpath = d / "manifest.json"
        try:
            manifest = json.loads(mpath.read_text(encoding="utf-8"))
            for k in MANIFEST_KEYS:
                if k not in manifest:
                    errors.append(f"{name}/manifest.json: missing key {k}")
            if manifest.get("scenario") not in (None, name):
                errors.append(f"{name}/manifest.json: scenario is {manifest.get('scenario')!r}")
        except (OSError, ValueError) as e:
            errors.append(f"{name}/manifest.json: unreadable ({e})")
        hooks = sorted((d / "hooks").glob("*.json")) if (d / "hooks").is_dir() else []
        if not hooks:
            errors.append(f"{name}: no hook payloads in hooks/")
        for h in hooks:
            t = h.read_text(encoding="utf-8")
            texts.append((f"{name}/hooks/{h.name}", t))
            try:
                json.loads(t)
            except ValueError as e:
                errors.append(f"{name}/hooks/{h.name}: not valid JSON ({e})")
        transcripts = [p for p in d.rglob("*.jsonl")]
        if not (d / "transcript.jsonl").is_file():
            errors.append(f"{name}: transcript.jsonl missing")
        for p in transcripts:
            rel = p.relative_to(d).as_posix()
            t = p.read_text(encoding="utf-8")
            texts.append((f"{name}/{rel}", t))
            for i, line in enumerate(t.splitlines(), 1):
                if not line.strip():
                    continue
                try:
                    json.loads(line)
                except ValueError:
                    errors.append(f"{name}/{rel}: line {i} is not valid JSON")
        for where, t in texts + [(f"{name}/manifest.json", mpath.read_text(encoding="utf-8") if mpath.is_file() else "")]:
            scan_text(t, where, errors)
        if name not in CANARY_EXEMPT and not any(CANARY in t for _, t in texts):
            errors.append(f"{name}: no {CANARY} canary string in hooks or transcripts")
    if errors:
        for e in errors:
            print("check: " + e, file=sys.stderr)
        print(f"check: FAILED ({len(errors)} problems)", file=sys.stderr)
        return 1
    print(f"check: ok ({len(SCENARIO_NAMES)} scenarios in {out})")
    return 0


# ---------------------------------------------------------------- process control


def popen_group(argv, **kw):
    if os.name == "nt":
        kw["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kw["start_new_session"] = True
    return subprocess.Popen(argv, **kw)


def kill_tree(proc):
    if proc.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(["taskkill", "/T", "/F", "/PID", str(proc.pid)], capture_output=True)
    else:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass


def interrupt(proc):
    if proc.poll() is not None:
        return
    if os.name == "nt":
        proc.send_signal(signal.CTRL_BREAK_EVENT)
    else:
        try:
            os.killpg(proc.pid, signal.SIGINT)
        except ProcessLookupError:
            pass


def run_bounded(argv, timeout, env=None, cwd=None, on_tick=None):
    """Run argv with a hard wall-clock timeout that kills the whole tree.

    Returns (returncode or None on timeout, stdout, stderr)."""
    proc = popen_group(
        argv,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=cwd,
    )
    result = {}

    def pump():
        out, err = proc.communicate()
        result["out"], result["err"] = out, err

    t = threading.Thread(target=pump, daemon=True)
    t.start()
    deadline = time.monotonic() + timeout
    while t.is_alive() and time.monotonic() < deadline:
        t.join(0.2)
        if on_tick:
            on_tick(proc)
    if t.is_alive():
        kill_tree(proc)
        t.join(5)
        return None, "", ""
    # claude may leave children behind; reap the group.
    kill_tree(proc)
    dec = lambda b: (b or b"").decode("utf-8", "replace")  # noqa: E731
    return proc.returncode, dec(result.get("out")), dec(result.get("err"))


# ---------------------------------------------------------------- environment


def child_env(home: Path, config_dir: Path, extra=None):
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": str(home),
        "USERPROFILE": str(home),
        "CLAUDE_CONFIG_DIR": str(config_dir),
        "LANG": "C.UTF-8",
        "GIT_AUTHOR_NAME": FIXTURE_GIT_NAME,
        "GIT_AUTHOR_EMAIL": FIXTURE_GIT_EMAIL,
        "GIT_COMMITTER_NAME": FIXTURE_GIT_NAME,
        "GIT_COMMITTER_EMAIL": FIXTURE_GIT_EMAIL,
        "GIT_CONFIG_NOSYSTEM": "1",
        "DISABLE_AUTOUPDATER": "1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    }
    for k in ("SYSTEMROOT", "SystemRoot", "COMSPEC", "PATHEXT", "TMPDIR", "TEMP", "TMP", "USER", "LOGNAME"):
        if k in os.environ:
            env[k] = os.environ[k]
    for k in ("CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"):
        if os.environ.get(k):
            env[k] = os.environ[k]
    env.update(extra or {})
    return env


def resolve_program(prog: str):
    p = shutil.which(prog)
    if p:
        return p
    return prog if Path(prog).is_file() and os.access(prog, os.X_OK) else None


class PreflightError(Exception):
    def __init__(self, reason, detail):
        super().__init__(f"{reason}: {detail}")
        self.reason = reason
        self.detail = detail


VERSION_RE = re.compile(r"^\s*(\d+\.\d+\.\d+)")


def classify_failure(text: str) -> str:
    low = text.lower()
    if "not logged in" in low or "/login" in low or "invalid api key" in low or "authentication" in low or "401" in low:
        return "not-logged-in"
    if "connection" in low or "network" in low or "enotfound" in low or "econnrefused" in low or "timed out" in low:
        return "no-network"
    return "not-logged-in"


def preflight(prog_arg: str, timeout: float):
    prog = resolve_program(prog_arg)
    if not prog:
        raise PreflightError("claude-missing", f"{prog_arg!r} is not on PATH")
    rc, out, _ = run_bounded([prog, "--version"], timeout)
    if rc is None:
        raise PreflightError("timeout", "`claude --version` did not finish")
    m = VERSION_RE.match(out)
    if rc != 0 or not m:
        raise PreflightError("claude-missing", f"`claude --version` printed {out.strip()[:80]!r}")
    version = m.group(1)
    with tempfile.TemporaryDirectory(prefix="traction-preflight-") as tmp:
        home = Path(tmp) / "home"
        cfg = Path(tmp) / "claude-config"
        home.mkdir()
        cfg.mkdir()
        rc, out, err = run_bounded(
            [prog, "-p", "Reply with the single word OK.", "--output-format", "json", "--max-turns", "1"],
            timeout,
            env=child_env(home, cfg),
            cwd=str(home),
        )
    if rc is None:
        raise PreflightError("timeout", f"`claude -p` did not answer within {timeout:g}s")
    try:
        res = json.loads(out.strip().splitlines()[-1]) if out.strip() else {}
    except ValueError:
        res = {}
    if rc != 0 or res.get("is_error") or not res:
        detail = str(res.get("result") or err.strip() or out.strip())[:200]
        raise PreflightError(classify_failure(detail), detail)
    return prog, version


# ---------------------------------------------------------------- hook listener


class HookListener:
    """Loopback HTTP server: records every hook payload unchanged, answers 200 {}."""

    def __init__(self, respond=None):
        self.payloads = []  # (hook_event_name, raw bytes)
        self.lock = threading.Lock()
        self.respond = respond or (lambda body: {})
        listener = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):  # noqa: N802
                n = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(n)
                try:
                    body = json.loads(raw)
                except ValueError:
                    body = {}
                with listener.lock:
                    listener.payloads.append((str(body.get("hook_event_name", "unknown")), raw))
                answer = json.dumps(listener.respond(body)).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(answer)))
                self.end_headers()
                self.wfile.write(answer)

            def log_message(self, *a):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *a):
        self.server.shutdown()
        self.server.server_close()

    def seen(self, hook, tool=None):
        with self.lock:
            for name, raw in self.payloads:
                if name == hook and (tool is None or tool.encode() in raw):
                    return True
        return False


class ApiErrorStub:
    """Answers every Anthropic API call with HTTP 400 invalid_request_error."""

    def __init__(self):
        class Handler(http.server.BaseHTTPRequestHandler):
            def _answer(self):
                n = int(self.headers.get("Content-Length") or 0)
                self.rfile.read(n)  # discard (may carry a token)
                body = json.dumps(
                    {
                        "type": "error",
                        "error": {"type": "invalid_request_error", "message": "Traction fixture: simulated API error"},
                    }
                ).encode()
                self.send_response(400)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_POST = _answer  # noqa: N815
            do_GET = _answer  # noqa: N815

            def log_message(self, *a):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *a):
        self.server.shutdown()
        self.server.server_close()


def stop_track_responder(body):
    if body.get("hook_event_name") != "UserPromptSubmit":
        return {}
    prompt = str(body.get("prompt", "")).strip()
    if prompt in ("/stop-track", STOP_TRACK_BODY):
        return {"decision": "block", "reason": BLOCK_REASON_PAUSED}
    if prompt in ("/resume-track", RESUME_TRACK_BODY):
        return {"decision": "block", "reason": BLOCK_REASON_RESUMED}
    return {}


# ---------------------------------------------------------------- capture


def write_capture_config(cfg: Path, port: int):
    hooks = {}
    for h in HOOKS:
        entry = {"type": "http", "url": f"http://127.0.0.1:{port}/hook", "timeout": 5}
        group = {"hooks": [entry]}
        if h in TOOL_HOOKS:
            group["matcher"] = "*"
        hooks[h] = [group]
    (cfg / "settings.json").write_text(json.dumps({"hooks": hooks}, indent=2))
    cmds = cfg / "commands"
    cmds.mkdir(parents=True, exist_ok=True)
    (cmds / "stop-track.md").write_text(command_file("Pause Traction tracking for this session", STOP_TRACK_BODY))
    (cmds / "resume-track.md").write_text(command_file("Resume Traction tracking for this session", RESUME_TRACK_BODY))


def make_repo(repo: Path, env):
    repo.mkdir(parents=True)
    (repo / "sub").mkdir()
    (repo / "README.md").write_text(f"# Fixture repo\n\nSynthetic throwaway repo. {CANARY}-readme\n")
    (repo / "sub" / "keep.txt").write_text("sub folder\n")
    for argv in (["git", "init", "-q"], ["git", "add", "-A"], ["git", "commit", "-q", "-m", "fixture"]):
        subprocess.run(argv, cwd=repo, env=env, check=True, capture_output=True, timeout=30)


def scrub_pairs(values):
    """Build (old, new) replacements, longest first, incl. JSON-escaped and slug forms."""
    pairs = []
    for old, new in values:
        if not old or len(old) < 3:
            continue
        forms = {old}
        try:
            forms.add(str(Path(old).resolve()))
        except OSError:
            pass
        for f in list(forms):
            if f.startswith("/private/"):
                forms.add(f[len("/private") :])
        for f in list(forms):
            pairs.append((f, new))
            pairs.append((f.replace("\\", "\\\\"), new))  # JSON-escaped Windows path
            if "/" in f or "\\" in f:
                # Claude's project-folder slug: every non-alphanumeric char becomes '-'.
                pairs.append((re.sub(r"[^A-Za-z0-9]", "-", f), "-" + new))
    seen, out = set(), []
    for old, new in sorted(pairs, key=lambda p: -len(p[0])):
        if old not in seen:
            seen.add(old)
            out.append((old, new))
    return out


def scrub(text: str, pairs) -> str:
    for old, new in pairs:
        text = text.replace(old, new)
    text = CLAUDE_SCRATCH_RE.sub("<CLAUDE_TMP>", text)
    text = EMAIL_RE.sub("fixture@example.invalid", text)
    text = ACCOUNT_KEYS_RE.sub(lambda m: f'"{m.group(1)}": "<REDACTED>"', text)
    text = SECRET_VALUE_RE.sub("<REDACTED>", text)
    return text


def capture_scenario(sc: Scenario, prog: str, version: str, out: Path, timeout: float, model: str):
    with tempfile.TemporaryDirectory(prefix="traction-capture-") as tmp:
        tmp = Path(tmp)
        home, cfg, repo = tmp / "home", tmp / "claude-config", tmp / "repo"
        home.mkdir()
        cfg.mkdir()
        responder = stop_track_responder if sc.name == "stop-track" else None
        with HookListener(responder) as listener, ApiErrorStub() as stub:
            write_capture_config(cfg, listener.port)
            extra = {}
            if sc.name == "api-error-stop":
                extra["ANTHROPIC_BASE_URL"] = f"http://127.0.0.1:{stub.port}"
            env = child_env(home, cfg, extra)
            make_repo(repo, env)
            results = []
            session_id = None
            for i, prompt in enumerate(sc.prompts):
                argv = [prog, "-p", prompt, "--output-format", "json", "--max-turns", "10", "--strict-mcp-config"]
                if model:
                    argv += ["--model", model]
                argv += sc.flags
                if session_id and i > 0:
                    argv += ["--resume", session_id]
                tick = None
                if sc.name == "interrupted-turn":
                    state = {"sent": False}

                    def tick(proc, state=state):
                        if not state["sent"] and listener.seen("PreToolUse", "Bash"):
                            time.sleep(2)
                            interrupt(proc)
                            state["sent"] = True

                rc, sout, serr = run_bounded(argv, timeout, env=env, cwd=str(repo), on_tick=tick)
                if rc is None:
                    raise RuntimeError(f"timed out after {timeout:g}s")
                try:
                    res = json.loads(sout.strip().splitlines()[-1]) if sout.strip() else {}
                except ValueError:
                    res = {}
                session_id = res.get("session_id") or session_id
                results.append(
                    {
                        "exit_code": rc,
                        "is_error": res.get("is_error"),
                        "terminal_reason": res.get("terminal_reason"),
                        "api_calls_ms": res.get("duration_api_ms"),
                        "num_turns": res.get("num_turns"),
                    }
                )
            time.sleep(1)  # let SessionEnd land
            payloads = list(listener.payloads)

        if not payloads:
            raise RuntimeError("no hook payloads were received")
        pairs = scrub_pairs(
            [
                (str(repo), "<REPO>"),
                (str(cfg), "<CLAUDE_CONFIG_DIR>"),
                (str(home), "<HOME>"),
                (str(tmp), "<TMP>"),
                (tempfile.gettempdir(), "<TMP>"),
                (str(Path.home()), "<HOME>"),
                (socket.gethostname(), "<HOST>"),
                (socket.gethostname().split(".")[0], "<HOST>"),
            ]
        )
        user = getpass.getuser()
        stage = out.parent / f".{sc.name}.staging"
        if stage.exists():
            shutil.rmtree(stage)
        (stage / "hooks").mkdir(parents=True)

        def clean(t: str) -> str:
            t = scrub(t, pairs)
            if len(user) >= 3:
                t = re.sub(rf"(?<=[/\\-]){re.escape(user)}(?=[/\\\"-]|$)", "<USER>", t)
            return t

        session_ids = []
        for n, (name, raw) in enumerate(payloads):
            (stage / "hooks" / f"{n:03d}-{name}.json").write_text(
                clean(raw.decode("utf-8", "replace")), encoding="utf-8"
            )
            try:
                sid = json.loads(raw).get("session_id")
            except ValueError:
                sid = None
            if sid and sid not in session_ids:
                session_ids.append(sid)
        projects = cfg / "projects"
        main = []
        for sid in session_ids:
            main += list(projects.rglob(f"{sid}.jsonl")) if projects.is_dir() else []
        if main:
            lines = []
            for p in main:
                lines.append(p.read_text(encoding="utf-8"))
            (stage / "transcript.jsonl").write_text(clean("".join(lines)), encoding="utf-8")
            for p in main:
                sub = p.with_suffix("") / "subagents"
                if sub.is_dir():
                    (stage / "subagents").mkdir(exist_ok=True)
                    for s in sorted(sub.glob("*.jsonl")):
                        (stage / "subagents" / s.name).write_text(clean(s.read_text(encoding="utf-8")), encoding="utf-8")
        else:
            (stage / "transcript.jsonl").write_text("", encoding="utf-8")
        manifest = {
            "scenario": sc.name,
            "claude_code_version": version,
            "captured_at": datetime.date.today().isoformat(),
            "os": {"darwin": "macos", "win32": "windows"}.get(sys.platform, sys.platform),
            "os_release": platform.release(),
            "simulated": sc.simulated,
            "note": sc.note,
            "prompts": sc.prompts,
            "model": model or "default",
            "session_ids": session_ids,
            "hooks_received": [name for name, _ in payloads],
            "runs": results,
            "transcript_found": bool(main),
        }
        (stage / "manifest.json").write_text(clean(json.dumps(manifest, indent=2)) + "\n", encoding="utf-8")
        dest = out / sc.name
        if dest.exists():
            shutil.rmtree(dest)
        out.mkdir(parents=True, exist_ok=True)
        stage.rename(dest)


def capture(args) -> int:
    try:
        prog, version = preflight(args.claude, args.preflight_timeout)
    except PreflightError as e:
        print(f"preflight failed: {e.reason}", file=sys.stderr)
        print(f"  {e.detail}", file=sys.stderr)
        if e.reason == "not-logged-in":
            print(
                "  A temp CLAUDE_CONFIG_DIR has no login: run `claude setup-token` and rerun with "
                "CLAUDE_CODE_OAUTH_TOKEN=<token> (or ANTHROPIC_API_KEY) set.",
                file=sys.stderr,
            )
        return 2
    print(f"preflight ok: claude {version}")
    wanted = args.scenario or SCENARIO_NAMES
    failed = []
    for sc in SCENARIOS:
        if sc.name not in wanted:
            continue
        print(f"[{sc.name}] capturing...", flush=True)
        try:
            capture_scenario(sc, prog, version, args.out, args.timeout, args.model)
            print(f"[{sc.name}] ok")
        except Exception as e:  # report and skip; the rest still run
            print(f"[{sc.name}] FAILED: {e}", file=sys.stderr)
            failed.append(sc.name)
    if failed:
        print(f"failed scenarios: {', '.join(failed)}", file=sys.stderr)
        return 1
    return check(args.out)


def rescrub(out: Path) -> int:
    """Re-apply the machine-independent scrub rules to fixtures already on disk.

    Never calls `claude`. Capture-time replacements (temp dirs, home, host) are
    gone with the capture run, so this applies only the pattern-based rules,
    then runs `--check`. Idempotent.
    """
    user = getpass.getuser()
    for f in sorted(out.rglob("*")):
        if not f.is_file() or f.suffix not in (".json", ".jsonl"):
            continue
        before = f.read_text(encoding="utf-8")
        after = scrub(before, [])
        if len(user) >= 3:
            after = re.sub(rf"(?<=[/\\-]){re.escape(user)}(?=[/\\\"-]|$)", "<USER>", after)
        if after != before:
            f.write_text(after, encoding="utf-8")
            print(f"rescrubbed {f.relative_to(out)}")
    return check(out)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--check", action="store_true", help="validate committed fixtures; never calls claude")
    ap.add_argument(
        "--rescrub", action="store_true", help="re-apply scrub rules to fixtures on disk, then --check; never calls claude"
    )
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT, help="fixture folder")
    ap.add_argument("--claude", default="claude", help="claude program (default: claude on PATH)")
    ap.add_argument("--scenario", action="append", choices=SCENARIO_NAMES, help="capture only these")
    ap.add_argument("--timeout", type=float, default=240, help="hard wall-clock seconds per claude call")
    ap.add_argument("--preflight-timeout", type=float, default=45, help="seconds per preflight step")
    ap.add_argument("--model", default="haiku", help="model alias passed to claude (empty: default)")
    args = ap.parse_args(argv)
    if args.check:
        return check(args.out)
    if args.rescrub:
        return rescrub(args.out)
    return capture(args)


if __name__ == "__main__":
    sys.exit(main())
