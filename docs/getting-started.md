# Getting started

`botctl` is a Rust CLI for launching, observing, and safely driving Claude Code sessions inside `tmux`.

## Requirements

- Rust and Cargo
- `tmux`
- `claude` on `PATH`

## Build and test

```bash
cargo test
```

## Basic workflow

Start a managed Claude session:

```bash
cargo run -- start --session demo --cwd /path/to/project
```

Inspect what `botctl` sees:

```bash
cargo run -- list-panes
cargo run -- doctor --session demo
cargo run -- status --pane %19
```

Attach with tmux when you want the full terminal UI:

```bash
tmux attach -t demo
```

## Adopt an existing Claude pane

If Claude was already started inside tmux, point `botctl` at the pane explicitly:

```bash
cargo run -- list-panes --all
cargo run -- attach --pane %19
```

## Recover known blockers

If Claude is blocked on a supported confirmation flow, target the pane directly:

```bash
cargo run -- approve-permission --pane %19
cargo run -- reject-permission --pane %19
cargo run -- dismiss-survey --pane %19
cargo run -- continue-session --pane %19
cargo run -- auto-unstick --pane %19
```

## Prompt handoff flow

```bash
cargo run -- prepare-prompt --session demo --text "Summarize the current repo"
cargo run -- submit-prompt --session demo --pane %19 --text "Summarize the current repo"
```

## Long-lived observation

`serve` is the first long-lived observer path.

Run it against one tmux session:

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

## Current safety rules

- automation always resolves to an explicit tmux pane id
- Claude ownership is validated before automation runs
- guarded workflows only run from compatible classified states
- `Unknown` is treated as a refusal state, not a guess
- folder-trust confirmation is special and still sends raw `Enter`

## Current limits

- live classification still depends on `capture-pane` plus focused recent lines
- the classifier is conservative and keyword-based
- `serve` is currently a foreground observer, not the full daemon/API system yet
