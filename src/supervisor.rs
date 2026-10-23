//! Per-application process supervisor and state machine (`ApplicationSupervisor`).

use crate::ansi::strip_ansi_codes;
use crate::buffer::OutputLineBuffer;
use crate::config::{LogLevel, ResolvedApplicationConfig};
use crate::duration::{format_timespan, format_uptime};
use crate::health::{wait_for_startup_health, HealthCheckOutcome, ReqwestHealthProbe};
use crate::logging::WrapperLogger;
use crate::platform::{configure_child_command, execute_send_ctrl_c_helper, ProcessJobGuard};
use crate::port_checker::is_port_in_use;
use crate::restart_policy::{RestartDecision, RestartPolicy};
use crate::state_store::{DesiredState, DesiredStateSource};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{watch, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

/// Application lifecycle states (Phase 2 §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApplicationState {
    Stopped,
    Starting,
    Running,
    Restarting,
    Stopping,
    Failed,
}

impl ApplicationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApplicationState::Stopped => "Stopped",
            ApplicationState::Starting => "Starting",
            ApplicationState::Running => "Running",
            ApplicationState::Restarting => "Restarting",
            ApplicationState::Stopping => "Stopping",
            ApplicationState::Failed => "Failed",
        }
    }

    /// Returns true if the supervisor loop is actively managing or starting the app.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ApplicationState::Starting | ApplicationState::Running | ApplicationState::Restarting
        )
    }
}

/// Immutable point-in-time status snapshot of an application (Phase 2 §5.1 & §7.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationSnapshot {
    pub name: String,
    pub state: ApplicationState,
    pub pid: Option<u32>,
    pub start_time: Option<DateTime<Utc>>,
    pub uptime: Option<String>,
    pub uptime_seconds: Option<u64>,
    pub restart_count: u32,
    pub last_exit_code: Option<i32>,
    pub last_exit_time: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub desired_state: DesiredState,
    pub desired_state_source: DesiredStateSource,
}

struct SupervisorMutableState {
    config: ResolvedApplicationConfig,
    state: ApplicationState,
    pid: Option<u32>,
    start_time: Option<DateTime<Utc>>,
    start_instant: Option<Instant>,
    restart_count: u32,
    last_exit_code: Option<i32>,
    last_exit_time: Option<DateTime<Utc>>,
    last_error: Option<String>,
    desired_state: DesiredState,
    desired_state_source: DesiredStateSource,
    restart_policy: RestartPolicy,
    output_buffer: OutputLineBuffer,
    crash_buffer: OutputLineBuffer,
}

struct RunningLoopHandle {
    stop_tx: watch::Sender<bool>,
    join_handle: JoinHandle<()>,
}

/// Supervises a single child application with serialized lifecycle commands (`start`, `stop`, `restart`).
pub struct ApplicationSupervisor {
    name: String,
    inner: Arc<Mutex<SupervisorMutableState>>,
    command_lock: Arc<AsyncMutex<Option<RunningLoopHandle>>>,
    logger: WrapperLogger,
    health_probe: ReqwestHealthProbe,
}

