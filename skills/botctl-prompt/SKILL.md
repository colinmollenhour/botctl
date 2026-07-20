---
name: botctl-prompt
description: >-
  Run advanced agentic Claude Code invocations via `botctl prompt` (observable
  tmux TUI, YOLO-safe blockers, transcript-backed final answer). Use when you
  need a one-shot or multi-file Claude run that keeps a real interactive session
  (not `claude -p`), when launching parallel Claude participants for reviews or
  many-brain workflows, when a long source packet must be staged reliably, or
  when the user mentions botctl prompt, TUI-backed Claude, or isolated Claude
  sessions with --session-id.
allowed-tools: Bash(botctl *), Bash(tmux *)
---

# botctl-prompt

Use **`botctl prompt`** to run Claude Code as a real interactive TUI inside
tmux, wait for completion, and print **only the final assistant message** to
stdout. Prefer this over `claude -p` / `claude --print` when the task needs:

- tool use inside a full Claude Code session
- permission dialogs that botctl can auto-approve (YOLO)
- multi-file / large instruction packets
- session isolation (`--session-id`, named windows)
- observable panes for debugging failed runs

## Prerequisites

```bash
command -v botctl && command -v tmux && command -v claude
botctl --version
```

Optional: install this skill into Claude Code (or other agents) from the
embedded copy shipped in the binary:

```bash
botctl install-skill
```

Inspect the embedded skill without installing:

```bash
botctl view-skill
# or: botctl view-skill botctl-prompt
```

Install from GitHub with the skills CLI (project or global):

```bash
npx skills add colinmollenhour/botctl --skill botctl-prompt
npx skills add colinmollenhour/botctl --skill botctl-prompt -g
```

## Core pattern

```bash
botctl prompt \
  --text "Your task here. Return only the deliverable." \
  --cwd "/path/to/project" \
  --verbose \
  -- \
  --model sonnet \
  --name "botctl: short label"
```

**Contract**

| Piece | Behavior |
| --- | --- |
| stdout | Final assistant Markdown/text only (on success) |
| stderr | Launch/wait progress when `--verbose` |
| exit 0 | Fresh assistant message extracted for **this** pane/session |
| exit ≠ 0 | Failure; the prompt window is **left alive** for inspection |
| after `--` | Extra Claude CLI args (`--model`, `--name`, `--session-id`, …) |
| prompt mode | `claude -p` / `--prompt` is refused — botctl always uses the TUI |

## Model selection (Claude Code)

Pass models after `--`:

```bash
# Sonnet (default for many batch / review participants)
botctl prompt --text "..." -- --model sonnet --name "botctl: sonnet"

# Opus for hard reasoning
botctl prompt --text "..." -- --model opus --effort max --name "botctl: opus"

# Haiku for cheap triage
botctl prompt --text "..." -- --model haiku --name "botctl: haiku"
```

## Input shapes

### Short text

```bash
botctl prompt --text "Summarize the top three risks in this repo." --cwd "$PWD" -- --model sonnet
```

### File packet (`--source`, repeatable)

```bash
botctl prompt \
  --source .tmp/review/packet.md \
  --source .tmp/review/extra-constraints.md \
  --cwd "$PWD" \
  -- \
  --model sonnet \
  --name "botctl: review packet"
```

### Large packets

Default threshold is 8192 bytes. Larger combined input is written to a temp
instruction file; Claude is told to read that file. Keep the temp file for
debugging with `--keep-temp`.

```bash
botctl prompt \
  --source .tmp/huge-packet.md \
  --large-prompt-threshold 8192 \
  --keep-temp \
  --verbose \
  --cwd "$PWD" \
  -- \
  --model sonnet
```

If a direct paste fails with `tmux set-buffer failed`, lower the threshold so
botctl uses the instruction-file path (or raise only when you know tmux can
accept the buffer size).

### System-style appendices

```bash
botctl prompt \
  --append-system-prompt .tmp/rules.md \
  --text "Apply the rules and answer the question." \
  --cwd "$PWD" \
  -- \
  --model sonnet
```

### Stdin

```bash
cat .tmp/prompt.md | botctl prompt --stdin --cwd "$PWD" -- --model sonnet
```

## Session isolation (required for parallel Claude)

