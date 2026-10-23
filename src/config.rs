//! Configuration model, overlay binding (`ApplicationDefaults` + `Applications`),
//! Phase 1/2 validation rules, and live reload diffing.

use crate::duration::parse_timespan;
use crate::path_resolver::{resolve_executable, resolve_path};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

/// Log severity levels matching Serilog's `MinimumLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogLevel {
    Verbose = 0,
    Debug = 1,
    Information = 2,
    Warning = 3,
    Error = 4,
    Fatal = 5,
}

impl LogLevel {
    pub fn from_str_case_insensitive(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "verbose" | "trace" => Some(LogLevel::Verbose),
            "debug" => Some(LogLevel::Debug),
            "information" | "info" => Some(LogLevel::Information),
            "warning" | "warn" => Some(LogLevel::Warning),
            "error" => Some(LogLevel::Error),
            "fatal" | "critical" => Some(LogLevel::Fatal),
            _ => None,
        }
    }

    pub fn as_serilog_name(&self) -> &'static str {
        match self {
            LogLevel::Verbose => "Verbose",
            LogLevel::Debug => "Debug",
            LogLevel::Information => "Information",
            LogLevel::Warning => "Warning",
            LogLevel::Error => "Error",
            LogLevel::Fatal => "Fatal",
        }
    }
}

/// Resolved restart policy for an application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRestartPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub max_restarts: u32,
    pub restart_window: Duration,
}

/// Resolved startup health check settings for an application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedHealthCheck {
    pub enabled: bool,
    pub url: Option<String>,
    pub startup_timeout: Duration,
    pub retry_interval: Duration,
    pub request_timeout: Duration,
}

/// Resolved configuration for a single application after merging `ApplicationDefaults` and per-app settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedApplicationConfig {
    pub name: String,
    pub raw_executable_path: String,
    pub executable_path: PathBuf,
    pub arguments: String,
    pub raw_working_directory: String,
    pub working_directory: PathBuf,
    pub environment_variables: BTreeMap<String, String>,
    pub shutdown_timeout: Duration,
    pub version_arguments: Option<String>,
    pub preflight_port: Option<u16>,
    pub auto_start: bool,
    pub output_buffer_lines: usize,
    pub restart_policy: ResolvedRestartPolicy,
    pub health_check: ResolvedHealthCheck,
}

/// Resolved `SerilogOptions` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSerilogOptions {
    pub log_directory: PathBuf,
    pub minimum_level: LogLevel,
    pub retained_file_count_limit: usize,
    pub file_size_limit_bytes: u64,
    pub crash_context_lines: usize,
}

/// Resolved `ControlApi` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedControlApiOptions {
    pub url: String,
    pub bind_host: String,
    pub port: u16,
}

/// Complete validated configuration for the wrapper service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedWrapperConfig {
    pub base_dir: PathBuf,
    pub applications: Vec<ResolvedApplicationConfig>,
    pub control_api: ResolvedControlApiOptions,
    pub enable_console_logging: bool,
    pub serilog: ResolvedSerilogOptions,
    pub state_file: PathBuf,
}

/// Summary of differences when reloading configuration at runtime (Phase 2 §5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigReloadDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub unchanged: Vec<String>,
    pub requires_service_restart: Vec<String>,
}

impl ConfigReloadDiff {
    pub fn summary_string(&self) -> String {
        let added_str = if self.added.is_empty() {
            "none".to_string()
        } else {
            self.added.join(", ")
        };
        let removed_str = if self.removed.is_empty() {
            "none".to_string()
        } else {
            self.removed.join(", ")
        };
        let changed_str = if self.changed.is_empty() {
            "none".to_string()
        } else {
            self.changed.join(", ")
        };
        format!(
            "added: {}; removed: {}; restarted: {}; unchanged: {}",
            added_str,
            removed_str,
            changed_str,
            self.unchanged.len()
        )
    }
}

/// Compute the diff between an active configuration and a newly validated configuration.
pub fn diff_configurations(
    old: &ValidatedWrapperConfig,
    new: &ValidatedWrapperConfig,
) -> ConfigReloadDiff {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();

    for new_app in &new.applications {
        if let Some(old_app) = old
            .applications
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(&new_app.name))
        {
            if old_app == new_app {
                unchanged.push(new_app.name.clone());
            } else {
                changed.push(new_app.name.clone());
            }
        } else {
            added.push(new_app.name.clone());
        }
    }

    for old_app in &old.applications {
        if !new
            .applications
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(&old_app.name))
        {
            removed.push(old_app.name.clone());
        }
    }

    let mut requires_service_restart = Vec::new();
    if old.control_api != new.control_api {
        requires_service_restart.push("ControlApi".to_string());
    }
    if old.serilog != new.serilog {
        requires_service_restart.push("SerilogOptions".to_string());
    }
    if old.enable_console_logging != new.enable_console_logging {
        requires_service_restart.push("ENABLE_CONSOLE_LOGGING".to_string());
    }
    if old.state_file != new.state_file {
        requires_service_restart.push("StateFile".to_string());
    }

    ConfigReloadDiff {
        added,
        removed,
        changed,
        unchanged,
        requires_service_restart,
    }
}

