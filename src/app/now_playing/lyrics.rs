//! The lyrics panel: lyrics kept for the songs played lately and fetched
//! ahead for the next, the line and words being sung, and the lyrics hub's
//! pass.

use std::{collections::HashMap, rc::Rc, time::Duration};

use slint::{ComponentHandle, ModelRc, TimerMode, VecModel};
use tokio::task::AbortHandle;

use super::super::{App, TICK, window};
use crate::{
    LyricLine, LyricWord, LyricsClock, LyricsExtras,
    api::{self, Track, lyrics::hub},
    lyrics_pass,
};

/// Songs whose lyrics are kept, starting over once full.
const CACHE_LIMIT: usize = 32;

#[derive(Default)]
pub(in crate::app) struct Lyrics {
    /// The song whose lyrics are shown or loading.
    shown_for: Option<String>,
    pub(in crate::app) shown: Rc<api::Lyrics>,
    /// `None` for a song without lyrics.
    cache: HashMap<String, Option<Rc<api::Lyrics>>>,
    fetching: HashMap<String, AbortHandle>,
}

/// The lyrics hub's pass being checked for, one check at a time.
pub(in crate::app) enum PassCheck {
    /// Held for its WebView2, which dropping closes.
    Running { _check: lyrics_pass::Check },
    /// Its WebView2 being made, which sign-in waits out.
    Starting,
    /// Its WebView2 closing, or its pass verified.
    Verifying,
}

impl App {
    /// Shows the playing song's lyrics, unless they are shown or loading.
    pub(in crate::app) fn load_lyrics(self: &Rc<Self>) {
        let mut guard = self.state.borrow_mut();
        let state = &mut *guard;
        let lyrics = &mut state.lyrics;
        let Some(track) = state.queue.current() else {
            drop(guard);
            // The same song played again then loads afresh.
            self.leave_lyrics();
            self.show_message("Play something to see its lyrics.");
            return;
        };
        if lyrics.shown_for.as_ref() == Some(&track.id) {
            return;
        }
        lyrics.shown_for = Some(track.id.clone());
        if let Some(found) = lyrics.cache.get(&track.id).cloned() {
            drop(guard);
            self.show_found(found);
            return;
        }
        // One fetched ahead shows once it lands.
        let fetch = (!lyrics.fetching.contains_key(&track.id)).then(|| track.clone());
        drop(guard);
        self.ui().set_lyrics_loading(true);
        if let Some(track) = fetch {
            self.fetch_lyrics(track);
        }
    }

    /// Stops fetching the song left's lyrics, so skipping through songs asks
    /// the sites for none of them.
    pub(in crate::app) fn leave_lyrics(&self) {
        // Its line was the song left's.
        self.lyric_timer.stop();
        let mut state = self.state.borrow_mut();
        let lyrics = &mut state.lyrics;
        if let Some(handle) = lyrics
            .shown_for
            .take()
            .and_then(|id| lyrics.fetching.remove(&id))
        {
            handle.abort();
        }
        lyrics.shown = Rc::default();
    }

    /// Fetches the next song's lyrics while lyrics are on screen, so they show
    /// as it starts. A music video is left for its song version, which plays
    /// instead.
    pub(in crate::app) fn prefetch_lyrics(self: &Rc<Self>) {
        if !self.ui().get_lyrics_shown() {
            return;
        }
        let next = {
            let state = self.state.borrow();
            let lyrics = &state.lyrics;
            state
                .queue
                .peek_next()
                .filter(|t| {
                    !t.video
                        && !lyrics.cache.contains_key(&t.id)
                        && !lyrics.fetching.contains_key(&t.id)
                })
                .cloned()
        };
        if let Some(next) = next {
            self.fetch_lyrics(next);
        }
    }

    fn fetch_lyrics(self: &Rc<Self>, track: Track) {
        let api = self.api.clone();
        let id = track.id.clone();
        let handle = self.spawn(async move { api.lyrics(&track).await }, {
            let id = id.clone();
            move |app, result| app.lyrics_fetched(id, result)
        });
        self.state.borrow_mut().lyrics.fetching.insert(id, handle);
    }

