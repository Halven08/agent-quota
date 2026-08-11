# agent-quota-core

`agent-quota-core` is the embeddable library behind
[Agent Quota](https://github.com/Halven08/agent-quota). It normalizes Codex and
Claude Code quota information into a versioned snapshot model that keeps probe
health separate from remaining quota. Use `ProviderUsageSnapshot::is_ready()` for
the safe positive signal that a probe succeeded and reported available quota.

See the [project README](https://github.com/Halven08/agent-quota#readme) for
privacy implications, provider prerequisites, examples, and compatibility
guidance.

This crate is an early `0.x` API published on crates.io and from tagged GitHub
releases. Pin an exact version and review the changelog before upgrading.
