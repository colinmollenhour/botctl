I want botctl to implement a two-man team continuous agent re-focusing loop - called by some a "ralph" loop. But two
heads are smarter than one so mine is called "dynamic-duo". It takes a path to a text file as an argument which describes
the large task at hand. E.g. `botctl dynamic-duo ./WISHLIST.md --pane 0:1.0`

It works by kicking off on an already running claude instance specified by the CLI (e.g. --pane X or if not specified, find
a single claude pane running in the **current** window - if no claude or more than one claude is in the current window, then
exit with a helpful error because we need to know which claude to attach to).

The instance should already have context as to what the task at hand is. The "dynamic-duo" command does this:

1. It activates yolo mode unless --no-yolo was specified (similar semantics to "keep-going").
2. It watches the claude for an idle state - that is, claude is waiting for the user to input something in the chat (it is likely
   already idle). If the loop has not started yet, the first prompt to submit will be "restart the loop".
3. When it becomes idle, botctl checks the last line of output for one of the keywords: `DUO_PREVAILED`, `DUE_FAILED`, `GRUNT_FINISHED`, `BOSS_FINISHED`.
   - If the `DUO_PREVAILED` keyword is found, botctl exits with a happy message and code 0, the entire work is complete!
   - If the `DUO_FAILED` keyword is found, botctl exits with a sad message and code 2, the human will have to intervene.
   - If the `GRUNT_FINISHED` keyword is found, botctl needs to "bring in the boss".
   - If the `BOSS_FINISHED` keyword is found, botctl needs to "restart the loop".
   - Else if there is a message but none of the keywords are found, botctl needs to prompt the agent to "keep going".

When an agent reports `GRUNT_FINISHED` or `BOSS_FINSHED`, botctl should send the command `/clear` to clear the session. This
only takes a few milliseconds and then the prompt will be ready for input again. It then enters a new prompt which is one of
the prompts described below plus the "standard prompt" at the end.

## Restart The Loop

**(this is not the exact prompt, write an elegant prompt that captures this workflow)**
**(the `{{file_path}}` part should be injected by botctl so it's impossible to be forgotten in context rot.**

--- Begin
Your job is to work the wishlist or planning file ({{file_path}}) in serial individual slices.

- Look at the last git log messages for context, ignoring the default branch (main, develop, master, dev, etc) and anything older than it. E.g. `git log --stat $(git merge-base main HEAD)...` - if there are no commits then we must be starting fresh so just move on.
- Pick the highest-priority unchecked item (`[ ]`) that is feasible to start now based on the plan (no blockers noted).
- If all checkboxes were already checked BEFORE the agent read the file, (either checked `[x]` or marked as blockers `[?]` or `[!]`) then output a happy congratulations and the final line should be `RALPH_DONE`. Stop here.
- Otherwise intelligently pick an empty checkbox and that can advances the task progress and hop to it.
- Prefer code/tests/docs that move the item forward over analysis about the item.
- If an item is just too ambiguous to perform and serious implementation risk remains without clarification, do not guess. Instead,
  append a `Questions and Problems` section to the end of the wishlist/planning file if needed, add the blocking question/problem there,
  referencing which checklist item it belongs to. Mark the checklist item as `[~]` and then output a short summary message (no more tool
  calls needed) and the final line output should be `SLICE_FINISHED`.

Assuming the chosen slice CAN be executed, here are the execution rules:

- Work on one bounded slice at a time.
- Each completed slice should be its own separate git commit.
- Use the built-in Todo tool if needed for large slices.

Completion: When finished with all of the work needed to complete the slice, including quality gates, docs, tests, security checks, e2e tests (if possible), smoke tests, etc, then mark the checklist item with a tilde `[~]`, commit, push and echo `SLICE_FINISHED`.
--- End

## Bring In The Boss

**(this is not the exact prompt, write an elegant prompt that captures this workflow)**
**(the `{{file_path}}` part should be injected by botctl so it's impossible to be forgotten in context rot.**

--- Begin
You job is to do code review and problem-solve and fix up or otherwise "unstick" the project slice described in the wishlist or planning file ({{file_path}}) that is marked with a `[~]` (and should be described in the most recent commit).

- Look at the last git log message(s) for context (same as above)
- Find the wishlist item that is marked with `[~]` and look for any Questions or Problems that reference that checklist item. This is your problem to solve. If there is no Question or Problem, then you're just doing a comprehensive code review of the last commit.
- Gather context as needed - README, product specs, vision, code, etc. to try and come up with a good solution.
- If you're confident in the solution then go ahead and fix it yourself!
- Update the checklist:
  - `[x]` - If successful in solving the problem fully and the wording of the task described can be said to be fully satisfied by the work product
    - Add a response to the related Questions and Problems section so it is clear it was addressed.
  - `[?]` - slice was blocked by an ambiguity or critical question that you must defer to the human in charge - describe the conundrum
  - `[!]` - slice was blocked by an unresolvable bug or technical issue - describe the problem (not the solution!)
- Commit and push
- Send the final message, always `SLICE_FINISHED`
--- End

## Standard Prompt

This is appended to both prompts above.

--- Begin
Git rules:

- Always commit the progress and push as the last step before sending the final message. If it's marked as a question or problem then it **must** be added to the commit message AND the project file.
- Stage only files relevant to the slice's work, do add to `.gitignore` if files were added in the process of development that should not be committed. The working directory should be completely clean before exiting.
- Use a concise conventional commit message in the first line. Add additional detail after a blank line, in particular anything that will be helpful for future coders to know about this particular feature that isn't obvious.
- Push the new commit to the upstream - do not use `-u` or `--set-upstream-to` - if there is no upstream then skip the push.
--- End

### Keep Going

When an agent leaves a message for the user that doesn't have a keyword, then probably it has gotten off track or lost context
and forgotten its instructions. We need to prompt it to get back on track and keep working until the slice is absolutely finished.
It may need a reminder of it's instructions that were given when it started:

--- Begin
You should not have stopped yet. If you know what can be done next on this slice then keep going. Otherwise, if there is a major
blocker, you should have completed the task termination instead of responding with a normal message.

{{repeat_original_prompt}}
--- End

If the same agent responds more than twice with an invalid response (no required keyword present) then assume it has gone off the rails and
just clear it and start a new session as if it had responded with `GRUNT_FINISHED` or `BOSS_FINISHED`.
