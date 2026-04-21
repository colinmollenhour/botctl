# Getting started

`botctl` is a Rust CLI for launching, observing, and safely driving Claude Code sessions inside `tmux`.

For end-to-end operator flows, see [workflows](workflows.md). For command details, see [command reference](command-reference.md).

## Requirements

- Rust and Cargo
- `tmux`
- `claude` on `PATH`

## Build and test

```bash
cargo test
```

## First success

Start a managed Claude session:

```bash
cargo run -- start --session demo --cwd /path/to/project
```

Then inspect the session from `botctl`:

```bash
cargo run -- list-panes
cargo run -- doctor --session demo
cargo run -- status --pane %19
```

Attach with tmux when you want the full terminal UI:

```bash
tmux attach -t demo
```

## If Claude already exists in tmux

If Claude was already started inside tmux, resolve and verify the pane explicitly before acting:

```bash
cargo run -- list-panes --all
cargo run -- attach --pane %19
```

## Observe versus serve

- `observe` is bounded and returns a fixed snapshot/event sample.
- `serve` is the current foreground observer for one tmux session.
- `serve` is not a full daemon/API/SSE product yet.

Run `serve` against one tmux session:

```bash
cargo run -- serve --session demo
```

Watch one specific pane only:

```bash
cargo run -- serve --session demo --pane %19
```

Machine-readable stream:

```bash
cargo run -- serve --session demo --format jsonl
```

## Safety rules

- Prefer explicit pane IDs for automation.
- Never automate an ambiguous target.
- Claude ownership is validated before automation runs.
- `Unknown` is a refusal state, not a guess.
- Folder trust approval is special and uses raw `Enter`.

For blocker recovery, prompt handoff, and recovery examples, see [workflows](workflows.md) and [prompt handoff](prompt-handoff.md).

## Current limits

- live classification is still built around `capture-pane`, with `serve` using a best-effort merged stream model when that helps break `Unknown` states
- the classifier is conservative and keyword-based
- `serve` is currently a foreground observer, not the full daemon/API system yet
