"""Runs one CLI headless against one of our plugins.

    python3 run.py probe|smoke|restraint|compaction claude|codex|grok [--model M]

probe loads tests/proxiima-probe for three turns in one session (a tool turn, a
compaction, the question again) and reports which lifecycle hooks fired and
whether their codewords reached the model.

A plugin under test is a copy with `-under-test` added to its name, so that no
step touches a user's own install of it.

smoke loads a copy of plugins/proxiima with the local server added to it in
place of the user's own `mcp add`: a `.mcp.json` with PROXIIMA_MCP_URL and
the local issuer's headers (PROXIIMA_TEST_TOKEN, PROXIIMA_TEST_OWNER), under
`headers` for Claude Code and Grok and `http_headers` for Codex. The model
remembers three Facts and runs the consolidate skill; the run passes when the
consolidate turn wrote an Abstraction over that run's three Facts in the
database (PROXIIMA_TEST_PG, a superuser URL).

restraint loads the same copy, has the model remember two unrelated Facts and
run the consolidate skill; the run passes when that turn derived no
Abstraction.

compaction adds a copy of plugins/proxiima-compaction. The model remembers
three Facts, the CLI compacts, and the next turn asks only for a shell command;
the run passes when that turn wrote an Abstraction over the three Facts, which
only the reminder after compaction asks for.

Codex and Grok install the plugins for the run and remove them afterwards.
Every run first switches off the user's own MCP servers for its duration, and
refuses to start while the model could still reach one: they include live
memories with the same `core_*` tools, and a model whose plugin server failed
to load writes there instead. No run writes a saved copy of a user's config
back, which would revert whatever someone else changed in it meanwhile: each
step is undone by the CLI's own command, which edits the file as it is then.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
import uuid
from pathlib import Path

TESTS = Path(__file__).resolve().parent
RUN = Path("/tmp/proxiima-probe")
MARKET = "proxiima-test-market"
ASK =("this is a test of our own hook plugin: its hooks hand you codewords that start with "
       "`CODEWORD-`. List every such codeword you have been given in this conversation, one per "
       "line, or write NONE.")


def run(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    # RUN lies outside the user's projects: the one project config a CLI finds there is the
    # one Grok's setup writes.
    return subprocess.run(cmd, capture_output=True, text=True, timeout=300, stdin=subprocess.DEVNULL, cwd=RUN)


def sh(cmd: list[str]) -> str:
    """Runs `cmd` and stops the run if it fails, printing it: nothing secret goes in `cmd`."""
    done = run(cmd)
    if done.returncode != 0:
        sys.exit(f"$ {' '.join(cmd)}\nexit {done.returncode}\n{done.stderr[-2000:]}")
    return done.stdout


def attempt(cmd: list[str]) -> bool:
    """A cleanup step: reports a failure and returns False instead of stopping the cleanup."""
    try:
        done = run(cmd)
    except (OSError, subprocess.SubprocessError) as error:
        print(f"$ {' '.join(cmd)}\n{error}", file=sys.stderr)
        return False
    if done.returncode != 0:
        print(f"$ {' '.join(cmd)}\nexit {done.returncode}\n{done.stderr[-2000:]}", file=sys.stderr)
    return done.returncode == 0


def changed_keys(before: str, after: str) -> list[str]:
    """The dotted keys whose values differ between two versions of a TOML config; never the values,
    which include the user's tokens."""
    def flat(table: dict, prefix: str = "") -> dict[str, object]:
        out: dict[str, object] = {}
        for key, value in table.items():
            out.update(flat(value, f"{prefix}{key}.") if isinstance(value, dict) else {f"{prefix}{key}": value})
        return out
    old, new = flat(tomllib.loads(before)), flat(tomllib.loads(after))
    return sorted(key for key in old.keys() | new.keys() if old.get(key) != new.get(key))


def phase(name: str) -> None:
    (RUN / "phase").write_text(name)