/// Extract raw bootstrap logging settings before full validation runs (Phase 1 §6.1 step 2).
pub fn read_bootstrap_serilog_options(
    raw_json: Option<&Value>,
    base_dir: &Path,
) -> (ResolvedSerilogOptions, bool) {
    let default_log_dir = base_dir.join("Logs");
    let mut opts = ResolvedSerilogOptions {
        log_directory: default_log_dir,
        minimum_level: LogLevel::Information,
        retained_file_count_limit: 31,
        file_size_limit_bytes: 104_857_600,
        crash_context_lines: 50,
    };
    let mut enable_console = false;

    if let Some(root) = raw_json.and_then(|v| v.as_object()) {
        if let Some(flag) = get_field_ci(root, "ENABLE_CONSOLE_LOGGING").and_then(|v| v.as_bool()) {
            enable_console = flag;
        }
        if let Some(serilog_obj) = get_field_ci(root, "SerilogOptions").and_then(|v| v.as_object()) {
            if let Some(dir_str) = get_field_ci(serilog_obj, "LogDirectory")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
            {
                opts.log_directory = resolve_path(dir_str, base_dir);
            }
            if let Some(lvl_str) = get_field_ci(serilog_obj, "MinimumLevel").and_then(|v| v.as_str())
            {
                if let Some(lvl) = LogLevel::from_str_case_insensitive(lvl_str) {
                    opts.minimum_level = lvl;
                }
            }
            if let Some(retained) =
                get_field_ci(serilog_obj, "RetainedFileCountLimit").and_then(|v| v.as_u64())
            {
                opts.retained_file_count_limit = retained as usize;
            }
            if let Some(size_limit) =
                get_field_ci(serilog_obj, "FileSizeLimitBytes").and_then(|v| v.as_u64())
            {
                opts.file_size_limit_bytes = size_limit;
            }
            if let Some(crash_lines) =
                get_field_ci(serilog_obj, "CrashContextLines").and_then(|v| v.as_u64())
            {
                opts.crash_context_lines = crash_lines as usize;
            }
        }
    }

    (opts, enable_console)
}

/// Load `appsettings.json` (and overlaid `appsettings.{Environment}.json` when `DOTNET_ENVIRONMENT` is set)
/// from `base_dir` and validate all settings.
pub fn load_and_validate_from_dir(
    base_dir: &Path,
    environment: Option<&str>,
) -> Result<ValidatedWrapperConfig, Vec<String>> {
    let main_path = base_dir.join("appsettings.json");
    if !main_path.exists() {
        return Err(vec![format!(
            "Configuration file '{}' was not found",
            main_path.display()
        )]);
    }

    let main_text = fs::read_to_string(&main_path)
        .map_err(|e| vec![format!("Failed to read '{}': {e}", main_path.display())])?;
    let mut root_val: Value = serde_json::from_str(&main_text)
        .map_err(|e| vec![format!("Invalid JSON in '{}': {e}", main_path.display())])?;

    if let Some(env_name) = environment.filter(|s| !s.trim().is_empty()) {
        let env_path = base_dir.join(format!("appsettings.{env_name}.json"));
        if env_path.exists() {
            let env_text = fs::read_to_string(&env_path)
                .map_err(|e| vec![format!("Failed to read '{}': {e}", env_path.display())])?;
            let env_val: Value = serde_json::from_str(&env_text)
                .map_err(|e| vec![format!("Invalid JSON in '{}': {e}", env_path.display())])?;
            merge_json_values(&mut root_val, &env_val);
        }
    }

    validate_config_value(&root_val, base_dir)
}

/// Merge `overlay` onto `base` following .NET `IConfiguration` hierarchical binding semantics.
pub fn merge_json_values(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (k, v) in overlay_map {
                let existing_key = base_map
                    .keys()
                    .find(|bk| bk.eq_ignore_ascii_case(k))
                    .cloned();
                if let Some(ek) = existing_key {
                    if let Some(existing_val) = base_map.get_mut(&ek) {
                        merge_json_values(existing_val, v);
                    }
                } else {
                    base_map.insert(k.clone(), v.clone());
                }
            }
        }
        (base_slot, overlay_val) => {
            *base_slot = overlay_val.clone();
        }
    }
}

