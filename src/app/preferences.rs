//! Settings as the window shows and changes them: the theme and accent, the
//! music folders, the output device and the proxy.

use std::rc::Rc;

use slint::{
    ComponentHandle, Model, ModelRc, SharedString, VecModel,
    winit_030::{WinitWindowAccessor, winit},
};

use super::{App, SPEEDS, opaque, s};
use crate::{
    AppWindow, LyricsExtras, Theme,
    audio::eq,
    audio_cache::AudioCache,
    images, local,
    settings::{self, AUDIO_CACHE_MB, Accent, CROSSFADE_SECS, Look, Settings},
};

impl App {
    pub(super) fn apply_settings_to_ui(&self, ui: &AppWindow) {
        let state = self.state.borrow();
        let settings = &state.settings;
        ui.set_volume(settings.volume);
        ui.set_muted(settings.muted);
        ui.set_shuffle(settings.shuffle);
        ui.set_smart_shuffle(settings.smart_shuffle);
        ui.set_repeat_mode(settings.repeat);
        ui.set_normalise(settings.normalise.index());
        ui.set_discord(settings.discord);
        ui.set_animated_art(settings.animated_art);
        let extras = ui.global::<LyricsExtras>();
        extras.set_translate(settings.lyrics_translation);
        extras.set_romanise(settings.lyrics_romanisation);
        ui.set_autoplay(settings.autoplay);
        ui.set_youtube_history(settings.youtube_history);
        ui.set_rating_buttons(settings.rating_buttons);
        ui.set_quality(settings.quality.index());
        ui.set_crossfade(settings.crossfade);
        ui.set_crossfade_secs(
            CROSSFADE_SECS
                .iter()
                .position(|&s| s == settings.crossfade_secs)
                .map_or(-1, |i| i as i32),
        );
        ui.set_automix(settings.automix);
        ui.set_pause_on_unplug(settings.pause_on_unplug);
        ui.set_speed(
            SPEEDS
                .iter()
                .position(|&s| s == settings.speed)
                .map_or(-1, |i| i as i32),
        );
        ui.set_audio_cache_size(
            AUDIO_CACHE_MB
                .iter()
                .position(|&mb| mb == settings.audio_cache_mb)
                .map_or(-1, |i| i as i32),
        );
        ui.set_close_to_tray(settings.close_to_tray);
        ui.set_albums_by_artist(settings.albums_by_artist);
        ui.set_sidebar_collapsed(settings.sidebar_collapsed);
        ui.set_check_updates(settings.check_updates);
        ui.set_equaliser(settings.equaliser);
        ui.set_eq_preset(
            eq::PRESETS
                .iter()
                .position(|(_, gains)| *gains == settings.equaliser_gains)
                .map_or(-1, |i| i as i32),
        );
        for (band, &db) in settings.equaliser_gains.iter().enumerate() {
            self.eq_gains.set_row_data(band, db);
        }
        ui.set_crossfeed(settings.crossfeed);
        ui.set_mono(settings.mono);
        ui.set_balance(settings.balance);
        ui.set_accent_choice(settings.accent.index());
        ui.set_look_choice(settings.look.index());
    }

    /// Lists the music folders, and lets their files be read; run as they change.
    pub(super) fn show_folders(&self) {
        let folders = &self.state.borrow().settings.local_folders;
        crate::local::set_folders(folders);
        self.watcher.take();
        let ui = self.ui.clone();
        *self.watcher.borrow_mut() = local::watch(folders, move || {
            let _ = ui.upgrade_in_event_loop(|ui| ui.invoke_local_files_changed());
        });
        let names: Vec<SharedString> = folders.iter().map(|f| s(f.to_string_lossy())).collect();
        self.ui()
            .set_local_folders(ModelRc::new(VecModel::from(names)));
    }

