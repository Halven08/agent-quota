use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_quota_core::{
    default_claude_credentials_path, AgentQuotaClient, AgentQuotaConfig, CollectUsageOptions,
    ProbeStatus, ProviderId, ProviderProfile, ProviderUsageCache, ProviderUsageSnapshot,
    QuotaState,
};
use serde::Serialize;
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

const DEFAULT_INTERVAL_SECONDS: u64 = 300;
const MINIMUM_INTERVAL_SECONDS: u64 = 60;
const ALL_PROBES_FAILED_EXIT_CODE: i32 = 3;

#[derive(Debug, Clone)]
struct CliOptions {
    command: CliCommand,
    json: bool,
    watch: bool,
    interval_seconds: u64,
    interval_explicit: bool,
    providers: Vec<ProviderId>,
    profiles: Vec<String>,
    config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliCommand {
    Status,
    ProfilesList,
    Doctor,
    Help,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            command: CliCommand::Status,
            json: false,
            watch: false,
            interval_seconds: DEFAULT_INTERVAL_SECONDS,
            interval_explicit: false,
            providers: Vec::new(),
            profiles: Vec::new(),
            config_path: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorCheck {
    profile_id: String,
    provider: ProviderId,
    ok: bool,
    detail: String,
    provider_impact: String,
}

#[tokio::main]
async fn main() {
    let options = match parse_args(std::env::args().skip(1).collect()) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}\n");
            print_usage(true);
            process::exit(2);
        }
    };

    if options.command == CliCommand::Help {
        print_usage(false);
        return;
    }

    match options.command {
        CliCommand::ProfilesList => run_profiles_list(&options),
        CliCommand::Doctor => run_doctor(&options).await,
        CliCommand::Status => run_status(&options).await,
        CliCommand::Help => unreachable!("help returned above"),
    }
}

async fn run_status(options: &CliOptions) {
    let collect_options = match collect_options(options) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            process::exit(1);
        }
    };
    let client = AgentQuotaClient::new();
    let cache = ProviderUsageCache::default();

    loop {
        let snapshots = cache.collect(&client, collect_options.clone(), false).await;
        if options.json {
            let serialized = if options.watch {
                serde_json::to_string(&snapshots)
            } else {
                serde_json::to_string_pretty(&snapshots)
            };
            match serialized {
                Ok(output) => println!("{output}"),
                Err(error) => {
                    eprintln!("error: failed to serialize quota status: {error}");
                    process::exit(1);
                }
            }
        } else {
            print_table(&snapshots);
        }

        if !options.watch {
            if !snapshots.is_empty()
                && snapshots
                    .iter()
                    .all(|snapshot| snapshot.probe_status != ProbeStatus::Ok)
            {
                process::exit(ALL_PROBES_FAILED_EXIT_CODE);
            }
            break;
        }
        sleep(Duration::from_secs(options.interval_seconds)).await;
    }
}

fn run_profiles_list(options: &CliOptions) {
    let profiles = match load_profiles(options, true) {
        Ok(profiles) => profiles,
        Err(message) => {
            eprintln!("error: {message}");
            process::exit(1);
        }
    };
    if options.json {
        match serde_json::to_string_pretty(&profiles) {
            Ok(output) => println!("{output}"),
            Err(error) => {
                eprintln!("error: failed to serialize profiles: {error}");
                process::exit(1);
            }
        }
    } else {
        print_profiles(&profiles);
    }
}

async fn run_doctor(options: &CliOptions) {
    let profiles = match diagnostic_profiles(options) {
        Ok(profiles) => profiles,
        Err(message) => {
            eprintln!("error: {message}");
            process::exit(1);
        }
    };
    let checks = futures::future::join_all(profiles.iter().map(diagnose_profile)).await;

    if options.json {
        match serde_json::to_string_pretty(&checks) {
            Ok(output) => println!("{output}"),
            Err(error) => {
                eprintln!("error: failed to serialize diagnostics: {error}");
                process::exit(1);
            }
        }
    } else {
        println!("Agent Quota diagnostics");
        println!();
        for check in &checks {
            let marker = if check.ok { "OK" } else { "ACTION NEEDED" };
            println!(
                "[{marker}] {} ({})",
                check.profile_id,
                check.provider.label()
            );
            println!("  {}", check.detail);
            println!("  Provider impact: {}", check.provider_impact);
        }
        println!();
        println!("Doctor does not perform quota requests or send provider API messages.");
    }

    if checks.iter().any(|check| !check.ok) {
        process::exit(1);
    }
}

