//! Everything here runs on the UI thread; network and disk work runs on
//! Tokio and comes back through `slint::spawn_local`.

mod account;
mod artwork;
mod bind;
mod browse;
mod library;
mod now_playing;
mod playback;
mod preferences;
mod session;
mod state;
mod tasks;
mod updates;
mod window;

use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tokio::{runtime::Handle, task::AbortHandle};

use crate::{
    AccentChoice, Api as ApiGlobal, AppWindow, CardItem, MiniPlayer, QueueItem, RowItem,
    api::{self, Api, Page, Route},
    audio::{Player, PlayerEvent, eq},
    downloads::Downloads,
    google_sign_in::SignIn,
    images, local,
    platform::media_controls::{Action, Controls},
    presence::PresenceHandle,
    queue::{Queue, Repeat},
    settings::{self, AUDIO_CACHE_MB, Accent, CROSSFADE_SECS, Settings},
    update,
};
use artwork::RowArt;
pub(crate) use now_playing::mini::window_attributes as mini_window_attributes;
use playback::premium_state::PremiumShared;
use playback::song_version::SongVersion;
use preferences::proxy_detail;
use state::{HistoryEntry, Playback, State};

const TICK: Duration = Duration::from_millis(250);
/// In the order Settings lists them.
const SPEEDS: [f32; 6] = [0.8, 0.9, 1.0, 1.1, 1.2, 1.3];
/// Equaliser animation step; a few frames a second keeps repaints cheap.
const EQ_FRAME: Duration = Duration::from_millis(125);
const TOAST_FOR: Duration = Duration::from_secs(5);
/// After the first page has had the network.
const AFTER_STARTUP: Duration = Duration::from_secs(10);
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);

pub struct App {
    ui: slint::Weak<AppWindow>,
    rt: Handle,
    api: Api,
    player: Player,
    presence: PresenceHandle,
    media: RefCell<Option<Controls>>,
    state: RefCell<State>,
    /// The page's rows, replaced for each new page (`App::show_page`).
    rows: RefCell<Rc<VecModel<RowItem>>>,
    /// The page `rows` were built for.
    rows_route: RefCell<Option<Route>>,
    library: Rc<VecModel<CardItem>>,
    queue_model: Rc<VecModel<QueueItem>>,
    eq_gains: Rc<VecModel<f32>>,
    /// Queue revision the queue model shows.
    queue_shown: Cell<Option<u64>>,
    selection: Cell<now_playing::queue_panel::Selection>,
    /// The page's and the queue's row artwork.
    row_art: RowArt,
    queue_art: RowArt,
    /// Each lazy grid's cards near the screen, by page row, as it last said.
    grid_lines: RefCell<HashMap<usize, (usize, usize)>>,
    /// The page's and the queue's artwork is let go while the window is hidden.
    art_hidden: Cell<bool>,
    tick: Timer,
    eq_timer: Timer,
    /// Lights up the next lyric line as it starts, between ticks.
    lyric_timer: Timer,
    premium: Arc<PremiumShared>,
    premium_shown: Cell<u8>,
    toast_timer: Timer,
    suggest_timer: Timer,
    sign_in_window: RefCell<Option<SignIn>>,
    sign_in_timer: Timer,
    lyrics_pass: RefCell<Option<now_playing::lyrics::PassCheck>>,
    motion: Arc<crate::motion::Motion>,
    /// The full player's animated artwork, while one plays.
    motion_playing: RefCell<Option<now_playing::motion::Playing>>,
    motion_timer: Timer,
    /// Bumped as the clip is stopped, so a late lookup starts none.
    motion_generation: Cell<u64>,
    update: RefCell<Option<update::Update>>,
    update_timer: Timer,
    cookie_timer: Timer,
    /// Fetches the AI music list again once a day, or after a failure.
    ai_artists_timer: Timer,
    /// Failed fetches of it in a row, which lengthen the wait.
    ai_artists_failures: Cell<u32>,
    /// The load in flight, aborted as another starts or the switch goes off.
    ai_artists_load: Cell<Option<AbortHandle>>,
    persist_timer: Timer,
    /// Retries scrobbles a service could not take.
    scrobble_timer: Timer,
    /// The signed-in Home, asked for as the app starts, for the first visit.
    home_ahead: RefCell<Option<HomeAhead>>,
    /// The local files as last read, loaded with the first scan.
    local: Arc<Mutex<Option<local::Library>>>,
    watcher: RefCell<Option<local::Watcher>>,
    /// Set while a check after files changed runs, to whether they changed again since.
    local_check: Cell<Option<bool>>,
    /// The local library's revision Local files last showed.
    local_shown: Cell<u64>,
    /// Settings' devices after "System default": cpal ID and name.
    output_devices: RefCell<Vec<(String, String)>>,
    /// Bumped as the audio cache is measured, so only the last shows.
    audio_cache_asks: Cell<u64>,
    tray: OnceCell<crate::platform::tray::Tray>,
    /// Set as the window minimises on its way to the tray.
    hide_pending: Cell<bool>,
    mini: RefCell<Option<MiniPlayer>>,
}

