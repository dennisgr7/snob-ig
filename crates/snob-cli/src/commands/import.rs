//! `snob import dyi`: reads the archive Instagram hands over under "Download
//! your information" and works out the same relationships the live commands do.
//!
//! **Not reachable from the CLI.** The reader below works and is covered by
//! tests, but the command is deliberately not registered in `cli.rs`: what is
//! unsettled is not the parsing, it is what an import should be able to do once
//! it is in — whether it can be crossed against a live list, how it should be
//! exported, and whether it belongs in the store at all. Shipping the subcommand
//! would answer those questions by accident. Re-registering it is two lines,
//! once they have answers.
//!
//! It costs nothing and risks nothing: no session, no network, no request. That
//! is the whole point — it is the answer for anyone who would rather not have a
//! tool talk to Instagram on their behalf at all.
//!
//! It also stores nothing. The archive names accounts by username, and
//! everything in the database is keyed by the numeric id, which an export never
//! carries. Rather than guess at that mapping, the analysis stays here: the
//! stored snapshots keep meaning "walked live", and a crossing can never
//! silently mix a months-old export with a list fetched a minute ago.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::cli::{Format, ImportCommand};
use crate::exit::ExitCode;
use crate::output::{self, Rendered};
use crate::ui;

/// Ceiling on everything pulled out of the archive, across all its files.
///
/// The largest real follower list runs to a few megabytes, so this is already
/// generous by an order of magnitude. It is a total rather than a per-file
/// limit because the number of matching files is not bounded either:
/// `followers_1` through `followers_9999` are all valid names, and deflate
/// packs about a thousand to one, so a per-file cap leaves a one-megabyte
/// archive able to ask for gigabytes.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

pub fn run(command: ImportCommand) -> Result<ExitCode> {
    let ImportCommand::Dyi { path } = command;
    let export = read_export(&path)?;

    ui::info(&format!(
        "Read {} followers and {} following from {}",
        export.followers.len(),
        export.following.len(),
        path.display()
    ));
    ui::warn(
        "an export describes the moment Instagram built it, not this one; \
         anything that changed since is not in here",
    );

    let analysis = analyze(&export);
    let format = output::effective_format(None, None);
    output::write_rendered(&render(&analysis, format)?, None)?;
    Ok(ExitCode::Ok)
}

/// Sets rather than lists: Instagram repeats accounts across the split files,
/// so the duplicates have to go anyway, and a set drops them as they arrive
/// instead of in a pass of its own afterwards.
#[derive(Debug, Default)]
struct Export {
    followers: HashSet<String>,
    following: HashSet<String>,
}

struct Analysis {
    followers: usize,
    following: usize,
    friends: Vec<String>,
    fans: Vec<String>,
    unfollowers: Vec<String>,
}

/// The set arithmetic, on usernames.
///
/// Everywhere else the tool crosses lists by numeric id, because a username can
/// be given up and taken by somebody else between two walks. Here it is safe:
/// both lists were written by the same export at the same instant, so no rename
/// can have happened in between.
fn analyze(export: &Export) -> Analysis {
    let (followers, following) = (&export.followers, &export.following);

    fn sorted<'a>(names: impl Iterator<Item = &'a String>) -> Vec<String> {
        let mut names: Vec<String> = names.cloned().collect();
        names.sort();
        names
    }

    Analysis {
        followers: followers.len(),
        following: following.len(),
        friends: sorted(followers.intersection(following)),
        fans: sorted(followers.difference(following)),
        unfollowers: sorted(following.difference(followers)),
    }
}

fn render(analysis: &Analysis, format: Format) -> Result<Rendered> {
    // Without `--format` or `-o` the choice is only ever a table on a terminal
    // or JSON down a pipe; the other formats cannot be reached from here.
    if format == Format::Table {
        let mut out = String::new();
        for (label, count) in [
            ("Followers:", analysis.followers),
            ("Following:", analysis.following),
            ("Friends:", analysis.friends.len()),
            ("Fans:", analysis.fans.len()),
            ("Unfollowers:", analysis.unfollowers.len()),
        ] {
            out.push_str(&format!("{label:<14}{count}\n"));
        }
        if !analysis.unfollowers.is_empty() {
            out.push('\n');
            for name in &analysis.unfollowers {
                out.push_str(name);
                out.push('\n');
            }
        }
        return Ok(Rendered::Text(out));
    }

    // The keys match `snob scan`, so a script reading one reads the other.
    let object = serde_json::json!({
        "source": "dyi",
        "counts": {
            "followers": analysis.followers,
            "following": analysis.following,
            "friends": analysis.friends.len(),
            "fans": analysis.fans.len(),
            "unfollowers": analysis.unfollowers.len(),
        },
        "unfollowers": analysis.unfollowers,
        "fans": analysis.fans,
        "friends": analysis.friends,
    });
    let mut text = if format == Format::Ndjson {
        serde_json::to_string(&object)?
    } else {
        serde_json::to_string_pretty(&object)?
    };
    text.push('\n');
    Ok(Rendered::Text(text))
}