/// Validate a parsed JSON configuration value against all Phase 1 and Phase 2 rules,
/// collecting every problem found into a single error list.
pub fn validate_config_value(
    root_val: &Value,
    base_dir: &Path,
) -> Result<ValidatedWrapperConfig, Vec<String>> {
    let mut errors = Vec::new();

    let Some(root) = root_val.as_object() else {
        return Err(vec!["Root configuration must be a JSON object".to_string()]);
    };

    // 1. ENABLE_CONSOLE_LOGGING
    let mut enable_console_logging = false;
    if let Some(v) = get_field_ci(root, "ENABLE_CONSOLE_LOGGING") {
        match v.as_bool() {
            Some(b) => enable_console_logging = b,
            None => errors.push(
                "ENABLE_CONSOLE_LOGGING: Must be a boolean (true or false)".to_string(),
            ),
        }
    }

    // 2. SerilogOptions
    let serilog = validate_serilog_options(root, base_dir, &mut errors);

    // 3. StateFile
    let state_file = match get_field_ci(root, "StateFile") {
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => resolve_path(s, base_dir),
            _ => {
                errors.push("StateFile: Must be a non-empty path string".to_string());
                base_dir.join("state.json")
            }
        },
        None => base_dir.join("state.json"),
    };

    // 4. ControlApi
    let control_api = validate_control_api_options(root, &mut errors);

    // 5. Applications (Phase 2 `ApplicationDefaults` + `Applications`, or Phase 1 `ServiceWrapper` fallback)
    let mut applications = Vec::new();
    let defaults_val = get_field_ci(root, "ApplicationDefaults");

    if let Some(apps_val) = get_field_ci(root, "Applications") {
        match apps_val.as_object() {
            Some(apps_map) if !apps_map.is_empty() => {
                let mut seen_names_lower: HashMap<String, String> = HashMap::new();
                for (app_name, app_section) in apps_map {
                    if !is_valid_app_name(app_name) {
                        errors.push(format!(
                            "Applications:{app_name}: Application name '{app_name}' is invalid (must match ^[A-Za-z0-9._-]+$)"
                        ));
                    }
                    let lower = app_name.to_ascii_lowercase();
                    if let Some(prev) = seen_names_lower.insert(lower, app_name.clone()) {
                        errors.push(format!(
                            "Applications:{app_name}: Duplicate application name (conflicts with '{prev}')"
                        ));
                    }

                    // Overlay ApplicationDefaults with the app's own section
                    let mut combined = defaults_val.cloned().unwrap_or_else(|| Value::Object(Default::default()));
                    merge_json_values(&mut combined, app_section);

                    if let Some(resolved) = validate_single_application(
                        app_name,
                        &format!("Applications:{app_name}"),
                        &combined,
                        base_dir,
                        &mut errors,
                    ) {
                        applications.push(resolved);
                    }
                }
            }
            Some(_) => {
                errors.push("Applications: At least one application must be defined".to_string());
            }
            None => {
                errors.push("Applications: Must be a JSON object keyed by application name".to_string());
            }
        }
    } else if let Some(sw_val) = get_field_ci(root, "ServiceWrapper") {
        // Phase 1 single-application configuration compatibility
        let mut combined = sw_val.clone();
        if let Some(combined_obj) = combined.as_object_mut() {
            if let Some(rp) = get_field_ci(root, "RestartPolicy") {
                combined_obj.insert("RestartPolicy".to_string(), rp.clone());
            }
            if let Some(hc) = get_field_ci(root, "HealthCheck") {
                combined_obj.insert("HealthCheck".to_string(), hc.clone());
            }
        }
        let app_name = sw_val
            .as_object()
            .and_then(|o| get_field_ci(o, "AppName"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if app_name.is_empty() {
            errors.push("ServiceWrapper:AppName: AppName is required".to_string());
        } else if !is_valid_app_name(&app_name) {
            errors.push(format!(
                "ServiceWrapper:AppName: '{app_name}' is invalid (must match ^[A-Za-z0-9._-]+$)"
            ));
        }

        let label_name = if app_name.is_empty() {
            "default"
        } else {
            &app_name
        };
        if let Some(resolved) = validate_single_application(
            label_name,
            "ServiceWrapper",
            &combined,
            base_dir,
            &mut errors,
        ) {
            applications.push(resolved);
        }
    } else {
        errors.push("Applications: At least one application must be defined".to_string());
    }

    // 6. Cross-application and ControlApi port collision checks (Phase 2 §4.2)
    validate_cross_app_ports(&applications, &control_api, &mut errors);

    if errors.is_empty() {
        Ok(ValidatedWrapperConfig {
            base_dir: base_dir.to_path_buf(),
            applications,
            control_api,
            enable_console_logging,
            serilog,
            state_file,
        })
    } else {
        Err(errors)
    }
}

fn is_valid_app_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn validate_serilog_options(
    root: &serde_json::Map<String, Value>,
    base_dir: &Path,
    errors: &mut Vec<String>,
) -> ResolvedSerilogOptions {
    let mut opts = ResolvedSerilogOptions {
        log_directory: base_dir.join("Logs"),
        minimum_level: LogLevel::Information,
        retained_file_count_limit: 31,
        file_size_limit_bytes: 104_857_600,
        crash_context_lines: 50,
    };

    let Some(val) = get_field_ci(root, "SerilogOptions") else {
        return opts;
    };
    let Some(obj) = val.as_object() else {
        errors.push("SerilogOptions: Must be a JSON object".to_string());
        return opts;
    };

    if let Some(dir_val) = get_field_ci(obj, "LogDirectory") {
        match dir_val.as_str() {
            Some(s) if !s.trim().is_empty() => {
                opts.log_directory = resolve_path(s, base_dir);
            }
            _ => errors.push("SerilogOptions:LogDirectory: Must be a non-empty string".to_string()),
        }
    }

    if let Some(lvl_val) = get_field_ci(obj, "MinimumLevel") {
        match lvl_val.as_str().and_then(LogLevel::from_str_case_insensitive) {
            Some(lvl) => opts.minimum_level = lvl,
            None => errors.push(format!(
                "SerilogOptions:MinimumLevel: Invalid log level '{}' (expected Verbose, Debug, Information, Warning, Error, or Fatal)",
                lvl_val
            )),
        }
    }

    if let Some(ret_val) = get_field_ci(obj, "RetainedFileCountLimit") {
        match ret_val.as_u64() {
            Some(n) if n >= 1 => opts.retained_file_count_limit = n as usize,
            _ => errors.push(
                "SerilogOptions:RetainedFileCountLimit: Must be a positive integer".to_string(),
            ),
        }
    }

    if let Some(size_val) = get_field_ci(obj, "FileSizeLimitBytes") {
        match size_val.as_u64() {
            Some(n) if n >= 1024 => opts.file_size_limit_bytes = n,
            _ => errors.push(
                "SerilogOptions:FileSizeLimitBytes: Must be an integer >= 1024".to_string(),
            ),
        }
    }

    if let Some(crash_val) = get_field_ci(obj, "CrashContextLines") {
        match crash_val.as_u64() {
            Some(n) => opts.crash_context_lines = n as usize,
            None => errors.push(
                "SerilogOptions:CrashContextLines: Must be a non-negative integer".to_string(),
            ),
        }
    }

    opts
}

fn validate_control_api_options(
    root: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) -> ResolvedControlApiOptions {
    let default_url = "http://127.0.0.1:5080".to_string();
    let raw_url = match get_field_ci(root, "ControlApi") {
        Some(val) => match val.as_object() {
            Some(obj) => match get_field_ci(obj, "Url") {
                Some(u) => match u.as_str() {
                    Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                    _ => {
                        errors.push("ControlApi:Url: Must be a non-empty URL string".to_string());
                        return ResolvedControlApiOptions {
                            url: default_url,
                            bind_host: "127.0.0.1".to_string(),
                            port: 5080,
                        };
                    }
                },
                None => default_url.clone(),
            },
            None => {
                errors.push("ControlApi: Must be a JSON object".to_string());
                return ResolvedControlApiOptions {
                    url: default_url,
                    bind_host: "127.0.0.1".to_string(),
                    port: 5080,
                };
            }
        },
        None => default_url.clone(),
    };

    match Url::parse(&raw_url) {
        Ok(parsed) => {
            if parsed.scheme() != "http" {
                errors.push(format!(
                    "ControlApi:Url: Must use the 'http' scheme on loopback (got '{}')",
                    parsed.scheme()
                ));
            }
            let host = parsed.host_str().unwrap_or("");
            let is_loopback = matches!(
                host.to_ascii_lowercase().as_str(),
                "127.0.0.1" | "localhost" | "[::1]" | "::1"
            );
            if !is_loopback {
                errors.push(format!(
                    "ControlApi:Url: Must bind to a loopback address (127.0.0.1, ::1, or localhost), got '{host}'"
                ));
            }
            let port = parsed.port_or_known_default().unwrap_or(80);
            let bind_host = if host == "[::1]" || host == "::1" {
                "::1".to_string()
            } else {
                "127.0.0.1".to_string()
            };
            ResolvedControlApiOptions {
                url: raw_url.trim_end_matches('/').to_string(),
                bind_host,
                port,
            }
        }
        Err(e) => {
            errors.push(format!("ControlApi:Url: Invalid URL '{raw_url}': {e}"));
            ResolvedControlApiOptions {
                url: default_url,
                bind_host: "127.0.0.1".to_string(),
                port: 5080,
            }
        }
    }
}

fn validate_single_application(
    app_name: &str,
    prefix: &str,
    combined_val: &Value,
    base_dir: &Path,
    errors: &mut Vec<String>,
) -> Option<ResolvedApplicationConfig> {
    let Some(obj) = combined_val.as_object() else {
        errors.push(format!("{prefix}: Application section must be a JSON object"));
        return None;
    };

    // ExecutablePath
    let mut raw_exe = String::new();
    let mut resolved_exe = PathBuf::new();
    match get_field_ci(obj, "ExecutablePath") {
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => {
                raw_exe = s.trim().to_string();
                match resolve_executable(&raw_exe, base_dir) {
                    Some(p) => resolved_exe = p,
                    None => errors.push(format!(
                        "{prefix}:ExecutablePath: Executable '{raw_exe}' does not exist or could not be found in PATH"
                    )),
                }
            }
            _ => errors.push(format!("{prefix}:ExecutablePath: ExecutablePath is required")),
        },
        None => errors.push(format!("{prefix}:ExecutablePath: ExecutablePath is required")),
    }

    // Arguments
    let arguments = match get_field_ci(obj, "Arguments") {
        Some(v) if !v.is_null() => match v.as_str() {
            Some(s) => s.to_string(),
            None => {
                errors.push(format!("{prefix}:Arguments: Must be a string"));
                String::new()
            }
        },
        _ => String::new(),
    };

    // WorkingDirectory
    let mut raw_work_dir = String::new();
    let mut working_directory = PathBuf::new();
    match get_field_ci(obj, "WorkingDirectory") {
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => {
                raw_work_dir = s.trim().to_string();
                let candidate = resolve_path(&raw_work_dir, base_dir);
                if candidate.is_dir() {
                    working_directory = candidate;
                } else {
                    errors.push(format!(
                        "{prefix}:WorkingDirectory: Working directory '{}' ('{}') does not exist",
                        raw_work_dir,
                        candidate.display()
                    ));
                }
            }
            _ => errors.push(format!(
                "{prefix}:WorkingDirectory: WorkingDirectory is required"
            )),
        },
        None => errors.push(format!(
            "{prefix}:WorkingDirectory: WorkingDirectory is required"
        )),
    }

    // EnvironmentVariables
    let mut environment_variables = BTreeMap::new();
    if let Some(env_val) = get_field_ci(obj, "EnvironmentVariables") {
        if !env_val.is_null() {
            match env_val.as_object() {
                Some(env_map) => {
                    for (k, v) in env_map {
                        match v.as_str() {
                            Some(s) => {
                                environment_variables.insert(k.clone(), s.to_string());
                            }
                            None if v.is_number() || v.is_boolean() => {
                                environment_variables.insert(k.clone(), v.to_string());
                            }
                            None => errors.push(format!(
                                "{prefix}:EnvironmentVariables:{k}: Value must be a string"
                            )),
                        }
                    }
                }
                None => errors.push(format!(
                    "{prefix}:EnvironmentVariables: Must be a JSON object of string key/value pairs"
                )),
            }
        }
    }

    // ShutdownTimeout (required, between 1 and 25 seconds)
    let mut shutdown_timeout = Duration::from_secs(10);
    match get_field_ci(obj, "ShutdownTimeout") {
        Some(v) => match v.as_str() {
            Some(s) => match parse_timespan(s) {
                Ok(d) => {
                    if d < Duration::from_secs(1) || d > Duration::from_secs(25) {
                        errors.push(format!(
                            "{prefix}:ShutdownTimeout: Must be between 1 and 25 seconds (got '{s}')"
                        ));
                    } else {
                        shutdown_timeout = d;
                    }
                }
                Err(e) => errors.push(format!("{prefix}:ShutdownTimeout: {e}")),
            },
            None => errors.push(format!(
                "{prefix}:ShutdownTimeout: Must be a duration string in format 'd.hh:mm:ss'"
            )),
        },
        None => errors.push(format!(
            "{prefix}:ShutdownTimeout: ShutdownTimeout is required"
        )),
    }

    // VersionArguments
    let version_arguments = match get_field_ci(obj, "VersionArguments") {
        Some(v) if !v.is_null() => match v.as_str() {
            Some(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            None => {
                errors.push(format!("{prefix}:VersionArguments: Must be a string"));
                None
            }
        },
        _ => None,
    };

    // PreflightPort
    let mut preflight_port = None;
    if let Some(v) = get_field_ci(obj, "PreflightPort") {
        if !v.is_null() {
            match v.as_u64() {
                Some(p) if (1..=65535).contains(&p) => preflight_port = Some(p as u16),
                _ => errors.push(format!(
                    "{prefix}:PreflightPort: Must be a valid TCP port between 1 and 65535"
                )),
            }
        }
    }

    // AutoStart (default true)
    let mut auto_start = true;
    if let Some(v) = get_field_ci(obj, "AutoStart") {
        if !v.is_null() {
            match v.as_bool() {
                Some(b) => auto_start = b,
                None => errors.push(format!("{prefix}:AutoStart: Must be a boolean")),
            }
        }
    }

    // OutputBufferLines (default 500)
    let mut output_buffer_lines = 500usize;
    if let Some(v) = get_field_ci(obj, "OutputBufferLines") {
        if !v.is_null() {
            match v.as_u64() {
                Some(n) if n >= 1 => output_buffer_lines = n as usize,
                _ => errors.push(format!(
                    "{prefix}:OutputBufferLines: Must be a positive integer"
                )),
            }
        }
    }

    // RestartPolicy
    let restart_policy = validate_restart_policy(obj, prefix, errors);

    // HealthCheck
    let health_check = validate_health_check(obj, prefix, errors);

    Some(ResolvedApplicationConfig {
        name: app_name.to_string(),
        raw_executable_path: raw_exe,
        executable_path: resolved_exe,
        arguments,
        raw_working_directory: raw_work_dir,
        working_directory,
        environment_variables,
        shutdown_timeout,
        version_arguments,
        preflight_port,
        auto_start,
        output_buffer_lines,
        restart_policy,
        health_check,
    })
}

