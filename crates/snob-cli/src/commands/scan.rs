//! `snob scan`: the whole-account summary.
//!
//! Walks both lists and prints the five counts. Unlike the set commands it
//! returns no account list, so `--limit` has nothing to trim, the machine
//! formats emit a counts object, and the row formats come out one row wide.
//!
//! On somebody else's account it opens with the people you both know, which is
//! the line you actually read first — and it costs nothing, because the answer
//! is already in the database.

use anyhow::Result;
use snob_core::filters::Filter;
use snob_core::model::{ListKind, User};
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_core::sets;

use crate::cli::{Format, ListArgs};
use crate::commands::common::{self, Destination, Session};
use crate::engine::{self, ListOutcome, ResultSource, people};
use crate::exit::ExitCode;
use crate::output::Rendered;
use crate::output::xlsx::Cell;
use crate::report;
use crate::{output, ui};

/// How many names the opening line puts before it starts counting.
const NAMES_SHOWN: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCounts {
    pub followers: usize,
    pub following: usize,
    pub friends: usize,
    pub fans: usize,
    pub unfollowers: usize,
}

impl ScanCounts {
    /// The five counts in the order every format prints them.
    ///
    /// Six parallel lists used to say this — the column widths, the JSON
    /// object, the header, the csv row, the spreadsheet cells and the labels —
    /// all of which had to agree, and two of which are the same type, so
    /// swapping a pair would have compiled and been quietly wrong.
    fn values(self) -> [usize; 5] {
        [
            self.followers,
            self.following,
            self.friends,
            self.fans,
            self.unfollowers,
        ]
    }
}

/// Everything the renderer needs, gathered so it can be tested as a pure
/// function.
struct Summary<'a> {
    counts: ScanCounts,
    target: &'a str,
    /// Whether the target was named on the command line. When it was, the
    /// hints repeat it: a bare `snob fans` would answer about your account,
    /// not the one this summary describes.
    explicit_target: bool,
    filtered: bool,
    /// The accounts you follow who also follow this one. `None` on your own
    /// account, where the question is what `snob friends` answers, and when
    /// nothing is stored to answer it with.
    followed_by: Option<&'a [User]>,
    followers: &'a ListOutcome,
    following: &'a ListOutcome,
}

pub async fn run(args: ListArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let filter = common::filter_from(&args)?;
    let destination = common::destination(&args)?;

    if args.limit.is_some() {
        ui::warn("--limit has no effect on scan: it prints counts, not accounts");
    }

    let Session::Open(mut app) = common::open(&args, &secrets, paths)? else {
        return Ok(ExitCode::NoSession);
    };

    // The label of the account being summarized, worked out up front so the
    // hints can name it.
    let explicit_target = args.target.is_some();
    let viewer = app.viewer().clone();
    let target = match &args.target {
        Some(raw) => engine::target::clean(raw).to_string(),
        None => viewer
            .username
            .clone()
            .unwrap_or_else(|| viewer.pk.to_string()),
    };

    // Followers first, mirroring the order `unfollowers` consumes the cache
    // in, and so an incomplete list is found out before the second walk is
    // spent.
    // The bar is finished before the `?`, not after it: `indicatif` leaves its
    // last line on screen when dropped, so an error used to print underneath a
    // spinner that had stopped spinning.
    let subject = engine::target::label(&app, &args);
    app.progress()
        .begin(&report::walking(ListKind::Followers, &subject));
    let first = engine::list(&mut app, &args, ListKind::Followers).await;
    let (followers, followers_outcome) = match first {
        Ok(pair) => pair,
        Err(e) => {
            app.progress().finish();
            return Err(e);
        }
    };
    if let Err(e) = check_complete(ListKind::Followers, &followers_outcome) {
        app.progress().finish();
        return Err(e);
    }

    app.progress()
        .begin(&report::walking(ListKind::Following, &subject));
    let second = engine::list(&mut app, &args, ListKind::Following).await;
    app.progress().finish();
    let (following, following_outcome) = second?;
    check_complete(ListKind::Following, &following_outcome)?;
    engine::cooldown::check_same_moment(&followers_outcome, &following_outcome)?;

    // Your own account is excluded rather than unsupported: "who you both
    // know" about yourself is the whole of `snob friends`.
    //
    // Decided from the id the engine reported rather than by comparing what
    // was typed against the stored username: that name can be absent, can be
    // spelled differently, and on a fresh session is not known at all.
    let is_self = followers_outcome.is_own(&viewer);
    let followed_by = if is_self {
        None
    } else {
        people::in_common(&app, &followers)?
    };

    if !is_self && followed_by.is_none() && destination.is_interactive() {
        ui::info(
            "Who you both follow is not shown: nothing of your own following is stored yet. \
             Run \"snob following\" once and it will appear from then on.",
        );
    }

    let filtered = !filter.is_empty();
    let summary = Summary {
        counts: summarize(&followers, &following, &filter),
        target: &target,
        explicit_target,
        filtered,
        followed_by: followed_by.as_deref(),
        followers: &followers_outcome,
        following: &following_outcome,
    };

    render_to(&summary, &destination)?;

    let mut line = format!(
        "account summary of @{target} - {}",
        report::requests(followers_outcome.requests + following_outcome.requests)
    );
    if filtered {
        line.push_str(" - filters active: the counts reflect them");
    }
    ui::info(&line);

    Ok(ExitCode::Ok)
}

