# botctl

`botctl` is a Rust CLI for launching, inspecting, and driving Claude Code sessions inside `tmux`.

The project is built around a simple rule: terminal automation is only safe when tmux transport, live observation, classification, and action policy stay separate. Sending keys alone is not enough.

## Current Features

- launch a managed Claude session in tmux
- list panes and inspect tmux metadata
- capture pane contents and classify the current UI state
- run `status` and `doctor` against a live Claude pane
- run `serve` as a foreground long-lived observer for one tmux session
- record and replay fixture cases for classifier regression tests
- prepare prompts and hand them off through an external-editor workflow
- run guarded higher-level actions such as prompt submission, permission approval, permission rejection, and survey dismissal

## Docs

- [`docs/README.md`](docs/README.md) - docs index
- [`docs/getting-started.md`](docs/getting-started.md) - quick setup and operator workflow
- [`docs/workflows.md`](docs/workflows.md) - end-to-end operator flows
- [`docs/architecture.md`](docs/architecture.md) - module boundaries and design rules
- [`docs/serve-mode.md`](docs/serve-mode.md) - current serve-mode behavior and next steps

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

## Basic Commands

Pane-targeted commands accept either a raw tmux pane id like `%19` or an explicit tmux pane target like `0:2.3`.

Show the CLI help:

```bash
cargo run -- help
```

Start a managed Claude session in the current directory:

```bash
cargo run -- start --session demo
```

See what pane was created:

```bash
cargo run -- list-panes
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

Run the long-lived observer for one tmux session:

```bash
cargo run -- serve --session demo
```

## Real Session Workflow

Typical loop:

```bash
cargo run -- start --session demo --cwd /path/to/project
cargo run -- doctor --session demo
cargo run -- list-panes
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
