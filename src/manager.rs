//! Multi-application coordinator (`ApplicationManager`) owning all `ApplicationSupervisor` instances,
//! `DesiredStateStore`, parallel startup/shutdown, and live configuration reload.

use crate::config::{
    diff_configurations, load_and_validate_from_dir, ConfigReloadDiff, LogLevel,
    ValidatedWrapperConfig,
};
use crate::logging::WrapperLogger;
use crate::state_store::{DesiredState, DesiredStateSource, DesiredStateStore};
use crate::supervisor::{ApplicationSnapshot, ApplicationState, ApplicationSupervisor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex as AsyncMutex;

/// Error returned when an application control operation cannot be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerError {
    UnknownApplication(String),
    ValidationFailed(Vec<String>),
    InternalError(String),
}

/// Service-wide health response for `GET /api/health` (Phase 2 §7.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceHealthReport {
    pub status: String,
    pub healthy: bool,
    pub applications: Vec<AppHealthSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppHealthSummary {
    pub name: String,
    pub state: ApplicationState,
    pub desired_state: DesiredState,
    pub healthy: bool,
    pub last_error: Option<String>,
}

/// Manages the lifecycle of all configured applications.
#[derive(Clone)]
pub struct ApplicationManager {
    active_config: Arc<RwLock<ValidatedWrapperConfig>>,
    supervisors: Arc<RwLock<Vec<Arc<ApplicationSupervisor>>>>,
    state_store: DesiredStateStore,
    logger: WrapperLogger,
    environment: Option<String>,
    reload_lock: Arc<AsyncMutex<()>>,
}

