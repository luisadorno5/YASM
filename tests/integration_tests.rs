//! Comprehensive integration tests covering Phase 1 (§11, §13) and Phase 2 (§12):
//! - Multi-app supervision side-by-side
//! - Graceful Ctrl+C / SIGINT stop (exit 130) vs forced kill-tree after ShutdownTimeout
//! - Crash-loop isolation (one app goes to `Failed` while sibling stays `Running`)
//! - Stdout/stderr capture, ANSI stripping, and crash context ring buffer
//! - Startup HTTP health check polling
//! - Live configuration reload (added, removed, changed, unchanged)
//! - Control API endpoints, RFC 7807 Problem Details, anti-CSRF (`X-Wrapper-Request: 1`), and Host header DNS-rebinding checks
//! - Early startup validation failure exit code `13`

use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use node_application_service_wrapper::api::build_router;
use node_application_service_wrapper::config::{
    load_and_validate_from_dir, LogLevel, ResolvedSerilogOptions,
};
use node_application_service_wrapper::constants::{exit_codes, headers};
use node_application_service_wrapper::logging::WrapperLogger;
use node_application_service_wrapper::manager::ApplicationManager;
use node_application_service_wrapper::service::run_host;
use node_application_service_wrapper::state_store::{DesiredState, DesiredStateStore};
use node_application_service_wrapper::supervisor::ApplicationState;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tower::ServiceExt;

fn find_node_binary() -> String {
    for candidate in [
        "/opt/homebrew/bin/node",
        "/usr/local/bin/node",
        "/usr/bin/node",
    ] {
        if Path::new(candidate).is_file() {
            return candidate.to_string();
        }
    }
    "node".to_string()
}

fn create_manager_from_json(tmp: &TempDir, cfg_json: &Value) -> (ApplicationManager, PathBuf) {
    let wrapper_dir = tmp.path().join("wrapper");
    fs::create_dir_all(&wrapper_dir).unwrap();
    fs::write(
        wrapper_dir.join("appsettings.json"),
        serde_json::to_string_pretty(cfg_json).unwrap(),
    )
    .unwrap();

    let validated = load_and_validate_from_dir(&wrapper_dir, None).expect("config should be valid");
    let logger = WrapperLogger::initialize(
        ResolvedSerilogOptions {
            log_directory: validated.serilog.log_directory.clone(),
            minimum_level: LogLevel::Information,
            retained_file_count_limit: 5,
            file_size_limit_bytes: 10 * 1024 * 1024,
            crash_context_lines: 25,
        },
        false,
        &wrapper_dir,
    )
    .ok()
    .unwrap();

    let (state_store, _) = DesiredStateStore::load(validated.state_file.clone());
    let manager = ApplicationManager::new(validated, state_store, logger, None);
    (manager, wrapper_dir)
}

