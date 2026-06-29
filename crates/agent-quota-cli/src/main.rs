use std::time::Duration;
use std::{path::PathBuf, process};

use agent_quota_core::{
    collect_usage, AgentQuotaConfig, CollectUsageOptions, ProviderId, ProviderProfile,
    ProviderUsageSnapshot,
};
use tokio::time::sleep;

#[derive(Debug, Clone)]
struct CliOptions {
    command: CliCommand,
    json: bool,
    watch: bool,
    interval_seconds: u64,
    providers: Vec<ProviderId>,
    profiles: Vec<String>,
    config_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliCommand {
    Status,
    ProfilesList,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            command: CliCommand::Status,
            json: false,
            watch: false,
            interval_seconds: 300,
            providers: Vec::new(),
            profiles: Vec::new(),
            config_path: None,
        }
    }
}

#[tokio::main]
async fn main() {
    let options = match parse_args(std::env::args().skip(1).collect()) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n");
            print_usage();
            process::exit(2);
        }
    };

    if options.command == CliCommand::ProfilesList {
        let profiles = match load_configured_profiles(&options) {
            Ok(profiles) => profiles,
            Err(message) => {
                eprintln!("{message}");
                process::exit(1);
            }
        };
        if options.json {
            match serde_json::to_string_pretty(&profiles) {
                Ok(output) => println!("{output}"),
                Err(error) => {
                    eprintln!("failed to serialize profiles: {error}");
                    process::exit(1);
                }
            }
        } else {
            print_profiles(&profiles);
        }
        return;
    }

    let collect_options = match collect_options(&options) {
        Ok(collect_options) => collect_options,
        Err(message) => {
            eprintln!("{message}");
            process::exit(1);
        }
    };

    loop {
        let snapshots = collect_usage(collect_options.clone()).await;
        if options.json {
            match serde_json::to_string_pretty(&snapshots) {
                Ok(output) => println!("{output}"),
                Err(error) => {
                    eprintln!("failed to serialize quota status: {error}");
                    process::exit(1);
                }
            }
        } else {
            print_table(&snapshots);
        }

        if !options.watch {
            break;
        }
        sleep(Duration::from_secs(options.interval_seconds)).await;
    }
}

fn parse_args(args: Vec<String>) -> Result<CliOptions, String> {
    let mut options = CliOptions::default();
    let mut index = 0;

    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        print_usage();
        process::exit(0);
    }

    if matches!(args.first().map(String::as_str), Some("watch")) {
        options.watch = true;
        index = 1;
    } else if matches!(args.first().map(String::as_str), Some("status")) {
        index = 1;
    } else if matches!(args.first().map(String::as_str), Some("profiles")) {
        if !matches!(args.get(1).map(String::as_str), Some("list")) {
            return Err("profiles requires the `list` subcommand".to_owned());
        }
        options.command = CliCommand::ProfilesList;
        index = 2;
    }

    while index < args.len() {
        match args[index].as_str() {
            "--json" => options.json = true,
            "--watch" => options.watch = true,
            "--provider" | "-p" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or_else(|| "--provider requires codex or claude".to_owned())?;
                let provider = ProviderId::from_name(raw)
                    .ok_or_else(|| format!("unknown provider `{raw}`; expected codex or claude"))?;
                options.providers.push(provider);
            }
            "--profile" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or_else(|| "--profile requires a profile id".to_owned())?;
                options.profiles.push(raw.to_owned());
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
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
        index += 1;
    }

    Ok(options)
}

fn collect_options(options: &CliOptions) -> Result<CollectUsageOptions, String> {
    if options.config_path.is_some() || !options.profiles.is_empty() {
        let profiles = load_configured_profiles(options)?;
        return Ok(CollectUsageOptions::profiles(profiles));
    }

    Ok(CollectUsageOptions::providers(options.providers.clone()))
}

