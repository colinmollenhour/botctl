# sdmux Serve Mode Vision

## Goal

Add a long-lived local `sdmux serve` process that can observe, classify, and safely drive many Claude Code instances running inside `tmux`.

The main product outcome is a stable control plane for Claude sessions:

- one local server process
- many tracked Claude instances
- HTTP for commands and queries
- SSE for streamed state, observation, and action events

This is the natural evolution of the current CLI from one-shot probing into durable supervision.

## Why This Exists

Today, `sdmux` can probe a pane, capture text, classify a state, and send guarded actions. That is enough for single-shot commands, but it is not enough for durable automation.

The missing capability is a persistent observer that stays attached to tmux, maintains current state over time, and serves that state to multiple clients without repeated attach and capture cycles.

Serve mode should solve that gap first. The HTTP API is a delivery mechanism for that persistent state, not the primary goal by itself.

## Product Shape

The user-facing surface should be:

- `sdmux serve` to run the local daemon
- `sdmux` client commands that talk to the daemon when it is available
- a local HTTP API for structured control
- SSE endpoints for real-time updates

SSE is one-way from server to client. Command and mutation traffic should remain standard HTTP requests.

## Managed Layout

The default managed layout should be:

- one managed tmux session owned by `sdmux`
- one Claude instance per tmux window
- explicit tmux pane IDs as the canonical automation target

This gives a strong operator experience:

- one `tmux attach` shows the full fleet
- windows are a natural Claude instance boundary
- humans can still inspect or recover manually

Internally, the design should remain session-agnostic. The default layout can be one session with many windows, but the registry should not assume that only one tmux session can ever exist.

## Identity And Ownership

The canonical identity for automation should remain the tmux `pane_id`.

Other identifiers are useful, but secondary:

- instance ID: stable API-facing identifier
- session name: useful for grouping and operator UX
- window name or index: useful for display and navigation

All state transitions and all key sends should resolve to an explicit pane ID before any automation runs.

## Core Responsibilities

Serve mode should own five responsibilities:

1. Observation
Maintain long-lived tmux control-mode connections and collect streamed events continuously.

2. Reconciliation
Use periodic or triggered `capture-pane` snapshots to correct drift and recover from missing or ambiguous streamed state.

3. Classification
Produce conservative pane state decisions from the best available reconstructed frame, preferring `Unknown` over unsafe guesses.

4. Automation
Run guarded actions only when the current classified state allows them.

5. Distribution
Expose current state and live updates to clients through a structured local API.

## Observation Model

The scaling unit should be one observer task per tmux session.

Each observer task should:

- hold one long-lived tmux control-mode connection
- parse `%output`, `%extended-output`, and notification lines
- route events into per-pane state pipelines
- detect topology changes such as pane swaps, window changes, or session renames
- trigger reconciliation when streamed state is incomplete or stale

Each tracked pane should have:

- a bounded event buffer
- last known tmux metadata
- last known reconstructed frame
- last known classification
- timestamps for last stream activity and last reconciliation
- last action and recent action history

## Performance Model

The expected default case is one tmux session with multiple Claude windows. That should be sufficient for typical local usage.

Performance should be protected by policy, not by splitting sessions early:

- debounce classification so token bursts do not trigger excessive work
- coalesce multiple stream events into a latest-known pane state update
- reconcile with `capture-pane` on a timer or on ambiguity, not on every event
- use per-pane queues so one noisy Claude instance does not starve others
- coalesce SSE output so clients receive meaningful state changes, not raw terminal noise by default

If measurement later shows a real bottleneck, the architecture can shard across multiple tmux sessions without changing the core model.

## API Shape

The first API should stay small and explicit.

Suggested HTTP endpoints:

- `POST /instances` to start or adopt a Claude target
- `GET /instances` to list tracked instances
- `GET /instances/:id` to fetch current metadata and classified state
- `POST /instances/:id/actions/submit-prompt`
- `POST /instances/:id/actions/approve-permission`
- `POST /instances/:id/actions/reject-permission`
- `POST /instances/:id/actions/dismiss-survey`
- `POST /instances/:id/actions/continue-session`
- `POST /instances/:id/actions/auto-unstick`

Suggested SSE endpoints:

- `GET /events` for fleet-wide events
- `GET /instances/:id/events` for per-instance events

The default SSE stream should emit structured events such as:

- instance discovered
- instance updated
- classification changed
- reconciliation completed
- action started
- action succeeded
- action refused
- observer warning

Raw output streaming should be treated as a debug surface, not the default client contract.

## Safety Model

Serve mode must preserve the existing safety rules:

- never automate an ambiguous target
- always resolve to an explicit pane ID
- verify that the target is actually Claude before automation
- keep workflow guards state-aware
- treat `Unknown` as a safe refusal state
- continue respecting user Claude keybindings as the source of truth
- preserve the folder-trust special case that requires raw `Enter`

The server should be opinionated about guarded actions and conservative by default. Clients should not need to reproduce policy logic.

## Adoption Model

Serve mode should support two ways to bring instances under management:

1. Managed instances
`sdmux` creates and owns the Claude tmux window.

2. Adopted instances
`sdmux` attaches to an already-running Claude target discovered in tmux.

Adoption should preserve the same safety guarantees as managed instances. No action should run until the target is verified and resolved to an explicit pane ID.

## Initial Scope

A good first version of serve mode should include:

- localhost-only server
- one managed tmux session by default
- one Claude instance per tmux window
- long-lived control-mode observation
- periodic `capture-pane` reconciliation
- per-pane classified state tracking
- guarded HTTP actions
- SSE for state and action events
- support for both managed and adopted instances

This first version does not need:

- remote access
- authentication or TLS
- arbitrary raw key injection over the public API
- multi-host orchestration
- a full terminal emulator

## Relationship To Existing Wishlist Items

Serve mode is not a single wishlist item. It is the umbrella direction that ties together several items:

- P1-1 Replace the bounded observer with a long-lived control-mode connection
- P1-2 Reconstruct a live terminal screen model
- P1-3 Merge streamed output with periodic `capture-pane` reconciliation
- P1-5 Detect pane swaps, session renames, and window changes
- P3-1 Add full session lifecycle commands
- P3-2 Persist managed-session metadata and recent history
- P3-3 Add policy-driven continuous automation
- P3-4 Improve CLI and scripting ergonomics

The key rule is that serve mode should be built on top of the observer and classifier foundations, not as a shortcut around them.

## Practical Sequence

The implementation should likely proceed in this order:

1. Replace one-shot observation with a long-lived per-session observer.
2. Add per-pane state tracking and reconciliation.
3. Detect topology changes and preserve pane ownership.
4. Add a local registry of tracked instances.
5. Expose a minimal local HTTP API.
6. Add SSE streams for state and action events.
7. Add higher-level workflows such as `continue-session` and `auto-unstick`.
8. Add continuous policy execution once the observer is stable.

## Open Design Notes

These choices are still intentionally flexible:

- whether the HTTP server uses a Rust web framework or a minimal custom stack
- whether raw debug event streams are exposed in v1 or deferred
- how aggressively to persist history and observation artifacts on disk
- whether client commands auto-discover the local server or require an explicit address

Those details should follow the core principle of explicit targeting, conservative automation, and operator-friendly recovery.
