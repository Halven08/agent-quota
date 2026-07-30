# Security policy

## Supported versions

Security fixes are applied to the latest tagged `0.x` release. Because provider
interfaces evolve quickly, older minor releases are not supported.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting feature for this
repository. If it is unavailable, contact the repository owner privately
through the contact method on their GitHub profile.

Do not open a public issue for vulnerabilities involving credential exposure,
command execution, request authentication, or unsafe provider interactions.
Reports should include affected versions, impact, reproduction steps, and a
suggested mitigation when possible.

Never send real OAuth tokens, API keys, or credential files. Use synthetic
values and redact account identifiers and authorization headers.

## Security boundaries

Agent Quota:

- reads local provider credentials into memory;
- starts a locally installed Codex executable;
- sends a fixed minimal request to Anthropic for Claude quota headers;
- accepts executable paths and environment overrides from user-controlled
  configuration.

Configuration files must therefore be treated as trusted local input. Do not
run Agent Quota with a configuration received from an untrusted source.
