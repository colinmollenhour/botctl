# botctl Agents

This document describes the internal agent roles that the system will eventually implement. These are logical roles inside `botctl`, not separate products.

## Current Modules

- `src/tmux.rs`: transport and tmux wrappers for session launch, pane lookup, capture, and key send
- `src/observe.rs`: bounded observation and control-mode parsing
- `src/classifier.rs`: frame-to-state classification with supporting signals
- `src/automation.rs`: action definitions, keybinding resolution, and guarded workflow constraints
- `src/app.rs`: CLI orchestration, live diagnostics, and high-level workflow execution
- `src/fixtures.rs`: fixture recording and loading
- `src/prompt.rs`: pending-prompt handoff and external-editor helper path

## Session Manager

Responsible for launching Claude Code inside tmux, tracking the owning session and pane IDs, and exposing basic lifecycle operations such as start, stop, capture, and key send.

## Observer

Responsible for reading tmux control mode output and periodic pane snapshots, then producing normalized frames that higher-level logic can consume.

## Classifier

Responsible for mapping a frame into an explicit state with supporting signals. It should be conservative: `Unknown` is preferable to a false positive that triggers the wrong keypress.

## Driver

Responsible for turning policy decisions into concrete tmux actions. It should only operate on explicit session or pane IDs and must avoid ambiguous targets.

## Fixture Recorder

Responsible for capturing pane snapshots and event tapes from live sessions, organizing them by Claude Code version and scenario, and making them available for replay tests.

## Policy Engine

Responsible for deciding what action is allowed in the current state. Example policies include:

- allow a permission once
- decline a survey prompt
- submit a prepared prompt
- interrupt a long-running session

## Engineering Rules

- Keep transport, observation, classification, and policy separate.
- Do not make automation decisions from raw `send-keys` success alone.
- Favor explicit pane IDs and structured events over implicit terminal assumptions.
- Preserve enough fixture data to explain classifier behavior when Claude Code changes.

## State And Action Contracts

- `Unknown` is safer than a false positive. If a new flow is ambiguous, refuse to act until the classifier improves.
- `submit-prompt` only runs from `ChatReady`.
- `approve-permission` currently accepts both `PermissionDialog` and `FolderTrustPrompt`.
- `FolderTrustPrompt` is special: the approval path must send raw `Enter`, not the user's `confirm-yes` binding.
- Guarded workflows should validate the current classified state before any key is sent.

## Keybinding Safety Rules

- Treat the user's Claude keybindings as the source of truth for action routing.
- Never silently overwrite `~/.claude/keybindings.json`.
- `install-bindings` must remain non-destructive for an existing custom keybinding file unless the product is intentionally redesigned.
- If an action is missing from the user's Claude keymap, fail clearly and point the operator at `doctor` or `bindings`.

## Pane Targeting Rules

- Prefer explicit pane IDs for all automation.
- Session names are only a convenience for resolving an active pane.
- Never automate an ambiguous target.
- Before adding attach-to-existing-session support, preserve the same pane-ID safety guarantees that managed sessions already rely on.

## Live Classification Caveat

- Current live classification is based on `capture-pane` text plus a recent-lines heuristic.
- This is a stopgap, not a full terminal screen model.
- Any work on observation or diagnosis should assume stale scrollback is still a real failure mode.

## When Editing

- If you add or change a classifier state, update guarded workflow assumptions and tests in the same change.
- If you change action routing, verify both the resolved keybinding behavior and any special-case raw keys like folder-trust `Enter`.
- If you change diagnostics, keep `status` and `doctor` useful for a human operator first.
- If you add fixture scenarios, make sure they explain why the classifier made its decision, not just what state it returned.
