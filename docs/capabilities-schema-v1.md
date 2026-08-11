# Capabilities schema v1

`agent-quota capabilities --json` reports what this Agent Quota build can
collect before an application selects or probes a provider.

The top-level `schemaVersion` versions this document independently from
`snapshotSchemaVersion`, which identifies the quota snapshot contract.

Each provider reports:

- `probeTransport`: `local_process` or `remote_api`.
- `quotaSource`: the machine-readable interface used to normalize quota.
- `credentialSource`: the local authenticated session read by Agent Quota.
- `submitsMessage`: whether a quota probe submits a provider message.
- `mayAffectQuotaOrBilling`: whether probing may consume quota or incur cost.
- `defaultCacheTtlSeconds`: the default cache lifetime for successful results.
- `probeImpact`: concise explanatory text for people.

Consumers should use the boolean and enum fields for decisions and display
`probeImpact` as explanatory text. Unknown capability schema versions must be
rejected rather than guessed. New optional fields may be added within schema
v1; existing field meanings will not change.

Running `capabilities` does not read credentials, start provider processes, or
send provider API requests.