/// Which of the two lists a file inside the archive belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Followers,
    Following,
}

/// Recognizes `followers.json`, `followers_1.json`, `following.json` and so on,
/// wherever they sit in the archive.
///
/// The numeric suffix is checked rather than assumed, so neighbors like
/// `follow_requests_sent.json` are not swept in — they hold different
/// relationships and would corrupt every count.
fn list_in(name: &str) -> Option<Which> {
    let file = name.rsplit(['/', '\\']).next()?;
    let stem = file.strip_suffix(".json")?;

    for (prefix, which) in [
        ("followers", Which::Followers),
        ("following", Which::Following),
    ] {
        let Some(rest) = stem.strip_prefix(prefix) else {
            continue;
        };
        let numbered = rest
            .strip_prefix('_')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if rest.is_empty() || numbered {
            return Some(which);
        }
    }
    None
}

fn read_export(path: &Path) -> Result<Export> {
    let file =
        std::fs::File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a readable zip archive", path.display()))?;

    let mut export = Export::default();
    let mut json_seen = false;
    let mut budget = MAX_TOTAL_BYTES;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        if name.ends_with(".json") {
            json_seen = true;
        }
        let Some(which) = list_in(&name) else {
            continue;
        };

        let mut text = String::new();
        let read = entry
            .by_ref()
            .take(budget + 1)
            .read_to_string(&mut text)
            .with_context(|| format!("could not read {name} out of the archive"))?
            as u64;
        if read > budget {
            bail!(
                "the lists in that archive add up to more than {} MB, which no real \
                 export does",
                MAX_TOTAL_BYTES / (1024 * 1024)
            );
        }
        budget -= read;

        let names = usernames_from_json(&text, which)
            .with_context(|| format!("could not make sense of {name}"))?;
        match which {
            // Instagram splits a long list over followers_1, followers_2 and so
            // on, so these accumulate rather than replace.
            Which::Followers => export.followers.extend(names),
            Which::Following => export.following.extend(names),
        }
    }

    #[allow(clippy::collapsible_if)]
    if export.followers.is_empty() && export.following.is_empty() {
        if json_seen {
            bail!(
                "that archive has no followers or following lists in it. They live under \
                 connections/followers_and_following/ — check the export included \
                 \"Followers and following\""
            );
        }
        bail!(
            "that archive holds no JSON. Instagram offers the export as HTML or as JSON, \
             and only JSON can be read here — ask for it again and pick JSON"
        );
    }

    Ok(export)
}

/// Instagram has spelled the payload two ways over the years: a bare array, or
/// an object wrapping one under `relationships_followers` and friends.
///
/// The key belonging to *this* list is tried first, then the shape, because the
/// naming is what has moved. Taking the first array of whichever key happened
/// to come first would let a file carrying both hand back the wrong one and
/// swap fans with unfollowers. An object holding more than one unnamed array is
/// refused rather than guessed at, for the same reason.
fn entries(value: &Value, which: Which) -> Option<&[Value]> {
    if let Some(items) = value.as_array() {
        return Some(items);
    }

    let map = value.as_object()?;
    let expected = match which {
        Which::Followers => "relationships_followers",
        Which::Following => "relationships_following",
    };
    if let Some(items) = map.get(expected).and_then(Value::as_array) {
        return Some(items);
    }

    let mut arrays = map.values().filter_map(Value::as_array);
    let only = arrays.next()?;
    arrays.next().is_none().then_some(only.as_slice())
}