fn load_configured_profiles(options: &CliOptions) -> Result<Vec<ProviderProfile>, String> {
    let path = options
        .config_path
        .as_ref()
        .ok_or_else(|| "--config is required when using profiles".to_owned())?;
    let config = AgentQuotaConfig::load(path)
        .map_err(|error| format!("failed to read config at {}: {error}", path.display()))?;
    let mut profiles = config.profiles();

    if !options.providers.is_empty() {
        profiles.retain(|profile| options.providers.contains(&profile.provider));
    }
    if !options.profiles.is_empty() {
        profiles.retain(|profile| options.profiles.contains(&profile.id));
    }
    if profiles.is_empty() {
        return Err("no enabled profiles matched the requested filters".to_owned());
    }

    Ok(profiles)
}

fn print_usage() {
    eprintln!("agent-quota status [--json] [--provider codex|claude]");
    eprintln!(
        "agent-quota status --config agent-quota.toml [--profile id] [--provider codex|claude]"
    );
    eprintln!("agent-quota watch [--json] [--interval seconds] [--config agent-quota.toml]");
    eprintln!("agent-quota profiles list --config agent-quota.toml [--json]");
}

fn print_profiles(profiles: &[ProviderProfile]) {
    println!("Profile              Provider      Source");
    println!("-------------------  ------------  ----------------------------------------");
    for profile in profiles {
        let source = profile
            .credentials_path
            .as_ref()
            .or(profile.command_path.as_ref())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "default CLI account".to_owned());
        println!(
            "{:<19}  {:<12}  {}",
            profile.display_name(),
            profile.provider.label(),
            source
        );
    }
}

fn print_table(snapshots: &[ProviderUsageSnapshot]) {
    println!("Provider/profile      Plan/account        Usage");
    println!("--------------------  ------------------  ----------------------------------------");
    for snapshot in snapshots {
        let profile = snapshot
            .profile_name
            .as_deref()
            .unwrap_or(snapshot.provider_name.as_str());
        let account = snapshot
            .account_label
            .as_deref()
            .or(snapshot.plan.as_deref())
            .unwrap_or({
                if snapshot.status == agent_quota_core::ProviderUsageStatus::Available {
                    "Signed in"
                } else {
                    "Unavailable"
                }
            });
        let usage = if snapshot.windows.is_empty() {
            snapshot
                .message
                .as_deref()
                .unwrap_or("usage unavailable")
                .to_owned()
        } else {
            snapshot
                .windows
                .iter()
                .map(|window| {
                    let remaining = window
                        .remaining_percent
                        .map(|value| format!("{value:.0}% left"))
                        .unwrap_or_else(|| "n/a".to_owned());
                    match window.resets_at {
                        Some(reset) => format!("{}: {remaining}, resets at {reset}", window.label),
                        None => format!("{}: {remaining}", window.label),
                    }
                })
                .collect::<Vec<_>>()
                .join(" / ")
        };
        println!("{profile:<20}  {account:<18}  {usage}");
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
        assert_eq!(options.config_path, Some(PathBuf::from("agent-quota.toml")));
        assert_eq!(options.profiles, vec!["claude-work"]);
        assert_eq!(options.providers, vec![ProviderId::Claude]);
    }

    #[test]
    fn parses_profiles_list() {
        let options = parse_args(vec![
            "profiles".to_owned(),
            "list".to_owned(),
            "--config".to_owned(),
            "agent-quota.toml".to_owned(),
        ])
        .expect("profiles list args should parse");

        assert_eq!(options.command, CliCommand::ProfilesList);
        assert_eq!(options.config_path, Some(PathBuf::from("agent-quota.toml")));
    }

    #[test]
    fn rejects_profile_filter_without_value() {
        let error = parse_args(vec!["status".to_owned(), "--profile".to_owned()])
            .expect_err("missing profile id should fail");

        assert_eq!(error, "--profile requires a profile id");
    }
}
