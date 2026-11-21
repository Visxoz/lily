//! The download thread: fetches the stream range by range, retrying, riding
//! out a lost connection, and asking for a fresh URL once googlevideo refuses one.

use std::{
    io::{self, Read},
    time::{Duration, Instant},
};

use super::{CHUNK, Download, Refresh, Source};

const READ_BUF: usize = 64 * 1024;
const MAX_RETRIES: u32 = 4;
/// How long a connection that stops answering is retried, so one that
/// comes back within it plays on.
const RIDE_OUT: Duration = Duration::from_secs(60);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Fresh URLs asked for in a row without a whole range arriving from them:
/// the same client once more, then each of the others.
const MAX_REFRESHES: u32 = crate::streams::CLIENT_COUNT as u32;

impl Download {
    pub(super) fn run(&self, mut source: Source, mut refresh: Refresh) {
        if let Err(e) = check_host(&source.url) {
            return self.fail(e);
        }
        // Kept apart, so a stray error once a lost connection is back gets
        // its own tries.
        let (mut retries, mut drops, mut refreshes) = (0, 0, 0);
        // From the first failure in a row, not the last byte, as a PC
        // waking from sleep fails at once while its network comes back.
        let mut failing_since = None;
        let mut buf = vec![0; READ_BUF];
        loop {
            let (offset, total) = {
                let mut state = self.state.lock().unwrap();
                loop {
                    if state.cancelled {
                        return;
                    }
                    if state.total > 0 && state.run_end(0) >= state.total {
                        state.done = true;
                        drop(state);
                        self.changed.notify_all();
                        return;
                    }
                    state.evict();
                    if let Some(offset) = state.next_range() {
                        break (offset, state.total);
                    }
                    if state.abandoned {
                        // Frees the window, and fails any late reader at once.
                        drop(state);
                        return self.cancel();
                    }
                    // The window ahead is held, so on once the reader moves.
                    let block = state.pos / CHUNK;
                    state = self
                        .changed
                        .wait_while(state, |s| {
                            !s.cancelled && !s.abandoned && s.pos / CHUNK == block
                        })
                        .unwrap();
                }
            };
            let error = match self.fetch(&source, offset, total, &mut buf) {
                Ok(()) => {
                    retries = 0;
                    drops = 0;
                    refreshes = 0;
                    failing_since = None;
                    continue;
                }
                Err(e) => e,
            };
            {
                let state = self.state.lock().unwrap();
                // Nobody reads a cancelled download, so it neither waits nor asks.
                if state.cancelled {
                    return;
                }
                // A range that broke off after some bytes still got somewhere,
                // so only a stretch with nothing arriving gives up.
                if state.run_end(offset) > offset {
                    retries = 0;
                    drops = 0;
                    failing_since = None;
                }
            }
            let refused = is_refused(&*error);
            if refused && refreshes < MAX_REFRESHES {
                // Asking the same URL again cannot help, but a fresh one
                // carries on from the same byte.
                refreshes += 1;
                match refreshed(&mut refresh, &source, refreshes > 1) {
                    Ok(new) => {
                        source = new;
                        continue;
                    }
                    Err(e) => log::warn!("no fresh URL for a refused download: {e}"),
                }
            } else if is_connection(&*error) {
                let failing = *failing_since.get_or_insert_with(Instant::now);
                if let Some(delay) = retry_delay(drops, failing.elapsed()) {
                    drops += 1;
                    log::debug!("download lost its connection, retrying ({drops}): {error}");
                    self.wait(delay);
                    continue;
                }
                self.state.lock().unwrap().dropped = true;
            } else if !refused && retries < MAX_RETRIES {
                retries += 1;
                log::debug!("download chunk failed, retrying ({retries}): {error}");
                self.wait(RETRY_DELAY * retries);
                continue;
            }
            log::warn!("download failed: {error}");
            return self.fail(error.to_string());
        }
    }

    /// Sleeps for `delay`, or until cancelled.
    fn wait(&self, delay: Duration) {
        let state = self.state.lock().unwrap();
        let _ = self
            .changed
            .wait_timeout_while(state, delay, |s| !s.cancelled);
    }

