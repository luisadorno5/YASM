//! Windows Service integration (`windows-service`), `StartupFailureService` early exit-code reporting,
//! and console / service host runner.

use crate::api::build_router;
use crate::config::{
    load_and_validate_from_dir, read_bootstrap_serilog_options, LogLevel, ValidatedWrapperConfig,
};
use crate::constants::exit_codes;
use crate::logging::WrapperLogger;
use crate::manager::ApplicationManager;
use crate::state_store::DesiredStateStore;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Determine the base directory containing the wrapper executable (never `Environment.CurrentDirectory`
/// which is `C:\Windows\System32` under the Windows Service Control Manager).
pub fn resolve_base_directory() -> PathBuf {
    if let Ok(override_dir) = env::var("WRAPPER_BASE_DIR") {
        let p = PathBuf::from(override_dir);
        if p.is_dir() {
            return p;
        }
    }

    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Startup sequence implementing Phase 1 §6.1 and Phase 2 §6:
/// 1. Read bootstrap `SerilogOptions` from raw `appsettings.json` and initialize the logger
///    (falling back to `<base_dir>/Logs` with exit code `13` if unwritable).
/// 2. Validate all configuration sections upfront before starting the service or applications.
///    If invalid, log one `Invalid configuration` line listing every problem and return exit code `13`.
/// 3. Run the `ApplicationManager` and Control API server until `shutdown_rx` completes.
pub async fn run_host(base_dir: PathBuf, shutdown_rx: oneshot::Receiver<()>) -> i32 {
    let environment = env::var("DOTNET_ENVIRONMENT")
        .or_else(|_| env::var("ASPNETCORE_ENVIRONMENT"))
        .ok();

    // Step 1: Read raw JSON for bootstrap logger
    let raw_json = read_raw_config_for_bootstrap(&base_dir, environment.as_deref());
    let (bootstrap_serilog, bootstrap_console) =
        read_bootstrap_serilog_options(raw_json.as_ref(), &base_dir);

    let logger = match WrapperLogger::initialize(bootstrap_serilog, bootstrap_console, &base_dir) {
        Ok(l) => l,
        Err((fallback_logger, _err_msg)) => {
            fallback_logger.flush();
            return exit_codes::INVALID_CONFIGURATION;
        }
    };

    // Step 2: Validate every configuration section before reporting Running
    let validated_config = match load_and_validate_from_dir(&base_dir, environment.as_deref()) {
        Ok(cfg) => cfg,
        Err(errors) => {
            let joined = errors.join("; ");
            logger.log_wrapper(
                LogLevel::Fatal,
                None,
                &format!("Invalid configuration | Errors: {joined}"),
                vec![("Errors", json!(errors))],
            );
            logger.flush();
            return exit_codes::INVALID_CONFIGURATION;
        }
    };

    // Re-initialize logger with validated SerilogOptions
    let logger = match WrapperLogger::initialize(
        validated_config.serilog.clone(),
        validated_config.enable_console_logging,
        &base_dir,
    ) {
        Ok(l) => l,
        Err((fallback_logger, _)) => {
            fallback_logger.flush();
            return exit_codes::INVALID_CONFIGURATION;
        }
    };

    execute_validated_host(validated_config, logger, environment, shutdown_rx).await
}

async fn execute_validated_host(
    config: ValidatedWrapperConfig,
    logger: WrapperLogger,
    environment: Option<String>,
    shutdown_rx: oneshot::Receiver<()>,
) -> i32 {
    // Load state.json (ignoring corruption with a warning so startup never blocks)
    let (state_store, state_warning) = DesiredStateStore::load(config.state_file.clone());
    if let Some(warn) = state_warning {
        logger.log_wrapper(
            LogLevel::Warning,
            None,
            &warn,
            vec![(
                "StateFile",
                Value::String(config.state_file.display().to_string()),
            )],
        );
    }

    // Bind Control API listener first so any port bind failure exits with 1064 (Phase 2 §6)
    let bind_addr_str = if config.control_api.bind_host.contains(':') {
        format!("[{}]:{}", config.control_api.bind_host, config.control_api.port)
    } else {
        format!("{}:{}", config.control_api.bind_host, config.control_api.port)
    };

    let socket_addr: SocketAddr = match bind_addr_str.parse() {
        Ok(a) => a,
        Err(e) => {
            logger.log_wrapper(
                LogLevel::Fatal,
                None,
                &format!("Failed to parse ControlApi bind address '{bind_addr_str}': {e}"),
                vec![("Error", Value::String(e.to_string()))],
            );
            logger.flush();
            return exit_codes::UNEXPECTED_FAILURE;
        }
    };

    let listener = match TcpListener::bind(socket_addr).await {
        Ok(l) => l,
        Err(e) => {
            logger.log_wrapper(
                LogLevel::Fatal,
                None,
                &format!(
                    "Control API failed to bind port | Url: {} | Error: {e}",
                    config.control_api.url
                ),
                vec![
                    ("Url", Value::String(config.control_api.url.clone())),
                    ("Error", Value::String(e.to_string())),
                ],
            );
            logger.flush();
            return exit_codes::UNEXPECTED_FAILURE;
        }
    };

    let control_port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(config.control_api.port);

    logger.log_wrapper(
        LogLevel::Information,
        None,
        &format!(
            "Control API listening | Url: http://{}:{}",
            config.control_api.bind_host, control_port
        ),
        vec![("Port", json!(control_port))],
    );

    let manager = ApplicationManager::new(config, state_store, logger.clone(), environment);

    // Start all applications with DesiredState == Running in parallel
    manager.start_all().await;

    let router = build_router(manager.clone(), control_port);

    let serve_result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await;

    // Stop all applications in parallel on host shutdown
    logger.log_wrapper(
        LogLevel::Information,
        None,
        "Stopping all supervised applications",
        vec![],
    );
    manager.stop_all().await;
    logger.log_wrapper(
        LogLevel::Information,
        None,
        "Service stopped normally",
        vec![],
    );
    logger.flush();

    match serve_result {
        Ok(()) => exit_codes::SUCCESS,
        Err(e) => {
            logger.log_wrapper(
                LogLevel::Fatal,
                None,
                &format!("Control API server error: {e}"),
                vec![("Error", Value::String(e.to_string()))],
            );
            logger.flush();
            exit_codes::UNEXPECTED_FAILURE
        }
    }
}

fn read_raw_config_for_bootstrap(base_dir: &Path, environment: Option<&str>) -> Option<Value> {
    let main_path = base_dir.join("appsettings.json");
    let main_text = fs::read_to_string(&main_path).ok()?;
    let mut root: Value = serde_json::from_str(&main_text).ok()?;

    if let Some(env_name) = environment.filter(|s| !s.trim().is_empty()) {
        let env_path = base_dir.join(format!("appsettings.{env_name}.json"));
        if let Ok(env_text) = fs::read_to_string(&env_path) {
            if let Ok(env_val) = serde_json::from_str::<Value>(&env_text) {
                crate::config::merge_json_values(&mut root, &env_val);
            }
        }
    }
    Some(root)
}

/// Entrypoint for running either as a Windows Service (when launched by the SCM)
/// or interactively in a terminal.
pub fn run_service_or_console(base_dir: PathBuf) -> i32 {
    #[cfg(windows)]
    {
        if let Ok(code) = windows_scm::try_run_as_windows_service(base_dir.clone()) {
            return code;
        }
    }

    run_console_host(base_dir)
}

fn run_console_host(base_dir: PathBuf) -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to initialize Tokio runtime: {e}");
            return exit_codes::UNEXPECTED_FAILURE;
        }
    };

    rt.block_on(async move {
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = shutdown_tx.send(());
        });
        run_host(base_dir, shutdown_rx).await
    })
}

