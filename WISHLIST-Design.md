# botctl CLI Design Wishlist

This is a CLI-first wishlist for making `botctl` feel safer, clearer, and more beautiful to operate. It focuses on command shape, help, output, errors, scripting contracts, and operator workflow rather than core classifier or tmux internals.

## Checklist

1. [x] P0-1 Support `botctl --version`.
2. [x] P0-2 Make `-h` and `--help` work for every command and subcommand.
3. [x] P0-3 Stop printing ANSI color when output is not a TTY, `NO_COLOR` is set, or `TERM=dumb`.
4. [ ] P0-4 Split output streams: data/results to `stdout`; errors, warnings, progress, prompts, and long-running status to `stderr`.
5. [x] P0-5 Replace full global help dumps on parse errors with concise, command-specific errors plus one next command.
6. [ ] P0-6 Add stable `--json` output for inspection and one-shot state-changing commands.
7. [x] P0-7 Add per-command help pages for `dashboard`, `yolo`, `serve`, `status`, `doctor`, target syntax, and safety rules.
8. [ ] P1-1 Move to a mature parser such as `clap` before adding more command surface.
9. [ ] P1-2 Normalize naming around `yolo`, `approve`, `reject`, `dismiss-survey`, and long aliases.
10. [ ] P1-3 Create a reusable target grammar and document it everywhere as one concept.
11. [ ] P1-4 Add `--no-input`, `--quiet`, `--verbose`, `--debug`, and `--no-color` where they matter.
12. [ ] P1-5 Add interactive target selection only when stdin is a TTY and no explicit target is provided.
13. [ ] P1-6 Make dangerous or surprising automation choices show exactly what will happen before acting.
14. [ ] P1-7 Keep human output beautiful, but make machine output boring and stable.
15. [ ] P2-1 Reorganize commands by operator workflow, not implementation layer.
16. [x] P2-2 Add `botctl help <topic>` for concepts: `targeting`, `safety`, `json`, `state-dir`, `dashboard-keys`, and `opencode`.
17. [ ] P2-3 Add shell completions and completions docs.
18. [ ] P2-4 Add command examples to help output, not only docs.
19. [ ] P2-5 Add `--plain` for line-oriented output where rich text may evolve.
20. [ ] P2-6 Add structured exit-code conventions and document them.
21. [ ] P3-1 Consider a larger command taxonomy redesign after contracts stabilize.
22. [ ] P3-2 Consider aliases for common human flows, but keep canonical commands explicit.
23. [ ] P3-3 Add terminal-accessible docs generated from the same source as web docs.
24. [ ] P3-4 Add UX snapshot tests for help, errors, JSON, and non-TTY behavior.

## What Already Works

`botctl` has strong bones.

- Command purpose is clear: observe and safely drive Claude/OpenCode sessions in tmux.
- Safety model is visible in docs and code: explicit pane targeting, ownership checks, classified states, guarded workflows.
- Output from many commands uses stable `key=value` lines, which is script-friendly-ish.
- Long-lived commands already think about SIGINT and cleanup.
- `serve --format jsonl` is a good machine-readable stream shape.
- Runtime state now follows XDG by default, with `--state-dir` override.
- Docs are broad and current enough to guide real use.

## P0: Correctness And Trust

### P0-1 Support `botctl --version`

`--version` is a CLI contract. Today it fails as an unknown subcommand.

Recommended behavior:

```text
botctl 0.4.0
```

Why: scripts, bug reports, support, package verification, and human trust all expect this.

### P0-2 Make Help Work Everywhere

Today `botctl --help` works, but `botctl status --help` fails with `unknown status flag: --help` and then prints full global help. This violates user muscle memory.

Recommended behavior:

- `botctl -h` and `botctl --help` show global help and exit `0`.
- `botctl help` shows global help and exit `0`.
- `botctl help status`, `botctl status -h`, and `botctl status --help` show only `status` help and exit `0`.
- Help flags should win no matter where they appear.

Per-command help should include:

- one-line purpose
- usage
- examples
- options with defaults
- targeting rules
- output format notes
- safety notes when command sends keys

### P0-3 Respect Color Rules

Global help always injects ANSI escape codes. That makes redirected help, docs generation, tests, and non-TTY output ugly.

Recommended behavior:

- color only when output stream is a TTY
- no color when `NO_COLOR` is set and non-empty
- no color when `TERM=dumb`
- support `--no-color`
- keep `FORCE_COLOR` optional and conservative

Color should enhance hierarchy, not become part of data.

### P0-4 Fix Output Stream Boundaries

Current `main` prints all successful command output to `stdout`; long-running commands call `emit_babysit_output`; warnings go to `stderr`; parse errors print full help to `stderr`. Direction is decent but not consistently contractual.

Recommended stream rules:

- final command result and machine data: `stdout`
- warnings, progress, prompts, diagnostics, status while waiting: `stderr`
- `--json` and `--plain`: always `stdout`
- TUI: owns terminal directly, no extra stdout chatter
- long-lived `serve --format jsonl`: event stream on `stdout`, warnings on `stderr`

