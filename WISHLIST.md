# botctl Wishlist

## Checklist

1. [x] P0-1 Attach to existing Claude tmux targets.
2. [x] P0-2 Add `continue-session` and `auto-unstick` commands.
3. [x] P0-3 Validate Claude ownership before driving a pane.
4. [ ] P0-4 Keep higher-level automation state-aware.
5. [ ] P0-5 Improve live status and doctor output.
6. [ ] P1-1 Replace the bounded observer with a long-lived control-mode connection.
7. [ ] P1-2 Reconstruct a live terminal screen model.
8. [ ] P1-3 Merge streamed output with periodic `capture-pane` reconciliation.
9. [ ] P1-4 Capture structured event tapes for fixtures.
10. [ ] P1-5 Detect pane swaps, session renames, and window changes.
11. [ ] P1-6 Decorate tmux window titles when user attention is needed.
12. [ ] P2-1 Expand the classifier to cover more Claude UI states.
13. [ ] P2-2 Distinguish similar confirmation flows.
14. [ ] P2-3 Track classifier confidence and drift.
15. [ ] P2-4 Improve fixture organization and coverage.
16. [ ] P2-5 Add tooling to diff and refresh fixture corpora.
17. [ ] P3-1 Add full session lifecycle commands.
18. [ ] P3-2 Persist managed-session metadata and recent history.
19. [ ] P3-3 Add policy-driven continuous automation.
20. [ ] P3-4 Improve CLI and scripting ergonomics.
21. [ ] P3-5 Add end-to-end tests against real tmux sessions.
22. [ ] P3-6 Add docs, packaging, and release automation.
23. [x] P3-7 Add a one-off permission babysit mode for a single instance.
24. [ ] P3-8 Add interactive target selection for single-pane commands.
25. [ ] P3-9 Move runtime state to XDG and split SQLite control-plane state from file artifacts.
26. [ ] P3-10 Let `keep-going` run user-supplied prompt loops.

## P0

1. Attach to existing Claude tmux targets.
Allow `botctl` to adopt an already-running tmux session, window, or pane when the current command is `claude`. This closes a major workflow gap because operators often start Claude first and only later want structured observation and automation.

2. Add `continue-session` and `auto-unstick` commands.
These commands should inspect the current pane, clear known blocking prompts like folder trust, permission dialogs, and surveys, and then leave the session in a usable state. This turns the current low-level guarded actions into a practical operator flow.

3. Validate Claude ownership before driving a pane.
Before any automation sends keys, `botctl` should confirm the pane is really a Claude session and fail conservatively when it is not. This is a direct safety requirement for attaching to arbitrary existing tmux targets.

4. Keep higher-level automation state-aware.
Prompt submission, permission approval, rejection, survey dismissal, and trust acceptance should continue to fire only in compatible states. The remaining work is to harden the state machine so similar prompts do not collapse into the same bucket.

5. Improve live status and doctor output.
`status` and `doctor` should report the real current screen, not stale scrollback, and should explain exactly which recovery action is safe next. This is essential for debugging automation decisions in live sessions.

## P1

1. Replace the bounded observer with a long-lived control-mode connection.
The current one-shot observer is enough for probing but not enough for durable automation. A persistent connection is needed so `botctl` can watch sessions continuously and react without repeated attach/capture cycles.
See `PLANS-Serve-Mode.md` for the high-level serve-mode architecture that this persistent observer enables.

2. Reconstruct a live terminal screen model.
Classification should be based on a reconstructed frame from streaming output plus reconciliation, not plain text snapshots. That is the real fix for stale scrollback and fragile keyword matching.

3. Merge streamed output with periodic `capture-pane` reconciliation.
Streaming is low-latency but imperfect, while pane capture is slower but authoritative. The system should combine both so it can stay current and still recover from drift or dropped events.

4. Capture structured event tapes for fixtures.
Recorded cases should always include the control-mode output that led to a state decision, not just a final pane snapshot. That makes regression testing and classifier debugging much more explainable.

5. Detect pane swaps, session renames, and window changes.
Once `botctl` claims a pane, it should keep ownership even as tmux topology changes. This matters much more once existing-session attachment becomes a first-class workflow.

6. Decorate tmux window titles when user attention is needed.
When any pane in a tmux window is waiting on operator input, `botctl` should add a lightweight marker like `👋` to that window title and remove it once the window is no longer blocked. This is feasible with the current pane classification model, but it fits best once continuous observation/reconciliation owns tmux window metadata so title updates stay accurate and non-destructive.

## P2

1. Expand the classifier to cover more Claude UI states.
Add support for autocomplete, history search, transcript view, model picker, settings, tabs, task mode, and other non-chat screens. The current scaffold only covers the most obvious blocking states.

2. Distinguish similar confirmation flows.
Permission prompts, folder trust prompts, diff confirmations, and future dialogs should not all look the same to the policy layer. Better separation makes automation safer and lets commands express clearer intent.