/// Both lists have to be complete. The set commands only need the crossed-
/// against list whole; here every one of the five counts leans on both lists,
/// so a single missing account would bend the summary from partial to wrong.
fn check_complete(kind: ListKind, outcome: &ListOutcome) -> Result<()> {
    if outcome.is_complete() {
        return Ok(());
    }
    // Unlike a crossing, there is no single misreading to name: every one of
    // the five counts leans on both lists, so a missing account bends all of
    // them at once.
    Err(report::refuse_incomplete(
        kind,
        outcome.reason,
        outcome.exit_code(),
        "they were not there at all",
    ))
}

/// The counts go through the same pipeline as the set commands — cross by pk
/// first, then filter — so each derived count is exactly what the matching
/// command prints with the same flags. The displayed totals are the sums of
/// their regions, which keeps the identity true even if an account's
/// attributes changed between the two walks.
fn summarize(followers: &[User], following: &[User], filter: &Filter) -> ScanCounts {
    let friends = filter.apply(sets::intersection(followers, following)).len();
    let fans = filter.apply(sets::difference(followers, following)).len();
    let unfollowers = filter.apply(sets::difference(following, followers)).len();
    ScanCounts {
        followers: fans + friends,
        following: unfollowers + friends,
        friends,
        fans,
        unfollowers,
    }
}

fn render_to(summary: &Summary<'_>, destination: &Destination) -> Result<()> {
    // The hints are advice for a person reading along, so they belong to the
    // terminal and not to a file or a pipe.
    let hints = destination.format() == Format::Table && destination.is_interactive();
    destination.write_rendered(&render(summary, destination.format(), hints)?)
}

fn render(summary: &Summary<'_>, format: Format, hints: bool) -> Result<Rendered> {
    if format == Format::Xlsx {
        return Ok(Rendered::Bytes(output::xlsx::single_row_workbook(
            &ROW_HEADER,
            row_cells(summary),
        )?));
    }
    Ok(Rendered::Text(match format {
        Format::Table => text_table(summary, hints),
        Format::Json | Format::Ndjson => {
            // Every value here is a stable token, never a human-facing
            // string: rewording a message must not be able to break this
            // contract.
            let object = serde_json::json!({
                "target": summary.target,
                "filtered": summary.filtered,
                "counts": {
                    "followers": summary.counts.followers,
                    "following": summary.counts.following,
                    "friends": summary.counts.friends,
                    "fans": summary.counts.fans,
                    "unfollowers": summary.counts.unfollowers,
                },
                "followed_by": summary.followed_by.map(|people| serde_json::json!({
                    "count": people.len(),
                    "accounts": people,
                })),
                "lists": {
                    "followers": list_object(summary.followers),
                    "following": list_object(summary.following),
                },
            });
            let mut s = if format == Format::Json {
                serde_json::to_string_pretty(&object)?
            } else {
                serde_json::to_string(&object)?
            };
            s.push('\n');
            s
        }
        Format::Md => output::md::summary(
            summary.target,
            summary.filtered,
            followed_by_line(summary).as_deref(),
            &labeled(summary),
        ),
        Format::Csv => output::csv::single_row(&ROW_HEADER, &row_fields(summary))?,
        // Handled above: it is the one format that is not text.
        Format::Xlsx => unreachable!(),
    }))
}