impl ApplicationManager {
    pub fn new(
        config: ValidatedWrapperConfig,
        state_store: DesiredStateStore,
        logger: WrapperLogger,
        environment: Option<String>,
    ) -> Self {
        let mut supervisors = Vec::with_capacity(config.applications.len());
        for app_cfg in &config.applications {
            let (desired, source) =
                state_store.resolve_desired_state(&app_cfg.name, app_cfg.auto_start);
            let sup = Arc::new(ApplicationSupervisor::new(
                app_cfg.clone(),
                desired,
                source,
                logger.clone(),
            ));
            supervisors.push(sup);
        }

        Self {
            active_config: Arc::new(RwLock::new(config)),
            supervisors: Arc::new(RwLock::new(supervisors)),
            state_store,
            logger,
            environment,
            reload_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Start all applications whose desired state is `Running` in parallel (Phase 2 §6).
    pub async fn start_all(&self) {
        let sups: Vec<Arc<ApplicationSupervisor>> = {
            let guard = self.supervisors.read().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };

        let mut handles = Vec::new();
        for sup in sups {
            let snap = sup.snapshot();
            if snap.desired_state == DesiredState::Running {
                handles.push(tokio::spawn(async move {
                    sup.start().await;
                }));
            } else {
                self.logger.log_wrapper(
                    LogLevel::Information,
                    Some(sup.name()),
                    &format!(
                        "Skipping startup (DesiredState is Stopped) | AppName: {} | Source: {:?}",
                        sup.name(),
                        snap.desired_state_source
                    ),
                    vec![(
                        "DesiredStateSource",
                        json!(format!("{:?}", snap.desired_state_source)),
                    )],
                );
            }
        }

        for h in handles {
            let _ = h.await;
        }
    }

    /// Stop all running applications in parallel when the service shuts down (Phase 2 §6).
    pub async fn stop_all(&self) {
        let sups: Vec<Arc<ApplicationSupervisor>> = {
            let guard = self.supervisors.read().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };

        let mut handles = Vec::new();
        for sup in sups {
            handles.push(tokio::spawn(async move {
                sup.stop().await;
            }));
        }

        for h in handles {
            let _ = h.await;
        }
    }

    /// Return snapshots for all applications in configuration order.
    pub fn list_snapshots(&self) -> Vec<ApplicationSnapshot> {
        let guard = self.supervisors.read().unwrap_or_else(|e| e.into_inner());
        guard.iter().map(|s| s.snapshot()).collect()
    }

    /// Return snapshot for a single application by case-insensitive name.
    pub fn get_snapshot(&self, name: &str) -> Result<ApplicationSnapshot, ManagerError> {
        let sup = self.find_supervisor(name)?;
        Ok(sup.snapshot())
    }

    /// Return recent stdout/stderr lines for an application.
    pub fn get_output(&self, name: &str, lines: usize) -> Result<Vec<String>, ManagerError> {
        let sup = self.find_supervisor(name)?;
        Ok(sup.recent_output(lines))
    }

    /// Start an application manually, persist `DesiredState = Running` to `state.json`, and log the action.
    pub async fn start_application(
        &self,
        name: &str,
        source: &str,
        user: Option<&str>,
    ) -> Result<ApplicationSnapshot, ManagerError> {
        let sup = self.find_supervisor(name)?;
        let actor = format_actor(source, user);

        self.logger.log_wrapper(
            LogLevel::Information,
            Some(sup.name()),
            &format!(
                "Control command: start | AppName: {} | Source: {source} | User: {}",
                sup.name(),
                user.unwrap_or("local")
            ),
            vec![
                ("Command", Value::String("start".to_string())),
                ("Source", Value::String(source.to_string())),
                ("User", Value::String(user.unwrap_or("local").to_string())),
            ],
        );

        sup.set_desired_state(DesiredState::Running, DesiredStateSource::Manual);
        if let Err(e) = self
            .state_store
            .set_manual_state(sup.name(), DesiredState::Running, &actor)
        {
            self.logger.log_wrapper(
                LogLevel::Warning,
                Some(sup.name()),
                &format!("Failed to persist state.json: {e}"),
                vec![("Error", Value::String(e))],
            );
        }

        Ok(sup.start().await)
    }

    /// Stop an application manually, persist `DesiredState = Stopped` to `state.json`, and log the action.
    /// Spawns the graceful stop task and returns immediately once accepted.
    pub async fn stop_application(
        &self,
        name: &str,
        source: &str,
        user: Option<&str>,
    ) -> Result<ApplicationSnapshot, ManagerError> {
        let sup = self.find_supervisor(name)?;
        let actor = format_actor(source, user);

        self.logger.log_wrapper(
            LogLevel::Information,
            Some(sup.name()),
            &format!(
                "Control command: stop | AppName: {} | Source: {source} | User: {}",
                sup.name(),
                user.unwrap_or("local")
            ),
            vec![
                ("Command", Value::String("stop".to_string())),
                ("Source", Value::String(source.to_string())),
                ("User", Value::String(user.unwrap_or("local").to_string())),
            ],
        );

        sup.set_desired_state(DesiredState::Stopped, DesiredStateSource::Manual);
        if let Err(e) = self
            .state_store
            .set_manual_state(sup.name(), DesiredState::Stopped, &actor)
        {
            self.logger.log_wrapper(
                LogLevel::Warning,
                Some(sup.name()),
                &format!("Failed to persist state.json: {e}"),
                vec![("Error", Value::String(e))],
            );
        }

        let sup_clone = sup.clone();
        tokio::spawn(async move {
            sup_clone.stop().await;
        });

        // Give the spawned task a brief yield so fast transitions or "Stopping" state are reflected
        tokio::task::yield_now().await;
        Ok(sup.snapshot())
    }

    /// Restart an application manually, persist `DesiredState = Running` to `state.json`, and log the action.
    pub async fn restart_application(
        &self,
        name: &str,
        source: &str,
        user: Option<&str>,
    ) -> Result<ApplicationSnapshot, ManagerError> {
        let sup = self.find_supervisor(name)?;
        let actor = format_actor(source, user);

        self.logger.log_wrapper(
            LogLevel::Information,
            Some(sup.name()),
            &format!(
                "Control command: restart | AppName: {} | Source: {source} | User: {}",
                sup.name(),
                user.unwrap_or("local")
            ),
            vec![
                ("Command", Value::String("restart".to_string())),
                ("Source", Value::String(source.to_string())),
                ("User", Value::String(user.unwrap_or("local").to_string())),
            ],
        );

        sup.set_desired_state(DesiredState::Running, DesiredStateSource::Manual);
        if let Err(e) = self
            .state_store
            .set_manual_state(sup.name(), DesiredState::Running, &actor)
        {
            self.logger.log_wrapper(
                LogLevel::Warning,
                Some(sup.name()),
                &format!("Failed to persist state.json: {e}"),
                vec![("Error", Value::String(e))],
            );
        }

        let sup_clone = sup.clone();
        tokio::spawn(async move {
            sup_clone.restart().await;
        });

        tokio::task::yield_now().await;
        Ok(sup.snapshot())
    }

    /// Re-read `appsettings.json`, validate it, and apply added, removed, and changed applications
    /// without touching unchanged applications (Phase 2 §5.4).
    pub async fn reload_configuration(
        &self,
        source: &str,
        user: Option<&str>,
    ) -> Result<ConfigReloadDiff, ManagerError> {
        let _guard = self.reload_lock.lock().await;

        let base_dir = {
            let cfg = self
                .active_config
                .read()
                .unwrap_or_else(|e| e.into_inner());
            cfg.base_dir.clone()
        };

        let new_config = match load_and_validate_from_dir(&base_dir, self.environment.as_deref()) {
            Ok(c) => c,
            Err(errors) => {
                self.logger.log_wrapper(
                    LogLevel::Warning,
                    None,
                    &format!(
                        "Configuration reload rejected due to validation errors | Source: {source} | Errors: {}",
                        errors.join("; ")
                    ),
                    vec![
                        ("Source", Value::String(source.to_string())),
                        ("Errors", json!(errors)),
                    ],
                );
                return Err(ManagerError::ValidationFailed(errors));
            }
        };

        let diff = {
            let old_cfg = self
                .active_config
                .read()
                .unwrap_or_else(|e| e.into_inner());
            diff_configurations(&old_cfg, &new_config)
        };

        // 1. Stop and remove deleted applications
        for removed_name in &diff.removed {
            let removed_sup = {
                let mut sups = self.supervisors.write().unwrap_or_else(|e| e.into_inner());
                if let Some(pos) = sups
                    .iter()
                    .position(|s| s.name().eq_ignore_ascii_case(removed_name))
                {
                    Some(sups.remove(pos))
                } else {
                    None
                }
            };
            if let Some(sup) = removed_sup {
                sup.stop().await;
            }
        }

        // 2. Prune removed apps from state.json
        let valid_names: Vec<String> = new_config
            .applications
            .iter()
            .map(|a| a.name.clone())
            .collect();
        let _ = self.state_store.retain_apps(&valid_names);

        // 3. Apply changed applications
        for changed_name in &diff.changed {
            if let Some(new_app_cfg) = new_config
                .applications
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(changed_name))
            {
                if let Ok(sup) = self.find_supervisor(changed_name) {
                    let (desired, desired_source) = self
                        .state_store
                        .resolve_desired_state(&new_app_cfg.name, new_app_cfg.auto_start);
                    sup.set_desired_state(desired, desired_source);
                    sup.apply_reloaded_config(new_app_cfg.clone()).await;
                }
            }
        }

        // 4. Create and optionally start added applications
        for added_name in &diff.added {
            if let Some(new_app_cfg) = new_config
                .applications
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(added_name))
            {
                let (desired, desired_source) = self
                    .state_store
                    .resolve_desired_state(&new_app_cfg.name, new_app_cfg.auto_start);
                let sup = Arc::new(ApplicationSupervisor::new(
                    new_app_cfg.clone(),
                    desired,
                    desired_source,
                    self.logger.clone(),
                ));
                {
                    let mut sups = self.supervisors.write().unwrap_or_else(|e| e.into_inner());
                    sups.push(sup.clone());
                }
                if desired == DesiredState::Running {
                    sup.start().await;
                }
            }
        }

        // 5. Update stored active config
        {
            let mut cfg_guard = self
                .active_config
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *cfg_guard = new_config;
        }

        self.logger.log_wrapper(
            LogLevel::Information,
            None,
            &format!(
                "Configuration reloaded | Summary: {} | Source: {source} | User: {}",
                diff.summary_string(),
                user.unwrap_or("local")
            ),
            vec![
                ("Added", json!(diff.added)),
                ("Removed", json!(diff.removed)),
                ("Changed", json!(diff.changed)),
                ("UnchangedCount", json!(diff.unchanged.len())),
                (
                    "RequiresServiceRestart",
                    json!(diff.requires_service_restart),
                ),
                ("Source", Value::String(source.to_string())),
                ("User", Value::String(user.unwrap_or("local").to_string())),
            ],
        );

        Ok(diff)
    }