    fn lyrics_fetched(self: &Rc<Self>, id: String, result: api::Result<Option<api::Lyrics>>) {
        let result = result.map(|found| found.map(Rc::new));
        // Not while it is already showing, as after the hub's pass.
        let loading = self.ui().get_lyrics_loading();
        let shown = {
            let mut state = self.state.borrow_mut();
            let lyrics = &mut state.lyrics;
            lyrics.fetching.remove(&id);
            let mut shown = lyrics.shown_for.as_ref() == Some(&id);
            match &result {
                Ok(found) => {
                    shown &= loading || found.as_deref() != Some(&*lyrics.shown);
                    if lyrics.cache.len() >= CACHE_LIMIT {
                        lyrics.cache.clear();
                    }
                    lyrics.cache.insert(id, found.clone());
                }
                // A failure keeps the lyrics showing; one loading is asked
                // again when next shown.
                Err(e) => {
                    if shown && !loading {
                        log::debug!("lyrics looked up again failed: {e}");
                    }
                    shown &= loading;
                    if shown {
                        lyrics.shown_for = None;
                    }
                }
            }
            shown
        };
        if shown {
            match result {
                Ok(found) => self.show_found(found),
                Err(e) => self.show_message(&format!("Couldn't load lyrics: {e}")),
            }
        }
        self.check_lyrics_pass();
    }

    fn show_found(self: &Rc<Self>, found: Option<Rc<api::Lyrics>>) {
        match found {
            Some(lyrics) => self.show_lyrics(lyrics),
            None => self.show_message("Lyrics aren't available for this song."),
        }
    }

    fn show_message(self: &Rc<Self>, message: &str) {
        self.show_lyrics(Rc::new(api::Lyrics::plain(message, String::new())));
    }

    fn show_lyrics(self: &Rc<Self>, lyrics: Rc<api::Lyrics>) {
        let ui = self.ui();
        let words: &[Vec<api::lyrics::Word>] = if lyrics.is_word_synced() {
            &lyrics.words
        } else {
            &[]
        };
        // Skips the empty ones, as a `SharedString` of "" still allocates.
        let extra = |lines: &[String], i: usize| {
            lines
                .get(i)
                .filter(|text| !text.is_empty())
                .map(Into::into)
                .unwrap_or_default()
        };
        let lines = lyrics.lines.iter().enumerate().map(|(i, text)| LyricLine {
            text: text.into(),
            translation: extra(&lyrics.translations, i),
            romanised: extra(&lyrics.romanised, i),
            words: words
                .get(i)
                .filter(|words| !words.is_empty())
                .map(|words| {
                    ModelRc::new(VecModel::from_iter(words.iter().map(|word| LyricWord {
                        text: word.text.as_str().into(),
                        start: word.start as f32,
                        end: word.end as f32,
                    })))
                })
                .unwrap_or_default(),
        });
        ui.set_lyrics(ModelRc::new(VecModel::from_iter(lines)));
        let extras = ui.global::<LyricsExtras>();
        extras.set_has_translation(!lyrics.translations.is_empty());
        extras.set_has_romanisation(!lyrics.romanised.is_empty());
        ui.set_lyrics_synced(lyrics.is_synced());
        ui.set_lyrics_source(lyrics.source.as_str().into());
        ui.set_lyrics_current(-1);
        ui.set_lyrics_loading(false);
        self.state.borrow_mut().lyrics.shown = lyrics;
        // The bar's position, which is the restored one before playing.
        self.show_lyric_line(f64::from(ui.get_np_position()));
    }

    /// Lights up the lyric line being sung at `position` and sets the clock
    /// its words fill by, when synced lyrics are on screen. A line starting
    /// before the next tick is lit by a timer, so it lands on the beat.
    pub(in crate::app) fn show_lyric_line(self: &Rc<Self>, position: f64) {
        let ui = self.ui();
        if !ui.get_lyrics_shown() {
            return;
        }
        let (line, wait, sung_until, speed, started) = {
            let state = self.state.borrow();
            let lyrics = &state.lyrics.shown;
            if !lyrics.is_synced() {
                return;
            }
            let speed = f64::from(state.settings.speed);
            let line = lyrics.starts.partition_point(|&start| start <= position);
            let words = line.checked_sub(1).and_then(|i| lyrics.words.get(i));
            (
                line as i32 - 1,
                // On the clock, as the timer counts.
                lyrics
                    .starts
                    .get(line)
                    .map(|start| (start - position) / speed),
                words.and_then(|words| words.iter().map(|w| w.end).reduce(f64::max)),
                speed,
                // Until a track has started, the engine's position is not the bar's.
                state.playback.started,
            )
        };
        ui.set_lyrics_current(line);
        let playing = started && !self.player.is_paused();
        if let Some(until) = sung_until {
            let clock = ui.global::<LyricsClock>();
            clock.set_until(until as f32);
            clock.set_at(position as f32);
            clock.set_rate(if playing { speed as f32 } else { 0.0 });
        }
        if let Some(wait) = wait.filter(|&wait| wait < TICK.as_secs_f64())
            && playing
        {
            let weak = Rc::downgrade(self);
            self.lyric_timer.start(
                TimerMode::SingleShot,
                Duration::from_secs_f64(wait),
                move || {
                    if let Some(app) = weak.upgrade() {
                        app.show_lyric_line(app.player.position());
                    }
                },
            );
        }
    }

