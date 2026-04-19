# sdmux Vision

`sdmux` is a CLI for managing and driving Claude Code sessions that run inside `tmux`.

The system is built around one assumption: terminal automation is only reliable when input, observation, and policy are treated as separate concerns. Sending keys is not enough. The CLI needs to know which pane it owns, what the pane is currently showing, and which action is safe to take next.

## Product Goal

Provide a durable operator-facing CLI that can:

- launch Claude Code in dedicated tmux sessions and windows
- identify and track the target pane by tmux pane ID
- drive Claude Code using stable keybindings and explicit actions
- observe session state from tmux output and pane snapshots
- classify important UI states such as permission prompts, survey prompts, chat-ready state, and external editor flows
- replay captured pane buffers and event streams against a regression test suite

## Core Principles

- Prefer tmux pane IDs over names or indexes.
- Prefer machine-readable observation over screen scraping.
- Keep Claude automation deterministic by defining custom automation keybindings.
- Treat classifiers as versioned software with fixtures, replay tests, and drift detection.
- Separate session orchestration from policy logic so behavior can evolve without rewriting the transport layer.

## Observation Model

`sdmux` uses two observation paths:

- Primary: tmux control mode output for low-latency event streaming.
- Secondary: `capture-pane` snapshots for reconciliation, fixture capture, and debugging.

This combination keeps the live system efficient while preserving a stable artifact format for tests and incident review.

## Near-Term Outcome

The first usable version should let an operator:

1. launch a managed Claude Code session in tmux
2. capture pane content
3. classify the current UI state
4. replay saved fixtures through the same classifier locally

## Longer-Term Outcome

The longer-term system should support:

- continuous observation of multiple sessions
- policy-driven automation for confirmation flows
- prompt injection through a controlled external editor helper
- fixture collection for new Claude Code releases
- compatibility checks that reveal classifier drift before automation is trusted in production