fn validate_restart_policy(
    app_obj: &serde_json::Map<String, Value>,
    prefix: &str,
    errors: &mut Vec<String>,
) -> ResolvedRestartPolicy {
    let mut policy = ResolvedRestartPolicy {
        initial_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(60),
        max_restarts: 5,
        restart_window: Duration::from_secs(600),
    };

    let Some(rp_val) = get_field_ci(app_obj, "RestartPolicy") else {
        errors.push(format!("{prefix}:RestartPolicy: RestartPolicy section is required"));
        return policy;
    };
    let Some(rp_obj) = rp_val.as_object() else {
        errors.push(format!("{prefix}:RestartPolicy: Must be a JSON object"));
        return policy;
    };

    let mut initial_ok = false;
    let mut max_ok = false;

    match get_field_ci(rp_obj, "InitialDelay") {
        Some(v) => match v.as_str() {
            Some(s) => match parse_timespan(s) {
                Ok(d) if !d.is_zero() => {
                    policy.initial_delay = d;
                    initial_ok = true;
                }
                Ok(_) => errors.push(format!(
                    "{prefix}:RestartPolicy:InitialDelay: Must be greater than zero"
                )),
                Err(e) => errors.push(format!("{prefix}:RestartPolicy:InitialDelay: {e}")),
            },
            None => errors.push(format!(
                "{prefix}:RestartPolicy:InitialDelay: Must be a duration string"
            )),
        },
        None => errors.push(format!(
            "{prefix}:RestartPolicy:InitialDelay: InitialDelay is required"
        )),
    }

    match get_field_ci(rp_obj, "MaxDelay") {
        Some(v) => match v.as_str() {
            Some(s) => match parse_timespan(s) {
                Ok(d) if !d.is_zero() => {
                    policy.max_delay = d;
                    max_ok = true;
                }
                Ok(_) => errors.push(format!(
                    "{prefix}:RestartPolicy:MaxDelay: Must be greater than zero"
                )),
                Err(e) => errors.push(format!("{prefix}:RestartPolicy:MaxDelay: {e}")),
            },
            None => errors.push(format!(
                "{prefix}:RestartPolicy:MaxDelay: Must be a duration string"
            )),
        },
        None => errors.push(format!(
            "{prefix}:RestartPolicy:MaxDelay: MaxDelay is required"
        )),
    }

    if initial_ok && max_ok && policy.max_delay < policy.initial_delay {
        errors.push(format!(
            "{prefix}:RestartPolicy:MaxDelay: MaxDelay must be at least InitialDelay"
        ));
    }

    match get_field_ci(rp_obj, "MaxRestarts") {
        Some(v) => match v.as_u64() {
            Some(n) if n <= u32::MAX as u64 => policy.max_restarts = n as u32,
            _ => errors.push(format!(
                "{prefix}:RestartPolicy:MaxRestarts: Must be a non-negative integer"
            )),
        },
        None => errors.push(format!(
            "{prefix}:RestartPolicy:MaxRestarts: MaxRestarts is required"
        )),
    }

    match get_field_ci(rp_obj, "RestartWindow") {
        Some(v) => match v.as_str() {
            Some(s) => match parse_timespan(s) {
                Ok(d) if !d.is_zero() => policy.restart_window = d,
                Ok(_) => errors.push(format!(
                    "{prefix}:RestartPolicy:RestartWindow: Must be greater than zero"
                )),
                Err(e) => errors.push(format!("{prefix}:RestartPolicy:RestartWindow: {e}")),
            },
            None => errors.push(format!(
                "{prefix}:RestartPolicy:RestartWindow: Must be a duration string"
            )),
        },
        None => errors.push(format!(
            "{prefix}:RestartPolicy:RestartWindow: RestartWindow is required"
        )),
    }

    policy
}