#[cfg(windows)]
mod windows_scm {
    use super::*;
    use crate::constants::WINDOWS_SERVICE_NAME;
    use std::ffi::OsString;
    use std::sync::OnceLock;
    use std::time::Duration;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    static SERVICE_BASE_DIR: OnceLock<PathBuf> = OnceLock::new();
    static SERVICE_EXIT_CODE: OnceLock<i32> = OnceLock::new();

    windows_service::define_windows_service!(ffi_service_main, service_main);

    /// Attempt to connect to the Windows Service Control Manager.
    /// If the process was started interactively from a console rather than by the SCM,
    /// `service_dispatcher::start` returns `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` (1063)
    /// and we fall back to `run_console_host`.
    pub fn try_run_as_windows_service(base_dir: PathBuf) -> Result<i32, ()> {
        let _ = SERVICE_BASE_DIR.set(base_dir);
        match service_dispatcher::start(WINDOWS_SERVICE_NAME, ffi_service_main) {
            Ok(()) => Ok(*SERVICE_EXIT_CODE.get().unwrap_or(&exit_codes::SUCCESS)),
            Err(_) => Err(()),
        }
    }

    fn service_main(_arguments: Vec<OsString>) {
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let shutdown_tx_cell = std::sync::Mutex::new(Some(shutdown_tx));

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Ok(mut guard) = shutdown_tx_cell.lock() {
                        if let Some(tx) = guard.take() {
                            let _ = tx.send(());
                        }
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let Ok(status_handle) =
            service_control_handler::register(WINDOWS_SERVICE_NAME, event_handler)
        else {
            let _ = SERVICE_EXIT_CODE.set(exit_codes::UNEXPECTED_FAILURE);
            return;
        };

        // Report Running to the SCM immediately (Phase 1 §6.1 & §6.7: StartupFailureService pattern
        // ensures early validation failures report their real Win32 exit code 13 or 1064 instead of timing out)
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });

        let base_dir = SERVICE_BASE_DIR
            .get()
            .cloned()
            .unwrap_or_else(resolve_base_directory);

        let exit_code = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(run_host(base_dir, shutdown_rx)),
            Err(_) => exit_codes::UNEXPECTED_FAILURE,
        };

        let _ = SERVICE_EXIT_CODE.set(exit_code);

        // Report Stopped with the Win32 exit code (0, 13, or 1064) so SCM event 7023 and recovery work
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(exit_code as u32),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });
    }
}
