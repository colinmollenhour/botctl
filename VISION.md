# botctl Vision

`botctl` is a CLI for managing and driving Claude Code sessions that run inside `tmux`.

The system is built around one assumption: terminal automation is only reliable when input, observation, and policy are treated as separate concerns. Sending keys is not enough. The CLI needs to know which pane it owns, what the pane is currently showing, and which action is safe to take next.

## Product Goal

Provide a durable operator-facing CLI that can:

- launch Claude Code in dedicated tmux sessions and windows
- identify and track the target pane by tmux pane ID
- drive Claude Code using stable keybindings and explicit actions
- observe session state from tmux output and pane snapshots
- classify important UI states such as permission prompts, survey prompts, chat-ready state, and external editor flows
- replay captured pane buffers and event streams against a regression test suite

## Current Capabilities

The current scaffold already supports:

- launching a managed Claude session in tmux
- listing panes and resolving active panes for a session
- capturing pane contents and recording fixture cases
- classifying live panes and replay fixtures through the same classifier
- running `status` and `doctor` diagnostics on a live pane or session
- preparing prompts through the external-editor handoff path
- guarded higher-level actions for prompt submission, permission approval, permission rejection, and survey dismissal

## Current State Model

The classifier currently recognizes these explicit states:

- `ChatReady`
- `BusyResponding`
- `PermissionDialog`
- `FolderTrustPrompt`
- `SurveyPrompt`
- `ExternalEditorActive`
- `DiffDialog`
- `Unknown`

`Unknown` should remain the preferred fallback whenever the classifier is not confident enough to drive automation safely.

## Core Principles

- Prefer tmux pane IDs over names or indexes.
- Prefer machine-readable observation over screen scraping.
- Keep Claude automation deterministic by resolving explicit Claude actions to concrete tmux key sequences.
- Treat classifiers as versioned software with fixtures, replay tests, and drift detection.
- Separate session orchestration from policy logic so behavior can evolve without rewriting the transport layer.

## Keybinding Policy

`botctl` must respect the user's existing Claude keybindings. It should resolve the user's configured bindings for actions such as submit, external editor, and confirmation flows, and it must not silently overwrite `~/.claude/keybindings.json`.

The `install-bindings` command exists to write the recommended automation keymap only when there is no conflicting existing file, or when a developer points it at an alternate output path for inspection.

## Persistence Model

`botctl` should keep runtime state out of the repository by default.

The default machine-local state root should be:

- `$XDG_STATE_HOME/botctl`
- fallback: `~/.local/state/botctl`

Within that root, `botctl` should use a hybrid persistence model:

- SQLite at `$XDG_STATE_HOME/botctl/state.db` for durable control-plane state
- regular files for larger artifacts and operator-facing debug exports

SQLite is the right home for data that needs transactional updates, cross-process coordination, stable identity, and restart recovery. That includes things like:

- tracked instance metadata
- managed/adopted ownership records
- last-known classifications and timestamps
- recent action history
- policy/babysit registrations
- prompt handoff state

Regular files should remain the home for data that is naturally append-only, bulky, or easier to inspect outside the database. That includes things like:

- captured event tapes
- pane snapshots and debug bundles
- exported diagnostics
- fixture corpora and other reviewable artifacts

The important split is conceptual: control-plane state in SQLite, artifacts in files. `botctl` should not use a repo-local `.botctl/` tree as the default runtime state store.

## Observation Model

`botctl` uses two observation paths:

- Primary: tmux control mode output for low-latency event streaming.
- Secondary: `capture-pane` snapshots for reconciliation, fixture capture, and debugging.

This combination keeps the live system efficient while preserving a stable artifact format for tests and incident review.

Today, live classification still uses `capture-pane` plus a recent-lines heuristic. A reconstructed screen model from control-mode output remains future work.

## Attachment Model

The current tool is strongest when it launches and manages its own Claude session, but its targeting model is already built around explicit pane IDs and tmux-discovered session metadata. Attaching `botctl` to arbitrary existing Claude panes is a planned first-class workflow and should preserve the same pane-ID safety rules.

## Known Limits

- The classifier is still keyword-based and intentionally conservative.
- `status` and `doctor` are probes, not a persistent observer.
- Live state decisions can still be affected by stale scrollback because `botctl` does not yet reconstruct the full visible screen.
- There is no long-lived policy engine or supervisor process yet.

## Near-Term Outcome

The first usable version should let an operator:

1. launch a managed Claude Code session in tmux
2. capture pane content
3. classify the current UI state
4. replay saved fixtures through the same classifier locally
5. diagnose a live pane with `status` and `doctor`
6. drive a small set of guarded workflows safely

## Longer-Term Outcome

The longer-term system should support:

- continuous observation of multiple sessions
- policy-driven automation for confirmation flows
- prompt injection through a controlled external editor helper
- fixture collection for new Claude Code releases
- compatibility checks that reveal classifier drift before automation is trusted in production
