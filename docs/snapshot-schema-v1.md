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
- `collection.cachedAtMs` and `collection.expiresAtMs` are Unix epoch
  milliseconds.
- `collection.probeDurationMs` is a duration in milliseconds.
- `billableUsage.resetsAtEpochSeconds` is Unix epoch seconds.

## Billable usage and credits

The optional `billableUsage` object contains provider-formatted `used` and
`limit` amounts, a normalized `remainingPercent`, and its reset time. Agent
Quota deliberately preserves amount strings because providers may use different
currencies or units. A zero remaining percentage or an explicit provider spend
control signal makes `quotaState` exhausted.

The optional `credits` object reports `hasCredits`, `unlimited`, and a
provider-formatted `balance` when supplied. The optional
`rateLimitResetCredits.availableCount` is a separate count of credits that can
reset a provider rate limit; it is not a monetary balance. Agent Quota reads
these fields but never redeems a reset credit or initiates a purchase.

## Collection metadata

The optional `collection` object describes how Agent Quota obtained this copy
of a snapshot without changing the provider quota decision:

- `freshness` is `live` when the collection call probed the provider and
  `cached` when it reused a successful profile result.
- `probeDurationMs` measures the original provider probe, including on cached
  copies.
- `cachedAtMs` and `expiresAtMs` are present when a result participates in a
  `ProviderUsageCache`.

Consumers written before v0.5.0 can ignore this optional object. Use
`observedAtMs`, not the cache timestamps, as the provider observation time.

## Compatibility

Consumers must reject an unsupported `schemaVersion` rather than guessing. New
optional fields may be added within schema v1. Removing fields, changing time
units, changing enum meanings, or renaming serialized fields requires a new
schema version.