    /// Ends the download with `error`, which a blocked read then returns.
    fn fail(&self, error: String) {
        self.state.lock().unwrap().error = Some(error);
        self.changed.notify_all();
    }

    /// Fetches one range starting at `offset`, reading through `buf`: to the
    /// end of its block, or all of a stream of unknown length.
    fn fetch(
        &self,
        source: &Source,
        offset: u64,
        total: u64,
        buf: &mut [u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut request = crate::http::blocking_client().get(&source.url);
        for (name, value) in &source.headers {
            request = request.header(name, value);
        }
        let mut end = u64::MAX;
        if total > 0 {
            // Ends on a megabyte, so starting after a prefetched head doesn't
            // push the first range past where cut-off URLs start refusing.
            end = ((offset / CHUNK + 1) * CHUNK).min(total);
            let last = end - 1;
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-{last}"));
        }
        let mut response = request
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(reqwest::Error::without_url)?;
        // Anything but a partial answer starts at the file's first byte,
        // which must not follow the bytes already held.
        if offset > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err("the answer does not follow the bytes held".into());
        }
        if total == 0 {
            self.state.lock().unwrap().total = response.content_length().unwrap_or(0);
        }

        let mut at = offset;
        loop {
            if at == end {
                break;
            }
            // Never past the range, as the next block may be held already.
            let room = usize::try_from(end - at).unwrap_or(usize::MAX);
            let n = response.read(buf)?.min(room);
            if n == 0 {
                break;
            }
            let mut state = self.state.lock().unwrap();
            // A seek out of the window would only see this block dropped.
            if state.cancelled || !state.kept((at / CHUNK) as usize) {
                return Ok(());
            }
            state.put(at, &buf[..n]);
            at += n as u64;
            drop(state);
            self.changed.notify_all();
        }
        // An empty answer would be asked again forever.
        if at == offset {
            return Err("the server sent no bytes".into());
        }
        let mut state = self.state.lock().unwrap();
        // All of a stream of unknown length has arrived, so now it is known.
        if state.total == 0 {
            state.total = at;
        }
        Ok(())
    }
}

/// The wait before retrying after `retries` failures in a row, doubling, or
/// `None` once they have gone on for `RIDE_OUT`.
fn retry_delay(retries: u32, failing: Duration) -> Option<Duration> {
    (failing < RIDE_OUT).then(|| (RETRY_DELAY * 2u32.saturating_pow(retries)).min(MAX_RETRY_DELAY))
}

fn check_host(url: &str) -> Result<(), String> {
    if crate::http::allowed(url) {
        return Ok(());
    }
    log::warn!("refused a stream from a host YouTube does not use");
    Err("unexpected stream host".into())
}

/// Whether the connection failed, rather than googlevideo answering with
/// an error or bytes that don't fit.
fn is_connection(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<io::Error>()
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.status().is_none())
}

/// Whether googlevideo refused the URL itself rather than one request.
fn is_refused(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        == Some(reqwest::StatusCode::FORBIDDEN)
}

/// A fresh URL for the file `old` serves. It must be the same encode, as
/// the bytes already downloaded have to line up with the rest.
fn refreshed(refresh: &mut Refresh, old: &Source, again: bool) -> Result<Source, String> {
    let new = refresh(again)?;
    check_host(&new.url)?;
    if new.size != old.size || !same_file(&old.url, &new.url) {
        return Err("YouTube served a different file".into());
    }
    Ok(new)
}

/// Whether two googlevideo URLs serve the same file: the same format, of
/// the same length, from the same encode.
fn same_file(a: &str, b: &str) -> bool {
    query_param(a, "itag").is_some()
        && ["itag", "clen", "lmt"]
            .iter()
            .all(|name| query_param(a, name) == query_param(b, name))
}

