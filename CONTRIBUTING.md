# Contributing

Contributions are welcome. This document explains the workflow.

## Reporting bugs or requesting features

Open an [issue](https://github.com/dnacenta/pulse-null/issues). Use a clear title and include enough context to reproduce the problem or understand the request.

## Making changes

1. Fork the repo
2. Create a branch from `development` (see naming below)
3. Make your changes
4. Open a PR targeting `development`

`main` is protected. All changes go through `development` first.

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

- Run `cargo fmt` before submitting
- Run `cargo clippy` -- no warnings (dead_code from unused trait methods is acceptable during early development)
- Run `cargo test` to make sure nothing breaks
- Keep changes focused -- one issue per PR

## Release workflow

Releases go from `development` to `main` via merge commit (not squash). Feature PRs to `development` use squash merge.
