# Troubleshooting

Common operator failures and what they usually mean.

## Missing keybindings

Symptoms:

- `doctor` reports missing or invalid bindings
- guarded workflows fail before any keypresses are sent
- `install-bindings` reports invalid JSON or a key conflict in the existing file

Fix:

- run `botctl bindings` to inspect the recommended map
- use `botctl install-bindings` to merge in any missing required bindings when it can
- if install still fails, fix the conflicting or invalid JSON manually
- re-run `doctor`

## Ambiguous targets

Symptoms:

- a command says the target is ambiguous
- session-only targeting finds no active pane

Fix:

- prefer `--pane %ID`
- if using a session target, add `--window NAME` so the active pane can be resolved safely

## Non-Claude panes

Symptoms:

- refusal mentions the current command is not `claude`
- `doctor` shows `command_matches_claude=false`

Fix:

- stop targeting that pane
- find the actual Claude pane first
- never force automation into shells, editors, or unrelated tmux panes

## Stale observation

Symptoms:

- `status` or `serve` reports `Unknown` unexpectedly
- the visible UI and captured text disagree

Fix:

- wait for the next reconcile/capture
- increase history lines if the relevant text is off-screen
- re-run `doctor` or `status` on the explicit pane

## `submit-prompt` fails

Symptoms:

- submission reports that no prompt-submission transition was observed
- the pane is not `ChatReady`

Fix:

- confirm the pane is Claude-owned
- confirm the state is `ChatReady`
- check keybindings with `doctor`
- if Claude is in `ExternalEditorActive` or `DiffDialog`, review manually first

## Babysit already active

Symptoms:

- starting `permission-babysit` / `yolo` reports an existing record or tracked pane

Fix:

- stop the existing babysit record first
- then restart it for the same pane

## tmux problems

Symptoms:

- pane lookup fails
- capture or send-keys errors mention tmux

Fix:

- verify tmux is running
- confirm the pane id still exists
- make sure the session was not renamed or destroyed

## When to stop and review manually

Always review manually when the state is:

- `Unknown`
- `DiffDialog`
- `ExternalEditorActive`

Those states are refusals, not invitation to guess.