    /// Runs Cloudflare's check for the hub's pass when it's due, while the
    /// window is on screen, as it passes only there.
    fn check_lyrics_pass(self: &Rc<Self>) {
        let ui = self.ui();
        // Its WebView2 shares sign-in's folder, which one window at a time uses;
        // sign-in's window is out of its cell while its page is attached.
        let signing_in = || {
            self.sign_in_window.borrow().is_some()
                || ui.get_signing_in_browser()
                || ui.get_spotify_signing_in()
        };
        if self.lyrics_pass.borrow().is_some()
            || signing_in()
            || !window::is_shown(&ui)
            || !hub::wants_check()
        {
            return;
        }
        // Held while the WebView2 is made, as that pumps messages that may
        // land here again.
        *self.lyrics_pass.borrow_mut() = Some(PassCheck::Starting);
        let weak = Rc::downgrade(self);
        let done = move |token| {
            if let Some(app) = weak.upgrade() {
                app.lyrics_pass_checked(token);
            }
        };
        let started = lyrics_pass::Check::start(&ui, done);
        let pass = match started {
            Ok(check) if !signing_in() => Some(PassCheck::Running { _check: check }),
            // Sign-in opened meanwhile; the check closes as it drops.
            Ok(_) => None,
            Err(e) => {
                log::warn!("could not run the lyrics check: {e}");
                hub::check_failed();
                None
            }
        };
        *self.lyrics_pass.borrow_mut() = pass;
    }

    fn lyrics_pass_checked(self: &Rc<Self>, token: Option<String>) {
        // The first answer ends the check; any after it are dropped.
        let Some(check) = self.stop_lyrics_pass() else {
            return;
        };
        // Held while its WebView2 closes, so no check starts meanwhile.
        *self.lyrics_pass.borrow_mut() = Some(PassCheck::Verifying);
        drop(check);
        let Some(token) = token else {
            log::info!("the lyrics check didn't pass");
            hub::check_failed();
            *self.lyrics_pass.borrow_mut() = None;
            return;
        };
        self.spawn(hub::verify(token), |app, verified| {
            *app.lyrics_pass.borrow_mut() = None;
            match verified {
                Some(true) => app.refetch_lyrics(),
                Some(false) => {}
                None => {
                    log::info!("the lyrics hub gave no pass");
                    hub::check_failed();
                }
            }
        });
    }

    /// Songs looked up without a pass went without the hub's lyrics, so the
    /// cache is dropped and the song shown looked up again, its lyrics staying
    /// until the new ones arrive.
    fn refetch_lyrics(self: &Rc<Self>) {
        let track = {
            let mut guard = self.state.borrow_mut();
            let state = &mut *guard;
            let lyrics = &mut state.lyrics;
            lyrics.cache.clear();
            let track = state
                .queue
                .current()
                .filter(|t| lyrics.shown_for.as_ref() == Some(&t.id))
                .cloned();
            // One in flight started without the pass.
            if let Some(handle) = track.as_ref().and_then(|t| lyrics.fetching.remove(&t.id)) {
                handle.abort();
            }
            track
        };
        if let Some(track) = track {
            self.fetch_lyrics(track);
        }
    }

    /// Takes out a running check, as sign-in's WebView2 would share its folder.
    /// It closes as the caller drops it, once the borrow is released.
    pub(in crate::app) fn stop_lyrics_pass(&self) -> Option<PassCheck> {
        self.lyrics_pass
            .borrow_mut()
            .take_if(|pass| matches!(pass, PassCheck::Running { .. }))
    }
}
