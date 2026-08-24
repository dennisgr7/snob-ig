//! Markdown tables.
//!
//! A format for people rather than for parsers: same columns as the terminal
//! table, with the usernames as links so they stay clickable once the file is
//! pasted somewhere.

use snob_core::model::User;

use super::table;

/// A pipe table, the flavor every markdown renderer agrees on.
pub(crate) fn table(users: &[User]) -> String {
    let mut out = String::new();
    out.push_str(&row(&table::HEADER.map(String::from)));
    out.push_str(&row(&["---".into(), "---".into(), "---".into()]));
    for user in users {
        out.push_str(&row(&[
            link(user),
            escape(&user.safe_full_name().unwrap_or_default()),
            table::attributes(user),
        ]));
    }
    out
}

fn row(cells: &[String; 3]) -> String {
    format!("| {} | {} | {} |\n", cells[0], cells[1], cells[2])
}

/// The summary: a heading naming the account, the people you both know when
/// there are any, then one row per count.
pub(crate) fn summary(
    target: &str,
    filtered: bool,
    followed_by: Option<&str>,
    counts: &[(&str, usize)],
) -> String {
    let mut out = format!("# Account summary of @{}\n\n", escape(target));
    if let Some(line) = followed_by {
        out.push_str(&format!("{}\n\n", escape(line)));
    }
    if filtered {
        out.push_str("Filters are active: the counts reflect them.\n\n");
    }
    out.push_str("| Count | Accounts |\n| --- | --- |\n");
    for (label, value) in counts {
        out.push_str(&format!("| {label} | {value} |\n"));
    }
    out
}

fn link(user: &User) -> String {
    format!(
        "[{}]({})",
        escape(&user.safe_username()),
        user.profile_url()
    )
}

/// Escapes what would otherwise break the table or the link around it.
///
/// The backslash goes first: doing it after the pipe would escape the escape
/// and leave the pipe bare. A newline cannot live in a cell at all, so it
/// becomes a space.
///
/// The brackets are here because every string this escapes is then wrapped in
/// `[…]` by [`link`], and a `]` in a name ends that label early — so a username
/// of `x](http://evil.test)` produced a second, working link to somewhere else,
/// sitting in the row as though this file had put it there. The destination is
/// a separate problem with a separate answer: `User::profile_url` encodes.
pub(crate) fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace("\r\n", " ")
        .replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_core::Pk;

    fn user() -> User {
        User {
            pk: Pk::new(1),
            username: "one".into(),
            full_name: Some("One Person".into()),
            is_private: None,
            is_verified: Some(true),
            pfp_url: None,
        }
    }

    #[test]
    fn a_username_is_a_link_to_the_profile() {
        let out = table(&[user()]);
        assert!(
            out.contains("| [one](https://www.instagram.com/one/) | One Person | verified |"),
            "{out}"
        );
    }

    /// A row is a link, and both halves of one were open.
    ///
    /// The destination took the raw name, so a `)` in it ended the address
    /// early; and the label is wrapped in `[…]`, so a `]` ended *that* early and
    /// what followed became a second, working link to wherever the name said.
    /// Two holes, two answers: the destination is percent-encoded and the label
    /// is escaped.
    #[test]
    fn a_bracket_in_a_name_cannot_end_the_link_early() {
        let out = table(&[User {
            username: "one](http://evil.test)".into(),
            ..user()
        }]);

        // The only place a label ends is the one this file wrote.
        assert_eq!(
            out.matches("](https://www.instagram.com/").count(),
            1,
            "{out}"
        );
        assert!(
            out.contains("\\](http://evil.test)"),
            "the name's own bracket has to be escaped, not left to close the label: {out}"
        );
        assert!(out.contains("%5D"), "and encoded in the destination: {out}");
    }

    #[test]
    fn the_header_matches_the_terminal_table() {
        let out = table(&[]);
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(
            lines,
            vec![
                "| Username | Full name | Attributes |",
                "| --- | --- | --- |"
            ]
        );
    }

    #[test]
    fn a_pipe_in_a_name_does_not_split_the_row() {
        let awkward = User {
            full_name: Some("a|b".into()),
            ..user()
        };
        let out = table(&[awkward]);
        let row = out.lines().nth(2).unwrap();
        assert!(row.contains("a\\|b"), "{row}");
        assert_eq!(row.matches(" | ").count(), 2, "{row}");
    }

    /// The backslash has to be doubled before the pipe is escaped, or the
    /// escape ends up escaping the backslash and the pipe goes in bare.
    #[test]
    fn a_backslash_is_doubled_before_the_pipe_is_escaped() {
        let awkward = User {
            full_name: Some("a\\|b".into()),
            ..user()
        };
        let out = table(&[awkward]);
        assert!(out.contains("a\\\\\\|b"), "{out}");
    }

    #[test]
    fn a_newline_becomes_a_space() {
        let awkward = User {
            full_name: Some("two\nlines".into()),
            ..user()
        };
        let out = table(&[awkward]);
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.contains("two lines"), "{out}");
    }

    #[test]
    fn the_summary_names_the_account_and_its_counts() {
        let out = summary(
            "someone",
            false,
            None,
            &[("Unfollowers", 32), ("Fans", 197)],
        );
        assert!(out.contains("# Account summary of @someone"), "{out}");
        assert!(out.contains("| Unfollowers | 32 |"), "{out}");
        assert!(out.contains("| Fans | 197 |"), "{out}");
        assert!(!out.contains("Filters are active"), "{out}");
    }

    #[test]
    fn the_summary_says_when_filters_shaped_it() {
        let out = summary("someone", true, None, &[("Fans", 1)]);
        assert!(out.contains("Filters are active"), "{out}");
    }

    /// The people you both know go above the table, and their names are
    /// escaped like every other value that came from a server.
    #[test]
    fn the_summary_can_open_with_who_you_both_know() {
        let out = summary(
            "someone",
            false,
            Some("Followed by @ana and @lu|is"),
            &[("Fans", 1)],
        );
        let heading = out.find("# Account").unwrap();
        let line = out.find("Followed by").unwrap();
        let table = out.find("| Count").unwrap();
        assert!(heading < line && line < table, "{out}");
        assert!(out.contains("@lu\\|is"), "{out}");
    }

    #[test]
    fn a_missing_name_leaves_the_cell_empty() {
        let nameless = User {
            full_name: None,
            ..user()
        };
        let out = table(&[nameless]);
        assert!(out.lines().nth(2).unwrap().contains("|  |"), "{out}");
    }
}