class Claude:
    model = "haiku"

    def __init__(self, model: str, plugins: list[Path], endpoint: str | None) -> None:
        # Claude's init event names no URLs; the plugins are ours, so their prefixes are the provenance.
        self.model, self.plugins, self.session = model, plugins, ""
        self.ours = tuple(f"plugin:{plugin.name}:" for plugin in plugins)
        self.dirs = [arg for plugin in plugins for arg in ("--plugin-dir", str(plugin))]
        self.deny: list[str] = []

    def init(self) -> dict:
        """Claude's startup event for this configuration, read before any model request."""
        cmd = ["claude", "-p", "--model", self.model, *self.dirs, "--tools", "",
               "--output-format", "stream-json", "--verbose", *self.denied(), "--", "Reply OK."]
        with subprocess.Popen(cmd, stdout=subprocess.PIPE, stdin=subprocess.DEVNULL, text=True, cwd=RUN) as claude:
            for line in claude.stdout or ():
                event = json.loads(line)
                if event.get("subtype") == "init":
                    claude.kill()
                    return event
        sys.exit("claude printed no init event")

    def denied(self) -> list[str]:
        return ["--disallowedTools", *self.deny] if self.deny else []

    def setup(self) -> None:
        # --strict-mcp-config drops plugin servers too, so every other server's tools are denied.
        self.deny = ["mcp__" + re.sub(r"[^A-Za-z0-9_-]", "_", server["name"])
                     for server in self.init()["mcp_servers"] if not server["name"].startswith(self.ours)]

    def others(self) -> list[str]:
        ours = tuple("mcp__" + re.sub(r"[^A-Za-z0-9_-]", "_", prefix) for prefix in self.ours)
        return sorted({t for t in self.init()["tools"] if t.startswith("mcp__") and not t.startswith(ours)})

    def teardown(self) -> None:
        pass

    def compact(self) -> str:
        return self.turn("/compact")

    def turn(self, prompt: str) -> str:
        servers = [f"mcp__plugin_{plugin.name}_proxiima" for plugin in self.plugins]
        cmd = ["claude", "-p", "--model", self.model, *self.dirs, "--output-format", "json",
               "--allowedTools", "Bash(echo probe)", "Skill", *servers]
        if self.session:
            cmd += ["--resume", self.session]
        out = sh(cmd + self.denied() + ["--", prompt])
        reply = json.loads(out) if out.strip() else {}
        self.session = reply.get("session_id", self.session)
        return reply.get("result", out)


