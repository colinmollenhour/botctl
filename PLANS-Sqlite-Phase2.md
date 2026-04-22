# botctl SQLite Phase 2 Plan

## Execution Checklist

- [x] Add botctl-managed workspace UUIDs and instance UUIDs.
- [x] Add workspace resolution for `--workspace <PATH|UUID>` and cwd fallback.
- [x] Add `workspaces` and `instances` tables plus schema migration from the current SQLite layout.
- [x] Convert pending prompts to reference `instance_id`.
- [x] Convert babysit registrations to reference `instance_id`.
- [x] Create placeholder instances for prompt preparation before a live pane exists.
- [x] Promote or merge placeholder instances when prompt submission resolves a live pane.
- [x] Keep `yolo --all` global by default.
- [x] Add `--workspace <PATH|UUID>` filtering to prompt and babysit commands.
- [x] Update tests for workspace resolution, migration, prompt flow, and babysit flow.
- [x] Update command and workflow docs.

## Scope

Phase 2 extends the SQLite runtime state so machine-local state is no longer keyed only by ad hoc session names or pane ids. The runtime model becomes:

- `workspaces`: canonical workspace identity with a botctl UUID
- `instances`: canonical runtime instance identity with a botctl UUID
- prompt handoff and babysit state referencing `instance_id`

## Workspace Rules

- `--workspace` accepts either a botctl workspace UUID or a path.
- Relative paths are resolved from the current working directory.
- If the resolved path is inside a Git worktree, botctl scopes to that canonical worktree root.
- Sibling worktrees are related by a shared parent-repo key derived from Git common-dir state.
- If the path is not Git-backed, the resolved path itself is the workspace root.

## Instance Rules

- `instances` is the primary runtime identity table.
- Real tmux-backed instances carry the latest pane metadata.
- Placeholder instances exist so `prepare-prompt` can store a prompt before a live pane is resolved.
- Prompt and babysit tables should reference `instance_id` rather than using pane ids or session names as their primary durable identity.

## Command Behavior

- `prepare-prompt` resolves a workspace and stores the prompt under a placeholder instance.
- `editor-helper` resolves the same placeholder instance and reads or consumes its pending prompt.
- `submit-prompt` resolves a live pane, finds the matching placeholder instance, and promotes or merges it into a live instance.
- `yolo start` resolves or creates a tmux-backed instance for the target pane.
- `yolo --all` remains global across the state DB unless `--workspace` is provided.

## Migration

- Migrate the current schema forward without dropping SQLite state.
- Create workspaces from inferred paths where possible.
- Create instances for existing babysit rows.
- Create placeholder instances for existing pending prompts.
- Preserve enough metadata to make later serve-mode persistence attach naturally to `instances`.
