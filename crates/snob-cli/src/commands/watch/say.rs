//! The sentences a person reads.
//!
//! The other half of a report from `super::wire`, and the split is by audience
//! rather than by layer: that module decides what a receiver is sent, this one
//! decides what is printed. Rewording anything here is free; rewording anything
//! there is an integration change.
//!
//! What actually moved is `crate::engine::watch`'s answer and nothing in here
//! recomputes any of it — the module rule that `engine` never decides how
//! something looks has a matching half, which is that this never decides what
//! is true.

use snob_core::model::{ListKind, User, printable};
use snob_core::watch::{Basis, ListDiff};

use crate::engine::Provenance;
use crate::engine::watch::{Skipped, WatchReport};
use crate::report;

/// Why a list was not compared, said in a sentence.
///
/// `engine` handed over what happened and nothing else; which words that
/// deserves is this module's question, which is why the tokens are matched
/// here rather than carried as strings.
pub(super) fn refusal_line(kind: ListKind, skipped: Skipped) -> String {
    match skipped {
        Skipped::NobodyLooked(provenance) => format!(
            "the {kind} list was served from storage and nothing checked whether it is still \
             true{}, so it was not compared and the monitor did not move on",
            match provenance {
                Provenance::Cooldown => " (the account is in cooldown)",
                Provenance::PollFailed => " (the check failed)",
                _ => "",
            }
        ),
        // The same half-sentence the summary uses for the same situation.
        // Writing a second one here is how two commands end up describing one
        // event in two ways.
        Skipped::Incomplete(reason, _) => format!(
            "the {kind} list could not be read in full ({}), so it was not compared: the \
             accounts missing from it would have been reported as people who left",
            report::why_incomplete(reason).unwrap_or("it stopped early")
        ),
    }
}

/// The answer as a person reads it.
///
/// Says that reports were abandoned, because nothing else will.
///
/// The sweep marks a report `expired` once it is too old to be news, and that
/// is the moment a set of arrivals and departures stops existing: `due` has
/// already been refusing to hand it back, so the retry ladder never reaches the
/// sentence in `send_one` that was written for exactly this. Until this line,
/// the whole event was a row changing state in silence -- the run printed
/// nothing, `status` counts only `pending` and so showed nothing, and the
/// health verdict went from `warning` to `ok` at the instant the news was lost.
pub(super) fn say_what_was_given_up(given_up: usize) {
    if given_up == 0 {
        return;
    }
    let (subject, what) = if given_up == 1 {
        ("report was", "what it said is")
    } else {
        ("reports were", "what they said is")
    };
    eprintln!(
        "warning: {given_up} {subject} given up on for being too old to be news; \
         {what} not reported a second time."
    );
}

