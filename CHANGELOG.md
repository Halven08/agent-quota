# Changelog

All notable changes are documented here. The project follows semantic
versioning conventions appropriate for a pre-1.0 Rust crate.

## Unreleased

## v0.3.0 - 2026-07-30

### Added

- Separate probe health and quota availability states with typed failure codes.
- Versioned snapshot schema, explicit timestamp units, and a shared Janus
  compatibility fixture.
- Selection-keyed five-minute cache that skips completely failed results.
- Injectable HTTP client, Claude endpoint, and provider timeouts.
- `agent-quota doctor`, local/relative reset times, documented exit codes, and
  NDJSON watch output.
- Production-path HTTP and CLI integration tests.
- Contribution, security, dependency maintenance, audit, and binary release
  automation.

### Changed

- Validate duplicate/empty profile IDs, unknown TOML fields, and
  provider-incompatible profile fields.
- Reject watch intervals below 60 seconds.
- Inspect Codex JSON-RPC errors and Claude HTTP status before accepting quota
  data.
- Update the locked `quinn-proto` dependency to `0.11.15` to address
  RUSTSEC-2026-0185.
- Treat an explicit empty provider/profile selection as no probes.
- Bump `agent-quota-core` and the CLI to `0.3.0`.

### Removed

- Ambiguous `ProviderUsageStatus` and test-only Claude CLI output parser.
- Public mutable provider/profile vectors from `CollectUsageOptions`.

### Migration

- Replace `snapshot.status` with `snapshot.probe_status` and
  `snapshot.quota_state`.
- `provider_id` is now `ProviderId`, profile identifiers/names are non-optional,
  `updatedAt` is now `observedAtMs`, and `resetsAt` is now
  `resetsAtEpochSeconds`.
- Replace `ProviderUsageCache::get_or_refresh` with
  `ProviderUsageCache::collect`.

## v0.2.0 - 2026-06-29

### Added

- Multi-account profile model for Codex and Claude Code quota checks.
- TOML config loading through `AgentQuotaConfig`.
- CLI support for `--config`, `--profile`, and `profiles list`.
- Example multi-account configuration.
- CI smoke test for profile parsing and CLI JSON output.

### Changed

- Snapshots include profile metadata, account labels, and data source fields.
- CI uses locked dependencies and verifies core crate packaging.

## v0.1.1 - 2026-06-19

### Changed

- Polished documentation and public-preview packaging.

## v0.1.0 - 2026-06-19

### Added

- Initial Codex and Claude Code probes.
- Normalized quota window model, Rust library, CLI, and cross-platform CI.