class Codex:
    model = "gpt-6-luna"

    config = Path.home() / ".codex" / "config.toml"

    def __init__(self, model: str, plugins: list[Path], endpoint: str | None) -> None:
        self.model, self.plugins, self.endpoint, self.thread = model, plugins, endpoint, ""
        self.market = RUN / "market"
        self.saved = self.config.read_text()  # only to report what the run changed
        # The removal of each step setup began, newest first. A step is recorded before it runs:
        # a command can change the config and still fail.
        self.added: list[list[str]] = []

    def setup(self) -> None:
        for plugin in self.plugins:
            shutil.copytree(plugin, self.market / plugin.name)
        index = self.market / ".agents" / "plugins" / "marketplace.json"
        index.parent.mkdir(parents=True)
        index.write_text(json.dumps({"name": MARKET, "plugins": [{
            "name": plugin.name,
            "source": {"source": "local", "path": f"./{plugin.name}"},
            "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
        } for plugin in self.plugins]}))
        self.added.insert(0, ["codex", "plugin", "marketplace", "remove", MARKET])
        sh(["codex", "plugin", "marketplace", "add", str(self.market)])
        for plugin in self.plugins:
            self.added.insert(0, ["codex", "plugin", "remove", f"{plugin.name}@{MARKET}"])
            sh(["codex", "plugin", "add", f"{plugin.name}@{MARKET}"])

    def isolation(self) -> list[str]:
        """Overrides that switch off the user's MCP servers and every other plugin for one run."""
        config = tomllib.loads(self.config.read_text())
        flags = []
        for name in config.get("mcp_servers", {}):
            flags += ["-c", f"mcp_servers.{name}.enabled=false"]
        # A dotted `plugins."x@y".enabled` override does not take; replacing the table does.
        # Our server's tools run unprompted: Codex asks before a write tool marked open-world
        # (core_derive), and exec has no one to ask.
        ours = {f"{plugin.name}@{MARKET}": (plugin / ".mcp.json").exists() for plugin in self.plugins}
        approve = ', mcp_servers = { proxiima = { default_tools_approval_mode = "approve" } }'
        entries = [f'"{key}" = {{ enabled = false }}' for key in config.get("plugins", {}) if key not in ours]
        entries += [f'"{key}" = {{ enabled = true{approve if server else ""} }}' for key, server in ours.items()]
        return [*flags, "-c", f"plugins={{ {', '.join(entries)} }}"]

    def others(self) -> list[str]:
        """Enabled servers other than the test server, which is known by its URL, not its name:
        a user who followed the README has a live server named `proxiima` too."""
        listed = json.loads(sh(["codex", "mcp", "list", "--json", *self.isolation()]))
        return sorted(s["name"] for s in listed if s["enabled"]
                      and not (self.endpoint and s["transport"].get("url") == self.endpoint))

    def teardown(self) -> None:
        # Each removal undoes its setup step, so the file is never written back: that would
        # also revert whatever someone else changed in it while the run went on.
        removed = [attempt(step) for step in self.added]
        if changed := changed_keys(self.saved, self.config.read_text()):
            print(f"{self.config} changed during the run, left as it is: {changed}")
        if not all(removed):
            sys.exit("teardown left a test plugin or the test marketplace installed in Codex; see above")

    def compact(self) -> str:
        # exec runs no slash commands; a low limit makes Codex compact before the request.
        return self.turn("Reply OK.", ("-c", "model_auto_compact_token_limit=2000"))

    def turn(self, prompt: str, extra: tuple[str, ...] = ()) -> str:
        last = RUN / "codex-last.txt"
        last.unlink(missing_ok=True)
        flags = ["--json", "-m", self.model, "--dangerously-bypass-hook-trust", "--skip-git-repo-check",
                 "-c", 'sandbox_mode="read-only"', "-c", 'approval_policy="never"', "-o", str(last), *self.isolation(), *extra]
        cmd = ["codex", "exec", "resume", self.thread, *flags] if self.thread else ["codex", "exec", *flags]
        for line in sh(cmd + [prompt]).splitlines():
            item = json.loads(line) if line.startswith("{") else {}
            self.thread = item.get("thread_id", self.thread)
        return last.read_text() if last.exists() else ""


