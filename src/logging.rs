//! Structured two-stream file logger (Serilog Compact JSON for wrapper events + plain-text per-app logs)
//! with daily rolling, file-size limit rolling, retention pruning, fallback directory handling, and optional console output.

use crate::config::{LogLevel, ResolvedSerilogOptions};
use crate::constants::log_sources;
use chrono::{DateTime, Local, Utc};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Central thread-safe logger implementing Phase 1 §7 and Phase 2 §9 requirements.
#[derive(Clone)]
pub struct WrapperLogger {
    options: ResolvedSerilogOptions,
    enable_console: bool,
    machine_name: String,
    environment_name: String,
    inner: Arc<Mutex<LoggerHandles>>,
}

struct LoggerHandles {
    active_files: HashMap<String, ActiveLogFile>,
}

struct ActiveLogFile {
    date_str: String,
    roll_index: u32,
    current_size: u64,
    file: File,
}

impl WrapperLogger {
    /// Initialize the logger in the configured `LogDirectory`.
    /// If `LogDirectory` cannot be created or written to, falls back to `<base_dir>/Logs`,
    /// logs the error there, and returns `Err(error_message)` so the caller can exit with code `13`.
    pub fn initialize(
        options: ResolvedSerilogOptions,
        enable_console: bool,
        base_dir: &Path,
    ) -> Result<Self, (Self, String)> {
        let machine_name = env::var("COMPUTERNAME")
            .or_else(|_| env::var("HOSTNAME"))
            .unwrap_or_else(|_| "localhost".to_string());
        let environment_name = env::var("DOTNET_ENVIRONMENT")
            .or_else(|_| env::var("ASPNETCORE_ENVIRONMENT"))
            .unwrap_or_else(|_| "Production".to_string());

        match Self::verify_writable_directory(&options.log_directory) {
            Ok(()) => Ok(Self {
                options,
                enable_console,
                machine_name,
                environment_name,
                inner: Arc::new(Mutex::new(LoggerHandles {
                    active_files: HashMap::new(),
                })),
            }),
            Err(primary_err) => {
                let fallback_dir = base_dir.join("Logs");
                let _ = Self::verify_writable_directory(&fallback_dir);
                let mut fallback_opts = options.clone();
                fallback_opts.log_directory = fallback_dir;

                let fallback_logger = Self {
                    options: fallback_opts,
                    enable_console,
                    machine_name,
                    environment_name,
                    inner: Arc::new(Mutex::new(LoggerHandles {
                        active_files: HashMap::new(),
                    })),
                };

                let msg = format!(
                    "Invalid configuration | LogDirectory '{}' cannot be created or written to: {}",
                    options.log_directory.display(),
                    primary_err
                );
                fallback_logger.log_wrapper(
                    LogLevel::Fatal,
                    None,
                    &msg,
                    vec![
                        (
                            "ConfiguredLogDirectory",
                            Value::String(options.log_directory.display().to_string()),
                        ),
                        ("Error", Value::String(primary_err)),
                    ],
                );
                Err((fallback_logger, msg))
            }
        }
    }

    fn verify_writable_directory(dir: &Path) -> Result<(), String> {
        fs::create_dir_all(dir)
            .map_err(|e| format!("Failed to create directory '{}': {e}", dir.display()))?;
        let probe = dir.join(format!(".write-probe-{}", std::process::id()));
        {
            let mut f = File::create(&probe)
                .map_err(|e| format!("Failed to create probe file in '{}': {e}", dir.display()))?;
            f.write_all(b"ok")
                .map_err(|e| format!("Failed to write to '{}': {e}", dir.display()))?;
        }
        let _ = fs::remove_file(&probe);
        Ok(())
    }

