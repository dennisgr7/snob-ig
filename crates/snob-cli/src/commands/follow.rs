//! `snob follow` and `snob unfollow`: the only two commands that change
//! anything on Instagram.
//!
//! Two requests: the profile, to turn a name into an id and to find out what
//! the relationship is already, and the write itself. Both are paid for, the
//! second out of the write budget, and neither happens during a cooldown.
//!
//! **One account per run, and there is no flag that changes it.** The reasoning
//! is in `AGENTS.md` with the rest of the write regime, and the short version is
//! that a burst is what strains a service, not a daily total — so the shape of
//! this command is the safeguard, not a limit inside it. Somebody determined to
//! run it in a loop can write the loop; what this refuses to do is ship one.
//!
//! The other half of the design is that it asks first, because a follow reaches
//! another person in a way a read never does. Unfollowing afterwards does not
//! take back the notification they already got, and follow-then-unfollow churn
//! is a growth-hacking trick rather than the housekeeping this tool is for. So
//! the question is asked before the request, and `-y` is how a script answers it
//! in advance.

use anyhow::{Result, anyhow};
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_ig::graphql::DocIds;
use snob_ig::model::FriendshipStatus;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::FollowArgs;
use crate::commands::common;
use crate::engine::target;
use crate::exit::{ExitCode, ExitError};
use crate::ui;

/// Which of the two verbs is being run.
///
/// Not a boolean. Every sentence this command prints differs between the two,
/// and a boolean named `unfollow` reads backwards at exactly the call sites
/// where getting it backwards is worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Follow,
    Unfollow,
}

impl Verb {
    fn present(self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::Unfollow => "unfollow",
        }
    }
}

pub async fn run(
    args: FollowArgs,
    verb: Verb,
    secrets: SecretStore,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // No progress bar: two requests, and the second one may sit on the write
    // budget for a quarter of an hour, which is a wait to be told about in a
    // sentence rather than animated.
    let app = common::app(&secrets, paths, false)?;

    // **Before anything is spent.** A session that cannot write is the common
    // case rather than the odd one — `snob login --paste` produces one unless
    // `--csrftoken` was given — and finding out from Instagram's 403 would cost
    // a request, a slot of write budget, and a message telling the user their
    // session had expired when it had not.
    if app.client().session().csrftoken.is_none() {
        return Err(ExitError::new(
            ExitCode::NoSession,
            format!(
                "this session can read but not {}: it has no CSRF token",
                verb.present()
            ),
        )
        .with_hint(
            "run \"snob login --browser\", which captures it, or \
             \"snob login --paste --csrftoken <token>\"",
        )
        .into());
    }

    common::refuse_during_cooldown(&app, "nothing can be sent")?;

    // **Also before anything is spent.** Whether anybody can answer is a
    // property of this process's streams, not of the account, so it is known
    // now — and a run that is going to refuse for want of an answer should not
    // pay for a lookup first. Found by running it: under a pipe it resolved the
    // profile and then said there was nobody to ask.
    //
    // The question itself still waits until after the lookup, deliberately: it
    // names the account the way Instagram spells it, it says "send a follow
    // request" rather than "follow" for a private account, and it is not asked
    // at all when the relationship already holds. All three need the profile.
    if !args.yes && !ui::can_be_asked() {
        return Err(ExitError::new(
            ExitCode::Interrupted,
            format!(
                "nothing was {}ed: there is nobody to confirm it",
                verb.present()
            ),
        )
        .with_hint("pass -y to confirm in advance")
        .into());
    }

    let profile = app
        .client()
        .web_profile_info(target::clean(&args.target))
        .await?;

    // Instagram spells the name; the user typed a spelling of it. Everything
    // from here on uses Instagram's, filtered, because it is going to a
    // terminal and it came off a server.
    let name = printable(&profile.username);

    // Refusing to follow yourself here rather than letting Instagram do it:
    // its answer is a generic 400 that `classify` cannot tell apart from a
    // real failure, and the user would be told the account is throttled.
    if Some(profile.id) == Some(app.client().session().ds_user_id) {
        return Err(anyhow!("you cannot {} your own account", verb.present()));
    }

    // Nothing to do is worth saying rather than doing. It also saves the write
    // budget for a write that changes something, which matters when a slot is
    // fifteen minutes.
    if let Some(settled) = already_done(verb, &profile, &name) {
        ui::info(&settled);
        return Ok(ExitCode::Ok);
    }

    let question = match verb {
        Verb::Follow if profile.is_private == Some(true) => {
            format!("Send @{name} a follow request?")
        }
        Verb::Follow => format!("Follow @{name}?"),
        Verb::Unfollow => format!("Unfollow @{name}?"),
    };

    // `confirm` answers with its default the moment nobody can answer, which
    // would turn "nobody was there" into "they said no" — two different events
    // that were reported as one everywhere else in this tool until it was
    // fixed. The guard above is what stops that being reachable here, so this
    // branch only ever runs with somebody at the keyboard.
    if !args.yes && !ui::confirm(&question, false)? {
        return Ok(ExitCode::Interrupted);
    }

    let ids = RememberedIds { db: app.db() };
    let status = send(app.client(), verb, profile.id, &profile.username, &ids).await?;
    ui::info(&outcome(verb, &status, &name));
    Ok(ExitCode::Ok)
}

