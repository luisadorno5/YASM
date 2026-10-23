//! CLI client (`list`, `status`, `start`, `stop`, `restart`, `logs`, `reload`) communicating
//! with the local Control REST API (Phase 2 §8).

use crate::api::{OutputResponse, ReloadResponse};
use crate::constants::{cli_exit_codes, headers};
use crate::supervisor::{ApplicationSnapshot, ApplicationState};
use clap::{Parser, Subcommand};
use reqwest::StatusCode;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "Node.Application.Service.Wrapper",
    about = "Windows Service process supervisor and CLI for Node.js and standalone applications",
    version
)]
pub struct CliArgs {
    /// Override the Control API URL (defaults to ControlApi:Url in appsettings.json or http://127.0.0.1:5080).
    #[arg(long, global = true)]
    pub url: Option<String>,

    /// Internal helper flag: send Ctrl+C to a child PID and exit immediately.
    #[arg(long = "send-ctrl-c", hide = true)]
    pub send_ctrl_c: Option<u32>,

    #[command(subcommand)]
    pub command: Option<CliCommand>,
}

#[derive(Debug, Subcommand, Clone)]
pub enum CliCommand {
    /// List all applications in a table.
    List,
    /// Show detailed status for one application.
    Status {
        /// Application name.
        name: String,
    },
    /// Start an application (waits for Running or Failed unless --no-wait is passed).
    Start {
        /// Application name.
        name: String,
        /// Return as soon as the command is accepted without waiting for final state.
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
    /// Stop an application gracefully (waits for Stopped unless --no-wait is passed).
    Stop {
        /// Application name.
        name: String,
        /// Return as soon as the command is accepted without waiting for final state.
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
    /// Restart an application (waits for Running or Failed unless --no-wait is passed).
    Restart {
        /// Application name.
        name: String,
        /// Return as soon as the command is accepted without waiting for final state.
        #[arg(long = "no-wait")]
        no_wait: bool,
    },
    /// Print recent stdout/stderr lines for an application.
    Logs {
        /// Application name.
        name: String,
        /// Number of recent lines to display.
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// Reload appsettings.json without touching unchanged applications.
    Reload,
}

/// Resolve the Control API base URL from `--url` or `appsettings.json` in `base_dir`.
pub fn resolve_control_api_url(url_override: Option<&str>, base_dir: &Path) -> String {
    if let Some(u) = url_override.filter(|s| !s.trim().is_empty()) {
        return u.trim().trim_end_matches('/').to_string();
    }

    let cfg_path = base_dir.join("appsettings.json");
    if let Ok(text) = fs::read_to_string(&cfg_path) {
        if let Ok(val) = serde_json::from_str::<Value>(&text) {
            if let Some(url_str) = val
                .as_object()
                .and_then(|o| {
                    o.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("ControlApi"))
                        .map(|(_, v)| v)
                })
                .and_then(|v| v.as_object())
                .and_then(|o| {
                    o.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("Url"))
                        .map(|(_, v)| v)
                })
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                return url_str.trim().trim_end_matches('/').to_string();
            }
        }
    }

    "http://127.0.0.1:5080".to_string()
}