fn query_param<'a>(url: &'a str, name: &str) -> Option<&'a str> {
    let (_, query) = url.split_once('?')?;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    use crate::audio::source::{
        KEEP_AHEAD,
        tests::{MB, URL, fill},
    };

    #[test]
    fn resumes_only_on_the_same_file() {
        let fresh = "https://rr4.googlevideo.com/videoplayback?itag=251&n=b&lmt=1700000000000000&clen=4171552";
        assert!(same_file(URL, fresh), "another host and order, same file");
        assert!(!same_file(URL, &fresh.replace("itag=251", "itag=250")));
        assert!(!same_file(
            URL,
            &fresh.replace("clen=4171552", "clen=4171553")
        ));
        assert!(
            !same_file(URL, &fresh.replace("lmt=1700000000000000", "lmt=1")),
            "re-encoded"
        );
        assert!(
            !same_file("https://a/251", "https://a/251"),
            "no format to go by"
        );
    }

    fn source(url: &str, size: u64) -> Source {
        Source {
            url: url.into(),
            size,
            headers: Vec::new(),
            head: Vec::new(),
        }
    }

    fn refresh_to(url: &'static str, size: u64) -> Refresh {
        Box::new(move |_| Ok(source(url, size)))
    }

    #[test]
    fn takes_a_fresh_url_only_for_the_same_file_and_host() {
        let old = source(URL, 4171552);
        let fresh =
            "https://rr4.googlevideo.com/videoplayback?itag=251&clen=4171552&lmt=1700000000000000";
        assert!(refreshed(&mut refresh_to(fresh, 4171552), &old, false).is_ok());
        assert!(
            refreshed(&mut refresh_to(fresh, 4171553), &old, false).is_err(),
            "another length"
        );
        let elsewhere =
            "https://example.com/videoplayback?itag=251&clen=4171552&lmt=1700000000000000";
        assert_eq!(
            refreshed(&mut refresh_to(elsewhere, 4171552), &old, false).err(),
            Some("unexpected stream host".into())
        );
        let mut failing: Refresh = Box::new(|_| Err("offline".into()));
        assert_eq!(
            refreshed(&mut failing, &old, false).err(),
            Some("offline".into())
        );
    }

    #[test]
    fn tells_refusals_and_lost_connections_from_other_errors() {
        let answer = |status: u16| {
            let response = http::Response::builder().status(status).body("").unwrap();
            reqwest::Response::from(response)
                .error_for_status()
                .map_err(reqwest::Error::without_url)
                .unwrap_err()
        };
        assert!(is_refused(&answer(403)));
        assert!(!is_refused(&answer(404)), "another status is retried");
        assert!(!is_refused(&answer(503)));
        let other: Box<dyn std::error::Error> = "the server sent no bytes".into();
        assert!(!is_refused(&*other), "not an HTTP error");
        assert!(
            !is_connection(&answer(503)),
            "an answer, not a lost connection"
        );
        assert!(!is_connection(&*other));
        assert!(is_connection(&io::Error::from(
            io::ErrorKind::ConnectionReset
        )));
    }

    #[test]
    fn a_window_stops_once_nothing_reads_it() {
        let download = Arc::new(Download::sized(100 * MB));
        {
            let mut state = download.state.lock().unwrap();
            state.window = true;
            for block in 0..KEEP_AHEAD as u64 {
                fill(&mut state, block);
            }
        }
        let reader = download.reader();
        let (tx, stopped) = std::sync::mpsc::channel();
        let worker = Arc::clone(&download);
        thread::spawn(move || {
            worker.run(source(URL, 100 * MB), refresh_to(URL, 100 * MB));
            let _ = tx.send(());
        });
        let wait = Duration::from_millis(200);
        assert!(stopped.recv_timeout(wait).is_err(), "waits on the reader");
        drop(reader);
        assert!(stopped.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(!download.whole(URL), "never done");
    }

    #[test]
    fn retries_a_dropped_connection_until_it_has_been_gone_a_while() {
        let secs = Duration::from_secs;
        assert_eq!(retry_delay(0, Duration::ZERO), Some(RETRY_DELAY));
        assert_eq!(retry_delay(1, secs(1)), Some(RETRY_DELAY * 2));
        assert_eq!(retry_delay(40, secs(30)), Some(MAX_RETRY_DELAY), "capped");
        assert_eq!(retry_delay(0, RIDE_OUT), None, "given up");
    }

    #[test]
    fn reads_whole_query_parameters() {
        let url = "https://a/v?xitag=1&itag=251&itag2=3";
        assert_eq!(query_param(url, "itag"), Some("251"));
        assert_eq!(query_param(url, "clen"), None);
        assert_eq!(query_param("https://a/v", "itag"), None);
    }
}
