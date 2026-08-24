//! Which accounts a run watches, and on whose say-so.
//!
//! One answer, for both modes and for `super::status`, which is what a file of
//! its own is for here: `once` built its own set from the command line for a
//! while, and the two disagreed about the accounts the file names.

use snob_store::config::WatchConfig;

use crate::engine::watch::Watched;

/// Which account a scheduled run watches, and whether it may.
///
/// A name on the command line is checked against the file, because that is the
/// only place a consent can have been recorded — and an unattended run that
/// could be pointed at a stranger by an argument would make the recording
/// pointless.
pub(super) fn watched_from(
    target: Option<String>,
    configured: Option<&WatchConfig>,
) -> Vec<Watched> {
    if let Some(name) = target {
        return vec![with_recorded_consent(&name, configured)];
    }

    let listed: Vec<Watched> = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .map(|account| {
            if account.is_own() {
                Watched::own()
            } else {
                with_recorded_consent(&account.target, configured)
            }
        })
        .collect();

    // A file with no `[[account]]` at all means the obvious thing rather than
    // nothing: somebody who configured a schedule and a webhook and never
    // mentioned an account meant their own.
    if listed.is_empty() {
        vec![Watched::own()]
    } else {
        listed
    }
}

/// One account, with whatever answer is on record for it.
///
/// The file is the only place a consent can have come from, which is what makes
/// an unattended run safe: an argument cannot grant one.
///
/// **The at sign comes off here, on both sides**, which makes this the boundary
/// a `Watched` is built at and every reader downstream of it. `Watched::target`
/// used to carry the string exactly as typed or configured while every other
/// reader in the tool cleaned — `target::resolve`, `target::from_store`,
/// `target::label` — and the two that did not are both load-bearing. Typing
/// `snob watch "@friend"` against a file recording an answer for `friend`
/// matched nothing, so a correctly consented monitor refused at startup citing
/// a consent written in the file it had just read, and `refuse_unattended`
/// printed the doubled `@@friend` that gives it away. A hand-edited `target = "@friend"`
/// went the other way and reached `engine::check`, which asked Instagram for
/// `username=@friend` and reported a working configuration as broken. README.md
/// promises without qualification that a username may be written either way, so
/// this is that promise being kept rather than a special case.
fn with_recorded_consent(name: &str, configured: Option<&WatchConfig>) -> Watched {
    let name = crate::engine::target::clean(name);
    let recorded = configured
        .into_iter()
        .flat_map(|c| c.accounts.iter())
        .find(|account| {
            !account.is_own()
                && crate::engine::target::clean(&account.target).eq_ignore_ascii_case(name)
        })
        .and_then(|account| account.consent);

    if recorded.is_some() {
        // The table is what matters, not what is in it. `[account.consent]`
        // exists because somebody was asked; its `agreed_at` is the record of
        // when, kept in the file, and nothing downstream of here reads it or
        // could tell a real moment from whatever a hand-edit wrote.
        Watched::consented(name.to_string(), crate::engine::watch::Consent)
    } else {
        Watched::asking(name.to_string())
    }
}