impl ApplicationSupervisor {
    pub fn new(
        config: ResolvedApplicationConfig,
        desired_state: DesiredState,
        desired_state_source: DesiredStateSource,
        logger: WrapperLogger,
    ) -> Self {
        let name = config.name.clone();
        let output_buffer = OutputLineBuffer::new(config.output_buffer_lines);
        let crash_buffer = OutputLineBuffer::new(logger.crash_context_lines());
        let restart_policy = RestartPolicy::new(config.restart_policy.clone());

        Self {
            name,
            inner: Arc::new(Mutex::new(SupervisorMutableState {
                config,
                state: ApplicationState::Stopped,
                pid: None,
                start_time: None,
                start_instant: None,
                restart_count: 0,
                last_exit_code: None,
                last_exit_time: None,
                last_error: None,
                desired_state,
                desired_state_source,
                restart_policy,
                output_buffer,
                crash_buffer,
            })),
            command_lock: Arc::new(AsyncMutex::new(None)),
            logger,
            health_probe: ReqwestHealthProbe::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn current_config(&self) -> ResolvedApplicationConfig {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .clone()
    }

    /// Return a current status snapshot of the application.
    pub fn snapshot(&self) -> ApplicationSnapshot {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (uptime, uptime_seconds) = match guard.start_instant {
            Some(inst)
                if matches!(
                    guard.state,
                    ApplicationState::Starting | ApplicationState::Running
                ) =>
            {
                let elapsed = inst.elapsed();
                (Some(format_uptime(elapsed)), Some(elapsed.as_secs()))
            }
            _ => (None, None),
        };

        ApplicationSnapshot {
            name: guard.config.name.clone(),
            state: guard.state,
            pid: guard.pid,
            start_time: guard.start_time,
            uptime,
            uptime_seconds,
            restart_count: guard.restart_count,
            last_exit_code: guard.last_exit_code,
            last_exit_time: guard.last_exit_time,
            last_error: guard.last_error.clone(),
            desired_state: guard.desired_state,
            desired_state_source: guard.desired_state_source,
        }
    }

    /// Return the last `lines` stdout/stderr lines from the in-memory ring buffer.
    pub fn recent_output(&self, lines: usize) -> Vec<String> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.output_buffer.last_n(lines)
    }

    /// Update the desired state metadata stored on this supervisor.
    pub fn set_desired_state(&self, desired: DesiredState, source: DesiredStateSource) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.desired_state = desired;
        guard.desired_state_source = source;
    }

    /// Start the application (serialized).
    /// - From `Stopped` or `Failed`: resets the restart policy and starts the supervision loop.
    /// - From `Starting`, `Running`, or `Restarting`: no-op ("already running").
    pub async fn start(&self) -> ApplicationSnapshot {
        let mut cmd_guard = self.command_lock.lock().await;

        // Clean up finished loop handle if it previously ended in `Failed` or `Stopped`
        if let Some(existing) = cmd_guard.as_ref() {
            if existing.join_handle.is_finished() {
                if let Some(old) = cmd_guard.take() {
                    let _ = old.join_handle.await;
                }
            }
        }

        {
            let mut state_guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state_guard.state.is_active() && cmd_guard.is_some() {
                return self.snapshot_from_guard(&state_guard);
            }

            state_guard.restart_policy.reset();
            state_guard.restart_count = 0;
            state_guard.last_error = None;
            state_guard.state = ApplicationState::Starting;
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let inner = self.inner.clone();
        let logger = self.logger.clone();
        let probe = self.health_probe.clone();

        let join_handle = tokio::spawn(async move {
            run_supervision_loop(inner, logger, probe, stop_rx).await;
        });

        *cmd_guard = Some(RunningLoopHandle {
            stop_tx,
            join_handle,
        });

        self.snapshot()
    }

    /// Stop the application gracefully (serialized).
    /// - From `Stopped` or `Failed`: no-op ("already stopped").
    /// - From `Starting`, `Running`, or `Restarting`: sends Ctrl+C, waits up to `ShutdownTimeout`, then kills tree.
    pub async fn stop(&self) -> ApplicationSnapshot {
        let mut cmd_guard = self.command_lock.lock().await;
        self.stop_locked(&mut cmd_guard).await;
        self.snapshot()
    }

    /// Restart the application (serialized): stops if active, resets restart policy, and starts.
    pub async fn restart(&self) -> ApplicationSnapshot {
        let mut cmd_guard = self.command_lock.lock().await;
        self.stop_locked(&mut cmd_guard).await;

        {
            let mut state_guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            state_guard.restart_policy.reset();
            state_guard.restart_count = 0;
            state_guard.last_error = None;
            state_guard.state = ApplicationState::Starting;
        }

        let (stop_tx, stop_rx) = watch::channel(false);
        let inner = self.inner.clone();
        let logger = self.logger.clone();
        let probe = self.health_probe.clone();

        let join_handle = tokio::spawn(async move {
            run_supervision_loop(inner, logger, probe, stop_rx).await;
        });

        *cmd_guard = Some(RunningLoopHandle {
            stop_tx,
            join_handle,
        });

        self.snapshot()
    }

    /// Apply a new configuration during a reload (Phase 2 §5.4).
    /// If the app is active, restarts it with the new settings; if stopped/failed, stores the new settings.
    pub async fn apply_reloaded_config(
        &self,
        new_config: ResolvedApplicationConfig,
    ) -> (bool, ApplicationSnapshot) {
        let mut cmd_guard = self.command_lock.lock().await;
        let was_active = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.state.is_active()
        };

        if was_active {
            self.stop_locked(&mut cmd_guard).await;
        }

        {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .restart_policy
                .update_options(new_config.restart_policy.clone());
            if guard.output_buffer.capacity() != new_config.output_buffer_lines {
                guard.output_buffer = OutputLineBuffer::new(new_config.output_buffer_lines);
            }
            guard.config = new_config;
        }

        if was_active {
            {
                let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                guard.restart_policy.reset();
                guard.restart_count = 0;
                guard.last_error = None;
                guard.state = ApplicationState::Starting;
            }
            let (stop_tx, stop_rx) = watch::channel(false);
            let inner = self.inner.clone();
            let logger = self.logger.clone();
            let probe = self.health_probe.clone();

            let join_handle = tokio::spawn(async move {
                run_supervision_loop(inner, logger, probe, stop_rx).await;
            });

            *cmd_guard = Some(RunningLoopHandle {
                stop_tx,
                join_handle,
            });
            (true, self.snapshot())
        } else {
            (false, self.snapshot())
        }
    }

