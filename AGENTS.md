# sdmux Agents

This document describes the internal agent roles that the system will eventually implement. These are logical roles inside `sdmux`, not separate products.

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