    /// Evaluate service-level health (`GET /api/health`):
    /// Healthy (`200 OK`) when every application whose `DesiredState` is `Running` is `Running`,
    /// otherwise `503 Service Unavailable`.
    pub fn evaluate_service_health(&self) -> ServiceHealthReport {
        let snapshots = self.list_snapshots();
        let mut all_healthy = true;
        let mut app_summaries = Vec::with_capacity(snapshots.len());

        for snap in snapshots {
            let app_healthy = match snap.desired_state {
                DesiredState::Running => snap.state == ApplicationState::Running,
                DesiredState::Stopped => true,
            };
            if !app_healthy {
                all_healthy = false;
            }
            app_summaries.push(AppHealthSummary {
                name: snap.name,
                state: snap.state,
                desired_state: snap.desired_state,
                healthy: app_healthy,
                last_error: snap.last_error,
            });
        }

        ServiceHealthReport {
            status: if all_healthy {
                "Healthy".to_string()
            } else {
                "Unhealthy".to_string()
            },
            healthy: all_healthy,
            applications: app_summaries,
        }
    }

    fn find_supervisor(&self, name: &str) -> Result<Arc<ApplicationSupervisor>, ManagerError> {
        let guard = self.supervisors.read().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .find(|s| s.name().eq_ignore_ascii_case(name))
            .cloned()
            .ok_or_else(|| ManagerError::UnknownApplication(name.to_string()))
    }
}

fn format_actor(source: &str, user: Option<&str>) -> String {
    match user.filter(|u| !u.trim().is_empty()) {
        Some(u) if !u.eq_ignore_ascii_case(source) => format!("{source} ({u})"),
        _ => source.to_string(),
    }
}
