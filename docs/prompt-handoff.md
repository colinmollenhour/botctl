# Prompt handoff

`botctl` stages prompts in SQLite first, then hands them off through Claude's external-editor flow.

## Prompt paths

There are three supported paths:

1. **Manual staging**
   - `prepare-prompt` writes the prompt text to a session-scoped pending prompt record in the state database.
   - `editor-helper` reads that pending prompt from SQLite and writes it into the editor target path that Claude requested.

2. **One-shot submission**
   - `submit-prompt --text ...` or `submit-prompt --source ...` resolves the prompt text itself, stages it in the state database, and submits it.

3. **Loop submission**
   - `keep-going` uses the built-in audit loop prompt by default.
   - `keep-going --source ...` or `keep-going --text ...` stages a custom loop prompt in SQLite instead.

## State directory

The default state root is `$XDG_STATE_HOME/botctl` when `XDG_STATE_HOME` is set and non-empty. Otherwise, `botctl` uses `~/.local/state/botctl`.

`--state-dir PATH` overrides that root for commands that support it.

Relevant stateful commands now bootstrap `<state-dir>/state.db` with a minimal `schema_version` table plus the prompt-handoff table used for staged prompts.

Prompt handoff now uses the `pending_prompts` table inside `<state-dir>/state.db`, keyed by the CLI session name.

The external editor target that Claude requests is still a regular file, but the staged prompt itself is no longer stored under a separate prompt path.

## `prepare-prompt`

`prepare-prompt` accepts either `--text` or `--source` and writes the resolved content to the session's pending prompt record.

Use this when you want to stage content before the editor handoff begins.

## `editor-helper`

`editor-helper` is the bridge from staged prompt to Claude's editor target.

- with `--source`, it writes that source text directly to the target file
- without `--source`, it copies the pending prompt record into the target file
- by default it consumes the pending prompt record after copying
- `--keep-pending` leaves the staged prompt record in place

## `submit-prompt`

`submit-prompt` ultimately submits from `ChatReady`, but it can auto-dismiss `SurveyPrompt` first as a preflight recovery step.

It will:

- validate the target pane is Claude-owned
- optionally run a preflight recovery workflow when Claude is sitting in a supported intermediate state such as `SurveyPrompt`
- stage the prompt in the session state database
- send the submit sequence from the user's keybindings
- verify that the pane actually transitioned after submission

If the pane does not show a prompt-submission transition, the command fails rather than assuming success.

## Related docs

- [`command-reference.md`](command-reference.md)
- [`workflows.md`](workflows.md)
- [`automation.md`](automation.md)
