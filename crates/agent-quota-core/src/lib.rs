#![warn(missing_docs)]
//! Local-credential quota probes for AI coding agents.
//!
//! Agent Quota reads locally configured accounts, performs the smallest
//! provider-specific check available, and returns a versioned, provider-neutral
//! snapshot. A successful probe and usable quota are represented separately so
//! consumers do not need to infer routing decisions from transport health.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, process::Stdio};

use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

const DEFAULT_CODEX_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CLAUDE_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const CLAUDE_USAGE_MODEL: &str = "claude-haiku-4-5-20251001";
const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";
const CLAUDE_CREDENTIALS_RELATIVE_PATH: &str = ".claude/.credentials.json";

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Current serialized snapshot schema.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Result type used for configuration operations.
pub type AgentQuotaResult<T> = Result<T, AgentQuotaError>;

/// Configuration loading or validation error.
#[derive(Debug)]
pub enum AgentQuotaError {
    /// The configuration file could not be read.
    ConfigRead {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// The configuration file is not valid TOML or contains unknown fields.
    ConfigParse {
        /// Path containing invalid configuration.
        path: PathBuf,
        /// Parser diagnostic.
        message: String,
    },
    /// The configuration is syntactically valid but semantically unsafe.
    ConfigValidation {
        /// Human-readable validation diagnostic.
        message: String,
    },
}

impl std::fmt::Display for AgentQuotaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigRead { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::ConfigParse { path, message } => {
                write!(
                    formatter,
                    "invalid configuration in {}: {message}",
                    path.display()
                )
            }
            Self::ConfigValidation { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AgentQuotaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ConfigRead { source, .. } => Some(source),
            Self::ConfigParse { .. } | Self::ConfigValidation { .. } => None,
        }
    }
}

/// Supported quota provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    /// OpenAI Codex CLI.
    Codex,
    /// Anthropic Claude Code credentials.
    Claude,
}

impl ProviderId {
    /// Stable provider identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// Human-readable provider name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
        }
    }

    /// Parse a provider name or common alias.
    pub fn from_name(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" | "openai-codex" => Some(Self::Codex),
            "claude" | "claude-code" | "anthropic" => Some(Self::Claude),
            _ => None,
        }
    }
}

/// One locally configured provider account.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProviderProfile {
    /// Stable, non-empty profile identifier.
    pub id: String,
    /// Provider used by this profile.
    pub provider: ProviderId,
    /// Optional human-readable label.
    pub label: Option<String>,
    /// Optional Codex executable path.
    pub command_path: Option<PathBuf>,
    /// Optional Claude Code credentials file.
    pub credentials_path: Option<PathBuf>,
    /// Codex process environment overrides.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether this profile participates in configured collection.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl ProviderProfile {
    /// Construct the conventional local profile for a provider.
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

    /// Human-readable profile name.
    pub fn display_name(&self) -> String {
        self.label
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.provider.label().to_owned())
    }

    /// Validate provider-specific and identity fields.
    pub fn validate(&self) -> AgentQuotaResult<()> {
        if self.id.trim().is_empty() {
            return Err(validation_error("profile id must not be empty"));
        }
        if self.id.trim() != self.id {
            return Err(validation_error(format!(
                "profile id `{}` must not start or end with whitespace",
                self.id
            )));
        }
        if self
            .label
            .as_ref()
            .is_some_and(|label| label.trim().is_empty())
        {
            return Err(validation_error(format!(
                "profile `{}` has an empty label; omit `label` to use the provider name",
                self.id
            )));
        }

        match self.provider {
            ProviderId::Codex if self.credentials_path.is_some() => {
                return Err(validation_error(format!(
                    "profile `{}` is Codex, so `credentials_path` is not supported",
                    self.id
                )));
            }
            ProviderId::Claude if self.command_path.is_some() => {
                return Err(validation_error(format!(
                    "profile `{}` is Claude, so `command_path` is not supported",
                    self.id
                )));
            }
            ProviderId::Claude if !self.env.is_empty() => {
                return Err(validation_error(format!(
                    "profile `{}` is Claude, so process environment overrides are not supported",
                    self.id
                )));
            }
            ProviderId::Codex | ProviderId::Claude => {}
        }
        Ok(())
    }
}

fn default_enabled() -> bool {
    true
}

/// Top-level TOML configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct AgentQuotaConfig {
    /// Configured provider accounts.
    #[serde(default)]
    pub profiles: Vec<ProviderProfile>,
}

impl AgentQuotaConfig {
    /// Load and validate a TOML configuration file.
    pub fn load(path: impl AsRef<Path>) -> AgentQuotaResult<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| AgentQuotaError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        let config =
            toml::from_str::<Self>(&raw).map_err(|error| AgentQuotaError::ConfigParse {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        config.validate()?;
        Ok(config)
    }

    /// Validate profile identities and provider-specific fields.
    pub fn validate(&self) -> AgentQuotaResult<()> {
        let mut ids = BTreeSet::new();
        for profile in &self.profiles {
            profile.validate()?;
            if !ids.insert(&profile.id) {
                return Err(validation_error(format!(
                    "profile id `{}` is defined more than once",
                    profile.id
                )));
            }
        }
        Ok(())
    }

    /// Return enabled profiles in configuration order.
    pub fn profiles(&self) -> Vec<ProviderProfile> {
        self.profiles
            .iter()
            .filter(|profile| profile.enabled)
            .cloned()
            .collect()
    }
}

fn validation_error(message: impl Into<String>) -> AgentQuotaError {
    AgentQuotaError::ConfigValidation {
        message: message.into(),
    }
}

/// Explicit set of accounts to collect.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum UsageSelection {
    All,
    Providers(Vec<ProviderId>),
    Profiles(Vec<ProviderProfile>),
}

/// Options identifying exactly which provider accounts to collect.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CollectUsageOptions {
    selection: UsageSelection,
}

impl Default for CollectUsageOptions {
    fn default() -> Self {
        Self::all()
    }
}

impl CollectUsageOptions {
    /// Collect conventional local profiles for every supported provider.
    pub fn all() -> Self {
        Self {
            selection: UsageSelection::All,
        }
    }

