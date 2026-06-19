# Agent Quota

One-glance quota status for Codex, Claude Code, and local AI coding agents.

Agent Quota is a local-first Rust library and CLI for checking which AI coding
subscription still has room before you start another agent run. It is designed
for two use cases:

- standalone CLI or desktop app for developers who use multiple AI coding tools;
- embeddable quota component for tools like Janus, Tauri apps, Electron apps,
  status bars, and internal developer dashboards.

## Why this exists

Usage analytics tools can tell you what you spent. Agent Quota focuses on the
operational question you ask before starting work:

> Which coding agent can I still use right now, and when does it reset?

## Current provider probes

- Codex: starts `codex app-server --listen stdio://` and reads account/rate-limit
  data through the local CLI process.
- Claude Code: reads local Claude Code OAuth credentials and makes a minimal
  Anthropic Messages API request to inspect rate-limit headers.

Agent Quota does not store provider API keys. Probes use local credentials in
memory and normalize provider-specific responses into a small JSON model.

## CLI

```bash
agent-quota status
agent-quota status --json
agent-quota status --provider codex
agent-quota watch --interval 60
```

Example JSON shape:

```json
[
  {
    "providerId": "codex",
    "providerName": "Codex",
    "status": "available",
    "plan": "Plus",
    "windows": [
      {
        "kind": "five_hour",
        "label": "5h window",
        "usedPercent": 42,
        "remainingPercent": 58,
        "windowMinutes": 300,
        "resetsAt": 1730947200,
        "detail": null
      }
    ],
    "updatedAt": 1730000000000,
    "message": null
  }
]
```

## Library

```rust
use agent_quota_core::{collect_usage, CollectUsageOptions};

let snapshots = collect_usage(CollectUsageOptions::all()).await;
```

## Status

Early extraction from Janus. The API is intentionally small, but provider probes
are best-effort and may need updates when CLIs, credential files, or response
headers change.

## Not official

Agent Quota is not an official OpenAI, Anthropic, Codex, or Claude Code product.
It relies on local CLI behavior and provider-exposed/local data where available.
