# botctl SQLite State Plan

## Remaining Work Checklist

- [ ] Move the default runtime state root to `$XDG_STATE_HOME/botctl`.
- [ ] Keep `--state-dir` as an explicit override for the state root.
- [ ] Add `state.db` under the state root and initialize a schema-version table.
- [ ] Move prompt handoff state from ad hoc files into SQLite.
- [ ] Move babysit / continuous-automation registrations from ad hoc files into SQLite.
- [ ] Add stable workspace/worktree scoping so multiple repos do not bleed into each other.
- [ ] Add durable tracked-instance tables that can support serve mode.
- [ ] Add durable action-history tables for restart-safe automation and operator auditability.
- [ ] Keep bulky artifacts, event tapes, and exported diagnostics as regular files.
- [ ] Use SQLite in WAL mode with foreign keys and a busy timeout.
- [ ] Add startup schema migration logic for future versions of `state.db`.
- [ ] Do **not** migrate existing `.botctl` state; start fresh when the new state root is introduced.

## Goal

Replace the current repo-local runtime state tree with a more reliable machine-local storage model:

- SQLite for botctl's control-plane state
- regular files for artifacts and exports

The default path should be:

- `$XDG_STATE_HOME/botctl/state.db`
- fallback root: `~/.local/state/botctl`

The purpose is reliability, not novelty. The storage design should make concurrent CLI use, restart recovery, serve-mode growth, and future automation history safer and easier to reason about.

## Why This Exists

The current runtime state is intentionally small, but it already shows the weaknesses of a file-tree scratch store:

- repo pollution and accidental git adds
- check-then-write races for babysit/yolo registration
- identity encoded in directory names instead of structured records
- awkward growth path for serve mode, tracked instances, and action history

Those problems become much more important once `botctl` grows into a persistent local control plane.

SQLite is a better fit for the state that needs:

- atomic updates
- uniqueness constraints
- restart-safe state transitions
- cross-process readers and writers
- durable history tied to stable instance identity

## Storage Split

The split should be explicit and stable.

### SQLite: control-plane state

`state.db` should hold the durable, structured runtime state that `botctl` uses to make decisions.

That includes:

- workspace/worktree records
- tracked instance identity and ownership metadata
- last-known tmux metadata relevant to instance identity
- last-known classification and observation timestamps
- prompt handoff records
- babysit / continuous-automation registrations
- recent action history and refusal history
- future serve-mode registry state

### Files: artifacts and exports

Regular files should hold data that is naturally file-shaped rather than relational.

That includes:

- captured pane snapshots
- structured event tapes
- exported diagnostics bundles
- fixture corpora and replay inputs
- any future human-reviewed capture artifacts

These files should live under the XDG state root when they are machine-local runtime artifacts, and inside the repository only when they are intentional checked-in test fixtures.

## Default Layout

Suggested default layout:

```text
$XDG_STATE_HOME/botctl/
  state.db
  artifacts/
    captures/
    tapes/
    exports/
```

`--state-dir` should override the root directory, not the database filename. Under an override root, the database path should remain `state.db` for predictability.

## Reliability Rules

If SQLite is adopted, the implementation should be deliberately conservative:

- enable WAL mode
- enable foreign keys
- set a busy timeout
- keep transactions short and explicit
- use unique constraints instead of check-then-write logic where possible
- make action transitions atomic with their history writes
- avoid storing canonical identity only in path names

The design goal is that a crash or overlapping CLI invocation should not leave `botctl` unsure whether a prompt handoff or automation registration exists.

## Scope Boundaries

This plan is about botctl's **runtime state**, not every file the project owns.

Not everything should move into SQLite:

- checked-in fixtures should remain regular repository files
- large observation tapes should remain files
- operator-exported debug bundles should remain files

The database should hold the state that powers behavior. Files should hold artifacts that humans inspect or replay.

## Initial Schema Direction

The first schema can stay narrow and grow later. A good starting shape is:

- `schema_version`
- `workspaces`
- `instances`
- `instance_observations`
- `pending_prompts`
- `automation_registrations`
- `action_history`

The exact table names can change, but the first version should already support:

- stable workspace scoping
- stable instance identity beyond raw pane-id strings alone
- restart-safe prompt handoff
- restart-safe babysit / automation registration
- recent action auditability

## No Legacy Migration

Do not migrate existing `.botctl` state.

That data is inconsequential and disposable. When the XDG-backed SQLite store lands, `botctl` should simply start with a fresh state root and fresh `state.db`.

If old repo-local scratch state exists, it can be ignored or cleaned up manually. The new implementation should not spend effort importing it.

## Relationship To Other Plans

This plan supports, but does not replace, the broader serve-mode work.

It is most valuable for:

- P3-2 persisted managed-session metadata and recent history
- P3-3 policy-driven continuous automation
- future serve-mode tracked-instance registries and local APIs

See `PLANS-Serve-Mode.md` for the higher-level daemon and API shape. This document only defines how botctl should persist its local runtime state reliably.