/// What the opening line names, so somebody starting the service can see that
/// it understood which accounts it is for.
pub(super) fn watching_label(watched: &[Watched]) -> String {
    let names: Vec<String> = watched
        .iter()
        .map(|w| crate::app::target_label(w.name()))
        .collect();

    match names.len() {
        0 => "nothing".to_string(),
        1 => names[0].clone(),
        _ => {
            let (last, rest) = names.split_last().expect("more than one");
            format!("{} and {last}", rest.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::fixtures::watch_toml;

    /// A stranger is asked unless the file records that somebody answered.
    ///
    /// `with_recorded_consent` is the only thing standing between `--target`
    /// and a scheduled run enumerating somebody else's lists: make its `None`
    /// arm hand back a `Watched::consented` and an unattended run reads a
    /// stranger on nobody's say-so. Nothing went through it. The test that
    /// looks like it covers this asks `may_run_unattended` of three values
    /// built by hand, so it never reaches the function that decides which of
    /// the three you get.
    #[test]
    fn a_stranger_is_asked_unless_the_file_says_somebody_answered() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "self"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }

[[account]]
target = "acquaintance"
"#,
        );

        let asked = |name: &str, config: Option<&WatchConfig>| {
            let watched = watched_from(Some(name.to_string()), config);
            assert_eq!(watched.len(), 1);
            !watched[0].may_run_unattended()
        };

        assert!(
            !asked("friend", Some(&file)),
            "the file records an answer for them"
        );
        assert!(
            asked("stranger", Some(&file)),
            "nobody ever agreed to this one being read"
        );
        assert!(
            asked("acquaintance", Some(&file)),
            "listed is not the same as consented -- the answer is the `consent` table"
        );
        assert!(
            asked("friend", None),
            "with no file there is nowhere an answer could have been recorded"
        );

        // Instagram's spelling and the typed one need not agree in case.
        assert!(!asked("FRIEND", Some(&file)));

        // `self` on the command line is not the `[[account]] target = "self"`
        // line: that one is your own account, which needs nobody's permission,
        // and matching it would hand a stranger named `self` a consent.
        assert!(asked("self", Some(&file)));
    }

    /// The at sign a person types does not change which account is watched.
    ///
    /// Both spellings mean the same account and README.md says so without
    /// qualification, but `Watched` was built from the raw string. Typing
    /// `snob watch "@friend"` against a file recording an answer for
    /// `friend` found no answer, so a correctly configured, correctly consented
    /// service died at startup quoting a consent that is written in the file it
    /// had just read -- and it died before the first tick, so it looked like a
    /// configuration error rather than a spelling one. The name also traveled
    /// on: `engine::check` sent it as `username=@friend` and called a working
    /// monitor broken.
    #[test]
    fn an_at_sign_does_not_change_which_account_is_watched() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }
"#,
        );

        let typed = watched_from(Some("@friend".to_string()), Some(&file));
        assert_eq!(typed.len(), 1);
        assert_eq!(
            typed[0].name(),
            Some("friend"),
            "what reaches the profile endpoint is a username, and the sign is not part of one"
        );
        assert!(
            typed[0].may_run_unattended(),
            "the file records an answer for this account, however it was spelled"
        );

        // And from the other side: the file is documented as safe to hand-edit,
        // so the sign can be in it instead.
        let edited = watch_toml(
            r#"
every = "6h"

[[account]]
target = "@friend"
consent = { agreed_at = 1700 }
"#,
        );
        let listed = watched_from(None, Some(&edited));
        assert_eq!(
            listed.iter().map(|w| w.name()).collect::<Vec<_>>(),
            vec![Some("friend")]
        );
        assert!(listed[0].may_run_unattended());

        // `@self` is the same line as `self`, and it is your own account. Read
        // as a stranger it would send a scheduled run looking for confirmation
        // to read an account it owns.
        let own = watch_toml("every = \"6h\"\n\n[[account]]\ntarget = \"@self\"\n");
        let listed = watched_from(None, Some(&own));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name(), None, "your own account names nobody");
        assert!(listed[0].may_run_unattended());
    }

    /// Every account the file lists is watched, and a file that lists none
    /// means your own.
    #[test]
    fn the_accounts_watched_are_the_ones_the_file_names() {
        let file = watch_toml(
            r#"
every = "6h"

[[account]]
target = "self"

[[account]]
target = "friend"
consent = { agreed_at = 1700 }
"#,
        );

        let watched = watched_from(None, Some(&file));
        let names: Vec<Option<&str>> = watched.iter().map(|w| w.name()).collect();
        assert_eq!(names, [None, Some("friend")]);
        assert!(watched.iter().all(|w| w.may_run_unattended()));

        // A schedule with no `[[account]]` at all means the obvious thing.
        let bare = watch_toml("every = \"6h\"\n");
        let watched = watched_from(None, Some(&bare));
        assert_eq!(watched.len(), 1);
        assert_eq!(watched[0].name(), None);
    }
}
