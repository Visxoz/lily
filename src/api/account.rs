use rustypipe::client::ClientType;

use super::{Api, Result, err};

impl Api {
    pub async fn sign_in(&self, cookie: &str) -> Result<()> {
        let cookie = cookie.trim();
        if cookie.contains('\t') {
            self.rp.user_auth_set_cookie_txt(cookie).await.map_err(err)
        } else {
            let cookie = cookie.strip_prefix("Cookie:").unwrap_or(cookie).trim();
            self.rp.user_auth_set_cookie(cookie).await.map_err(err)
        }
    }

    pub async fn sign_out(&self) {
        let _ = self.rp.user_auth_remove_cookie().await;
    }

    /// The guide has the current picture, while the account menu can keep
    /// serving the one before a change, so the menu is only a fallback.
    pub async fn account_photo(&self) -> Result<Option<String>> {
        let ask = async |client, endpoint: &str| {
            self.q()
                .authenticated()
                .raw(client, endpoint, &serde_json::json!({}))
                .await
        };
        match ask(ClientType::Desktop, "guide").await {
            Ok(guide) => match account_photo(&guide, GUIDE_PHOTO) {
                Some(url) => return Ok(Some(url)),
                None => log::debug!("the guide has no account picture"),
            },
            Err(e) => log::debug!("the guide failed: {e}"),
        }
        let menu = ask(ClientType::DesktopMusic, "account/account_menu")
            .await
            .map_err(err)?;
        Ok(account_photo(&menu, MENU_PHOTO))
    }

    /// Whether a sign-in cookie is stored, without asking YouTube.
    pub fn has_cookie(&self) -> bool {
        self.q().auth_enabled(ClientType::DesktopMusic)
    }

    /// The stored session's `Cookie` header, kept current by rustypipe's
    /// signed-in responses and by `rotate_cookies`.
    pub fn cookie(&self) -> Option<String> {
        self.rp.user_auth_cookie_header()
    }

    /// Whether the stored sign-in asks as its login's primary identity, as
    /// the Premium path's requests do (`X-Goog-AuthUser: 0`, no page ID).
    pub fn signs_in_as_primary(&self) -> bool {
        self.rp.user_auth_is_primary()
    }

    /// Asks Google for fresh session cookies, as an open YouTube tab does:
    /// nothing else renews the app's own session. `false` when the stored
    /// login is no longer the one renewed, as after signing out or in meanwhile.
    pub async fn rotate_cookies(&self) -> Result<bool> {
        let Some(cookie) = self.cookie() else {
            return Err("not signed in".into());
        };
        let response = crate::http::client()
            .post(ROTATE_URL)
            .header(
                reqwest::header::USER_AGENT,
                crate::streams::premium::WEB_USER_AGENT,
            )
            .header(reqwest::header::ORIGIN, ROTATE_ORIGIN)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::COOKIE, &cookie)
            .body(ROTATE_BODY)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| err(e.without_url()))?;
        // Only youtube.com may set youtube.com's cookies, should it redirect.
        if !crate::http::within(response.url(), &["youtube.com"]) {
            return Err("the renewal was redirected off youtube.com".into());
        }
        let set_cookie = response.headers().get_all(reqwest::header::SET_COOKIE);
        let set_cookie = set_cookie.iter().filter_map(|v| v.to_str().ok());
        if self.rp.user_auth_take_cookies(&cookie, set_cookie) {
            log::info!("renewed the sign-in");
        }
        self.save_session().await;
        // A 200 that set nothing new still renews, so the login is compared.
        Ok(same_login(&cookie, self.cookie().as_deref()))
    }

    /// Saves the session's cookie with what responses have changed in it.
    pub async fn save_session(&self) {
        self.rp.store_cache().await;
    }

    /// Waits for YouTube to answer a request sent with the cookie as signed
    /// out, as it does once the session has ended.
    pub async fn sign_in_lost(&self) {
        self.rp.user_auth_lost().await;
    }

    /// Checks the stored cookie with YouTube, which costs a page load.
    /// `None` when YouTube could not be asked.
    pub async fn cookie_valid(&self) -> Option<bool> {
        match self.rp.user_auth_check_cookie().await {
            Ok(()) => Some(true),
            Err(rustypipe::error::Error::Auth(_)) => Some(false),
            Err(e) => {
                log::warn!("could not check the sign-in: {e}");
                None
            }
        }
    }
}

const ROTATE_URL: &str = "https://accounts.youtube.com/RotateCookies";
/// YouTube calls it from a frame on accounts.youtube.com.
const ROTATE_ORIGIN: &str = "https://accounts.youtube.com";
const ROTATE_BODY: &str = r#"[000,"-0000000000000000000"]"#;

/// Whether the cookie `stored` now signs in as the login `sent` named.
fn same_login(sent: &str, stored: Option<&str>) -> bool {
    use crate::streams::premium::sapisid;
    sapisid(sent).is_some_and(|login| stored.and_then(sapisid) == Some(login))
}

const GUIDE_PHOTO: &str = "/userProfile/accountPhoto/thumbnails";
const MENU_PHOTO: &str = "/actions/0/openPopupAction/popup/multiPageMenuRenderer/header/activeAccountHeaderRenderer/accountPhoto/thumbnails";

