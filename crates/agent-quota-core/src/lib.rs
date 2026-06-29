//! Local-first quota probes for AI coding CLIs.
//!
//! The crate intentionally stays behind each provider's local CLI or local
//! credentials. It does not store provider API keys. Probes use local credentials
//! in memory only, then normalize provider-specific responses into a small
//! embeddable model.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, process::Stdio};

use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

const CODEX_TIMEOUT: Duration = Duration::from_secs(10);
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(20);
const PROVIDER_USAGE_CACHE_TTL_MS: i64 = 5 * 60 * 1000;
const CLAUDE_USAGE_MODEL: &str = "claude-haiku-4-5-20251001";
const CLAUDE_CREDENTIALS_RELATIVE_PATH: &str = ".claude/.credentials.json";

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

type UsageResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    Codex,
    Claude,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" | "openai-codex" => Some(Self::Codex),
            "claude" | "claude-code" | "anthropic" => Some(Self::Claude),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProviderProfile {
    pub id: String,
    pub provider: ProviderId,
    pub label: Option<String>,
    pub command_path: Option<PathBuf>,
    pub credentials_path: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl ProviderProfile {
    pub fn default_for_provider(provider: ProviderId) -> Self {
        Self {
            id: provider.as_str().to_owned(),
            provider,
            label: None,
            command_path: None,
            credentials_path: None,
            env: BTreeMap::new(),
            enabled: true,
        }
    }

    pub fn display_name(&self) -> String {
        self.label
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.provider.label().to_owned())
    }
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentQuotaConfig {
    #[serde(default)]
    pub profiles: Vec<ProviderProfile>,
}

impl AgentQuotaConfig {
    pub fn load(path: impl AsRef<Path>) -> UsageResult<Self> {
        let raw = fs::read_to_string(path.as_ref())?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn profiles(&self) -> Vec<ProviderProfile> {
        self.profiles
            .iter()
            .filter(|profile| profile.enabled)
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct CollectUsageOptions {
    pub providers: Vec<ProviderId>,
    pub profiles: Vec<ProviderProfile>,
}

impl CollectUsageOptions {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn providers(providers: impl IntoIterator<Item = ProviderId>) -> Self {
        Self {
            providers: providers.into_iter().collect(),
            profiles: Vec::new(),
        }
    }

    pub fn profiles(profiles: impl IntoIterator<Item = ProviderProfile>) -> Self {
        Self {
            providers: Vec::new(),
            profiles: profiles.into_iter().collect(),
        }
    }

    fn selected_profiles(&self) -> Vec<ProviderProfile> {
        if !self.profiles.is_empty() {
            self.profiles
                .iter()
                .filter(|profile| profile.enabled)
                .cloned()
                .collect()
        } else if self.providers.is_empty() {
            vec![
                ProviderProfile::default_for_provider(ProviderId::Codex),
                ProviderProfile::default_for_provider(ProviderId::Claude),
            ]
        } else {
            self.providers
                .iter()
                .copied()
                .map(ProviderProfile::default_for_provider)
                .collect()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderUsageCache {
    inner: Arc<Mutex<Option<CachedProviderUsage>>>,
}

#[derive(Debug, Clone)]
struct CachedProviderUsage {
    snapshots: Vec<ProviderUsageSnapshot>,
    cached_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageSnapshot {
    pub provider_id: String,
    pub provider_name: String,
    pub profile_id: Option<String>,
    pub profile_name: Option<String>,
    pub account_label: Option<String>,
    pub source: Option<String>,
    pub status: ProviderUsageStatus,
    pub plan: Option<String>,
    pub windows: Vec<ProviderUsageWindow>,
    pub updated_at: i64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUsageStatus {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageWindow {
    pub kind: ProviderUsageWindowKind,
    pub label: String,
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<i64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUsageWindowKind {
    FiveHour,
    Weekly,
    Other,
}

impl ProviderUsageCache {
    pub async fn get_or_refresh<F, Fut>(
        &self,
        force_refresh: bool,
        fetch: F,
    ) -> Vec<ProviderUsageSnapshot>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Vec<ProviderUsageSnapshot>>,
    {
        let mut guard = self.inner.lock().await;
        let now = now_epoch_ms();
        if !force_refresh {
            if let Some(cached) = guard.as_ref() {
                if now.saturating_sub(cached.cached_at) < PROVIDER_USAGE_CACHE_TTL_MS {
                    return cached.snapshots.clone();
                }
            }
        }

        let snapshots = fetch().await;
        *guard = Some(CachedProviderUsage {
            snapshots: snapshots.clone(),
            cached_at: now_epoch_ms(),
        });
        snapshots
    }
}

pub async fn collect_usage(options: CollectUsageOptions) -> Vec<ProviderUsageSnapshot> {
    let profiles = options.selected_profiles();
    let snapshots = profiles.into_iter().map(collect_profile_usage);
    futures::future::join_all(snapshots).await
}

pub async fn collect_provider_usage() -> Vec<ProviderUsageSnapshot> {
    collect_usage(CollectUsageOptions::all()).await
}

pub async fn collect_single_provider_usage(provider: ProviderId) -> ProviderUsageSnapshot {
    collect_profile_usage(ProviderProfile::default_for_provider(provider)).await
}

pub async fn collect_profile_usage(profile: ProviderProfile) -> ProviderUsageSnapshot {
    match profile.provider {
        ProviderId::Codex => codex_usage(&profile).await,
        ProviderId::Claude => claude_usage(&profile).await,
    }
}

async fn codex_usage(profile: &ProviderProfile) -> ProviderUsageSnapshot {
    match query_codex_usage(profile).await {
        Ok(snapshot) => snapshot,
        Err(error) => unavailable_for_profile(profile, format!("Usage unavailable: {error}")),
    }
}

async fn query_codex_usage(profile: &ProviderProfile) -> UsageResult<ProviderUsageSnapshot> {
    let mut command = Command::new(
        profile
            .command_path
            .as_deref()
            .unwrap_or_else(|| Path::new("codex")),
    );
    command.args(["app-server", "--listen", "stdio://"]);
    command.envs(&profile.env);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("codex app-server stdin was not available"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("codex app-server stdout was not available"))?;

    write_rpc(
        &mut stdin,
        json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "agent_quota",
                    "title": "Agent Quota",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
    )
    .await?;
    write_rpc(
        &mut stdin,
        json!({
            "method": "initialized",
            "params": {}
        }),
    )
    .await?;
    write_rpc(
        &mut stdin,
        json!({
            "method": "account/read",
            "id": 2,
            "params": { "refreshToken": false }
        }),
    )
    .await?;
    write_rpc(
        &mut stdin,
        json!({
            "method": "account/rateLimits/read",
            "id": 3
        }),
    )
    .await?;

    let mut lines = BufReader::new(stdout).lines();
    let read = async {
        let mut account_response = None;
        let mut rate_limits_response = None;

        while account_response.is_none() || rate_limits_response.is_none() {
            let Some(line) = lines.next_line().await? else {
                break;
            };
            let value = serde_json::from_str::<Value>(&line)?;
            match value.get("id").and_then(Value::as_i64) {
                Some(2) => account_response = Some(value),
                Some(3) => rate_limits_response = Some(value),
                _ => {}
            }
        }

        let account_response = account_response
            .ok_or_else(|| std::io::Error::other("codex account response was missing"))?;
        let rate_limits_response = rate_limits_response
            .ok_or_else(|| std::io::Error::other("codex rate-limit response was missing"))?;
        UsageResult::Ok((account_response, rate_limits_response))
    };

    let (account_response, rate_limits_response) = timeout(CODEX_TIMEOUT, read)
        .await
        .map_err(|_| std::io::Error::other("timed out waiting for codex usage"))??;
    let _ = child.kill().await;

    Ok(codex_snapshot_from_responses(
        profile,
        &account_response,
        &rate_limits_response,
    ))
}

async fn write_rpc(stdin: &mut ChildStdin, value: Value) -> std::io::Result<()> {
    stdin.write_all(value.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

fn codex_snapshot_from_responses(
    profile: &ProviderProfile,
    account_response: &Value,
    rate_limits_response: &Value,
) -> ProviderUsageSnapshot {
    let account = account_response.pointer("/result/account");
    let plan = account
        .and_then(|value| value.get("planType"))
        .and_then(Value::as_str)
        .map(format_plan_name);
    let account_label = account
        .and_then(|value| {
            value
                .get("email")
                .or_else(|| value.get("username"))
                .or_else(|| value.get("id"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let rate_limits = rate_limits_response.pointer("/result/rateLimits");
    let mut windows = Vec::new();

    if let Some(primary) = rate_limits.and_then(|value| value.get("primary")) {
        if let Some(window) = window_from_object("Primary window", primary) {
            windows.push(window);
        }
    }
    if let Some(secondary) = rate_limits.and_then(|value| value.get("secondary")) {
        if let Some(window) = window_from_object("Secondary window", secondary) {
            windows.push(window);
        }
    }
    sort_usage_windows(&mut windows);

    let message = rate_limits
        .and_then(|value| value.get("rateLimitReachedType"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| format!("Limit reached: {}", value.replace('_', " ")));

    ProviderUsageSnapshot {
        provider_id: "codex".to_owned(),
        provider_name: "Codex".to_owned(),
        profile_id: Some(profile.id.clone()),
        profile_name: Some(profile.display_name()),
        account_label,
        source: Some("codex_app_server".to_owned()),
        status: ProviderUsageStatus::Available,
        plan,
        windows,
        updated_at: now_epoch_ms(),
        message,
    }
}

async fn claude_usage(profile: &ProviderProfile) -> ProviderUsageSnapshot {
    match query_claude_usage(profile).await {
        Ok(snapshot) => snapshot,
        Err(error) => unavailable_for_profile(profile, format!("Usage unavailable: {error}")),
    }
}

async fn query_claude_usage(profile: &ProviderProfile) -> UsageResult<ProviderUsageSnapshot> {
    let token = read_claude_oauth_token(profile.credentials_path.as_deref())?;
    let client = reqwest::Client::new();
    let request = client
        .post("https://api.anthropic.com/v1/messages")
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&json!({
            "model": CLAUDE_USAGE_MODEL,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "hi" }],
        }));

    let response = timeout(CLAUDE_TIMEOUT, request.send())
        .await
        .map_err(|_| std::io::Error::other("timed out waiting for claude usage headers"))??;
    Ok(claude_snapshot_from_headers(profile, response.headers()))
}

fn read_claude_oauth_token(credentials_path: Option<&Path>) -> UsageResult<String> {
    let path = credentials_path
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(claude_credentials_path)?;
    let raw = fs::read_to_string(&path).map_err(|error| {
        std::io::Error::other(format!(
            "Claude Code credentials were not readable at {}: {error}",
            path.display()
        ))
    })?;
    let value = serde_json::from_str::<Value>(&raw)?;
    find_claude_oauth_token(&value)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "Claude Code OAuth token was not found in {}",
                path.display()
            ))
            .into()
        })
}

fn claude_credentials_path() -> UsageResult<PathBuf> {
    dirs::home_dir()
        .map(|home| home.join(CLAUDE_CREDENTIALS_RELATIVE_PATH))
        .ok_or_else(|| std::io::Error::other("home directory was not available").into())
}

fn find_claude_oauth_token(value: &Value) -> Option<&str> {
    value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            value
                .pointer("/oauth/accessToken")
                .and_then(Value::as_str)
                .filter(|token| !token.trim().is_empty())
        })
        .or_else(|| {
            value
                .pointer("/claudeAiOauth/access_token")
                .and_then(Value::as_str)
                .filter(|token| !token.trim().is_empty())
        })
}

fn claude_snapshot_from_headers(
    profile: &ProviderProfile,
    headers: &HeaderMap,
) -> ProviderUsageSnapshot {
    let mut windows = [
        claude_header_window(
            headers,
            ProviderUsageWindowKind::FiveHour,
            "5h window",
            "anthropic-ratelimit-unified-5h-utilization",
            "anthropic-ratelimit-unified-5h-reset",
        ),
        claude_header_window(
            headers,
            ProviderUsageWindowKind::Weekly,
            "Weekly window",
            "anthropic-ratelimit-unified-7d-utilization",
            "anthropic-ratelimit-unified-7d-reset",
        ),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    sort_usage_windows(&mut windows);

    let message = header_string(headers, "anthropic-ratelimit-unified-status")
        .filter(|status| !status.eq_ignore_ascii_case("active"))
        .map(|status| format!("Claude quota status: {status}"))
        .or_else(|| {
            if windows.is_empty() {
                Some("Claude usage headers were not returned by Anthropic.".to_owned())
            } else {
                Some("Live quota from Anthropic rate-limit headers.".to_owned())
            }
        });

    ProviderUsageSnapshot {
        provider_id: "claude".to_owned(),
        provider_name: "Claude Code".to_owned(),
        profile_id: Some(profile.id.clone()),
        profile_name: Some(profile.display_name()),
        account_label: None,
        source: Some("anthropic_rate_limit_headers".to_owned()),
        status: if windows.is_empty() {
            ProviderUsageStatus::Unavailable
        } else {
            ProviderUsageStatus::Available
        },
        plan: None,
        windows,
        updated_at: now_epoch_ms(),
        message,
    }
}

fn claude_header_window(
    headers: &HeaderMap,
    kind: ProviderUsageWindowKind,
    label: &str,
    utilization_header: &str,
    reset_header: &str,
) -> Option<ProviderUsageWindow> {
    let utilization = header_number(headers, utilization_header)?;
    let used_percent = Some((utilization * 100.0).clamp(0.0, 100.0));
    let remaining_percent = used_percent.map(|used| (100.0 - used).clamp(0.0, 100.0));
    Some(ProviderUsageWindow {
        kind,
        label: label.to_owned(),
        used_percent,
        remaining_percent,
        window_minutes: match kind {
            ProviderUsageWindowKind::FiveHour => Some(300),
            ProviderUsageWindowKind::Weekly => Some(10_080),
            ProviderUsageWindowKind::Other => None,
        },
        resets_at: header_integer(headers, reset_header).map(normalize_epoch_seconds),
        detail: header_string(
            headers,
            match kind {
                ProviderUsageWindowKind::FiveHour => "anthropic-ratelimit-unified-5h-status",
                ProviderUsageWindowKind::Weekly => "anthropic-ratelimit-unified-7d-status",
                ProviderUsageWindowKind::Other => "anthropic-ratelimit-unified-status",
            },
        )
        .map(|status| format!("Status: {status}")),
    })
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<f64> {
    header_string(headers, name).and_then(|value| value.parse::<f64>().ok())
}

fn header_integer(headers: &HeaderMap, name: &str) -> Option<i64> {
    header_string(headers, name).and_then(|value| value.parse::<i64>().ok())
}

#[cfg(test)]
fn claude_snapshot_from_output(output: &str) -> ProviderUsageSnapshot {
    let parsed = serde_json::from_str::<Value>(output).ok();
    let result_text = parsed
        .as_ref()
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        .unwrap_or(output);
    if result_text.to_lowercase().contains("unknown skill: usage") {
        return unavailable(
            "claude",
            "Claude Code",
            "Claude Code CLI does not expose subscription usage here; the agent can still run."
                .to_owned(),
        );
    }
    let mut windows = parsed
        .as_ref()
        .map_or_else(Vec::new, collect_json_usage_windows);
    windows.extend(parse_text_usage_windows(result_text));
    dedupe_windows(&mut windows);

    if windows.is_empty() {
        if let Some(cost_detail) = session_cost_detail(parsed.as_ref(), result_text) {
            windows.push(ProviderUsageWindow {
                kind: ProviderUsageWindowKind::Other,
                label: "Current session".to_owned(),
                used_percent: None,
                remaining_percent: None,
                window_minutes: None,
                resets_at: None,
                detail: Some(cost_detail),
            });
        }
    }
    sort_usage_windows(&mut windows);

    let status = if windows.is_empty() {
        ProviderUsageStatus::Unavailable
    } else {
        ProviderUsageStatus::Available
    };
    let message = if windows.is_empty() {
        Some("Claude Code did not return plan usage details.".to_owned())
    } else if windows
        .iter()
        .all(|window| window.remaining_percent.is_none() && window.used_percent.is_none())
    {
        Some("Session cost is available; plan usage bars were not returned.".to_owned())
    } else {
        None
    };

    ProviderUsageSnapshot {
        provider_id: "claude".to_owned(),
        provider_name: "Claude Code".to_owned(),
        profile_id: None,
        profile_name: None,
        account_label: None,
        source: Some("claude_cli_output".to_owned()),
        status,
        plan: parsed
            .as_ref()
            .and_then(|value| nested_string(value, &["plan", "planType", "subscription"])),
        windows,
        updated_at: now_epoch_ms(),
        message,
    }
}

#[cfg(test)]
fn collect_json_usage_windows(value: &Value) -> Vec<ProviderUsageWindow> {
    let mut windows = Vec::new();
    collect_json_usage_windows_inner(value, None, &mut windows);
    windows
}

#[cfg(test)]
fn collect_json_usage_windows_inner(
    value: &Value,
    label_hint: Option<&str>,
    windows: &mut Vec<ProviderUsageWindow>,
) {
    match value {
        Value::Object(map) => {
            if let Some(window) = window_from_map(label_hint.unwrap_or("Usage window"), map) {
                windows.push(window);
            }
            for (key, child) in map {
                collect_json_usage_windows_inner(child, Some(key), windows);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_json_usage_windows_inner(item, label_hint, windows);
            }
        }
        _ => {}
    }
}

fn window_from_object(label_hint: &str, value: &Value) -> Option<ProviderUsageWindow> {
    value
        .as_object()
        .and_then(|map| window_from_map(label_hint, map))
}

fn window_from_map(label_hint: &str, map: &Map<String, Value>) -> Option<ProviderUsageWindow> {
    let used_percent = number_field(
        map,
        &[
            "usedPercent",
            "used_percent",
            "percentUsed",
            "percent_used",
            "usagePercent",
            "usage_percent",
        ],
    );
    let remaining_percent = number_field(
        map,
        &[
            "remainingPercent",
            "remaining_percent",
            "percentRemaining",
            "percent_remaining",
        ],
    )
    .or_else(|| used_percent.map(|used| (100.0 - used).clamp(0.0, 100.0)));

    if used_percent.is_none() && remaining_percent.is_none() {
        return None;
    }

    let window_minutes = number_field(
        map,
        &[
            "windowDurationMins",
            "window_duration_mins",
            "windowMinutes",
            "window_minutes",
        ],
    )
    .and_then(f64_to_u64)
    .or_else(|| {
        number_field(map, &["limitWindowSeconds", "limit_window_seconds"])
            .and_then(|seconds| f64_to_u64(seconds / 60.0))
    });
    let resets_at = integer_field(
        map,
        &["resetsAt", "resetAt", "reset_at", "resets_at", "resetTime"],
    )
    .map(normalize_epoch_seconds);
    let kind = kind_for_usage_window(label_hint, window_minutes);
    let label = label_for_usage_window(label_hint, window_minutes, kind);

    Some(ProviderUsageWindow {
        kind,
        label,
        used_percent,
        remaining_percent,
        window_minutes,
        resets_at,
        detail: None,
    })
}

#[cfg(test)]
fn parse_text_usage_windows(text: &str) -> Vec<ProviderUsageWindow> {
    let mut windows = Vec::new();
    for line in text.lines() {
        let lower = line.to_lowercase();
        if !line.contains('%') || lower.contains("context") {
            continue;
        }

        let Some((percent, _position)) = first_percentage(line) else {
            continue;
        };
        let remaining_percent = if lower.contains("left") || lower.contains("remaining") {
            Some(percent)
        } else {
            Some((100.0 - percent).clamp(0.0, 100.0))
        };
        let used_percent = if lower.contains("left") || lower.contains("remaining") {
            Some((100.0 - percent).clamp(0.0, 100.0))
        } else {
            Some(percent)
        };

        let kind = kind_from_text_line(&lower);
        windows.push(ProviderUsageWindow {
            kind,
            label: label_from_text_line(line, kind),
            used_percent,
            remaining_percent,
            window_minutes: window_minutes_from_text(&lower),
            resets_at: None,
            detail: Some(line.trim().to_owned()),
        });
    }
    windows
}

#[cfg(test)]
fn first_percentage(line: &str) -> Option<(f64, usize)> {
    let percent_position = line.find('%')?;
    let before_percent = &line[..percent_position];
    let start = before_percent
        .char_indices()
        .rev()
        .find_map(|(index, ch)| {
            if ch.is_ascii_digit() || ch == '.' || ch.is_ascii_whitespace() {
                None
            } else {
                Some(index + ch.len_utf8())
            }
        })
        .unwrap_or(0);
    let raw = before_percent[start..].trim();
    raw.parse::<f64>()
        .ok()
        .map(|value| (value, percent_position))
}

#[cfg(test)]
fn kind_from_text_line(line: &str) -> ProviderUsageWindowKind {
    if line.contains("5-hour") || line.contains("5 hour") || line.contains("5h") {
        ProviderUsageWindowKind::FiveHour
    } else if line.contains("weekly") || line.contains("7-day") || line.contains("7 day") {
        ProviderUsageWindowKind::Weekly
    } else {
        ProviderUsageWindowKind::Other
    }
}

#[cfg(test)]
fn label_from_text_line(line: &str, kind: ProviderUsageWindowKind) -> String {
    let lower = line.to_lowercase();
    match kind {
        ProviderUsageWindowKind::FiveHour => return "5h window".to_owned(),
        ProviderUsageWindowKind::Weekly => return "Weekly window".to_owned(),
        ProviderUsageWindowKind::Other => {}
    }

    if lower.contains("monthly") {
        "Monthly window".to_owned()
    } else if lower.contains("opus") {
        "Opus".to_owned()
    } else if lower.contains("sonnet") {
        "Sonnet".to_owned()
    } else {
        "Plan usage".to_owned()
    }
}

#[cfg(test)]
fn window_minutes_from_text(line: &str) -> Option<u64> {
    if line.contains("5-hour") || line.contains("5 hour") || line.contains("5h") {
        Some(300)
    } else if line.contains("weekly") || line.contains("7-day") || line.contains("7 day") {
        Some(10_080)
    } else if line.contains("monthly") {
        Some(43_200)
    } else {
        None
    }
}

#[cfg(test)]
fn dedupe_windows(windows: &mut Vec<ProviderUsageWindow>) {
    let mut unique = Vec::new();
    for window in std::mem::take(windows) {
        if unique.iter().any(|candidate: &ProviderUsageWindow| {
            if window.kind == ProviderUsageWindowKind::Other {
                candidate.kind == window.kind && candidate.label == window.label
            } else {
                candidate.kind == window.kind
            }
        }) {
            continue;
        }
        unique.push(window);
    }
    *windows = unique;
}

fn sort_usage_windows(windows: &mut [ProviderUsageWindow]) {
    windows.sort_by(|left, right| {
        window_order(left)
            .cmp(&window_order(right))
            .then_with(|| left.label.cmp(&right.label))
    });
}

fn window_order(window: &ProviderUsageWindow) -> u8 {
    match window.kind {
        ProviderUsageWindowKind::FiveHour => 0,
        ProviderUsageWindowKind::Weekly => 1,
        ProviderUsageWindowKind::Other => 2,
    }
}

#[cfg(test)]
fn session_cost_detail(parsed: Option<&Value>, text: &str) -> Option<String> {
    parsed
        .and_then(|value| number_field_from_value(value, &["total_cost_usd"]))
        .map(|cost| format!("Total cost: ${cost:.2}"))
        .or_else(|| {
            text.lines()
                .map(str::trim)
                .find(|line| line.to_lowercase().starts_with("total cost:"))
                .map(str::to_owned)
        })
}

fn number_field(map: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(number_value))
}

fn integer_field(map: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(integer_value))
}

#[cfg(test)]
fn number_field_from_value(value: &Value, keys: &[&str]) -> Option<f64> {
    match value {
        Value::Object(map) => number_field(map, keys).or_else(|| {
            map.values()
                .find_map(|child| number_field_from_value(child, keys))
        }),
        Value::Array(items) => items
            .iter()
            .find_map(|child| number_field_from_value(child, keys)),
        _ => None,
    }
}

fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<f64>().ok()))
}

fn integer_value(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<i64>().ok()))
}

fn f64_to_u64(value: f64) -> Option<u64> {
    if value.is_finite() && value >= 0.0 {
        format!("{:.0}", value.round()).parse::<u64>().ok()
    } else {
        None
    }
}

fn normalize_epoch_seconds(value: i64) -> i64 {
    if value > 10_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn kind_for_usage_window(label_hint: &str, window_minutes: Option<u64>) -> ProviderUsageWindowKind {
    let label_hint = label_hint.to_lowercase();
    match window_minutes {
        Some(300) => ProviderUsageWindowKind::FiveHour,
        Some(10_080) => ProviderUsageWindowKind::Weekly,
        _ if label_hint.contains("five")
            || label_hint.contains("5h")
            || label_hint.contains("5 hour")
            || label_hint.contains("5-hour") =>
        {
            ProviderUsageWindowKind::FiveHour
        }
        _ if label_hint.contains("weekly")
            || label_hint.contains("week")
            || label_hint.contains("7 day")
            || label_hint.contains("7-day") =>
        {
            ProviderUsageWindowKind::Weekly
        }
        _ => ProviderUsageWindowKind::Other,
    }
}

fn label_for_usage_window(
    label_hint: &str,
    window_minutes: Option<u64>,
    kind: ProviderUsageWindowKind,
) -> String {
    match kind {
        ProviderUsageWindowKind::FiveHour => "5h window".to_owned(),
        ProviderUsageWindowKind::Weekly => "Weekly window".to_owned(),
        ProviderUsageWindowKind::Other => match window_minutes {
            Some(minutes) if minutes >= 60 && minutes % 60 == 0 => {
                format!("{}h window", minutes / 60)
            }
            Some(minutes) => format!("{minutes}m window"),
            None => humanize_label(label_hint),
        },
    }
}

fn humanize_label(value: &str) -> String {
    let mut label = String::new();
    let mut previous_lowercase = false;
    for ch in value.chars() {
        if ch == '_' || ch == '-' {
            label.push(' ');
            previous_lowercase = false;
        } else if ch.is_ascii_uppercase() && previous_lowercase {
            label.push(' ');
            label.push(ch);
            previous_lowercase = false;
        } else {
            label.push(ch);
            previous_lowercase = ch.is_ascii_lowercase();
        }
    }
    let label = label.trim();
    if label.eq_ignore_ascii_case("primary") {
        "Primary window".to_owned()
    } else if label.eq_ignore_ascii_case("secondary") {
        "Secondary window".to_owned()
    } else {
        let mut chars = label.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => "Usage window".to_owned(),
        }
    }
}

#[cfg(test)]
fn nested_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(found) = map.get(*key).and_then(Value::as_str) {
                    return Some(format_plan_name(found));
                }
            }
            map.values().find_map(|child| nested_string(child, keys))
        }
        Value::Array(items) => items.iter().find_map(|child| nested_string(child, keys)),
        _ => None,
    }
}

