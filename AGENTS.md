# AGENTS.md

## Documentation Update Rule

When a pull request or commit introduces significant behavior, UX, performance, or compatibility changes:

1. Update `CHANGELOG.MD`.
2. Update `README.md` if the change affects user understanding.

Do not merge significant changes without checking/updating both files.

## CHANGELOG.MD Guidelines

- Add entries in reverse chronological order (newest version at the top).
- Use a version heading format: `## [x.y.z] - YYYY-MM-DD`.
- Summarize only user-relevant changes.
- Prefer clear categories when useful: `Added`, `Changed`, `Fixed`, `Docs`, `Maintenance`.
- Keep each bullet concrete and short.

## README.md Guidelines

Update README when applicable:

- Feature list and screenshots.
- Behavior descriptions (especially quick-fix titles, track handling, and changelog display).
- Setup/build/release instructions.
- Known limitations or caveats.

## Release Hygiene

For version bumps:

1. Ensure `CHANGELOG.MD` has the new version entry.
2. Ensure README reflects the current behavior.
3. Keep version numbers aligned across project manifests.
