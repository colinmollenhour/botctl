# botctl Documentation Plan

## Remaining Work Checklist

- [ ] Add a complete command reference for every shipped CLI command and alias.
- [ ] Split operator guides from contributor and architecture documentation.
- [ ] Document the managed-session workflow, adopted-pane workflow, and recovery workflows end to end.
- [ ] Document prompt handoff and external-editor flows, including state directory behavior.
- [ ] Document `keep-going` and `permission-babysit` clearly as policy-driven automation, including limits and safety rules.
- [ ] Document fixture capture, replay, and regression-testing workflows for contributors.
- [ ] Document current classifier states, signals, refusal behavior, and known ambiguity limits.
- [ ] Update architecture docs to match the current module map, including `screen_model` and serve-mode pieces.
- [ ] Add troubleshooting docs for common operator failures: missing keybindings, ambiguous targets, non-Claude panes, stale observation, and tmux issues.
- [ ] Add docs ownership and update rules so code changes and doc changes stay aligned.
- [ ] Make `docs/README.md` a real index of published docs, not a placeholder.
- [ ] Link root-level planning docs from the docs set without forcing users to read plans to understand current behavior.

## Goal

Turn the current useful-but-partial docs into a solid product doc set that is accurate, navigable, and complete enough for both operators and contributors to use `botctl` without reading the source first.

The doc set should help a reader do three things reliably:

- understand what `botctl` is and what safety guarantees it makes
- run the tool successfully for common workflows
- change the codebase without violating the architecture or automation rules

## Why This Exists

The current docs are good orientation material, but they do not yet cover the full product surface.

Current gaps include:

- no full command reference
- missing or shallow coverage for several shipped commands
- architecture docs that lag the code surface
- contributor workflows that still live mostly in code and tests
- planning documents in the repo root that currently carry context users should be able to get from `docs/`

That means the docs currently help a reader get started, but not fully operate or extend the tool with confidence.

## Documentation Principles

The docs should follow the same product values as the code:

- explicit targets over ambiguity
- conservative claims over aspirational claims
- operator-first explanations before implementation detail
- current behavior documented separately from future plans
- one clear home for each topic

Important rule: docs must describe what ships now. Vision and plan documents can describe future direction, but they should not be required reading for basic usage.

## Primary Audiences

The docs should serve three audiences:

1. Operators
People who want to launch, inspect, recover, and supervise Claude sessions safely.

2. Contributors
People changing classifier logic, automation rules, tmux observation, or prompt workflows.

3. Maintainers
People deciding whether a behavior change needs corresponding test, fixture, and docs updates.

## Target Doc Set

The finished `docs/` set should include at least these documents.

### Product and operator docs

- `docs/README.md`
  Real index with a short description of each doc and who it is for.

- `docs/getting-started.md`
  Quickstart for first successful session launch, inspection, and manual tmux attach.

- `docs/command-reference.md`
  Full CLI reference for every command, alias, required flags, common examples, and important refusal cases.

- `docs/workflows.md`
  End-to-end operator workflows:
  - start a managed session
  - adopt an existing Claude pane
  - diagnose a pane with `status` and `doctor`
  - recover supported blockers
  - prepare and submit prompts
  - use `serve` for long-lived observation

- `docs/prompt-handoff.md`
  Pending-prompt lifecycle, `prepare-prompt`, `editor-helper`, `submit-prompt`, state directory behavior, and failure recovery.

- `docs/automation.md`
  Guarded workflows, keybinding resolution, action routing, folder-trust special case, and refusal semantics.

- `docs/serve-mode.md`
  Current serve behavior only. Keep future roadmap brief and link to plan docs for the long-term direction.

- `docs/troubleshooting.md`
  Common failure modes and practical recovery steps.

### Contributor docs

- `docs/architecture.md`
  Updated module map, data flow, and current boundaries.

- `docs/classifier.md`
  State model, signals, ambiguity handling, fixtures, and how to extend states safely.

- `docs/testing-and-fixtures.md`
  Fixture capture, replay, expected outputs, regression testing, and when to add new cases.

- `docs/contributing.md`
  High-level contributor workflow, where to make changes, and required docs/tests updates for common code changes.

## Command Reference Scope

`docs/command-reference.md` should document every shipped command from the actual CLI surface, including aliases where they exist.

At minimum, it should cover:

- `start`
- `attach`
- `list-panes` and `list`
- `capture`
- `status`
- `doctor`
- `observe`
- `serve`
- `record-fixture`
- `classify`
- `replay`
- `bindings`
- `install-bindings`
- `send-action`
- `approve` and `approve-permission`
- `reject` and `reject-permission`
- `dismiss-survey`
- `continue-session`
- `auto-unstick`
- `keep-going`
- `prepare-prompt`
- `editor-helper`
- `submit-prompt`
- `yolo`, `permission-babysit`, and `babysit`
- `help`

Each command entry should include:

- what it does
- required and optional flags
- how the target is resolved
- safety checks and refusal cases
- one minimal example
- one realistic example when the flow is not obvious

## Gaps To Close First

The highest-priority missing documentation is:

1. Full command reference
Because the shipped CLI surface is already larger than the docs imply.

2. Workflow docs
Because operators need clear end-to-end paths, not just command snippets.

3. Automation and prompt-handoff docs
Because these are safety-sensitive and have non-obvious behavior.

4. Testing, fixtures, and classifier docs
Because contributors need a stable way to evolve behavior safely.

5. Architecture refresh
Because the current architecture writeup is close, but no longer a full map of the code.

## Proposed Sequence

### Phase 1: Stabilize the public surface

Add or update:

- `docs/README.md`
- `docs/getting-started.md`
- `docs/command-reference.md`
- `docs/workflows.md`

Outcome:

- an operator can install, launch, inspect, recover, and submit a prompt without reading source files
- every public CLI command is at least documented once

### Phase 2: Document safety-sensitive behavior

Add or update:

- `docs/automation.md`
- `docs/prompt-handoff.md`
- `docs/troubleshooting.md`
- `docs/serve-mode.md`

Outcome:

- a reader can understand why `botctl` refuses actions, how keybindings are resolved, and how prompt staging works
- serve-mode docs describe current behavior clearly without mixing in too much roadmap content

### Phase 3: Document contributor workflows

Add or update:

- `docs/architecture.md`
- `docs/classifier.md`
- `docs/testing-and-fixtures.md`
- `docs/contributing.md`

Outcome:

- a contributor can safely change classifier states, automation, observation, or fixtures and know which docs/tests must change too

### Phase 4: Keep docs from drifting again

Add lightweight maintenance rules:

- when adding a new CLI command, update the command reference in the same change
- when changing a classifier state, update classifier docs and relevant workflow docs in the same change
- when changing action routing or keybinding behavior, update automation docs and troubleshooting
- when adding a new fixture workflow, update testing docs
- when a module is added to `src/lib.rs`, confirm architecture docs still match

Outcome:

- docs become part of the definition of done rather than a cleanup pass later

## Suggested Information Architecture

The docs index should group content by user need, not by source file.

Suggested top-level groups:

- Start here
- Operator guides
- Command reference
- Contributor docs
- Design and plans

Planning documents such as `VISION.md`, `PLAN.md`, and `PLANS-Serve-Mode.md` can remain in the repo root, but the docs index should label them clearly as planning material, not current product reference.

## Writing Standards

The docs should use a consistent style:

- say what happens today before mentioning future work
- prefer examples with real commands and explicit pane IDs
- call out refusal states and safety checks explicitly
- keep architecture claims aligned with actual modules and current behavior
- avoid repeating the same command explanation in many files; link to the command reference instead
- keep future roadmap content in plan docs, not operator guides

## Acceptance Criteria

The docs effort should count as successful when all of these are true:

- a new operator can complete the main workflows using `docs/` alone
- a contributor can understand the current module boundaries without reading every source file first
- every CLI command in `usage()` has a corresponding reference entry
- every current classifier state is documented with its intended meaning
- prompt handoff and policy-driven automation are documented end to end
- `docs/README.md` reads like a finished docs index, not a placeholder
- root planning docs are optional context, not required for normal usage

## Practical First Slice

If this work is done incrementally, the best first slice is:

1. Add `docs/command-reference.md`.
2. Add `docs/workflows.md`.
3. Add `docs/automation.md`.
4. Refresh `docs/README.md` to point to the real set.
5. Refresh `docs/architecture.md` so it matches the current module map.

That gets the repo from partial orientation docs to a usable documentation system quickly, while leaving more detailed contributor docs for the next pass.