fn validate_health_check(
    app_obj: &serde_json::Map<String, Value>,
    prefix: &str,
    errors: &mut Vec<String>,
) -> ResolvedHealthCheck {
    let mut hc = ResolvedHealthCheck {
        enabled: false,
        url: None,
        startup_timeout: Duration::from_secs(60),
        retry_interval: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
    };

    let Some(hc_val) = get_field_ci(app_obj, "HealthCheck") else {
        errors.push(format!("{prefix}:HealthCheck: HealthCheck section is required"));
        return hc;
    };
    let Some(hc_obj) = hc_val.as_object() else {
        errors.push(format!("{prefix}:HealthCheck: Must be a JSON object"));
        return hc;
    };

    match get_field_ci(hc_obj, "Enabled") {
        Some(v) => match v.as_bool() {
            Some(b) => hc.enabled = b,
            None => {
                errors.push(format!("{prefix}:HealthCheck:Enabled: Must be a boolean"));
                return hc;
            }
        },
        None => {
            errors.push(format!("{prefix}:HealthCheck:Enabled: Enabled is required"));
            return hc;
        }
    }

    // When Enabled == false, Url and timeouts are ignored and not validated (Phase 1 §5.2)
    if !hc.enabled {
        return hc;
    }

    // Enabled == true: validate Url and all three timeouts
    match get_field_ci(hc_obj, "Url") {
        Some(v) => match v.as_str() {
            Some(s) if !s.trim().is_empty() => match Url::parse(s.trim()) {
                Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                    if parsed.host_str().is_none() {
                        errors.push(format!(
                            "{prefix}:HealthCheck:Url: Must be an absolute http or https URL"
                        ));
                    } else {
                        hc.url = Some(s.trim().to_string());
                    }
                }
                _ => errors.push(format!(
                    "{prefix}:HealthCheck:Url: Must be an absolute http or https URL (got '{s}')"
                )),
            },
            _ => errors.push(format!(
                "{prefix}:HealthCheck:Url: Url is required when HealthCheck:Enabled is true"
            )),
        },
        None => errors.push(format!(
            "{prefix}:HealthCheck:Url: Url is required when HealthCheck:Enabled is true"
        )),
    }

    let mut startup_ok = false;
    let mut request_ok = false;

    match get_field_ci(hc_obj, "StartupTimeout") {
        Some(v) => match v.as_str() {
            Some(s) => match parse_timespan(s) {
                Ok(d) if !d.is_zero() => {
                    hc.startup_timeout = d;
                    startup_ok = true;
                }
                Ok(_) => errors.push(format!(
                    "{prefix}:HealthCheck:StartupTimeout: Must be greater than zero"
                )),
                Err(e) => errors.push(format!("{prefix}:HealthCheck:StartupTimeout: {e}")),
            },
            None => errors.push(format!(
                "{prefix}:HealthCheck:StartupTimeout: Must be a duration string"
            )),
        },
        None => errors.push(format!(
            "{prefix}:HealthCheck:StartupTimeout: StartupTimeout is required when HealthCheck:Enabled is true"
        )),
    }

    match get_field_ci(hc_obj, "RetryInterval") {
        Some(v) => match v.as_str() {
            Some(s) => match parse_timespan(s) {
                Ok(d) if !d.is_zero() => hc.retry_interval = d,
                Ok(_) => errors.push(format!(
                    "{prefix}:HealthCheck:RetryInterval: Must be greater than zero"
                )),
                Err(e) => errors.push(format!("{prefix}:HealthCheck:RetryInterval: {e}")),
            },
            None => errors.push(format!(
                "{prefix}:HealthCheck:RetryInterval: Must be a duration string"
            )),
        },
        None => errors.push(format!(
            "{prefix}:HealthCheck:RetryInterval: RetryInterval is required when HealthCheck:Enabled is true"
        )),
    }

    match get_field_ci(hc_obj, "RequestTimeout") {
        Some(v) => match v.as_str() {
            Some(s) => match parse_timespan(s) {
                Ok(d) if !d.is_zero() => {
                    hc.request_timeout = d;
                    request_ok = true;
                }
                Ok(_) => errors.push(format!(
                    "{prefix}:HealthCheck:RequestTimeout: Must be greater than zero"
                )),
                Err(e) => errors.push(format!("{prefix}:HealthCheck:RequestTimeout: {e}")),
            },
            None => errors.push(format!(
                "{prefix}:HealthCheck:RequestTimeout: Must be a duration string"
            )),
        },
        None => errors.push(format!(
            "{prefix}:HealthCheck:RequestTimeout: RequestTimeout is required when HealthCheck:Enabled is true"
        )),
    }

    if startup_ok && request_ok && hc.request_timeout > hc.startup_timeout {
        errors.push(format!(
            "{prefix}:HealthCheck:RequestTimeout: RequestTimeout must not be longer than StartupTimeout"
        ));
    }

    hc
}

