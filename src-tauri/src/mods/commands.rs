//! Tauri commands for mod install, toggle, export, delete, and status queries.

use std::path::Path;

use tauri::{AppHandle, Manager, State};

use crate::game_status;
use crate::mods;
use crate::mods::hero_cache::HeroCacheState;
use crate::mods::heroes::enrich_status_with_heroes;
use crate::mods::{BulkOpResult, ConflictReport, InstallResult, ModsStatus};
use crate::session_launch::{self, SessionLaunchState};
use crate::settings::{SettingsState, recursive_mod_scan};

fn disable_new_install(mods_folder: &str, file_name: &str) -> Result<(), String> {
    if Path::new(mods_folder).join(file_name).exists() {
        mods::toggle_mod_enabled(mods_folder, file_name, false)?;
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn get_mods_status(
    app: AppHandle,
    game_root: String,
) -> Result<ModsStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<SettingsState>();
        let session = app.state::<SessionLaunchState>();
        let cache = app.state::<HeroCacheState>();
        let recursive = recursive_mod_scan(&state);
        let mut status = mods::get_mods_status(&game_root, recursive);
        session_launch::decorate_status(&session, &mut status);
        enrich_status_with_heroes(&cache, &mut status);
        status
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) async fn check_mod_conflicts(
    state: State<'_, SettingsState>,
    session: State<'_, SessionLaunchState>,
    game_root: String,
) -> Result<ConflictReport, String> {
    if session_launch::is_enabled(&session) {
        return Err(
            "Conflict checking is unavailable while Session launch is enabled.".to_string(),
        );
    }
    let recursive = recursive_mod_scan(&state);
    tauri::async_runtime::spawn_blocking(move || mods::check_conflicts(&game_root, recursive))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) fn install_signature_bypass(
    session: State<'_, SessionLaunchState>,
    game_root: String,
) -> Result<String, String> {
    if crate::game_status::should_block_for_game() {
        return Err(crate::game_status::game_running_error());
    }
    if session_launch::is_enabled(&session) {
        session_launch::set_bypass_selected(&session, true)?;
        return Ok("Signature bypass will be deployed for Toolkit launches.".to_string());
    }
    mods::install_signature_bypass(&game_root)
}

#[tauri::command]
pub(crate) fn remove_signature_bypass(
    session: State<'_, SessionLaunchState>,
    game_root: String,
) -> Result<String, String> {
    if crate::game_status::should_block_for_game() {
        return Err(crate::game_status::game_running_error());
    }
    if session_launch::is_enabled(&session) {
        session_launch::set_bypass_selected(&session, false)?;
        return Ok("Signature bypass disabled for Toolkit launches.".to_string());
    }
    mods::remove_signature_bypass(&game_root)
}

#[tauri::command]
pub(crate) fn is_signature_bypass_installed(
    session: State<'_, SessionLaunchState>,
    game_root: String,
) -> bool {
    session_launch::logical_bypass_kind(&session, mods::signature_bypass_kind(&game_root))
        != mods::BypassKind::None
}

#[tauri::command]
pub(crate) fn get_signature_bypass_kind(
    session: State<'_, SessionLaunchState>,
    game_root: String,
) -> mods::BypassKind {
    session_launch::logical_bypass_kind(&session, mods::signature_bypass_kind(&game_root))
}

#[tauri::command]
pub(crate) fn open_mods_folder(game_root: String) -> Result<(), String> {
    mods::open_mods_folder(&game_root)
}

#[tauri::command]
pub(crate) fn toggle_mod_enabled(
    session: State<'_, SessionLaunchState>,
    mods_folder: String,
    full_name: String,
    enabled: bool,
) -> Result<(), String> {
    if game_status::should_block_for_game() {
        return Err(game_status::game_running_error());
    }
    if session_launch::is_enabled(&session) {
        return session_launch::set_mod_selected(&session, &full_name, enabled);
    }
    mods::toggle_mod_enabled(&mods_folder, &full_name, enabled)
}

#[tauri::command]
pub(crate) async fn export_mods_archive(
    state: State<'_, SettingsState>,
    mods_folder: String,
    dest_path: String,
) -> Result<String, String> {
    let recursive = recursive_mod_scan(&state);
    tauri::async_runtime::spawn_blocking(move || {
        mods::export_mods_archive(&mods_folder, &dest_path, recursive)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) fn rename_mod(
    session: State<'_, SessionLaunchState>,
    mods_folder: String,
    full_name: String,
    new_base: String,
) -> Result<String, String> {
    if game_status::should_block_for_game() {
        return Err(game_status::game_running_error());
    }
    let new_full_name = mods::rename_mod(&mods_folder, &full_name, &new_base)?;
    if session_launch::is_enabled(&session) {
        session_launch::rename_selected_mod(&session, &full_name, &new_full_name)?;
    }
    Ok(new_full_name)
}

#[tauri::command]
pub(crate) fn delete_mod(
    session: State<'_, SessionLaunchState>,
    mods_folder: String,
    full_name: String,
) -> Result<(), String> {
    if game_status::should_block_for_game() {
        return Err(game_status::game_running_error());
    }
    mods::delete_mod(&mods_folder, &full_name)?;
    if session_launch::is_enabled(&session) {
        session_launch::set_mod_selected(&session, &full_name, false)?;
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn toggle_mods_enabled(
    session: State<'_, SessionLaunchState>,
    mods_folder: String,
    full_names: Vec<String>,
    enabled: bool,
) -> Result<BulkOpResult, String> {
    if game_status::should_block_for_game() {
        return Err(game_status::game_running_error());
    }
    if session_launch::is_enabled(&session) {
        session_launch::set_mods_selected(&session, &full_names, enabled)?;
        return Ok(BulkOpResult {
            successes: full_names.len() as u32,
            failures: Vec::new(),
        });
    }
    Ok(mods::toggle_mods_enabled(
        &mods_folder,
        &full_names,
        enabled,
    ))
}

#[tauri::command]
pub(crate) fn delete_mods(
    session: State<'_, SessionLaunchState>,
    mods_folder: String,
    full_names: Vec<String>,
) -> Result<BulkOpResult, String> {
    if game_status::should_block_for_game() {
        return Err(game_status::game_running_error());
    }
    let result = mods::delete_mods(&mods_folder, &full_names);
    if session_launch::is_enabled(&session) {
        for name in &full_names {
            if !result
                .failures
                .iter()
                .any(|failure| failure.full_name == *name)
            {
                session_launch::set_mod_selected(&session, name, false)?;
            }
        }
    }
    Ok(result)
}

#[tauri::command]
pub(crate) fn install_mod(
    session: State<'_, SessionLaunchState>,
    mods_folder: String,
    source_path: String,
) -> Result<InstallResult, String> {
    if game_status::should_block_for_game() {
        return Err(game_status::game_running_error());
    }
    let result = mods::install_mod(&mods_folder, &source_path)?;
    if session_launch::is_enabled(&session) {
        disable_new_install(&mods_folder, &result.file_name).map_err(|e| {
            format!("Mod installed but could not be returned to the Session launch idle state: {e}")
        })?;
        session_launch::set_mod_selected(&session, &result.file_name, true)?;
    }
    Ok(result)
}

#[tauri::command]
pub(crate) async fn install_from_archive(
    session: State<'_, SessionLaunchState>,
    mods_folder: String,
    archive_path: String,
) -> Result<Vec<InstallResult>, String> {
    if game_status::should_block_for_game() {
        return Err(game_status::game_running_error());
    }
    let results = tauri::async_runtime::spawn_blocking({
        let mods_folder = mods_folder.clone();
        move || mods::install_from_archive(&mods_folder, &archive_path)
    })
    .await
    .map_err(|e| e.to_string())??;

    if session_launch::is_enabled(&session) {
        for result in &results {
            if let Err(e) = disable_new_install(&mods_folder, &result.file_name) {
                for installed in &results {
                    let _ = disable_new_install(&mods_folder, &installed.file_name);
                }
                return Err(format!(
                    "Archive installed but could not be returned to the Session launch idle state: {e}"
                ));
            }
        }
        let names: Vec<String> = results.iter().map(|result| result.file_name.clone()).collect();
        session_launch::set_mods_selected(&session, &names, true)?;
    }
    Ok(results)
}
