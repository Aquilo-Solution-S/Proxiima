#!/usr/bin/env python3
"""Architecture guardrails.

These checks intentionally scan source text rather than relying on compile-time
reachability. A stale public vocabulary or bypass surface should be removed, not
left around behind an adapter.
"""
from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Finding:
    path: Path
    line: int
    rule: str
    text: str

    def render(self) -> str:
        rel = self.path.relative_to(ROOT)
        return f"{rel}:{self.line}: {self.rule}: {self.text.strip()}"


def is_text_file(path: Path) -> bool:
    return path.suffix in {".rs", ".sql", ".toml", ".yaml", ".yml"}


def iter_files(*roots: str) -> list[Path]:
    files: list[Path] = []
    for root in roots:
        base = ROOT / root
        if not base.exists():
            continue
        if base.is_file():
            files.append(base)
            continue
        files.extend(
            p
            for p in base.rglob("*")
            if p.is_file()
            and is_text_file(p)
            and "/target/" not in p.as_posix()
            and "/.git/" not in p.as_posix()
        )
    return sorted(files)


# Census of reads of `proxiima_core.memory` that do not go through the owner
# read policy (#488). Owner-scope SQL is decided by `proxiima_owner_read`;
# platform-scope SQL bypasses it, so every such read states whether it wants
# live memories only or forgotten ones too. The counts are pinned so a new
# platform read is classified on purpose.
FORGET_POLICY_MARKER = re.compile(r"FORGET-POLICY: (live-only|includes-forgotten) \(([^)]*\S[^)]*)\)")
MEMORY_READ = re.compile(
    r"\b(?:FROM|JOIN|UPDATE|INTO)\s+(?:[\w.]+(?:\s+\w+)?\s*,\s*)*proxiima_core\.memory\b"
)
PLATFORM_SCOPE_BINDING = re.compile(
    r"PgPlatformScope|platform_scope|platform_transaction\(|set_config\('app\.proxiima_scope', 'platform'"
)
FUNCTION_START = re.compile(r"CREATE (?:OR REPLACE )?FUNCTION ")
MEMORY_CENSUS_ALWAYS = (
    "crates/storage-pg/definitions/functions.sql",
    "crates/storage-pg/definitions/triggers.sql",
    "crates/storage-pg/definitions/vector.sql",
)
# Verb modules whose callers bind platform scope (ports/embeddings.rs) rather
# than the module itself, so the file-level binding test cannot see them.
MEMORY_CENSUS_PLATFORM_CALLEES = (
    "crates/storage-pg/src/verbs/fact_embeddings/",
    # Content GC runs only inside transfer, forget and erase transactions.
    "crates/storage-pg/src/verbs/content.rs",
    # The head lookup the natural-key series resolution runs inside the
    # lifecycle scope of memory_timeseries.rs (#488 review r3).
    "crates/storage-pg/src/verbs/query/series_handle.rs",
)
# A live-only read filters forgotten rows itself (#488 S6): the read's line or
# the statement lines right after it say so. Reads inside generated owner-RLS
# policy text are hidden by the policy and say that in their reason instead.
LIVE_ONLY_FILTER = re.compile(r"\bforgotten_at IS NULL\b")
LIVE_ONLY_WINDOW = 8
LIVE_ONLY_RLS_REASON = "under owner RLS"


def in_memory_census(text: str, suffix: str, rel: str = "") -> bool:
    if suffix not in {".rs", ".sql"}:
        return False
    return (
        rel.startswith(MEMORY_CENSUS_PLATFORM_CALLEES)
        or PLATFORM_SCOPE_BINDING.search(text) is not None
        # A module that turns the lifecycle scope on reads forgotten memories
        # through the owner policy (#488 review r2), so its reads say which
        # side of forget they want, like platform-scope reads.
        or LIFECYCLE_SCOPE_ENTER.search(text) is not None
    )


def live_only_statement(lines: list[str], index: int, suffix: str) -> str:
    """The rest of the statement a read starts: up to the literal's closing
    quote (Rust) or the semicolon (SQL), at most LIVE_ONLY_WINDOW lines."""
    end = '"' if suffix == ".rs" else ";"
    match = MEMORY_READ.search(lines[index])
    parts: list[str] = []
    rest = lines[index][match.end() if match else 0 :]
    for offset in range(LIVE_ONLY_WINDOW):
        if offset:
            if index + offset >= len(lines):
                break
            rest = lines[index + offset]
        stop = rest.find(end)
        if stop >= 0:
            parts.append(rest[:stop])
            break
        parts.append(rest)
    return "\n".join(parts)


