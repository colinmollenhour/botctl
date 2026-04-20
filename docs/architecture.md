# Architecture

`botctl` is built around one constraint: terminal automation is only safe when transport, observation, classification, and policy stay separate.

## Core modules

- `src/tmux.rs` - tmux transport, pane discovery, pane capture, key sending, and control-mode session management
- `src/observe.rs` - bounded observation and control-line parsing helpers
- `src/classifier.rs` - frame-to-state classification
- `src/automation.rs` - action definitions, keybinding resolution, and guarded workflow rules
- `src/app.rs` - CLI orchestration and user-facing command behavior
- `src/serve.rs` - long-lived foreground observer loop for serve mode
- `src/fixtures.rs` - fixture recording and replay support
- `src/prompt.rs` - prompt staging and external-editor handoff helpers
- `src/permission_babysit.rs` - one-off permission babysit state persistence

## Safety boundaries

### Transport

The tmux layer should do tmux things only:

- resolve panes
- capture panes
- send keys
- open and hold control-mode connections

It should not decide whether an action is safe.

### Observation

Observation is responsible for gathering terminal evidence:

- control-mode stream lines
- `%output` and `%extended-output`
- tmux notifications
- `capture-pane` snapshots for reconciliation

Observation should preserve enough evidence to explain later classifier decisions.

### Classification

The classifier turns a frame into an explicit state.

Current states:

- `ChatReady`
- `BusyResponding`
- `PermissionDialog`
- `FolderTrustPrompt`
- `SurveyPrompt`
- `ExternalEditorActive`
- `DiffDialog`
- `Unknown`

`Unknown` is preferred over a false positive.

### Automation and policy

Automation should only run after:

1. the target is resolved to an explicit pane id
2. the pane is confirmed to be Claude-owned
3. the current classified state permits the workflow

This is why raw `send-keys` success is never enough.

## Observation model

Today `botctl` uses two observation paths:

- bounded one-shot observation through `observe`
- long-lived observation through `serve`

The current live model is still a compromise:

- stream events give low latency
- `capture-pane` gives authoritative snapshots
- classification still runs on captured pane text, not a full reconstructed terminal screen

That means serve mode is a foundation, not the finished screen model.

## Serve-mode architecture today

The current `serve` implementation is intentionally small:

- one foreground process
- one tmux control-mode session per served tmux session
- per-pane buffering of recent streamed output
- debounced reconciliation via `capture-pane`
- structured human or JSONL events on stdout

This is the first slice of the larger serve-mode plan described in `PLANS-Serve-Mode.md`.

## Design rules

- prefer explicit pane ids over names or indexes
- never automate ambiguous targets
- keep observation and policy separate
- preserve the user's Claude keybindings as the source of truth
- keep fixture-based regression coverage close to classifier behavior