/// Returned as lines rather than printed, so a test can read them without
/// capturing standard output.
pub(super) fn describe(report: &WatchReport, refused: bool) -> Vec<String> {
    let who = crate::app::label(report.account_pk, report.username.as_deref());

    if !report.has_anything_stored() {
        // A run in which every list was refused concluded nothing, and that is
        // not the same as an account nothing has ever been walked for. It used
        // to print "Nothing has been walked for @me yet -- run \"snob
        // followers\" once" two lines above the warnings saying both lists had
        // just been served from storage during a cooldown, with two complete
        // captures on disk. The reason comes from the refusal lines the caller
        // prints next, so this only has to stop claiming the opposite.
        if refused {
            return vec![format!("Nothing could be looked at for {who} this time.")];
        }
        return vec![format!(
            "Nothing has been walked for {who} yet, so there is nothing to compare.\n\
             Run \"snob followers\" once and this will have something to say from then on."
        )];
    }

    // Said before anything else, because everything after it would otherwise
    // read as "nothing happened" when what it means is "this is the first look".
    let baselines: Vec<ListKind> = [report.followers.as_ref(), report.following.as_ref()]
        .into_iter()
        .flatten()
        .filter(|r| matches!(r.basis, Basis::Baseline { .. }))
        .map(|r| r.kind)
        .collect();

    let mut lines = Vec::new();
    if !baselines.is_empty() {
        let which = baselines
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(" and ");
        // Both lists are the common case — a first look almost always finds
        // two — so the sentence has to read for two as well as for one.
        let (noun, verb) = if baselines.len() > 1 {
            ("lists have", "them")
        } else {
            ("list has", "it")
        };
        lines.push(format!(
            "The {which} {noun} never been reported on, so there is no earlier capture to \
             compare {verb} against. The next run is the first that can say anything."
        ));
    }

    let changes = report.changes();
    if changes.is_empty() {
        if baselines.is_empty() {
            // **Say what was done, rather than asserting the negative.** Three
            // different runs reached this one sentence and only one of them had
            // earned it.
            //
            // A list whose counter had not moved is served from storage without
            // being read, which is where nearly all of this tool's savings come
            // from and is worth keeping — walking three hundred accounts costs
            // fourteen requests and asking whether they changed costs one. But
            // a counter cannot see a swap: one departure and one arrival leave
            // it identical, and a rename does not move it at all. So a monitor
            // ticking every half hour printed "nothing has changed" over and
            // over, for up to the freshness window, while somebody had in fact
            // left. Nothing is lost — the walk happens once the capture ages
            // out and the comparison is against the last *reported* capture, so
            // the departure is reported in full then — but for those hours the
            // tool was stating a fact it had not checked.
            //
            // And a run where one list was refused and the other had no news
            // printed it too, because the refusal branch above is gated on
            // there being nothing stored at all and one surviving list gets
            // past it. On an account permanently behind the truncation wall
            // that is every run, about a list snob has never once read.
            //
            // The distinction is already in the domain type:
            // `Basis::Unchanged` is documented as "this one did not have to
            // look", against a `Compare` that read both and found nothing.
            let nobody_looked = [report.followers.as_ref(), report.following.as_ref()]
                .into_iter()
                .flatten()
                .all(|r| matches!(r.basis, Basis::Unchanged { .. }));

            lines.push(format!(
                "Nothing has changed for {who} since the last report."
            ));
            if refused {
                lines.push(
                    "One of the lists could not be read this time, so this speaks only for \
                     the other one."
                        .to_string(),
                );
            } else if nobody_looked {
                lines.push(
                    "Their counters had not moved, so the lists were not read again.".to_string(),
                );
            }
            lines.push(since_line(report));
        }
        return lines;
    }

    lines.push(format!("Changes for {who}{}", period(report)));
    lines.push(String::new());

    for (kind, diff) in [
        (ListKind::Followers, &changes.followers),
        (ListKind::Following, &changes.following),
    ] {
        lines.extend(list_lines(kind, diff));
    }

    if !changes.renamed.is_empty() {
        // The verb agrees, because one rename is the common case: it is what
        // `once`, `diff` and the loop print most often, and the line above the
        // names read "1 now go by another name". The baseline sentence a screen
        // up already branches this way for the same reason.
        let renamed = changes.renamed.len();
        lines.push(format!(
            "  {renamed} now {} by another name",
            if renamed == 1 { "goes" } else { "go" }
        ));
        for r in &changes.renamed {
            lines.push(format!(
                "    @{} is now @{}",
                printable(&r.from),
                printable(&r.to)
            ));
        }
    }

    lines
}

pub(super) fn list_lines(kind: ListKind, diff: &ListDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for (verb, who) in [("gained", &diff.gained), ("lost", &diff.lost)] {
        if who.is_empty() {
            continue;
        }
        lines.push(format!("  {kind} {verb}: {}", who.len()));
        for user in who {
            lines.push(format!("    {}", name_of(user)));
        }
    }
    lines
}

/// A name as it is drawn, filtered because it came off Instagram.
pub(super) fn name_of(user: &User) -> String {
    match user.full_name.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(full) => format!("@{} ({})", user.safe_username(), printable(full)),
        None => format!("@{}", user.safe_username()),
    }
}

