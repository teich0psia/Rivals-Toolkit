//! Fork-only logical profile behavior for session launch mode.
//!
//! Upstream profile code continues to own the persistent-deployment path. This module mirrors the
//! small amount of profile bookkeeping needed when selected mods are logical rather than physically
//! enabled on disk.

use std::collections::{BTreeSet, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::mods::profiles::{ProfileApplyResult, ProfileDiff};
use crate::mods::status::get_mods_status;
use crate::settings::{ModProfile, Settings};

use super::{SessionLaunchState, replace_selected_mods, selected_mods};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn save(
    settings: &Mutex<Settings>,
    session: &SessionLaunchState,
    name: &str,
) -> Result<ModProfile, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Profile name cannot be empty".to_string());
    }

    let now = now_secs();
    let profile = ModProfile {
        name: trimmed.to_string(),
        enabled_mods: selected_mods(session).into_iter().collect(),
        created_at: now,
        modified_at: now,
    };

    let mut guard = settings.lock().map_err(|e| e.to_string())?;
    if guard.mod_profiles.iter().any(|p| p.name == trimmed) {
        return Err(format!("Profile \"{trimmed}\" already exists"));
    }
    guard.mod_profiles.push(profile.clone());
    guard.save()?;
    Ok(profile)
}

pub(crate) fn overwrite(
    settings: &Mutex<Settings>,
    session: &SessionLaunchState,
    name: &str,
) -> Result<ModProfile, String> {
    let enabled_mods: Vec<String> = selected_mods(session).into_iter().collect();
    let mut guard = settings.lock().map_err(|e| e.to_string())?;
    let profile = guard
        .mod_profiles
        .iter_mut()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("Profile \"{name}\" not found"))?;
    profile.enabled_mods = enabled_mods;
    profile.modified_at = now_secs();
    let result = profile.clone();
    guard.save()?;
    Ok(result)
}

pub(crate) fn preview(
    settings: &Mutex<Settings>,
    session: &SessionLaunchState,
    name: &str,
    game_root: &str,
    recursive: bool,
) -> Result<ProfileDiff, String> {
    let profile_mods: HashSet<String> = {
        let guard = settings.lock().map_err(|e| e.to_string())?;
        let profile = guard
            .mod_profiles
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("Profile \"{name}\" not found"))?;
        profile.enabled_mods.iter().cloned().collect()
    };

    let status = get_mods_status(game_root, recursive);
    let all_on_disk: HashSet<String> = status
        .mod_entries
        .iter()
        .map(|entry| entry.display_name.clone())
        .collect();
    let currently_enabled: HashSet<String> = selected_mods(session).into_iter().collect();

    let mut to_enable: Vec<String> = profile_mods
        .iter()
        .filter(|name| all_on_disk.contains(*name) && !currently_enabled.contains(*name))
        .cloned()
        .collect();
    let mut to_disable: Vec<String> = currently_enabled
        .iter()
        .filter(|name| !profile_mods.contains(*name))
        .cloned()
        .collect();
    let mut missing: Vec<String> = profile_mods
        .iter()
        .filter(|name| !all_on_disk.contains(*name))
        .cloned()
        .collect();
    let mut unchanged: Vec<String> = currently_enabled
        .iter()
        .filter(|name| profile_mods.contains(*name))
        .cloned()
        .collect();

    to_enable.sort();
    to_disable.sort();
    missing.sort();
    unchanged.sort();

    Ok(ProfileDiff {
        to_enable,
        to_disable,
        missing,
        unchanged,
    })
}

pub(crate) fn apply(
    settings: &Mutex<Settings>,
    session: &SessionLaunchState,
    name: &str,
    game_root: &str,
    recursive: bool,
) -> Result<ProfileApplyResult, String> {
    let diff = preview(settings, session, name, game_root, recursive)?;
    let status = get_mods_status(game_root, recursive);
    let all_on_disk: BTreeSet<String> = status
        .mod_entries
        .iter()
        .map(|entry| entry.display_name.clone())
        .collect();
    let selected: BTreeSet<String> = {
        let guard = settings.lock().map_err(|e| e.to_string())?;
        guard
            .mod_profiles
            .iter()
            .find(|profile| profile.name == name)
            .ok_or_else(|| format!("Profile \"{name}\" not found"))?
            .enabled_mods
            .iter()
            .filter(|mod_name| all_on_disk.contains(*mod_name))
            .cloned()
            .collect()
    };

    let successes = (diff.to_enable.len() + diff.to_disable.len()) as u32;
    replace_selected_mods(session, selected)?;
    Ok(ProfileApplyResult {
        successes,
        failed: 0,
        missing: diff.missing,
    })
}
