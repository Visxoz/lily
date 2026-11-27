//! Chooses between the Premium and the free stream for a track; what the
//! Premium path keeps across songs is in `premium_state`.

use std::{future::Future, rc::Rc, sync::Arc, time::Instant};

use super::premium_state::{
    PREMIUM_NO, PREMIUM_YES, PremiumShared, premium_dry_run, premium_player,
};
use crate::app::App;
use crate::{
    api::{self, Api},
    audio::{Input, Keep, Opening, Refresh, Source, StreamSpec},
    audio_cache::AudioCache,
    settings::Quality,
    streams::{self, ResolvedStream, premium},
};

/// The web client's stream for `video_id` through the Premium path's
/// solver, preparing the player first: a Premium one, or with `any_format`
/// the best there is.
async fn solved_stream(
    shared: &Arc<PremiumShared>,
    video_id: &str,
    cookie: &str,
    visitor: &str,
    any_format: bool,
    max_kbps: Option<u64>,
) -> Result<ResolvedStream, String> {
    let player = premium_player(shared).await?;
    match premium::resolve(
        video_id,
        Some(cookie),
        visitor,
        &player,
        any_format,
        max_kbps,
        false,
    )
    .await?
    {
        premium::Outcome::Stream(stream) => Ok(stream),
        premium::Outcome::NoPremiumFormats => Err("no Premium formats".into()),
    }
}

/// The best stream the web client has, through the Premium path's solver,
/// for when no client hands out plain URLs.
async fn web_fallback(
    shared: &Arc<PremiumShared>,
    video_id: &str,
    cookie: &str,
    visitor: &str,
    max_kbps: Option<u64>,
) -> Result<ResolvedStream, String> {
    let stream = solved_stream(shared, video_id, cookie, visitor, true, max_kbps).await?;
    premium::diag(format!(
        "{video_id}: no client gave a plain URL; playing through the web client"
    ));
    Ok(stream)
}

/// The player's spec for `stream`, with `keep` seeing all of it before the
/// opening takes its mime, which the copy kept is filed with.
fn spec(
    mut stream: ResolvedStream,
    start_at: f64,
    keep: impl FnOnce(&ResolvedStream) -> Option<Keep>,
    remote: impl FnOnce(ResolvedStream) -> (Source, Refresh),
) -> StreamSpec {
    let file = stream.file.take();
    let keep = file.is_none().then(|| keep(&stream)).flatten();
    let opening = Opening {
        mime: std::mem::take(&mut stream.mime),
        loudness_db: stream.loudness_db,
        start_at,
    };
    let input = match file {
        Some(path) => Input::File(path),
        None => {
            let (source, refresh) = remote(stream);
            Input::Stream(source, refresh, keep)
        }
    };
    StreamSpec { input, opening }
}

fn source(stream: ResolvedStream) -> Source {
    Source {
        url: stream.url,
        size: stream.size,
        headers: stream.headers,
        head: stream.head.get().cloned().unwrap_or_default(),
    }
}

/// What a track's stream is resolved with, read from the app's state on the
/// UI thread so the resolving can run, and run again, off it.
struct Resolver {
    api: Api,
    premium: Arc<PremiumShared>,
    video_id: String,
    want_premium: bool,
    max_kbps: Option<u64>,
    /// Opening a song again after its connection dropped, whose failures
    /// say nothing about Premium.
    reopening: bool,
    /// Its play goes to the account's history, as `record_play` decides.
    reports_play: bool,
}