async fn wait_for_app_state(
    manager: &ApplicationManager,
    app_name: &str,
    expected: ApplicationState,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(snap) = manager.get_snapshot(app_name) {
            if snap.state == expected {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    false
}

#[tokio::test]
async fn api_security_should_enforce_host_header_and_anti_csrf_header() {
    // Arrange
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("apps").join("storefront");
    fs::create_dir_all(&app_dir).unwrap();
    let node_bin = find_node_binary();

    let cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "Arguments": "-e \"setInterval(() => {}, 1000)\"",
            "ShutdownTimeout": "0.00:00:02",
            "AutoStart": false,
            "RestartPolicy": { "InitialDelay": "0.00:00:01", "MaxDelay": "0.00:00:05", "MaxRestarts": 2, "RestartWindow": "0.00:01:00" },
            "HealthCheck": { "Enabled": false }
        },
        "Applications": {
            "storefront": { "WorkingDirectory": app_dir.to_string_lossy() }
        },
        "ControlApi": { "Url": "http://127.0.0.1:5080" }
    });

    let (manager, _) = create_manager_from_json(&tmp, &cfg);
    let router = build_router(manager, 5080);

    // Act 1: Request with a non-loopback Host header (DNS-rebinding attempt)
    let bad_host_req = Request::builder()
        .method(Method::GET)
        .uri("/api/applications")
        .header(header::HOST, "evil.attacker.com:5080")
        .body(Body::empty())
        .unwrap();
    let bad_host_resp = router.clone().oneshot(bad_host_req).await.unwrap();

    // Assert 1: 403 Forbidden
    assert_eq!(bad_host_resp.status(), StatusCode::FORBIDDEN);

    // Act 2: POST without X-Wrapper-Request: 1 (CSRF attempt)
    let csrf_req = Request::builder()
        .method(Method::POST)
        .uri("/api/applications/storefront/start")
        .header(header::HOST, "127.0.0.1:5080")
        .body(Body::empty())
        .unwrap();
    let csrf_resp = router.clone().oneshot(csrf_req).await.unwrap();

    // Assert 2: 403 Forbidden
    assert_eq!(csrf_resp.status(), StatusCode::FORBIDDEN);

    // Act 3: GET / dashboard with valid loopback Host
    let dash_req = Request::builder()
        .method(Method::GET)
        .uri("/")
        .header(header::HOST, "127.0.0.1:5080")
        .body(Body::empty())
        .unwrap();
    let dash_resp = router.clone().oneshot(dash_req).await.unwrap();

    // Assert 3: 200 OK with security headers
    assert_eq!(dash_resp.status(), StatusCode::OK);
    assert_eq!(
        dash_resp
            .headers()
            .get(header::X_FRAME_OPTIONS)
            .unwrap()
            .to_str()
            .unwrap(),
        "DENY"
    );
    assert_eq!(
        dash_resp
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .unwrap()
            .to_str()
            .unwrap(),
        "nosniff"
    );

    // Act 4: Unknown application returns 404 Problem Details
    let not_found_req = Request::builder()
        .method(Method::GET)
        .uri("/api/applications/nonexistent")
        .header(header::HOST, "localhost:5080")
        .body(Body::empty())
        .unwrap();
    let not_found_resp = router.oneshot(not_found_req).await.unwrap();
    assert_eq!(not_found_resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        not_found_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );
}

#[tokio::test]
async fn supervisor_should_gracefully_stop_node_on_sigint_and_force_kill_when_ignored() {
    // Arrange
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("apps");
    fs::create_dir_all(&app_dir).unwrap();
    let node_bin = find_node_binary();

    let graceful_script = app_dir.join("graceful.js");
    fs::write(
        &graceful_script,
        r#"
        process.on('SIGINT', () => {
            console.log('\x1b[32mShutting down cleanly\x1b[0m');
            process.exit(130);
        });
        console.log('\x1b[36mServer started\x1b[0m');
        setInterval(() => {}, 1000);
        "#,
    )
    .unwrap();

    let stubborn_script = app_dir.join("stubborn.js");
    fs::write(
        &stubborn_script,
        r#"
        process.on('SIGINT', () => {
            console.error('Ignoring SIGINT');
        });
        setInterval(() => {}, 1000);
        "#,
    )
    .unwrap();

    let cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "WorkingDirectory": app_dir.to_string_lossy(),
            "ShutdownTimeout": "0.00:00:01",
            "RestartPolicy": { "InitialDelay": "0.00:00:01", "MaxDelay": "0.00:00:05", "MaxRestarts": 2, "RestartWindow": "0.00:01:00" },
            "HealthCheck": { "Enabled": false }
        },
        "Applications": {
            "graceful-app": { "Arguments": "graceful.js" },
            "stubborn-app": { "Arguments": "stubborn.js" }
        }
    });

    let (manager, _) = create_manager_from_json(&tmp, &cfg);

    // Act: Start both applications in parallel
    manager.start_all().await;
    assert!(
        wait_for_app_state(&manager, "graceful-app", ApplicationState::Running, Duration::from_secs(5)).await
    );
    assert!(
        wait_for_app_state(&manager, "stubborn-app", ApplicationState::Running, Duration::from_secs(5)).await
    );

    // Allow stdout to flush
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify ANSI codes were stripped in output buffer
    let out_lines = manager.get_output("graceful-app", 50).unwrap();
    assert!(out_lines.iter().any(|l| l.contains("Server started") && !l.contains("\x1b")));

    // Stop graceful-app only: stubborn-app must stay Running
    manager
        .stop_application("graceful-app", "cli", Some("tester"))
        .await
        .unwrap();
    assert!(
        wait_for_app_state(&manager, "graceful-app", ApplicationState::Stopped, Duration::from_secs(5)).await
    );

    let graceful_snap = manager.get_snapshot("graceful-app").unwrap();
    assert_eq!(graceful_snap.last_exit_code, Some(130));
    assert_eq!(graceful_snap.desired_state, DesiredState::Stopped);

    let stubborn_snap = manager.get_snapshot("stubborn-app").unwrap();
    assert_eq!(stubborn_snap.state, ApplicationState::Running);

    // Now stop stubborn-app (ignores SIGINT, so killed after 1s ShutdownTimeout)
    manager
        .stop_application("stubborn-app", "cli", Some("tester"))
        .await
        .unwrap();
    assert!(
        wait_for_app_state(&manager, "stubborn-app", ApplicationState::Stopped, Duration::from_secs(5)).await
    );
}

