# sdmux Plan

## Phase 1: Executable Scaffold

- Initialize a Rust CLI crate with a library-backed module layout.
- Add tmux command wrappers for session launch, pane listing, key sending, and pane capture.
- Define core domain types for sessions, pane frames, and classifier results.
- Add a minimal CLI with `start`, `list-panes`, `capture`, `classify`, and `replay`.
- Create a fixture layout and replay tests that work from real captured-pane style text files.

## Phase 2: Observation Pipeline

- Add tmux control mode client support.
- Parse `%output`, `%extended-output`, and notification lines into structured events.
- Reconstruct screen state from streamed output plus periodic `capture-pane` reconciliation.
- Store event tapes and snapshots together for deterministic replay.

## Phase 3: Claude-Specific Automation

- Standardize an automation keymap in Claude Code.
- Add actions for permission navigation, submission flow, interrupt, and dialog handling.
- Add an external-editor helper so prompt injection does not depend on driving an interactive editor.
- Introduce an explicit state machine for safe transitions between chat, confirmation, and editor states.

## Phase 4: Regression and Drift Detection

- Build a corpus of fixtures grouped by Claude Code version and scenario.
- Snapshot classification outputs and reasoning signals.
- Add commands to capture fresh fixtures from live tmux sessions.
- Add diff tooling to compare a new fixture corpus against the expected classifier outcomes.

## Phase 5: Multi-Session Supervision

- Add a long-lived supervisor process or socket API if needed.
- Support multiple managed sessions with policy-driven automation.
- Persist metadata about sessions, panes, and last known classifier state.

## Immediate Milestones

1. Land the crate scaffold and project docs.
2. Prove tmux launch and pane capture from the CLI.
3. Prove fixture replay and classifier regression tests.
4. Add control mode observation without breaking the fixture model.