impl Resolver {
    /// Premium when signed in with Premium, otherwise the first plain-URL
    /// client that answers, and signed in, the web client as a last resort;
    /// with whether the Premium path answered, so a copy kept of it has
    /// nothing better to wait for. One got while Premium was skipped, backed
    /// off or failing is asked for again once the Premium path is taken.
    async fn resolve(&self) -> Result<(ResolvedStream, bool), String> {
        let Self {
            api,
            premium,
            video_id,
            want_premium,
            reopening,
            reports_play,
            ..
        } = self;
        let video_id = video_id.as_str();
        if !api::is_video_id(video_id) {
            return Err("not a YouTube song".into());
        }
        let started = Instant::now();
        let visitor = api.visitor_data(false).await?;
        let cookie = api.cookie().filter(|_| *want_premium);
        let mut settled = false;
        if let Some(cookie) = cookie.as_deref().filter(|_| !premium.skipped()) {
            match premium_player(premium).await {
                Ok(player) => {
                    // A known Premium account plays a non-Premium song from this
                    // answer, saving the free clients; an unknown one counts a miss.
                    let known = premium.status() == PREMIUM_YES;
                    match premium::resolve(
                        video_id,
                        Some(cookie),
                        &visitor,
                        &player,
                        known,
                        None,
                        // A reopened song's play was reported already, and a
                        // brand channel's or another login's play must be
                        // reported as it, not as this request asked.
                        *reports_play && !reopening && api.signs_in_as_primary(),
                    )
                    .await
                    {
                        Ok(premium::Outcome::Stream(stream)) => {
                            if stream.premium {
                                premium.record_premium();
                            } else {
                                premium.record_miss();
                            }
                            log::info!(
                                "{video_id}: {} stream resolved in {} ms",
                                if stream.premium { "Premium" } else { "web" },
                                started.elapsed().as_millis()
                            );
                            return Ok((stream, true));
                        }
                        Ok(premium::Outcome::NoPremiumFormats) => {
                            premium.record_miss();
                            settled = true;
                        }
                        Err(e) => {
                            premium::diag(format!(
                                "{video_id}: Premium path failed, using the free stream: {e}"
                            ));
                            if !reopening {
                                premium.record_error();
                            }
                        }
                    }
                }
                Err(e) => premium::diag(format!("could not prepare the player: {e}")),
            }
        } else if let Some(cookie) = cookie.filter(|_| premium.take_recheck()) {
            // Checked off the playback path: this song plays the free
            // stream at once, and the next one gets Premium if it is back.
            let premium = Arc::clone(premium);
            let (video_id, visitor) = (video_id.to_owned(), visitor.clone());
            tokio::spawn(async move {
                let Ok(player) = premium_player(&premium).await else {
                    return;
                };
                match premium::resolve(
                    &video_id,
                    Some(&cookie),
                    &visitor,
                    &player,
                    false,
                    None,
                    false,
                )
                .await
                {
                    Ok(premium::Outcome::Stream(_)) => premium.record_premium(),
                    Ok(premium::Outcome::NoPremiumFormats) => premium.record_miss(),
                    Err(e) => premium::diag(format!("{video_id}: Premium check failed: {e}")),
                }
            });
        } else if !want_premium
            && premium_dry_run()
            && let Ok(player) = premium_player(premium).await
        {
            // Times the Premium path without an account; the result is
            // discarded as YouTube won't serve it anonymously.
            let dry = Instant::now();
            let result =
                premium::resolve(video_id, None, &visitor, &player, false, None, false).await;
            premium::diag(format!(
                "{video_id}: dry run {} in {} ms",
                if result.is_ok() { "resolved" } else { "failed" },
                dry.elapsed().as_millis()
            ));
        }
        Ok((self.free(&visitor, None).await?, settled))
    }

    /// The stream to download. Counts nothing towards the Premium state, as
    /// a playlist of music videos or uploads would read as a lapsed account.
    async fn for_keeping(&self) -> Result<ResolvedStream, String> {
        let video_id = self.video_id.as_str();
        if !api::is_video_id(video_id) {
            return Err("not a YouTube song".into());
        }
        let visitor = self.api.visitor_data(false).await?;
        let cookie = self
            .api
            .cookie()
            .filter(|_| self.want_premium && !self.premium.skipped());
        if let Some(cookie) = cookie
            && let Ok(stream) =
                solved_stream(&self.premium, video_id, &cookie, &visitor, false, None).await
        {
            return Ok(stream);
        }
        self.free(&visitor, None).await
    }

