//! Persistent store (`state.json`) for manual application start/stop decisions (Phase 2 §5.3).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Desired state of an application (`Running` or `Stopped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DesiredState::Running => "Running",
            DesiredState::Stopped => "Stopped",
        }
    }
}

/// How the current desired state was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredStateSource {
    AutoStart,
    Manual,
}

/// Persisted entry in `state.json` for a single application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAppState {
    #[serde(rename = "DesiredState")]
    pub desired_state: DesiredState,
    #[serde(rename = "ChangedAt")]
    pub changed_at: DateTime<Utc>,
    #[serde(rename = "ChangedBy")]
    pub changed_by: String,
}

/// Thread-safe store backed by `state.json` using atomic write-then-rename.
#[derive(Clone, Debug)]
pub struct DesiredStateStore {
    path: PathBuf,
    entries: Arc<Mutex<BTreeMap<String, PersistedAppState>>>,
}

impl DesiredStateStore {
    /// Load `state.json` from `path`. If the file does not exist, starts empty.
    /// If the file is corrupt, returns `Ok((store, Some(warning)))` and starts empty so startup never blocks.
    pub fn load(path: PathBuf) -> (Self, Option<String>) {
        if !path.exists() {
            return (
                Self {
                    path,
                    entries: Arc::new(Mutex::new(BTreeMap::new())),
                },
                None,
            );
        }

        match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<BTreeMap<String, PersistedAppState>>(&contents) {
                Ok(map) => (
                    Self {
                        path,
                        entries: Arc::new(Mutex::new(map)),
                    },
                    None,
                ),
                Err(err) => {
                    let warning = format!(
                        "State file '{}' is corrupt and will be ignored (falling back to AutoStart): {err}",
                        path.display()
                    );
                    (
                        Self {
                            path,
                            entries: Arc::new(Mutex::new(BTreeMap::new())),
                        },
                        Some(warning),
                    )
                }
            },
            Err(err) => {
                let warning = format!(
                    "State file '{}' could not be read and will be ignored (falling back to AutoStart): {err}",
                    path.display()
                );
                (
                    Self {
                        path,
                        entries: Arc::new(Mutex::new(BTreeMap::new())),
                    },
                    Some(warning),
                )
            }
        }
    }

    /// Resolve the effective `(DesiredState, DesiredStateSource)` for `app_name` given its `auto_start` config.
    pub fn resolve_desired_state(&self, app_name: &str, auto_start: bool) -> (DesiredState, DesiredStateSource) {
        let guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in guard.iter() {
            if k.eq_ignore_ascii_case(app_name) {
                return (v.desired_state, DesiredStateSource::Manual);
            }
        }
        let default_state = if auto_start {
            DesiredState::Running
        } else {
            DesiredState::Stopped
        };
        (default_state, DesiredStateSource::AutoStart)
    }

    /// Record a manual start/stop decision and persist `state.json` atomically.
    pub fn set_manual_state(
        &self,
        app_name: &str,
        desired_state: DesiredState,
        changed_by: impl Into<String>,
    ) -> Result<(), String> {
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let existing_key = guard
            .keys()
            .find(|k| k.eq_ignore_ascii_case(app_name))
            .cloned()
            .unwrap_or_else(|| app_name.to_string());

        guard.insert(
            existing_key,
            PersistedAppState {
                desired_state,
                changed_at: Utc::now(),
                changed_by: changed_by.into(),
            },
        );

        Self::write_atomically(&self.path, &guard)
    }

    /// Remove entries for applications that no longer exist in configuration (called on reload).
    pub fn retain_apps(&self, valid_app_names: &[String]) -> Result<bool, String> {
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.retain(|k, _| valid_app_names.iter().any(|name| name.eq_ignore_ascii_case(k)));
        let changed = guard.len() != before;
        if changed && (self.path.exists() || !guard.is_empty()) {
            Self::write_atomically(&self.path, &guard)?;
        }
        Ok(changed)
    }

    /// Return the path to `state.json`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write_atomically(
        path: &Path,
        entries: &BTreeMap<String, PersistedAppState>,
    ) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create directory for state file: {e}"))?;
            }
        }

        let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
        let serialized = serde_json::to_string_pretty(entries)
            .map_err(|e| format!("Failed to serialize state.json: {e}"))?;

        {
            let mut file = File::create(&tmp_path)
                .map_err(|e| format!("Failed to create temp state file '{}': {e}", tmp_path.display()))?;
            file.write_all(serialized.as_bytes())
                .map_err(|e| format!("Failed to write temp state file '{}': {e}", tmp_path.display()))?;
            let _ = file.sync_all();
        }

        fs::rename(&tmp_path, path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            format!(
                "Failed to atomically replace state file '{}': {e}",
                path.display()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn desired_state_store_should_follow_auto_start_when_no_manual_entry_exists() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        let (store, warn) = DesiredStateStore::load(tmp.path().join("state.json"));
        assert!(warn.is_none());

        // Act & Assert
        assert_eq!(
            store.resolve_desired_state("storefront", true),
            (DesiredState::Running, DesiredStateSource::AutoStart)
        );
        assert_eq!(
            store.resolve_desired_state("admin", false),
            (DesiredState::Stopped, DesiredStateSource::AutoStart)
        );
    }

    #[test]
    fn desired_state_store_should_persist_and_reload_manual_decisions_atomically() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        let state_path = tmp.path().join("state.json");
        let (store, _) = DesiredStateStore::load(state_path.clone());

        // Act
        store
            .set_manual_state("admin", DesiredState::Running, "cli")
            .unwrap();
        let (reloaded, warn) = DesiredStateStore::load(state_path);

        // Assert
        assert!(warn.is_none());
        assert_eq!(
            reloaded.resolve_desired_state("admin", false),
            (DesiredState::Running, DesiredStateSource::Manual)
        );
    }

    #[test]
    fn desired_state_store_should_ignore_corrupt_file_and_fall_back_to_auto_start() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        let state_path = tmp.path().join("state.json");
        fs::write(&state_path, "{ not valid json !!!").unwrap();

        // Act
        let (store, warn) = DesiredStateStore::load(state_path);

        // Assert
        assert!(warn.is_some());
        assert_eq!(
            store.resolve_desired_state("storefront", true),
            (DesiredState::Running, DesiredStateSource::AutoStart)
        );
    }

    #[test]
    fn retain_apps_should_prune_removed_applications() {
        // Arrange
        let tmp = TempDir::new().unwrap();
        let state_path = tmp.path().join("state.json");
        let (store, _) = DesiredStateStore::load(state_path);
        store
            .set_manual_state("storefront", DesiredState::Stopped, "dashboard")
            .unwrap();
        store
            .set_manual_state("legacy", DesiredState::Running, "cli")
            .unwrap();

        // Act
        let changed = store.retain_apps(&["storefront".to_string()]).unwrap();

        // Assert
        assert!(changed);
        assert_eq!(
            store.resolve_desired_state("legacy", false),
            (DesiredState::Stopped, DesiredStateSource::AutoStart)
        );
        assert_eq!(
            store.resolve_desired_state("storefront", true),
            (DesiredState::Stopped, DesiredStateSource::Manual)
        );
    }
}
