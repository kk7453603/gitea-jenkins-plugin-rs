---
name: brief
description: Surface what watchmen detected in this workspace since the last curator run — new skills, CLAUDE.md changes, suggested actions to take this session.
allowed-tools:
  - Bash(*read_state.sh*)
---

# Watchmen brief

You are reporting watchmen's latest findings for the user's current workspace. The user invoked you because they saw a `💡 watchmen` indicator in their statusLine and want to know what changed.

## Current state for this workspace

The block below contains the JSON state file watchmen wrote at the end of its last curator run for this project. If it says `(no state)` or `(not a tracked project)`, watchmen has nothing new to report.

!`${CLAUDE_PLUGIN_ROOT}/bin/read_state.sh`

## What to do

1. **If the state says `(no state)` or `(not a tracked project)`**: tell the user there's nothing new and stop. One short sentence.

2. **Otherwise, summarize in plain English what changed**:
   - Which skills were added/updated/removed
   - Whether CLAUDE.md changed
   - When this happened (the `ts` field)
   - Keep it tight — 3-5 lines max. The user will ask for detail if they want it.

3. **If `suggested_skill` is non-null in the state**: ask the user *"Want me to load the `<skill>` skill for this session?"* and wait for a yes/no. If yes, instruct them to invoke it (e.g. `/skill-name`) — you can't load it programmatically, only signal.

4. **Include a closing link to the run's details**:
   - If `diff_url` is non-null (schema 2+): show that — it points at the side-by-side diff for this exact run, e.g. `Diff: http://127.0.0.1:8979/p/kai-agent-new/diff/<sha>`
   - Else fall back to `viewer_url` (the project overview page)
   - One line, link only — no extra commentary.

The `read_state.sh` call also touches an acknowledgment file, which clears the statusLine indicator for this project. So once you've shown the user this brief, the indicator will be gone until the next curator run.