def unfiltered_live_only(text: str, suffix: str) -> list[int]:
    """Live-only reads whose statement does not filter forgotten rows."""
    lines = text.splitlines()
    unfiltered: list[int] = []
    for line_no, kind in memory_read_hits(text, suffix):
        if kind != "live-only":
            continue
        if LIVE_ONLY_FILTER.search(live_only_statement(lines, line_no - 1, suffix)):
            continue
        if suffix == ".sql" and LIVE_ONLY_RLS_REASON in text:
            reasons = [m.group(2) for m in FORGET_POLICY_MARKER.finditer(text) if m.group(1) == "live-only"]
            if reasons and all(LIVE_ONLY_RLS_REASON in reason for reason in reasons):
                continue
        unfiltered.append(line_no)
    return unfiltered


def memory_read_hits(text: str, suffix: str) -> list[tuple[int, str | None]]:
    """Each read of the memory spine, with its marker class (None: unmarked)."""
    lines = text.splitlines()
    comment = "//" if suffix == ".rs" else "--"
    hits: list[tuple[int, str | None]] = []
    for index, line in enumerate(lines):
        # An inline test module ends the census: tests read the spine freely.
        following = lines[index + 1] if index + 1 < len(lines) else ""
        if suffix == ".rs" and line == "#[cfg(test)]" and re.match(r"mod \w+ \{", following):
            break
        if line.strip().startswith(comment) or not MEMORY_READ.search(line):
            continue
        kind = None
        candidates = [line, lines[index - 1] if index else ""]
        if suffix == ".sql":
            # A function body is pinned verbatim elsewhere (migrations,
            # generated triggers), so a definitions function carries one
            # marker, on the line above its CREATE, for every read inside it.
            for back in range(index, -1, -1):
                if FUNCTION_START.match(lines[back]):
                    candidates.append(lines[back - 1] if back else "")
                    break
        for candidate in candidates:
            marker = FORGET_POLICY_MARKER.search(candidate)
            if marker:
                kind = marker.group(1)
                break
        hits.append((index + 1, kind))
    return hits


def memory_census_files() -> list[Path]:
    files = [ROOT / rel for rel in MEMORY_CENSUS_ALWAYS]
    for path in iter_files("crates/storage-pg/src"):
        if path.suffix not in {".rs", ".sql"} or path.stem == "tests" or path.stem.endswith("_tests"):
            continue
        if in_memory_census(path.read_text(encoding="utf-8"), path.suffix, path.relative_to(ROOT).as_posix()):
            files.append(path)
    return files


def check_memory_reads(findings: list[Finding]) -> None:
    for path in memory_census_files():
        text = path.read_text(encoding="utf-8")
        lines = text.splitlines()
        for line_no, kind in memory_read_hits(text, path.suffix):
            if kind is None:
                findings.append(
                    Finding(path, line_no, "platform-scope memory read without FORGET-POLICY marker", lines[line_no - 1])
                )
        for line_no in unfiltered_live_only(text, path.suffix):
            findings.append(
                Finding(path, line_no, "live-only memory read without forgotten_at IS NULL", lines[line_no - 1])
            )


# #488: the owner read policy shows forgotten memories only when the
# transaction-local GUC below is on. One module sets it (PgLifecycleScope);
# a second writer would be a way around the policy nobody reviewed.
LIFECYCLE_GUC_WRITE = re.compile(
    r"set_config\(\s*'app\.proxiima_lifecycle'|\bSET\s+(?:LOCAL\s+)?(?:SESSION\s+)?app\.proxiima_lifecycle\b",
    re.IGNORECASE,
)
LIFECYCLE_GUC_MODULE = "crates/storage-pg/src/lifecycle_scope.rs"
# A module that enters the lifecycle scope reads forgotten memories, so its
# memory reads join the census above.
LIFECYCLE_SCOPE_ENTER = re.compile(r"\bPgLifecycleScope::(?:enter|begin)\(")
LIFECYCLE_GUC_ROOTS = ("crates", "apps", "tools", "spiices")


def lifecycle_guc_writers(files: dict[str, str]) -> list[tuple[str, int]]:
    """Every place outside the lifecycle module that sets the lifecycle GUC."""
    writers: list[tuple[str, int]] = []
    for rel, text in sorted(files.items()):
        if rel == LIFECYCLE_GUC_MODULE:
            continue
        for index, line in enumerate(text.splitlines(), start=1):
            if LIFECYCLE_GUC_WRITE.search(line):
                writers.append((rel, index))
    return writers


def check_lifecycle_guc(findings: list[Finding]) -> None:
    files: dict[str, str] = {}
    for root in LIFECYCLE_GUC_ROOTS:
        base = ROOT / root
        if not base.is_dir():
            continue
        for path in base.rglob("*"):
            if path.suffix not in {".rs", ".sql"} or "target" in path.parts or not path.is_file():
                continue
            files[path.relative_to(ROOT).as_posix()] = path.read_text(encoding="utf-8")
    module = files.get(LIFECYCLE_GUC_MODULE, "")
    if not LIFECYCLE_GUC_WRITE.search(module):
        findings.append(Finding(ROOT / LIFECYCLE_GUC_MODULE, 1, "lifecycle scope", "the lifecycle module no longer sets app.proxiima_lifecycle"))
    for rel, line_no in lifecycle_guc_writers(files):
        line = files[rel].splitlines()[line_no - 1]
        findings.append(Finding(ROOT / rel, line_no, "app.proxiima_lifecycle set outside PgLifecycleScope", line))