Why: users pipe `stdout`; humans read `stderr` status.

### P0-5 Stop Dumping Global Help On Normal Errors

`botctl status` currently prints `missing required flag: --pane`, then full global help. That is noisy and hides the fix.

Recommended error shape:

```text
Missing required option: --pane

Usage:
  botctl status --pane %ID|session:window.pane [--history-lines N]

Try `botctl status --help` for examples.
```

For unknown subcommands:

```text
Unknown command: stats

Did you mean `status`?
Try `botctl help` to list commands.
```

Exit codes:

- help success: `0`
- usage/parse errors: `2`
- runtime failure: `1`
- guarded refusal / unsafe state: document as either `1` or `3`, then keep stable

### P0-6 Add Stable `--json` For One-Shot Commands

`serve` and `yolo` have `--format human|jsonl`, but `status`, `doctor`, `list`, `attach`, `start --dry-run`, guarded actions, `continue-session`, `auto-unstick`, `prepare-prompt`, and `submit-prompt` are still human/key-value only.

Recommended next JSON targets:

- `status --json`
- `doctor --json`
- `list --json`
- `attach --json`
- `continue-session --json`
- `auto-unstick --json`
- `approve --json` and `reject --json` as aliases or replacement for `--format jsonl` on one-shot commands

Use JSON objects for one-shot commands. Reserve JSONL for streams.

Example:

```json
{
  "pane_id": "%19",
  "session": "demo",
  "window": "claude",
  "command": "claude",
  "state": "ChatReady",
  "automation_ready": true,
  "next_safe_action": "submit-prompt"
}
```

### P0-7 Add Real Per-Command Help

Global help is attractive but overcrowded. It lists many commands without enough guidance.

First per-command pages to create:

- `botctl dashboard --help`
- `botctl yolo --help`
- `botctl yolo stop --help`
- `botctl serve --help`
- `botctl status --help`
- `botctl doctor --help`
- `botctl approve --help`
- `botctl keep-going --help`

Each should have copy-paste examples. Examples matter more than exhaustive option prose.

## P1: Operator Ergonomics

### P1-1 Adopt `clap`

Hand-rolled parsing was fine early. It is now blocking conventional behavior: subcommand help, version, suggestions, completions, default values in help, value enums, and shell completions.

Recommended approach:

- migrate parser only, keeping app structs and behavior stable
- use `clap` derive or builder, whichever keeps code smaller
- preserve aliases and current command names during migration
- add snapshot tests for help/error output before changing wording heavily

This is not about dependency fashion. It buys CLI correctness.

### P1-2 Normalize Command Names And Aliases

Current names mix polished and internal terms:

- `approve` and `approve-permission`
- `reject` and `reject-permission`
- `dismiss-survey`
- `continue-session`
- `auto-unstick`
- `send-action`
- `record-fixture`
- `editor-helper`

Recommendation:

- keep existing names for compatibility
- document one canonical name per workflow
- mark internal/plumbing commands as advanced in help
- consider a future `action` or `pane` namespace only if surface keeps growing

Suggested canonical human names:

- `approve` over `approve-permission`
- `reject` over `reject-permission`
- `recover` or keep `auto-unstick`, but explain it clearly
- `prompt prepare`, `prompt submit`, `prompt helper` only if doing larger taxonomy later

### P1-3 Treat Targeting As One First-Class Grammar

Targeting is the heart of this CLI. It currently appears as repeated text in help and docs.

Recommended target grammar docs:

```text
Targets:
  --pane %19              tmux pane id, safest
  --pane 0:2.3            explicit tmux pane target
  --session demo --window claude
                           named tmux session/window, accepted only where unambiguous
```

Add `botctl help targeting` and include same short block in every target-accepting command help.

### P1-4 Add Common Control Flags

Recommended flags:

- `--no-input`: never prompt, never open editor, fail instead
- `--quiet`: only final result or nothing on success
- `--verbose`: more human diagnostics
- `--debug`: internal diagnostics, stack/source details where useful
- `--no-color`: disable color/rich styling

Do not add all flags to all commands blindly. Add where behavior exists.

### P1-5 Interactive Target Selection

When run from an interactive TTY, commands like `status`, `doctor`, `approve`, `reject`, `continue-session`, `auto-unstick`, and `yolo --pane` could offer target selection if no target is supplied.

Rules:

- only prompt when stdin is TTY
- never prompt when `--no-input`
- in non-interactive mode, fail with exact target flags needed
- show pane id, session, window, cwd, state, and command
- require explicit confirmation before sending keys

This would make human use beautiful without hurting scripts.

### P1-6 Preview Before Automation

For state-changing keypress commands, show the planned action before sending keys when interactive and not already explicit enough.

Example:

```text
Pane: %19 demo:claude cwd=/repo
State: PermissionDialog
Action: approve permission using Claude binding `ctrl+y`

Approve? [y/N]
```

For scripts:

- `--yes` or `--confirm` skips prompt
- no prompt when command is already a narrowly named one-shot and target/state are explicit, if current behavior must stay fast

