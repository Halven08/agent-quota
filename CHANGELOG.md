# Changelog

All notable changes are documented here. The project follows semantic
versioning conventions appropriate for a pre-1.0 Rust crate.

## v0.5.0 - 2026-08-11

### Added

- Add `agent-quota capabilities` with versioned human and JSON provider
  discovery, including probe transport, credential source, message submission,
  billing/quota impact, and default cache lifetime.
- Add versioned JSON documents for `doctor --json` and `check --json`.
- Add `ReadinessPolicy` and `ReadinessSummary` to the core library for typed
  `any`/`all` evaluation with ready, failed, and exhausted profile counts.
- Add optional `collection` metadata to snapshot schema v1 with live/cached
  freshness, original probe duration, and cache insertion/expiration times.
- Add optional billable usage, provider credit balance, and available
  rate-limit reset-credit fields to snapshot schema v1.
- Add internal provider adapter modules and contract tests so new providers can
  be introduced behind a consistent boundary.

### Changed

- Redesign human quota output with an interactive probe spinner, proportional
  usage bars, explicit used and remaining percentages, and one readable block
  per provider profile. JSON and redirected output remain animation-free.
- Treat an exhausted provider spend control as exhausted quota for readiness.
- Source doctor impact text from the same capability contract exposed to
  integrations.
- Make `check --json` return snapshots together with the evaluated readiness
  summary instead of requiring callers to infer the decision from the exit code.
- Bump `agent-quota-core` and the CLI to `0.5.0` while preserving snapshot
  schema v1 and all established exit-code meanings.

## v0.4.1 - 2026-08-11

### Added

- Publish `agent-quota` and `agent-quota-core` through crates.io for standard
  Cargo installation and hosted API documentation.
- Add Cargo Binstall metadata for the existing Linux, Windows, and macOS
  release archives without using the third-party QuickInstall fallback.
- Add a compact terminal demo to make the primary workflow immediately visible.

### Changed

- Update install and library guidance for registry-based distribution.
- Verify the core crate archive in CI; publish and verify the CLI after the
  matching core version reaches crates.io.
- Bump `agent-quota-core` and the CLI to `0.4.1` without changing snapshot
  schema v1 or readiness behavior.

## v0.4.0 - 2026-08-10

### Added

- Add `agent-quota check` as a script-friendly readiness gate with `any` and
  `all` profile policies.
- Add `ProviderUsageSnapshot::is_ready()` for the safe positive routing signal.
- Add `agent-quota --version` and a dedicated readiness exit code.

### Changed

- Refresh the locked serde, serde_json, and Tokio dependencies.
- Upgrade the TOML parser from 0.8 to 1.1 while preserving strict config validation.
- Refresh the locked time dependency to 0.3.55.
- Bump `agent-quota-core` and the CLI to `0.4.0`.
- Preserve snapshot schema v1; `check --json` emits the existing snapshot shape.
- Improve public onboarding, install guidance, project presentation, and
  community contribution paths.
- Remove unpublished docs.rs metadata until hosted API documentation is
  available.
- Describe the compatibility fixture in implementation-neutral terms.

## v0.3.0 - 2026-07-30

### Added

- Separate probe health and quota availability states with typed failure codes.
- Versioned snapshot schema, explicit timestamp units, and a shared downstream
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