/// "Followed by @ana, @luis and @eva and 2 others".
///
/// `None` when there is nobody to name, which includes both "you follow nobody
/// who follows them" and "we have no stored list to check against". The
/// difference between those two is reported on standard error, not here: a
/// summary is not the place to explain what is missing from it.
fn followed_by_line(summary: &Summary<'_>) -> Option<String> {
    let people = summary.followed_by?;
    people::name_a_few(people, NAMES_SHOWN).map(|names| format!("Followed by {names}"))
}

fn text_table(summary: &Summary<'_>, hints: bool) -> String {
    let c = summary.counts;
    let width = c
        .values()
        .into_iter()
        .map(|n| n.to_string().len())
        .max()
        .unwrap_or(1);
    let suffix = if summary.explicit_target {
        format!(" @{}", summary.target)
    } else {
        String::new()
    };

    let mut rows = Vec::new();
    // First, because it is the line a person actually reads first.
    if let Some(line) = followed_by_line(summary) {
        rows.push(line);
        rows.push(String::new());
    }
    rows.push(format!("{:<14}@{}", "Account:", summary.target));
    rows.push(format!("{:<14}{:<width$}", "Followers:", c.followers));
    rows.push(format!("{:<14}{:<width$}", "Following:", c.following));

    for (label, count, command) in [
        ("Friends:", c.friends, "friends"),
        ("Fans:", c.fans, "fans"),
        ("Unfollowers:", c.unfollowers, "unfollowers"),
    ] {
        let mut row = format!("{label:<14}{count:<width$}");
        if hints {
            row.push_str(&format!("  for details, run \"snob {command}{suffix}\""));
        }
        rows.push(row);
    }

    let mut s = String::new();
    for row in rows {
        s.push_str(row.trim_end());
        s.push('\n');
    }
    s
}

/// The column names for the row formats. The five counts are the same stable
/// tokens the JSON object uses, so a spreadsheet and a script name them alike.
///
/// `followed_by` is the count alone: a cell holding a list of names is a cell
/// the next tool has to parse, and the JSON output is where the names live.
const ROW_HEADER: [&str; 8] = [
    "target",
    "filtered",
    "followers",
    "following",
    "friends",
    "fans",
    "unfollowers",
    "followed_by",
];

/// The count of people in common, or an empty cell when the question could not
/// be answered. Never a zero: "nobody" and "we did not look" are different
/// answers, and the empty cell is how the rest of the tool spells the second.
fn followed_by_count(summary: &Summary<'_>) -> Option<usize> {
    summary.followed_by.map(<[User]>::len)
}

fn row_fields(summary: &Summary<'_>) -> Vec<String> {
    let mut fields = vec![summary.target.to_string(), summary.filtered.to_string()];
    fields.extend(summary.counts.values().map(|n| n.to_string()));
    fields.push(
        followed_by_count(summary)
            .map(|n| n.to_string())
            .unwrap_or_default(),
    );
    fields
}

fn row_cells(summary: &Summary<'_>) -> Vec<Cell> {
    let mut cells = vec![
        Cell::Text(summary.target.to_string()),
        Cell::Bool(summary.filtered),
    ];
    cells.extend(
        summary
            .counts
            .values()
            .map(|count| Cell::Number(count as f64)),
    );
    cells.push(match followed_by_count(summary) {
        Some(n) => Cell::Number(n as f64),
        None => Cell::Empty,
    });
    cells
}

