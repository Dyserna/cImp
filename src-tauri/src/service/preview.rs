//! The Preview tab's two decisions: may this URL be loaded, and what does the
//! toolbar persist.
//!
//! ## What the A1-3 preview run found
//!
//! Nine commands, and seven of them are the same three lines — look a
//! `tab_id` up in the registry, call one method on the `Webview` it hands
//! back, wrap the error. `Webview` is Tauri's, so a service in front of
//! `hide`/`show`/`navigate`/`set_bounds` would need one trait method per
//! webview method: `Webview` with extra steps, which is exactly what
//! [`WebviewHost`](crate::service::sink::WebviewHost)'s doc refuses. Those
//! seven stay at the boundary, noted there.
//!
//! What is here is the two things that are decisions rather than effects:
//!
//! * [`admit`] — the navigation policy gate, shared by `preview_open` and
//!   `preview_navigate`. The predicate underneath
//!   ([`is_allowed_preview_host`](crate::preview::is_allowed_preview_host)) was
//!   always pure and has thirteen tests; what did NOT have one is the pair of
//!   consequences the commands spell twice each: the refusal's wording, and
//!   whether the URL is handed to the system browser instead. Those two are one
//!   value now.
//! * [`PreviewConfig`] — the toolbar's persistence, which is a settings
//!   mutation with a rule (a removed or non-Preview tab is a no-op, but the
//!   project's "last URL" is remembered either way).

use url::Url;

use crate::error::AppError;
use crate::settings::{SettingsHandle, TabConfig};

/// A URL the Preview tab will not load, and what to do about it.
///
/// `open_externally` is the difference between the two refusals and it is not
/// cosmetic: a URL that is well-formed but outside the localhost/RFC-1918
/// policy is a page the user plainly meant to visit, so it goes to their
/// browser; a URL that will not parse is not a destination at all, and handing
/// it to `ShellExecute` would be handing an unvalidated string to the OS.
#[derive(Debug)]
pub struct Refused {
    pub error: AppError,
    pub open_externally: bool,
}

/// Decide whether `url` may be loaded in a Preview tab's embedded webview, and
/// parse it while we are here.
///
/// The order is load-bearing: the host policy is checked BEFORE the parse, so
/// the refusal a user sees for `http://example.com` is the policy one (with
/// their browser opening) rather than a parse error. `is_allowed_preview_host`
/// does its own parse and rejects anything malformed or hostless, so the
/// `Url::parse` below cannot be reached by a string the policy admitted — it is
/// there to produce the value, not to re-validate.
pub fn admit(url: &str, allow_remote: bool) -> Result<Url, Refused> {
    if !crate::preview::is_allowed_preview_host(url, allow_remote) {
        return Err(Refused {
            error: AppError::Preview(format!(
                "{url} is outside the Preview tab's localhost/RFC-1918 policy; opened in your browser instead"
            )),
            open_externally: true,
        });
    }
    Url::parse(url).map_err(|e| Refused {
        error: AppError::Preview(format!("invalid preview URL {url}: {e}")),
        open_externally: false,
    })
}

/// The Preview toolbar's persistence, over a borrowed settings handle.
pub struct PreviewConfig<'a> {
    settings: &'a SettingsHandle,
}

impl<'a> PreviewConfig<'a> {
    pub fn new(settings: &'a SettingsHandle) -> Self {
        Self { settings }
    }

    /// Persist the toolbar's live `url` / `device_width` / `auto_reload` onto
    /// the tab's `PreviewTabConfig`, so a restart reopens with the same state,
    /// and remember `url` as the project's `preview_last_url` for the next
    /// "New Preview tab".
    ///
    /// A `tab_id` that names a non-Preview or already-removed tab is a no-op
    /// rather than an error — the toolbar saves on teardown, and losing that
    /// race is normal. **`preview_last_url` is written anyway**, deliberately:
    /// it is a project-level memory of where the user was last looking, and the
    /// tab going away is not a reason to forget it. One `mutate`, so a
    /// concurrent settings write cannot clobber either half.
    pub fn remember(
        &self,
        tab_id: String,
        url: String,
        device_width: Option<u32>,
        auto_reload: bool,
    ) {
        self.settings.mutate(move |snap| {
            if let Some(TabConfig::Preview(cfg)) = snap.tabs.iter_mut().find(|t| t.id() == tab_id) {
                cfg.url = url.clone();
                cfg.device_width = device_width;
                cfg.auto_reload = auto_reload;
            }
            snap.preview_last_url = Some(url.clone());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every policy refusal carries the same two consequences**, and they
    /// used to be spelled twice — once in `preview_open`, once in
    /// `preview_navigate`. The wording is what the toolbar shows, and
    /// `open_externally` is what sends the page to the user's browser instead.
    ///
    /// It is `true` for a malformed URL too, which looks surprising and is the
    /// shipped contract: `is_allowed_preview_host` rejects malformed and
    /// hostless URLs along with off-policy ones, and the scheme allowlist in
    /// [`open_external`](crate::preview::open_external) — not this decision —
    /// is what stops `javascript:` reaching `ShellExecute`. Two gates, and this
    /// test pins which one is which so a later "tidy-up" cannot collapse them.
    #[test]
    fn every_policy_refusal_names_the_policy_and_defers_to_the_scheme_gate() {
        let blocked = admit("http://example.com/", false).expect_err("outside the policy");
        assert!(blocked.open_externally, "the user meant to visit this page");
        assert!(
            blocked.error.to_string().contains("localhost/RFC-1918"),
            "the refusal names the policy: {}",
            blocked.error
        );

        for bad in ["not a url", "javascript:alert(1)", "about:blank"] {
            let refused = admit(bad, true).err().unwrap_or_else(|| panic!("{bad}"));
            assert!(
                refused.error.to_string().contains("localhost/RFC-1918"),
                "{bad} is refused by the host policy, not by a parse: {}",
                refused.error
            );
            assert!(
                !crate::preview::is_externally_openable(bad),
                "{bad} must be stopped by the scheme allowlist, which is the gate                  that actually protects the OS opener"
            );
        }
    }

    /// The admitted case yields a parsed URL, and `allow_remote` widens which
    /// hosts qualify without widening what counts as well-formed.
    #[test]
    fn a_local_url_is_admitted_and_allow_remote_widens_only_hosts() {
        let ok = admit("http://localhost:5173/app", false).expect("localhost");
        assert_eq!(ok.host_str(), Some("localhost"));
        assert_eq!(ok.path(), "/app");

        assert!(admit("http://example.com/", false).is_err());
        assert!(admit("http://example.com/", true).is_ok());
        assert!(
            admit("about:blank", true).is_err(),
            "allow_remote never admits a hostless scheme"
        );
    }
}