/// The largest profile picture at `pointer` in a response, as a JPEG: an
/// animated picture otherwise comes as a GIF, which artwork isn't decoded from.
fn account_photo(json: &str, pointer: &str) -> Option<String> {
    let response: serde_json::Value = serde_json::from_str(json).ok()?;
    let url = response
        .pointer(pointer)?
        .as_array()?
        .iter()
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0))?["url"]
        .as_str()?;
    Some(if url.contains('=') && !url.ends_with("-rj") {
        format!("{url}-rj")
    } else {
        url.to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_account_photo() {
        let menu = r#"{"actions":[{"openPopupAction":{"popup":{"multiPageMenuRenderer":{"header":
            {"activeAccountHeaderRenderer":{"accountName":{"runs":[{"text":"Alex"}]},
            "accountPhoto":{"thumbnails":[
                {"url":"https://yt3.ggpht.com/a=s88","width":88,"height":88},
                {"url":"https://yt3.ggpht.com/a=s176","width":176,"height":176}]}}}}}}}]}"#;
        assert_eq!(
            account_photo(menu, MENU_PHOTO).as_deref(),
            Some("https://yt3.ggpht.com/a=s176-rj")
        );
        let guide = r#"{"items":[],"userProfile":{"accountPhoto":{"thumbnails":[
            {"url":"https://yt3.ggpht.com/yti/b=s88-c-k-no-rj"}]}}}"#;
        assert_eq!(
            account_photo(guide, GUIDE_PHOTO).as_deref(),
            Some("https://yt3.ggpht.com/yti/b=s88-c-k-no-rj")
        );
        assert_eq!(account_photo(menu, GUIDE_PHOTO), None);
        assert_eq!(account_photo("{}", MENU_PHOTO), None);
        assert_eq!(account_photo("not json", MENU_PHOTO), None);
    }

    #[test]
    fn notices_answers_served_signed_out() {
        use rustypipe::client::answered_signed_out;
        let context = |logged_in| {
            format!(
                r#"{{"responseContext":{{"serviceTrackingParams":[{{"service":"GFEEDBACK",
                "params":[{{"key":"browse_id","value":"FEmusic_home"}},
                {{"key":"logged_in","value":"{logged_in}"}}]}}]}}}}"#
            )
        };
        assert!(answered_signed_out(&context("0")));
        assert!(!answered_signed_out(&context("1")));
        assert!(!answered_signed_out("{}"));
    }

    #[test]
    fn tells_the_primary_identity_from_others() {
        let dir = std::env::temp_dir().join(format!("lily-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let primary = |cache: &str| {
            crate::platform::secret::write(&dir.join("rustypipe_cache.bin"), cache).unwrap();
            Api::in_dir(dir.clone(), None)
                .unwrap()
                .signs_in_as_primary()
        };
        let signed_in = |auth: &str| {
            primary(&format!(
                r#"{{"auth_cookie":{{"cookie":"SAPISID=a"{auth}}}}}"#
            ))
        };
        assert!(signed_in(""));
        assert!(signed_in(r#","session_index":"0""#));
        assert!(
            !signed_in(r#","session_index":"1""#),
            "another login in the jar"
        );
        assert!(
            !signed_in(r#","session_index":"0","channel_syncid":"x""#),
            "a brand channel"
        );
        assert!(!primary("{}"), "signed out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renewal_counts_only_for_the_login_still_stored() {
        let sent = "SID=a; SAPISID=one";
        assert!(same_login(sent, Some("SID=b; SAPISID=one")), "rotated");
        assert!(
            !same_login(sent, Some("SID=a; SAPISID=two")),
            "signed in again"
        );
        assert!(!same_login(sent, None), "signed out");
        assert!(!same_login("SID=a", Some("SID=a")), "no login named");
    }

    #[test]
    fn takes_renewed_cookies_but_not_deletions() {
        use rustypipe::client::merge_set_cookies;
        let cookie = "SID=a; __Secure-1PSIDTS=old; SAPISID=s";
        let merged = merge_set_cookies(
            cookie,
            [
                "__Secure-1PSIDTS=new; Domain=.youtube.com; Path=/; Secure; HttpOnly",
                "SIDCC=c; expires=Fri, 19-Nov-2099 10:00:00 GMT; path=/; domain=youtube.com",
                "SID=EXPIRED; Domain=.youtube.com; Expires=Thu, 01-Jan-1970 00:00:10 GMT",
                "SAPISID=gone; Domain=.youtube.com; Max-Age=0",
                "YSC=host-only; Path=/",
                "NID=z; Domain=.google.com",
            ],
        );
        assert_eq!(
            merged.as_deref(),
            Some("SID=a; __Secure-1PSIDTS=new; SAPISID=s; SIDCC=c")
        );
        assert_eq!(
            merge_set_cookies(cookie, ["SID=a; Domain=.youtube.com"]),
            None,
            "nothing changed"
        );
        assert_eq!(
            merge_set_cookies(cookie, ["SID=; Domain=.youtube.com"]),
            None
        );
    }
}