    /// Colours the window and its title bar. Run only where the look changes,
    /// as it makes Windows restyle the window; the accent alone goes through
    /// `apply_accent`.
    pub(super) fn apply_theme(&self, ui: &AppWindow) {
        let look = self.state.borrow().settings.look.shown();
        ui.global::<Theme>().set_look(look.index());
        self.apply_accent(ui);
        let light = look == Look::Light;
        // Windows draws the title bar. Until the native window exists this
        // does nothing, and `attach_window` applies the theme again.
        ui.window().with_winit_window(|window| {
            window.set_theme(Some(if light {
                winit::window::Theme::Light
            } else {
                winit::window::Theme::Dark
            }));
        });
    }

    /// Run as Windows' settings change, which they do in bursts and for many
    /// reasons, so the window restyles only when System's look moved.
    pub(super) fn follow_system_look(&self) {
        let ui = self.ui();
        let look = self.state.borrow().settings.look;
        if look == Look::System && look.shown().index() != ui.global::<Theme>().get_look() {
            self.apply_theme(&ui);
        }
    }

    /// Sets the accent alone, which leaves the title bar as it is.
    pub(super) fn apply_accent(&self, ui: &AppWindow) {
        let state = self.state.borrow();
        let settings = &state.settings;
        let accent = settings
            .accent
            .rgb()
            .map(opaque)
            .or(state.artwork_accent)
            .unwrap_or_else(|| opaque(settings::LILY));
        let theme = ui.global::<Theme>();
        theme.set_accent(images::readable(accent, theme.get_light()));
        drop(state);
        self.theme_mini();
    }

    /// Keeps the playing artwork's colour, recolouring the window only when
    /// the accent follows the artwork and the colour changed.
    pub(super) fn set_artwork_accent(&self, accent: Option<slint::Color>) {
        let recolour = {
            let mut state = self.state.borrow_mut();
            let changed = state.artwork_accent != accent;
            state.artwork_accent = accent;
            changed && state.settings.accent == Accent::Artwork
        };
        if recolour {
            self.apply_accent(&self.ui());
        }
    }

    pub(super) fn update_settings(&self, change: impl FnOnce(&mut Settings)) {
        let settings = {
            let mut state = self.state.borrow_mut();
            change(&mut state.settings);
            state.settings.clone()
        };
        settings.save();
        self.apply_settings_to_ui(&self.ui());
    }

    /// Changes settings shown at once, saving only once `save`, as a slider
    /// sends every move of a drag.
    pub(super) fn change_settings(&self, save: bool, change: impl FnOnce(&mut Settings)) {
        if save {
            self.update_settings(change);
        } else {
            change(&mut self.state.borrow_mut().settings);
            self.apply_settings_to_ui(&self.ui());
        }
    }

    pub(super) fn change_equaliser(&self, save: bool, change: impl FnOnce(&mut Settings)) {
        self.change_settings(save, change);
        self.player
            .set_equaliser(self.state.borrow().settings.equaliser());
    }

