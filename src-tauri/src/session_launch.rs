//! Fork-only session launch mode.
//!
//! Keeps Toolkit-managed mods and the signature bypass disabled at rest, deploys the
//! logical selection only for launches started by Toolkit, and restores the at-rest
//! state after the shipping process exits.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::detect::InstallInfo;
use crate::mods::{self, BypassKind, ModsStatus};

const FILE_NAME: &str = "session-launch.json";
const WATCHDOG_ARG: &str = "--rivals-toolkit-session-watchdog";
const START_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DeploymentRecord {
    game_root: String,
    #[serde(default)]
    deployed_mods: Vec<String>,
    #[serde(default)]
    bypass_deployed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct SessionLaunchConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    selected_mods: BTreeSet<String>,
    #[serde(default)]
    bypass_enabled: bool,
    #[serde(default)]
    deployment: Option<DeploymentRecord>,
}

pub(crate) type SessionLaunchState = Mutex<SessionLaunchConfig>;

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("rivals-toolkit").join(FILE_NAME))
}

fn load_config() -> SessionLaunchConfig {
    let Some(path) = config_path() else {
        return SessionLaunchConfig::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return SessionLaunchConfig::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_config(config: &SessionLaunchConfig) -> Result<(), String> {
    let path = config_path().ok_or_else(|| "Could not resolve config directory".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
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
    Mutex::new(load_config())
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

fn remove_physical_bypass_if_present(game_root: &str) -> Result<(), String> {
    if mods::signature_bypass_kind(game_root) != BypassKind::None {
        mods::remove_signature_bypass(game_root)?;
    }
    Ok(())
}

fn restore_session_idle(game_root: &str) -> Result<(), String> {
    disable_all_physical_mods(game_root)?;
    remove_physical_bypass_if_present(game_root)
}

fn restore_persistent_state(
    game_root: &str,
    selected: &BTreeSet<String>,
    bypass_enabled: bool,
) -> Result<(), String> {
    disable_all_physical_mods(game_root)?;
    remove_physical_bypass_if_present(game_root)?;
    let deployed = deploy_selected_mods(game_root, selected)?;
    let bypass_result = if bypass_enabled {
        mods::install_signature_bypass(game_root).map(|_| ())
    } else {
        Ok(())
    };
    if let Err(e) = bypass_result {
        let _ = disable_recorded_mods(game_root, &deployed);
        let _ = remove_physical_bypass_if_present(game_root);
        return Err(e);
    }
    Ok(())
}

fn cleanup_record(record: &DeploymentRecord) -> Result<(), String> {
    disable_recorded_mods(&record.game_root, &record.deployed_mods)?;
    if record.bypass_deployed {
        remove_physical_bypass_if_present(&record.game_root)?;
    }
    Ok(())
}

fn clear_deployment(state: &SessionLaunchState) -> Result<(), String> {
    update_config(state, |config| config.deployment = None)
}

fn cleanup_current_deployment(state: &SessionLaunchState) -> Result<(), String> {
    let record = state.lock().map_err(|e| e.to_string())?.deployment.clone();
    if let Some(record) = record {
        cleanup_record(&record)?;
        clear_deployment(state)?;
    }
    Ok(())
}

pub(crate) fn recover_or_resume(state: &SessionLaunchState) {
    let record = state.lock().ok().and_then(|s| s.deployment.clone());
    let Some(record) = record else {
        return;
    };
    if crate::game_status::is_game_running() {
        let _ = spawn_watchdog(&record.game_root);
    } else if cleanup_record(&record).is_ok() {
        let _ = clear_deployment(state);
    }
}

fn set_mode(state: &SessionLaunchState, game_root: &str, enabled: bool) -> Result<String, String> {
    if crate::game_status::is_game_running() {
        return Err("Close Marvel Rivals before changing session launch mode.".to_string());
    }
    if is_enabled(state) == enabled {
        return Ok(if enabled {
            "Session launch is already enabled.".to_string()
        } else {
            "Session launch is already disabled.".to_string()
        });
    }

    if enabled {
        let physical = mods::get_mods_status(game_root, true);
        let selected: BTreeSet<String> = physical
            .mod_entries
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.display_name.clone())
            .collect();
        let bypass_enabled = physical.sig_bypass_kind != BypassKind::None;

        if let Err(e) = restore_session_idle(game_root) {
            let _ = restore_persistent_state(game_root, &selected, bypass_enabled);
            return Err(e);
        }

        let save_result = update_config(state, |config| {
            config.enabled = true;
            config.selected_mods = selected.clone();
            config.bypass_enabled = bypass_enabled;
            config.deployment = None;
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
        if let Err(e) = update_config(state, |config| {
            config.enabled = false;
            config.deployment = None;
        }) {
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
    if state
        .lock()
        .map_err(|e| e.to_string())?
        .deployment
        .is_some()
    {
        return Err("A session launch is already pending.".to_string());
    }

    restore_session_idle(game_root)?;
    let (selected, bypass_enabled) = {
        let guard = state.lock().map_err(|e| e.to_string())?;
        (guard.selected_mods.clone(), guard.bypass_enabled)
    };

    let deployed_mods = deploy_selected_mods(game_root, &selected)?;
    let mut bypass_deployed = false;
    if bypass_enabled {
        if let Err(e) = mods::install_signature_bypass(game_root) {
            let _ = disable_recorded_mods(game_root, &deployed_mods);
            return Err(e);
        }
        bypass_deployed = true;
    }

    let record = DeploymentRecord {
        game_root: game_root.to_string(),
        deployed_mods,
        bypass_deployed,
    };
    if let Err(e) = update_config(state, |config| config.deployment = Some(record.clone())) {
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
        let _ = cleanup_current_deployment(&state);
        return Err(e);
    }
    if let Err(e) = spawn_watchdog(&install_info.path) {
        let _ = cleanup_current_deployment(&state);
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

    let state = load_state();
    let record = state.lock().ok().and_then(|s| s.deployment.clone());
    if let Some(record) = record.filter(|r| r.game_root == game_root) {
        if cleanup_record(&record).is_ok() {
            let _ = clear_deployment(&state);
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
mod tests {
    use super::display_name;

    #[test]
    fn display_name_strips_only_disabled_suffix() {
        assert_eq!(display_name("Foo.pak.disabled"), "Foo.pak");
        assert_eq!(display_name("Foo.pak"), "Foo.pak");
        assert_eq!(display_name("sub/Foo.pak.disabled"), "sub/Foo.pak");
    }
}