    /// Log a structured wrapper lifecycle event to `wrapper-YYYYMMDD.log` (Compact JSON format).
    pub fn log_wrapper(
        &self,
        level: LogLevel,
        app_name: Option<&str>,
        message: &str,
        properties: Vec<(&str, Value)>,
    ) {
        if level < self.options.minimum_level {
            return;
        }

        let now_utc: DateTime<Utc> = Utc::now();
        let now_local: DateTime<Local> = Local::now();
        let date_str = now_local.format("%Y%m%d").to_string();

        let full_message = match app_name {
            Some(app) if !message.starts_with(app) => format!("{app} | {message}"),
            _ => message.to_string(),
        };

        let mut obj = Map::new();
        obj.insert(
            "@t".to_string(),
            Value::String(now_utc.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
        obj.insert("@m".to_string(), Value::String(full_message.clone()));
        obj.insert(
            "@l".to_string(),
            Value::String(level.as_serilog_name().to_string()),
        );
        obj.insert(
            "SourceContext".to_string(),
            Value::String(log_sources::WRAPPER.to_string()),
        );
        obj.insert(
            "MachineName".to_string(),
            Value::String(self.machine_name.clone()),
        );
        obj.insert(
            "EnvironmentName".to_string(),
            Value::String(self.environment_name.clone()),
        );
        if let Some(app) = app_name {
            obj.insert("AppName".to_string(), Value::String(app.to_string()));
        }
        for (k, v) in properties {
            obj.insert(k.to_string(), v);
        }

        let mut line = serde_json::to_string(&Value::Object(obj)).unwrap_or_else(|_| {
            json!({ "@m": full_message }).to_string()
        });
        line.push('\n');

        self.write_to_stream("wrapper", &date_str, line.as_bytes());

        if self.enable_console {
            let prefix = app_name
                .map(|a| format!("[{a}]"))
                .unwrap_or_else(|| "[wrapper]".to_string());
            let level_tag = match level {
                LogLevel::Verbose => "VRB",
                LogLevel::Debug => "DBG",
                LogLevel::Information => "INF",
                LogLevel::Warning => "WRN",
                LogLevel::Error => "ERR",
                LogLevel::Fatal => "FTL",
            };
            println!(
                "{} {} [{}] {}",
                prefix,
                now_local.format("%H:%M:%S"),
                level_tag,
                full_message
            );
        }
    }

    /// Log a child process stdout (`is_stderr = false` -> `INF`) or stderr (`is_stderr = true` -> `WRN`)
    /// line to `<AppName>-app-YYYYMMDD.log` in plain text format.
    pub fn log_app_output(&self, app_name: &str, line: &str, is_stderr: bool) {
        let level = if is_stderr {
            LogLevel::Warning
        } else {
            LogLevel::Information
        };
        if level < self.options.minimum_level {
            return;
        }

        let now_local: DateTime<Local> = Local::now();
        let date_str = now_local.format("%Y%m%d").to_string();
        let level_u3 = if is_stderr { "WRN" } else { "INF" };

        let formatted = format!(
            "{} [{}] {}\n",
            now_local.format("%Y-%m-%d %H:%M:%S%.3f %:z"),
            level_u3,
            line
        );

        let stream_prefix = format!("{app_name}-app");
        self.write_to_stream(&stream_prefix, &date_str, formatted.as_bytes());

        if self.enable_console {
            println!("[{}] [{}] {}", app_name, level_u3, line);
        }
    }

    /// Flush all open log file handles.
    pub fn flush(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for active in guard.active_files.values_mut() {
            let _ = active.file.flush();
        }
    }

    pub fn log_directory(&self) -> &Path {
        &self.options.log_directory
    }

    pub fn crash_context_lines(&self) -> usize {
        self.options.crash_context_lines
    }

    fn write_to_stream(&self, stream_prefix: &str, date_str: &str, bytes: &[u8]) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let limit = self.options.file_size_limit_bytes;

        let needs_reopen = match guard.active_files.get(stream_prefix) {
            Some(active) => {
                active.date_str != date_str
                    || (active.current_size > 0
                        && active.current_size.saturating_add(bytes.len() as u64) > limit)
            }
            None => true,
        };

        if needs_reopen {
            let next_index = match guard.active_files.get(stream_prefix) {
                Some(active) if active.date_str == date_str => active.roll_index + 1,
                _ => self.find_starting_roll_index(stream_prefix, date_str, bytes.len() as u64),
            };

            let file_path = self.build_log_file_path(stream_prefix, date_str, next_index);
            if let Ok(file) = OpenOptions::new().create(true).append(true).open(&file_path) {
                let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);
                guard.active_files.insert(
                    stream_prefix.to_string(),
                    ActiveLogFile {
                        date_str: date_str.to_string(),
                        roll_index: next_index,
                        current_size,
                        file,
                    },
                );
                self.enforce_retention(stream_prefix);
            }
        }

        if let Some(active) = guard.active_files.get_mut(stream_prefix) {
            if active.file.write_all(bytes).is_ok() {
                active.current_size += bytes.len() as u64;
                let _ = active.file.flush();
            }
        }
    }