    /// A fresh URL for the same file. A free stream, web-client ones too, is
    /// asked of the plain-URL clients in format `itag`, so one googlevideo
    /// refuses still plays. Counts nothing towards the Premium counters, as
    /// the song already counted, but has the player checked sooner.
    async fn refresh(&self, was_premium: bool, itag: u32) -> Result<ResolvedStream, String> {
        let video_id = self.video_id.as_str();
        premium::diag(format!(
            "{video_id}: stream refused partway; asking for a fresh URL"
        ));
        let visitor = self.api.visitor_data(false).await?;
        if !was_premium {
            return self.free(&visitor, Some(itag)).await;
        }
        self.premium.suspect_player();
        let cookie = self.api.cookie().ok_or("signed out")?;
        solved_stream(&self.premium, video_id, &cookie, &visitor, false, None).await
    }

    /// The first plain-URL client that answers, and signed in, the web
    /// client as a last resort. `itag` holds the plain-URL clients to that
    /// format, but not the web client.
    async fn free(&self, visitor: &str, itag: Option<u32>) -> Result<ResolvedStream, String> {
        let Self {
            api,
            premium,
            video_id,
            max_kbps,
            ..
        } = self;
        let (video_id, max_kbps) = (video_id.as_str(), *max_kbps);
        let free = Instant::now();
        let result = match streams::resolve(video_id, visitor, max_kbps, itag).await {
            Ok(stream) => Ok(stream),
            // YouTube sometimes bot-checks a visitor ID; retry with a fresh one.
            Err(e) => {
                log::debug!("retrying with a new visitor ID: {e}");
                let fresh = api.visitor_data(true).await?;
                let result = streams::resolve(video_id, &fresh, max_kbps, itag).await;
                // Only a fresh ID working shows the old one was at fault;
                // both failing points at the song instead.
                if result.is_ok() {
                    api.forget_visitor(visitor);
                }
                result
            }
        };
        log::info!(
            "{video_id}: free stream resolved in {} ms",
            free.elapsed().as_millis()
        );
        match result {
            Ok(stream) => Ok(stream),
            // Signed in, the web client's scrambled URLs are a last resort,
            // read by the Premium solver whether or not the account has Premium.
            Err(e) => match api.cookie() {
                Some(cookie) => web_fallback(premium, video_id, &cookie, visitor, max_kbps)
                    .await
                    .map_err(|web| {
                        log::debug!("{video_id}: web client fallback failed: {web}");
                        e
                    }),
                None => Err(e),
            },
        }
    }
}

impl App {
    fn resolver(&self, video_id: String) -> Resolver {
        let state = self.state.borrow();
        let want_premium = state.signed_in
            && state.settings.quality == Quality::High
            && state.playback.force_free.as_deref() != Some(video_id.as_str());
        Resolver {
            api: self.api.clone(),
            premium: Arc::clone(&self.premium),
            reopening: state.playback.reopening(&video_id),
            reports_play: state.signed_in && state.settings.youtube_history,
            video_id,
            want_premium,
            max_kbps: state.settings.quality.max_kbps(),
        }
    }

    /// The playing song's stream and when it was resolved, while the player
    /// holds all of it, so a replay plays from memory (its URL may have expired).
    pub(in crate::app) fn held_stream(&self, video_id: &str) -> Option<(ResolvedStream, Instant)> {
        let state = self.state.borrow();
        let (id, stream, at) = state.playback.playing.as_ref()?;
        (id == video_id && self.player.holds(&stream.url)).then(|| (stream.clone(), *at))
    }

