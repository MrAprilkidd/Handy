//! Spacebar dictation.
//!
//! On Windows, a low-level keyboard hook turns a held Space into push-to-talk
//! and leaves a quick tap as a normal space. Other platforms persist the same
//! settings and do not install a hook, so Space is unchanged.

mod machine;

#[cfg(target_os = "windows")]
mod windows;

use log::debug;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

use crate::settings::{self, AppSettings};
use crate::tray;

/// Lowest hold that still leaves a recognizable tap. Shorter values make
/// ordinary spaces feel like they failed to insert.
pub const MIN_HOLD_THRESHOLD_MS: u64 = 50;
pub const MAX_HOLD_THRESHOLD_MS: u64 = 2_000;

/// Ignore hotkey auto-repeat so one physical press toggles once.
const TOGGLE_DEBOUNCE_MS: u64 = 400;
static LAST_TOGGLE_MS: AtomicU64 = AtomicU64::new(0);

pub fn clamp_hold_threshold_ms(ms: u64) -> u64 {
    ms.clamp(MIN_HOLD_THRESHOLD_MS, MAX_HOLD_THRESHOLD_MS)
}

/// Compare excluded-app entries to a foreground executable.
///
/// `LeagueOfLegends.exe`, `leagueoflegends`, and a full path all become
/// `leagueoflegends`.
pub fn normalize_process_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let base = trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed);
    let lower = base.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// Split a user-entered list (newlines, commas, or semicolons) into unique
/// normalized executable names.
pub fn parse_excluded_apps<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for item in items {
        for part in item.as_ref().split([',', '\n', '\r', ';']) {
            let name = normalize_process_name(part);
            if name.is_empty() || out.iter().any(|existing| existing == &name) {
                continue;
            }
            out.push(name);
        }
    }
    out
}

pub fn init(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    windows::init(app);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
    }
}

pub fn apply_settings(app: &AppHandle) {
    #[cfg(target_os = "windows")]
    windows::apply_settings(app);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
    }
}

/// True while the user is recording a shortcut in the settings UI. Space must
/// not be swallowed out from under that capture.
pub fn set_shortcut_capture(active: bool) {
    #[cfg(target_os = "windows")]
    windows::set_shortcut_capture(active);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = active;
    }
}

/// Reinstall the hook so it sits ahead of the shortcut backend's hook.
pub fn prefer_hook_front() {
    #[cfg(target_os = "windows")]
    windows::prefer_hook_front();
}

/// Hide the caret indicator when a recording is cancelled some other way
/// (Escape, tray Cancel) while Space is still physically down.
pub fn on_recording_cancelled() {
    #[cfg(target_os = "windows")]
    windows::on_recording_cancelled();
}

/// The transcription pipeline is recording or turning audio into text.
/// A swallowed Space during that window would be lost, because the
/// coordinator ignores a press it cannot start.
pub fn set_pipeline_active(active: bool) {
    #[cfg(target_os = "windows")]
    windows::set_pipeline_active(active);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = active;
    }
}

fn persist(app: &AppHandle, mutate: impl FnOnce(&mut AppSettings)) {
    let mut settings = settings::get_settings(app);
    mutate(&mut settings);
    let enabled = settings.spacebar_dictation_enabled;
    settings::write_settings(app, settings);
    apply_settings(app);
    tray::update_tray_menu(app);
    let _ = app.emit("settings-changed", ());
    debug!("spacebar dictation enabled={enabled}");
}

/// Flip the tray / hotkey toggle. Repeats inside [`TOGGLE_DEBOUNCE_MS`] are
/// ignored so a held hotkey does not chatter.
pub fn toggle_enabled(app: &AppHandle) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let previous = LAST_TOGGLE_MS.load(Ordering::SeqCst);
    if now.saturating_sub(previous) < TOGGLE_DEBOUNCE_MS {
        return;
    }
    LAST_TOGGLE_MS.store(now, Ordering::SeqCst);

    let enabled = !settings::get_settings(app).spacebar_dictation_enabled;
    persist(app, |settings| {
        settings.spacebar_dictation_enabled = enabled;
    });
}

#[tauri::command]
#[specta::specta]
pub fn change_spacebar_dictation_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    persist(&app, |settings| {
        settings.spacebar_dictation_enabled = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_spacebar_hold_threshold_ms_setting(app: AppHandle, ms: u64) -> Result<(), String> {
    let ms = clamp_hold_threshold_ms(ms);
    persist(&app, |settings| {
        settings.spacebar_hold_threshold_ms = ms;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_spacebar_text_field_only_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    persist(&app, |settings| {
        settings.spacebar_text_field_only = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_spacebar_block_fullscreen_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    persist(&app, |settings| {
        settings.spacebar_block_fullscreen = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_spacebar_caret_indicator_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    persist(&app, |settings| {
        settings.spacebar_caret_indicator = enabled;
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_spacebar_excluded_apps_setting(
    app: AppHandle,
    apps: Vec<String>,
) -> Result<(), String> {
    let apps = parse_excluded_apps(apps);
    persist(&app, |settings| {
        settings.spacebar_excluded_apps = apps;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_names_ignore_path_extension_and_case() {
        assert_eq!(
            normalize_process_name(r"C:\Games\League of Legends.exe"),
            "league of legends"
        );
        assert_eq!(normalize_process_name("NOTEPAD"), "notepad");
        assert_eq!(normalize_process_name("  "), "");
    }

    #[test]
    fn excluded_list_splits_and_dedupes() {
        let parsed = parse_excluded_apps(["Game.exe, notepad", "NOTEPAD.EXE\nSteam"]);
        assert_eq!(parsed, vec!["game", "notepad", "steam"]);
    }

    #[test]
    fn threshold_is_clamped() {
        assert_eq!(clamp_hold_threshold_ms(0), MIN_HOLD_THRESHOLD_MS);
        assert_eq!(clamp_hold_threshold_ms(350), 350);
        assert_eq!(clamp_hold_threshold_ms(9_000), MAX_HOLD_THRESHOLD_MS);
    }
}
