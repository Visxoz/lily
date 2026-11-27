//! Windows integration: the tray icon, media overlay, single instance,
//! folder picker and DPAPI-encrypted storage.

pub mod folder_picker;
pub mod media_controls;
pub mod secret;
pub mod single_instance;
pub mod tray;

/// Whether Windows' apps are in light mode, light where it isn't set, as
/// Windows itself takes it.
pub fn apps_light() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_u32("AppsUseLightTheme"))
        != Ok(0)
}