    async fn stop_locked(&self, cmd_guard: &mut Option<RunningLoopHandle>) {
        if let Some(handle) = cmd_guard.take() {
            {
                let mut state_guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(
                    state_guard.state,
                    ApplicationState::Starting
                        | ApplicationState::Running
                        | ApplicationState::Restarting
                ) {
                    state_guard.state = ApplicationState::Stopping;
                }
            }
            let _ = handle.stop_tx.send(true);
            let _ = handle.join_handle.await;
        }

        let mut state_guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state_guard.state != ApplicationState::Failed {
            state_guard.state = ApplicationState::Stopped;
        }
        state_guard.pid = None;
        state_guard.start_time = None;
        state_guard.start_instant = None;
    }

    fn snapshot_from_guard(&self, guard: &SupervisorMutableState) -> ApplicationSnapshot {
        let (uptime, uptime_seconds) = match guard.start_instant {
            Some(inst)
                if matches!(
                    guard.state,
                    ApplicationState::Starting | ApplicationState::Running
                ) =>
            {
                let elapsed = inst.elapsed();
                (Some(format_uptime(elapsed)), Some(elapsed.as_secs()))
            }
            _ => (None, None),
        };

        ApplicationSnapshot {
            name: guard.config.name.clone(),
            state: guard.state,
            pid: guard.pid,
            start_time: guard.start_time,
            uptime,
            uptime_seconds,
            restart_count: guard.restart_count,
            last_exit_code: guard.last_exit_code,
            last_exit_time: guard.last_exit_time,
            last_error: guard.last_error.clone(),
            desired_state: guard.desired_state,
            desired_state_source: guard.desired_state_source,
        }
    }
}

