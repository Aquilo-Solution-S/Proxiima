#!/usr/bin/env python3
"""Validate the workspace version against the tags already published.

The version is `[workspace.package].version` in the root `Cargo.toml`, shared by
every member and exposed as `proxiima_core::RELEASE_VERSION`. The tag IS the
release. `release.yml` cuts `v${version}` when a merge to `main` carries a value
no tag holds yet, and cuts nothing when the value is unchanged, so this one
number is the release trigger. That makes it worth checking on every PR rather
than remembering at tag time, because the mistakes it catches are only visible
once the tag exists and the notes are written.

Two shapes pass:

  unchanged   the workspace version already has a tag; merging cuts no release.
  bumped      the workspace version is the next patch, minor or major of the
              highest tag; merging cuts exactly that tag.

Everything else fails: a version that skips a number, goes backwards, or
reuses a tag whose notes are already published and whose commit is already
named by a consumer's `Cargo.toml`.

Every workspace member must inherit the version (`version.workspace = true`),
so a build cannot report two. `--current` prints the workspace version alone,
for the workflows.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from collections.abc import Callable
from pathlib import Path

MANIFEST = Path("Cargo.toml")
SEMVER = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def read_workspace_version(manifest: dict) -> str:
    try:
        version = manifest["workspace"]["package"]["version"]
    except KeyError:
        version = None
    if not isinstance(version, str):
        raise ValueError(
            f"{MANIFEST} declares no `[workspace.package] version`; "
            "release.yml derives the tag from it and cannot run without it"
        )
    return version


def uninherited_members(manifest: dict, read_member: Callable[[str], dict]) -> list[str]:
    """Workspace members whose `[package].version` is not `version.workspace = true`."""
    return [
        member
        for member in manifest["workspace"]["members"]
        if read_member(member).get("package", {}).get("version") != {"workspace": True}
    ]


def load(path: Path) -> dict:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def parse(version: str) -> tuple[int, int, int] | None:
    found = SEMVER.match(version)
    if not found:
        return None
    return (int(found.group(1)), int(found.group(2)), int(found.group(3)))


def successors(latest: tuple[int, int, int]) -> list[tuple[int, int, int]]:
    """The three versions that may follow `latest`.

    Deliberately not "anything greater". A typo that lands 0.0.51 where 0.0.15
    was meant is still monotonic, and the number it skipped can never be used
    afterwards without going backwards.
    """
    major, minor, patch = latest
    return [(major, minor, patch + 1), (major, minor + 1, 0), (major + 1, 0, 0)]


def show(version: tuple[int, int, int]) -> str:
    return ".".join(str(part) for part in version)


def validate(current: str, tags: list[str]) -> tuple[bool, str]:
    parsed = parse(current)
    if parsed is None:
        return False, f"workspace version {current!r} is not a MAJOR.MINOR.PATCH version"

    released = sorted(filter(None, (parse(tag.removeprefix("v")) for tag in tags)))
    if not released:
        return True, f"no released tag yet; merging cuts v{current}"

    latest = released[-1]
    if parsed == latest:
        return True, f"workspace version is v{current}, already released; merging cuts no tag"

    if parsed in released:
        return False, (
            f"v{current} is already released. A tag names one commit forever — "
            f"its notes are written and consumers pin it. Next is v{show(successors(latest)[0])}"
        )

    allowed = successors(latest)
    if parsed not in allowed:
        return False, (
            f"v{current} does not follow the latest release v{show(latest)}. "
            f"Expected one of: {', '.join('v' + show(nxt) for nxt in allowed)}"
        )

    return True, f"workspace version bumped v{show(latest)} -> v{current}; merging cuts that tag"


def git_tags() -> list[str]:
    result = subprocess.run(
        ["git", "tag", "--list", "v*"],
        capture_output=True,
        text=True,
        check=True,
    )
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def self_test() -> int:
    tags = ["v0.0.13", "v0.0.14", "not-a-version"]
    cases: list[tuple[str, list[str], bool, str]] = [
        ("0.0.14", tags, True, "unchanged from the latest release"),
        ("0.0.15", tags, True, "next patch"),
        ("0.1.0", tags, True, "next minor"),
        ("1.0.0", tags, True, "next major"),
        ("0.0.13", tags, False, "an earlier release, already tagged"),
        ("0.0.16", tags, False, "skips a patch"),
        ("0.2.0", tags, False, "skips a minor"),
        ("0.0.14-rc1", tags, False, "not MAJOR.MINOR.PATCH"),
        ("0.0.1", [], True, "first release, no tags yet"),
    ]
    failures = 0
    members = {
        "a": {"package": {"version": {"workspace": True}}},
        "b": {"package": {"version": "0.1.0"}},
        "c": {"package": {}},
    }
    refused = uninherited_members({"workspace": {"members": list(members)}}, members.__getitem__)
    if refused != ["b", "c"]:
        failures += 1
        print(f"self-test FAILED (member version inheritance): refused {refused}", file=sys.stderr)
    for current, available, expected, why in cases:
        ok, message = validate(current, available)
        if ok != expected:
            failures += 1
            print(f"self-test FAILED ({why}): {current} -> {ok}: {message}", file=sys.stderr)
    if failures:
        print(f"{failures} self-test case(s) failed", file=sys.stderr)
        return 1
    print(f"release-version self-test: {len(cases) + 1} cases passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", help="check this script's own rules")
    parser.add_argument("--current", action="store_true", help="print the workspace version only")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    try:
        manifest = load(MANIFEST)
        current = read_workspace_version(manifest)
        if args.current:
            print(current)
            return 0
        refused = uninherited_members(manifest, lambda member: load(Path(member) / "Cargo.toml"))
    except (OSError, ValueError, KeyError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    if refused:
        print(
            f"error: members without `version.workspace = true`: {', '.join(refused)}; "
            f"a build must report the one version in {MANIFEST}",
            file=sys.stderr,
        )
        return 1
    ok, message = validate(current, git_tags())
    if not ok:
        print(f"error: {message}", file=sys.stderr)
        return 1
    print(message)
    return 0


if __name__ == "__main__":
    sys.exit(main())
