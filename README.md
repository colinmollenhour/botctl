# botctl

`botctl` is a Rust CLI for launching, inspecting, and driving Claude Code sessions inside `tmux`.

The project is built around a simple rule: terminal automation is only safe when tmux transport, live observation, classification, and action policy stay separate. Sending keys alone is not enough.

## Main Commands

These are the commands that matter most in day-to-day use:

- `dashboard` to see all live Claude panes, grouped by workspace, with state, age, and YOLO controls
- `yolo` to babysit one pane or a scoped set of panes automatically
- `serve` to stream live observation data for one tmux session in human or JSONL form

Everything else is mostly setup, diagnostics, recovery, or lower-level plumbing around those flows.

## Current Features

- launch a managed Claude session in tmux
- list panes and inspect tmux metadata
- capture pane contents and classify the current UI state
- run `status` and `doctor` against a live Claude pane
- run `serve` as a foreground long-lived observer for one tmux session
- run `dashboard` as a popup-sized TUI across Claude panes, grouped by workspace with per-pane YOLO controls
- record and replay fixture cases for classifier regression tests
- prepare prompts and hand them off through an external-editor workflow
- run guarded higher-level actions such as prompt submission, permission approval, permission rejection, and survey dismissal

## Docs

- published site: `https://botctl.readthedocs.io/en/latest/`
- docs app source: [`docs/`](docs/)
- intro: [`docs/docs/intro.mdx`](docs/docs/intro.mdx)
- getting started: [`docs/docs/getting-started.mdx`](docs/docs/getting-started.mdx)
- workflows: [`docs/docs/workflows.mdx`](docs/docs/workflows.mdx)
- architecture: [`docs/docs/architecture.mdx`](docs/docs/architecture.mdx)
- serve mode: [`docs/docs/serve-mode.mdx`](docs/docs/serve-mode.mdx)

## Requirements

- Rust and Cargo
- `tmux`
- `claude` available on `PATH`

## Test

Run the full test suite from the repo root:

```bash
cargo test
```

Run a single test by name:

```bash
cargo test resolves_custom_binding_keys_for_actions
```

## Start Here

If you just want the useful path quickly:

```bash
cargo run -- dashboard
cargo run -- yolo --pane 0:6.0
cargo run -- serve --session demo --format jsonl
```

## Core Workflows

Pane-targeted commands accept either a raw tmux pane id like `%19` or an explicit tmux pane target like `0:2.3`.

Open the live dashboard across Claude panes:

```bash
cargo run -- dashboard
```

Keep the dashboard alive in a dedicated tmux-backed popup session:

```bash
cargo run -- dashboard --persistent
```

Quick and easy tmux popup binding:

```tmux
bind-key C-c display-popup -E -w 80% -h 40% botctl dashboard --persistent
```

Start YOLO babysitting for one pane:

```bash
cargo run -- yolo --pane 0:6.0
```

Run the long-lived observer for one tmux session:

```bash
cargo run -- serve --session demo
```

Run the observer and a localhost HTTP API for a web UI:

```bash
cargo run -- serve --session demo --http 127.0.0.1:8787
```

Use machine-readable output for tooling:

```bash
cargo run -- serve --session demo --format jsonl
```

The HTTP API exposes live pane state plus interactive controls such as visible prompt options. Useful endpoints include:

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
```

Inspect a managed session:

```bash
cargo run -- doctor --session demo
```

Capture a pane directly:

```bash
cargo run -- capture --pane %19 --history-lines 120
```

Check the live classified state for a pane:

```bash
cargo run -- status --pane %19
```

The same command using tmux pane syntax:

```bash
cargo run -- status --pane 0:2.3
```

The dashboard groups Claude panes by workspace, shows the current classified state and age for each pane, lets you jump directly to a pane with `Enter`, and can toggle YOLO per pane, per workspace, or globally while it is open. While it runs, it also prefixes tmux window names with per-pane status emojis in pane-index order.

Persistent mode creates or reuses a dedicated tmux session named `botctl-dashboard` on a separate tmux socket. It then attaches to that session, so if you launch it from `tmux display-popup`, tmux keeps control of popup size and closing the popup only detaches from the persistent dashboard. When launched from tmux, the persistent dashboard captures the outer tmux socket first and continues inspecting that outer server's Claude panes instead of its own dedicated dashboard pane. Inside persistent mode, pressing `q` also detaches instead of stopping the dashboard process.

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
cargo run -- approve-permission --pane %19
cargo run -- reject-permission --pane %19
cargo run -- dismiss-survey --pane %19
```

Or with an explicit tmux pane target:

```bash
cargo run -- approve-permission --pane 0:2.3
```

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

## Current State Model

The classifier currently recognizes:

- `ChatReady`
- `BusyResponding`
- `PermissionDialog`
- `FolderTrustPrompt`
- `SurveyPrompt`
- `ExternalEditorActive`
- `DiffDialog`
- `Unknown`

Recap is auxiliary metadata, not a primary state. Strong anchors like `while you were away` and `away summary` can surface it, but `/recap` by itself does not.

`approve-permission` accepts both `PermissionDialog` and `FolderTrustPrompt`. For `FolderTrustPrompt`, `botctl` sends raw `Enter` because that flow must confirm the default selected option directly.

## Current Limits

- Live classification is still built around `capture-pane`, with `serve` using a best-effort merged stream model when that helps break `Unknown` states.
- The classifier is keyword-based and intentionally conservative.
- `botctl` can attach to existing Claude panes, but the strongest and most tested path is still managed sessions.
- `serve` is an initial foreground observer, not the full daemon/API/SSE control plane described in `PLANS-Serve-Mode.md` yet.