async fn run_supervision_loop(
    inner: Arc<Mutex<SupervisorMutableState>>,
    logger: WrapperLogger,
    probe: ReqwestHealthProbe,
    mut stop_rx: watch::Receiver<bool>,
) {
    let cfg = {
        let guard = inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.config.clone()
    };

    // 1. Log startup summary (Environment variable NAMES only, never values! Phase 1 §7)
    let env_names: Vec<String> = cfg.environment_variables.keys().cloned().collect();
    logger.log_wrapper(
        LogLevel::Information,
        Some(&cfg.name),
        &format!(
            "Starting application supervisor | AppName: {} | ExecutablePath: {} | Arguments: {} | WorkingDirectory: {} | EnvironmentVariableNames: [{}] | LogDirectory: {}",
            cfg.name,
            cfg.executable_path.display(),
            cfg.arguments,
            cfg.working_directory.display(),
            env_names.join(", "),
            logger.log_directory().display()
        ),
        vec![
            ("ExecutablePath", Value::String(cfg.executable_path.display().to_string())),
            ("Arguments", Value::String(cfg.arguments.clone())),
            ("WorkingDirectory", Value::String(cfg.working_directory.display().to_string())),
            ("EnvironmentVariableNames", json!(env_names)),
            ("LogDirectory", Value::String(logger.log_directory().display().to_string())),
        ],
    );

    // 2. Optional runtime version check (`VersionArguments`)
    if let Some(ver_args) = cfg.version_arguments.as_deref() {
        log_runtime_version(&cfg, ver_args, &logger).await;
    }

    let mut logged_health_disabled = false;
    let loop_epoch = Instant::now();

    loop {
        if *stop_rx.borrow() {
            mark_stopped(&inner);
            return;
        }

        let cfg = {
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.state = ApplicationState::Starting;
            guard.crash_buffer.clear();
            guard.config.clone()
        };

        // 3. Optional TCP port preflight check (`PreflightPort`)
        if let Some(port) = cfg.preflight_port {
            if is_port_in_use(port) {
                let err_msg = format!("Port is already in use | Port: {port}");
                logger.log_wrapper(
                    LogLevel::Error,
                    Some(&cfg.name),
                    &format!("Port is already in use | AppName: {} | Port: {port}", cfg.name),
                    vec![("Port", json!(port))],
                );

                if !handle_failed_attempt_or_sleep(
                    &inner,
                    &logger,
                    &cfg.name,
                    err_msg,
                    loop_epoch.elapsed().as_millis() as u64,
                    &mut stop_rx,
                )
                .await
                {
                    return;
                }
                continue;
            }
        }

        // 4. Spawn child process and attach to Job Object / process group
        let mut cmd = Command::new(&cfg.executable_path);
        for arg in split_command_args(&cfg.arguments) {
            cmd.arg(arg);
        }
        cmd.current_dir(&cfg.working_directory)
            .envs(&cfg.environment_variables)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_command(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(spawn_err) => {
                let err_msg = format!("Failed to start process: {spawn_err}");
                logger.log_wrapper(
                    LogLevel::Error,
                    Some(&cfg.name),
                    &format!(
                        "Failed to start process | AppName: {} | ExecutablePath: {} | Error: {spawn_err}",
                        cfg.name,
                        cfg.executable_path.display()
                    ),
                    vec![("Error", Value::String(spawn_err.to_string()))],
                );
                if !handle_failed_attempt_or_sleep(
                    &inner,
                    &logger,
                    &cfg.name,
                    err_msg,
                    loop_epoch.elapsed().as_millis() as u64,
                    &mut stop_rx,
                )
                .await
                {
                    return;
                }
                continue;
            }
        };

        let pid = child.id().unwrap_or(0);
        let job_guard = match ProcessJobGuard::attach(pid) {
            Ok(g) => Some(g),
            Err(job_err) => {
                logger.log_wrapper(
                    LogLevel::Warning,
                    Some(&cfg.name),
                    &format!(
                        "Job Object attach warning | AppName: {} | Pid: {pid} | Error: {job_err}",
                        cfg.name
                    ),
                    vec![("Pid", json!(pid)), ("Error", Value::String(job_err))],
                );
                None
            }
        };

        let child_start_instant = Instant::now();
        let (output_buffer, crash_buffer) = {
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.pid = Some(pid);
            guard.start_time = Some(Utc::now());
            guard.start_instant = Some(child_start_instant);
            (guard.output_buffer.clone(), guard.crash_buffer.clone())
        };

        logger.log_wrapper(
            LogLevel::Information,
            Some(&cfg.name),
            &format!("Process started | AppName: {} | Pid: {pid}", cfg.name),
            vec![("Pid", json!(pid))],
        );

        // 5. Spawn async stdout/stderr line capture tasks
        let stdout_task = child.stdout.take().map(|stdout| {
            let app_name = cfg.name.clone();
            let logger_clone = logger.clone();
            let out_buf = output_buffer.clone();
            let crash_buf = crash_buffer.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let clean = strip_ansi_codes(&line).into_owned();
                    logger_clone.log_app_output(&app_name, &clean, false);
                    out_buf.push(format!("[INF] {clean}"));
                    crash_buf.push(clean);
                }
            })
        });

        let stderr_task = child.stderr.take().map(|stderr| {
            let app_name = cfg.name.clone();
            let logger_clone = logger.clone();
            let out_buf = output_buffer.clone();
            let crash_buf = crash_buffer.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let clean = strip_ansi_codes(&line).into_owned();
                    logger_clone.log_app_output(&app_name, &clean, true);
                    out_buf.push(format!("[WRN] {clean}"));
                    crash_buf.push(clean);
                }
            })
        });

        // 6. Startup health check (if enabled)
        if cfg.health_check.enabled {
            let (hc_cancel_tx, hc_cancel_rx) = watch::channel(false);
            let hc_settings = cfg.health_check.clone();
            let probe_clone = probe.clone();

            let mut hc_task = tokio::spawn(async move {
                wait_for_startup_health(&hc_settings, &probe_clone, hc_cancel_rx).await
            });

            enum StartupWaitResult {
                HealthOutcome(HealthCheckOutcome),
                ChildExitedEarly(std::io::Result<std::process::ExitStatus>),
                StopRequested,
            }

            let wait_res = tokio::select! {
                res = &mut hc_task => {
                    StartupWaitResult::HealthOutcome(res.unwrap_or(HealthCheckOutcome::Cancelled))
                }
                status = child.wait() => {
                    let _ = hc_cancel_tx.send(true);
                    let _ = hc_task.await;
                    StartupWaitResult::ChildExitedEarly(status)
                }
                _ = stop_rx.changed() => {
                    let _ = hc_cancel_tx.send(true);
                    let _ = hc_task.await;
                    StartupWaitResult::StopRequested
                }
            };

            match wait_res {
                StartupWaitResult::StopRequested => {
                    perform_graceful_stop(
                        &inner,
                        &logger,
                        &cfg,
                        pid,
                        &mut child,
                        job_guard,
                        stdout_task,
                        stderr_task,
                    )
                    .await;
                    return;
                }
                StartupWaitResult::ChildExitedEarly(exit_res) => {
                    drain_output_tasks(stdout_task, stderr_task).await;
                    drop(job_guard);
                    let exit_code = exit_res.ok().and_then(|s| s.code()).unwrap_or(-1);
                    let uptime = child_start_instant.elapsed();
                    if !record_unexpected_exit_and_sleep(
                        &inner,
                        &logger,
                        &cfg.name,
                        exit_code,
                        uptime,
                        None,
                        loop_epoch.elapsed().as_millis() as u64,
                        &mut stop_rx,
                    )
                    .await
                    {
                        return;
                    }
                    continue;
                }
                StartupWaitResult::HealthOutcome(HealthCheckOutcome::Healthy { elapsed }) => {
                    let url_str = cfg.health_check.url.as_deref().unwrap_or("");
                    logger.log_wrapper(
                        LogLevel::Information,
                        Some(&cfg.name),
                        &format!(
                            "Health check passed | AppName: {} | Url: {url_str} | Elapsed: {}",
                            cfg.name,
                            format_timespan(elapsed)
                        ),
                        vec![
                            ("Url", Value::String(url_str.to_string())),
                            ("Elapsed", Value::String(format_timespan(elapsed))),
                        ],
                    );
                    let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                    guard.state = ApplicationState::Running;
                    guard.last_error = None;
                }
                StartupWaitResult::HealthOutcome(HealthCheckOutcome::TimedOut { last_result }) => {
                    let url_str = cfg.health_check.url.as_deref().unwrap_or("");
                    logger.log_wrapper(
                        LogLevel::Error,
                        Some(&cfg.name),
                        &format!(
                            "Health check failed, killing process | AppName: {} | Url: {url_str} | LastResult: {last_result}",
                            cfg.name
                        ),
                        vec![
                            ("Url", Value::String(url_str.to_string())),
                            ("LastResult", Value::String(last_result.clone())),
                        ],
                    );
                    if let Some(ref jg) = job_guard {
                        jg.kill_tree();
                    }
                    let _ = child.kill().await;
                    let exit_res = child.wait().await;
                    drain_output_tasks(stdout_task, stderr_task).await;
                    drop(job_guard);

                    let exit_code = exit_res.ok().and_then(|s| s.code()).unwrap_or(-1);
                    let uptime = child_start_instant.elapsed();
                    let custom_err = format!("Health check failed | {last_result}");
                    if !record_unexpected_exit_and_sleep(
                        &inner,
                        &logger,
                        &cfg.name,
                        exit_code,
                        uptime,
                        Some(custom_err),
                        loop_epoch.elapsed().as_millis() as u64,
                        &mut stop_rx,
                    )
                    .await
                    {
                        return;
                    }
                    continue;
                }
                StartupWaitResult::HealthOutcome(HealthCheckOutcome::Disabled)
                | StartupWaitResult::HealthOutcome(HealthCheckOutcome::Cancelled) => {
                    if *stop_rx.borrow() {
                        perform_graceful_stop(
                            &inner,
                            &logger,
                            &cfg,
                            pid,
                            &mut child,
                            job_guard,
                            stdout_task,
                            stderr_task,
                        )
                        .await;
                        return;
                    }
                    let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                    guard.state = ApplicationState::Running;
                    guard.last_error = None;
                }
            }
        } else {
            if !logged_health_disabled {
                logger.log_wrapper(
                    LogLevel::Information,
                    Some(&cfg.name),
                    &format!("Health check disabled | AppName: {}", cfg.name),
                    vec![],
                );
                logged_health_disabled = true;
            }
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.state = ApplicationState::Running;
            guard.last_error = None;
        }

        // 7. Supervise running child process until it exits or a stop is requested
        tokio::select! {
            exit_res = child.wait() => {
                if *stop_rx.borrow() {
                    drain_output_tasks(stdout_task, stderr_task).await;
                    drop(job_guard);
                    mark_stopped(&inner);
                    return;
                }
                drain_output_tasks(stdout_task, stderr_task).await;
                drop(job_guard);

                let exit_code = exit_res.ok().and_then(|s| s.code()).unwrap_or(-1);
                let uptime = child_start_instant.elapsed();
                if !record_unexpected_exit_and_sleep(
                    &inner,
                    &logger,
                    &cfg.name,
                    exit_code,
                    uptime,
                    None,
                    loop_epoch.elapsed().as_millis() as u64,
                    &mut stop_rx,
                )
                .await
                {
                    return;
                }
            }
            _ = stop_rx.changed() => {
                perform_graceful_stop(
                    &inner,
                    &logger,
                    &cfg,
                    pid,
                    &mut child,
                    job_guard,
                    stdout_task,
                    stderr_task,
                )
                .await;
                return;
            }
        }
    }
}