/// The counts under headings a person reads, rather than the tokens a script
/// matches on.
fn labeled(summary: &Summary<'_>) -> Vec<(&'static str, usize)> {
    let c = summary.counts;
    vec![
        ("Followers", c.followers),
        ("Following", c.following),
        ("Friends", c.friends),
        ("Fans", c.fans),
        ("Unfollowers", c.unfollowers),
    ]
}

fn list_object(outcome: &ListOutcome) -> serde_json::Value {
    serde_json::json!({
        "source": source_token(outcome.source()),
        "taken_at": outcome.taken_at,
        "requests": outcome.requests,
    })
}

fn source_token(source: ResultSource) -> &'static str {
    match source {
        ResultSource::Fetched => "fetched",
        ResultSource::Cached => "cached",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::filters::Attribute;
    use snob_core::model::StopReason;

    fn user(pk: u64, name: &str) -> User {
        User {
            pk,
            username: name.into(),
            full_name: None,
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    fn verified(pk: u64, name: &str) -> User {
        User {
            is_verified: Some(true),
            ..user(pk, name)
        }
    }

    fn outcome() -> ListOutcome {
        ListOutcome {
            provenance: engine::Provenance::Walked,
            reason: StopReason::Completed,
            requests: 3,
            taken_at: 1_722_700_000,
            account_pk: 1,
            stopped_by: None,
        }
    }

    fn summary<'a>(
        counts: ScanCounts,
        explicit_target: bool,
        outcomes: &'a (ListOutcome, ListOutcome),
    ) -> Summary<'a> {
        Summary {
            counts,
            target: "someone",
            explicit_target,
            filtered: false,
            followed_by: None,
            followers: &outcomes.0,
            following: &outcomes.1,
        }
    }

    fn counts() -> ScanCounts {
        ScanCounts {
            followers: 301,
            following: 136,
            friends: 104,
            fans: 197,
            unfollowers: 32,
        }
    }

    #[test]
    fn the_counts_partition_both_lists() {
        let following = vec![user(1, "a"), user(2, "b"), user(3, "c")];
        let followers = vec![user(2, "b"), user(3, "c"), user(4, "d")];

        let c = summarize(&followers, &following, &Filter::default());
        assert_eq!(c.unfollowers, 1);
        assert_eq!(c.fans, 1);
        assert_eq!(c.friends, 2);
        assert_eq!(c.unfollowers + c.friends, c.following);
        assert_eq!(c.fans + c.friends, c.followers);
    }

    /// The identity holds after filtering because the filter is a per-account
    /// predicate: it removes each account from every region at once.
    #[test]
    fn the_identity_survives_filtering() {
        let following = vec![
            user(1, "friend"),
            verified(2, "famous_friend"),
            user(3, "snob"),
            verified(4, "famous_snob"),
        ];
        let followers = vec![
            user(1, "friend"),
            verified(2, "famous_friend"),
            user(5, "fan"),
            verified(6, "famous_fan"),
        ];

        let filter = Filter {
            hide: vec![Attribute::Verified],
            ..Default::default()
        };
        let c = summarize(&followers, &following, &filter);

        assert_eq!(c.unfollowers + c.friends, c.following);
        assert_eq!(c.fans + c.friends, c.followers);
        assert_eq!(c.friends, 1);
        assert_eq!(c.unfollowers, 1);
        assert_eq!(c.fans, 1);
    }

    /// What `scan` counts has to be what `unfollowers` would print with the
    /// same flags: they share the cross-by-pk-then-filter pipeline.
    #[test]
    fn scan_agrees_with_the_set_commands_under_filters() {
        let following = vec![
            user(1, "friend"),
            verified(2, "famous_snob"),
            user(3, "snob"),
        ];
        let followers = vec![user(1, "friend"), user(4, "fan")];
        let filter = Filter {
            hide: vec![Attribute::Verified],
            ..Default::default()
        };

        let set_command = filter.apply(sets::difference(&following, &followers)).len();
        let scanned = summarize(&followers, &following, &filter).unfollowers;

        assert_eq!(scanned, set_command);
    }

    /// The rule the set commands apply to the crossed-against list, here
    /// applied to both: an incomplete list makes every count wrong.
    #[test]
    fn an_incomplete_list_blocks_the_summary() {
        assert!(check_complete(ListKind::Followers, &outcome()).is_ok());

        for reason in [
            StopReason::Canceled,
            StopReason::PageLimit,
            StopReason::Truncated,
            StopReason::RateLimit,
            StopReason::Network,
            StopReason::SessionInvalid,
        ] {
            let mut incomplete = outcome();
            incomplete.reason = reason;
            let result = check_complete(ListKind::Followers, &incomplete);
            assert!(result.is_err(), "with {reason:?} no summary can be given");
            assert!(result.unwrap_err().to_string().contains("wrong"));
        }
    }

    /// The summary renders as text in every format that has one.
    fn rendered_text(summary: &Summary<'_>, format: Format, hints: bool) -> String {
        match render(summary, format, hints).unwrap() {
            Rendered::Text(text) => text,
            Rendered::Bytes(_) => panic!("expected text, got bytes"),
        }
    }

    #[test]
    fn the_json_object_carries_the_counts_and_no_hints() {
        let outcomes = (outcome(), outcome());
        let text = rendered_text(&summary(counts(), false, &outcomes), Format::Json, false);

        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["target"], "someone");
        assert_eq!(parsed["counts"]["unfollowers"], 32);
        assert_eq!(parsed["counts"]["followers"], 301);
        assert_eq!(parsed["counts"]["friends"], 104);
        assert_eq!(parsed["lists"]["followers"]["source"], "fetched");
        assert!(!text.contains("for details"));

        let line = rendered_text(&summary(counts(), false, &outcomes), Format::Ndjson, false);
        assert_eq!(line.lines().count(), 1, "ndjson is one object on one line");
    }

    #[test]
    fn hints_appear_only_on_the_table_for_a_terminal() {
        let outcomes = (outcome(), outcome());

        let with = rendered_text(&summary(counts(), false, &outcomes), Format::Table, true);
        assert!(with.contains("for details, run \"snob unfollowers\""));
        assert!(with.contains("for details, run \"snob friends\""));

        let without = rendered_text(&summary(counts(), false, &outcomes), Format::Table, false);
        assert!(!without.contains("for details"));
    }

    /// The hint is a drill-down of the number on its line: for someone else's
    /// account, a bare `snob fans` would answer about the wrong account.
    #[test]
    fn the_hints_repeat_an_explicit_target() {
        let outcomes = (outcome(), outcome());
        let text = rendered_text(&summary(counts(), true, &outcomes), Format::Table, true);
        assert!(text.contains("for details, run \"snob fans @someone\""));
    }

    /// The summary is counts, not accounts, so its row formats are one row
    /// wide. The five numbers still have to be all of them.
    #[test]
    fn the_row_formats_emit_the_five_counts() {
        let outcomes = (outcome(), outcome());

        let csv = rendered_text(&summary(counts(), false, &outcomes), Format::Csv, false);
        let lines: Vec<_> = csv.lines().collect();
        assert_eq!(lines.len(), 2, "{csv}");
        assert_eq!(
            lines[0],
            "target,filtered,followers,following,friends,fans,unfollowers,followed_by"
        );
        assert_eq!(lines[1], "someone,false,301,136,104,197,32,");

        let md = rendered_text(&summary(counts(), false, &outcomes), Format::Md, false);
        assert!(md.contains("@someone"), "{md}");
        assert!(md.contains("| Unfollowers | 32 |"), "{md}");
        assert!(md.contains("| Friends | 104 |"), "{md}");

        match render(&summary(counts(), false, &outcomes), Format::Xlsx, false).unwrap() {
            Rendered::Bytes(bytes) => assert_eq!(&bytes[..4], b"PK\x03\x04"),
            Rendered::Text(_) => panic!("a workbook is not text"),
        }
    }

    /// The counts a person reads are the ones a script reads. If the summary
    /// and the row ever disagreed, one of them would be wrong.
    #[test]
    fn the_row_repeats_what_the_json_says() {
        let outcomes = (outcome(), outcome());
        let json = rendered_text(&summary(counts(), false, &outcomes), Format::Json, false);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        let csv = rendered_text(&summary(counts(), false, &outcomes), Format::Csv, false);
        let fields: Vec<&str> = csv.lines().nth(1).unwrap().split(',').collect();

        // The five counts, which is what both formats agree on. `followed_by`
        // is deliberately shaped differently in each and is checked apart.
        for (column, name) in ROW_HEADER.iter().enumerate().take(7).skip(2) {
            assert_eq!(fields[column], parsed["counts"][name].to_string(), "{name}");
        }
    }

    fn with_people<'a>(
        outcomes: &'a (ListOutcome, ListOutcome),
        people: &'a [User],
    ) -> Summary<'a> {
        Summary {
            followed_by: Some(people),
            explicit_target: true,
            ..summary(counts(), true, outcomes)
        }
    }

    /// The line a person reads first, so it goes first.
    #[test]
    fn the_people_you_both_know_open_the_table() {
        let outcomes = (outcome(), outcome());
        let known = vec![user(1, "ana"), user(2, "luis")];
        let text = rendered_text(&with_people(&outcomes, &known), Format::Table, false);

        assert!(text.starts_with("Followed by @ana and @luis\n"), "{text}");
        assert!(text.contains("Account:      @someone"), "{text}");
    }

    /// Nobody in common is an answer, and it is not the same answer as having
    /// nothing to check against. The count says so; the line says nothing.
    #[test]
    fn nobody_in_common_prints_no_line_but_still_counts() {
        let outcomes = (outcome(), outcome());
        let nobody: Vec<User> = Vec::new();
        let summary = with_people(&outcomes, &nobody);

        let text = rendered_text(&summary, Format::Table, false);
        assert!(!text.contains("Followed by"), "{text}");
        assert!(text.starts_with("Account:"), "{text}");

        let csv = rendered_text(&summary, Format::Csv, false);
        assert!(csv.lines().nth(1).unwrap().ends_with(",0"), "{csv}");
    }

    /// Not having looked is an empty cell, never a zero: a script must be able
    /// to tell "nobody" from "we could not say".
    #[test]
    fn not_having_looked_is_an_empty_cell_and_a_null() {
        let outcomes = (outcome(), outcome());
        let unknown = summary(counts(), true, &outcomes);

        let csv = rendered_text(&unknown, Format::Csv, false);
        assert!(csv.lines().nth(1).unwrap().ends_with(",32,"), "{csv}");

        let json = rendered_text(&unknown, Format::Json, false);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["followed_by"].is_null(), "{json}");
    }

    #[test]
    fn the_json_names_everyone_in_common_not_just_the_first_few() {
        let outcomes = (outcome(), outcome());
        let many: Vec<User> = (1..=6).map(|i| user(i, &format!("u{i}"))).collect();
        let json = rendered_text(&with_people(&outcomes, &many), Format::Json, false);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["followed_by"]["count"], 6);
        assert_eq!(
            parsed["followed_by"]["accounts"].as_array().unwrap().len(),
            6,
            "the table names a few; the machine format names them all"
        );
    }

    #[test]
    fn the_markdown_summary_opens_with_the_same_line() {
        let outcomes = (outcome(), outcome());
        let known = vec![
            user(1, "ana"),
            user(2, "luis"),
            user(3, "eva"),
            user(4, "j"),
        ];
        let md = rendered_text(&with_people(&outcomes, &known), Format::Md, false);
        assert!(
            md.contains("Followed by @ana, @luis and @eva and 1 other"),
            "{md}"
        );
    }
}