    /// A downloaded or local file plays from there, online or not, then a
    /// download the player holds whole, then one kept from playing lately,
    /// ahead of anything prefetched, as the song moved on from stays
    /// prefetched for Previous.
    pub(in crate::app) fn resolve_future(
        self: &Rc<Self>,
        video_id: String,
        prefetched: Option<ResolvedStream>,
    ) -> impl Future<Output = Result<ResolvedStream, String>> + Send + 'static {
        // Only `offline_stream` vouches for a file, which may since have gone.
        let offline = self.offline_stream(&video_id);
        let prefetched = prefetched.filter(|s| s.file.is_none());
        let held = prefetched
            .as_ref()
            .is_some_and(|s| self.player.holds(&s.url));
        let quality = self.state.borrow().settings.quality;
        let resolver = self.resolver(video_id);
        let want_premium = resolver.want_premium;
        // An account taken as without Premium plays any copy kept, but one
        // found to have it after all asks again for those kept meanwhile.
        let premium_copy = want_premium && self.premium.status() != PREMIUM_NO;
        async move {
            if let Some(stream) = offline {
                return Ok(stream);
            }
            let prefetched = match prefetched {
                Some(stream) if held => return Ok(stream),
                prefetched => prefetched,
            };
            let id = resolver.video_id.clone();
            // Off Tokio's workers, as a sleeping disk can take a while.
            let kept = tokio::task::spawn_blocking(move || {
                AudioCache::default().find(&id, quality, premium_copy)
            });
            if let Ok(Some(stream)) = kept.await {
                return Ok(stream);
            }
            match prefetched {
                Some(stream) => Ok(stream),
                None => resolver
                    .resolve()
                    .await
                    .map(|(stream, settled)| ResolvedStream {
                        asked: Some((quality, want_premium && settled)),
                        ..stream
                    }),
            }
        }
    }

    /// `video_id`'s stream to download, from YouTube.
    pub(in crate::app) fn keeping_future(
        &self,
        video_id: String,
    ) -> impl Future<Output = Result<ResolvedStream, String>> + Send + 'static {
        let resolver = self.resolver(video_id);
        async move { resolver.for_keeping().await }
    }

    /// The player's spec for `video_id`'s stream.
    pub(in crate::app) fn stream_spec(
        &self,
        video_id: &str,
        stream: ResolvedStream,
        start_at: f64,
    ) -> StreamSpec {
        // A song downloaded to keep is on disk already.
        let downloaded = self.state.borrow().downloads.contains(video_id);
        spec(
            stream,
            start_at,
            |stream| {
                (!downloaded)
                    .then(|| AudioCache::default().keep(video_id, stream))
                    .flatten()
            },
            |stream| self.remote_source(video_id, stream),
        )
    }

    /// Where `video_id`'s stream downloads from. Should googlevideo refuse its
    /// URL partway, the download's thread waits on a fresh one, and should
    /// that be refused too, on one from another client.
    pub(in crate::app) fn remote_source(
        &self,
        video_id: &str,
        stream: ResolvedStream,
    ) -> (Source, Refresh) {
        let resolver = Arc::new(self.resolver(video_id.to_owned()));
        let (rt, was_premium, itag) = (self.rt.clone(), stream.premium, stream.itag);
        let mut client = stream.client;
        let refresh = Box::new(move |again| {
            if again && let Some(name) = client {
                streams::cut_off(name);
            }
            let resolver = Arc::clone(&resolver);
            // Spawned rather than driven here: a runtime shutting down on
            // exit drops the task and the wait errors, where this would panic.
            let task = rt.spawn(async move { resolver.refresh(was_premium, itag).await });
            let stream = rt
                .block_on(task)
                .map_err(|_| "the refresh did not finish".to_owned())??;
            client = stream.client;
            Ok(source(stream))
        });
        (source(stream), refresh)
    }

    /// Prepares the Premium player script in the background, so the first
    /// song does not wait for it. Only runs when Premium audio can apply.
    pub(in crate::app) fn warm_premium(self: &Rc<Self>) {
        let wanted = {
            let state = self.state.borrow();
            state.signed_in && state.settings.quality == Quality::High && self.api.has_cookie()
        };
        if !wanted && !premium_dry_run() {
            return;
        }
        let shared = Arc::clone(&self.premium);
        self.rt.spawn(async move {
            if let Err(e) = premium_player(&shared).await {
                premium::diag(format!("could not prepare the player: {e}"));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_stream_with_its_mime() {
        let stream = ResolvedStream {
            mime: "audio/webm".into(),
            ..Default::default()
        };
        let mut seen = String::new();
        let keep = |stream: &ResolvedStream| {
            seen.clone_from(&stream.mime);
            None
        };
        let remote = |stream| (source(stream), Box::new(|_| Err(String::new())) as Refresh);
        let spec = spec(stream, 0.0, keep, remote);
        assert_eq!(seen, "audio/webm");
        assert_eq!(spec.opening.mime, "audio/webm");
    }
}
