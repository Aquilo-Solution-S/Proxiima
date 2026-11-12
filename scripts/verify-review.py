#!/usr/bin/env python3
"""Read the review record of the commits `verified` is about to advance over.

`verified` is the last commit that passed CI. `.github/workflows/verify.yml`
moves it only when this script finds every commit of `verified..tip` covered:

  authors   a commit's git author address and the address of each
            `Co-Authored-By` trailer.
  covered   the commit, or a later commit in the range, carries a
            `Reviewed-By: Name <address>` trailer whose address is not one of
            the commit's authors.
  workflow  a commit whose committer address is `--workflow-committer` is the
            workflow's own revert and needs no cover.

An address is what stands between `<` and `>` at the end of a trailer value,
compared case-insensitively. A `Reviewed-By` or `Co-Authored-By` value without
one records nothing and is named in the failure output.

Subcommands:

  check --base B --tip T [--workflow-committer ADDRESS]
                               exit 0 when no commit of B..T is uncovered,
                               exit 1 and the reasons on stderr otherwise.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

# git refuses a NUL in a commit message, so NUL frames the fields and tokens.
# `key=` matches case-insensitively and may repeat; `unfold` joins a wrapped
# value onto one line, so each trailer is one `Key: value` token.
SHOW_FORMAT = (
    "%ae%x00%ce%x00%s%x00"
    "%(trailers:key=Co-Authored-By,key=Reviewed-By,unfold,separator=%x00)"
)

TRAILING_ADDRESS = re.compile(r"<([^<>\s]+)>\s*$")


@dataclass(frozen=True)
class Commit:
    sha: str
    subject: str
    authors: frozenset[str]
    reviewers: frozenset[str]
    by_workflow: bool
    unread: tuple[str, ...]


@dataclass(frozen=True)
class Repo:
    """A repository and the environment git runs in; the default is the caller's."""

    cwd: Path | None = None
    env: dict[str, str] | None = None

    def git(self, *args: str, input: bytes | None = None) -> str:
        out = subprocess.run(
            ["git", *args],
            check=True,
            capture_output=True,
            input=input,
            cwd=self.cwd,
            env=self.env,
        ).stdout
        # git prints UTF-8 unless configured otherwise; any other byte is
        # replaced, so no commit can make the read fail.
        return out.decode("utf-8", errors="replace")


def address(value: str) -> str | None:
    found = TRAILING_ADDRESS.search(value)
    return found.group(1).lower() if found else None


def addresses(values: list[str]) -> tuple[frozenset[str], tuple[str, ...]]:
    """The addresses of trailer values, and the values that have none."""
    read = {a for v in values if (a := address(v))}
    return frozenset(read), tuple(v for v in values if address(v) is None)


def read_commit(repo: Repo, sha: str, workflow_committer: str | None) -> Commit:
    fields = repo.git("log", "-1", f"--format={SHOW_FORMAT}", sha).rstrip("\n")
    author, committer, subject, *tokens = [*fields.split("\x00"), "", "", ""]
    # The padding above leaves empty tokens, which are not trailers.
    co: list[str] = []
    reviewed: list[str] = []
    for token in tokens:
        key, _, value = token.partition(":")
        match key.strip().lower():
            case "co-authored-by":
                co.append(value.strip())
            case "reviewed-by":
                reviewed.append(value.strip())
            case _:
                pass
    co_read, co_unread = addresses([v for v in co if v])
    reviewed_read, reviewed_unread = addresses([v for v in reviewed if v])
    return Commit(
        sha=sha,
        subject=subject,
        authors=frozenset({author.lower()}) | co_read,
        reviewers=reviewed_read,
        by_workflow=workflow_committer is not None
        and committer.lower() == workflow_committer.lower(),
        unread=co_unread + reviewed_unread,
    )


def read_range(
    repo: Repo, base: str, tip: str, workflow_committer: str | None
) -> list[Commit]:
    """The commits of base..tip, oldest first."""
    shas = repo.git("rev-list", "--reverse", f"{base}..{tip}")
    return [read_commit(repo, sha, workflow_committer) for sha in shas.split()]