class Grok:
    model = "grok-4.7-build-fast"
    config = Path.home() / ".grok" / "config.toml"

    def __init__(self, model: str, plugins: list[Path], endpoint: str | None) -> None:
        self.model, self.plugins, self.endpoint = model, plugins, endpoint
        self.session, self.started = str(uuid.uuid4()), False
        self.saved = self.config.read_text()  # only to report what the run changed
        self.installed: list[str] = []
        self.disabled: list[str] = []  # the user's servers setup switched off, for teardown to switch on

    def setup(self) -> None:
        listed = json.loads(sh(["grok", "inspect", "--json"]))["mcpServers"]
        # Grok loads Claude Code's plugins too. Only a `plugins.disabled` entry switches their
        # servers off, which neither `grok plugin disable` nor `grok mcp disable` writes; RUN's
        # project config holds it, so the user's config does not.
        project = RUN / ".grok" / "config.toml"
        project.parent.mkdir()
        foreign = sorted({s["source"]["plugin_name"] for s in listed if s["source"]["type"] == "plugin"})
        project.write_text(f"[plugins]\ndisabled = {json.dumps(foreign)}\n")
        # Installed, as a user installs them: a plugin loaded from a path stays off until Grok
        # trusts it, and only install records that trust.
        for plugin in self.plugins:
            self.installed.append(plugin.name)  # before: a command can change the config and still fail
            sh(["grok", "plugin", "install", str(plugin), "--trust"])
        # A project config cannot switch off the user's own servers; `grok mcp disable` can, and
        # `grok mcp enable` undoes it exactly. Servers the user switched off stay as they are.
        config = tomllib.loads(self.saved)
        off = set(config.get("disabled_mcp_servers", [])) | {
            name for name, server in config.get("mcp_servers", {}).items() if server.get("enabled") is False}
        for server in listed:
            if server["source"]["type"] != "plugin" and server["name"] not in off:
                self.disabled.append(server["name"])
                sh(["grok", "mcp", "disable", server["name"]])

    def others(self) -> list[str]:
        """Servers `mcp doctor` does not report disabled, other than the test server, known by
        its URL. A session's init event cannot tell: with a plugin loaded it shows every server
        as pending."""
        # doctor exits 1 while any server fails its checks, and a disabled one does.
        report = json.loads(run(["grok", "mcp", "doctor", "--json"]).stdout)["servers"]
        return sorted(s["name"] for s in report
                      if not (self.endpoint and s["target"] == self.endpoint)
                      and not any(c["label"] == "disabled in config" and not c["passed"] for c in s["checks"]))

    def teardown(self) -> None:
        enabled = [attempt(["grok", "mcp", "enable", name]) for name in self.disabled]
        removed = [attempt(["grok", "plugin", "uninstall", name]) for name in self.installed]
        forgot = self.forget_enabled()
        if changed := changed_keys(self.saved, self.config.read_text()):
            print(f"{self.config} changed during the run, left as it is: {changed}")
        if not all([*enabled, *removed, forgot]):
            sys.exit("teardown left a test plugin installed or named in plugins.enabled, or one of "
                     "your MCP servers switched off, in Grok; see above")

    def forget_enabled(self) -> bool:
        """Grok 1.0.77's uninstall leaves the plugin's name in `plugins.enabled`, and no command
        removes it. Removes the names setup installed from the file as it is now, with the array
        and the table if that empties them, and writes only if the result parses to exactly that;
        otherwise it names them for the user and returns False. Like Grok's own commands it reads,
        then writes: an edit landing in those milliseconds is lost."""
        text = self.config.read_text()
        want = tomllib.loads(text)
        plugins = want.get("plugins", {})
        left = [name for name in plugins.get("enabled", []) if name in self.installed]
        if not left:
            return True
        plugins["enabled"] = [name for name in plugins["enabled"] if name not in left]
        if not plugins["enabled"]:
            del plugins["enabled"]
        if not plugins:
            del want["plugins"]
        # The table runs from its header to the next one. Grok writes the array on one line or
        # one entry per line; either way it is rewritten with the names that stay. Any other
        # layout fails the parse check below.
        lines = text.splitlines(keepends=True)
        start = next((i for i, line in enumerate(lines) if line.split("#")[0].strip() == "[plugins]"), None)
        if start is not None:
            end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("[")), len(lines))
            # ensure_ascii=False: JSON escapes a character outside the BMP as a surrogate pair,
            # which TOML rejects.
            names = json.dumps(plugins.get("enabled", []), ensure_ascii=False)
            stay = f"enabled = {names}\n" if "enabled" in plugins else ""
            table = re.sub(r"^enabled = \[[^\]]*\]\n", lambda _: stay, "".join(lines[start:end]),
                           count=1, flags=re.M)
            head = "".join(lines[:start])
            if table.strip() == "[plugins]":
                table = ""
                if head.endswith("\n\n"):  # the blank line Grok writes above a table it adds
                    head = head[:-1]
            new = head + table + "".join(lines[end:])
            try:
                fits = tomllib.loads(new) == want
            except tomllib.TOMLDecodeError:
                fits = False
            if fits:
                self.config.write_text(new)
                return True
        print(f"left {left} in plugins.enabled of {self.config}; remove them by hand")
        return False

    def compact(self) -> str:
        return self.turn("/compact")

    def turn(self, prompt: str) -> str:
        resume = ["-r", self.session] if self.started else ["-s", self.session]
        self.started = True
        return sh(["grok", *resume, "-m", self.model, "--always-approve", "-p", prompt])