Be careful: too many prompts ruin automation. Use this for dangerous or ambiguous flows, not every safe no-op.

### P1-7 Better Human Output, Stable Machine Output

Human output can be prettier:

```text
Pane %19 is ready
Session: demo / claude
Path:    /home/colin/project
State:   ChatReady

Next: botctl submit-prompt --pane %19 --text "..."
```

Machine output should stay boring:

```text
pane_id=%19
session=demo
window=claude
state=ChatReady
automation_ready=true
```

Recommendation:

- default human output for terminal users
- `--plain` for key-value/line output if scripts already depend on it
- `--json` for new scripts

## P2: Information Architecture

### P2-1 Group Commands By User Intent

Current help groups are okay, but still partly implementation-shaped.

Recommended top-level help groups:

- Daily use: `dashboard`, `yolo`, `serve`
- Inspect: `list`, `status`, `doctor`, `capture`
- Recover: `approve`, `reject`, `dismiss-survey`, `continue-session`, `auto-unstick`
- Prompt: `prepare-prompt`, `submit-prompt`, `keep-going`, `editor-helper`
- Setup: `start`, `attach`, `bindings`, `install-bindings`
- Advanced diagnostics: `observe`, `record-fixture`, `classify`, `replay`, `send-action`

This matches how operators think.

### P2-2 Add Help Topics

Useful topics:

- `botctl help targeting`
- `botctl help safety`
- `botctl help json`
- `botctl help state-dir`
- `botctl help dashboard-keys`
- `botctl help opencode`

These can be short. Terminal docs should answer the thing users forgot without opening browser.

### P2-3 Add Shell Completions

Completions matter for command discovery and flag correctness.

Recommended:

- `botctl completions bash|zsh|fish`
- docs for installing completions
- dynamic pane/session completion later if safe and fast

### P2-4 Put Examples In Help

Docs have examples. Help should too.

Global help should keep 3-5 top examples:

```text
Examples:
  botctl dashboard
  botctl yolo --pane %19
  botctl status --pane 0:2.3
  botctl serve --session demo --format jsonl
```

Per-command help should have command-specific examples.

### P2-5 Add `--plain`

If current `key=value` output becomes prettier by default, preserve line-oriented scripting with `--plain`.

Good candidates:

- `list --plain`
- `status --plain`
- `doctor --plain`
- `attach --plain`

### P2-6 Document Exit Codes

Exit codes are part of CLI API.

Recommended contract:

- `0`: success
- `1`: runtime failure
- `2`: usage error or unsafe/non-actionable guarded refusal
- `130`: interrupted by Ctrl-C, if not already handled as graceful success

If guarded refusal deserves a separate code, choose it now and document it before scripts depend on current behavior.

## P3: Bigger Redesign Ideas

### P3-1 Future Command Taxonomy

Only do this after parser/help/JSON contracts stabilize.

Possible future shape:

```text
botctl dashboard
botctl pane list
botctl pane status --pane %19
botctl pane capture --pane %19
botctl recover approve --pane %19
botctl recover auto --pane %19
botctl prompt prepare --session demo --text ...
botctl prompt submit --pane %19 --text ...
botctl yolo start --pane %19
botctl yolo stop --pane %19
botctl fixtures record --session demo --case ready
```

This is cleaner long-term, but it is a breaking mental-model change. Keep old commands as aliases if pursued.

### P3-2 Human-Friendly Aliases

Possible aliases after canonical behavior is stable:

- `botctl ready --pane %19` for `status` focused on next safe action
- `botctl recover --pane %19` for `auto-unstick`
- `botctl ask --pane %19 --text "..."` for `submit-prompt`
- `botctl watch --session demo` for `serve --session demo`

Aliases are nice, but every alias becomes support surface. Add only for common flows.

### P3-3 Generate Terminal Docs From Web Docs

Docs already exist under `docs/docs`. Eventually generate `botctl help topic` from shared source or a small embedded copy to avoid drift.

### P3-4 UX Snapshot Tests

Add tests that lock down:

- global help
- every per-command help page
- parse error shape
- `--version`
- color disabled under non-TTY / `NO_COLOR`
- JSON schema for key commands
- stdout/stderr separation for representative success/error paths

This lets design improve without accidental breakage.

## Suggested Implementation Order

1. Add `--version`, global color gating, and subcommand help handling.
2. Add concise command-specific parse errors.
3. Add `status --json`, `doctor --json`, and `list --json`.
4. Migrate parser to `clap` behind same public command names.
5. Add per-command help content and help topics.
6. Add `--no-input`, `--quiet`, `--verbose`, `--debug`, and `--no-color` where behavior exists.
7. Add interactive target selection.
8. Revisit command taxonomy only after compatibility and JSON contracts are solid.

## Design North Star

`botctl` should feel like a careful tmux co-pilot:

- calm when observing
- explicit before driving a pane
- terse when scripted
- helpful when blocked
- beautiful for humans without leaking beauty into pipes
- conservative by default, fast when user is explicit