/// " since 14/08 at 09:12", when there is a moment to name.
pub(super) fn period(report: &WatchReport) -> String {
    match earliest_since(report) {
        Some(since) => format!(" since {}", report::stored_on(since)),
        None => String::new(),
    }
}

pub(super) fn since_line(report: &WatchReport) -> String {
    match earliest_since(report) {
        Some(since) => format!("The last report was on {}.", report::stored_on(since)),
        None => "Nothing has been reported yet.".to_string(),
    }
}

/// The older of the two receipts.
///
/// The two lists are marked apart and can be reported at different moments, so
/// the interval the user is being shown starts at whichever was reported first
/// — saying the later one would claim a window shorter than the one the numbers
/// actually cover.
pub(super) fn earliest_since(report: &WatchReport) -> Option<i64> {
    [report.followers.as_ref(), report.following.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|r| r.since)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::tests::fixtures::{list, report_with, user};
    use crate::engine::watch::ListReport;
    use snob_core::Pk;
    use snob_core::model::ListKind;
    use snob_core::watch::{Basis, ListDiff, Rename};

    /// Somebody who has never run the tool is told to run it, not told that
    /// nothing changed — which would be true and useless.
    #[test]
    fn an_account_with_nothing_stored_is_told_what_to_run() {
        let lines = describe(&report_with(None, vec![]), false);
        assert!(lines.join("\n").contains("snob followers"), "{lines:?}");
    }

    /// A run that could not look at either list is not an account nothing has
    /// been walked for.
    ///
    /// Both are "no report to show", and they were printed the same way — so a
    /// tick during a cooldown, with two complete captures on disk, said "Nothing
    /// has been walked for @me yet" and told the reader to run `snob followers`,
    /// two lines above the warnings saying both lists had just been served from
    /// storage.
    #[test]
    fn a_run_that_could_not_look_does_not_claim_the_account_is_unknown() {
        let lines = describe(&report_with(None, vec![]), true).join("\n");
        assert!(!lines.contains("snob followers"), "{lines}");
        assert!(lines.contains("could be looked at"), "{lines}");
    }

    /// The worst thing this feature could print. A first look has no earlier
    /// capture, so it must say so rather than report an empty diff as calm.
    #[test]
    fn a_first_look_says_so_instead_of_saying_nothing_changed() {
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Baseline { snapshot_id: 1 },
                    ListDiff::default(),
                    None,
                )),
                vec![],
            ),
            false,
        );
        let text = lines.join("\n");
        assert!(text.contains("never been reported"), "{text}");
        assert!(
            !text.contains("Nothing has changed"),
            "a baseline is not a quiet account: {text}"
        );
    }

    /// A first look almost always finds both lists, so the common case is the
    /// plural one — and it read "the followers and following list has" until
    /// somebody ran it.
    #[test]
    fn a_first_look_at_both_lists_says_so_in_the_plural() {
        let baseline = |kind| ListReport {
            kind,
            basis: Basis::Baseline { snapshot_id: 1 },
            since: None,
            until: 2_000,
            diff: ListDiff::default(),
            total: 309,
        };
        let report = WatchReport {
            account_pk: 42,
            username: Some("me".into()),
            is_self: true,
            followers: Some(baseline(ListKind::Followers)),
            following: Some(baseline(ListKind::Following)),
            renamed: vec![],
        };

        let text = describe(&report, false).join("\n");
        assert!(text.contains("lists have never been reported"), "{text}");
        assert!(!text.contains("list has never"), "{text}");
    }

    /// A run that did not read the lists must not assert that nothing changed.
    ///
    /// `Basis::Unchanged` is documented as "this one did not have to look": the
    /// counter had not moved, so the stored capture was served without being
    /// re-read. A counter cannot see a swap — one departure and one arrival
    /// leave it identical — so a monitor printed "nothing has changed" for
    /// hours while somebody had left. The saving is right and stays; the
    /// unhedged sentence was not.
    #[test]
    fn a_run_that_did_not_look_says_so() {
        let unread = describe(
            &report_with(
                Some(list(
                    Basis::Unchanged { snapshot_id: 7 },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        )
        .join("\n");
        assert!(
            unread.contains("were not read again"),
            "a counter poll is not a look: {unread}"
        );

        // And a run that really did read both and found nothing keeps the
        // plain sentence, because there it is true.
        let read = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        )
        .join("\n");
        assert!(read.contains("Nothing has changed"), "{read}");
        assert!(
            !read.contains("were not read again"),
            "this one did look: {read}"
        );

        // A refused list is a third case, and it used to print the same
        // sentence as the other two: the refusal branch is reached only when
        // *nothing* is stored, so one surviving list got past it.
        let partial = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![],
            ),
            true,
        )
        .join("\n");
        assert!(
            partial.contains("could not be read this time"),
            "a refused list must not be reported as quiet: {partial}"
        );
    }

    #[test]
    fn an_arrival_and_a_departure_are_both_named() {
        let diff = ListDiff {
            gained: vec![user(1, "arrived")],
            lost: vec![user(2, "left")],
        };
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    diff,
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        );

        let text = lines.join("\n");
        assert!(text.contains("@arrived"), "{text}");
        assert!(text.contains("@left"), "{text}");
        assert!(text.contains("followers gained: 1"), "{text}");
        assert!(text.contains("followers lost: 1"), "{text}");
    }

    /// A rename on its own is news. It used to be possible for the empty-diff
    /// check to swallow a run whose only change was somebody's name.
    #[test]
    fn a_rename_on_its_own_is_still_reported() {
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff::default(),
                    Some(1_000),
                )),
                vec![Rename {
                    pk: 7,
                    history_id: 7,
                    from: "before".into(),
                    to: "after".into(),
                    at: 1_500,
                }],
            ),
            false,
        );

        let text = lines.join("\n");
        assert!(text.contains("@before is now @after"), "{text}");
        assert!(!text.contains("Nothing has changed"), "{text}");
    }

    /// And one rename is counted as one.
    ///
    /// The test above builds exactly one `Rename` and asserts only the line
    /// *below* the count, so it passes with that line deleted and passed with it
    /// reading "1 now go by another name". One is the common case here: it is
    /// what `once`, `diff` and the loop print most often.
    #[test]
    fn one_rename_is_counted_as_one() {
        let renamed = |names: &[(&str, &str)]| {
            describe(
                &report_with(
                    Some(list(
                        Basis::Compare {
                            before: 1,
                            after: 2,
                        },
                        ListDiff::default(),
                        Some(1_000),
                    )),
                    names
                        .iter()
                        .enumerate()
                        .map(|(n, (from, to))| Rename {
                            pk: n as Pk,
                            history_id: n as i64,
                            from: (*from).into(),
                            to: (*to).into(),
                            at: 1_500,
                        })
                        .collect(),
                ),
                false,
            )
            .join("\n")
        };

        let one = renamed(&[("before", "after")]);
        assert!(one.contains("1 now goes by another name"), "{one}");

        let two = renamed(&[("before", "after"), ("other", "later")]);
        assert!(two.contains("2 now go by another name"), "{two}");
    }

    /// A control character in a name reaches a terminal through this command
    /// like any other, so it goes through the same filter.
    #[test]
    fn a_name_is_filtered_before_it_is_drawn() {
        let lines = describe(
            &report_with(
                Some(list(
                    Basis::Compare {
                        before: 1,
                        after: 2,
                    },
                    ListDiff {
                        gained: vec![user(1, "bad\u{202e}name")],
                        lost: vec![],
                    },
                    Some(1_000),
                )),
                vec![],
            ),
            false,
        );
        assert!(
            !lines.join("\n").contains('\u{202e}'),
            "a bidi override reached the terminal"
        );
    }
}