    fn find_starting_roll_index(
        &self,
        stream_prefix: &str,
        date_str: &str,
        incoming_len: u64,
    ) -> u32 {
        let limit = self.options.file_size_limit_bytes;
        for idx in 0..1000 {
            let path = self.build_log_file_path(stream_prefix, date_str, idx);
            match fs::metadata(&path) {
                Ok(meta) => {
                    if meta.len().saturating_add(incoming_len) <= limit {
                        return idx;
                    }
                }
                Err(_) => return idx,
            }
        }
        0
    }

    fn build_log_file_path(&self, stream_prefix: &str, date_str: &str, roll_index: u32) -> PathBuf {
        let file_name = if roll_index == 0 {
            format!("{stream_prefix}-{date_str}.log")
        } else {
            format!("{stream_prefix}-{date_str}_{roll_index:03}.log")
        };
        self.options.log_directory.join(file_name)
    }

    fn enforce_retention(&self, stream_prefix: &str) {
        let max_files = self.options.retained_file_count_limit;
        if max_files == 0 {
            return;
        }
        let prefix_with_dash = format!("{stream_prefix}-");
        let Ok(entries) = fs::read_dir(&self.options.log_directory) else {
            return;
        };

        let mut matching: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| name.starts_with(&prefix_with_dash) && name.ends_with(".log"))
                    .unwrap_or(false)
            })
            .collect();

        if matching.len() <= max_files {
            return;
        }

        // Sort lexicographically by filename (YYYYMMDD[_NNN].log orders oldest first)
        matching.sort();
        let to_remove = matching.len() - max_files;
        for old_path in matching.into_iter().take(to_remove) {
            let _ = fs::remove_file(old_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn logger_should_write_compact_json_wrapper_log_and_plain_text_app_log() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().join("Logs");
        let opts = ResolvedSerilogOptions {
            log_directory: log_dir.clone(),
            minimum_level: LogLevel::Information,
            retained_file_count_limit: 5,
            file_size_limit_bytes: 1024 * 1024,
            crash_context_lines: 50,
        };
        let logger = WrapperLogger::initialize(opts, false, tmp.path())
            .ok()
            .unwrap();

        // Act
        logger.log_wrapper(
            LogLevel::Information,
            Some("storefront"),
            "Health check passed | Url: http://127.0.0.1:3000/api/health",
            vec![("Url", Value::String("http://127.0.0.1:3000/api/health".to_string()))],
        );
        logger.log_app_output("storefront", "Ready on port 3000", false);
        logger.flush();

        // Assert
        let files: Vec<PathBuf> = fs::read_dir(&log_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 2);

        let wrapper_file = files
            .iter()
            .find(|p| p.file_name().unwrap().to_str().unwrap().starts_with("wrapper-"))
            .unwrap();
        let wrapper_content = fs::read_to_string(wrapper_file).unwrap();
        let parsed: Value = serde_json::from_str(wrapper_content.trim()).unwrap();
        assert_eq!(parsed["AppName"], "storefront");
        assert_eq!(parsed["@l"], "Information");

        let app_file = files
            .iter()
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("storefront-app-")
            })
            .unwrap();
        let app_content = fs::read_to_string(app_file).unwrap();
        assert!(app_content.contains("[INF] Ready on port 3000"));
    }

    #[test]
    fn logger_should_roll_on_file_size_limit_and_enforce_retention() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().join("Logs");
        let opts = ResolvedSerilogOptions {
            log_directory: log_dir.clone(),
            minimum_level: LogLevel::Information,
            retained_file_count_limit: 2,
            file_size_limit_bytes: 120,
            crash_context_lines: 10,
        };
        let logger = WrapperLogger::initialize(opts, false, tmp.path())
            .ok()
            .unwrap();

        // Act: write 4 large lines so it rolls multiple times and prunes beyond 2 files
        for i in 0..4 {
            logger.log_app_output(
                "storefront",
                &format!("Line {i} with padding to exceed 120 bytes ------------------------------"),
                false,
            );
        }
        logger.flush();

        // Assert
        let app_files: Vec<PathBuf> = fs::read_dir(&log_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with("storefront-app-")
            })
            .collect();
        assert_eq!(app_files.len(), 2);
    }
}