pub type HomeAhead = tokio::task::JoinHandle<api::Result<Page>>;

fn s(text: impl AsRef<str>) -> SharedString {
    SharedString::from(text.as_ref())
}

/// The colour `0xRRGGBB`, ignoring any alpha byte.
fn opaque(rgb: u32) -> slint::Color {
    slint::Color::from_argb_encoded(0xff00_0000 | (rgb & 0x00ff_ffff))
}

/// The local date and time, or UTC when Windows will not say the offset.
fn local_now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc())
}

impl App {
    pub fn new(
        ui: &AppWindow,
        rt: Handle,
        api: Api,
        settings: Settings,
        home_ahead: Option<HomeAhead>,
    ) -> Rc<Self> {
        let weak = ui.as_weak();
        let player = Player::spawn(move |event| {
            let (kind, message) = match event {
                PlayerEvent::Started => ("started", String::new()),
                PlayerEvent::Advanced => ("advanced", String::new()),
                PlayerEvent::NextDropped => ("next-dropped", String::new()),
                PlayerEvent::NextFailed { dropped } => {
                    ("next-failed", if dropped { "dropped" } else { "" }.into())
                }
                PlayerEvent::Ended => ("ended", String::new()),
                PlayerEvent::Error(e) => ("error", e),
                PlayerEvent::Dropped(e) => ("dropped", e),
                PlayerEvent::Unplugged => ("unplugged", String::new()),
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.invoke_player_event(kind.into(), message.into())
            });
        });
        player.set_volume(if settings.muted { 0.0 } else { settings.volume });
        player.set_normalise(settings.normalise);
        player.set_equaliser(settings.equaliser());
        player.set_crossfeed(settings.crossfeed);
        player.set_mono(settings.mono);
        player.set_balance(settings.balance);
        player.set_speed(settings.speed);
        player.set_pause_on_unplug(settings.pause_on_unplug);
        player.set_output_device(settings.output_device.as_ref().map(|d| d.id.clone()));

        let premium = PremiumShared::new(settings.premium_account, settings.no_premium);
        settings::remove_old_cookie();
        let presence = PresenceHandle::spawn().expect("presence thread spawns");
        let mut queue = Queue::new();
        queue.repeat = Repeat::from_index(settings.repeat);