fn usernames_from_json(text: &str, which: Which) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(text).context("it is not valid JSON")?;
    let Some(items) = entries(&value, which) else {
        bail!("expected a list of accounts and found something else");
    };

    let names: Vec<String> = items
        .iter()
        .filter_map(|entry| {
            let name = entry
                .get("string_list_data")?
                .as_array()?
                .first()?
                .get("value")?
                .as_str()?
                .trim();
            // Usernames are case-insensitive on Instagram, and the export has
            // been seen to disagree with itself about capitalization. Crossing
            // two lists that spell the same account differently would invent
            // both an unfollower and a fan out of one person.
            (!name.is_empty()).then(|| name.to_lowercase())
        })
        .collect();

    // A list with entries in it that yields no names at all means the shape
    // moved again. Returning an empty list would be worse than failing: every
    // account on the other side would be reported as an unfollower, confidently
    // and wrongly.
    if names.is_empty() && !items.is_empty() {
        bail!(
            "it holds {} entries but no usernames, so the export's shape is not the one \
             this understands",
            items.len()
        );
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// One entry in the shape every recent export uses.
    fn entry(name: &str) -> String {
        format!(
            r#"{{"title":"","media_list_data":[],"string_list_data":
               [{{"href":"https://www.instagram.com/{name}","value":"{name}",
                  "timestamp":1704067200}}]}}"#
        )
    }

    fn wrapped(key: &str, names: &[&str]) -> String {
        let items: Vec<String> = names.iter().map(|n| entry(n)).collect();
        format!(r#"{{"{key}":[{}]}}"#, items.join(","))
    }

    fn bare(names: &[&str]) -> String {
        let items: Vec<String> = names.iter().map(|n| entry(n)).collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn it_reads_the_wrapped_shape() {
        let names = usernames_from_json(
            &wrapped("relationships_following", &["ann", "bob"]),
            Which::Following,
        )
        .unwrap();
        assert_eq!(names, vec!["ann", "bob"]);
    }

    /// The older exports handed followers over as a naked array.
    #[test]
    fn it_reads_the_bare_array_shape() {
        assert_eq!(
            usernames_from_json(&bare(&["ann"]), Which::Followers).unwrap(),
            vec!["ann"]
        );
    }

    /// The wrapper key has been renamed before, so an unfamiliar one must not
    /// stop it working.
    #[test]
    fn an_unfamiliar_wrapper_key_still_works() {
        let names = usernames_from_json(
            &wrapped("relationships_something_new", &["ann"]),
            Which::Followers,
        )
        .unwrap();
        assert_eq!(names, vec!["ann"]);
    }

    /// Two arrays and no known key is ambiguous. Guessing would give a
    /// confident wrong answer, which is worse than saying so.
    #[test]
    fn an_ambiguous_object_is_refused() {
        let json = format!(r#"{{"a":[{}],"b":[{}]}}"#, entry("ann"), entry("bob"));
        assert!(usernames_from_json(&json, Which::Followers).is_err());
    }

    #[test]
    fn entries_without_a_username_are_skipped() {
        let json = format!(
            r#"[{},{{"string_list_data":[]}},{{"string_list_data":[{{"href":"x"}}]}}]"#,
            entry("ann")
        );
        assert_eq!(
            usernames_from_json(&json, Which::Followers).unwrap(),
            vec!["ann"]
        );
    }

    /// A file carrying both keys must hand back the one it was opened for.
    /// Taking the other would swap fans with unfollowers and say nothing.
    #[test]
    fn a_file_holding_both_keys_yields_the_list_it_was_read_for() {
        let json = format!(
            r#"{{"relationships_followers":[{}],"relationships_following":[{}]}}"#,
            entry("follower"),
            entry("followed")
        );
        assert_eq!(
            usernames_from_json(&json, Which::Followers).unwrap(),
            vec!["follower"]
        );
        assert_eq!(
            usernames_from_json(&json, Which::Following).unwrap(),
            vec!["followed"]
        );
    }

    /// If the shape moves again, entries that yield no username at all have to
    /// be an error. Reporting an empty list would mark everyone on the other
    /// side as an unfollower, confidently and wrongly.
    #[test]
    fn entries_that_yield_no_names_are_an_error_not_an_empty_list() {
        let json = r#"[{"title":"ann","string_list_data":[{"href":"https://x/ann"}]},
                       {"title":"bob","string_list_data":[{"href":"https://x/bob"}]}]"#;
        let error = usernames_from_json(json, Which::Followers).unwrap_err();
        assert!(error.to_string().contains("no usernames"), "{error}");

        // A genuinely empty list is still fine: some accounts follow nobody.
        assert!(
            usernames_from_json("[]", Which::Followers)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn capitalization_does_not_split_one_account_in_two() {
        let names = usernames_from_json(&bare(&["Ann"]), Which::Followers).unwrap();
        assert_eq!(names, vec!["ann"]);
    }

    #[test]
    fn it_recognizes_the_files_and_leaves_the_neighbors_alone() {
        let followers = Some(Which::Followers);
        let following = Some(Which::Following);
        assert_eq!(
            list_in("connections/followers_and_following/followers_1.json"),
            followers
        );
        assert_eq!(list_in("followers.json"), followers);
        assert_eq!(list_in("followers_12.json"), followers);
        assert_eq!(
            list_in("connections/followers_and_following/following.json"),
            following
        );
        assert_eq!(list_in("following_2.json"), following);

        // Different relationships that must not be counted as either.
        assert_eq!(list_in("follow_requests_sent.json"), None);
        assert_eq!(list_in("pending_follow_requests.json"), None);
        assert_eq!(list_in("recently_unfollowed_profiles.json"), None);
        assert_eq!(list_in("followers_and_following.html"), None);
        assert_eq!(list_in("followers_extra.json"), None);
    }

    fn archive(files: &[(&str, String)]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in files {
                writer.start_file(*name, options).unwrap();
                writer.write_all(body.as_bytes()).unwrap();
            }
            let bytes = writer.finish().unwrap().into_inner();
            file.write_all(&bytes).unwrap();
            file.flush().unwrap();
        }
        file
    }

    #[test]
    fn a_split_follower_list_is_read_as_one() {
        let zipped = archive(&[
            (
                "connections/followers_and_following/followers_1.json",
                bare(&["ann", "bob"]),
            ),
            (
                "connections/followers_and_following/followers_2.json",
                bare(&["cat"]),
            ),
            (
                "connections/followers_and_following/following.json",
                wrapped("relationships_following", &["bob", "dan"]),
            ),
        ]);

        let export = read_export(zipped.path()).unwrap();
        let mut followers: Vec<&String> = export.followers.iter().collect();
        followers.sort();
        assert_eq!(followers, ["ann", "bob", "cat"].iter().collect::<Vec<_>>());
        assert_eq!(export.following.len(), 2);

        let analysis = analyze(&export);
        assert_eq!(analysis.followers, 3);
        assert_eq!(analysis.following, 2);
        assert_eq!(analysis.friends, vec!["bob"]);
        assert_eq!(analysis.fans, vec!["ann", "cat"]);
        assert_eq!(analysis.unfollowers, vec!["dan"]);
    }

    /// The counts have to add up the same way the live commands' do.
    #[test]
    fn the_arithmetic_holds() {
        let export = Export {
            followers: ["a", "b", "c"].iter().map(|s| s.to_string()).collect(),
            following: ["b", "c", "d", "e"].iter().map(|s| s.to_string()).collect(),
        };
        let a = analyze(&export);
        assert_eq!(a.fans.len() + a.friends.len(), a.followers);
        assert_eq!(a.unfollowers.len() + a.friends.len(), a.following);
    }

    /// Picking HTML instead of JSON is the likeliest mistake, so the message
    /// has to name the fix.
    #[test]
    fn an_html_export_says_what_to_do_about_it() {
        let zipped = archive(&[("connections/followers_1.html", "<html></html>".into())]);
        let error = read_export(zipped.path()).unwrap_err().to_string();
        assert!(error.contains("JSON"), "{error}");
    }

    #[test]
    fn an_archive_without_the_lists_says_where_they_should_be() {
        let zipped = archive(&[("personal_information.json", "{}".into())]);
        let error = read_export(zipped.path()).unwrap_err().to_string();
        assert!(error.contains("followers_and_following"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_reported_as_such() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"not a zip at all").unwrap();
        let error = read_export(file.path()).unwrap_err().to_string();
        assert!(error.contains("zip"), "{error}");
    }

    #[test]
    fn the_json_output_names_where_it_came_from() {
        let analysis = analyze(&Export {
            followers: HashSet::from(["ann".to_string()]),
            following: HashSet::from(["bob".to_string()]),
        });
        let Rendered::Text(json) = render(&analysis, Format::Json).unwrap() else {
            panic!("json should be text");
        };
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["source"], "dyi");
        assert_eq!(value["counts"]["unfollowers"], 1);
        assert_eq!(value["unfollowers"][0], "bob");
    }

    #[test]
    fn the_table_lists_the_unfollowers_under_the_counts() {
        let analysis = analyze(&Export {
            followers: HashSet::from(["ann".to_string()]),
            following: HashSet::from(["bob".to_string()]),
        });
        let Rendered::Text(table) = render(&analysis, Format::Table).unwrap() else {
            panic!("a table should be text");
        };
        assert!(table.contains("Unfollowers:  1"), "{table}");
        assert!(table.ends_with("bob\n"), "{table}");
    }
}