#[tokio::test]
async fn crash_looping_app_should_transition_to_failed_while_sibling_stays_running() {
    // Arrange
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("apps");
    fs::create_dir_all(&app_dir).unwrap();
    let node_bin = find_node_binary();

    let crash_script = app_dir.join("crash.js");
    fs::write(
        &crash_script,
        "console.error('Fatal startup error'); process.exit(1);\n",
    )
    .unwrap();

    let healthy_script = app_dir.join("healthy.js");
    fs::write(&healthy_script, "setInterval(() => {}, 1000);\n").unwrap();

    let cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "WorkingDirectory": app_dir.to_string_lossy(),
            "ShutdownTimeout": "0.00:00:01",
            "RestartPolicy": {
                "InitialDelay": "0.00:00:00.100",
                "MaxDelay": "0.00:00:00.200",
                "MaxRestarts": 2,
                "RestartWindow": "0.00:01:00"
            },
            "HealthCheck": { "Enabled": false }
        },
        "Applications": {
            "crashing-app": { "Arguments": "crash.js" },
            "healthy-app": { "Arguments": "healthy.js" }
        }
    });

    let (manager, _) = create_manager_from_json(&tmp, &cfg);

    // Act
    manager.start_all().await;

    // Assert: crashing-app reaches Failed after 2 restarts, while healthy-app stays Running
    assert!(
        wait_for_app_state(&manager, "crashing-app", ApplicationState::Failed, Duration::from_secs(5)).await
    );
    let crash_snap = manager.get_snapshot("crashing-app").unwrap();
    assert_eq!(crash_snap.state, ApplicationState::Failed);
    assert_eq!(crash_snap.restart_count, 2);
    assert_eq!(crash_snap.last_exit_code, Some(1));
    // DesiredState stays Running even when Failed (Phase 2 §5.3)
    assert_eq!(crash_snap.desired_state, DesiredState::Running);

    let healthy_snap = manager.get_snapshot("healthy-app").unwrap();
    assert_eq!(healthy_snap.state, ApplicationState::Running);

    // Service health report is Unhealthy because crashing-app has DesiredState=Running but State=Failed
    let health_report = manager.evaluate_service_health();
    assert!(!health_report.healthy);

    manager.stop_all().await;
}

