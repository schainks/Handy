use crate::settings;
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
#[specta::specta]
pub fn change_voice_commands_enabled_setting(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.voice_commands_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_voice_commands_api_key_setting(
    app: AppHandle,
    api_key: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.voice_commands_api_key = api_key.trim().to_string().into();
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_voice_commands_model_setting(app: AppHandle, model: String) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.voice_commands_model = model.trim().to_string();
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_voice_commands_threshold_setting(
    app: AppHandle,
    threshold: f64,
) -> Result<(), String> {
    if !(0.0..=1.0).contains(&threshold) {
        return Err(format!(
            "Threshold must be between 0 and 1, got {threshold}"
        ));
    }
    let mut settings = settings::get_settings(&app);
    settings.voice_commands_threshold = threshold;
    settings::write_settings(&app, settings);
    Ok(())
}

#[derive(Serialize, Type)]
pub struct VoiceCommandsStatus {
    /// Voice commands run actions through macOS automation.
    pub supported: bool,
    /// Hammerspoon's `hs` CLI is installed (window tiling and media keys).
    pub hammerspoon: bool,
    pub builtin_actions: u32,
    pub custom_commands: u32,
    /// Why the custom commands file couldn't be used, if it couldn't.
    pub custom_commands_error: Option<String>,
    pub custom_commands_path: String,
}

#[tauri::command]
#[specta::specta]
pub fn get_voice_commands_status(app: AppHandle) -> Result<VoiceCommandsStatus, String> {
    let status = crate::voice_control::status(&app)?;
    Ok(VoiceCommandsStatus {
        supported: cfg!(target_os = "macos"),
        hammerspoon: status.hammerspoon,
        builtin_actions: status.builtin_actions as u32,
        custom_commands: status.custom_commands as u32,
        custom_commands_error: status.custom_commands_error,
        custom_commands_path: status.custom_commands_path.to_string_lossy().into_owned(),
    })
}

/// Open the custom commands file in the default editor, creating it from a
/// template first if needed.
#[tauri::command]
#[specta::specta]
pub fn open_voice_commands_file(app: AppHandle) -> Result<(), String> {
    let path = crate::voice_control::ensure_custom_commands_file(&app)?;
    app.opener()
        .open_path(path.to_string_lossy().as_ref(), None::<String>)
        .map_err(|e| format!("Failed to open {}: {e}", path.display()))
}
