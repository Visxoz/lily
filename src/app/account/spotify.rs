//! Signing in to Spotify, whose web player's session finds each song's
//! Canvas for the full player.

use std::rc::Rc;

use super::super::App;
use crate::google_sign_in;

impl App {
    pub(in crate::app) fn sign_in_to_spotify(self: &Rc<Self>) {
        self.open_sign_in(&google_sign_in::SPOTIFY);
    }

    /// Keeps the session once Spotify takes it as signed in.
    pub(in crate::app) fn spotify_signed_in(self: &Rc<Self>, cookie: String) {
        let motion = self.motion.clone();
        self.spawn(
            async move { motion.sign_in_to_spotify(&cookie).await },
            |app, result| {
                app.ui()
                    .set_spotify_signed_in(app.motion.spotify_signed_in());
                match result {
                    Ok(()) => {
                        app.toast("Signed in to Spotify");
                        app.refresh_motion();
                    }
                    Err(e) => app.toast(format!("Couldn't sign in to Spotify: {e}")),
                }
            },
        );
    }

    /// Also cancels a sign-in under way.
    pub(in crate::app) fn sign_out_of_spotify(self: &Rc<Self>) {
        let open = self
            .sign_in_window
            .borrow()
            .as_ref()
            .is_some_and(|open| !open.site().is_google());
        if open {
            self.close_sign_in();
        }
        if self.motion.spotify_signed_in() {
            self.motion.sign_out_of_spotify();
            self.ui().set_spotify_signed_in(false);
            self.toast("Signed out of Spotify");
            self.refresh_motion();
        }
    }
}