async fn diagnose_profile(profile: &ProviderProfile) -> DoctorCheck {
    match profile.provider {
        ProviderId::Codex => {
            let executable = profile
                .command_path
                .as_deref()
                .unwrap_or_else(|| Path::new("codex"));
            let mut command = Command::new(executable);
            command.arg("--version").envs(&profile.env);
            let result = timeout(Duration::from_secs(5), command.output()).await;
            let (ok, detail) = match result {
                Ok(Ok(output)) if output.status.success() => {
                    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    (
                        true,
                        if version.is_empty() {
                            format!("Codex executable found at `{}`.", executable.display())
                        } else {
                            format!("{version} (`{}`).", executable.display())
                        },
                    )
                }
                Ok(Ok(output)) => (
                    false,
                    format!(
                        "`{}` returned exit status {} for `--version`.",
                        executable.display(),
                        output.status
                    ),
                ),
                Ok(Err(error)) => (
                    false,
                    format!("Could not start `{}`: {error}", executable.display()),
                ),
                Err(_) => (
                    false,
                    format!(
                        "`{}` did not respond within 5 seconds.",
                        executable.display()
                    ),
                ),
            };
            DoctorCheck {
                profile_id: profile.id.clone(),
                provider: profile.provider,
                ok,
                detail,
                provider_impact:
                    "Quota checks start the local Codex app-server and do not submit a prompt."
                        .to_owned(),
            }
        }
        ProviderId::Claude => {
            let path = profile
                .credentials_path
                .clone()
                .or_else(|| default_claude_credentials_path().ok());
            let (ok, detail) = match path {
                Some(path) if path.is_file() => (
                    true,
                    format!("Claude Code credentials file found at `{}`.", path.display()),
                ),
                Some(path) => (
                    false,
                    format!(
                        "Claude Code credentials file was not found at `{}`. Sign in with Claude Code first.",
                        path.display()
                    ),
                ),
                None => (
                    false,
                    "Could not determine the current user's Claude Code credentials path."
                        .to_owned(),
                ),
            };
            DoctorCheck {
                profile_id: profile.id.clone(),
                provider: profile.provider,
                ok,
                detail,
                provider_impact: "Quota checks send a fixed one-token `hi` message to Anthropic; results are cached for five minutes.".to_owned(),
            }
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<CliOptions, String> {
    let mut options = CliOptions::default();
    let mut index = 0;

    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        options.command = CliCommand::Help;
        return Ok(options);
    }

    match args.first().map(String::as_str) {
        Some("watch") => {
            options.watch = true;
            index = 1;
        }
        Some("status") => index = 1,
        Some("doctor") => {
            options.command = CliCommand::Doctor;
            index = 1;
        }
        Some("profiles") => {
            if !matches!(args.get(1).map(String::as_str), Some("list")) {
                return Err("profiles requires the `list` subcommand".to_owned());
            }
            options.command = CliCommand::ProfilesList;
            index = 2;
        }
        Some(value) if !value.starts_with('-') => {
            return Err(format!("unknown command `{value}`"));
        }
        _ => {}
    }

    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                options.command = CliCommand::Help;
                return Ok(options);
            }
            "--json" => options.json = true,
            "--watch" => options.watch = true,
            "--provider" | "-p" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or_else(|| "--provider requires codex or claude".to_owned())?;
                let provider = ProviderId::from_name(raw)
                    .ok_or_else(|| format!("unknown provider `{raw}`; expected codex or claude"))?;
                if !options.providers.contains(&provider) {
                    options.providers.push(provider);
                }
            }
            "--profile" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or_else(|| "--profile requires a profile id".to_owned())?;
                if !options.profiles.contains(raw) {
                    options.profiles.push(raw.to_owned());
                }
            }
            "--config" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or_else(|| "--config requires a path".to_owned())?;
                options.config_path = Some(PathBuf::from(raw));
            }
            "--interval" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or_else(|| "--interval requires seconds".to_owned())?;
                options.interval_seconds = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid interval `{raw}`"))?;
                options.interval_explicit = true;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
        index += 1;
    }

    validate_options(&options)?;
    Ok(options)
}

fn validate_options(options: &CliOptions) -> Result<(), String> {
    if !options.profiles.is_empty() && options.config_path.is_none() {
        return Err("--profile requires --config".to_owned());
    }
    if options.interval_explicit && !options.watch {
        return Err("--interval can only be used with watch or --watch".to_owned());
    }
    if options.watch && options.interval_seconds < MINIMUM_INTERVAL_SECONDS {
        return Err(format!(
            "--interval must be at least {MINIMUM_INTERVAL_SECONDS} seconds to protect provider quota"
        ));
    }
    if options.command == CliCommand::ProfilesList && options.config_path.is_none() {
        return Err("profiles list requires --config".to_owned());
    }
    if options.command == CliCommand::ProfilesList && options.watch {
        return Err("profiles list does not support --watch".to_owned());
    }
    if options.command == CliCommand::Doctor && options.watch {
        return Err("doctor does not support --watch".to_owned());
    }
    Ok(())
}