    /// Collect conventional profiles for exactly the supplied providers.
    ///
    /// An empty iterator performs no probes.
    pub fn providers(providers: impl IntoIterator<Item = ProviderId>) -> Self {
        Self {
            selection: UsageSelection::Providers(providers.into_iter().collect()),
        }
    }

    /// Collect exactly the supplied enabled profiles.
    ///
    /// An empty iterator performs no probes.
    pub fn profiles(profiles: impl IntoIterator<Item = ProviderProfile>) -> Self {
        Self {
            selection: UsageSelection::Profiles(profiles.into_iter().collect()),
        }
    }

    fn selected_profiles(&self) -> Vec<ProviderProfile> {
        match &self.selection {
            UsageSelection::All => vec![
                ProviderProfile::default_for_provider(ProviderId::Codex),
                ProviderProfile::default_for_provider(ProviderId::Claude),
            ],
            UsageSelection::Providers(providers) => providers
                .iter()
                .copied()
                .map(ProviderProfile::default_for_provider)
                .collect(),
            UsageSelection::Profiles(profiles) => profiles
                .iter()
                .filter(|profile| profile.enabled)
                .cloned()
                .collect(),
        }
    }
}

/// Health of the provider probe itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// Provider returned a recognizable quota response.
    Ok,
    /// Local credentials are missing or rejected.
    AuthenticationRequired,
    /// Installed provider version does not expose compatible quota data.
    Unsupported,
    /// A retryable process, network, or provider failure occurred.
    TransientError,
    /// Provider returned a response that could not be interpreted safely.
    InvalidResponse,
    /// The supplied profile is invalid.
    InvalidConfiguration,
}

/// Whether normalized quota permits another agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaState {
    /// Every reported blocking window has remaining capacity.
    Available,
    /// At least one reported blocking window is exhausted.
    Exhausted,
    /// Capacity cannot be determined.
    Unknown,
}

/// Stable machine-readable failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUsageErrorCode {
    /// Profile fields are invalid.
    InvalidConfiguration,
    /// Required executable is not installed or not discoverable.
    ExecutableNotFound,
    /// Credential file is unavailable.
    CredentialsUnavailable,
    /// Provider rejected or could not find credentials.
    AuthenticationRequired,
    /// Provider did not respond before the configured deadline.
    Timeout,
    /// Network transport failed.
    Network,
    /// Provider returned an unsuccessful response.
    ProviderResponse,
    /// Response shape could not be interpreted.
    InvalidResponse,
    /// Provider version does not expose the expected quota interface.
    Unsupported,
}

/// Structured probe failure for automation and user interfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageError {
    /// Stable error code.
    pub code: ProviderUsageErrorCode,
    /// Safe human-readable diagnostic.
    pub message: String,
    /// Whether retrying later may succeed without user action.
    pub retryable: bool,
}

/// Normalized provider quota snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageSnapshot {
    /// Serialized schema version.
    pub schema_version: u32,
    /// Stable provider identifier.
    pub provider_id: ProviderId,
    /// Human-readable provider name.
    pub provider_name: String,
    /// Stable profile identifier.
    pub profile_id: String,
    /// Human-readable profile name.
    pub profile_name: String,
    /// Provider account label, when exposed.
    pub account_label: Option<String>,
    /// Machine-readable origin of quota data.
    pub source: Option<String>,
    /// Health of the quota probe.
    pub probe_status: ProbeStatus,
    /// Whether reported quota permits more work.
    pub quota_state: QuotaState,
    /// Provider subscription plan, when exposed.
    pub plan: Option<String>,
    /// Normalized quota windows.
    pub windows: Vec<ProviderUsageWindow>,
    /// Observation time in Unix epoch milliseconds.
    pub observed_at_ms: i64,
    /// Optional informational note.
    pub message: Option<String>,
    /// Structured error when the probe did not succeed.
    pub error: Option<ProviderUsageError>,
}

/// One provider quota window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsageWindow {
    /// Normalized window category.
    pub kind: ProviderUsageWindowKind,
    /// Human-readable label.
    pub label: String,
    /// Consumed capacity from zero to one hundred.
    pub used_percent: Option<f64>,
    /// Remaining capacity from zero to one hundred.
    pub remaining_percent: Option<f64>,
    /// Window duration in minutes.
    pub window_minutes: Option<u64>,
    /// Reset time in Unix epoch seconds.
    pub resets_at_epoch_seconds: Option<i64>,
    /// Optional provider status detail.
    pub detail: Option<String>,
}

/// Normalized quota window category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUsageWindowKind {
    /// Rolling five-hour window.
    FiveHour,
    /// Rolling or calendar weekly window.
    Weekly,
    /// Provider-specific window.
    Other,
}

#[derive(Debug, Clone)]
struct ProbeFailure {
    error: ProviderUsageError,
}

impl ProbeFailure {
    fn new(code: ProviderUsageErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            error: ProviderUsageError {
                code,
                message: message.into(),
                retryable,
            },
        }
    }

    fn status(&self) -> ProbeStatus {
        match self.error.code {
            ProviderUsageErrorCode::InvalidConfiguration => ProbeStatus::InvalidConfiguration,
            ProviderUsageErrorCode::CredentialsUnavailable
            | ProviderUsageErrorCode::AuthenticationRequired => ProbeStatus::AuthenticationRequired,
            ProviderUsageErrorCode::Unsupported => ProbeStatus::Unsupported,
            ProviderUsageErrorCode::InvalidResponse => ProbeStatus::InvalidResponse,
            ProviderUsageErrorCode::ExecutableNotFound
            | ProviderUsageErrorCode::Timeout
            | ProviderUsageErrorCode::Network
            | ProviderUsageErrorCode::ProviderResponse => ProbeStatus::TransientError,
        }
    }
}

/// Reusable collector with injectable transport settings.
#[derive(Debug, Clone)]
pub struct AgentQuotaClient {
    http_client: reqwest::Client,
    claude_api_url: String,
    claude_model: String,
    codex_timeout: Duration,
    claude_timeout: Duration,
}

