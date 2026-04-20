# sdmux

`sdmux` is a Rust CLI for launching, inspecting, and driving Claude Code sessions inside `tmux`.

The project is built around a simple rule: terminal automation is only safe when tmux transport, live observation, classification, and action policy stay separate. Sending keys alone is not enough.

## Current Features

- launch a managed Claude session in tmux
- list panes and inspect tmux metadata
- capture pane contents and classify the current UI state
- run `status` and `doctor` against a live Claude pane
- record and replay fixture cases for classifier regression tests
- prepare prompts and hand them off through an external-editor workflow
- run guarded higher-level actions such as prompt submission, permission approval, permission rejection, and survey dismissal

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

Prepare and submit a prompt:

```bash
cargo run -- prepare-prompt --session demo --text "Summarize the current repo"
cargo run -- submit-prompt --session demo --pane %19 --text "Summarize the current repo"
```

## Keybinding Policy

`sdmux` respects the user's existing Claude keybindings. It resolves actions like submit, external editor, and confirmation flows from `~/.claude/keybindings.json` instead of assuming that a hard-coded automation keymap is installed.

`install-bindings` is intentionally non-destructive. If the user already has a custom Claude keybinding file, `sdmux` will refuse to overwrite it.

Print the recommended automation keymap:

```bash
cargo run -- bindings
```

Write the recommended keymap only when no conflicting file already exists:

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

`approve-permission` accepts both `PermissionDialog` and `FolderTrustPrompt`. For `FolderTrustPrompt`, `sdmux` sends raw `Enter` because that flow must confirm the default selected option directly.

## Current Limits

- Live classification still uses `capture-pane` plus a recent-lines heuristic.
- The classifier is keyword-based and intentionally conservative.
- `sdmux` is strongest with managed sessions today; attaching to arbitrary existing Claude panes is still planned work.
- There is no long-lived observer or supervisor process yet.