fn collect_options(options: &CliOptions) -> Result<CollectUsageOptions, String> {
    if options.config_path.is_some() {
        return Ok(CollectUsageOptions::profiles(load_profiles(
            options, false,
        )?));
    }
    if options.providers.is_empty() {
        Ok(CollectUsageOptions::all())
    } else {
        Ok(CollectUsageOptions::providers(options.providers.clone()))
    }
}

fn diagnostic_profiles(options: &CliOptions) -> Result<Vec<ProviderProfile>, String> {
    if options.config_path.is_some() {
        load_profiles(options, false)
    } else {
        let providers = if options.providers.is_empty() {
            vec![ProviderId::Codex, ProviderId::Claude]
        } else {
            options.providers.clone()
        };
        Ok(providers
            .into_iter()
            .map(ProviderProfile::default_for_provider)
            .collect())
    }
}

fn load_profiles(
    options: &CliOptions,
    include_disabled: bool,
) -> Result<Vec<ProviderProfile>, String> {
    let path = options
        .config_path
        .as_ref()
        .ok_or_else(|| "--config is required when using profiles".to_owned())?;
    let config =
        AgentQuotaConfig::load(path).map_err(|error| format!("failed to load config: {error}"))?;
    let mut profiles = if include_disabled {
        config.profiles
    } else {
        config.profiles()
    };

    if !options.providers.is_empty() {
        profiles.retain(|profile| options.providers.contains(&profile.provider));
    }
    if !options.profiles.is_empty() {
        let known = profiles
            .iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();
        let missing = options
            .profiles
            .iter()
            .filter(|requested| !known.contains(&requested.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "unknown or disabled profile(s): {}; available profiles: {}",
                missing.join(", "),
                if known.is_empty() {
                    "(none)".to_owned()
                } else {
                    known.join(", ")
                }
            ));
        }
        profiles.retain(|profile| options.profiles.contains(&profile.id));
    }
    if profiles.is_empty() {
        return Err("no enabled profiles matched the requested filters".to_owned());
    }
    Ok(profiles)
}

fn print_usage(stderr: bool) {
    let lines = [
        "Agent Quota — local coding-agent quota status",
        "",
        "USAGE:",
        "  agent-quota [status] [--json] [--provider codex|claude]",
        "  agent-quota status --config <path> [--profile <id>]",
        "  agent-quota watch [--json] [--interval <seconds>] [filters]",
        "  agent-quota doctor [--json] [--config <path>] [filters]",
        "  agent-quota profiles list --config <path> [--json]",
        "",
        "NOTES:",
        "  Watch intervals must be at least 60 seconds and use a five-minute cache.",
        "  `watch --json` emits one compact JSON value per line (NDJSON).",
        "  Exit code 3 means every requested provider probe failed.",
    ];
    for line in lines {
        if stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}

fn print_profiles(profiles: &[ProviderProfile]) {
    println!("Profile              Provider      Enabled  Source");
    println!(
        "-------------------  ------------  -------  ----------------------------------------"
    );
    for profile in profiles {
        let source = profile
            .credentials_path
            .as_ref()
            .or(profile.command_path.as_ref())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "default local account".to_owned());
        println!(
            "{:<19}  {:<12}  {:<7}  {}",
            profile.display_name(),
            profile.provider.label(),
            if profile.enabled { "yes" } else { "no" },
            source
        );
    }
}

fn print_table(snapshots: &[ProviderUsageSnapshot]) {
    println!("Provider/profile      Plan/account             Quota");
    println!(
        "--------------------  -----------------------  ----------------------------------------"
    );
    if snapshots.is_empty() {
        println!("No provider profiles selected.");
        return;
    }

    for snapshot in snapshots {
        let account = match (&snapshot.plan, &snapshot.account_label) {
            (Some(plan), Some(account)) => format!("{plan} / {account}"),
            (Some(plan), None) => plan.clone(),
            (None, Some(account)) => account.clone(),
            (None, None) if snapshot.probe_status == ProbeStatus::Ok => "Signed in".to_owned(),
            (None, None) => "Unavailable".to_owned(),
        };
        let usage = if let Some(error) = &snapshot.error {
            format!(
                "{}: {}",
                probe_status_label(snapshot.probe_status),
                error.message
            )
        } else if snapshot.windows.is_empty() {
            snapshot
                .message
                .clone()
                .unwrap_or_else(|| "Quota state unknown".to_owned())
        } else {
            let windows = snapshot
                .windows
                .iter()
                .map(|window| {
                    let remaining = window
                        .remaining_percent
                        .map(|value| format!("{value:.0}% left"))
                        .unwrap_or_else(|| "remaining unknown".to_owned());
                    match window.resets_at_epoch_seconds {
                        Some(reset) => {
                            format!(
                                "{}: {remaining}, resets {}",
                                window.label,
                                format_reset(reset)
                            )
                        }
                        None => format!("{}: {remaining}", window.label),
                    }
                })
                .collect::<Vec<_>>()
                .join(" / ");
            if snapshot.quota_state == QuotaState::Exhausted {
                format!("EXHAUSTED — {windows}")
            } else {
                windows
            }
        };
        println!("{:<20}  {:<23}  {usage}", snapshot.profile_name, account);
    }
}

