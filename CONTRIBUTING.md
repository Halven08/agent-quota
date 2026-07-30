# Contributing

Thank you for helping improve Agent Quota.

## Before opening a change

- Search existing issues and explain the user problem, not only the proposed
  implementation.
- Never include credential files, OAuth tokens, provider request headers, or
  unredacted account identifiers.
- Provider behavior can vary by account plan and CLI version. Include the
  operating system and provider CLI version when reporting incompatibilities.

## Development

Install Rust 1.88 with the `rustfmt` and `clippy` components, then run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo package -p agent-quota-core --locked
```

Tests must not contact production provider endpoints. Use
`AgentQuotaClient::with_claude_api_url` and local test servers. Process tests
must use an explicit mock executable.

## Compatibility expectations

- Add or update tests for every provider response shape.
- Update `crates/agent-quota-core/fixtures/provider-usage-v1.json` only when
  making a compatible schema change.
- A field removal, rename, unit change, or enum meaning change requires a new
  snapshot schema version.
- User-facing messages may change; consumers should branch on typed status and
  error codes.

## Pull requests

Keep changes focused, update the changelog, describe provider impact, and state
which checks were run. Maintainers may ask for redacted fixtures but will never
ask for real credentials.
