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


# Core SQL names the bare table only to write it, to lock its rows, or in
# row-security policy text; every other read goes through a view that states
# whether forgotten memories are wanted (`definitions/views.sql`). Not scanned:
# `policies.sql`, `views.sql` and migrations (released files are immutable).
MEMORY_READ = re.compile(
    r"\b(DELETE\s+)?(?:FROM|JOIN|USING)\s+(?:[\w.]+(?:\s+\w+)?\s*,\s*)*(proxiima_core\.memory)(?![\w.])",
    re.IGNORECASE,
)
ROW_LOCK = re.compile(r"\bFOR\s+(?:NO\s+KEY\s+UPDATE|UPDATE|KEY\s+SHARE|SHARE)\b", re.IGNORECASE)
# The rest of a statement: to the next `;` in SQL, to the closing quote of the
# string literal in Rust.
STATEMENT_REST = {".sql": re.compile(r"[^;]*"), ".rs": re.compile(r'(?:[^"\\]|\\[\s\S])*')}
# Test code is not a core read: a file gated `#![cfg(test)]`, and everything
# from an inline test module on.
TEST_CODE = re.compile(r"^#!\[cfg\(test\)\]|^#\[cfg\(test\)\]\nmod \w+ \{", re.MULTILINE)
MEMORY_READ_FILES = (
    "crates/storage-pg/definitions/functions.sql",
    "crates/storage-pg/definitions/triggers.sql",
    "crates/storage-pg/definitions/vector.sql",
)
MEMORY_READ_RUST_ROOTS = ("crates/*/src", "apps", "tools", "spiices")


def bare_memory_reads(text: str, suffix: str) -> list[int]:
    """Lines of reads of the bare table. A read that locks its rows is allowed:
    the serving role cannot lock rows through a SELECT-only view."""
    if suffix == ".rs" and (tests := TEST_CODE.search(text)):
        text = text[: tests.start()]
    comment = "//" if suffix == ".rs" else "--"
    lines = text.splitlines()
    hits: list[int] = []
    for read in MEMORY_READ.finditer(text):
        line_no = text.count("\n", 0, read.start(2)) + 1
        if read.group(1) or lines[line_no - 1].lstrip().startswith(comment):
            continue
        rest = STATEMENT_REST[suffix].match(text, read.end())
        if rest is None or not ROW_LOCK.search(rest.group()):
            hits.append(line_no)
    return hits


def memory_read_files() -> list[Path]:
    files = [ROOT / rel for rel in MEMORY_READ_FILES]
    for pattern in MEMORY_READ_RUST_ROOTS:
        for root in ROOT.glob(pattern):
            files.extend(
                path
                for path in root.rglob("*.rs")
                if not {"target", "tests"} & set(path.relative_to(ROOT).parts)
                and path.stem != "tests"
                and not path.stem.endswith("_tests")
            )
    return sorted(set(files))


def check_memory_reads(findings: list[Finding]) -> None:
    for path in memory_read_files():
        text = path.read_text(encoding="utf-8")
        lines = text.splitlines()
        for line_no in bare_memory_reads(text, path.suffix):
            findings.append(
                Finding(
                    path,
                    line_no,
                    "core SQL reads proxiima_core.memory; name proxiima_core.live_memory or proxiima_core.memory_including_forgotten",
                    lines[line_no - 1],
                )
            )


# #488: the owner read policy shows forgotten memories only when the
# transaction-local GUC below is on. One module sets it (PgLifecycleScope);
# a second writer would be a way around the policy nobody reviewed.
LIFECYCLE_GUC_WRITE = re.compile(
    r"set_config\(\s*'app\.proxiima_lifecycle'|\bSET\s+(?:LOCAL\s+)?(?:SESSION\s+)?app\.proxiima_lifecycle\b",
    re.IGNORECASE,
)
LIFECYCLE_GUC_MODULE = "crates/storage-pg/src/lifecycle_scope.rs"
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
    fixture_text = memory_fixture.read_text(encoding="utf-8")
    flagged = bare_memory_reads(fixture_text, ".rs")
    expected = [5, 6, 7, 8, 9, 13]  # the BAD lines of the fixture; a multi-line read counts at its FROM
    if flagged != expected:
        failures.append(f"memory reads: flagged lines {flagged}, expected {expected}")
    sql_sample = (
        "-- FROM proxiima_core.memory in a comment\n"
        "SELECT 1 FROM proxiima_core.memory m\n"
        "  WHERE m.id = 1\n"
        "  FOR UPDATE;\n"
        "SELECT 1 FROM proxiima_core.memory m;\n"
        "SELECT 1 FROM proxiima_core.live_memory FOR SHARE;\n"
        "SELECT 1 FROM proxiima_core.memory_head;\n"
        "DELETE FROM proxiima_core.memory WHERE id = 1;\n"
        "SELECT 1 FROM proxiima_core.memory;\n"
        "select 1 from proxiima_core.memory;\n"
    )
    if bare_memory_reads(sql_sample, ".sql") != [5, 9, 10]:
        failures.append(f"memory reads: sql sample flagged {bare_memory_reads(sql_sample, '.sql')}, expected [5, 9, 10]")
    gated = '#![cfg(test)]\nconst READ: &str = "SELECT 1 FROM proxiima_core.memory";\n'
    if bare_memory_reads(gated, ".rs"):
        failures.append("memory reads: a file gated #![cfg(test)] was scanned")
    guc_files = {
        LIFECYCLE_GUC_MODULE: "sqlx::query(\"SELECT set_config('app.proxiima_lifecycle', 'on', true)\")",
        "crates/storage-pg/src/other.rs": "sqlx::query(\"SELECT set_config( 'app.proxiima_lifecycle', 'on', true)\")",
        "crates/x/src/y.sql": "SET LOCAL app.proxiima_lifecycle = 'on';",
        "crates/storage-pg/definitions/policies.sql": "current_setting('app.proxiima_lifecycle'::text, true) = 'on'::text",
    }
    if [rel for rel, _line in lifecycle_guc_writers(guc_files)] != ["crates/storage-pg/src/other.rs", "crates/x/src/y.sql"]:
        failures.append("lifecycle scope: a second writer of app.proxiima_lifecycle was not detected (or a reader was)")
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