fn probe_status_label(status: ProbeStatus) -> &'static str {
    match status {
        ProbeStatus::Ok => "ok",
        ProbeStatus::AuthenticationRequired => "authentication required",
        ProbeStatus::Unsupported => "unsupported",
        ProbeStatus::TransientError => "temporary failure",
        ProbeStatus::InvalidResponse => "invalid provider response",
        ProbeStatus::InvalidConfiguration => "invalid configuration",
    }
}

fn format_reset(epoch_seconds: i64) -> String {
    let absolute = OffsetDateTime::from_unix_timestamp(epoch_seconds)
        .ok()
        .map(|instant| {
            let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
            instant
                .to_offset(offset)
                .format(&Rfc3339)
                .unwrap_or_else(|_| epoch_seconds.to_string())
        })
        .unwrap_or_else(|| epoch_seconds.to_string());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default();
    let relative = relative_duration(epoch_seconds.saturating_sub(now));
    format!("{relative} ({absolute})")
}

fn relative_duration(seconds: i64) -> String {
    if seconds <= 0 {
        return "now".to_owned();
    }
    let minutes = (seconds + 59) / 60;
    if minutes < 60 {
        return format!("in {minutes}m");
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours < 24 {
        return if remaining_minutes == 0 {
            format!("in {hours}h")
        } else {
            format!("in {hours}h {remaining_minutes}m")
        };
    }
    let days = hours / 24;
    let remaining_hours = hours % 24;
    if remaining_hours == 0 {
        format!("in {days}d")
    } else {
        format!("in {days}d {remaining_hours}h")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_with_config_profile_and_provider() {
        let options = parse_args(vec![
            "status".to_owned(),
            "--config".to_owned(),
            "agent-quota.toml".to_owned(),
            "--profile".to_owned(),
            "claude-work".to_owned(),
            "--provider".to_owned(),
            "claude".to_owned(),
            "--json".to_owned(),
        ])
        .expect("status args should parse");

        assert_eq!(options.command, CliCommand::Status);
        assert!(options.json);
        assert_eq!(options.profiles, vec!["claude-work"]);
        assert_eq!(options.providers, vec![ProviderId::Claude]);
    }

    #[test]
    fn parses_doctor_and_help() {
        let doctor =
            parse_args(vec!["doctor".to_owned(), "--json".to_owned()]).expect("doctor parses");
        assert_eq!(doctor.command, CliCommand::Doctor);
        let help = parse_args(vec!["--help".to_owned()]).expect("help parses");
        assert_eq!(help.command, CliCommand::Help);
    }

    #[test]
    fn rejects_unsafe_or_irrelevant_intervals() {
        let unsafe_interval = parse_args(vec![
            "watch".to_owned(),
            "--interval".to_owned(),
            "0".to_owned(),
        ])
        .expect_err("zero interval should fail");
        assert!(unsafe_interval.contains("at least 60"));

        let status_interval = parse_args(vec![
            "status".to_owned(),
            "--interval".to_owned(),
            "300".to_owned(),
        ])
        .expect_err("status interval should fail");
        assert!(status_interval.contains("only be used with watch"));
    }

    #[test]
    fn rejects_profile_without_config() {
        let error = parse_args(vec![
            "status".to_owned(),
            "--profile".to_owned(),
            "work".to_owned(),
        ])
        .expect_err("profile without config should fail");
        assert_eq!(error, "--profile requires --config");
    }

    #[test]
    fn formats_relative_durations() {
        assert_eq!(relative_duration(59), "in 1m");
        assert_eq!(relative_duration(3_900), "in 1h 5m");
        assert_eq!(relative_duration(90_000), "in 1d 1h");
        assert_eq!(relative_duration(-1), "now");
    }
}
