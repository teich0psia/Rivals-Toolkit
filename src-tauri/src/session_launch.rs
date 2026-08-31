//! Fork-only session launch mode.
//!
//! Keeps Toolkit-managed mods and the signature bypass disabled at rest, deploys the
//! logical selection only for launches started by Toolkit, and restores the at-rest
//! state after the shipping process exits.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::detect::InstallInfo;
use crate::mods::{self, BypassKind, ModsStatus};

const CONFIG_FILE_NAME: &str = "session-launch.json";
const DEPLOYMENT_FILE_NAME: &str = "session-deployment.json";
const WATCHDOG_ARG: &str = "--rivals-toolkit-session-watchdog";
const START_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DeploymentRecord {
    game_root: String,
    #[serde(default)]
    deployed_mods: Vec<String>,
    #[serde(default)]
    bypass_loader_deployed: bool,
    #[serde(default)]
    bypass_payload_deployed: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct LegacyDeploymentRecord {
    game_root: String,
    #[serde(default)]
    deployed_mods: Vec<String>,
    #[serde(default)]
    bypass_deployed: bool,
}

impl From<LegacyDeploymentRecord> for DeploymentRecord {
    fn from(legacy: LegacyDeploymentRecord) -> Self {
        Self {
            game_root: legacy.game_root,
            deployed_mods: legacy.deployed_mods,
            // The old session path removed all bypass files before deployment, so a successful
            // legacy bypass deployment necessarily owned both files it then installed.
            bypass_loader_deployed: legacy.bypass_deployed,
            bypass_payload_deployed: legacy.bypass_deployed,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct StoredSessionLaunchConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    selected_mods: BTreeSet<String>,
    #[serde(default)]
    bypass_enabled: bool,
    #[serde(default)]
    deployment: Option<LegacyDeploymentRecord>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct SessionLaunchConfig {
    enabled: bool,
    selected_mods: BTreeSet<String>,
    bypass_enabled: bool,
}

pub(crate) type SessionLaunchState = Mutex<SessionLaunchConfig>;

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("rivals-toolkit"))
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

fn deployment_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(DEPLOYMENT_FILE_NAME))
}

fn replace_file(tmp: &Path, path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| e.to_string())?;
    }

    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

fn write_json_replacing<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    replace_file(&tmp, path)
}

fn write_json_new<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if path.exists() {
        return Err("A session launch is already pending.".to_string());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.to_string())
        }
    }
}

fn parse_config(text: &str) -> Result<(SessionLaunchConfig, Option<DeploymentRecord>), String> {
    let stored: StoredSessionLaunchConfig =
        serde_json::from_str(text).map_err(|e| e.to_string())?;
    let config = SessionLaunchConfig {
        enabled: stored.enabled,
        selected_mods: stored.selected_mods,
        bypass_enabled: stored.bypass_enabled,
    };
    Ok((config, stored.deployment.map(Into::into)))
}

fn load_config() -> (SessionLaunchConfig, Option<DeploymentRecord>) {
    let Some(path) = config_path() else {
        return (SessionLaunchConfig::default(), None);
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return (SessionLaunchConfig::default(), None);
    };
    parse_config(&text).unwrap_or_default()
}

fn save_config(config: &SessionLaunchConfig) -> Result<(), String> {
    let path = config_path().ok_or_else(|| "Could not resolve config directory".to_string())?;
    write_json_replacing(&path, config)
}

fn load_deployment() -> Result<Option<DeploymentRecord>, String> {
    let path = deployment_path().ok_or_else(|| "Could not resolve config directory".to_string())?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
            format!(
                "Could not parse session deployment record {}: {e}",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Could not read session deployment record {}: {e}",
            path.display()
        )),
    }
}

fn save_deployment(record: &DeploymentRecord) -> Result<(), String> {
    let path = deployment_path().ok_or_else(|| "Could not resolve config directory".to_string())?;
    write_json_new(&path, record)
}

