"""Runs one CLI headless against one of our plugins.

    python3 run.py probe claude|codex|grok [--model M]
    python3 run.py smoke claude|codex|grok [--model M]

probe loads tests/proxiima-probe for three turns in one session (a tool turn, a
compaction, the question again) and reports which lifecycle hooks fired and
whether their codewords reached the model.

smoke loads a copy of plugins/proxiima, renamed `proxiima-under-test` so that
no step touches a user's own install, with the local server added to the copy
in place of the user's own `mcp add`: a `.mcp.json` with PROXIIMA_MCP_URL and
the local issuer's headers (PROXIIMA_TEST_TOKEN, PROXIIMA_TEST_OWNER), under
`headers` for Claude Code and Grok and `http_headers` for Codex. The model
remembers three Facts and runs the consolidate skill; the run passes when the
consolidate turn wrote an Abstraction over that run's three Facts in the
database (PROXIIMA_TEST_PG, a superuser URL).

Codex and Grok install the plugin for the run and remove it afterwards. Every
run first switches off the user's own MCP servers for its duration, and refuses
to start while the model could still reach one: they include live memories
with the same `core_*` tools, and a model whose plugin server failed to load
writes there instead.
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
# The smoke copy's own name: install and cleanup never touch a user's own `proxiima` plugin.
UNDER_TEST = "proxiima-under-test"
ASK = ("this is a test of our own hook plugin: its hooks hand you codewords that start with "
       "`CODEWORD-`. List every such codeword you have been given in this conversation, one per "
       "line, or write NONE.")


def run(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    # RUN lies outside any project, so no CLI loads a project's MCP config.
    return subprocess.run(cmd, capture_output=True, text=True, timeout=300, stdin=subprocess.DEVNULL, cwd=RUN)


def sh(cmd: list[str]) -> str:
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


def restore(config: Path, saved: str) -> None:
    """Writes `saved` back and names the keys the run changed, never their values."""
    changed = set(config.read_text().splitlines()) - set(saved.splitlines())
    config.write_text(saved)
    if changed:
        print(f"restored {config}; the run had changed {sorted({line.split('=')[0].strip() for line in changed})}")


def phase(name: str) -> None:
    (RUN / "phase").write_text(name)


class Claude:
    model = "haiku"

    def __init__(self, model: str, plugin: Path, endpoint: str | None) -> None:
        # Claude's init event names no URLs; the plugin copy is ours, so its prefix is the provenance.
        self.model, self.plugin, self.session = model, plugin, ""
        self.ours = f"plugin:{plugin.name}:"
        self.deny: list[str] = []

    def init(self) -> dict:
        """Claude's startup event for this configuration, read before any model request."""
        cmd = ["claude", "-p", "--model", self.model, "--plugin-dir", str(self.plugin), "--tools", "",
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
        ours = "mcp__" + re.sub(r"[^A-Za-z0-9_-]", "_", self.ours)
        return sorted({t for t in self.init()["tools"] if t.startswith("mcp__") and not t.startswith(ours)})

    def teardown(self) -> None:
        pass

    def compact(self) -> str:
        return self.turn("/compact")

    def turn(self, prompt: str) -> str:
        cmd = ["claude", "-p", "--model", self.model, "--plugin-dir", str(self.plugin),
               "--output-format", "json",
               "--allowedTools", "Bash(echo probe)", "Skill", f"mcp__plugin_{self.plugin.name}_proxiima"]
        if self.session:
            cmd += ["--resume", self.session]
        out = sh(cmd + self.denied() + ["--", prompt])
        reply = json.loads(out) if out.strip() else {}
        self.session = reply.get("session_id", self.session)
        return reply.get("result", out)


class Codex:
    model = "gpt-6-luna"

    config = Path.home() / ".codex" / "config.toml"

    def __init__(self, model: str, plugin: Path, endpoint: str | None) -> None:
        self.model, self.plugin, self.endpoint, self.thread = model, plugin, endpoint, ""
        self.market = RUN / "market"
        self.saved = self.config.read_text()
        self.added: list[list[str]] = []  # the removal of each step setup completed, newest first

    def setup(self) -> None:
        shutil.copytree(self.plugin, self.market / self.plugin.name)
        index = self.market / ".agents" / "plugins" / "marketplace.json"
        index.parent.mkdir(parents=True)
        index.write_text(json.dumps({"name": MARKET, "plugins": [{
            "name": self.plugin.name,
            "source": {"source": "local", "path": f"./{self.plugin.name}"},
            "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
        }]}))
        sh(["codex", "plugin", "marketplace", "add", str(self.market)])
        self.added.insert(0, ["codex", "plugin", "marketplace", "remove", MARKET])
        sh(["codex", "plugin", "add", f"{self.plugin.name}@{MARKET}"])
        self.added.insert(0, ["codex", "plugin", "remove", f"{self.plugin.name}@{MARKET}"])

    def isolation(self) -> list[str]:
        """Overrides that switch off the user's MCP servers and every other plugin for one run."""
        config = tomllib.loads(self.config.read_text())
        flags = []
        for name in config.get("mcp_servers", {}):
            flags += ["-c", f"mcp_servers.{name}.enabled=false"]
        # A dotted `plugins."x@y".enabled` override does not take; replacing the table does.
        # Our server's tools run unprompted: Codex asks before a write tool marked open-world
        # (core_derive), and exec has no one to ask.
        ours = f"{self.plugin.name}@{MARKET}"
        approve = 'mcp_servers = { proxiima = { default_tools_approval_mode = "approve" } }'
        entries = [f'"{key}" = {{ enabled = false }}' for key in config.get("plugins", {}) if key != ours]
        entries.append(f'"{ours}" = {{ enabled = true, {approve} }}')
        return [*flags, "-c", f"plugins={{ {', '.join(entries)} }}"]

    def others(self) -> list[str]:
        """Enabled servers other than the test server, which is known by its URL, not its name:
        a user who followed the README has a live server named `proxiima` too."""
        listed = json.loads(sh(["codex", "mcp", "list", "--json", *self.isolation()]))
        return sorted(s["name"] for s in listed if s["enabled"]
                      and not (self.endpoint and s["transport"].get("url") == self.endpoint))

    def teardown(self) -> None:
        removed = [attempt(step) for step in self.added]
        restore(self.config, self.saved)
        if not all(removed):
            sys.exit("teardown left the test plugin or marketplace installed in Codex; see above")

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

    def __init__(self, model: str, plugin: Path, endpoint: str | None) -> None:
        self.model, self.plugin, self.endpoint = model, plugin, endpoint
        self.session, self.started = str(uuid.uuid4()), False
        # Install writes [plugins].enabled and uninstall leaves it behind (Grok 1.0.77);
        # the switches in setup write there too. Teardown restores the file.
        self.saved = self.config.read_text()
        self.installed = False

    def setup(self) -> None:
        listed = json.loads(sh(["grok", "inspect", "--json"]))["mcpServers"]
        # Servers of Claude Code plugins go with the plugin; `grok plugin disable` does not
        # reach them, a [plugins].disabled entry does.
        claude_plugins = sorted({s["source"]["plugin_name"] for s in listed if s["source"]["type"] == "plugin"})
        if claude_plugins:
            if "[plugins]" in self.saved:
                sys.exit(f"{self.config} has a [plugins] table; disable {claude_plugins} by hand")
            self.config.write_text(f"{self.saved}\n[plugins]\ndisabled = {json.dumps(claude_plugins)}\n")
        sh(["grok", "plugin", "install", str(self.plugin), "--trust"])
        self.installed = True
        for server in listed:
            if server["source"]["type"] != "plugin":
                sh(["grok", "mcp", "disable", server["name"]])

    def others(self) -> list[str]:
        """Servers `mcp doctor` does not report disabled, other than the test server, known by
        its URL. A session's init event cannot tell: with a plugin installed it shows every
        server as pending."""
        # doctor exits 1 while any server fails its checks, and a disabled one does.
        report = json.loads(run(["grok", "mcp", "doctor", "--json"]).stdout)["servers"]
        return sorted(s["name"] for s in report
                      if not (self.endpoint and s["target"] == self.endpoint)
                      and not any(c["label"] == "disabled in config" and not c["passed"] for c in s["checks"]))

    def teardown(self) -> None:
        removed = attempt(["grok", "plugin", "uninstall", self.plugin.name]) if self.installed else True
        restore(self.config, self.saved)
        if not removed:
            sys.exit("teardown left the test plugin installed in Grok; see above")

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


def memories() -> dict[str, tuple[str, list[str]]]:
    """Every memory version `t` with its kind and origins; origins pin versions, not handles."""
    rows = sh(["psql", os.environ["PROXIIMA_TEST_PG"], "-At", "-F", "|", "-c",
               "SELECT t, kind, array_to_string(origins, ',') FROM proxiima_core.memory"])
    return {t: (k, [o for o in origins.split(",") if o]) for t, k, origins in
            (line.split("|") for line in rows.splitlines())}


def smoke_plugin() -> Path:
    """A copy of plugins/proxiima, renamed, with the local server in it standing in for `mcp add`:
    the isolation switches off servers the user added, so the test server comes with the plugin."""
    copy = RUN / UNDER_TEST
    shutil.copytree(TESTS.parent / "proxiima", copy)
    for manifest in (copy / ".claude-plugin" / "plugin.json", copy / ".codex-plugin" / "plugin.json"):
        manifest.write_text(json.dumps({**json.loads(manifest.read_text()), "name": UNDER_TEST}, indent=2))
    headers = {"Authorization": f"Bearer {os.environ['PROXIIMA_TEST_TOKEN']}",
               "X-Proxiima-Owner": os.environ["PROXIIMA_TEST_OWNER"]}
    server = {"type": "http", "url": os.environ["PROXIIMA_MCP_URL"], "headers": headers, "http_headers": headers}
    (copy / ".mcp.json").write_text(json.dumps({"mcpServers": {"proxiima": server}}, indent=2))
    return copy


def run_facts(nonce: str) -> set[str]:
    """The Fact versions whose note names this run; the nonce is ours and hex, so it is inlined."""
    rows = sh(["psql", os.environ["PROXIIMA_TEST_PG"], "-At", "-c",
               "SELECT m.t FROM proxiima_core.memory m JOIN proxiima_core.agent_note_v1 n USING (t) "
               f"WHERE m.kind = 'fact' AND position('{nonce}' in n.title || ' ' || n.body) > 0"])
    return set(rows.split())


def smoke(cli, nonce: str) -> None:
    # Three observations a conclusion follows from, so the skill has something to derive.
    remembered = cli.turn(
        "Use the proxiima MCP tools. Store each of these observations as its own Fact with "
        f"core_remember, then reply with the handles. (1) Build {nonce} at 09:00 died: out of memory "
        f"at 2 GB. (2) Build {nonce} at 11:00 died: out of memory at 4 GB, after the limit was doubled. "
        f"(3) Build {nonce} at 13:00 died: out of memory at 8 GB, after the limit was doubled again.")
    print(f"\n--- remember reply ---\n{remembered.strip()[:800]}")
    facts = run_facts(nonce)
    if len(facts) != 3:
        sys.exit(f"FAIL: expected this run's three Facts, found {len(facts)}")

    before = memories()
    consolidated = cli.turn("Use the consolidate skill to store this session's work in Proxiima.")
    print(f"\n--- consolidate reply ---\n{consolidated.strip()[:800]}")
    new = {t: v for t, v in memories().items() if t not in before}
    print(f"\nnew memories in the consolidate turn: {sorted(kind for kind, _ in new.values())}")
    if not any(kind == "abstraction" and facts <= set(origins) for kind, origins in new.values()):
        sys.exit("FAIL: the consolidate turn wrote no Abstraction over this run's three Facts")
    print("PASS: the consolidate turn wrote an Abstraction over this run's three Facts")


def main() -> None:
    clis = {"claude": Claude, "codex": Codex, "grok": Grok}
    parser = argparse.ArgumentParser()
    parser.add_argument("scenario", choices=("probe", "smoke"))
    parser.add_argument("cli", choices=clis)
    parser.add_argument("--model")
    args = parser.parse_args()

    shutil.rmtree(RUN, ignore_errors=True)
    RUN.mkdir()
    nonce = uuid.uuid4().hex[:8]
    (RUN / "nonce").write_text(nonce)
    phase("setup")
    plugin = TESTS / "proxiima-probe" if args.scenario == "probe" else smoke_plugin()
    kind = clis[args.cli]
    cli = kind(args.model or kind.model, plugin, os.environ["PROXIIMA_MCP_URL"] if args.scenario == "smoke" else None)
    print(f"{args.scenario} {args.cli} {cli.model}")
    try:
        cli.setup()
        if reachable := cli.others():
            sys.exit(f"refusing to run: the model could still reach {reachable}")
        probe(cli) if args.scenario == "probe" else smoke(cli, nonce)
    finally:
        phase("teardown")
        cli.teardown()


if __name__ == "__main__":
    main()
