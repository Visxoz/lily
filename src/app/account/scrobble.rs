//! Scrobbling to Last.fm and ListenBrainz: connecting the accounts, and
//! logging each song once enough of it has been heard.

use std::{
    cell::{Ref, RefMut},
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{SharedString, Timer, TimerMode};

use crate::app::App;
use crate::lastfm;
use crate::scrobbling::{self, Error, Pending, Play, Service, Session};

/// Polling for access allowed in the browser. The token lasts an hour, and
/// someone who first has to sign up or reset a password can take a while.
const POLL_EVERY: Duration = Duration::from_secs(3);
const POLL_FOR: Duration = Duration::from_secs(20 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// One service's account and the plays it has yet to take.
struct Link {
    session: Option<Session>,
    pending: Pending,
    sending: bool,
    /// The service refused a batch as invalid, so this many of its plays go
    /// one at a time, to find the one it refuses and drop it alone.
    singles: usize,
    /// Bumped by connecting and disconnecting, so replies for an earlier
    /// account or attempt are ignored.
    generation: u64,
}

impl Link {
    fn load(service: Service) -> Self {
        let session = service.load_session();
        let mut pending = service.pending();
        if let Some(session) = &session {
            pending.for_account(&session.name);
        }
        Self {
            session,
            pending,
            sending: false,
            singles: 0,
            generation: 0,
        }
    }
}

pub(in crate::app) struct Scrobbler {
    /// By `Service::ALL`'s order.
    links: [Link; 2],
    /// The song playing, until enough of it has been heard to count.
    playing: Option<Play>,
}

impl Scrobbler {
    pub(in crate::app) fn load() -> Self {
        Self {
            links: Service::ALL.map(Link::load),
            playing: None,
        }
    }

    pub(in crate::app) fn save(&mut self) {
        for link in &mut self.links {
            link.pending.save();
        }
    }
}

impl App {
    fn link(&self, service: Service) -> Ref<'_, Link> {
        Ref::map(self.state.borrow(), |state| {
            &state.scrobbler.links[service as usize]
        })
    }

    fn link_mut(&self, service: Service) -> RefMut<'_, Link> {
        RefMut::map(self.state.borrow_mut(), |state| {
            &mut state.scrobbler.links[service as usize]
        })
    }

    pub(in crate::app) fn show_scrobbling(&self) {
        self.ui().set_lastfm_available(lastfm::available());
        for service in Service::ALL {
            self.show_scrobbling_account(service);
        }
    }

    fn show_scrobbling_account(&self, service: Service) {
        let name: SharedString = self
            .link(service)
            .session
            .as_ref()
            .map_or_else(SharedString::new, |s| s.name.as_str().into());
        let ui = self.ui();
        match service {
            Service::LastFm => ui.set_lastfm_user(name),
            Service::ListenBrainz => ui.set_listenbrainz_user(name),
        }
    }

    fn show_scrobbling_connecting(&self, service: Service, connecting: bool) {
        let ui = self.ui();
        match service {
            Service::LastFm => ui.set_lastfm_connecting(connecting),
            Service::ListenBrainz => ui.set_listenbrainz_connecting(connecting),
        }
    }

    /// Starts logging the song just started, and sends it as playing now.
    pub(in crate::app) fn scrobble_started(self: &Rc<Self>) {
        let play = {
            let mut state = self.state.borrow_mut();
            let state = &mut *state;
            let scrobbler = &mut state.scrobbler;
            scrobbler.playing = None;
            if scrobbler.links.iter().all(|link| link.session.is_none()) {
                return;
            }
            let Some(track) = state.queue.current() else {
                return;
            };
            let play = Play {
                // The first artist, as Last.fm's catalogue lists features
                // in the title rather than as artists.
                artist: track
                    .artists
                    .first()
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
                track: track.title.clone(),
                album: track.album.as_ref().map(|(_, name)| name.clone()),
                duration: track.duration,
                timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                local: track.is_local(),
            };
            // Both refuse a play without either.
            if play.artist.is_empty() || play.track.is_empty() {
                return;
            }
            scrobbler.playing = Some(play.clone());
            play
        };
        for service in Service::ALL {
            let Some((session, generation)) = ({
                let link = self.link(service);
                link.session.clone().map(|s| (s, link.generation))
            }) else {
                continue;
            };
            let play = play.clone();
            self.spawn(
                async move { service.now_playing(&session, &play).await },
                move |app, result| app.scrobble_failed(service, result, generation),
            );
        }
    }

    /// Logs the song once half of it or four minutes has been heard.
    pub(in crate::app) fn scrobble_heard(self: &Rc<Self>, heard: Duration, length: Duration) {
        {
            let scrobbler = &mut self.state.borrow_mut().scrobbler;
            let Some(play) = scrobbler
                .playing
                .take_if(|_| scrobbling::counts(heard, length))
            else {
                return;
            };
            for link in scrobbler.links.iter_mut().filter(|l| l.session.is_some()) {
                link.pending.push(play.clone());
            }
        }
        self.send_scrobbles();
    }

    /// Sends every service its pending plays.
    pub(in crate::app) fn send_scrobbles(self: &Rc<Self>) {
        for service in Service::ALL {
            self.send_scrobbles_to(service);
        }
    }

    /// Sends the oldest pending plays, then the rest, saving them once the
    /// service answers.
    fn send_scrobbles_to(self: &Rc<Self>, service: Service) {
        let (session, batch, generation) = {
            let mut link = self.link_mut(service);
            if link.sending || link.pending.is_empty() {
                return;
            }
            let Some(session) = link.session.clone() else {
                return;
            };
            link.sending = true;
            let batch = link.pending.batch(link.singles > 0);
            (session, batch, link.generation)
        };
        // Only the result comes back, so a panic still clears `sending`.
        let sent = batch.clone();
        self.spawn(
            async move { service.scrobble(&session, &sent).await },
            move |app, result| {
                let more = {
                    let link = &mut *app.link_mut(service);
                    link.sending = false;
                    let more = match &result {
                        // For an earlier account, whose plays are gone; newer
                        // ones may have waited behind it.
                        _ if link.generation != generation => true,
                        Ok(()) => {
                            link.pending.sent(&batch);
                            link.singles = link.singles.saturating_sub(1);
                            true
                        }
                        // One play spoils a batch; find it, and drop it alone.
                        Err(Error::Rejected(_)) if batch.len() > 1 => {
                            link.singles = batch.len();
                            true
                        }
                        // Waits before the next, so a refusal that isn't
                        // about the play costs a play a minute, not them all.
                        Err(Error::Rejected(e)) => {
                            log::warn!("{} refused a play: {e}", service.name());
                            link.pending.sent(&batch);
                            link.singles = link.singles.saturating_sub(1);
                            false
                        }
                        Err(_) => false,
                    };
                    link.pending.save();
                    (!link.pending.is_empty()).then_some(more)
                };
                app.scrobble_failed(service, result, generation);
                match more {
                    Some(true) => app.send_scrobbles_to(service),
                    Some(false) => app.send_scrobbles_later(),
                    None => {}
                }
            },
        );
    }

    /// Tries the plays again in a while, as nothing else may send them until
    /// another song counts.
    fn send_scrobbles_later(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.scrobble_timer
            .start(TimerMode::SingleShot, RETRY_AFTER, move || {
                if let Some(app) = weak.upgrade() {
                    app.send_scrobbles();
                }
            });
    }

    /// Forgets an account whose access was revoked, keeping its plays for
    /// when it connects again; other failures keep the plays for the next
    /// attempt. Replies for an earlier account change nothing.
    fn scrobble_failed(
        self: &Rc<Self>,
        service: Service,
        result: Result<(), Error>,
        generation: u64,
    ) {
        match result {
            Ok(()) | Err(Error::Rejected(_)) => {}
            Err(_) if self.link(service).generation != generation => {}
            Err(Error::SignedOut) => {
                self.end_scrobbling(service);
                self.toast(format!(
                    "{} no longer allows access. Connect it again in Settings.",
                    service.name()
                ));
            }
            Err(e) => log::warn!("{}: {e}", service.name()),
        }
    }

    /// Starts an attempt to connect, returning its generation.
    fn begin_scrobbling_connect(&self, service: Service) -> u64 {
        let generation = {
            let mut link = self.link_mut(service);
            link.generation += 1;
            link.generation
        };
        self.show_scrobbling_connecting(service, true);
        generation
    }

    /// Opens Last.fm in the browser for the user to allow access, then
    /// waits for them to.
    pub(in crate::app) fn connect_lastfm(self: &Rc<Self>) {
        let generation = self.begin_scrobbling_connect(Service::LastFm);
        self.spawn(lastfm::authorise(), move |app, result| {
            if app.link(Service::LastFm).generation != generation {
                return;
            }
            match result {
                Ok((page, token)) => {
                    crate::http::open_in_browser(&page);
                    app.poll_lastfm(token, generation, Instant::now());
                }
                Err(e) => app.scrobbling_not_connected(Service::LastFm, &e),
            }
        });
    }

    /// Asks whether access was allowed, again every few seconds until it is,
    /// through any brief failure to reach Last.fm.
    fn poll_lastfm(self: &Rc<Self>, token: String, generation: u64, since: Instant) {
        let asked = token.clone();
        self.spawn(
            async move { lastfm::session(&asked).await },
            move |app, result| {
                if app.link(Service::LastFm).generation != generation {
                    return;
                }
                match result {
                    Ok(session) => app.scrobbling_connected(Service::LastFm, session),
                    Err(Error::NotYetAllowed | Error::Failed(_)) if since.elapsed() < POLL_FOR => {
                        let weak = Rc::downgrade(app);
                        Timer::single_shot(POLL_EVERY, move || {
                            if let Some(app) = weak.upgrade() {
                                app.poll_lastfm(token, generation, since);
                            }
                        });
                    }
                    Err(e) => app.scrobbling_not_connected(Service::LastFm, &e),
                }
            },
        );
    }

    /// Connects ListenBrainz with the user token pasted in Settings.
    pub(in crate::app) fn connect_listenbrainz(self: &Rc<Self>, token: &str) {
        let generation = self.begin_scrobbling_connect(Service::ListenBrainz);
        let token = token.trim().to_owned();
        self.spawn(crate::listenbrainz::validate(token), move |app, result| {
            if app.link(Service::ListenBrainz).generation != generation {
                return;
            }
            match result {
                Ok(session) => app.scrobbling_connected(Service::ListenBrainz, session),
                Err(e) => app.scrobbling_not_connected(Service::ListenBrainz, &e),
            }
        });
    }

    fn scrobbling_connected(self: &Rc<Self>, service: Service, session: Session) {
        service.save_session(&session);
        self.toast(format!(
            "Scrobbling to {} as {}",
            service.name(),
            session.name
        ));
        {
            let mut link = self.link_mut(service);
            link.pending.for_account(&session.name);
            link.pending.save();
            link.session = Some(session);
        }
        self.show_scrobbling_connecting(service, false);
        self.show_scrobbling_account(service);
        self.send_scrobbles_to(service);
    }

    fn scrobbling_not_connected(self: &Rc<Self>, service: Service, error: &Error) {
        let name = service.name();
        log::warn!("could not connect {name}: {error}");
        self.show_scrobbling_connecting(service, false);
        self.toast(match error {
            // Only a token can be refused while connecting.
            Error::SignedOut => format!("{name} didn't accept that token."),
            _ => format!("Couldn't connect to {name}. Try again in Settings."),
        });
    }

    /// Forgets the account, or stops waiting for one to connect. Plays not
    /// yet sent are dropped with an account, so none reach the next one;
    /// stopping a wait keeps those of an account whose access was revoked.
    pub(in crate::app) fn disconnect_scrobbling(&self, service: Service) {
        let connected = self.link(service).session.is_some();
        self.end_scrobbling(service);
        if connected {
            let pending = &mut self.link_mut(service).pending;
            pending.clear();
            pending.save();
        }
    }

    /// Forgets the account's session, and any reply on its way for it.
    fn end_scrobbling(&self, service: Service) {
        {
            let mut link = self.link_mut(service);
            link.generation += 1;
            link.session = None;
            link.singles = 0;
        }
        service.remove_session();
        self.show_scrobbling_connecting(service, false);
        self.show_scrobbling_account(service);
    }
}