fn clear_deployment() -> Result<(), String> {
    let path = deployment_path().ok_or_else(|| "Could not resolve config directory".to_string())?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn update_config(
    state: &SessionLaunchState,
    change: impl FnOnce(&mut SessionLaunchConfig),
) -> Result<(), String> {
    let mut guard = state.lock().map_err(|e| e.to_string())?;
    let mut next = guard.clone();
    change(&mut next);
    save_config(&next)?;
    *guard = next;
    Ok(())
}

pub(crate) fn load_state() -> SessionLaunchState {
    let (config, legacy_deployment) = load_config();
    if let Some(legacy) = legacy_deployment {
        let migrated = match load_deployment() {
            Ok(Some(_)) => true,
            Ok(None) => save_deployment(&legacy).is_ok(),
            Err(_) => false,
        };
        if migrated {
            let _ = save_config(&config);
        }
    }
    Mutex::new(config)
}

pub(crate) fn is_enabled(state: &SessionLaunchState) -> bool {
    state.lock().map(|s| s.enabled).unwrap_or(false)
}

pub(crate) fn selected_mods(state: &SessionLaunchState) -> BTreeSet<String> {
    state
        .lock()
        .map(|s| s.selected_mods.clone())
        .unwrap_or_default()
}

fn display_name(full_name: &str) -> String {
    full_name
        .strip_suffix(".disabled")
        .unwrap_or(full_name)
        .to_string()
}

pub(crate) fn set_mod_selected(
    state: &SessionLaunchState,
    full_or_display_name: &str,
    enabled: bool,
) -> Result<(), String> {
    let name = display_name(full_or_display_name);
    update_config(state, |config| {
        if enabled {
            config.selected_mods.insert(name);
        } else {
            config.selected_mods.remove(&name);
        }
    })
}

pub(crate) fn set_mods_selected(
    state: &SessionLaunchState,
    names: &[String],
    enabled: bool,
) -> Result<(), String> {
    let names: Vec<String> = names.iter().map(|name| display_name(name)).collect();
    update_config(state, |config| {
        for name in names {
            if enabled {
                config.selected_mods.insert(name);
            } else {
                config.selected_mods.remove(&name);
            }
        }
    })
}

pub(crate) fn replace_selected_mods(
    state: &SessionLaunchState,
    names: BTreeSet<String>,
) -> Result<(), String> {
    update_config(state, |config| config.selected_mods = names)
}

pub(crate) fn rename_selected_mod(
    state: &SessionLaunchState,
    old_full_name: &str,
    new_full_name: &str,
) -> Result<(), String> {
    let old_name = display_name(old_full_name);
    let new_name = display_name(new_full_name);
    update_config(state, |config| {
        if config.selected_mods.remove(&old_name) {
            config.selected_mods.insert(new_name);
        }
    })
}

pub(crate) fn set_bypass_selected(state: &SessionLaunchState, enabled: bool) -> Result<(), String> {
    update_config(state, |config| config.bypass_enabled = enabled)
}

pub(crate) fn decorate_status(state: &SessionLaunchState, status: &mut ModsStatus) {
    let Ok(guard) = state.lock() else {
        return;
    };
    if !guard.enabled {
        return;
    }
    for entry in &mut status.mod_entries {
        entry.enabled = guard.selected_mods.contains(&entry.display_name);
    }
    status.sig_bypass_kind = if guard.bypass_enabled {
        BypassKind::Installed
    } else {
        BypassKind::None
    };
}

pub(crate) fn logical_bypass_kind(state: &SessionLaunchState, physical: BypassKind) -> BypassKind {
    let Ok(guard) = state.lock() else {
        return physical;
    };
    if !guard.enabled {
        return physical;
    }
    if guard.bypass_enabled {
        BypassKind::Installed
    } else {
        BypassKind::None
    }
}

fn disable_all_physical_mods(game_root: &str) -> Result<(), String> {
    let status = mods::get_mods_status(game_root, true);
    for entry in status.mod_entries.iter().filter(|e| e.enabled) {
        mods::toggle_mod_enabled(&status.mods_folder_path, &entry.full_name, false)?;
    }
    Ok(())
}

fn deploy_selected_mods(
    game_root: &str,
    selected: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    let status = mods::get_mods_status(game_root, true);
    let mut deployed = Vec::new();
    for entry in &status.mod_entries {
        if selected.contains(&entry.display_name) && !entry.enabled {
            if let Err(e) =
                mods::toggle_mod_enabled(&status.mods_folder_path, &entry.full_name, true)
            {
                let _ = disable_recorded_mods(game_root, &deployed);
                return Err(e);
            }
            deployed.push(entry.display_name.clone());
        }
    }
    Ok(deployed)
}

fn disable_recorded_mods(game_root: &str, deployed: &[String]) -> Result<(), String> {
    if deployed.is_empty() {
        return Ok(());
    }
    let names: BTreeSet<&str> = deployed.iter().map(String::as_str).collect();
    let status = mods::get_mods_status(game_root, true);
    for entry in status
        .mod_entries
        .iter()
        .filter(|e| e.enabled && names.contains(e.display_name.as_str()))
    {
        mods::toggle_mod_enabled(&status.mods_folder_path, &entry.full_name, false)?;
    }
    Ok(())
}

fn restore_session_idle(game_root: &str) -> Result<(), String> {
    mods::remove_session_signature_bypass_at_rest(game_root)?;
    disable_all_physical_mods(game_root)
}

fn restore_persistent_state(
    game_root: &str,
    selected: &BTreeSet<String>,
    bypass_enabled: bool,
) -> Result<(), String> {
    restore_session_idle(game_root)?;
    let deployed = deploy_selected_mods(game_root, selected)?;
    let bypass_result = if bypass_enabled {
        mods::deploy_session_signature_bypass(game_root).map(|_| ())
    } else {
        Ok(())
    };
    if let Err(e) = bypass_result {
        let _ = disable_recorded_mods(game_root, &deployed);
        let _ = mods::remove_session_signature_bypass_at_rest(game_root);
        return Err(e);
    }
    Ok(())
}

fn cleanup_record(record: &DeploymentRecord) -> Result<(), String> {
    disable_recorded_mods(&record.game_root, &record.deployed_mods)?;
    mods::cleanup_session_signature_bypass(
        &record.game_root,
        record.bypass_loader_deployed,
        record.bypass_payload_deployed,
    )
}

fn cleanup_current_deployment() -> Result<(), String> {
    if let Some(record) = load_deployment()? {
        cleanup_record(&record)?;
        clear_deployment()?;
    }
    Ok(())
}

pub(crate) fn recover_or_resume(_state: &SessionLaunchState) {
    let record = match load_deployment() {
        Ok(record) => record,
        Err(e) => {
            eprintln!("rivals-toolkit: {e}");
            return;
        }
    };
    let Some(record) = record else {
        return;
    };
    if crate::game_status::is_game_running() {
        let _ = spawn_watchdog(&record.game_root);
    } else if cleanup_record(&record).is_ok() {
        let _ = clear_deployment();
    }
}

fn set_mode(state: &SessionLaunchState, game_root: &str, enabled: bool) -> Result<String, String> {
    if crate::game_status::is_game_running() {
        return Err("Close Marvel Rivals before changing session launch mode.".to_string());
    }
    if load_deployment()?.is_some() {
        return Err("A session launch is already pending.".to_string());
    }
    if is_enabled(state) == enabled {
        return Ok(if enabled {
            "Session launch is already enabled.".to_string()
        } else {
            "Session launch is already disabled.".to_string()
        });
    }

    if enabled {
        mods::validate_session_signature_bypass(game_root)?;
        let physical = mods::get_mods_status(game_root, true);
        if physical.sig_bypass_kind == BypassKind::Outdated {
            return Err(
                "Session launch cannot manage the legacy signature bypass. Update or remove it first."
                    .to_string(),
            );
        }
        let selected: BTreeSet<String> = physical
            .mod_entries
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.display_name.clone())
            .collect();
        let bypass_enabled = physical.sig_bypass_kind == BypassKind::Installed;

        if let Err(e) = restore_session_idle(game_root) {
            let _ = restore_persistent_state(game_root, &selected, bypass_enabled);
            return Err(e);
        }

        let save_result = update_config(state, |config| {
            config.enabled = true;
            config.selected_mods = selected.clone();
            config.bypass_enabled = bypass_enabled;
        });
        if let Err(e) = save_result {
            let _ = restore_persistent_state(game_root, &selected, bypass_enabled);
            return Err(e);
        }
        Ok("Session launch enabled. Toolkit-managed mods are now inactive at rest.".to_string())
    } else {
        let (selected, bypass_enabled) = {
            let guard = state.lock().map_err(|e| e.to_string())?;
            (guard.selected_mods.clone(), guard.bypass_enabled)
        };

        restore_persistent_state(game_root, &selected, bypass_enabled)?;
        if let Err(e) = update_config(state, |config| config.enabled = false) {
            let _ = restore_session_idle(game_root);
            return Err(e);
        }
        Ok("Session launch disabled. Persistent upstream-style mod state restored.".to_string())
    }
}

