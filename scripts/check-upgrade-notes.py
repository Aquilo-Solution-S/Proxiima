#!/usr/bin/env python3
"""Check candidate Upgrade footers and exercise release-note rendering."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def cliff(*arguments: str, candidate: bool = False) -> str:
    env = os.environ.copy()
    if candidate:
        env["GIT_CLIFF__GIT__PROTECT_BREAKING_COMMITS"] = "true"
        env["GIT_CLIFF__GIT__FILTER_COMMITS"] = "false"
    return subprocess.run(
        ["git", "cliff", "--offline", "--config", str(ROOT / "cliff.toml"), *arguments],
        cwd=ROOT, env=env, text=True, capture_output=True, check=True,
    ).stdout


# `Proxiima.Repository.renderUpgrade`.
def validate(releases: list[dict]) -> list[str]:
    failures = []
    for release in releases:
        for commit in release["commits"]:
            if not commit["breaking"]:
                continue
            notes = [item["value"] for item in commit["footers"] if item["token"] == "Upgrade"]
            if len(notes) != 1 or not notes[0].strip():
                failures.append(f'{commit["id"][:7]} {commit["message"]}: breaking commit requires one nonempty Upgrade: footer')
    return failures


def self_test() -> None:
    # Custom messages enter the real parser and Tera template without changing
    # Git history. Unique markers distinguish fixture text from repository notes.
    messages = [
        "feat(core)!: fixture break\n\nUpgrade: upgrade-fixture-retire-resource.\n continuation-line-kept.",
        "feat(core): fixture addition\n\nUpgrade: upgrade-fixture-nonbreaking-hidden.",
        "fix(core): fixture fix",
    ]
    args = ["--unreleased"]
    for message in messages:
        args += ["--with-commit", message]
    rendered = cliff(*args)
    assert rendered.count("upgrade-fixture-retire-resource.") == 1, rendered
    assert "continuation-line-kept." in rendered, rendered
    assert "upgrade-fixture-nonbreaking-hidden." not in rendered, rendered
    assert "### Upgrade" in rendered, rendered
    parsed = json.loads(cliff(*args, "--context"))
    fixtures = [{"commits": [commit for release in parsed for commit in release["commits"]
                              if commit["message"].startswith("fixture")]}]
    assert not validate(fixtures)
    missing = json.loads(cliff("--unreleased", "--context", "--with-commit", "feat(core)!: fixture missing"))
    bad = [commit for release in missing for commit in release["commits"]
           if commit["message"] == "fixture missing"]
    assert len(validate([{"commits": bad}])) == 1
    duplicated = json.loads(cliff("--unreleased", "--context", "--with-commit",
                                 "feat(core)!: fixture duplicate\n\nUpgrade: first\nUpgrade: second"))
    bad = [commit for release in duplicated for commit in release["commits"]
           if commit["message"] == "fixture duplicate"]
    assert len(validate([{"commits": bad}])) == 1
    hidden = json.loads(cliff("--unreleased", "--context", "--with-commit",
                              "chore(ci)!: fixture hidden breaking", candidate=True))
    bad = [commit for release in hidden for commit in release["commits"]
           if commit["message"] == "fixture hidden breaking"]
    assert len(validate([{"commits": bad}])) == 1
    # GitHub renders release notes as GFM, which drops `<word>` placeholders as
    # raw HTML. Outside code spans `<` is escaped; inside them it stays literal.
    placeholder = cliff("--unreleased", "--with-commit",
                        'feat(core)!: fixture placeholder\n\nUpgrade: write "<crate-name>/<short>",'
                        " keep `Vec<u8>` and `-> user`; a <tag> and 2 > 1.")
    assert '- write "&lt;crate-name>/&lt;short>", keep `Vec<u8>` and `-> user`; a &lt;tag> and 2 > 1.\n' \
        in placeholder, placeholder
    unmatched = cliff("--unreleased", "--with-commit",
                      "feat(core)!: fixture unmatched\n\nUpgrade: run `a <b> and <c>.")
    assert "- run `a &lt;b> and &lt;c>.\n" in unmatched, unmatched
    print("upgrade notes: parser, breaking-only rendering, continuation, missing, duplicate and placeholder footer fixtures passed")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--range", dest="commit_range", metavar="BASE..TIP")
    args = parser.parse_args()
    try:
        if args.self_test:
            self_test()
        else:
            failures = validate(json.loads(cliff("--context", "--", args.commit_range, candidate=True)))
            if failures:
                print("\n".join(failures), file=sys.stderr)
                return 1
        return 0
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"upgrade notes check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