impl Default for AgentQuotaClient {
    fn default() -> Self {
        let http_client = reqwest::Client::builder()
            .user_agent(concat!("agent-quota/", env!("CARGO_PKG_VERSION")))
            .redirect(Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http_client,
            claude_api_url: CLAUDE_API_URL.to_owned(),
            claude_model: CLAUDE_USAGE_MODEL.to_owned(),
            codex_timeout: DEFAULT_CODEX_TIMEOUT,
            claude_timeout: DEFAULT_CLAUDE_TIMEOUT,
        }
    }
}

impl AgentQuotaClient {
    /// Construct a collector with production defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the HTTP client, useful for embedding and tests.
    ///
    /// The default client disables redirects. A custom client is responsible
    /// for protecting authorization headers across redirects and proxies.
    pub fn with_http_client(mut self, http_client: reqwest::Client) -> Self {
        self.http_client = http_client;
        self
    }

    /// Override the Claude Messages endpoint, useful for deterministic tests.
    ///
    /// The local OAuth token is sent to this URL. Never accept the URL from
    /// untrusted input.
    pub fn with_claude_api_url(mut self, url: impl Into<String>) -> Self {
        self.claude_api_url = url.into();
        self
    }

    /// Override the minimal Claude model used to obtain quota headers.
    pub fn with_claude_model(mut self, model: impl Into<String>) -> Self {
        self.claude_model = model.into();
        self
    }

    /// Override provider deadlines.
    pub fn with_timeouts(mut self, codex: Duration, claude: Duration) -> Self {
        self.codex_timeout = codex;
        self.claude_timeout = claude;
        self
    }

    /// Collect normalized snapshots for an explicit selection.
    pub async fn collect_usage(&self, options: CollectUsageOptions) -> Vec<ProviderUsageSnapshot> {
        let probes = options
            .selected_profiles()
            .into_iter()
            .map(|profile| self.collect_profile_usage(profile));
        futures::future::join_all(probes).await
    }

    /// Collect one configured profile.
    pub async fn collect_profile_usage(&self, profile: ProviderProfile) -> ProviderUsageSnapshot {
        if let Err(error) = profile.validate() {
            return failed_snapshot(
                &profile,
                ProbeFailure::new(
                    ProviderUsageErrorCode::InvalidConfiguration,
                    error.to_string(),
                    false,
                ),
            );
        }

        let result = match profile.provider {
            ProviderId::Codex => self.query_codex_usage(&profile).await,
            ProviderId::Claude => self.query_claude_usage(&profile).await,
        };
        result.unwrap_or_else(|error| failed_snapshot(&profile, error))
    }

    async fn query_codex_usage(
        &self,
        profile: &ProviderProfile,
    ) -> Result<ProviderUsageSnapshot, ProbeFailure> {
        let executable = profile
            .command_path
            .as_deref()
            .unwrap_or_else(|| Path::new("codex"));
        let mut command = Command::new(executable);
        command.args(["app-server", "--listen", "stdio://"]);
        command.envs(&profile.env);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(target_os = "windows")]
        command.creation_flags(CREATE_NO_WINDOW);

        let mut child = command.spawn().map_err(|error| {
            let code = if error.kind() == std::io::ErrorKind::NotFound {
                ProviderUsageErrorCode::ExecutableNotFound
            } else {
                ProviderUsageErrorCode::ProviderResponse
            };
            ProbeFailure::new(
                code,
                format!("could not start `{}`: {error}", executable.display()),
                true,
            )
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            ProbeFailure::new(
                ProviderUsageErrorCode::InvalidResponse,
                "Codex app-server stdin was unavailable",
                true,
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ProbeFailure::new(
                ProviderUsageErrorCode::InvalidResponse,
                "Codex app-server stdout was unavailable",
                true,
            )
        })?;

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
        .await
        .map_err(codex_io_failure)?;
        write_rpc(&mut stdin, json!({ "method": "initialized", "params": {} }))
            .await
            .map_err(codex_io_failure)?;
        write_rpc(
            &mut stdin,
            json!({
                "method": "account/read",
                "id": 2,
                "params": { "refreshToken": false }
            }),
        )
        .await
        .map_err(codex_io_failure)?;
        write_rpc(
            &mut stdin,
            json!({ "method": "account/rateLimits/read", "id": 3 }),
        )
        .await
        .map_err(codex_io_failure)?;

        let mut lines = BufReader::new(stdout).lines();
        let read = async {
            let mut account_response = None;
            let mut rate_limits_response = None;
            while account_response.is_none() || rate_limits_response.is_none() {
                let Some(line) = lines.next_line().await.map_err(codex_io_failure)? else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match value.get("id").and_then(Value::as_i64) {
                    Some(2) => account_response = Some(value),
                    Some(3) => rate_limits_response = Some(value),
                    _ => {}
                }
            }
            let account_response = account_response.ok_or_else(|| {
                ProbeFailure::new(
                    ProviderUsageErrorCode::InvalidResponse,
                    "Codex account response was missing",
                    true,
                )
            })?;
            let rate_limits_response = rate_limits_response.ok_or_else(|| {
                ProbeFailure::new(
                    ProviderUsageErrorCode::InvalidResponse,
                    "Codex rate-limit response was missing",
                    true,
                )
            })?;
            Ok::<_, ProbeFailure>((account_response, rate_limits_response))
        };

        let responses = timeout(self.codex_timeout, read).await.map_err(|_| {
            ProbeFailure::new(
                ProviderUsageErrorCode::Timeout,
                "timed out waiting for Codex quota data",
                true,
            )
        })??;
        let _ = child.kill().await;
        codex_snapshot_from_responses(profile, &responses.0, &responses.1)
    }

