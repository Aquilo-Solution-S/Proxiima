"""Reminds the agent to consolidate into Proxiima after its context was compacted.

PostCompact marks the session compacted and clears its claim; the first
PostToolUse after that claims the reminder and hands it to the model. PostToolUse
is the one event whose output reaches the model in Claude Code, Codex and Grok
alike; Grok gives SessionStart no way to add context. The handles the
compaction dropped stay on the server, where the skill finds them again.

Claude Code, Codex and Grok all send `session_id` and set CLAUDE_PLUGIN_DATA.
"""

import json
import os
import re
import sys
from pathlib import Path

REMINDER = ("Your context was compacted. When your current step is done, run the `consolidate` "
            "skill, so that what this session established before and after the compaction is "
            "stored in Proxiima.")
SESSION = re.compile(r"[A-Za-z0-9_-]{1,128}")


def main() -> None:
    event = sys.argv[1]
    session = json.loads(sys.stdin.read()).get("session_id")
    if not isinstance(session, str) or not SESSION.fullmatch(session):
        sys.exit(f"proxiima-compaction: no usable session_id in the {event} payload")
    data = Path(os.environ["CLAUDE_PLUGIN_DATA"])
    compacted, claimed = data / "compacted" / session, data / "claimed" / session
    if event == "PostCompact":
        for folder in (compacted.parent, claimed.parent):
            folder.mkdir(parents=True, exist_ok=True)
        claimed.unlink(missing_ok=True)
        compacted.touch()
        return
    if not compacted.exists():
        return
    # Of concurrent tool hooks, the one whose exclusive create succeeds reminds. Removing a
    # shared marker is no such claim: on macOS, concurrent unlinks of one file can each succeed.
    try:
        os.close(os.open(claimed, os.O_CREAT | os.O_EXCL | os.O_WRONLY))
    except FileExistsError:
        return
    print(json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": REMINDER}}))


if __name__ == "__main__":
    main()