def uncovered(commits: list[Commit]) -> list[Commit]:
    """The commits no later (or own) reviewer outside their authors covers."""
    seen: set[str] = set()
    found = []
    for commit in reversed(commits):
        seen |= commit.reviewers
        if not commit.by_workflow and not (seen - commit.authors):
            found.append(commit)
    return found[::-1]


def reviewers_from(commits: list[Commit], commit: Commit) -> set[str]:
    return {r for c in commits[commits.index(commit) :] for r in c.reviewers}


def report(commits: list[Commit], missing: list[Commit]) -> str:
    on_actions = bool(os.environ.get("GITHUB_ACTIONS"))
    lines = []
    for commit in missing:
        head = f"{commit.sha[:12]} {commit.subject}"
        if on_actions:
            lines.append(f"::error::no independent review recorded for {head}")
        lines.append(head)
        lines.append(f"  authors: {', '.join(sorted(commit.authors))}")
        found = sorted(reviewers_from(commits, commit))
        lines.append(f"  reviewers at or after it: {', '.join(found) or 'none'}")
        for value in commit.unread:
            lines.append(f"  trailer value without an <address>: {value}")
    lines += [
        "Record the independent review on the last commit of the pushed range:",
        '  git commit --amend --no-edit --trailer "Reviewed-By: <reviewer> <address>"',
        "The address is one no author or Co-Authored-By of the reviewed commits "
        "uses. A later push can record a review an earlier one left out. "
        'See AGENTS.md, "Branch and release policy".',
    ]
    return "\n".join(lines) + "\n"


def commit(
    sha: str,
    author: str,
    *,
    co: tuple[str, ...] = (),
    reviewed: tuple[str, ...] = (),
    by_workflow: bool = False,
) -> Commit:
    return Commit(
        sha=sha,
        subject=f"subject {sha}",
        authors=frozenset({author, *co}),
        reviewers=frozenset(reviewed),
        by_workflow=by_workflow,
        unread=(),
    )


def shas(commits: list[Commit]) -> list[str]:
    return [c.sha for c in commits]


def self_test_rule() -> None:
    assert shas(uncovered([commit("c1", "a")])) == ["c1"]
    assert shas(uncovered([commit("c1", "a", co=("b",), reviewed=("b",))])) == ["c1"]
    assert shas(uncovered([commit("c1", "a", reviewed=("a",))])) == ["c1"]
    assert uncovered([commit("c1", "a", reviewed=("b",))]) == []

    three = [commit("c1", "a"), commit("c2", "a"), commit("c3", "a", reviewed=("b",))]
    assert uncovered(three) == []

    batch = [
        commit("c1", "a"),
        commit("c2", "a", reviewed=("b",)),
        commit("c3", "b"),
        commit("c4", "b", reviewed=("a",)),
    ]
    assert uncovered(batch) == []

    after = [commit("c1", "a", reviewed=("b",)), commit("c2", "a")]
    assert shas(uncovered(after)) == ["c2"]

    left_out = [commit("c1", "a"), commit("c2", "c", reviewed=("b",))]
    assert uncovered(left_out) == []
    own = [commit("c1", "a"), commit("c2", "c", reviewed=("a",))]
    assert shas(uncovered(own)) == ["c1"]

    revert = commit("r", "bot", by_workflow=True)
    assert uncovered([commit("c1", "a", reviewed=("b",)), revert]) == []
    assert shas(uncovered([commit("c1", "a"), revert])) == ["c1"]

    assert uncovered([]) == []


