# botctl

`botctl` is a Rust CLI for keeping Claude Code, Codex CLI, OpenCode, Pi, and Antigravity sessions visible and controlled inside `tmux`.

It can launch and safely drive Claude Code panes, classify Codex CLI panes from tmux screen captures with narrow YOLO approval for command permission dialogs, passively discover OpenCode panes, discover Pi panes from `~/.pi/agent/sessions` for dashboard visibility, recent-message context, state classification, and tmux window status, and passively discover Antigravity (`agy`) panes for dashboard visibility, state classification, and pane-scrape last-message extraction.

It can also dump the latest persisted assistant message from a Claude, Codex, OpenCode, Pi, or Antigravity pane to a Markdown file with `last-message`.

The project is built around a simple rule: terminal automation is only safe when tmux transport, live observation, classification, and action policy stay separate. Sending keys alone is not enough.

## Why the 🧌 emoji for Codex?

![A totally unrelated discussion about choosing the Codex dashboard emoji](https://snips.t3.tigrisfiles.io/colin/clipboard-2026-05-08T18-18-14.webp)

## Install

The recommended install method is Cargo:

```bash
cargo install botctl
```

This pulls the latest release from [crates.io](https://crates.io/crates/botctl) and places the `botctl` binary on your `PATH` (typically `~/.cargo/bin`).

To build from source instead:

```bash
git clone https://github.com/colinmollenhour/botctl
cd botctl
cargo install --path .
```

See [Requirements](#requirements) for the runtime dependencies (`tmux`, plus `claude` for Claude automation, `codex` for Codex CLI visibility and permission approval, `opencode` for OpenCode dashboard visibility, and `agy` for Antigravity dashboard visibility).

## Main Commands

These are the commands that matter most in day-to-day use:

- `runtime` to start or stop the single local coordinator that owns live observation and automation
- `dashboard` to see Claude Code panes, runtime-discovered panes, and supported Codex/OpenCode/Pi/Antigravity visibility, grouped by workspace, with state, PID, CPU, memory, observed active cook time, and YOLO controls for Claude and Codex
- `prompt` to run a one-shot prompt through a new interactive Claude TUI window in tmux and print only the final assistant text to stdout
- `last-message` to export the full latest assistant text from a pane transcript to Markdown
- `yolo` to set central YOLO policy for one pane or a scoped set of panes
- `serve` to expose HTTP and event output as a facade over runtime state
- `mcp` to expose persistent Claude Code sessions as a small MCP JSON-RPC tool API over stdio or minimal `POST /mcp` HTTP

For recovery actions, use the canonical names `approve`, `reject`, and `dismiss-survey`. The long names `approve-permission` and `reject-permission` remain compatibility aliases.

Everything else is mostly setup, diagnostics, recovery, or lower-level plumbing around those flows.

## Current Features

- launch a managed Claude Code session in tmux
- run a central local runtime over a Unix socket at `<state-dir>/runtime.sock`
- classify Codex CLI panes from captured terminal screens and approve command permission dialogs with YOLO
- passively discover OpenCode panes by matching their tmux title and cwd against OpenCode's SQLite session database, Pi panes by matching `pi` tmux commands to JSONL sessions under `~/.pi/agent/sessions`, and Antigravity panes by process-name plus state-dir detection
- list panes and inspect tmux metadata, including each tracked pane PID
- capture pane contents and classify the current UI state
- run `status` and `doctor` against a live Claude Code or Codex CLI pane
- dump the latest persisted assistant message from Claude, Codex, OpenCode, Pi, or Antigravity to `MESSAGE_<conversation-id>.md` or a path passed with `--out`
- run `serve` as a runtime-backed foreground facade for one tmux session
- run `dashboard` as a runtime-backed popup-sized TUI grouped by workspace with per-pane YOLO controls for Claude and Codex
- record and replay fixture cases for classifier regression tests
- prepare prompts and hand them off through an external-editor workflow
- run one-shot TUI-backed prompts with `prompt`, including file/stdin input and large-prompt temp instruction files
- run guarded higher-level actions such as prompt submission, permission approval, permission rejection, and survey dismissal
- run MCP tools for persistent tmux-backed agent sessions (`spawn`, `prompt`, `wait`, `kill`, `snapshot`, `send_keys`); `spawn` accepts optional `provider` (`claude` default, `codex`, `agy`), `model`, `effort`, and `agent` — validated per provider

## Docs

- published site: `https://botctl.readthedocs.io/en/latest/`
- docs app source: [`docs/`](docs/)
- intro: [`docs/docs/intro.mdx`](docs/docs/intro.mdx)
- getting started: [`docs/docs/getting-started.mdx`](docs/docs/getting-started.mdx)
- workflows: [`docs/docs/workflows.mdx`](docs/docs/workflows.mdx)
- architecture: [`docs/docs/architecture.mdx`](docs/docs/architecture.mdx)
- serve mode: [`docs/docs/serve-mode.mdx`](docs/docs/serve-mode.mdx)
- MCP sessions: [`docs/docs/mcp.mdx`](docs/docs/mcp.mdx)

## Requirements

- Rust and Cargo
- `tmux`
- `claude` available on `PATH` for Claude Code automation
- `codex` panes for Codex CLI screen classification and command permission approval
- `opencode` panes with `OC | <session title>` pane titles for passive OpenCode dashboard visibility
- `agy` available on `PATH` (optional) for Antigravity dashboard visibility; the state directory defaults to `~/.gemini/antigravity-cli` and can be overridden with `ANTIGRAVITY_STATE_DIR`; the history file can be overridden independently with `ANTIGRAVITY_HISTORY_FILE`

## Test

Run the full test suite from the repo root:

```bash
cargo test
```

Run a single test by name:

```bash
cargo test resolves_custom_binding_keys_for_actions
```

## Start here

For a first useful run, open a tmux pane that is running Claude Code, Codex CLI, OpenCode, Pi, or Antigravity, then run:

```bash
botctl runtime
botctl dashboard
```

By default, `dashboard`, `yolo`, and `serve` run in managed mode. If no runtime is available, they auto-start one in a hidden tmux session and connect to it. The runtime stays alive while managed clients still need it, and you can manage it directly with:

```bash
cargo run -- runtime
cargo run -- runtime stop
cargo run -- runtime --foreground
```

Use `--unmanaged` on `dashboard`, `yolo`, or `serve` when you want them to require an already-running runtime instead of auto-starting one.

From there:

- Use `yolo` for one Claude Code or Codex pane that is blocked on a supported permission dialog.
- Use `serve` when you need a foreground event stream or localhost HTTP API.
- Use `last-message` when you need the full latest assistant reply as Markdown.
- Use `prompt` when you want `botctl` to launch Claude, submit one prompt, wait for the reply, and print only assistant text.

```bash
botctl prompt --text "Summarize this repo"
cat prompt.md | botctl prompt --stdin --cwd /path/to/project
botctl yolo --pane 0:6.0
botctl serve --session demo --format jsonl
botctl last-message --pane 0:6.0 --out -
```

Use `cargo run -- ...` instead of `botctl ...` when running from a source checkout without installing the binary.

## Core workflows

Pane-targeted commands accept either a raw tmux pane id like `%19` or an explicit tmux pane target like `0:2.3`.

Open the live dashboard across Claude Code, Codex, OpenCode, Pi, and Antigravity panes:

```bash
cargo run -- dashboard
```

Keep the dashboard alive in a dedicated tmux-backed popup session:

```bash
cargo run -- dashboard --persistent
```

Quick tmux popup binding:

```tmux
bind-key C-c display-popup -E -w 80% -h 40% botctl dashboard --persistent
```

Set YOLO policy for one pane:

```bash
cargo run -- yolo --pane 0:6.0
```

Tail runtime-backed YOLO events for one pane:

```bash
cargo run -- yolo --pane 0:6.0 --follow
```

Run the runtime facade for one tmux session:

```bash
cargo run -- serve --session demo
```

Dump the latest assistant message from a pane transcript:

```bash
cargo run -- last-message --pane 0:4.1
cargo run -- last-message --pane 0:4.1 --out last-agent-message.md
cargo run -- last-message --pane 0:4.1 --out -
```

Run a one-shot prompt through an interactive Claude TUI in a new tmux window:

```bash
cargo run -- prompt --text "Say exactly hello"
cargo run -- prompt --source task.md --append-system-prompt rules.md
printf 'Summarize this input' | cargo run -- prompt --stdin
cargo run -- prompt --text "Say hi" -- --model sonnet --name "Just testing"
```

`prompt` does not use `claude -p` or `--prompt`; it creates a new window in the owning tmux session, creates that session first when needed, waits for `ChatReady`, pastes the prompt through tmux into the interactive TUI, waits for a fresh final assistant message, kills only that captured prompt window on success, and prints assistant text only on stdout. The owning session defaults to `botctl`; pass `--session NAME` to override it. Failed prompt windows stay alive for inspection. Pass `--verbose` to send launch/wait progress to stderr. Arguments after `--` are passed through to the interactive Claude command.

Run the observer and a localhost HTTP API for a web UI:

```bash
cargo run -- serve --session demo --http 127.0.0.1:8787 --allowed-origin http://localhost:3000
```

Use machine-readable runtime-backed output for tooling:

```bash
cargo run -- serve --session demo --format jsonl
```

The HTTP API exposes runtime-backed pane state plus interactive controls such as visible prompt options. Useful endpoints include:

- `GET /instances`
- `GET /instances/%251`
- `POST /instances/%251/prompt`
- `POST /instances/%251/actions/approve-permission`
- `POST /instances/%251/actions/continue-session`
- `POST /instances/%251/actions/auto-unstick`
- `POST /instances/%251/interactions/2`

## Session Setup And Inspection

Start a managed Claude session in the current directory:

```bash
cargo run -- start --session demo
```

See what pane was created:

```bash
cargo run -- list
cargo run -- list --plain
```

Inspect a managed session:

```bash
cargo run -- doctor --session demo
cargo run -- doctor --session demo --plain
```

Capture a pane directly:

```bash
cargo run -- capture --pane %19 --history-lines 120
```

Check the live classified state for a pane:

```bash
cargo run -- status --pane %19
cargo run -- status --pane %19 --plain
```

`--plain` preserves the current line-oriented output for `attach`, `list`, `status`, and `doctor` if richer human output is added later. `list`, `status`, and `doctor` also support `--json`; `--json` and `--plain` are mutually exclusive.

The same command using tmux pane syntax:

```bash
cargo run -- status --pane 0:2.3
```

The dashboard groups runtime-observed panes plus supported Codex/OpenCode/Pi/Antigravity visibility by workspace, shows the shared runtime state plus current pane PID, process-tree average CPU, memory, and observed active `Cook` time, lets you jump directly to a pane with `Enter`, and can toggle YOLO for Claude Code and Codex panes per pane, per workspace, or globally while it is open. `Cook` counts only observed busy agent work, pauses on idle and permission waits, resets when the agent session changes, and may be off by roughly one dashboard poll around state transitions. While it runs, it also prefixes tmux window names with per-pane status emojis in pane-index order.

The dashboard also passively includes OpenCode panes when they can be resolved without using an OpenCode API server. A pane is included only when its tmux command is `opencode`, its pane title is `OC | <session title>`, and exactly one row in OpenCode's SQLite database matches both the pane cwd and stripped title. If OpenCode truncates the pane title with `...`, botctl accepts that title as a prefix only when it is still unique within the same cwd. Missing, ambiguous, duplicate, or unreadable matches are ignored. For resolved panes, the details panel shows a bounded excerpt from recent OpenCode `message`/`part` rows. OpenCode and Pi support is dashboard/window-title visibility only; YOLO, prompt submission, and guarded keypress workflows remain Claude-only.

Antigravity panes are passively included when the tmux pane command is `agy` and the secondary signal passes (the resolved state directory exists, or the captured frame contains an Antigravity fingerprint). Conversation identity is resolved by walking the pane process tree for an open protobuf file descriptor first, and falling back to `~/.gemini/antigravity-cli/history.jsonl` for an exact-cwd workspace match. Antigravity panes show `⚛` in the dashboard, support state classification (`ChatReady`/`BusyResponding`/`AgyCommandPermissionPrompt`/`AgyFolderTrustPrompt`/`AgySettingsPersistPrompt`/`Unknown`), and derive cook time from `BusyResponding` using the standard derivation. Antigravity YOLO is opt-in per-pane via `botctl yolo start --pane <agy-pane>`; only the command-permission prompt (default option `1. Yes`) is auto-approved with `Enter` when the pane process is `agy`. Folder-trust and settings-persist prompts classify into their own states but require manual review. Prompt submission and Claude-style keybinding automation are not supported for Antigravity.

Codex CLI panes are included by capturing likely Codex terminal panes and requiring Codex screen text such as the OpenAI Codex header, Codex-specific prompt/approval language, or a `/statusline` that includes the `run-state` field. With that statusline enabled, `Ready` maps to `ChatReady`, while `Working` and `Thinking` map to `BusyResponding`. Codex YOLO can approve command permission dialogs by sending `y` for `Yes, proceed`; broader prompt submission and keybinding-based automation remain Claude-only.

Persistent mode creates or reuses a dedicated tmux session named `botctl-dashboard` on a separate tmux socket. It then attaches to that session, so if you launch it from `tmux display-popup`, tmux keeps control of popup size and closing the popup only detaches from the persistent dashboard. When launched from tmux, the persistent dashboard captures the outer tmux socket first and continues inspecting that outer server's Claude Code panes, resolvable OpenCode panes, and Pi panes instead of its own dedicated dashboard pane. Inside persistent mode, pressing `q` also detaches instead of stopping the dashboard process.

## Recovery And Prompt Work

Typical loop:

```bash
cargo run -- start --session demo --cwd /path/to/project
cargo run -- doctor --session demo
cargo run -- list
tmux attach -t demo
```

If the session is blocked on a known confirmation flow, target the pane directly:

```bash
cargo run -- approve --pane %19
cargo run -- reject --pane %19
cargo run -- dismiss-survey --pane %19
```

Or with an explicit tmux pane target:

```bash
cargo run -- approve --pane 0:2.3
```

`approve-permission` and `reject-permission` still work as aliases for older scripts.

Prepare and submit a prompt:

```bash
cargo run -- prepare-prompt --session demo --text "Summarize the current repo"
cargo run -- submit-prompt --session demo --pane %19 --text "Summarize the current repo"
```

Scope prompt prep or babysit work to one workspace:

```bash
cargo run -- prepare-prompt --session demo --workspace . --text "Summarize the current repo"
cargo run -- yolo start --all --workspace .
```

The runtime keeps desired YOLO policy in SQLite and enforces the effective supervision state in memory. Disabling YOLO through the dashboard, CLI, or HTTP becomes visible to the other clients immediately because they all read the same runtime state.

Managed clients use shared runtime leases, so a later dashboard or serve session can keep an auto-started runtime alive instead of having it torn down when an earlier client exits.

Show the CLI help:

```bash
cargo run -- help
```

## Keybinding Policy

`botctl` respects the user's existing Claude keybindings. It resolves actions like submit, external editor, and confirmation flows from `~/.claude/keybindings.json` instead of assuming that a hard-coded automation keymap is installed.

`install-bindings` is intentionally non-destructive. If the user already has a Claude keybinding file, `botctl` will merge in any missing required bindings when it can, and fail clearly on invalid JSON or key conflicts instead of overwriting the file.

Print the recommended automation keymap:

```bash
cargo run -- bindings
```

Create or update the keymap with any missing required bindings:

```bash
cargo run -- install-bindings
```

Write the recommended keymap to another path for inspection:

```bash
cargo run -- install-bindings --path /tmp/claude-keybindings.json
```

## Fixtures

Record a fixture case from a live session:

```bash
cargo run -- record-fixture --session demo --case folder_trust_prompt
```

Replay a saved fixture:

```bash
cargo run -- replay --path fixtures/cases/permission_dialog
```

## Release

Minimal release flow:

```bash
cargo fmt --check && cargo test && cargo package
git tag vX.Y.Z
cargo publish
git push && git push origin vX.Y.Z
```

## Current State Model

The classifier recognizes:

- `ChatReady`
- `PromptEditing`
- `UserQuestionPrompt`
- `BusyResponding`
- `PermissionDialog`
- `PlanApprovalPrompt`
- `FolderTrustPrompt`
- `SurveyPrompt`
- `ExternalEditorActive`
- `DiffDialog`
- `Unknown`

Recap is auxiliary metadata, not a primary state. Strong anchors like `while you were away` and `away summary` can surface it, but `/recap` by itself does not.

`approve` accepts both `PermissionDialog` and `FolderTrustPrompt`. For `FolderTrustPrompt`, `botctl` sends raw `Enter` because that flow must confirm the default selected option directly. `approve-permission` remains an alias for older scripts.

## Current limits

- Live classification is still built around `capture-pane`, with `serve` using a best-effort merged stream model when that helps break `Unknown` states.
- The classifier is keyword-based and intentionally conservative.
- `botctl` can attach to existing Claude Code panes, but the strongest and most tested automation path is still managed Claude sessions.
- OpenCode support is passive dashboard/status visibility. Codex support includes dashboard/status visibility and YOLO approval for command permission dialogs; broader guarded keypress automation remains Claude-only.
- Antigravity support is dashboard/status visibility plus opt-in YOLO auto-approve for the command-permission prompt only (`yolo start --pane <agy-pane>`). Folder-trust and settings-persist prompts classify per-shape but remain manual. Prompt submission and Claude-style keybinding automation are not supported.
- `serve` is an initial foreground observer, not the full daemon/API/SSE control plane described in `PLANS-Serve-Mode.md` yet.