def probe(cli) -> None:
    replies = {}
    phase("r1")
    replies["r1"] = cli.turn(f"Run the shell command `echo probe` exactly once. Then, without running anything else, {ASK}")
    phase("r2")
    replies["r2"] = cli.compact()
    phase("r3")
    replies["r3"] = cli.turn(f"Without running any tool, {ASK}")
    phase("report")
    log = RUN / "events.jsonl"
    events = [json.loads(line) for line in log.open()] if log.exists() else []
    print(f"{len(events)} hook runs, log {log}")
    print(f"{'phase':6} {'event':28} reached model in")
    # Setup's own checks start sessions too; only the three turns are the test.
    for e in (e for e in events if e["phase"] in replies):
        seen = [r for r in ("r1", "r3") if e["word"] and re.search(rf"{re.escape(e['word'])}\b", replies[r])]
        verdict = "-" if not e["word"] else (", ".join(seen) or "NOT SEEN")
        print(f"{e['phase']:6} {e['tag']:28} {verdict}")
    for name, reply in replies.items():
        print(f"\n--- {name} reply ---\n{reply.strip()[:800]}")


def psql(query: str) -> str:
    """Runs `query` on PROXIIMA_TEST_PG, a superuser URL. A failure prints neither the command
    nor psql's error: both can hold the URL's password."""
    try:
        done = run(["psql", os.environ["PROXIIMA_TEST_PG"], "-At", "-F", "|", "-c", query])
    except (OSError, subprocess.SubprocessError) as error:
        sys.exit(f"psql did not finish: {type(error).__name__}")
    if done.returncode != 0:
        sys.exit(f"psql exit {done.returncode}; run `psql \"$PROXIIMA_TEST_PG\" -c 'SELECT 1'` to see why")
    return done.stdout


def memories() -> dict[str, tuple[str, list[str]]]:
    """Every memory version `t` with its kind and origins; origins pin versions, not handles."""
    rows = psql("SELECT t, kind, array_to_string(origins, ',') FROM proxiima_core.memory")
    return {t: (k, [o for o in origins.split(",") if o]) for t, k, origins in
            (line.split("|") for line in rows.splitlines())}


def under_test(plugin: Path) -> Path:
    """A copy of `plugin`, renamed `<name>-under-test`."""
    copy = RUN / f"{plugin.name}-under-test"
    shutil.copytree(plugin, copy, ignore=shutil.ignore_patterns("__pycache__"))
    for manifest in (copy / ".claude-plugin" / "plugin.json", copy / ".codex-plugin" / "plugin.json"):
        manifest.write_text(json.dumps({**json.loads(manifest.read_text()), "name": copy.name}, indent=2))
    return copy


def with_server(plugin: Path) -> Path:
    """Adds the local server to a plugin, standing in for `mcp add`: the isolation switches off
    servers the user added, so the test server comes with the plugin."""
    headers = {"Authorization": f"Bearer {os.environ['PROXIIMA_TEST_TOKEN']}",
               "X-Proxiima-Owner": os.environ["PROXIIMA_TEST_OWNER"]}
    server = {"type": "http", "url": os.environ["PROXIIMA_MCP_URL"], "headers": headers, "http_headers": headers}
    (plugin / ".mcp.json").write_text(json.dumps({"mcpServers": {"proxiima": server}}, indent=2))
    return plugin


def run_facts(nonce: str) -> set[str]:
    """The Fact versions whose note names this run; the nonce is ours and hex, so it is inlined."""
    return set(psql("SELECT m.t FROM proxiima_core.memory m JOIN proxiima_core.agent_note_v1 n USING (t) "
                    f"WHERE m.kind = 'fact' AND position('{nonce}' in n.title || ' ' || n.body) > 0").split())


def remember_three(cli, nonce: str) -> set[str]:
    """Has the model store three observations a conclusion follows from; returns their Facts."""
    remembered = cli.turn(
        "Use the proxiima MCP tools. Store each of these observations as its own Fact with "
        f"core_remember, then reply with the handles. (1) Build {nonce} at 09:00 died: out of memory "
        f"at 2 GB. (2) Build {nonce} at 11:00 died: out of memory at 4 GB, after the limit was doubled. "
        f"(3) Build {nonce} at 13:00 died: out of memory at 8 GB, after the limit was doubled again.")
    print(f"\n--- remember reply ---\n{remembered.strip()[:800]}")
    facts = run_facts(nonce)
    if len(facts) != 3:
        sys.exit(f"FAIL: expected this run's three Facts, found {len(facts)}")
    return facts


