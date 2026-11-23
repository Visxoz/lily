//! Work run on Tokio for the UI thread, and what `done` is handed should it
//! panic, so a page shows its error rather than waiting.

use std::{future::Future, rc::Rc};

use tokio::task::AbortHandle;

use super::App;
use crate::api::{self, Page};

impl App {
    /// Runs `work` on Tokio and `done` with its result on the UI thread;
    /// aborted, neither carries on.
    pub(super) fn spawn<T: Outcome + Send + 'static>(
        self: &Rc<Self>,
        work: impl Future<Output = T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) -> AbortHandle {
        let handle = self.rt.spawn(work);
        let abort = handle.abort_handle();
        let app = Rc::clone(self);
        let _ = slint::spawn_local(async move {
            if let Some(value) = outcome(handle.await) {
                done(&app, value);
            }
        });
        abort
    }
}

/// What `App::spawn`'s `done` is given should its work panic; none skips it.
pub(super) trait Outcome: Sized {
    fn panicked() -> Option<Self> {
        None
    }
}

/// A failed request, so a page shows its error rather than waiting.
impl<T> Outcome for api::Result<T> {
    fn panicked() -> Option<Self> {
        Some(Err("something went wrong".to_owned()))
    }
}

/// The personal Home's failure, which falls back to the charts.
impl Outcome for Result<Page, Option<Vec<api::Section>>> {
    fn panicked() -> Option<Self> {
        Some(Err(None))
    }
}

impl<T> Outcome for Result<T, crate::lastfm::Error> {
    fn panicked() -> Option<Self> {
        Some(Err(crate::lastfm::Error::Failed(
            "something went wrong".to_owned(),
        )))
    }
}

/// No answer, so the sign-in check is armed again.
impl<T> Outcome for Option<T> {
    fn panicked() -> Option<Self> {
        Some(None)
    }
}

// Nothing to fail with, so these skip `done`. A `spawn_blocking` handle's
// panic already arrives as its `Err`.
impl Outcome for () {}
impl<T> Outcome for Vec<T> {}
impl<A, B> Outcome for (A, B) {}
impl<T> Outcome for Result<T, tokio::task::JoinError> {}

/// The panic is already logged by the hook; a task cancelled at shutdown ends
/// nothing.
fn outcome<T: Outcome>(joined: Result<T, tokio::task::JoinError>) -> Option<T> {
    match joined {
        Ok(value) => Some(value),
        Err(e) if e.is_panic() => T::panicked(),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_request_fails_as_a_failed_one_would() {
        fn fails() -> api::Result<u8> {
            panic!("a test panic")
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let joined = rt.block_on(rt.spawn(async { fails() }));
        assert_eq!(
            outcome(joined),
            Some(Err("something went wrong".to_owned()))
        );
        let joined = rt.block_on(rt.spawn(async { panic!("a test panic") }));
        assert_eq!(outcome::<()>(joined), None, "nothing to fail with");
        let cancelled = rt.spawn(std::future::pending::<api::Result<u8>>());
        cancelled.abort();
        assert_eq!(
            outcome(rt.block_on(cancelled)),
            None,
            "cancelled, not failed"
        );
    }
}
