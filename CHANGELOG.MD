# Changelog

All notable changes to this project are documented in this file.

## [1.0.5] - 2026-03-08

### Added

- Added fallback current-track quick-fix actions that offer the 3 most recent release targets between the installed version and the track head.
- Added release-age context to fallback quick-fix labels using `DD/MM/YYYY (x time ago)`.

### Changed

- Standardized release-date formatting to `DD/MM/YYYY (x time ago)` across diagnostics, track-switch actions, and hover track details.
- Fixed hover track rendering so the current/latest track now shows release date and relative age when available.
- Changelog sections now append release date + age per entry when npm publish timestamps can be resolved for the version.
- Track fallback selection now respects channel family for prerelease tracks (for example beta-only fallbacks on beta tracks).
- Updated the preferred quick-fix title for the `latest` track from `"{TYPE}: Update {package} to X.X.X (latest)"` to `"{TYPE}: Update {package} to latest X.X.X ({time ago})"`.
- Added `current_track_release_date` to version-status data so code actions and diagnostics can show release-age text for the active track (including `latest`) reliably.
- Reduced inline hint noise to keep update hints more minimal.
- Improved prerelease handling so experimental/canary-style signals are hidden unless the dependency is already on that track.
- Refined track-aware messaging and update surfaces in the LSP (including parser/registry/lsp coordination updates).

## [1.0.4] - 2026-02-08

### Changed

- Added dependency-check caching and refetching only for changed dependencies.
- Adjusted nightly-channel treatment to avoid surfacing it as informational underline noise.

### Maintenance

- Version bump and release metadata updates (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`).

## [1.0.3] - 2026-02-03

### Changed

- Expanded update detection to surface newer versions across tracks (not only the current track).

### Maintenance

- Version bump and release metadata updates (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`).

## [1.0.2] - 2026-01-31

### Added

- Support for monorepo changelog discovery.
- Support for multiple npm release tracks in version/status resolution.

### Maintenance

- Version bump and release metadata updates (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`).

## [1.0.1] - 2026-01-06

### Changed

- Show changelog content even when a dependency is already up to date.
- Naming cleanup and UX polish in extension/LSP surfaces.
- Release workflow and packaging updates.

## [1.0.0] - 2026-01-05

### Changed

- Performance improvements for dependency checking.
- Initial stable 1.0 release line.
