//! Who the run is asking about.
//!
//! Two ways in, and they are not interchangeable. [`resolve`] asks Instagram,
//! which is the only way to learn about an account the tool has never seen and
//! the only way to know it is private. [`from_store`] asks the local database,
//! which costs nothing and is therefore the only one allowed when the network
//! is off.

use anyhow::{Result, bail};
use snob_core::Pk;
use snob_core::model::{ListKind, printable};
use snob_core::store::{accounts, users};

use crate::app::App;
use crate::cli::ListArgs;

#[derive(Debug, Clone)]
pub struct Target {
    pub pk: Pk,
    /// As Instagram spells it, or as the store recorded it — never as it was
    /// typed. The upsert that follows writes this name back.
    ///
    /// `None` when it has genuinely never been learned, which happens on your
    /// own account whenever the session was stored without one: logging in
    /// while the account is throttled leaves it unset, and only `whoami` ever
    /// writes a resolved name back.
    ///
    /// It used to be a `String` holding `pk.to_string()` in that case, with a
    /// comment saying it was only a label. It was not: `engine::decide` writes
    /// it straight into `users.username`, so `--cache` clobbered a correct
    /// stored name with the number, filed a rename that never happened, and
    /// then told the user their own account had no stored list — because
    /// `find_pk_by_username` no longer matched the real one.
    pub username: Option<String>,
    pub is_self: bool,
    /// The counters, when resolving already asked for them.
    ///
    /// Resolving a name and polling its counters are the **same request** to
    /// the same endpoint, and doing both meant asking Instagram the identical
    /// question twice in a row — four times for a crossing. Carrying the answer
    /// forward is what makes the second one unnecessary rather than merely
    /// cheap.
    pub counters: Option<Counters>,
}

#[derive(Debug, Clone, Copy)]
pub struct Counters {
    pub followers: Option<u64>,
    pub following: Option<u64>,
}

impl Counters {
    pub fn of(&self, kind: ListKind) -> Option<u64> {
        match kind {
            ListKind::Followers => self.followers,
            ListKind::Following => self.following,
        }
    }
}

/// Strips the at sign people type out of habit. Both spellings mean the same
/// account, and Instagram takes neither with the sign attached.
pub fn clean(typed: &str) -> &str {
    typed.trim_start_matches('@')
}

/// How to name the account a run is about, before anything has resolved it.
///
/// The typed name when there is one, the viewer's own label otherwise. It goes
/// through `printable` because it is drawn on a terminal and `clean` only
/// strips the at sign.
pub fn label(app: &App, args: &ListArgs) -> String {
    match args.target.as_deref() {
        Some(raw) => format!("@{}", snob_core::model::printable(clean(raw))),
        None => app.viewer().label(),
    }
}

/// Resolves against Instagram, spending one request when a name was given.
///
/// A private account the viewer does not follow is refused **here**, before a
/// single page is walked: Instagram serves those lists to followers only, so
/// walking would buy nothing but empty pages.
pub async fn resolve(app: &mut App, args: &ListArgs) -> Result<Target> {
    let Some(typed) = args.target.as_deref() else {
        let viewer = app.viewer().clone();
        let resolved = match viewer.username {
            Some(u) => Some(u),
            None => app.client().resolve_username(viewer.pk).await?,
        };

        return Ok(Target {
            pk: viewer.pk,
            // `None` when it was never learned, rather than the numeric id
            // standing in for it. See the field.
            username: resolved.clone(),
            is_self: true,
            counters: match resolved {
                // Not asked for yet: your own account does not come through
                // the profile endpoint.
                Some(_) => None,
                // Without a real name there is nothing to poll **with** — the
                // profile endpoint takes a username — so saying "the counters
                // are unknown" costs nothing, while asking about a numeric id
                // would spend a request on a guaranteed 404 every single run.
                None => Some(Counters {
                    followers: None,
                    following: None,
                }),
            },
        });
    };

    let profile = app.client().web_profile_info(clean(typed)).await?;
    let is_self = app.viewer().pk == profile.id;

    // Only certainty blocks. This API has no contract, and a missing field must
    // never turn into a refusal: with `None` the walk runs and fails, or does
    // not, on its own terms.
    if !is_self && profile.is_private == Some(true) && profile.followed_by_viewer == Some(false) {
        // Filtered: it came off Instagram, not out of anybody's keyboard, and
        // this sentence is written to a terminal. `pfp.rs` does the same with
        // the same field in the same shape of refusal.
        if profile.requested_by_viewer == Some(true) {
            bail!(
                "@{} is private and your follow request has not been accepted yet, \
                 so its lists cannot be read",
                printable(&profile.username)
            );
        }
        bail!(
            "@{} is a private account you do not follow, so its lists cannot be read",
            printable(&profile.username)
        );
    }

    Ok(Target {
        is_self,
        pk: profile.id,
        // The same answer that named the account also counted it. Asking again
        // would be the identical request to the identical endpoint.
        counters: Some(Counters {
            followers: profile.follower_count(),
            following: profile.following_count(),
        }),
        username: Some(profile.username),
    })
}

/// Resolves from the local store alone, without touching the network.
///
/// A name that was never tracked has no snapshot either, so the refusal reads
/// the same as the cache miss that would have followed it.
pub fn from_store(app: &App, typed: Option<&str>, kind: ListKind) -> Result<Target> {
    let Some(typed) = typed else {
        let viewer = app.viewer();
        return Ok(Target {
            pk: viewer.pk,
            username: viewer.username.clone(),
            is_self: true,
            counters: None,
        });
    };

    let typed = clean(typed);
    let Some(pk) = accounts::find_pk_by_username(app.db().conn(), typed)? else {
        bail!("no snapshot of the {kind} list is stored; drop --cache to fetch it");
    };

    Ok(Target {
        pk,
        username: users::find(app.db().conn(), pk)?
            .map(|u| u.username)
            .filter(|name| !name.is_empty())
            .or_else(|| Some(typed.to_string())),
        is_self: app.viewer().pk == pk,
        // Nothing was asked of Instagram, so there is nothing to carry.
        counters: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_at_sign_is_optional() {
        assert_eq!(clean("@someone"), "someone");
        assert_eq!(clean("someone"), "someone");
        // Only the leading one: it is not part of any username anyway.
        assert_eq!(clean("@@someone"), "someone");
    }
}