    async fn query_claude_usage(
        &self,
        profile: &ProviderProfile,
    ) -> Result<ProviderUsageSnapshot, ProbeFailure> {
        let token = read_claude_oauth_token(profile.credentials_path.as_deref())?;
        let request = self
            .http_client
            .post(&self.claude_api_url)
            .bearer_auth(token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .json(&json!({
                "model": self.claude_model,
                "max_tokens": 1,
                "messages": [{ "role": "user", "content": "hi" }],
            }));
        let response = timeout(self.claude_timeout, request.send())
            .await
            .map_err(|_| {
                ProbeFailure::new(
                    ProviderUsageErrorCode::Timeout,
                    "timed out waiting for Claude quota headers",
                    true,
                )
            })?
            .map_err(|error| {
                ProbeFailure::new(
                    ProviderUsageErrorCode::Network,
                    format!("Claude quota request failed: {error}"),
                    true,
                )
            })?;

        let status = response.status();
        let snapshot = claude_snapshot_from_headers(profile, response.headers(), status)?;
        if status.is_success() || status == StatusCode::TOO_MANY_REQUESTS {
            Ok(snapshot)
        } else {
            Err(http_status_failure("Claude", status))
        }
    }
}

fn codex_io_failure(error: std::io::Error) -> ProbeFailure {
    ProbeFailure::new(
        ProviderUsageErrorCode::ProviderResponse,
        format!("Codex app-server communication failed: {error}"),
        true,
    )
}

fn http_status_failure(provider: &str, status: StatusCode) -> ProbeFailure {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        ProbeFailure::new(
            ProviderUsageErrorCode::AuthenticationRequired,
            format!("{provider} rejected the local credentials ({status})"),
            false,
        )
    } else {
        ProbeFailure::new(
            ProviderUsageErrorCode::ProviderResponse,
            format!("{provider} returned HTTP {status}"),
            status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
        )
    }
}

async fn write_rpc(stdin: &mut ChildStdin, value: Value) -> std::io::Result<()> {
    stdin.write_all(value.to_string().as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

fn rpc_failure(value: &Value, operation: &str) -> Option<ProbeFailure> {
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown provider error");
    let lower = message.to_ascii_lowercase();
    let authentication = ["auth", "login", "sign in", "credential"]
        .iter()
        .any(|needle| lower.contains(needle));
    Some(if authentication {
        ProbeFailure::new(
            ProviderUsageErrorCode::AuthenticationRequired,
            format!("Codex {operation} failed: {message}"),
            false,
        )
    } else {
        ProbeFailure::new(
            ProviderUsageErrorCode::ProviderResponse,
            format!("Codex {operation} failed: {message}"),
            true,
        )
    })
}

fn codex_snapshot_from_responses(
    profile: &ProviderProfile,
    account_response: &Value,
    rate_limits_response: &Value,
) -> Result<ProviderUsageSnapshot, ProbeFailure> {
    if let Some(error) = rpc_failure(account_response, "account lookup") {
        return Err(error);
    }
    if let Some(error) = rpc_failure(rate_limits_response, "quota lookup") {
        return Err(error);
    }

    let account = account_response
        .pointer("/result/account")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProbeFailure::new(
                ProviderUsageErrorCode::AuthenticationRequired,
                "Codex did not return a signed-in account",
                false,
            )
        })?;
    let rate_limits = rate_limits_response
        .pointer("/result/rateLimits")
        .ok_or_else(|| {
            ProbeFailure::new(
                ProviderUsageErrorCode::InvalidResponse,
                "Codex did not return rate-limit data",
                true,
            )
        })?;

    let plan = account
        .get("planType")
        .and_then(Value::as_str)
        .map(format_plan_name);
    let account_label = account
        .get("email")
        .or_else(|| account.get("username"))
        .or_else(|| account.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mut windows = Vec::new();
    if let Some(primary) = rate_limits.get("primary") {
        if let Some(window) = window_from_object("Primary window", primary) {
            windows.push(window);
        }
    }
    if let Some(secondary) = rate_limits.get("secondary") {
        if let Some(window) = window_from_object("Secondary window", secondary) {
            windows.push(window);
        }
    }
    sort_usage_windows(&mut windows);
    if windows.is_empty() {
        return Err(ProbeFailure::new(
            ProviderUsageErrorCode::InvalidResponse,
            "Codex rate-limit response contained no recognizable quota windows",
            true,
        ));
    }

    let reached = rate_limits
        .get("rateLimitReachedType")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let quota_state = if reached.is_some() {
        QuotaState::Exhausted
    } else {
        quota_state_from_windows(&windows)
    };
    Ok(success_snapshot(
        profile,
        account_label,
        Some("codex_app_server"),
        plan,
        windows,
        reached.map(|value| format!("Limit reached: {}", value.replace('_', " "))),
        quota_state,
    ))
}

fn read_claude_oauth_token(credentials_path: Option<&Path>) -> Result<String, ProbeFailure> {
    let path = credentials_path
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(default_claude_credentials_path)
        .map_err(|error| {
            ProbeFailure::new(
                ProviderUsageErrorCode::CredentialsUnavailable,
                error.to_string(),
                false,
            )
        })?;
    let raw = fs::read_to_string(&path).map_err(|error| {
        ProbeFailure::new(
            ProviderUsageErrorCode::CredentialsUnavailable,
            format!(
                "Claude Code credentials were not readable at {}: {error}",
                path.display()
            ),
            false,
        )
    })?;
    let value = serde_json::from_str::<Value>(&raw).map_err(|error| {
        ProbeFailure::new(
            ProviderUsageErrorCode::CredentialsUnavailable,
            format!(
                "Claude Code credentials at {} are invalid: {error}",
                path.display()
            ),
            false,
        )
    })?;
    find_claude_oauth_token(&value)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            ProbeFailure::new(
                ProviderUsageErrorCode::AuthenticationRequired,
                format!(
                    "Claude Code OAuth token was not found in {}",
                    path.display()
                ),
                false,
            )
        })
}

/// Conventional Claude Code credentials path for the current user.
pub fn default_claude_credentials_path() -> AgentQuotaResult<PathBuf> {
    dirs::home_dir()
        .map(|home| home.join(CLAUDE_CREDENTIALS_RELATIVE_PATH))
        .ok_or_else(|| validation_error("home directory was not available"))
}

fn find_claude_oauth_token(value: &Value) -> Option<&str> {
    [
        "/claudeAiOauth/accessToken",
        "/oauth/accessToken",
        "/claudeAiOauth/access_token",
    ]
    .into_iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
    .filter(|token| !token.trim().is_empty())
}