def run_self_test() -> int:
    """Assert each check's detector fires on its committed fixture or inline sample."""
    failures: list[str] = []
    memory_fixture = ROOT / "scripts/fixtures/architecture-guardrails/memory_reads.rs"
    rel = memory_fixture.relative_to(ROOT)
    memory_hits = memory_read_hits(memory_fixture.read_text(encoding="utf-8"), ".rs")
    unmarked = [line for line, kind in memory_hits if kind is None]
    marked = sorted(kind for _line, kind in memory_hits if kind is not None)
    if len(unmarked) != 2:
        failures.append(f"memory census: expected the unmarked and the empty-reason read flagged, got {unmarked}")
    if marked != ["includes-forgotten", "live-only"]:
        failures.append(f"memory census: marked reads not classified (same line, line above): {marked}")
    sql_fixture = (
        "-- FORGET-POLICY: live-only (fixture) applies to every read in this function\n"
        "CREATE OR REPLACE FUNCTION proxiima_core.f() RETURNS void AS $$\n"
        "  SELECT 1 FROM proxiima_core.memory m;\n$$;\n"
        "CREATE OR REPLACE FUNCTION proxiima_core.g() RETURNS void AS $$\n"
        "  SELECT 1 FROM proxiima_core.memory m;\n$$;\n"
    )
    if [kind for _line, kind in memory_read_hits(sql_fixture, ".sql")] != ["live-only", None]:
        failures.append("memory census: a function-level marker must cover its own function only")
    if not in_memory_census("let scope = PgPlatformScope::begin(&pool).await?;", ".rs"):
        failures.append("memory census: a platform-scope file was not recognized")
    if in_memory_census("let scope = owner_scope(&pool).await?;", ".rs"):
        failures.append("memory census: an owner-scope file was put in the census")
    guc_files = {
        LIFECYCLE_GUC_MODULE: "sqlx::query(\"SELECT set_config('app.proxiima_lifecycle', 'on', true)\")",
        "crates/storage-pg/src/other.rs": "sqlx::query(\"SELECT set_config( 'app.proxiima_lifecycle', 'on', true)\")",
        "crates/x/src/y.sql": "SET LOCAL app.proxiima_lifecycle = 'on';",
        "crates/storage-pg/definitions/policies.sql": "current_setting('app.proxiima_lifecycle'::text, true) = 'on'::text",
    }
    if [rel for rel, _line in lifecycle_guc_writers(guc_files)] != ["crates/storage-pg/src/other.rs", "crates/x/src/y.sql"]:
        failures.append("lifecycle scope: a second writer of app.proxiima_lifecycle was not detected (or a reader was)")
    if not in_memory_census("let lifecycle = crate::lifecycle_scope::PgLifecycleScope::enter(tx.as_mut()).await?;", ".rs"):
        failures.append("memory census: a lifecycle-scope file was not recognized")
    live_fixture = (
        'sqlx::query("SELECT t FROM proxiima_core.memory /* FORGET-POLICY: live-only (fixture) */ WHERE t = $1")\n'
        'sqlx::query("SELECT t FROM proxiima_core.memory /* FORGET-POLICY: live-only (fixture) */\n'
        '    WHERE t = $1 AND forgotten_at IS NULL")\n'
        'sqlx::query("UPDATE j FROM claimed, proxiima_core.memory m /* FORGET-POLICY: includes-forgotten (fixture) */")\n'
    )
    if unfiltered_live_only(live_fixture, ".rs") != [1]:
        failures.append("memory census: a live-only read without the forgotten filter was not flagged (or a filtered one was)")
    if [kind for _line, kind in memory_read_hits(live_fixture, ".rs")] != ["live-only", "live-only", "includes-forgotten"]:
        failures.append("memory census: a comma join to proxiima_core.memory was not recognized as a read")
    if not in_memory_census("fn f() {}", ".rs", "crates/storage-pg/src/verbs/fact_embeddings/write.rs"):
        failures.append("memory census: a platform callee module was not put in the census")
    if failures:
        for failure in failures:
            print(f"self-test failed: {failure}: {rel}", file=sys.stderr)
        return 1
    print("self-test passed", file=sys.stderr)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="assert the detectors fire on their fixtures; exits zero when they do",
    )
    args = parser.parse_args()

    if args.self_test:
        return run_self_test()

    findings: list[Finding] = []
    check_memory_reads(findings)
    check_lifecycle_guc(findings)

    if findings:
        print("architecture guardrail violations:", file=sys.stderr)
        for finding in findings:
            print(f"  {finding.render()}", file=sys.stderr)
        return 1
    print("architecture guardrails passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