fn validate_cross_app_ports(
    applications: &[ResolvedApplicationConfig],
    control_api: &ResolvedControlApiOptions,
    errors: &mut Vec<String>,
) {
    let mut preflight_owners: HashMap<u16, String> = HashMap::new();
    let mut env_port_owners: HashMap<u16, String> = HashMap::new();

    for app in applications {
        if let Some(p) = app.preflight_port {
            if p == control_api.port {
                errors.push(format!(
                    "Applications:{}:PreflightPort: Port {p} conflicts with ControlApi:Url port {}",
                    app.name, control_api.port
                ));
            }
            if let Some(prev) = preflight_owners.insert(p, app.name.clone()) {
                errors.push(format!(
                    "Applications:{}:PreflightPort: Port {p} is already used by application '{prev}'",
                    app.name
                ));
            }
        }

        if let Some(port_str) = app
            .environment_variables
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("PORT"))
            .map(|(_, v)| v)
        {
            if let Ok(env_port) = port_str.trim().parse::<u16>() {
                if env_port == control_api.port {
                    errors.push(format!(
                        "Applications:{}:EnvironmentVariables:PORT: Port {env_port} conflicts with ControlApi:Url port {}",
                        app.name, control_api.port
                    ));
                }
                if let Some(prev) = env_port_owners.insert(env_port, app.name.clone()) {
                    errors.push(format!(
                        "Applications:{}:EnvironmentVariables:PORT: Port {env_port} is already used by application '{prev}'",
                        app.name
                    ));
                }
                // Also check cross-collision with another app's PreflightPort
                if let Some(other) = preflight_owners.get(&env_port) {
                    if !other.eq_ignore_ascii_case(&app.name) {
                        errors.push(format!(
                            "Applications:{}:EnvironmentVariables:PORT: Port {env_port} conflicts with PreflightPort of application '{other}'",
                            app.name
                        ));
                    }
                }
            }
        }
    }
}

