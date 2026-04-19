# sdmux Wishlist

This file tracks features that are not implemented yet in the current scaffold.

## Recap

The current scaffold already includes:

- a Rust CLI with commands for session start planning, pane listing, capture, observe, replay, fixture recording, prompt preparation, editor-helper writes, and named Claude actions
- tmux wrappers for session launch, pane discovery, pane capture, key sending, and bounded control-mode attachment
- a first-pass classifier for chat-ready, permission, survey, diff, external-editor, busy, and unknown states
- a fixture corpus format with replay tests and recorded metadata
- a deterministic prompt handoff path built around pending prompts plus an external-editor helper

The remaining items below are the major missing capabilities and hardening work.

## Session Lifecycle

- Stop, restart, and destroy managed Claude sessions from the CLI.
- Create additional windows or panes for related workflows.
- Persist managed-session metadata instead of discovering it ad hoc.
- Validate that a launched pane is actually running Claude Code before driving it.
- Support multiple concurrent managed sessions with clear ownership and naming rules.

## Observation

- Maintain a long-lived tmux control-mode connection instead of a bounded one-shot observer.
- Reconstruct a terminal screen model from streamed output rather than classifying plain decoded text.
- Merge streamed output with periodic `capture-pane` reconciliation.
- Subscribe to tmux format changes and client notifications that matter for automation.
- Detect pane swaps, session renames, and window changes without losing ownership of the target pane.
- Capture structured event tapes from live sessions for replay.

## Claude Automation

- Install or update the recommended Claude keybindings automatically.
- Verify that the expected Claude automation keymap is active before sending actions.
- Add higher-level actions such as prompt submission, permission approval, permission rejection, survey dismissal, and interrupt recovery.
- Implement the external-editor helper so prompts can be injected deterministically.
- Add guardrails so actions only fire in compatible Claude UI states.
- Add a state machine for chat, confirmation, diff, survey, and editor flows.

## Classification

- Replace the keyword classifier with a more robust frame classifier.
- Track classifier confidence and supporting evidence.
- Classify more Claude states such as autocomplete, history search, transcript view, model picker, settings, tabs, and task mode.
- Distinguish between similar confirmation flows instead of collapsing them into one dialog bucket.
- Detect classifier drift across Claude Code releases.

## Fixtures And Regression Testing

- Record real control-mode event streams alongside captured pane snapshots.
- Group fixtures by Claude Code version and scenario.
- Add snapshot-style regression tests for classifier outputs and signals.
- Add tooling to diff new fixture corpora against expected classifications.
- Add commands to refresh fixtures from a live tmux session.
- Add fixture coverage for busy responses, diffs, editor mode, unknown states, and failure cases.

## CLI And UX

- Replace the hand-rolled CLI parser with a more ergonomic command-line interface if needed.
- Add JSON output modes for scripting and machine consumption.
- Add clearer error messages for missing sessions, panes, and tmux failures.
- Add logging and verbosity controls for debugging automation runs.
- Add commands for status, inspect, doctor, and fixture capture.

## Reliability And Safety

- Enforce pane ID targeting everywhere and reject ambiguous targets.
- Add retry, timeout, and backoff policies around tmux interactions.
- Prevent unsafe actions when the classifier returns `Unknown`.
- Add explicit recovery paths for lost control-mode connections.
- Handle non-UTF-8 pane output and ANSI escapes more carefully.
- Add integration tests against real tmux sessions and realistic terminal behavior.

## Persistence And Supervision

- Add a supervisor process or daemon mode for long-lived orchestration.
- Persist last-known state, recent observations, and action history.
- Expose a local control socket or API for external tooling.
- Support policy-driven automation rules that can run continuously.

## Packaging And Distribution

- Add installation instructions and operator documentation.
- Add a `README.md` with usage examples and architecture notes.
- Add release automation and versioning strategy.
- Add CI for formatting, tests, and fixture regression checks.

## Next Steps

- Replace the current bounded `script`-backed control-mode probe with a longer-lived observer that reliably captures live `%output` pane events.
- Build a real terminal screen reconstruction layer so classification is based on frames instead of plain captured text.
- Add higher-level guarded workflows for prompt submission, permission handling, and survey dismissal that check classifier state before acting.
- Install and validate the Claude automation keymap automatically so action routing is not a manual prerequisite.
- Extend fixture capture so recorded cases always include meaningful control-mode event tapes, not just pane snapshots and notifications.
- Add end-to-end tests against real tmux sessions, then start exercising the flows against a real Claude Code session.