When multiple Claude sessions share a cwd, always pin identity:

```bash
SESSION_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"  # or any stable UUID

botctl prompt \
  --text "Reply with exactly SENTINEL_A and nothing else." \
  --cwd "$PWD" \
  --session "botctl-mbot" \
  --window "claude-a" \
  --verbose \
  -- \
  --model sonnet \
  --session-id "$SESSION_ID" \
  --name "botctl: sentinel A"
```

Rules:

1. Prefer a unique Claude `--session-id` (UUID) per concurrent participant.
2. Prefer unique `--window` names under a shared owning `--session` (default `botctl`).
3. Do **not** assume newest transcript for a cwd is correct — botctl binds via
   `~/.claude/sessions/<pid>.json`, FDs, and `--session-id`.
4. On ambiguity, botctl fails closed rather than returning another task's text.

## YOLO and permissions

By default, safe permission dialogs may be auto-approved while waiting.

```bash
# Default: allow safe YOLO approvals during the run
botctl prompt --text "..." --cwd "$PWD" -- --model sonnet

# Never auto-approve (human must handle the pane)
botctl prompt --text "..." --cwd "$PWD" --no-yolo -- --model sonnet
```

Sensitive paths (e.g. Claude settings) still require manual review.

## Timeouts and observability

| Flag | Default | Use |
| --- | --- | --- |
| `--ready-timeout-ms` | 30000 | Wait for ChatReady before submit |
| `--idle-timeout-ms` | 600000 | Wait for response / fresh last-message |
| `--poll-ms` | 1000 | Classification poll interval |
| `--submit-delay-ms` | 250 | Delay after paste before submit |
| `--verbose` | off | Progress on stderr |

Failed runs leave the tmux window up:

```bash
tmux list-windows -t botctl
botctl capture --pane '%N'
botctl last-message --pane '%N'
```

## Parallel participants (many-brain / multi-model)

Launch each Claude participant as its own `botctl prompt` (background jobs),
each with a unique `--session-id` and `--window`:

```bash
run_id="mbot-$(date +%s)"
mkdir -p ".tmp/${run_id}/results"

for role in bugs runtime craft; do
  sid="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  botctl prompt \
    --source ".tmp/${run_id}/${role}.md" \
    --cwd "$PWD" \
    --session "botctl-mbot" \
    --window "${run_id}-${role}" \
    --idle-timeout-ms 900000 \
    --verbose \
    -- \
    --model sonnet \
    --session-id "$sid" \
    --name "botctl: ${run_id} ${role}" \
    > ".tmp/${run_id}/results/${role}.out" \
    2> ".tmp/${run_id}/results/${role}.err" &
done
wait
```

Success = exit 0 **and** non-whitespace stdout. On failure, read `.err` and
inspect the surviving window.

## When **not** to use botctl prompt

- Trivial one-liner completions where `claude --print` is enough and no tools
  are needed.
- You already have a live managed pane and only need `botctl submit-prompt` /
  `botctl keep-going` against that pane.
- Non-Claude providers (use provider-native CLIs or botctl MCP/`last-message`
  for visibility only).

## Skill discovery helpers

```bash
# Print the skill shipped inside this botctl binary
botctl view-skill

# Install/update ~/.claude/skills/botctl-prompt/SKILL.md from the binary
botctl install-skill

# Optional install roots
botctl install-skill --path ~/.claude/skills
botctl install-skill --path ~/.agents/skills
```

If the skill is not installed in the current agent, run `botctl view-skill` and
follow those instructions instead of guessing flags.

## Verified examples (Sonnet)

These shapes are exercised against live Claude Sonnet via `botctl prompt`:

1. **Short text** — exact sentinel reply.
2. **`--source` file** — read a packet and return a marker.
3. **Isolated `--session-id`** — unique session under a busy shared cwd.

Re-run locally:

```bash
botctl prompt --text 'Reply with exactly: BOTCTL_PROMPT_OK' \
  --cwd /tmp -- --model sonnet --name 'botctl-prompt: smoke'

printf '%s\n' 'Return only the token: SOURCE_OK' > /tmp/botctl-prompt-source.md
botctl prompt --source /tmp/botctl-prompt-source.md \
  --cwd /tmp -- --model sonnet --name 'botctl-prompt: source'
```