/// What the profile already says, when it says the request would change
/// nothing.
///
/// `followed_by_viewer` and `requested_by_viewer` are `Option` because the
/// endpoint omits them on some answers, and `None` means unknown. Unknown must
/// send the request rather than assume: refusing on a missing field would make
/// the command stop working the day Instagram drops it from the response.
fn already_done(
    verb: Verb,
    profile: &snob_ig::model::WebProfileInfo,
    name: &str,
) -> Option<String> {
    match verb {
        Verb::Follow if profile.followed_by_viewer == Some(true) => {
            Some(format!("You already follow @{name}."))
        }
        Verb::Follow if profile.requested_by_viewer == Some(true) => Some(format!(
            "You have already asked to follow @{name}, and they have not answered yet."
        )),
        Verb::Unfollow
            if profile.followed_by_viewer == Some(false)
                && profile.requested_by_viewer != Some(true) =>
        {
            Some(format!("You do not follow @{name}."))
        }
        _ => None,
    }
}

/// The network half, kept apart from the session, the question and the
/// terminal so a test can drive it against a mock server.
async fn send(
    client: &IgClient,
    verb: Verb,
    pk: snob_core::Pk,
    username: &str,
    ids: &dyn DocIds,
) -> Result<FriendshipStatus> {
    Ok(match verb {
        Verb::Follow => client.follow(pk, username, ids).await?,
        Verb::Unfollow => client.unfollow(pk, username, ids).await?,
    })
}

/// The mutation ids, remembered in the `meta` table between runs.
///
/// **Discovery is what makes a rotated id fix itself, and caching is what makes
/// discovery affordable.** Finding one means fetching JavaScript bundles until
/// a mutation turns up in one, and those are megabytes; once per rotation is
/// nothing, once per follow would be absurd.
///
/// Failures are swallowed on both sides, and the same reasoning applies to
/// each: a read that fails means discovery runs, which is slow and correct, and
/// a write that fails means the next run discovers again. Neither is worth
/// failing a follow over, and neither is worth a message the user cannot act
/// on.
struct RememberedIds<'a> {
    db: &'a snob_store::store::Store,
}

impl DocIds for RememberedIds<'_> {
    fn get(&self, friendly_name: &str) -> Option<String> {
        self.db.remembered(&key(friendly_name)).ok().flatten()
    }

    fn put(&self, friendly_name: &str, doc_id: &str) {
        if let Err(e) = self.db.remember(&key(friendly_name), doc_id) {
            tracing::debug!(error = %e, "the mutation id could not be remembered");
        }
    }
}