def abstraction_over(facts: set[str], cli, prompt: str, turn: str) -> None:
    """Runs one turn and passes when it wrote an Abstraction whose origins hold all `facts`."""
    before = memories()
    reply = cli.turn(prompt)
    print(f"\n--- {turn} reply ---\n{reply.strip()[:800]}")
    new = {t: v for t, v in memories().items() if t not in before}
    print(f"\nnew memories in the {turn} turn: {sorted(kind for kind, _ in new.values())}")
    if not any(kind == "abstraction" and facts <= set(origins) for kind, origins in new.values()):
        sys.exit(f"FAIL: the {turn} turn wrote no Abstraction over this run's three Facts")
    print(f"PASS: the {turn} turn wrote an Abstraction over this run's three Facts")


def smoke(cli, nonce: str) -> None:
    facts = remember_three(cli, nonce)
    abstraction_over(facts, cli, "Use the consolidate skill to store this session's work in Proxiima.", "consolidate")


def restraint(cli, nonce: str) -> None:
    """Two unrelated observations support no conclusion: consolidating must not invent one."""
    cli.turn("Use the proxiima MCP tools. Store each of these observations as its own Fact with "
             f"core_remember: 'Run {nonce}: the office plant was watered.' and "
             f"'Run {nonce}: the release notes were proofread.'")
    if len(run_facts(nonce)) != 2:
        sys.exit("FAIL: expected this run's two Facts")
    before = memories()
    reply = cli.turn("Use the consolidate skill to store this session's work in Proxiima.")
    print(f"\n--- consolidate reply ---\n{reply.strip()[:600]}")
    new = sorted(kind for t, (kind, _) in memories().items() if t not in before)
    print(f"\nnew memories in the consolidate turn: {new}")
    if "abstraction" in new:
        sys.exit("FAIL: the consolidate turn derived an Abstraction from two unrelated Facts")
    print("PASS: the consolidate turn derived nothing from two unrelated Facts")


def compaction(cli, nonce: str) -> None:
    """Nobody asks for consolidation: the reminder after compaction has to bring it about."""
    facts = remember_three(cli, nonce)
    print(f"\n--- compact reply ---\n{cli.compact().strip()[:400]}")
    abstraction_over(facts, cli, "Run the shell command `echo probe` once and tell me what it printed.", "next")


def main() -> None:
    clis = {"claude": Claude, "codex": Codex, "grok": Grok}
    parser = argparse.ArgumentParser()
    parser.add_argument("scenario", choices=("probe", "smoke", "restraint", "compaction"))
    parser.add_argument("cli", choices=clis)
    parser.add_argument("--model")
    args = parser.parse_args()

    shutil.rmtree(RUN, ignore_errors=True)
    RUN.mkdir()
    nonce = uuid.uuid4().hex[:8]
    (RUN / "nonce").write_text(nonce)
    phase("setup")
    shipped = TESTS.parent
    plugins = {"probe": lambda: [under_test(TESTS / "proxiima-probe")],
               "smoke": lambda: [with_server(under_test(shipped / "proxiima"))],
               "restraint": lambda: [with_server(under_test(shipped / "proxiima"))],
               "compaction": lambda: [with_server(under_test(shipped / "proxiima")),
                                      under_test(shipped / "proxiima-compaction")],
               }[args.scenario]()
    endpoint = None if args.scenario == "probe" else os.environ["PROXIIMA_MCP_URL"]
    kind = clis[args.cli]
    cli = kind(args.model or kind.model, plugins, endpoint)
    print(f"{args.scenario} {args.cli} {cli.model}")
    try:
        cli.setup()
        if reachable := cli.others():
            sys.exit(f"refusing to run: the model could still reach {reachable}")
        if args.scenario == "probe":
            probe(cli)
        else:
            {"smoke": smoke, "restraint": restraint, "compaction": compaction}[args.scenario](cli, nonce)
    finally:
        phase("teardown")
        cli.teardown()


if __name__ == "__main__":
    main()