fn claude_snapshot_from_headers(
    profile: &ProviderProfile,
    headers: &HeaderMap,
    status: StatusCode,
) -> Result<ProviderUsageSnapshot, ProbeFailure> {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(http_status_failure("Claude", status));
    }

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

    if windows.is_empty() {
        let code = if status.is_success() {
            ProviderUsageErrorCode::Unsupported
        } else {
            ProviderUsageErrorCode::ProviderResponse
        };
        return Err(ProbeFailure::new(
            code,
            if status.is_success() {
                "Anthropic did not return supported unified quota headers".to_owned()
            } else {
                format!("Claude returned HTTP {status} without quota headers")
            },
            status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
        ));
    }

    let provider_status = header_string(headers, "anthropic-ratelimit-unified-status");
    let quota_state = if status == StatusCode::TOO_MANY_REQUESTS
        || provider_status.as_deref().is_some_and(|value| {
            ["rejected", "exhausted", "blocked", "rate_limited"]
                .iter()
                .any(|candidate| value.eq_ignore_ascii_case(candidate))
        }) {
        QuotaState::Exhausted
    } else {
        quota_state_from_windows(&windows)
    };
    let message = provider_status
        .filter(|value| !value.eq_ignore_ascii_case("active"))
        .map(|value| format!("Claude quota status: {value}"))
        .or_else(|| Some("Live quota from Anthropic rate-limit headers.".to_owned()));

    Ok(success_snapshot(
        profile,
        None,
        Some("anthropic_rate_limit_headers"),
        None,
        windows,
        message,
        quota_state,
    ))
}

fn claude_header_window(
    headers: &HeaderMap,
    kind: ProviderUsageWindowKind,
    label: &str,
    utilization_header: &str,
    reset_header: &str,
) -> Option<ProviderUsageWindow> {
    let utilization = header_number(headers, utilization_header)?;
    if !utilization.is_finite() {
        return None;
    }
    let used_percent = (utilization * 100.0).clamp(0.0, 100.0);
    Some(ProviderUsageWindow {
        kind,
        label: label.to_owned(),
        used_percent: Some(used_percent),
        remaining_percent: Some((100.0 - used_percent).clamp(0.0, 100.0)),
        window_minutes: match kind {
            ProviderUsageWindowKind::FiveHour => Some(300),
            ProviderUsageWindowKind::Weekly => Some(10_080),
            ProviderUsageWindowKind::Other => None,
        },
        resets_at_epoch_seconds: header_integer(headers, reset_header).map(normalize_epoch_seconds),
        detail: header_string(
            headers,
            match kind {
                ProviderUsageWindowKind::FiveHour => "anthropic-ratelimit-unified-5h-status",
                ProviderUsageWindowKind::Weekly => "anthropic-ratelimit-unified-7d-status",
                ProviderUsageWindowKind::Other => "anthropic-ratelimit-unified-status",
            },
        )
        .map(|value| format!("Status: {value}")),
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
    )
    .filter(|value| value.is_finite())
    .map(|value| value.clamp(0.0, 100.0));
    let remaining_percent = number_field(
        map,
        &[
            "remainingPercent",
            "remaining_percent",
            "percentRemaining",
            "percent_remaining",
        ],
    )
    .filter(|value| value.is_finite())
    .map(|value| value.clamp(0.0, 100.0))
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
    let resets_at_epoch_seconds = integer_field(
        map,
        &["resetsAt", "resetAt", "reset_at", "resets_at", "resetTime"],
    )
    .map(normalize_epoch_seconds);
    let kind = kind_for_usage_window(label_hint, window_minutes);
    Some(ProviderUsageWindow {
        kind,
        label: label_for_usage_window(label_hint, window_minutes, kind),
        used_percent,
        remaining_percent,
        window_minutes,
        resets_at_epoch_seconds,
        detail: None,
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
        u64::try_from(value.round() as i128).ok()
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
        _ if ["five", "5h", "5 hour", "5-hour"]
            .iter()
            .any(|needle| label_hint.contains(needle)) =>
        {
            ProviderUsageWindowKind::FiveHour
        }
        _ if ["weekly", "week", "7 day", "7-day"]
            .iter()
            .any(|needle| label_hint.contains(needle)) =>
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

fn format_plan_name(value: &str) -> String {
    value
        .replace(['_', '-'], " ")
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn quota_state_from_windows(windows: &[ProviderUsageWindow]) -> QuotaState {
    if windows.is_empty() {
        return QuotaState::Unknown;
    }
    if windows.iter().any(|window| {
        window
            .remaining_percent
            .is_some_and(|remaining| remaining <= 0.0)
            || window.used_percent.is_some_and(|used| used >= 100.0)
    }) {
        QuotaState::Exhausted
    } else if windows
        .iter()
        .any(|window| window.remaining_percent.is_some() || window.used_percent.is_some())
    {
        QuotaState::Available
    } else {
        QuotaState::Unknown
    }
}

fn success_snapshot(
    profile: &ProviderProfile,
    account_label: Option<String>,
    source: Option<&str>,
    plan: Option<String>,
    windows: Vec<ProviderUsageWindow>,
    message: Option<String>,
    quota_state: QuotaState,
) -> ProviderUsageSnapshot {
    ProviderUsageSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        provider_id: profile.provider,
        provider_name: profile.provider.label().to_owned(),
        profile_id: profile.id.clone(),
        profile_name: profile.display_name(),
        account_label,
        source: source.map(ToOwned::to_owned),
        probe_status: ProbeStatus::Ok,
        quota_state,
        plan,
        windows,
        observed_at_ms: now_epoch_ms(),
        message,
        error: None,
    }
}

fn failed_snapshot(profile: &ProviderProfile, failure: ProbeFailure) -> ProviderUsageSnapshot {
    ProviderUsageSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        provider_id: profile.provider,
        provider_name: profile.provider.label().to_owned(),
        profile_id: profile.id.clone(),
        profile_name: profile.display_name(),
        account_label: None,
        source: None,
        probe_status: failure.status(),
        quota_state: QuotaState::Unknown,
        plan: None,
        windows: Vec::new(),
        observed_at_ms: now_epoch_ms(),
        message: None,
        error: Some(failure.error),
    }
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct CachedProviderUsage {
    snapshot: ProviderUsageSnapshot,
    cached_at_ms: i64,
}

/// Profile-keyed provider cache for repeated UI or CLI polling.
#[derive(Debug, Clone)]
pub struct ProviderUsageCache {
    inner: Arc<Mutex<BTreeMap<ProviderProfile, CachedProviderUsage>>>,
    ttl: Duration,
}

impl Default for ProviderUsageCache {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
            ttl: DEFAULT_CACHE_TTL,
        }
    }
}

impl ProviderUsageCache {
    /// Construct a cache with a custom time-to-live.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            ..Self::default()
        }
    }

    /// Collect through a cache keyed by every complete provider profile.
    ///
    /// Failed profiles are not cached, allowing immediate recovery after sign-in
    /// or transient failures while successful profiles keep their cached values.
    pub async fn collect(
        &self,
        client: &AgentQuotaClient,
        options: CollectUsageOptions,
        force_refresh: bool,
    ) -> Vec<ProviderUsageSnapshot> {
        let profiles = options.selected_profiles();
        let probes = profiles.into_iter().map(|profile| async move {
            let now = now_epoch_ms();
            if !force_refresh {
                let guard = self.inner.lock().await;
                if let Some(cached) = guard.get(&profile) {
                    let ttl_ms = i64::try_from(self.ttl.as_millis()).unwrap_or(i64::MAX);
                    if now.saturating_sub(cached.cached_at_ms) < ttl_ms {
                        return cached.snapshot.clone();
                    }
                }
            }

            let snapshot = client.collect_profile_usage(profile.clone()).await;
            let mut guard = self.inner.lock().await;
            if snapshot.probe_status == ProbeStatus::Ok {
                guard.insert(
                    profile,
                    CachedProviderUsage {
                        snapshot: snapshot.clone(),
                        cached_at_ms: now_epoch_ms(),
                    },
                );
            } else {
                guard.remove(&profile);
            }
            snapshot
        });
        futures::future::join_all(probes).await
    }

    /// Clear every cached selection.
    pub async fn clear(&self) {
        self.inner.lock().await.clear();
    }
}