fn get_field_ci<'a>(map: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a Value> {
    if let Some(v) = map.get(key) {
        return Some(v);
    }
    map.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn create_test_env() -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let wrapper_dir = tmp.path().join("wrapper");
        let app1_dir = tmp.path().join("apps").join("storefront");
        let app2_dir = tmp.path().join("apps").join("admin");
        fs::create_dir_all(&wrapper_dir).unwrap();
        fs::create_dir_all(&app1_dir).unwrap();
        fs::create_dir_all(&app2_dir).unwrap();
        let fake_node = wrapper_dir.join("fake-node.exe");
        fs::write(&fake_node, b"#!/bin/sh\n").unwrap();
        (tmp, wrapper_dir, app1_dir, app2_dir)
    }

    #[test]
    fn validate_config_value_should_overlay_defaults_and_merge_env_vars() {
        // Arrange
        let (_tmp, wrapper_dir, _app1_dir, _app2_dir) = create_test_env();
        let cfg_json = json!({
            "ApplicationDefaults": {
                "ExecutablePath": "./fake-node.exe",
                "Arguments": "server.js",
                "ShutdownTimeout": "0.00:00:10",
                "VersionArguments": "--version",
                "AutoStart": true,
                "EnvironmentVariables": { "NODE_ENV": "production", "HOSTNAME": "0.0.0.0" },
                "RestartPolicy": { "InitialDelay": "0.00:00:02", "MaxDelay": "0.00:01:00", "MaxRestarts": 5, "RestartWindow": "0.00:10:00" },
                "HealthCheck": { "Enabled": false, "StartupTimeout": "0.00:01:00", "RetryInterval": "0.00:00:02", "RequestTimeout": "0.00:00:05" }
            },
            "Applications": {
                "storefront": {
                    "WorkingDirectory": "../apps/storefront",
                    "EnvironmentVariables": { "PORT": "3000" },
                    "PreflightPort": 3000,
                    "HealthCheck": { "Enabled": true, "Url": "http://127.0.0.1:3000/api/health" }
                },
                "admin": {
                    "WorkingDirectory": "../apps/admin",
                    "EnvironmentVariables": { "PORT": "3001", "NODE_ENV": "staging" },
                    "PreflightPort": 3001,
                    "AutoStart": false
                }
            },
            "ControlApi": { "Url": "http://127.0.0.1:5080" }
        });

        // Act
        let validated = validate_config_value(&cfg_json, &wrapper_dir).unwrap();

        // Assert
        assert_eq!(validated.applications.len(), 2);
        let sf = validated
            .applications
            .iter()
            .find(|a| a.name == "storefront")
            .unwrap();
        assert_eq!(
            sf.environment_variables.get("NODE_ENV").map(|s| s.as_str()),
            Some("production")
        );
        assert_eq!(
            sf.environment_variables.get("PORT").map(|s| s.as_str()),
            Some("3000")
        );
        assert!(sf.health_check.enabled);
        assert!(sf.auto_start);

        let admin = validated
            .applications
            .iter()
            .find(|a| a.name == "admin")
            .unwrap();
        assert_eq!(
            admin.environment_variables.get("NODE_ENV").map(|s| s.as_str()),
            Some("staging")
        );
        assert!(!admin.health_check.enabled);
        assert!(!admin.auto_start);
    }

    #[test]
    fn validate_config_value_should_reject_duplicate_ports_and_non_loopback_control_api() {
        // Arrange
        let (_tmp, wrapper_dir, _app1_dir, _app2_dir) = create_test_env();
        let cfg_json = json!({
            "ApplicationDefaults": {
                "ExecutablePath": "./fake-node.exe",
                "ShutdownTimeout": "0.00:00:10",
                "RestartPolicy": { "InitialDelay": "0.00:00:02", "MaxDelay": "0.00:01:00", "MaxRestarts": 5, "RestartWindow": "0.00:10:00" },
                "HealthCheck": { "Enabled": false }
            },
            "Applications": {
                "storefront": {
                    "WorkingDirectory": "../apps/storefront",
                    "PreflightPort": 3000
                },
                "admin": {
                    "WorkingDirectory": "../apps/admin",
                    "PreflightPort": 3000
                }
            },
            "ControlApi": { "Url": "http://0.0.0.0:5080" }
        });

        // Act
        let err = validate_config_value(&cfg_json, &wrapper_dir).unwrap_err();

        // Assert
        assert!(err.iter().any(|e| e.contains("Port 3000 is already used")));
        assert!(err.iter().any(|e| e.contains("loopback address")));
    }

    #[test]
    fn validate_config_value_should_validate_health_check_only_when_enabled() {
        // Arrange
        let (_tmp, wrapper_dir, _app1_dir, _app2_dir) = create_test_env();
        let base_cfg = |enabled: bool, url: &str, req_timeout: &str| {
            json!({
                "ApplicationDefaults": {
                    "ExecutablePath": "./fake-node.exe",
                    "WorkingDirectory": "../apps/storefront",
                    "ShutdownTimeout": "0.00:00:10",
                    "RestartPolicy": { "InitialDelay": "0.00:00:02", "MaxDelay": "0.00:01:00", "MaxRestarts": 5, "RestartWindow": "0.00:10:00" },
                    "HealthCheck": {
                        "Enabled": enabled,
                        "Url": url,
                        "StartupTimeout": "0.00:00:10",
                        "RetryInterval": "0.00:00:02",
                        "RequestTimeout": req_timeout
                    }
                },
                "Applications": {
                    "storefront": {}
                }
            })
        };

        // Act & Assert: Invalid URL and zero timeout pass when Enabled == false
        assert!(validate_config_value(&base_cfg(false, "", "0.00:00:00"), &wrapper_dir).is_ok());

        // Fail when Enabled == true
        let errs =
            validate_config_value(&base_cfg(true, "/api/health", "0.00:00:00"), &wrapper_dir)
                .unwrap_err();
        assert!(errs.iter().any(|e| e.contains("HealthCheck:Url")));
        assert!(errs.iter().any(|e| e.contains("HealthCheck:RequestTimeout")));
    }
}