3. Track classifier confidence and drift.
Each classification should include evidence and a confidence signal, and fixture replay should reveal when a Claude release changes the UI. This is the foundation for deciding when automation should refuse to act.

4. Improve fixture organization and coverage.
Fixtures should be grouped by Claude Code version and scenario, with better coverage for busy responses, diffs, editor mode, unknown states, and failure cases. Snapshot-style regression output would make changes much easier to review.

5. Add tooling to diff and refresh fixture corpora.
Operators should be able to capture fresh live fixtures, compare them with expected outcomes, and quickly see what changed. That will make release hardening much cheaper.

## P3

1. Add full session lifecycle commands.
`botctl` should stop, restart, destroy, and supervise managed Claude sessions instead of only launching and probing them. It should also support opening related windows or panes for multi-step workflows.

2. Persist managed-session metadata and recent history.
The tool currently rediscovers most state ad hoc. Persisting ownership, last-known observations, and action history would make long-lived supervision realistic.

3. Add policy-driven continuous automation.
Once observation is durable, `botctl` should be able to run rules continuously, such as always trusting the workspace, allowing a permission once, or declining surveys. That requires a clear policy layer rather than ad hoc command chaining.
See `PLANS-Serve-Mode.md` for the intended local daemon, API, and continuous policy model.

4. Improve CLI and scripting ergonomics.
Add better error messages, verbosity controls, JSON output, and possibly a more ergonomic argument parser when the hand-rolled CLI becomes a drag. This is quality-of-life work, but it will matter once the command surface grows.

5. Add end-to-end tests against real tmux sessions.
The unit and replay coverage is useful, but real-session tests are needed to trust the transport and timing behavior. This is the main remaining validation gap before heavier automation should be considered production-ready.

6. Add docs, packaging, and release automation.
The repo still needs installation instructions, a real `README.md`, CI, and a release story. None of that changes core behavior, but it is required if `botctl` is going to be used beyond local development.

7. Add a one-off permission babysit mode for a single instance.
This mode should temporarily persist automation state for one adopted Claude instance and only accept permission prompts while the operator is away. It should not expand into general continuous automation, and it should stop once that single instance exits or the operator disables it.

8. Add interactive target selection for single-pane commands.
When a command acts on a single Claude pane and the operator runs it from an interactive TTY without `--pane`, `--session`, or `--window`, `botctl` should list the available Claude targets and let the operator choose one. A simple numbered prompt is enough, but an up/down + Enter selector would be even better. For example, `auto-unstick` with no explicit target should present the chooser instead of failing immediately.

9. Move runtime state to XDG and split SQLite control-plane state from file artifacts.
`botctl` should stop using a repo-local `.botctl/` tree as the default runtime state store. The default root should move to `$XDG_STATE_HOME/botctl` (fallback `~/.local/state/botctl`), with durable control-plane state in `state.db` and larger artifacts kept as regular files. This should support persisted instance metadata, automation registrations, prompt handoff state, and future serve-mode history without importing the old repo-local scratch data.
See `PLANS-Sqlite.md` for the storage split and migration boundary.

10. Let `keep-going` run user-supplied prompt loops.
`keep-going` should optionally accept a user-supplied prompt file instead of the built-in audit prompt, then keep feeding that prompt to an idle Claude pane until the session returns one of the existing loop stop tokens like `ALL_DONE` or `PANIC`. This would let operators define their own supervised loops without changing botctl code for each workflow.

## Questions For Review

- Should automation remain limited to `PermissionDialog`, `FolderTrustPrompt`, and `SurveyPrompt`, with diff-like and ambiguous dialogs staying manual-review only?
- Is the current focused `capture-pane` excerpt good enough for `status` and `doctor`, or should those commands wait for a persistent observer before claiming stronger live-screen accuracy?
- What should trigger `capture-pane` reconciliation once streamed observation lands: timer, post-action validation, ambiguity, or all three?
- Do classifier confidence/drift and versioned fixture corpora need to land before more automation, or can they wait for the persistent observer?
- Before expanding the feature surface further, is the next investment JSON/CLI contracts, real tmux end-to-end tests, or docs/packaging/release automation?

## Decisions

- `attach` stays one-shot and non-persistent for now. When serve mode lands, it should persist adopted target metadata.
- `pane_current_command == "claude"` is sufficient ownership validation for now.
- `continue-session` and `auto-unstick` should stay limited. A separate one-off mode should persist for a single adopted instance and only accept permission prompts while the operator is away.
- For now, automation stays limited to `PermissionDialog`, `FolderTrustPrompt`, and `SurveyPrompt`. Diff-like and ambiguous dialogs stay manual-review only unless a specific scenario justifies expanding automation.
- Persisted instance identity should include more than raw `pane_id`. Start with `pane_id`, `pane_tty`, Claude PID plus PID start time, Claude session id, and workspace root; keep tmux/window IDs and names as secondary metadata.