/// Collect all conventional local provider profiles.
pub async fn collect_provider_usage() -> Vec<ProviderUsageSnapshot> {
    collect_usage(CollectUsageOptions::all()).await
}

/// Collect an explicit selection with production defaults.
pub async fn collect_usage(options: CollectUsageOptions) -> Vec<ProviderUsageSnapshot> {
    AgentQuotaClient::new().collect_usage(options).await
}

/// Collect one conventional provider profile.
pub async fn collect_single_provider_usage(provider: ProviderId) -> ProviderUsageSnapshot {
    AgentQuotaClient::new()
        .collect_profile_usage(ProviderProfile::default_for_provider(provider))
        .await
}

/// Collect one configured provider profile.
pub async fn collect_profile_usage(profile: ProviderProfile) -> ProviderUsageSnapshot {
    AgentQuotaClient::new().collect_profile_usage(profile).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::thread;
    use std::time::Instant;

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

    fn start_http_server(
        status: &'static str,
        headers: &'static str,
        delay: Duration,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let mut stream = accept_test_connection(&listener);
            let mut request = [0_u8; 8_192];
            let bytes = stream.read(&mut request).expect("request should read");
            assert!(bytes > 0, "request should not be empty");
            thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-length: 2\r\nconnection: close\r\n{headers}\r\n{{}}"
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://{address}/v1/messages"), server)
    }

    fn accept_test_connection(listener: &TcpListener) -> TcpStream {
        listener
            .set_nonblocking(true)
            .expect("test listener should become nonblocking");
        let deadline = Instant::now() + Duration::from_secs(5);

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("test stream should become blocking");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("test stream should have a read timeout");
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .expect("test stream should have a write timeout");
                    return stream;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    panic!("request did not connect before the test deadline");
                }
                Err(error) => panic!("request should connect: {error}"),
            }
        }
    }

    fn temporary_claude_credentials() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agent-quota-test-{}-{}.json",
            std::process::id(),
            now_epoch_ms()
        ));
        fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"test-token"}}"#)
            .expect("credentials fixture should write");
        path
    }

    #[test]
    fn parses_and_validates_profile_config() {
        let config = toml::from_str::<AgentQuotaConfig>(
            r#"
                [[profiles]]
                id = "claude-work"
                provider = "claude"
                label = "Claude Work"
                credentials_path = "C:/Users/example/.claude-work/.credentials.json"

                [[profiles]]
                id = "codex-private"
                provider = "codex"
                label = "Codex Private"

                [profiles.env]
                CODEX_HOME = "C:/Users/example/.codex-private"
            "#,
        )
        .expect("profile config should parse");

        config.validate().expect("profile config should validate");
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles[0].provider, ProviderId::Claude);
    }

    #[test]
    fn rejects_unknown_and_duplicate_profile_fields() {
        let unknown = toml::from_str::<AgentQuotaConfig>(
            r#"
                [[profiles]]
                id = "codex"
                provider = "codex"
                comand_path = "codex"
            "#,
        );
        assert!(unknown.is_err());

        let duplicate = AgentQuotaConfig {
            profiles: vec![
                ProviderProfile::default_for_provider(ProviderId::Codex),
                ProviderProfile::default_for_provider(ProviderId::Codex),
            ],
        };
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn empty_explicit_selection_does_not_expand_to_all() {
        assert!(CollectUsageOptions::providers([])
            .selected_profiles()
            .is_empty());
        assert!(CollectUsageOptions::profiles([])
            .selected_profiles()
            .is_empty());
        assert_eq!(CollectUsageOptions::all().selected_profiles().len(), 2);
    }

    #[test]
    fn maps_codex_success_and_exhaustion() {
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
                    "primary": { "usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1_730_947_200 },
                    "secondary": { "usedPercent": 46, "windowDurationMins": 10080, "resetsAt": 1_731_206_400 },
                    "rateLimitReachedType": "primary"
                }
            }
        });
        let profile = ProviderProfile::default_for_provider(ProviderId::Codex);
        let snapshot = codex_snapshot_from_responses(&profile, &account, &rate_limits)
            .expect("response should map");

        assert_eq!(
            snapshot.probe_status,
            ProbeStatus::Ok,
            "unexpected snapshot: {snapshot:?}"
        );
        assert_eq!(snapshot.quota_state, QuotaState::Exhausted);
        assert_eq!(snapshot.account_label.as_deref(), Some("user@example.com"));
        assert_eq!(snapshot.windows.len(), 2);
    }

    #[test]
    fn rejects_codex_rpc_errors_and_missing_windows() {
        let profile = ProviderProfile::default_for_provider(ProviderId::Codex);
        let auth_error = json!({
            "id": 2,
            "error": { "code": -32000, "message": "Please sign in to Codex" }
        });
        let rate_limits = json!({ "id": 3, "result": { "rateLimits": {} } });
        let error = codex_snapshot_from_responses(&profile, &auth_error, &rate_limits)
            .expect_err("RPC error must not become an available snapshot");
        assert_eq!(
            error.error.code,
            ProviderUsageErrorCode::AuthenticationRequired
        );

        let account = json!({ "id": 2, "result": { "account": {} } });
        let error = codex_snapshot_from_responses(&profile, &account, &rate_limits)
            .expect_err("missing windows must be rejected");
        assert_eq!(error.error.code, ProviderUsageErrorCode::InvalidResponse);
    }

    #[tokio::test]
    async fn codex_production_process_path_maps_mock_app_server() {
        let extension = if cfg!(windows) { "cmd" } else { "sh" };
        let path = std::env::temp_dir().join(format!(
            "agent-quota-mock-codex-{}-{}.{}",
            std::process::id(),
            now_epoch_ms(),
            extension
        ));
        #[cfg(windows)]
        let script = concat!(
            "@echo off\r\n",
            "echo {\"id\":2,\"result\":{\"account\":{\"email\":\"test@example.com\",\"planType\":\"plus\"}}}\r\n",
            "echo {\"id\":3,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":25,\"windowDurationMins\":300,\"resetsAt\":1730947200}}}}\r\n",
            "ping -n 3 127.0.0.1 >nul\r\n"
        );
        #[cfg(unix)]
        let script = concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' '{\"id\":2,\"result\":{\"account\":{\"email\":\"test@example.com\",\"planType\":\"plus\"}}}'\n",
            "printf '%s\\n' '{\"id\":3,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":25,\"windowDurationMins\":300,\"resetsAt\":1730947200}}}}'\n",
            "sleep 2\n"
        );
        fs::write(&path, script).expect("mock Codex executable should write");
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path)
                .expect("mock metadata should read")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).expect("mock should become executable");
        }

        let profile = ProviderProfile {
            command_path: Some(path.clone()),
            ..ProviderProfile::default_for_provider(ProviderId::Codex)
        };
        let snapshot = AgentQuotaClient::new().collect_profile_usage(profile).await;
        let _ = fs::remove_file(path);

        assert_eq!(
            snapshot.probe_status,
            ProbeStatus::Ok,
            "unexpected snapshot: {snapshot:?}"
        );
        assert_eq!(snapshot.quota_state, QuotaState::Available);
        assert_eq!(snapshot.account_label.as_deref(), Some("test@example.com"));
        assert_eq!(snapshot.windows[0].remaining_percent, Some(75.0));
    }

    #[test]
    fn parses_claude_headers_and_http_failures() {
        let headers = header_map(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "1730947200"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.68"),
            ("anthropic-ratelimit-unified-7d-reset", "1731206400000"),
            ("anthropic-ratelimit-unified-status", "active"),
        ]);
        let profile = ProviderProfile::default_for_provider(ProviderId::Claude);
        let snapshot = claude_snapshot_from_headers(&profile, &headers, StatusCode::OK)
            .expect("headers should map");
        assert_eq!(snapshot.probe_status, ProbeStatus::Ok);
        assert_eq!(snapshot.quota_state, QuotaState::Available);
        assert_eq!(snapshot.windows[0].remaining_percent, Some(58.0));
        assert_eq!(
            snapshot.windows[1].resets_at_epoch_seconds,
            Some(1_731_206_400)
        );

        let error =
            claude_snapshot_from_headers(&profile, &HeaderMap::new(), StatusCode::UNAUTHORIZED)
                .expect_err("401 should be authentication failure");
        assert_eq!(
            error.error.code,
            ProviderUsageErrorCode::AuthenticationRequired
        );
    }

    #[test]
    fn reads_claude_oauth_token_from_supported_shapes() {
        let credentials = json!({
            "claudeAiOauth": {
                "accessToken": "oauth-token"
            }
        });
        assert_eq!(find_claude_oauth_token(&credentials), Some("oauth-token"));
    }

    #[tokio::test]
    async fn cache_is_profile_keyed_and_does_not_cache_failures() {
        let cache = ProviderUsageCache::default();
        let client = AgentQuotaClient::new();
        let codex_profile = ProviderProfile::default_for_provider(ProviderId::Codex);
        cache.inner.lock().await.insert(
            codex_profile.clone(),
            CachedProviderUsage {
                snapshot: success_snapshot(
                    &codex_profile,
                    None,
                    Some("fixture"),
                    None,
                    vec![ProviderUsageWindow {
                        kind: ProviderUsageWindowKind::FiveHour,
                        label: "5h window".to_owned(),
                        used_percent: Some(25.0),
                        remaining_percent: Some(75.0),
                        window_minutes: Some(300),
                        resets_at_epoch_seconds: None,
                        detail: None,
                    }],
                    None,
                    QuotaState::Available,
                ),
                cached_at_ms: now_epoch_ms(),
            },
        );
        let empty = CollectUsageOptions::profiles([]);
        assert!(cache
            .collect(&client, empty.clone(), false)
            .await
            .is_empty());

        let invalid_profile = ProviderProfile {
            id: String::new(),
            ..ProviderProfile::default_for_provider(ProviderId::Codex)
        };
        let invalid = CollectUsageOptions::profiles([invalid_profile]);
        let first = cache.collect(&client, invalid.clone(), false).await;
        let second = cache.collect(&client, invalid, false).await;
        assert_eq!(first[0].probe_status, ProbeStatus::InvalidConfiguration);
        assert_eq!(second[0].probe_status, ProbeStatus::InvalidConfiguration);
        let guard = cache.inner.lock().await;
        assert_eq!(guard.len(), 1);
        assert!(guard.contains_key(&codex_profile));
    }

    #[test]
    fn serializes_versioned_snapshot_contract() {
        let profile = ProviderProfile::default_for_provider(ProviderId::Codex);
        let snapshot = success_snapshot(
            &profile,
            Some("user@example.com".to_owned()),
            Some("fixture"),
            Some("Plus".to_owned()),
            vec![ProviderUsageWindow {
                kind: ProviderUsageWindowKind::FiveHour,
                label: "5h window".to_owned(),
                used_percent: Some(42.0),
                remaining_percent: Some(58.0),
                window_minutes: Some(300),
                resets_at_epoch_seconds: Some(1_730_947_200),
                detail: None,
            }],
            None,
            QuotaState::Available,
        );
        let value = serde_json::to_value(snapshot).expect("snapshot should serialize");
        assert_eq!(value["schemaVersion"], SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(value["providerId"], "codex");
        assert_eq!(value["probeStatus"], "ok");
        assert_eq!(value["quotaState"], "available");
        assert!(value["observedAtMs"].as_i64().is_some());
        assert_eq!(value["windows"][0]["resetsAtEpochSeconds"], 1_730_947_200);
    }

    #[test]
    fn shared_v1_fixture_matches_the_public_model() {
        let raw = include_str!("../fixtures/provider-usage-v1.json");
        let snapshots =
            serde_json::from_str::<Vec<ProviderUsageSnapshot>>(raw).expect("fixture should parse");
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].schema_version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(snapshots[0].probe_status, ProbeStatus::Ok);
        assert_eq!(
            snapshots[1].error.as_ref().map(|error| error.code),
            Some(ProviderUsageErrorCode::CredentialsUnavailable)
        );
    }

    #[tokio::test]
    async fn claude_production_path_uses_injected_endpoint_and_checks_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let server = thread::spawn(move || {
            let mut stream = accept_test_connection(&listener);
            let mut request = [0_u8; 8_192];
            let bytes = stream.read(&mut request).expect("request should read");
            let request = String::from_utf8_lossy(&request[..bytes]);
            assert!(request.starts_with("POST /v1/messages "));
            assert!(request
                .to_ascii_lowercase()
                .contains("user-agent: agent-quota/"));
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "content-length: 2\r\n",
                "connection: close\r\n",
                "anthropic-ratelimit-unified-5h-utilization: 0.25\r\n",
                "anthropic-ratelimit-unified-5h-reset: 1730947200\r\n",
                "anthropic-ratelimit-unified-status: active\r\n",
                "\r\n{}"
            );
            stream
                .write_all(response.as_bytes())
                .expect("response should write");
        });

        let credentials_path = temporary_claude_credentials();
        let profile = ProviderProfile {
            credentials_path: Some(credentials_path.clone()),
            ..ProviderProfile::default_for_provider(ProviderId::Claude)
        };
        let client =
            AgentQuotaClient::new().with_claude_api_url(format!("http://{address}/v1/messages"));
        let snapshot = client.collect_profile_usage(profile).await;
        let _ = fs::remove_file(credentials_path);
        server.join().expect("server should finish");

        assert_eq!(snapshot.probe_status, ProbeStatus::Ok);
        assert_eq!(snapshot.quota_state, QuotaState::Available);
        assert_eq!(snapshot.windows[0].remaining_percent, Some(75.0));
    }

    #[tokio::test]
    async fn claude_production_path_classifies_http_and_timeout_failures() {
        let credentials_path = temporary_claude_credentials();
        let profile = ProviderProfile {
            credentials_path: Some(credentials_path.clone()),
            ..ProviderProfile::default_for_provider(ProviderId::Claude)
        };

        let (url, server) = start_http_server("401 Unauthorized", "", Duration::from_millis(0));
        let snapshot = AgentQuotaClient::new()
            .with_claude_api_url(url)
            .collect_profile_usage(profile.clone())
            .await;
        server.join().expect("401 server should finish");
        assert_eq!(snapshot.probe_status, ProbeStatus::AuthenticationRequired);
        assert_eq!(
            snapshot.error.as_ref().map(|error| error.code),
            Some(ProviderUsageErrorCode::AuthenticationRequired)
        );

        let headers = concat!(
            "anthropic-ratelimit-unified-5h-utilization: 1.0\r\n",
            "anthropic-ratelimit-unified-5h-reset: 1730947200\r\n",
            "anthropic-ratelimit-unified-status: exhausted\r\n"
        );
        let (url, server) =
            start_http_server("429 Too Many Requests", headers, Duration::from_millis(0));
        let snapshot = AgentQuotaClient::new()
            .with_claude_api_url(url)
            .collect_profile_usage(profile.clone())
            .await;
        server.join().expect("429 server should finish");
        assert_eq!(snapshot.probe_status, ProbeStatus::Ok);
        assert_eq!(snapshot.quota_state, QuotaState::Exhausted);

        let (url, server) = start_http_server("200 OK", "", Duration::from_millis(0));
        let snapshot = AgentQuotaClient::new()
            .with_claude_api_url(url)
            .collect_profile_usage(profile.clone())
            .await;
        server.join().expect("no-header server should finish");
        assert_eq!(snapshot.probe_status, ProbeStatus::Unsupported);

        let (url, server) = start_http_server("200 OK", "", Duration::from_millis(500));
        let snapshot = AgentQuotaClient::new()
            .with_claude_api_url(url)
            .with_timeouts(DEFAULT_CODEX_TIMEOUT, Duration::from_millis(250))
            .collect_profile_usage(profile)
            .await;
        server.join().expect("timeout server should finish");
        let _ = fs::remove_file(credentials_path);
        assert_eq!(snapshot.probe_status, ProbeStatus::TransientError);
        assert_eq!(
            snapshot.error.as_ref().map(|error| error.code),
            Some(ProviderUsageErrorCode::Timeout)
        );
    }
}