/// Execute a CLI command against the Control API and return the CLI exit code (Phase 2 §8).
pub async fn run_cli_command(
    command: CliCommand,
    url_override: Option<&str>,
    base_dir: &Path,
) -> i32 {
    let base_url = resolve_control_api_url(url_override, base_dir);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to initialize HTTP client: {e}");
            return cli_exit_codes::COMMAND_FAILED;
        }
    };

    let current_user = env::var("USERNAME")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "local".to_string());

    match command {
        CliCommand::List => {
            let endpoint = format!("{base_url}/api/applications");
            let resp = match client.get(&endpoint).send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Service is not reachable at {base_url}: {e}");
                    return cli_exit_codes::SERVICE_UNREACHABLE;
                }
            };
            if !resp.status().is_success() {
                eprintln!("Request failed with HTTP {}", resp.status());
                return cli_exit_codes::COMMAND_FAILED;
            }
            match resp.json::<Vec<ApplicationSnapshot>>().await {
                Ok(apps) => {
                    print_applications_table(&apps);
                    cli_exit_codes::SUCCESS
                }
                Err(e) => {
                    eprintln!("Failed to parse response: {e}");
                    cli_exit_codes::COMMAND_FAILED
                }
            }
        }
        CliCommand::Status { name } => {
            match fetch_app_snapshot(&client, &base_url, &name).await {
                Ok(snap) => {
                    print_single_status(&snap);
                    cli_exit_codes::SUCCESS
                }
                Err(code) => code,
            }
        }
        CliCommand::Start { name, no_wait } => {
            run_lifecycle_action(
                &client,
                &base_url,
                &name,
                "start",
                &current_user,
                no_wait,
                TargetLifecycle::Running,
            )
            .await
        }
        CliCommand::Stop { name, no_wait } => {
            run_lifecycle_action(
                &client,
                &base_url,
                &name,
                "stop",
                &current_user,
                no_wait,
                TargetLifecycle::Stopped,
            )
            .await
        }
        CliCommand::Restart { name, no_wait } => {
            run_lifecycle_action(
                &client,
                &base_url,
                &name,
                "restart",
                &current_user,
                no_wait,
                TargetLifecycle::Running,
            )
            .await
        }
        CliCommand::Logs { name, lines } => {
            let endpoint = format!("{base_url}/api/applications/{name}/output?lines={lines}");
            let resp = match client.get(&endpoint).send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Service is not reachable at {base_url}: {e}");
                    return cli_exit_codes::SERVICE_UNREACHABLE;
                }
            };
            if resp.status() == StatusCode::NOT_FOUND {
                eprintln!("Unknown application '{name}'.");
                return cli_exit_codes::UNKNOWN_APPLICATION;
            }
            if !resp.status().is_success() {
                eprintln!("Request failed with HTTP {}", resp.status());
                return cli_exit_codes::COMMAND_FAILED;
            }
            match resp.json::<OutputResponse>().await {
                Ok(out) => {
                    for line in out.lines {
                        println!("{line}");
                    }
                    cli_exit_codes::SUCCESS
                }
                Err(e) => {
                    eprintln!("Failed to parse output response: {e}");
                    cli_exit_codes::COMMAND_FAILED
                }
            }
        }
        CliCommand::Reload => {
            let endpoint = format!("{base_url}/api/configuration/reload");
            let resp = match client
                .post(&endpoint)
                .header(headers::WRAPPER_REQUEST, headers::WRAPPER_REQUEST_VALUE)
                .header(headers::WRAPPER_SOURCE, "cli")
                .header(headers::WRAPPER_USER, &current_user)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Service is not reachable at {base_url}: {e}");
                    return cli_exit_codes::SERVICE_UNREACHABLE;
                }
            };

            if resp.status().is_success() {
                match resp.json::<ReloadResponse>().await {
                    Ok(reload) => {
                        println!("{}", reload.summary);
                        if !reload.requires_service_restart.is_empty() {
                            println!(
                                "Note: changes to [{}] require a service restart.",
                                reload.requires_service_restart.join(", ")
                            );
                        }
                        cli_exit_codes::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("Failed to parse reload response: {e}");
                        cli_exit_codes::COMMAND_FAILED
                    }
                }
            } else {
                let text = resp.text().await.unwrap_or_default();
                if let Ok(problem) = serde_json::from_str::<Value>(&text) {
                    if let Some(errs) = problem.get("errors").and_then(|v| v.as_array()) {
                        eprintln!("Configuration validation failed:");
                        for err in errs {
                            if let Some(s) = err.as_str() {
                                eprintln!("  - {s}");
                            }
                        }
                        return cli_exit_codes::COMMAND_FAILED;
                    }
                    if let Some(detail) = problem.get("detail").and_then(|v| v.as_str()) {
                        eprintln!("Reload failed: {detail}");
                        return cli_exit_codes::COMMAND_FAILED;
                    }
                }
                eprintln!("Reload failed: {text}");
                cli_exit_codes::COMMAND_FAILED
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetLifecycle {
    Running,
    Stopped,
}

async fn run_lifecycle_action(
    client: &reqwest::Client,
    base_url: &str,
    name: &str,
    action: &str,
    current_user: &str,
    no_wait: bool,
    target: TargetLifecycle,
) -> i32 {
    let endpoint = format!("{base_url}/api/applications/{name}/{action}");
    let resp = match client
        .post(&endpoint)
        .header(headers::WRAPPER_REQUEST, headers::WRAPPER_REQUEST_VALUE)
        .header(headers::WRAPPER_SOURCE, "cli")
        .header(headers::WRAPPER_USER, current_user)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Service is not reachable at {base_url}: {e}");
            return cli_exit_codes::SERVICE_UNREACHABLE;
        }
    };

    if resp.status() == StatusCode::NOT_FOUND {
        eprintln!("Unknown application '{name}'.");
        return cli_exit_codes::UNKNOWN_APPLICATION;
    }
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        eprintln!("Command '{action}' failed: {body}");
        return cli_exit_codes::COMMAND_FAILED;
    }

    let initial_snap: ApplicationSnapshot = match resp.json().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to parse response: {e}");
            return cli_exit_codes::COMMAND_FAILED;
        }
    };

    if no_wait {
        println!(
            "{}: {} (accepted '{action}')",
            initial_snap.name,
            initial_snap.state.as_str()
        );
        return cli_exit_codes::SUCCESS;
    }

    // Poll until target final state is reached (up to 90 seconds)
    let start_wait = Instant::now();
    let max_wait = Duration::from_secs(90);

    loop {
        let snap = match fetch_app_snapshot(client, base_url, name).await {
            Ok(s) => s,
            Err(code) => return code,
        };

        match target {
            TargetLifecycle::Running => match snap.state {
                ApplicationState::Running => {
                    println!(
                        "{}: Running (PID {})",
                        snap.name,
                        snap.pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "-".to_string())
                    );
                    return cli_exit_codes::SUCCESS;
                }
                ApplicationState::Failed | ApplicationState::Stopped => {
                    eprintln!(
                        "{}: {} ({})",
                        snap.name,
                        snap.state.as_str(),
                        snap.last_error.as_deref().unwrap_or("failed to start")
                    );
                    return cli_exit_codes::COMMAND_FAILED;
                }
                _ => {}
            },
            TargetLifecycle::Stopped => match snap.state {
                ApplicationState::Stopped => {
                    println!("{}: Stopped", snap.name);
                    return cli_exit_codes::SUCCESS;
                }
                ApplicationState::Failed => {
                    eprintln!("{}: Failed", snap.name);
                    return cli_exit_codes::COMMAND_FAILED;
                }
                _ => {}
            },
        }

        if start_wait.elapsed() >= max_wait {
            eprintln!("Timed out waiting for application '{name}' to complete '{action}'.");
            return cli_exit_codes::COMMAND_FAILED;
        }

        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn fetch_app_snapshot(
    client: &reqwest::Client,
    base_url: &str,
    name: &str,
) -> Result<ApplicationSnapshot, i32> {
    let endpoint = format!("{base_url}/api/applications/{name}");
    let resp = match client.get(&endpoint).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Service is not reachable at {base_url}: {e}");
            return Err(cli_exit_codes::SERVICE_UNREACHABLE);
        }
    };
    if resp.status() == StatusCode::NOT_FOUND {
        eprintln!("Unknown application '{name}'.");
        return Err(cli_exit_codes::UNKNOWN_APPLICATION);
    }
    if !resp.status().is_success() {
        eprintln!("Request failed with HTTP {}", resp.status());
        return Err(cli_exit_codes::COMMAND_FAILED);
    }
    resp.json::<ApplicationSnapshot>().await.map_err(|e| {
        eprintln!("Failed to parse application snapshot: {e}");
        cli_exit_codes::COMMAND_FAILED
    })
}