fn format_plan_name(value: &str) -> String {
    let words = value
        .replace(['_', '-'], " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>();
    words.join(" ")
}

#[cfg(test)]
fn unavailable(provider_id: &str, provider_name: &str, message: String) -> ProviderUsageSnapshot {
    ProviderUsageSnapshot {
        provider_id: provider_id.to_owned(),
        provider_name: provider_name.to_owned(),
        profile_id: None,
        profile_name: None,
        account_label: None,
        source: None,
        status: ProviderUsageStatus::Unavailable,
        plan: None,
        windows: Vec::new(),
        updated_at: now_epoch_ms(),
        message: Some(message),
    }
}

fn unavailable_for_profile(profile: &ProviderProfile, message: String) -> ProviderUsageSnapshot {
    ProviderUsageSnapshot {
        provider_id: profile.provider.as_str().to_owned(),
        provider_name: profile.provider.label().to_owned(),
        profile_id: Some(profile.id.clone()),
        profile_name: Some(profile.display_name()),
        account_label: None,
        source: None,
        status: ProviderUsageStatus::Unavailable,
        plan: None,
        windows: Vec::new(),
        updated_at: now_epoch_ms(),
        message: Some(message),
    }
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    fn header_map(entries: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        headers
    }

    #[test]
    fn parses_profile_config() {
        let config = toml::from_str::<AgentQuotaConfig>(
            r#"
                [[profiles]]
                id = "claude-work"
                provider = "claude"
                label = "Claude Work"
                credentials_path = "C:/Users/Maciej/.claude-work/.credentials.json"

                [[profiles]]
                id = "codex-private"
                provider = "codex"
                label = "Codex Private"
                command_path = "codex"

                [profiles.env]
                CODEX_HOME = "C:/Users/Maciej/.codex-private"
            "#,
        )
        .expect("profile config should parse");

        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles[0].provider, ProviderId::Claude);
        assert_eq!(
            config.profiles[0].credentials_path.as_deref(),
            Some(Path::new("C:/Users/Maciej/.claude-work/.credentials.json"))
        );
        assert_eq!(
            config.profiles[1].env.get("CODEX_HOME").map(String::as_str),
            Some("C:/Users/Maciej/.codex-private")
        );
    }

    #[test]
    fn collect_options_prefers_enabled_profiles() {
        let enabled = ProviderProfile {
            id: "claude-work".to_owned(),
            provider: ProviderId::Claude,
            ..ProviderProfile::default_for_provider(ProviderId::Claude)
        };
        let disabled = ProviderProfile {
            id: "codex-disabled".to_owned(),
            provider: ProviderId::Codex,
            enabled: false,
            ..ProviderProfile::default_for_provider(ProviderId::Codex)
        };

        let options = CollectUsageOptions::profiles([enabled, disabled]);
        let profiles = options.selected_profiles();

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "claude-work");
    }

    #[test]
    fn maps_codex_app_server_rate_limits() {
        let account = json!({
            "id": 2,
            "result": {
                "account": { "type": "chatgpt", "email": "user@example.com", "planType": "plus" }
            }
        });
        let rate_limits = json!({
            "id": 3,
            "result": {
                "rateLimits": {
                    "primary": { "usedPercent": 25, "windowDurationMins": 300, "resetsAt": 1_730_947_200 },
                    "secondary": { "usedPercent": 46, "windowDurationMins": 10080, "resetsAt": 1_731_206_400 },
                    "rateLimitReachedType": null
                }
            }
        });

        let profile = ProviderProfile {
            id: "codex-private".to_owned(),
            provider: ProviderId::Codex,
            label: Some("Codex Private".to_owned()),
            ..ProviderProfile::default_for_provider(ProviderId::Codex)
        };
        let snapshot = codex_snapshot_from_responses(&profile, &account, &rate_limits);

        assert_eq!(snapshot.provider_id, "codex");
        assert_eq!(snapshot.profile_id.as_deref(), Some("codex-private"));
        assert_eq!(snapshot.profile_name.as_deref(), Some("Codex Private"));
        assert_eq!(snapshot.account_label.as_deref(), Some("user@example.com"));
        assert_eq!(snapshot.plan.as_deref(), Some("Plus"));
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].kind, ProviderUsageWindowKind::FiveHour);
        assert_eq!(snapshot.windows[0].label, "5h window");
        assert_eq!(snapshot.windows[0].remaining_percent, Some(75.0));
        assert_eq!(snapshot.windows[1].kind, ProviderUsageWindowKind::Weekly);
        assert_eq!(snapshot.windows[1].label, "Weekly window");
        assert_eq!(snapshot.windows[1].remaining_percent, Some(54.0));
    }

    #[test]
    fn parses_claude_oauth_rate_limit_headers() {
        let headers = header_map(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "1730947200"),
            ("anthropic-ratelimit-unified-5h-status", "active"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.68"),
            ("anthropic-ratelimit-unified-7d-reset", "1731206400000"),
            ("anthropic-ratelimit-unified-7d-status", "warning"),
            ("anthropic-ratelimit-unified-status", "active"),
        ]);

        let profile = ProviderProfile {
            id: "claude-work".to_owned(),
            provider: ProviderId::Claude,
            label: Some("Claude Work".to_owned()),
            ..ProviderProfile::default_for_provider(ProviderId::Claude)
        };
        let snapshot = claude_snapshot_from_headers(&profile, &headers);

        assert_eq!(snapshot.provider_id, "claude");
        assert_eq!(snapshot.profile_id.as_deref(), Some("claude-work"));
        assert_eq!(snapshot.profile_name.as_deref(), Some("Claude Work"));
        assert_eq!(snapshot.status, ProviderUsageStatus::Available);
        assert_eq!(
            snapshot.message.as_deref(),
            Some("Live quota from Anthropic rate-limit headers.")
        );
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].kind, ProviderUsageWindowKind::FiveHour);
        assert_eq!(snapshot.windows[0].used_percent, Some(42.0));
        assert_eq!(snapshot.windows[0].remaining_percent, Some(58.0));
        assert_eq!(snapshot.windows[0].resets_at, Some(1_730_947_200));
        assert_eq!(snapshot.windows[1].kind, ProviderUsageWindowKind::Weekly);
        assert_eq!(snapshot.windows[1].used_percent, Some(68.0));
        assert_eq!(snapshot.windows[1].remaining_percent, Some(32.0));
        assert_eq!(snapshot.windows[1].resets_at, Some(1_731_206_400));
    }

    #[test]
    fn reads_claude_oauth_token_from_credentials_json() {
        let credentials = json!({
            "claudeAiOauth": {
                "accessToken": "oauth-token"
            }
        });

        assert_eq!(find_claude_oauth_token(&credentials), Some("oauth-token"));
    }

    #[test]
    fn parses_claude_usage_json_rate_limits() {
        let output = r#"{
            "type": "result",
            "result": "Usage summary",
            "planType": "max",
            "rate_limits": {
                "five_hour": { "used_percent": 31, "limit_window_seconds": 18000, "reset_at": 1730947200000 },
                "weekly": { "remaining_percent": 82, "window_duration_mins": 10080, "reset_at": 1731206400 }
            }
        }"#;

        let snapshot = claude_snapshot_from_output(output);

        assert_eq!(snapshot.provider_id, "claude");
        assert_eq!(snapshot.plan.as_deref(), Some("Max"));
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].kind, ProviderUsageWindowKind::FiveHour);
        assert_eq!(snapshot.windows[0].label, "5h window");
        assert_eq!(snapshot.windows[0].remaining_percent, Some(69.0));
        assert_eq!(snapshot.windows[0].resets_at, Some(1_730_947_200));
        assert_eq!(snapshot.windows[1].kind, ProviderUsageWindowKind::Weekly);
        assert_eq!(snapshot.windows[1].label, "Weekly window");
        assert_eq!(snapshot.windows[1].remaining_percent, Some(82.0));
    }

    #[test]
    fn parses_claude_usage_text_bars() {
        let output = r#"{
            "type": "result",
            "result": "Plan: Pro\n5-hour limit: 73% left (resets 18:22)\nWeekly limit: 54% left (resets Monday)"
        }"#;

        let snapshot = claude_snapshot_from_output(output);

        assert_eq!(snapshot.status, ProviderUsageStatus::Available);
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[0].kind, ProviderUsageWindowKind::FiveHour);
        assert_eq!(snapshot.windows[0].label, "5h window");
        assert_eq!(snapshot.windows[0].used_percent, Some(27.0));
        assert_eq!(snapshot.windows[0].remaining_percent, Some(73.0));
        assert_eq!(snapshot.windows[1].kind, ProviderUsageWindowKind::Weekly);
        assert_eq!(snapshot.windows[1].label, "Weekly window");
        assert_eq!(snapshot.windows[1].remaining_percent, Some(54.0));
    }

    #[test]
    fn treats_missing_claude_usage_command_as_unavailable() {
        let snapshot = claude_snapshot_from_output(
            r#"{"type":"result","subtype":"success","result":"Unknown skill: usage","total_cost_usd":0}"#,
        );

        assert_eq!(snapshot.status, ProviderUsageStatus::Unavailable);
        assert!(snapshot.windows.is_empty());
        assert_eq!(
            snapshot.message.as_deref(),
            Some(
                "Claude Code CLI does not expose subscription usage here; the agent can still run."
            )
        );
    }

    #[tokio::test]
    async fn cache_reuses_recent_usage_until_forced() {
        let cache = ProviderUsageCache::default();

        let first = cache
            .get_or_refresh(false, || async {
                vec![unavailable("codex", "Codex", "first".to_owned())]
            })
            .await;
        let second = cache
            .get_or_refresh(false, || async {
                vec![unavailable("codex", "Codex", "second".to_owned())]
            })
            .await;
        let refreshed = cache
            .get_or_refresh(true, || async {
                vec![unavailable("codex", "Codex", "forced".to_owned())]
            })
            .await;

        assert_eq!(first[0].message.as_deref(), Some("first"));
        assert_eq!(second[0].message.as_deref(), Some("first"));
        assert_eq!(refreshed[0].message.as_deref(), Some("forced"));
    }
}