#[tokio::test]
async fn live_configuration_reload_should_add_remove_and_update_apps_end_to_end() {
    // Arrange
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("apps");
    fs::create_dir_all(&app_dir).unwrap();
    let node_bin = find_node_binary();

    let worker_script = app_dir.join("worker.js");
    fs::write(
        &worker_script,
        r#"
        process.on('SIGINT', () => process.exit(130));
        console.log('Worker running with PORT=' + (process.env.PORT || 'none'));
        setInterval(() => {}, 1000);
        "#,
    )
    .unwrap();

    let initial_cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "WorkingDirectory": app_dir.to_string_lossy(),
            "Arguments": "worker.js",
            "ShutdownTimeout": "0.00:00:02",
            "RestartPolicy": { "InitialDelay": "0.00:00:01", "MaxDelay": "0.00:00:05", "MaxRestarts": 2, "RestartWindow": "0.00:01:00" },
            "HealthCheck": { "Enabled": false }
        },
        "Applications": {
            "storefront": {
                "EnvironmentVariables": { "PORT": "3000" }
            },
            "legacy": {
                "EnvironmentVariables": { "PORT": "3009" }
            }
        }
    });

    let (manager, wrapper_dir) = create_manager_from_json(&tmp, &initial_cfg);
    manager.start_all().await;

    assert!(
        wait_for_app_state(&manager, "storefront", ApplicationState::Running, Duration::from_secs(5)).await
    );
    assert!(
        wait_for_app_state(&manager, "legacy", ApplicationState::Running, Duration::from_secs(5)).await
    );
    let initial_storefront_pid = manager.get_snapshot("storefront").unwrap().pid;
    assert!(initial_storefront_pid.is_some());
    assert!(manager.evaluate_service_health().healthy);

    // Act: Write updated appsettings.json:
    // - `storefront` is unchanged (must keep the exact same PID)
    // - `legacy` is removed (must be stopped and removed)
    // - `admin` is added (must be created and started)
    let updated_cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "WorkingDirectory": app_dir.to_string_lossy(),
            "Arguments": "worker.js",
            "ShutdownTimeout": "0.00:00:02",
            "RestartPolicy": { "InitialDelay": "0.00:00:01", "MaxDelay": "0.00:00:05", "MaxRestarts": 2, "RestartWindow": "0.00:01:00" },
            "HealthCheck": { "Enabled": false }
        },
        "Applications": {
            "storefront": {
                "EnvironmentVariables": { "PORT": "3000" }
            },
            "admin": {
                "EnvironmentVariables": { "PORT": "3001" }
            }
        }
    });
    fs::write(
        wrapper_dir.join("appsettings.json"),
        serde_json::to_string_pretty(&updated_cfg).unwrap(),
    )
    .unwrap();

    let diff = manager
        .reload_configuration("cli", Some("admin-user"))
        .await
        .unwrap();

    // Assert: `admin` added, `legacy` removed, `storefront` unchanged with same PID
    assert_eq!(diff.added, vec!["admin"]);
    assert_eq!(diff.removed, vec!["legacy"]);
    assert_eq!(diff.unchanged, vec!["storefront"]);
    assert!(
        wait_for_app_state(&manager, "admin", ApplicationState::Running, Duration::from_secs(5)).await
    );
    assert_eq!(
        manager.get_snapshot("storefront").unwrap().pid,
        initial_storefront_pid
    );
    assert!(manager.get_snapshot("legacy").is_err());

    // Also test reload via HTTP API when config is invalid: nothing changes and returns 400
    fs::write(wrapper_dir.join("appsettings.json"), "{ invalid json").unwrap();
    let router = build_router(manager.clone(), 5080);
    let reload_req = Request::builder()
        .method(Method::POST)
        .uri("/api/configuration/reload")
        .header(header::HOST, "127.0.0.1:5080")
        .header(headers::WRAPPER_REQUEST, "1")
        .body(Body::empty())
        .unwrap();
    let reload_resp = router.oneshot(reload_req).await.unwrap();
    assert_eq!(reload_resp.status(), StatusCode::BAD_REQUEST);
    let body_bytes = to_bytes(reload_resp.into_body(), 65536).await.unwrap();
    let problem: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(problem.get("errors").is_some());

    // Both apps are still Running after the rejected reload
    assert_eq!(
        manager.get_snapshot("storefront").unwrap().state,
        ApplicationState::Running
    );
    assert_eq!(
        manager.get_snapshot("admin").unwrap().state,
        ApplicationState::Running
    );

    manager.stop_all().await;
}

#[tokio::test]
async fn failing_startup_health_check_should_kill_process_and_transition_to_failed() {
    // Arrange: Acceptance Test I (app starts but never passes health check -> killed -> Failed)
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("apps");
    fs::create_dir_all(&app_dir).unwrap();
    let node_bin = find_node_binary();

    let script = app_dir.join("unhealthy.js");
    fs::write(&script, "setInterval(() => {}, 1000);\n").unwrap();

    let cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "WorkingDirectory": app_dir.to_string_lossy(),
            "Arguments": "unhealthy.js",
            "ShutdownTimeout": "0.00:00:01",
            "RestartPolicy": {
                "InitialDelay": "0.00:00:00.100",
                "MaxDelay": "0.00:00:00.200",
                "MaxRestarts": 1,
                "RestartWindow": "0.00:01:00"
            },
            "HealthCheck": {
                "Enabled": true,
                "Url": "http://127.0.0.1:59999/api/missing",
                "StartupTimeout": "0.00:00:00.250",
                "RetryInterval": "0.00:00:00.100",
                "RequestTimeout": "0.00:00:00.100"
            }
        },
        "Applications": {
            "unhealthy-app": {}
        }
    });

    let (manager, _) = create_manager_from_json(&tmp, &cfg);

    // Act
    manager.start_all().await;

    // Assert: Kills the unhealthy process on each timeout and transitions to Failed after MaxRestarts=1
    assert!(
        wait_for_app_state(&manager, "unhealthy-app", ApplicationState::Failed, Duration::from_secs(5)).await
    );
    let snap = manager.get_snapshot("unhealthy-app").unwrap();
    assert_eq!(snap.state, ApplicationState::Failed);
    assert_eq!(snap.restart_count, 1);
    assert!(snap.pid.is_none());
    assert!(snap
        .last_error
        .as_deref()
        .unwrap_or("")
        .contains("Health check failed"));
}

