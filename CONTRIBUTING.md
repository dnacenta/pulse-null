# Contributing

Contributions are welcome. This document explains the workflow.

## Reporting bugs or requesting features

Open an [issue](https://github.com/dnacenta/pulse-null/issues). Use a clear title and include enough context to reproduce the problem or understand the request.

## Making changes

1. Fork the repo
2. Create a branch from `main` (see naming below)
3. Make your changes
4. Open a PR targeting `main`

`main` is protected — all changes land through a PR.

## Branch naming

Branches follow this pattern:

```
<type>/<issue-number>-<short-description>
```

| Type       | When to use                          | Example                              |
|------------|--------------------------------------|--------------------------------------|
| `feat`     | New functionality                    | `feat/12-plugin-voice-echo`          |
| `fix`      | Bug fix                              | `fix/7-auth-header-check`            |
| `refactor` | Code restructure, no behavior change | `refactor/15-extract-llm-module`     |
| `docs`     | Documentation only                   | `docs/3-config-reference`            |
| `chore`    | Maintenance, deps, CI                | `chore/20-update-dependencies`       |

If there's no issue yet, create one first so there's a number to reference.

## Commit messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/) (lowercase):

```
<type>(<scope>): <description>
```

Examples:

```
fix(auth): reject requests with empty secret header
feat(plugins): add voice-echo plugin
docs: add configuration examples
refactor(scheduler): split runner into separate module
```

Rules:
- Lowercase everything
- Imperative, present tense ("add" not "added")
- No period at the end
- Reference the issue in the body or footer: `Closes #7`

## Pull request titles

PR titles follow the same convention, referencing the issue number as scope:

```
fix(#7): reject requests with empty secret header
feat(#12): add voice-echo plugin
docs(#3): expand configuration reference
```

## Code style

- Run `scripts/gate.sh` before submitting -- it is CI's exact gate (fmt, `clippy --all-targets -- -D warnings`, tests), single source of truth for both
- Optionally run `scripts/install-hooks.sh` once per clone to make the gate a pre-push hook
- Intentionally unused code gets a targeted `#[allow(dead_code)]` with a comment explaining why -- there is no blanket `-A dead_code`, so unintentional dead code fails the gate
- Keep your toolchain current (`rustup update stable`) -- CI lints with the latest stable, so new clippy lints apply as Rust releases
- Keep changes focused -- one issue per PR

### The gate

`scripts/gate.sh` is deterministic by contract: no model and no human
judgment executes inside it, so it cannot habituate -- it reads PR 500 with
the same eyes it read PR 1. Human/AI review is for what the gate cannot
encode: whether the change should exist, collateral behavior, boundary
drift. Edits to `scripts/gate.sh` or `.github/workflows/ci.yml` are the one
diff class that deserves the sharpest review, since the gate cannot audit
changes to itself.

## Release workflow

Feature PRs target `main` and use squash merge.
