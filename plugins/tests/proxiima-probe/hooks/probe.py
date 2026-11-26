"""Probe hook: records every lifecycle event a CLI fires and offers the model a
codeword, so run.py can tell which events fire and which reach the model.

The hook file passes the event name; the payload on stdin is the CLI's own and
is recorded as it arrived, for later contract fixtures.
"""

import json
import sys
from pathlib import Path

RUN = Path("/tmp/proxiima-probe")
# Events whose hook output a CLI may add to the model's context.
OFFERS_CONTEXT = {"SessionStart", "UserPromptSubmit", "PostToolUse"}

event = sys.argv[1]
raw = sys.stdin.read()
try:
    payload = json.loads(raw)
except ValueError:
    payload = {"unparsed": raw}

source = payload.get("source") or payload.get("trigger") or ""
tag = f"{event}-{source}" if source else event
events = RUN / "events.jsonl"
serial = sum(1 for _ in events.open()) if events.exists() else 0
word = f"CODEWORD-{tag}-{(RUN / 'nonce').read_text().strip()}-{serial}"

with events.open("a") as log:
    log.write(json.dumps({
        "phase": (RUN / "phase").read_text().strip(),
        "tag": tag,
        "word": word if event in OFFERS_CONTEXT else None,
        "payload": payload,
    }) + "\n")

if event in OFFERS_CONTEXT:
    # The shape tesla-valve's hook proves in Claude Code and Codex.
    print(json.dumps({"hookSpecificOutput": {"hookEventName": event, "additionalContext": word}}))