/// Namespaced, because `meta` holds facts about the database and these are
/// facts about Instagram. Keyed by the mutation's own name so that a renamed
/// operation gets a new key rather than a stale value under an old one.
fn key(friendly_name: &str) -> String {
    format!("graphql.doc_id.{friendly_name}")
}

/// What actually happened, as Instagram describes it afterwards.
///
/// **Following a private account does not follow it — it asks.** Saying
/// "Followed" there would be the command lying about the one thing it was run
/// to do, and the user would go looking for stories that will not be there.
fn outcome(verb: Verb, status: &FriendshipStatus, name: &str) -> String {
    match verb {
        Verb::Follow if status.following => format!("Now following @{name}."),
        Verb::Follow if status.outgoing_request => {
            format!("Asked to follow @{name}. They have to accept it.")
        }
        // Instagram answered, and answered that nothing is different. Better
        // said plainly than dressed up as success.
        Verb::Follow => {
            format!("Instagram accepted the request, but @{name} is still not followed.")
        }
        Verb::Unfollow if !status.following && !status.outgoing_request => {
            format!("No longer following @{name}.")
        }
        Verb::Unfollow => format!("Instagram accepted the request, but @{name} is still followed."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Pk;
    use snob_ig::model::WebProfileInfo;

    fn profile(followed: Option<bool>, requested: Option<bool>) -> WebProfileInfo {
        WebProfileInfo {
            id: Pk::new(7),
            username: "someone".into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            followed_by_viewer: followed,
            requested_by_viewer: requested,
            profile_pic_url: None,
            profile_pic_url_hd: None,
            followers: None,
            following: None,
            follows_viewer: None,
            has_requested_viewer: None,
            biography: None,
            external_url: None,
            posts: None,
            mutual: None,
            highlight_reel_count: None,
            is_business_account: None,
            category_name: None,
            // The route that answered. Search is the fallback for the accounts
            // `web_profile_info` cannot serialize; these fixtures are the
            // ordinary case.
            via: snob_ig::model::Via::Profile,
        }
    }

    #[test]
    fn a_relationship_that_already_holds_costs_no_request() {
        assert!(already_done(Verb::Follow, &profile(Some(true), None), "x").is_some());
        assert!(already_done(Verb::Unfollow, &profile(Some(false), None), "x").is_some());
        assert!(already_done(Verb::Follow, &profile(Some(false), Some(true)), "x").is_some());
    }

    /// The endpoint omits these fields on some answers. Unknown has to mean
    /// "send it and find out", or the command stops working the day Instagram
    /// drops the field.
    #[test]
    fn an_unknown_relationship_is_not_treated_as_settled() {
        assert!(already_done(Verb::Follow, &profile(None, None), "x").is_none());
        assert!(already_done(Verb::Unfollow, &profile(None, None), "x").is_none());
    }

    /// A pending request is not a follow, and unfollow has something to do
    /// about it: withdrawing the request.
    #[test]
    fn a_pending_request_is_still_something_to_withdraw() {
        assert!(already_done(Verb::Unfollow, &profile(Some(false), Some(true)), "x").is_none());
    }

    #[test]
    fn following_a_private_account_is_reported_as_the_request_it_is() {
        let asked = FriendshipStatus {
            following: false,
            outgoing_request: true,
            ..Default::default()
        };
        let said = outcome(Verb::Follow, &asked, "someone");
        assert!(said.contains("Asked to follow"), "{said}");
        assert!(!said.contains("Now following"), "{said}");
    }

    #[test]
    fn a_write_instagram_accepted_but_did_not_apply_is_not_called_a_success() {
        let unchanged = FriendshipStatus::default();
        let said = outcome(Verb::Follow, &unchanged, "someone");
        assert!(said.contains("still not followed"), "{said}");
    }
}
