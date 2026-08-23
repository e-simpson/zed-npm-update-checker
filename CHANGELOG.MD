# Changelog

## [1.0.7] - 2026-08-23

### Added

- Progressive batched dependency and changelog updates, with cancellation of stale work
- Configurable loading hints (`show_loading_hints`, enabled by default) and changelog concurrency (`max_concurrent_changelog_requests`, default `4`)

### Changed

- Corrected `recent_releases_in_code_actions` default to `2`
- Improved dependency caching and gzip handling
- Dev-extension installs now prefer the locally built root LSP binary, while downloaded installs retain an offline fallback and otherwise activate the newest release immediately
- Updated dependencies and added reproducible, locked-build CI and release verification

## [1.0.6] - 2026-03-09

### Changed

- Settings are now applied via standard LSP configuration flow (`initialize.initializationOptions` and runtime `workspace/didChangeConfiguration`)

### Fixed

- Restored Zed `settings.json` application by forwarding extension `lsp.<server>.initialization_options` and `lsp.<server>.settings` through the extension API's LSP initialization/workspace configuration hooks

## [1.0.5] - 2026-03-08

### Added

- User-configurable LSP settings via Zed `settings.json` (`lsp.npm-package-json-checker-lsp.settings`) for registry URL, cache TTL, concurrency, timeout, fallback release action count, date tag mode/format, and experimental-track hint visibility

### Changed

- Settings are now reliably applied on startup by forwarding Zed LSP settings from the extension host to the server process
- Date display is now consistently configurable across code actions, hover track metadata, and changelog headers
- Current-track fallback quick-fix actions are configurable (default `2`, `0` disables fallback targets)
- Experimental-track inline diagnostics are now concise (for example `CANARY → x.y.z, BETA → x.y.z`)

## [1.0.4] - 2026-02-08

### Changed

- Added dependency-check caching and refetching only for changed dependencies
- Adjusted nightly-channel treatment to avoid surfacing it as informational underline noise

### Maintenance

- Version bump and release metadata updates (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`)

## [1.0.3] - 2026-02-03

### Changed

- Expanded update detection to surface newer versions across tracks (not only the current track)

### Maintenance

- Version bump and release metadata updates (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`)

## [1.0.2] - 2026-01-31

### Added

- Support for monorepo changelog discovery
- Support for multiple npm release tracks in version/status resolution

### Maintenance

- Version bump and release metadata updates (`Cargo.toml`, `lsp/Cargo.toml`, `extension.toml`)

## [1.0.1] - 2026-01-06

### Changed

- Show changelog content even when a dependency is already up to date
- Naming cleanup and UX polish in extension/LSP surfaces
- Release workflow and packaging updates

## [1.0.0] - 2026-01-05

### Changed

- Performance improvements for dependency checking
- Initial stable 1.0 release line