    /// Probed on a thread of their own, as cpal sets up COM on it.
    pub(super) fn list_output_devices(self: &Rc<Self>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::audio::output_devices());
        });
        self.spawn(
            async move { rx.await.unwrap_or_default() },
            |app, devices| {
                *app.output_devices.borrow_mut() = devices;
                app.show_output_device();
            },
        );
    }

    pub(super) fn show_output_device(&self) {
        let ui = self.ui();
        let devices = self.output_devices.borrow();
        let chosen = &self.state.borrow().settings.output_device;
        let current = chosen.as_ref().map_or(Some(0), |chosen| {
            devices
                .iter()
                .position(|(id, _)| *id == chosen.id)
                .map(|i| i + 1)
        });
        let detail = match chosen {
            None => "Follows the default in Windows.".to_owned(),
            // Plugged in, or not listed yet.
            Some(chosen) if current.is_some() || devices.is_empty() => chosen.name.clone(),
            Some(chosen) => format!(
                "{} isn't plugged in, so the default plays until it is.",
                chosen.name
            ),
        };
        let names: Vec<SharedString> = std::iter::once(s("System default"))
            .chain(devices.iter().map(|(_, name)| s(name)))
            .collect();
        ui.set_output_devices(ModelRc::new(VecModel::from(names)));
        ui.set_output_device(current.map_or(-1, |i| i as i32));
        ui.set_output_detail(detail.into());
    }

    /// Measures the audio cache for its Settings row off the UI thread, once
    /// `tidy` (a prune or a clear) has run there.
    pub(super) fn show_audio_cache(
        self: &Rc<Self>,
        tidy: impl FnOnce(&AudioCache) + Send + 'static,
    ) {
        let asked = self.audio_cache_asks.get() + 1;
        self.audio_cache_asks.set(asked);
        let measure = self.rt.spawn_blocking(move || {
            let cache = AudioCache::default();
            tidy(&cache);
            cache.size()
        });
        self.spawn(measure, move |app, size| {
            // The last asked for, as a clear can land after a later look.
            if let Ok(size) = size
                && app.audio_cache_asks.get() == asked
            {
                let detail = format!(
                    "{} of songs played lately, kept so they play again without the network.",
                    size_text(size)
                );
                app.ui().set_audio_cache_detail(detail.into());
            }
        });
    }

    /// Keeps as much as Settings lists at `index`, pruning to it at once.
    pub(super) fn set_audio_cache(self: &Rc<Self>, index: usize) {
        let Some(&mb) = AUDIO_CACHE_MB.get(index) else {
            return;
        };
        self.update_settings(|s| s.audio_cache_mb = mb);
        crate::audio_cache::set_budget(self.state.borrow().settings.audio_cache_budget());
        self.show_audio_cache(AudioCache::prune);
    }

    /// Saves `address` as the proxy for the next start, returning why it
    /// won't do, or nothing.
    pub(super) fn set_proxy(&self, address: &str) -> SharedString {
        match crate::http::save_proxy(address) {
            Ok((saved, pending)) => {
                let ui = self.ui();
                ui.set_proxy_detail(proxy_detail(saved.is_some(), pending).into());
                ui.set_proxy_address(saved.unwrap_or_default().into());
                SharedString::new()
            }
            Err(e) => e.into(),
        }
    }

    /// Plays through the device Settings lists at `index`, 0 being the default.
    pub(super) fn choose_output_device(&self, index: usize) {
        let device = match index.checked_sub(1) {
            None => None,
            Some(i) => match self.output_devices.borrow().get(i) {
                Some((id, name)) => Some(settings::OutputDevice {
                    id: id.clone(),
                    name: name.clone(),
                }),
                None => return,
            },
        };
        if device == self.state.borrow().settings.output_device {
            return;
        }
        self.player
            .set_output_device(device.as_ref().map(|d| d.id.clone()));
        self.update_settings(|s| s.output_device = device);
        self.show_output_device();
    }
}

/// A size in decimal units, as the audio cache's are offered.
pub(super) fn size_text(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{} GB", (bytes as f64 / 1e8).round() / 10.0)
    } else {
        format!("{} MB", (bytes as f64 / 1e6).round())
    }
}

/// What the proxy row says of the one saved, which is `pending` until the
/// next start, as the clients took theirs at startup.
pub(super) fn proxy_detail(saved: bool, pending: bool) -> &'static str {
    match (saved, pending) {
        (true, false) => "Connects through this proxy.",
        (true, true) => "Connects through this proxy from Lily's next start.",
        (false, false) => {
            "Follows the proxy set in Windows. Enter an http:// or socks5:// address to use another."
        }
        (false, true) => "Follows the proxy set in Windows from Lily's next start.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_sizes_as_offered() {
        assert_eq!(size_text(0), "0 MB");
        assert_eq!(size_text(12_400_000), "12 MB");
        assert_eq!(size_text(1_000_000_000), "1 GB");
        assert_eq!(size_text(1_460_000_000), "1.5 GB");
    }
}