#[tauri::command]
pub(crate) fn get_session_launch_enabled(state: State<'_, SessionLaunchState>) -> bool {
    is_enabled(&state)
}

#[tauri::command]
pub(crate) fn set_session_launch_enabled(
    state: State<'_, SessionLaunchState>,
    game_root: String,
    enabled: bool,
) -> Result<String, String> {
    set_mode(&state, &game_root, enabled)
}

fn prepare_deployment(state: &SessionLaunchState, game_root: &str) -> Result<(), String> {
    if load_deployment()?.is_some() {
        return Err("A session launch is already pending.".to_string());
    }

    restore_session_idle(game_root)?;
    let (selected, bypass_enabled) = {
        let guard = state.lock().map_err(|e| e.to_string())?;
        (guard.selected_mods.clone(), guard.bypass_enabled)
    };

    let deployed_mods = deploy_selected_mods(game_root, &selected)?;
    let (bypass_loader_deployed, bypass_payload_deployed) = if bypass_enabled {
        match mods::deploy_session_signature_bypass(game_root) {
            Ok(deployment) => deployment,
            Err(e) => {
                let _ = disable_recorded_mods(game_root, &deployed_mods);
                return Err(e);
            }
        }
    } else {
        (false, false)
    };

    let record = DeploymentRecord {
        game_root: game_root.to_string(),
        deployed_mods,
        bypass_loader_deployed,
        bypass_payload_deployed,
    };
    if let Err(e) = save_deployment(&record) {
        let _ = cleanup_record(&record);
        return Err(e);
    }
    Ok(())
}