def self_test_git() -> None:
    bot = "bot@example.test"
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_SYSTEM"] = os.devnull
    with tempfile.TemporaryDirectory() as tmp:
        repo = Repo(Path(tmp), env)
        repo.git("init", "-q")

        def make(author: str, message: bytes, committer: str | None = None) -> None:
            identity = ["-c", "user.name=Test", "-c", f"user.email={author}"]
            who = Repo(repo.cwd, {**env, "GIT_COMMITTER_EMAIL": committer or author})
            who.git(*identity, "commit", "-q", "--allow-empty", "-F", "-", input=message)

        make("a@x.test", b"base")
        base = repo.git("rev-parse", "HEAD").strip()
        a, x = "a@x.test", frozenset({"a@x.test"})
        # (author, message, committer) -> subject, authors, reviewers, by_workflow, unread
        cases = [
            (
                "A@X.test",
                b"wrote it\n\nCo-authored-by: Codex <Noreply@OpenAI.test>\n"
                b"Reviewed-By: Rev <rev@x.test>\n",
                None,
                ("wrote it", frozenset({a, "noreply@openai.test"}),
                 frozenset({"rev@x.test"}), False, ()),
            ),
            (
                a,
                b"no address\n\nReviewed-By: Rev without address\n",
                None,
                ("no address", x, frozenset(), False, ("Rev without address",)),
            ),
            (a, b"plain\n", None, ("plain", x, frozenset(), False, ())),
            (
                a,
                b"mixed\n\nCo-Authored-By: Nobody\nReviewed-By: Rev\n  <wrapped@x.test>\n",
                None,
                ("mixed", x, frozenset({"wrapped@x.test"}), False, ("Nobody",)),
            ),
            (
                a,
                b"sep \x1d \x1e \x1f\n\nReviewed-By: S <sep@x.test>\n",
                None,
                ("sep \x1d \x1e \x1f", x, frozenset({"sep@x.test"}), False, ()),
            ),
            (
                a,
                b"caf\xe9 subject\n\nReviewed-By: R\xe9v <rev2@x.test>\n",
                None,
                ("caf\xe9 subject", x, frozenset({"rev2@x.test"}), False, ()),
            ),
            # git re-encodes a message that is not UTF-8 as it prints it: the
            # byte arrives as a character.
            (
                a,
                b"bad bytes\n\nReviewed-By: R <r\xff@x.test>\n",
                None,
                ("bad bytes", x, frozenset({"r\xff@x.test"}), False, ()),
            ),
            (
                a,
                b"two\n\nReviewed-By: One <one@x.test>\nReviewed-By: Two <two@x.test>\n",
                None,
                ("two", x, frozenset({"one@x.test", "two@x.test"}), False, ()),
            ),
            (
                a,
                b"body\n\nReviewed-By: Fake <fake@x.test>\ntext after it\n\n"
                b"Signed-off-by: Real <real@x.test>\n",
                None,
                ("body", x, frozenset(), False, ()),
            ),
            (
                bot,
                b"revert: something\n",
                bot,
                ("revert: something", frozenset({bot}), frozenset(), True, ()),
            ),
        ]
        for author, message, committer, _ in cases:
            make(author, message, committer)

        commits = read_range(repo, base, "HEAD", bot.upper())
        got = [
            (c.subject, c.authors, c.reviewers, c.by_workflow, c.unread)
            for c in commits
        ]
        assert got == [expected for *_, expected in cases], got
        assert repo.git("rev-parse", "HEAD").strip() == commits[-1].sha
        assert read_range(repo, "HEAD", "HEAD", bot) == []

        # A line outside the trailer block is no record, and nothing after
        # that commit carries one.
        assert [c.subject for c in uncovered(commits)] == ["body"], uncovered(commits)
        assert uncovered(commits[:3]) == commits[1:3]

        text = report(commits[:3], commits[1:3])
        assert "Rev without address" in text and "no address" in text, text


def self_test() -> None:
    self_test_rule()
    self_test_git()
    print("verify-review self-test: ok")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check the review record of a commit range."
    )
    parser.add_argument("--self-test", action="store_true")
    sub = parser.add_subparsers(dest="command")
    c = sub.add_parser("check")
    c.add_argument("--base", required=True)
    c.add_argument("--tip", required=True)
    c.add_argument("--workflow-committer")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0
    if args.command == "check":
        commits = read_range(Repo(), args.base, args.tip, args.workflow_committer)
        missing = uncovered(commits)
        if missing:
            sys.stderr.write(report(commits, missing))
            return 1
        print(
            f"review record: {len(commits)} commits in "
            f"{args.base}..{args.tip} are covered"
        )
        return 0
    parser.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
