use std::time::Duration;

use agent_quota_core::{collect_usage, CollectUsageOptions, ProviderId, ProviderUsageSnapshot};
use tokio::time::sleep;

#[derive(Debug, Clone)]
struct CliOptions {
    json: bool,
    watch: bool,
    interval_seconds: u64,
    providers: Vec<ProviderId>,
}

impl Default for CliOptions {
    fn default() -> Self {
        Self {
            json: false,
            watch: false,
            interval_seconds: 300,
            providers: Vec::new(),
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
            std::process::exit(2);
        }
    };

    loop {
        let snapshots =
            collect_usage(CollectUsageOptions::providers(options.providers.clone())).await;
        if options.json {
            match serde_json::to_string_pretty(&snapshots) {
                Ok(output) => println!("{output}"),
                Err(error) => {
                    eprintln!("failed to serialize quota status: {error}");
                    std::process::exit(1);
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
        std::process::exit(0);
    }

    if matches!(args.first().map(String::as_str), Some("watch")) {
        options.watch = true;
        index = 1;
    } else if matches!(args.first().map(String::as_str), Some("status")) {
        index = 1;
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

fn print_usage() {
    eprintln!("agent-quota status [--json] [--provider codex|claude]");
    eprintln!("agent-quota watch [--json] [--interval seconds] [--provider codex|claude]");
}

fn print_table(snapshots: &[ProviderUsageSnapshot]) {
    println!("Provider      Plan/status        Usage");
    println!("------------  -----------------  ----------------------------------------");
    for snapshot in snapshots {
        let account = snapshot.plan.as_deref().unwrap_or({
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
        println!("{:<12}  {:<17}  {}", snapshot.provider_name, account, usage);
    }
}
