//! Better Lyrics' Cloudflare check, which passes only a real browser: run in a
//! WebView2 a pixel in size on the main window, about once a day. It passes by
//! itself or fails; one wanting a click isn't shown.

use std::{rc::Rc, time::Duration};

use serde::Deserialize;
use slint::{ComponentHandle, Timer, TimerMode};
use url::Url;
use wry::{
    NewWindowResponse, PermissionResponse, Rect, WebView,
    dpi::{LogicalPosition, LogicalSize},
    http::Request,
};

use crate::{AppWindow, api::lyrics::hub, google_sign_in};

/// Time for the widget to retry an error of its own once or twice.
const LIMIT: Duration = Duration::from_secs(20);
/// Far longer than any token Cloudflare gives.
const MAX_TOKEN: usize = 8192;
/// A token with room for the rest of its message.
const MAX_MESSAGE: usize = 2 * MAX_TOKEN;
/// The page posts its answer to its parent, which for a page alone is itself.
const LISTENER: &str = "addEventListener('message', e => {
  if (e.source === window && typeof e.data?.type === 'string' && e.data.type.startsWith('turnstile-'))
    window.ipc.postMessage(JSON.stringify(e.data));
});";

/// Dropping it ends the check.
pub struct Check {
    _webview: WebView,
    _limit: Timer,
    /// The only strong reference, so an answer still queued is dropped with it.
    _done: Rc<dyn Fn(Option<String>)>,
}

#[derive(Deserialize)]
struct Message<'a> {
    r#type: &'a str,
    token: Option<String>,
}

impl Check {
    /// `done` gets the token, or `None` once the check fails or runs out of
    /// time, after the event that brought it, so it may drop the check.
    pub fn start(ui: &AppWindow, done: impl Fn(Option<String>) + 'static) -> Result<Self, String> {
        let done: Rc<dyn Fn(Option<String>)> = Rc::new(done);
        let weak = Rc::downgrade(&done);
        let answer = move |token| {
            let weak = weak.clone();
            Timer::single_shot(Duration::ZERO, move || {
                if let Some(done) = weak.upgrade() {
                    done(token);
                }
            });
        };
        let timed_out = answer.clone();
        // wry reads the context only while building on Windows, so it isn't kept.
        let webview =
            google_sign_in::web_view(&mut google_sign_in::web_context(), hub::CHALLENGE_URL)
                .with_transparent(true)
                .with_focused(false)
                .with_bounds(Rect {
                    position: LogicalPosition::new(0, 0).into(),
                    size: LogicalSize::new(1, 1).into(),
                })
                .with_initialization_script(LISTENER)
                .with_ipc_handler(move |request: Request<String>| {
                    if let Some(token) = answer_in(&request) {
                        answer(token);
                    }
                })
                .with_navigation_handler(|url| {
                    Url::parse(&url).is_ok_and(|url| is_hub(Some(url.scheme()), url.host_str()))
                })
                .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
                .with_permission_handler(|_| PermissionResponse::Deny)
                .build_as_child(&ui.window().window_handle())
                .map_err(|e| e.to_string())?;
        let limit = Timer::default();
        limit.start(TimerMode::SingleShot, LIMIT, move || timed_out(None));
        Ok(Self {
            _webview: webview,
            _limit: limit,
            _done: done,
        })
    }
}

/// The hub's page only; Cloudflare's frames inside it aren't top-level, so
/// navigation never sees them.
fn is_hub(scheme: Option<&str>, host: Option<&str>) -> bool {
    scheme == Some("https") && host == Some(hub::HOST)
}

/// `Some(None)` for a failure, `None` for a message that answers nothing.
fn answer_in(request: &Request<String>) -> Option<Option<String>> {
    let uri = request.uri();
    if !is_hub(uri.scheme_str(), uri.host()) || request.body().len() > MAX_MESSAGE {
        return None;
    }
    let message: Message = serde_json::from_str(request.body()).ok()?;
    // The widget renews an expired token and retries an error itself; the
    // time limit ends one that keeps failing.
    match message.r#type {
        "turnstile-token" => Some(
            message
                .token
                .filter(|t| !t.is_empty() && t.len() <= MAX_TOKEN),
        ),
        "turnstile-timeout" => Some(None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(uri: &str, body: &str) -> Option<Option<String>> {
        answer_in(&Request::builder().uri(uri).body(body.to_owned()).unwrap())
    }

    const PAGE: &str = hub::CHALLENGE_URL;

    #[test]
    fn takes_only_the_hubs_answers() {
        let token = r#"{"type":"turnstile-token","token":"abc"}"#;
        assert_eq!(answer(PAGE, token), Some(Some("abc".to_owned())));
        assert_eq!(answer("http://lyrics.api.dacubeking.com/", token), None);
        assert_eq!(
            answer("https://lyrics.api.dacubeking.com.evil.com/", token),
            None
        );
        assert_eq!(answer("https://evil.com/", token), None);
    }

    #[test]
    fn reads_the_answer() {
        assert_eq!(answer(PAGE, r#"{"type":"turnstile-timeout"}"#), Some(None));
        assert_eq!(
            answer(PAGE, r#"{"type":"turnstile-token","token":""}"#),
            Some(None)
        );
        assert_eq!(answer(PAGE, r#"{"type":"turnstile-token"}"#), Some(None));
        let long = "a".repeat(MAX_TOKEN + 1);
        let body = format!(r#"{{"type":"turnstile-token","token":"{long}"}}"#);
        assert_eq!(answer(PAGE, &body), Some(None));
        assert_eq!(answer(PAGE, r#"{"type":"turnstile-error"}"#), None);
        assert_eq!(answer(PAGE, "not json"), None);
        let huge = format!(
            r#"{{"type":"turnstile-timeout","x":"{}"}}"#,
            "a".repeat(MAX_MESSAGE)
        );
        assert_eq!(answer(PAGE, &huge), None);
    }

    #[test]
    fn keeps_to_the_hub() {
        assert!(is_hub(Some("https"), Some(hub::HOST)));
        assert!(!is_hub(Some("http"), Some(hub::HOST)));
        assert!(!is_hub(Some("https"), Some("challenges.cloudflare.com")));
        assert!(!is_hub(Some("https"), None));
    }
}
