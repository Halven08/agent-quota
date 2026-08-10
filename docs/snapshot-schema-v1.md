# Snapshot schema v1

The canonical example is
[`provider-usage-v1.json`](../crates/agent-quota-core/fixtures/provider-usage-v1.json).
Agent Quota and downstream consumers can use this file as a contract fixture.

## Decision fields

- `probeStatus` describes whether quota information was collected reliably.
- `quotaState` is `available`, `exhausted`, or `unknown`.
- Route new work only when both values are `ok` and `available`.
- The CLI `check` command and Rust `is_ready()` helper apply exactly this rule.
  Neither adds fields to or changes snapshot schema v1.
- `error.code` is stable for automation. `error.message` is intended for people
  and may be reworded in compatible releases.

## Time units

- `observedAtMs` is Unix epoch milliseconds.
- `resetsAtEpochSeconds` is Unix epoch seconds.

## Compatibility

Consumers must reject an unsupported `schemaVersion` rather than guessing. New
optional fields may be added within schema v1. Removing fields, changing time
units, changing enum meanings, or renaming serialized fields requires a new
schema version.