/// Perform graceful stop (Phase 1 §6.4):
/// 1. Mark state as `Stopping`.
/// 2. Invoke `--send-ctrl-c <pid>` helper and log result.
/// 3. Wait up to `ShutdownTimeout` for the child to exit; log "Process stopped gracefully".
/// 4. If timeout expires, kill the entire process tree (`JobObject` / process group) and log a warning.
#[allow(clippy::too_many_arguments)]
async fn perform_graceful_stop(
    inner: &Arc<Mutex<SupervisorMutableState>>,
    logger: &WrapperLogger,
    cfg: &ResolvedApplicationConfig,
    pid: u32,
    child: &mut tokio::process::Child,
    job_guard: Option<ProcessJobGuard>,
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
) {
    {
        let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.state = ApplicationState::Stopping;
    }

    let helper_code = invoke_ctrl_c_helper_process(pid).await;
    logger.log_wrapper(
        LogLevel::Information,
        Some(&cfg.name),
        &format!(
            "Sent Ctrl+C to process | AppName: {} | Pid: {pid} | HelperExitCode: {helper_code}",
            cfg.name
        ),
        vec![
            ("Pid", json!(pid)),
            ("HelperExitCode", json!(helper_code)),
        ],
    );

    let exit_code = match tokio::time::timeout(cfg.shutdown_timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let code = status.code().unwrap_or(0);
            logger.log_wrapper(
                LogLevel::Information,
                Some(&cfg.name),
                &format!(
                    "Process stopped gracefully | AppName: {} | ExitCode: {code}",
                    cfg.name
                ),
                vec![("ExitCode", json!(code))],
            );
            code
        }
        Ok(Err(_)) | Err(_) => {
            logger.log_wrapper(
                LogLevel::Warning,
                Some(&cfg.name),
                &format!(
                    "Process did not exit within ShutdownTimeout, killing process tree | AppName: {} | Timeout: {}",
                    cfg.name,
                    format_timespan(cfg.shutdown_timeout)
                ),
                vec![(
                    "ShutdownTimeout",
                    Value::String(format_timespan(cfg.shutdown_timeout)),
                )],
            );
            if let Some(ref jg) = job_guard {
                jg.kill_tree();
            }
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill().await;
            }
            let status = child.wait().await;
            status.ok().and_then(|s| s.code()).unwrap_or(-1)
        }
    };

    drain_output_tasks(stdout_task, stderr_task).await;
    drop(job_guard);

    let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
    guard.pid = None;
    guard.start_time = None;
    guard.start_instant = None;
    guard.last_exit_code = Some(exit_code);
    guard.last_exit_time = Some(Utc::now());
    guard.state = ApplicationState::Stopped;
}