#[tokio::test]
async fn manual_restart_should_reset_restart_policy_and_recover_failed_app() {
    // Arrange
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("apps");
    fs::create_dir_all(&app_dir).unwrap();
    let node_bin = find_node_binary();

    let script = app_dir.join("flaky.js");
    // Initially crashes immediately
    fs::write(&script, "process.exit(1);\n").unwrap();

    let cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "WorkingDirectory": app_dir.to_string_lossy(),
            "Arguments": "flaky.js",
            "ShutdownTimeout": "0.00:00:01",
            "RestartPolicy": {
                "InitialDelay": "0.00:00:00.050",
                "MaxDelay": "0.00:00:00.100",
                "MaxRestarts": 1,
                "RestartWindow": "0.00:10:00"
            },
            "HealthCheck": { "Enabled": false }
        },
        "Applications": {
            "flaky-app": {}
        }
    });

    let (manager, _) = create_manager_from_json(&tmp, &cfg);
    manager.start_all().await;

    // Wait for flaky-app to hit Failed state
    assert!(
        wait_for_app_state(&manager, "flaky-app", ApplicationState::Failed, Duration::from_secs(5)).await
    );
    assert_eq!(manager.get_snapshot("flaky-app").unwrap().restart_count, 1);

    // Fix the script so it stays up, then issue manual `restart` via the Control API
    fs::write(&script, "setInterval(() => {}, 1000);\n").unwrap();

    let router = build_router(manager.clone(), 5080);
    let restart_req = Request::builder()
        .method(Method::POST)
        .uri("/api/applications/flaky-app/restart")
        .header(header::HOST, "127.0.0.1:5080")
        .header(headers::WRAPPER_REQUEST, "1")
        .header(headers::WRAPPER_SOURCE, "dashboard")
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(restart_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Assert: App recovers to Running and restart_count is reset to 0
    assert!(
        wait_for_app_state(&manager, "flaky-app", ApplicationState::Running, Duration::from_secs(5)).await
    );
    let snap = manager.get_snapshot("flaky-app").unwrap();
    assert_eq!(snap.state, ApplicationState::Running);
    assert_eq!(snap.restart_count, 0);

    manager.stop_all().await;
}

