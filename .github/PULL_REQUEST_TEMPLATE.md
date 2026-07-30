## Summary

Describe the user problem and the resulting behavior.

## Provider and privacy impact

State whether the change starts local processes, reads credentials, makes
provider requests, or changes quota/billing impact.

## Compatibility

State whether the Rust API, JSON schema, fixture, CLI output, or exit codes
change.

## Validation

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo package -p agent-quota-core --locked`
- [ ] Tests use synthetic credentials and do not contact production providers