/// Re-launch the wrapper executable in `--send-ctrl-c <pid>` helper mode on Windows (Phase 1 §6.4),
/// or send `SIGINT` to the child process group on Unix.
async fn invoke_ctrl_c_helper_process(pid: u32) -> i32 {
    #[cfg(windows)]
    {
        if let Ok(exe_path) = std::env::current_exe() {
            let mut cmd = Command::new(exe_path);
            cmd.arg("--send-ctrl-c")
                .arg(pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(3), cmd.status()).await
            {
                if let Some(code) = status.code() {
                    if code != 101 {
                        return code;
                    }
                }
            }
        }
    }
    execute_send_ctrl_c_helper(pid)
}

/// Wait up to a bounded 2 seconds for stdout and stderr stream readers to reach EOF
/// (Phase 1 §6.3 & §13 finding #3: prevents indefinite hang if a descendant holds a pipe open).
async fn drain_output_tasks(
    stdout_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
) {
    let drain = async {
        if let Some(t) = stdout_task {
            let _ = t.await;
        }
        if let Some(t) = stderr_task {
            let _ = t.await;
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
}

#[allow(clippy::too_many_arguments)]
async fn record_unexpected_exit_and_sleep(
    inner: &Arc<Mutex<SupervisorMutableState>>,
    logger: &WrapperLogger,
    app_name: &str,
    exit_code: i32,
    uptime: Duration,
    custom_error: Option<String>,
    now_ms: u64,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    let (crash_lines, decision) = {
        let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.pid = None;
        guard.start_time = None;
        guard.start_instant = None;
        guard.last_exit_code = Some(exit_code);
        guard.last_exit_time = Some(Utc::now());
        guard.last_error = Some(
            custom_error.unwrap_or_else(|| format!("Process exited unexpectedly (code {exit_code})")),
        );
        let lines = guard.crash_buffer.snapshot();
        let dec = guard.restart_policy.record_exit(now_ms);
        (lines, dec)
    };

    logger.log_wrapper(
        LogLevel::Error,
        Some(app_name),
        &format!(
            "Process exited unexpectedly | AppName: {app_name} | ExitCode: {exit_code} | Uptime: {}",
            format_timespan(uptime)
        ),
        vec![
            ("ExitCode", json!(exit_code)),
            ("Uptime", Value::String(format_timespan(uptime))),
            ("CrashContext", json!(crash_lines)),
        ],
    );

    apply_restart_decision(inner, logger, app_name, decision, stop_rx).await
}

async fn handle_failed_attempt_or_sleep(
    inner: &Arc<Mutex<SupervisorMutableState>>,
    logger: &WrapperLogger,
    app_name: &str,
    error_message: String,
    now_ms: u64,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    let decision = {
        let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.pid = None;
        guard.start_time = None;
        guard.start_instant = None;
        guard.last_error = Some(error_message);
        guard.restart_policy.record_exit(now_ms)
    };

    apply_restart_decision(inner, logger, app_name, decision, stop_rx).await
}

async fn apply_restart_decision(
    inner: &Arc<Mutex<SupervisorMutableState>>,
    logger: &WrapperLogger,
    app_name: &str,
    decision: RestartDecision,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    match decision {
        RestartDecision::GiveUp { exits_in_window } => {
            logger.log_wrapper(
                LogLevel::Fatal,
                Some(app_name),
                &format!(
                    "Giving up: too many exits | AppName: {app_name} | ExitsInWindow: {exits_in_window}"
                ),
                vec![("ExitsInWindow", json!(exits_in_window))],
            );
            let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
            guard.state = ApplicationState::Failed;
            if guard.last_error.is_none() {
                guard.last_error = Some(format!(
                    "Giving up: too many exits ({exits_in_window} within window)"
                ));
            }
            false
        }
        RestartDecision::Restart {
            attempt,
            delay,
            exits_in_window: _,
        } => {
            {
                let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
                guard.restart_count = guard.restart_count.saturating_add(1);
                guard.state = ApplicationState::Restarting;
            }
            logger.log_wrapper(
                LogLevel::Warning,
                Some(app_name),
                &format!(
                    "Restarting process | AppName: {app_name} | Attempt: {attempt} | Delay: {}",
                    format_timespan(delay)
                ),
                vec![
                    ("Attempt", json!(attempt)),
                    ("Delay", Value::String(format_timespan(delay))),
                ],
            );

            tokio::select! {
                _ = tokio::time::sleep(delay) => {
                    if *stop_rx.borrow() {
                        mark_stopped(inner);
                        return false;
                    }
                    true
                }
                _ = stop_rx.changed() => {
                    mark_stopped(inner);
                    false
                }
            }
        }
    }
}

fn mark_stopped(inner: &Arc<Mutex<SupervisorMutableState>>) {
    let mut guard = inner.lock().unwrap_or_else(|e| e.into_inner());
    guard.pid = None;
    guard.start_time = None;
    guard.start_instant = None;
    if guard.state != ApplicationState::Failed {
        guard.state = ApplicationState::Stopped;
    }
}

async fn log_runtime_version(
    cfg: &ResolvedApplicationConfig,
    version_args: &str,
    logger: &WrapperLogger,
) {
    let mut cmd = Command::new(&cfg.executable_path);
    for arg in split_command_args(version_args) {
        cmd.arg(arg);
    }
    cmd.current_dir(&cfg.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        let out_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let err_str = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let combined = if !out_str.is_empty() {
            out_str
        } else {
            err_str
        };
        if !combined.is_empty() {
            logger.log_wrapper(
                LogLevel::Information,
                Some(&cfg.name),
                &format!("Runtime version | AppName: {} | Version: {combined}", cfg.name),
                vec![("Version", Value::String(combined))],
            );
        }
    }
}

/// Split a command-line argument string into tokens while respecting double and single quotes.
pub fn split_command_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\\' if in_double => {
                if let Some(&next) = chars.peek() {
                    if next == '"' || next == '\\' {
                        current.push(chars.next().unwrap());
                    } else {
                        current.push(c);
                    }
                } else {
                    current.push(c);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(c);
            }
        }
    }

    if !current.is_empty() {
        args.push(current);
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_args_should_handle_quotes_and_flags() {
        // Arrange / Act
        let parsed = split_command_args("-e \"setInterval(() => {}, 1000)\" --port 3000");

        // Assert
        assert_eq!(
            parsed,
            vec!["-e", "setInterval(() => {}, 1000)", "--port", "3000"]
        );
    }
}