#[tokio::test]
async fn job_guard_should_kill_spawned_grandchild_processes_on_stop() {
    // Arrange
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("apps");
    fs::create_dir_all(&app_dir).unwrap();
    let node_bin = find_node_binary();
    let pid_file = app_dir.join("grandchild.pid");

    let parent_script = app_dir.join("parent.js");
    fs::write(
        &parent_script,
        format!(
            r#"
            const {{ spawn }} = require('child_process');
            const fs = require('fs');
            const child = spawn(process.execPath, ['-e', 'setInterval(() => {{}}, 1000)'], {{ stdio: 'ignore' }});
            fs.writeFileSync({}, String(child.pid));
            process.on('SIGINT', () => {{}}); // ignore SIGINT so JobGuard/kill_tree is exercised
            setInterval(() => {{}}, 1000);
            "#,
            serde_json::to_string(&pid_file.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();

    let cfg = json!({
        "ApplicationDefaults": {
            "ExecutablePath": node_bin,
            "WorkingDirectory": app_dir.to_string_lossy(),
            "Arguments": "parent.js",
            "ShutdownTimeout": "0.00:00:01",
            "RestartPolicy": { "InitialDelay": "0.00:00:01", "MaxDelay": "0.00:00:05", "MaxRestarts": 1, "RestartWindow": "0.00:01:00" },
            "HealthCheck": { "Enabled": false }
        },
        "Applications": {
            "parent-app": {}
        }
    });

    let (manager, _) = create_manager_from_json(&tmp, &cfg);
    manager.start_all().await;
    assert!(
        wait_for_app_state(&manager, "parent-app", ApplicationState::Running, Duration::from_secs(5)).await
    );

    // Wait for grandchild.pid file to be written
    let deadline = Instant::now() + Duration::from_secs(3);
    while !pid_file.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let grandchild_pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // Act: Stop parent-app (triggers kill_tree after 1s ShutdownTimeout)
    manager.stop_all().await;

    // Assert: Grandchild process is no longer alive
    #[cfg(unix)]
    unsafe {
        let alive = libc::kill(grandchild_pid, 0) == 0;
        assert!(!alive, "Grandchild process {grandchild_pid} should have been killed");
    }
}

#[test]
fn cli_parser_and_table_formatter_should_handle_all_commands() {
    use clap::Parser;
    use node_application_service_wrapper::cli::{
        format_applications_table, resolve_control_api_url, CliArgs, CliCommand,
    };
    use node_application_service_wrapper::state_store::DesiredStateSource;
    use node_application_service_wrapper::supervisor::ApplicationSnapshot;

    // Parse `start admin`
    let args = CliArgs::try_parse_from(["wrapper", "start", "admin"]).unwrap();
    assert!(matches!(
        args.command,
        Some(CliCommand::Start { ref name, no_wait: false }) if name == "admin"
    ));

    // Parse `logs storefront --lines 250 --url http://127.0.0.1:5090`
    let args2 = CliArgs::try_parse_from([
        "wrapper",
        "--url",
        "http://127.0.0.1:5090",
        "logs",
        "storefront",
        "--lines",
        "250",
    ])
    .unwrap();
    assert_eq!(args2.url.as_deref(), Some("http://127.0.0.1:5090"));
    assert!(matches!(
        args2.command,
        Some(CliCommand::Logs { ref name, lines: 250 }) if name == "storefront"
    ));

    // Format table
    let table = format_applications_table(&[ApplicationSnapshot {
        name: "storefront".to_string(),
        state: ApplicationState::Running,
        pid: Some(4321),
        start_time: None,
        uptime: Some("00:12:34".to_string()),
        uptime_seconds: Some(754),
        restart_count: 0,
        last_exit_code: None,
        last_exit_time: None,
        last_error: None,
        desired_state: DesiredState::Running,
        desired_state_source: DesiredStateSource::AutoStart,
    }]);
    assert!(table.contains("storefront"));
    assert!(table.contains("Running (auto)"));
    assert!(table.contains("00:12:34"));

    // URL resolution fallback
    let tmp = TempDir::new().unwrap();
    assert_eq!(
        resolve_control_api_url(None, tmp.path()),
        "http://127.0.0.1:5080"
    );
}

#[tokio::test]
async fn run_host_should_return_exit_code_13_and_log_single_invalid_config_line() {
    // Arrange
    let tmp = TempDir::new().unwrap();
    let wrapper_dir = tmp.path().join("wrapper");
    fs::create_dir_all(&wrapper_dir).unwrap();

    let invalid_cfg = json!({
        "Applications": {
            "storefront": {
                "ExecutablePath": "/nonexistent/node.exe",
                "WorkingDirectory": "/nonexistent/dir",
                "ShutdownTimeout": "two seconds",
                "RestartPolicy": {
                    "InitialDelay": "0.00:00:05",
                    "MaxDelay": "0.00:00:01",
                    "MaxRestarts": 3,
                    "RestartWindow": "0.00:01:00"
                },
                "HealthCheck": {
                    "Enabled": true,
                    "Url": "not-a-url",
                    "StartupTimeout": "0.00:00:05",
                    "RetryInterval": "0.00:00:01",
                    "RequestTimeout": "0.00:00:10"
                }
            }
        },
        "SerilogOptions": {
            "MinimumLevel": "NotALevel"
        }
    });
    fs::write(
        wrapper_dir.join("appsettings.json"),
        serde_json::to_string_pretty(&invalid_cfg).unwrap(),
    )
    .unwrap();

    let (_tx, rx) = oneshot::channel();

    // Act
    let code = run_host(wrapper_dir.clone(), rx).await;

    // Assert
    assert_eq!(code, exit_codes::INVALID_CONFIGURATION);
    let logs_dir = wrapper_dir.join("Logs");
    let log_files: Vec<PathBuf> = fs::read_dir(&logs_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(log_files.len(), 1);
    let log_content = fs::read_to_string(&log_files[0]).unwrap();
    assert_eq!(log_content.lines().count(), 1);
    assert!(log_content.contains("Invalid configuration"));
    assert!(log_content.contains("ExecutablePath"));
    assert!(log_content.contains("ShutdownTimeout"));
    assert!(log_content.contains("MinimumLevel"));
}