        let app = Rc::new(Self {
            ui: ui.as_weak(),
            rt,
            api,
            player,
            presence,
            media: RefCell::new(None),
            state: RefCell::new(State {
                settings,
                signed_in: false,
                route: Route::Home,
                back: Vec::new(),
                forward: Vec::new(),
                page: Page::default(),
                account: 0,
                page_generation: 0,
                page_edits: HashMap::new(),
                restore_scroll: None,
                more_paused: None,
                queue,
                queue_generation: 0,
                playback: Playback::default(),
                lyrics: Default::default(),
                sung_spans: HashMap::new(),
                sung_fetching: HashMap::new(),
                suggest_generation: 0,
                page_cache: HashMap::new(),
                song_versions: HashMap::new(),
                history: settings::History::load(),
                searches: settings::Searches::load(),
                smart: playback::smart_shuffle::Smart::default(),
                rating_generation: 0,
                saving: None,
                artwork_accent: None,
                scrobbler: account::scrobble::Scrobbler::load(),
                downloads: Downloads::load(),
                download_queue: VecDeque::new(),
                downloading: false,
                library: Vec::new(),
                library_edits: HashSet::new(),
                library_revision: 0,
                library_generation: 0,
                sleep: None,
            }),
            rows: RefCell::default(),
            rows_route: RefCell::default(),
            library: Rc::new(VecModel::default()),
            queue_model: Rc::new(VecModel::default()),
            eq_gains: Rc::new(VecModel::from(vec![0.0; eq::BANDS.len()])),
            queue_shown: Cell::new(None),
            selection: Cell::new(now_playing::queue_panel::Selection::default()),
            row_art: RowArt::default(),
            queue_art: RowArt::default(),
            grid_lines: RefCell::default(),
            art_hidden: Cell::new(false),
            tick: Timer::default(),
            eq_timer: Timer::default(),
            lyric_timer: Timer::default(),
            premium: Arc::new(premium),
            premium_shown: Cell::new(u8::MAX),
            toast_timer: Timer::default(),
            suggest_timer: Timer::default(),
            sign_in_window: RefCell::new(None),
            sign_in_timer: Timer::default(),
            lyrics_pass: RefCell::new(None),
            motion: Arc::new(crate::motion::Motion::new()),
            motion_playing: RefCell::new(None),
            motion_timer: Timer::default(),
            motion_generation: Cell::new(0),
            update: RefCell::new(None),
            update_timer: Timer::default(),
            cookie_timer: Timer::default(),
            ai_artists_timer: Timer::default(),
            ai_artists_failures: Cell::new(0),
            ai_artists_load: Cell::new(None),
            persist_timer: Timer::default(),
            scrobble_timer: Timer::default(),
            home_ahead: RefCell::new(home_ahead),
            local: Arc::default(),
            watcher: RefCell::new(None),
            local_check: Cell::new(None),
            local_shown: Cell::new(0),
            output_devices: RefCell::default(),
            audio_cache_asks: Cell::new(0),
            tray: OnceCell::new(),
            hide_pending: Cell::new(false),
            mini: RefCell::new(None),
        });
        app.bind(ui);
        app.attach_tray();
        app.apply_settings_to_ui(ui);
        // Once, as the switch then holds it; it goes on only once its popup is confirmed.
        ui.set_block_ai(app.state.borrow().settings.block_ai);
        ui.set_spotify_signed_in(app.motion.spotify_signed_in());
        let proxy = crate::http::proxy_in_use();
        ui.set_proxy_detail(proxy_detail(proxy.is_some(), false).into());
        ui.set_proxy_address(proxy.unwrap_or_default().into());
        app.show_folders();
        app.show_blocked();
        app.apply_theme(ui);
        // Set here and by the switch only, so other settings changing can't
        // wipe out a found update's line.
        ui.set_update_status(
            updates::up_to_date_status(app.state.borrow().settings.check_updates).into(),
        );
        ui.set_rows(ModelRc::from(app.rows()));
        ui.set_library(ModelRc::from(Rc::clone(&app.library)));
        ui.set_queue(ModelRc::from(Rc::clone(&app.queue_model)));
        ui.set_eq_gains(ModelRc::from(Rc::clone(&app.eq_gains)));
        ui.set_eq_max_db(eq::MAX_DB);
        let labels: Vec<SharedString> = eq::BANDS
            .iter()
            .map(|&hz| match hz {
                1_000.0.. => format!("{} kHz", hz / 1_000.0),
                _ => format!("{hz} Hz"),
            })
            .map(Into::into)
            .collect();
        ui.set_eq_bands(ModelRc::new(VecModel::from(labels)));
        let presets: Vec<SharedString> = eq::PRESETS.iter().map(|(name, _)| s(name)).collect();
        ui.set_eq_presets(ModelRc::new(VecModel::from(presets)));
        let accents: Vec<AccentChoice> = Accent::ALL
            .iter()
            .map(|accent| AccentChoice {
                name: s(accent.name()),
                colour: accent.rgb().map(opaque).unwrap_or_default(),
                artwork: accent.rgb().is_none(),
            })
            .collect();
        ui.set_accents(ModelRc::new(VecModel::from(accents)));
        app.show_recent_searches();
        app.show_output_device();
        let speeds: Vec<SharedString> = SPEEDS
            .iter()
            .map(|speed| format!("{speed}×").into())
            .collect();
        ui.set_speeds(ModelRc::new(VecModel::from(speeds)));
        let lengths: Vec<SharedString> = CROSSFADE_SECS
            .iter()
            .map(|secs| format!("{secs} s").into())
            .collect();
        ui.set_crossfade_lengths(ModelRc::new(VecModel::from(lengths)));
        let sizes: Vec<SharedString> = AUDIO_CACHE_MB
            .iter()
            .map(|&mb| preferences::size_text(settings::mb_bytes(mb)).into())
            .collect();
        ui.set_audio_cache_sizes(ModelRc::new(VecModel::from(sizes)));
        // Until measured as Settings opens.
        ui.set_audio_cache_detail(format!("Songs {}", preferences::AUDIO_CACHE_KEPT).into());
        app
    }

    fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("window outlives the app")
    }

    /// A handle rather than a borrow, so editing it can't overlap a swap.
    fn rows(&self) -> Rc<VecModel<RowItem>> {
        Rc::clone(&self.rows.borrow())
    }

    /// How much faster than the clock song time passes.
    fn speed(&self) -> f64 {
        f64::from(self.state.borrow().settings.speed)
    }

    fn show_playing(&self, playing: bool) {
        let ui = self.ui();
        ui.global::<ApiGlobal>().set_is_playing(playing);
        // The words stop where the sound did, and the next tick starts them again.
        if !playing {
            let clock = ui.global::<crate::LyricsClock>();
            clock.set_at(self.player.position() as f32);
            clock.set_rate(0.0);
        }
        self.sync_mini();
    }

    fn toast(self: &Rc<Self>, message: impl Into<String>) {
        let message = message.into();
        log::info!("{message}");
        let ui = self.ui();
        ui.set_toast(message.into());
        ui.set_toast_visible(true);
        let weak = self.ui.clone();
        self.toast_timer
            .start(TimerMode::SingleShot, TOAST_FOR, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_toast_visible(false);
                }
            });
    }

    /// Starts background work once the event loop runs.
    pub fn start(self: &Rc<Self>) {
        images::set_scale(self.ui().window().scale_factor());
        self.restore_session();
        // At once, from the copy kept however old, so the first plays leave them out.
        self.load_ai_artists(false);
        // Files deleted while the app was closed.
        self.local_files_changed();
        let app = Rc::clone(self);
        self.tick
            .start(TimerMode::Repeated, TICK, move || app.on_tick());

        let weak = self.ui.clone();
        self.eq_timer.start(TimerMode::Repeated, EQ_FRAME, move || {
            if let Some(ui) = weak.upgrade() {
                let api = ui.global::<ApiGlobal>();
                if api.get_is_playing() {
                    api.set_eq_phase(api.get_eq_phase().wrapping_add(1));
                }
            }
        });

        // A stored cookie counts as signed in, so Home loads without waiting
        // for YouTube to confirm it.
        let signed_in = self.api.has_cookie();
        self.state.borrow_mut().signed_in = signed_in;
        self.ui().global::<ApiGlobal>().set_signed_in(signed_in);
        // Home, opened below, loads the account's picture, and the first
        // signed-in answer that comes back signed out has the cookie checked.
        self.watch_sign_in();
        if signed_in {
            // Last run's playlists and picture show while fresh ones load.
            self.show_library(settings::load_library());
            let photo = self.state.borrow().settings.account_photo.clone();
            if let Some(url) = photo {
                self.show_account_photo(url);
            }
            self.load_library();
            if let Some(id) = self.playing_id() {
                self.load_rating(id);
            }
        }
        self.warm_premium();
        self.show_scrobbling();
        // Plays left over from a run that could not reach a service.
        self.send_scrobbles();
        self.navigate(Route::Home, false);
        // Once at startup, then again on their timers for a copy left running.
        let weak = Rc::downgrade(self);
        Timer::single_shot(AFTER_STARTUP, move || {
            if let Some(app) = weak.upgrade() {
                app.check_for_update();
                app.renew_sign_in_when_due();
            }
        });
        let weak = Rc::downgrade(self);
        self.update_timer
            .start(TimerMode::Repeated, UPDATE_CHECK_EVERY, move || {
                if let Some(app) = weak.upgrade() {
                    app.check_for_update();
                }
            });
    }

    /// Hooks up what needs the native window, which appears some time after
    /// the event loop starts: the media overlay and the title bar's colours.
    pub fn attach_window(&self, hwnd: *mut std::ffi::c_void) {
        self.apply_theme(&self.ui());
        let weak = self.ui.clone();
        let controls = Controls::new(hwnd, move |action| {
            let (name, value) = match action {
                Action::Toggle => ("toggle", 0.0),
                Action::Play => ("play", 0.0),
                Action::Pause => ("pause", 0.0),
                Action::Next => ("next", 0.0),
                Action::Previous => ("previous", 0.0),
                Action::Stop => ("pause", 0.0),
                Action::SeekBy(by) => ("seek-by", by),
                Action::SeekTo(at) => ("seek-to", at),
            };
            let _ = weak
                .upgrade_in_event_loop(move |ui| ui.invoke_media_action(name.into(), value as f32));
        });
        *self.media.borrow_mut() = controls;
    }

    fn shutdown(&self) {
        // Windows would leave the icon until the pointer passes over it.
        if let Some(tray) = self.tray.get() {
            tray.set_visible(false);
        }
        self.persist();
        self.player.stop();
        self.presence.shutdown();
    }
}