/// Format applications as a table with the same columns as the dashboard (Phase 2 §8).
pub fn format_applications_table(apps: &[ApplicationSnapshot]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<18} {:<12} {:<8} {:<12} {:<9} {:<12} {:<18} {}\n",
        "NAME", "STATE", "PID", "UPTIME", "RESTARTS", "LAST EXIT", "DESIRED", "LAST ERROR"
    ));
    out.push_str(&"-".repeat(110));
    out.push('\n');

    for app in apps {
        let pid_str = app
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        let uptime_str = app.uptime.clone().unwrap_or_else(|| "-".to_string());
        let last_exit_str = app
            .last_exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string());
        let source_tag = match app.desired_state_source {
            crate::state_store::DesiredStateSource::Manual => "manual",
            crate::state_store::DesiredStateSource::AutoStart => "auto",
        };
        let desired_str = format!("{} ({source_tag})", app.desired_state.as_str());
        let err_str = app.last_error.as_deref().unwrap_or("-");

        out.push_str(&format!(
            "{:<18} {:<12} {:<8} {:<12} {:<9} {:<12} {:<18} {}\n",
            app.name,
            app.state.as_str(),
            pid_str,
            uptime_str,
            app.restart_count,
            last_exit_str,
            desired_str,
            err_str
        ));
    }

    out
}

fn print_applications_table(apps: &[ApplicationSnapshot]) {
    print!("{}", format_applications_table(apps));
}

fn print_single_status(app: &ApplicationSnapshot) {
    let source_tag = match app.desired_state_source {
        crate::state_store::DesiredStateSource::Manual => "manual",
        crate::state_store::DesiredStateSource::AutoStart => "auto",
    };
    println!("Name:          {}", app.name);
    println!("State:         {}", app.state.as_str());
    println!(
        "PID:           {}",
        app.pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "Uptime:        {}",
        app.uptime.as_deref().unwrap_or("-")
    );
    println!("Restarts:      {}", app.restart_count);
    println!(
        "Last Exit:     {}",
        app.last_exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "Desired State: {} ({source_tag})",
        app.desired_state.as_str()
    );
    println!(
        "Last Error:    {}",
        app.last_error.as_deref().unwrap_or("-")
    );
}