fn spawn_watchdog(game_root: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg(WATCHDOG_ARG)
        .arg(game_root)
        .spawn()
        .map_err(|e| format!("Failed to start session cleanup watchdog: {e}"))?;
    Ok(())
}

pub(crate) fn launch_game(app: &AppHandle, install_info: InstallInfo) -> Result<(), String> {
    let state = app.state::<SessionLaunchState>();
    if !is_enabled(&state) {
        return install_info.launch_game();
    }
    if crate::game_status::is_game_running() {
        return Err("Marvel Rivals is already running.".to_string());
    }

    prepare_deployment(&state, &install_info.path)?;
    if let Err(e) = install_info.launch_game() {
        let _ = cleanup_current_deployment();
        return Err(e);
    }
    if let Err(e) = spawn_watchdog(&install_info.path) {
        let _ = cleanup_current_deployment();
        return Err(e);
    }
    Ok(())
}

fn watchdog_main(game_root: &str) {
    let start = Instant::now();
    while !crate::game_status::is_game_running() && start.elapsed() < START_TIMEOUT {
        thread::sleep(POLL_INTERVAL);
    }
    if crate::game_status::is_game_running() {
        while crate::game_status::is_game_running() {
            thread::sleep(POLL_INTERVAL);
        }
    }

    let record = match load_deployment() {
        Ok(record) => record,
        Err(e) => {
            eprintln!("rivals-toolkit watchdog: {e}");
            return;
        }
    };
    if let Some(record) = record.filter(|r| r.game_root == game_root) {
        if cleanup_record(&record).is_ok() {
            let _ = clear_deployment();
        }
    }
}

pub(crate) fn run_watchdog_from_args() -> bool {
    let mut args = std::env::args();
    let _ = args.next();
    if args.next().as_deref() != Some(WATCHDOG_ARG) {
        return false;
    }
    let Some(game_root) = args.next() else {
        return true;
    };
    watchdog_main(&game_root);
    true
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn scratch_path(file_name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "rivals-session-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
            .join(file_name)
    }

    #[test]
    fn display_name_strips_only_disabled_suffix() {
        assert_eq!(display_name("Foo.pak.disabled"), "Foo.pak");
        assert_eq!(display_name("Foo.pak"), "Foo.pak");
        assert_eq!(display_name("sub/Foo.pak.disabled"), "sub/Foo.pak");
    }

    #[test]
    fn legacy_deployment_is_migrated_out_of_config() {
        let json = r#"{
            "enabled": true,
            "selected_mods": ["Foo.pak"],
            "bypass_enabled": true,
            "deployment": {
                "game_root": "C:/Game",
                "deployed_mods": ["Foo.pak"],
                "bypass_deployed": true
            }
        }"#;
        let (config, deployment) = parse_config(json).expect("parse legacy config");
        let deployment = deployment.expect("legacy deployment");

        assert!(config.enabled);
        assert!(config.selected_mods.contains("Foo.pak"));
        assert!(config.bypass_enabled);
        assert_eq!(deployment.game_root, "C:/Game");
        assert!(deployment.bypass_loader_deployed);
        assert!(deployment.bypass_payload_deployed);
        assert!(
            !serde_json::to_string(&config)
                .expect("serialize config")
                .contains("deployment")
        );
    }

    #[test]
    fn replacing_json_file_supports_repeated_saves() {
        let path = scratch_path("state.json");
        write_json_replacing(&path, &serde_json::json!({ "value": 1 })).expect("first write");
        write_json_replacing(&path, &serde_json::json!({ "value": 2 })).expect("second write");

        let text = std::fs::read_to_string(&path).expect("read state");
        assert!(text.contains('2'));
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn deployment_file_creation_rejects_an_existing_record() {
        let path = scratch_path("deployment.json");
        let first = DeploymentRecord {
            game_root: "A".to_string(),
            ..DeploymentRecord::default()
        };
        let second = DeploymentRecord {
            game_root: "B".to_string(),
            ..DeploymentRecord::default()
        };

        write_json_new(&path, &first).expect("first deployment");
        assert!(write_json_new(&path, &second).is_err());
        let text = std::fs::read_to_string(&path).expect("read deployment");
        assert!(text.contains("\"A\""));
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }
}
