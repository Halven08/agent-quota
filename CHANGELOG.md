# Changelog

## v0.2.0 - 2026-06-29

### Added

- Multi-account profile model for Codex and Claude Code quota checks.
- TOML config loading through `AgentQuotaConfig`.
- CLI support for `--config`, `--profile`, and `profiles list`.
- Example multi-account config in `agent-quota.example.toml`.
- CI smoke test for profile config parsing and CLI JSON output.

### Changed

- `agent-quota-core` snapshots now include profile metadata, account label, and data source fields.
- CI now runs with `--locked` and verifies `agent-quota-core` packaging before release.
