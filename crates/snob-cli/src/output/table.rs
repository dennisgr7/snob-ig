//! The human-facing shape of a list: what the terminal draws, and what the
//! markdown export mirrors.
//!
//! The machine formats spell out all six fields of an account. This one shows
//! the three things a person reads a list for — who it is, what they are
//! called, and what stands out about them.

use comfy_table::{Attribute as Style, Cell, ContentArrangement, Table, presets};
use snob_core::filters::Attribute;
use snob_core::model::User;

use super::Presentation;

/// The column names, shared by the terminal table and the markdown one so the
/// two never drift apart.
pub(crate) const HEADER: [&str; 3] = ["Username", "Full name", "Attributes"];

/// Where the addresses are, said once for a terminal that cannot put them
/// behind the names.
///
/// A column of them would be the same twenty-six characters on every row,
/// wrapped over two lines, next to the username it already repeats.
const PROFILE_NOTE: &str = "Profiles: https://www.instagram.com/<username>";

/// The real table, for a terminal someone is reading.
///
/// An empty list draws nothing rather than an empty frame: the summary on
/// standard error already says the count was zero.
///
/// **Drawn, then checked, and drawn again without links if the check fails.**
/// A linked username is a short name wrapped in about fifty-five characters of
/// invisible address. comfy-table *measures* that cell with a parser that knows
/// what OSC 8 is, and then *wraps* it with one that does not — `console`'s
/// `AnsiCodeIterator` has a transition for `ESC [` and none for `ESC ]` — so
/// once a column is narrow enough to wrap, the address is cut in the middle and
/// the opener reaches the terminal with its terminator several drawn lines
/// away. Everything in between, borders and other columns and following rows,
/// is swallowed into the link. No error and no exit code, on a 70-column
/// terminal with an ordinary name.
///
/// Predicting the arrangement here would mean reimplementing it. Asking the
/// drawn table whether the links came out whole is exact, costs one linear
/// scan, and keeps being true when comfy-table changes its mind. The fallback
/// is the branch that already exists for terminals with no link support: the
/// plain name, and the address named once underneath.
pub(crate) fn table(users: &[User], presentation: Presentation) -> String {
    if users.is_empty() {
        return String::new();
    }

    if presentation.hyperlinks {
        let linked = draw(users, presentation);
        if links_are_whole(&linked) {
            return linked;
        }
    }
    draw(
        users,
        Presentation {
            hyperlinks: false,
            ..presentation
        },
    )
}

/// Whether every hyperlink in a drawn table is still in one piece.
///
/// An opener is whole when its terminator is on the same drawn line. Nothing
/// else in a cell can hold an `ESC`: both strings a row is built from go
/// through `printable` first, which is what makes this a question about the
/// layout rather than about the accounts.
fn links_are_whole(drawn: &str) -> bool {
    drawn.lines().all(|line| {
        line.match_indices("\x1b]8;;")
            .all(|(at, _)| line[at..].contains("\x1b\\"))
    })
}

fn draw(users: &[User], presentation: Presentation) -> String {
    let mut table = Table::new();
    table.load_preset(presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    if let Some(width) = presentation.width {
        table.set_width(width);
    }

    table.set_header(HEADER.iter().map(|name| {
        let cell = Cell::new(name);
        if presentation.color {
            cell.add_attribute(Style::Bold)
        } else {
            cell
        }
    }));

    // Without this, comfy-table drops styling whenever it cannot see a
    // terminal, which in a test is always. Deciding that is this crate's job,
    // and the answer already travels in `presentation`.
    if presentation.color {
        table.enforce_styling();
    }

    for user in users {
        // Both come from somebody else's profile, so both go through the
        // filter that takes out what a terminal would obey rather than show.
        let name = user.safe_username();
        let username = if presentation.hyperlinks {
            hyperlink(&name, &user.profile_url())
        } else {
            name
        };
        table.add_row(vec![
            username,
            user.safe_full_name().unwrap_or_default(),
            attributes(user),
        ]);
    }

    let mut out = table.to_string();
    out.push('\n');
    if !presentation.hyperlinks {
        out.push_str(PROFILE_NOTE);
        out.push('\n');
    }
    out
}

/// Wraps text in an OSC 8 hyperlink.
///
/// The sequence is understood by Windows Terminal, iTerm2, WezTerm, kitty,
/// Ghostty, Alacritty and GNOME Terminal. Whether the terminal at the other
/// end is one of those is not decided here.
fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// The marks an account carries, spelled the way `--hide` takes them.
///
/// Worked out with the very predicate the filters use, so what a row shows and
/// what `--hide verified` would drop can never disagree.
pub(crate) fn attributes(user: &User) -> String {
    [
        (Attribute::Verified, "verified"),
        (Attribute::Private, "private"),
        (Attribute::NoPfp, "no-pfp"),
    ]
    .into_iter()
    .filter(|(attribute, _)| attribute.matches(user))
    .map(|(_, name)| name)
    .collect::<Vec<_>>()
    .join(", ")
}

/// One name per line, no decoration.
///
/// The contract for pipes and files: a script reading `snob unfollowers` has
/// always got this, and the real table arriving for terminals does not change
/// it.
///
/// Filtered like the drawn table above. "Not a terminal" is where the output
/// goes, not where it ends up: `snob followers > people.txt` is followed by
/// somebody reading `people.txt`, and `| less -r` is a terminal with one step
/// in between.
pub(crate) fn plain(users: &[User]) -> String {
    let mut out = String::new();
    for user in users {
        out.push_str(&user.safe_username());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> User {
        User {
            pk: 1,
            username: "one".into(),
            full_name: Some("One Person".into()),
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }
    }

    #[test]
    fn an_account_with_nothing_to_say_has_no_marks() {
        assert_eq!(attributes(&user()), "");
    }

    #[test]
    fn the_marks_are_spelled_the_way_the_filters_take_them() {
        let flagged = User {
            is_verified: Some(true),
            is_private: Some(true),
            ..user()
        };
        assert_eq!(attributes(&flagged), "verified, private");
    }

    #[test]
    fn a_default_avatar_counts_as_no_picture() {
        let hash = snob_core::filters::DEFAULT_AVATAR_HASHES[0];
        let plain_avatar = User {
            pfp_url: Some(format!("https://example.test/{hash}.jpg")),
            ..user()
        };
        assert_eq!(attributes(&plain_avatar), "no-pfp");
    }

    #[test]
    fn plain_is_one_name_per_line() {
        let two = vec![
            user(),
            User {
                username: "two".into(),
                ..user()
            },
        ];
        assert_eq!(plain(&two), "one\ntwo\n");
        assert_eq!(plain(&[]), "");
    }

    fn looking_at_a_terminal() -> Presentation {
        Presentation {
            interactive: true,
            hyperlinks: true,
            color: true,
            width: Some(80),
        }
    }

    #[test]
    fn the_table_has_headers_and_a_frame() {
        let out = table(&[user()], looking_at_a_terminal());
        assert!(out.contains("Username"), "{out}");
        assert!(out.contains("Attributes"), "{out}");
        assert!(out.contains('─'), "{out}");
    }

    #[test]
    fn a_username_carries_a_link_when_the_terminal_takes_one() {
        let out = table(&[user()], looking_at_a_terminal());
        assert!(
            out.contains("\x1b]8;;https://www.instagram.com/one/\x1b\\one\x1b]8;;\x1b\\"),
            "{out:?}"
        );
        assert!(
            !out.contains(PROFILE_NOTE),
            "spelling the address out is redundant here"
        );
    }

    /// The address still has to be reachable, but a column of it would be the
    /// same prefix on every row beside the username it repeats. It is said
    /// once instead, and the table keeps its width.
    #[test]
    fn without_link_support_the_address_is_named_once() {
        let presentation = Presentation {
            hyperlinks: false,
            ..looking_at_a_terminal()
        };
        let out = table(&[user()], presentation);
        assert!(out.contains(PROFILE_NOTE), "{out}");
        assert_eq!(out.matches("instagram.com").count(), 1, "{out}");
        assert!(!out.contains("\x1b]8"), "{out:?}");
    }

    /// Color and links are separate questions. `NO_COLOR` takes the styling
    /// away; it does not make a clickable name unclickable, nor flatten the
    /// table into a list.
    #[test]
    fn without_color_the_links_and_the_frame_stay() {
        let presentation = Presentation {
            color: false,
            ..looking_at_a_terminal()
        };
        let out = table(&[user()], presentation);
        // `ESC [` is styling; `ESC ]` is the hyperlink.
        assert!(!out.contains("\x1b["), "{out:?}");
        assert!(out.contains("\x1b]8"), "{out:?}");
        assert!(out.contains('─'), "{out}");
    }

    #[test]
    fn with_color_the_header_is_styled() {
        let out = table(&[user()], looking_at_a_terminal());
        assert!(out.contains("\x1b["), "{out:?}");
    }

    /// The link is either whole or absent, at every width.
    ///
    /// comfy-table measures an OSC 8 cell with a parser that understands it and
    /// wraps it with one that does not, so a column narrow enough to wrap cuts
    /// the address in half: the opener reaches the terminal and its terminator
    /// arrives several drawn lines later, with the borders and the other
    /// columns in between swallowed into the link. An ordinary name on a
    /// 70-column terminal is enough.
    #[test]
    fn a_wrapped_column_never_cuts_a_link_in_half() {
        let long = User {
            username: "a_long_but_ordinary_name".into(),
            full_name: Some("A very long name that will not fit on one line at all".into()),
            is_verified: Some(true),
            is_private: Some(true),
            ..user()
        };

        let mut fell_back = 0;
        for width in [40, 50, 60, 70, 80, 100, 120] {
            let out = table(
                std::slice::from_ref(&long),
                Presentation {
                    width: Some(width),
                    ..looking_at_a_terminal()
                },
            );

            for line in out.lines() {
                for (at, _) in line.match_indices("\x1b]8;;") {
                    assert!(
                        line[at..].contains("\x1b\\"),
                        "at {width} columns a link runs off the end of its line: {line:?}"
                    );
                }
            }

            if out.contains(PROFILE_NOTE) {
                fell_back += 1;
                assert!(
                    !out.contains("\x1b]8"),
                    "the fallback says the address once instead of linking it: {out:?}"
                );
                // Not a third rendering: exactly what a terminal with no link
                // support would have been given at this width.
                let plainly = table(
                    std::slice::from_ref(&long),
                    Presentation {
                        hyperlinks: false,
                        width: Some(width),
                        ..looking_at_a_terminal()
                    },
                );
                assert_eq!(out, plainly, "at {width} columns");
            }
        }

        assert!(
            fell_back > 0,
            "the narrow widths are the ones this test is about"
        );
    }

    /// A width the name fits in keeps the link, which is what the fallback
    /// costs and therefore what it must not do more often than it has to.
    #[test]
    fn a_column_wide_enough_still_links() {
        let out = table(&[user()], looking_at_a_terminal());
        assert!(out.contains("\x1b]8;;"), "{out:?}");
        assert!(!out.contains(PROFILE_NOTE), "{out}");
    }

    #[test]
    fn an_empty_list_draws_nothing() {
        assert_eq!(table(&[], looking_at_a_terminal()), "");
    }

    /// A link is invisible characters wrapped around a short name. If they
    /// were counted as width the columns would come out crooked.
    ///
    /// The frame is what is measured, not everything printed: `PROFILE_NOTE` is
    /// one sentence under the table rather than a column of it, it is 45
    /// characters whatever the terminal is, and it soft-wraps like any other
    /// line of prose.
    #[test]
    fn a_narrow_terminal_is_respected_even_with_links() {
        let long = User {
            username: "a".repeat(60),
            full_name: Some("A very long name that will not fit on one line".into()),
            ..user()
        };
        let presentation = Presentation {
            width: Some(40),
            ..looking_at_a_terminal()
        };
        let out = table(&[long], presentation);
        for line in out.lines().filter(|l| !l.starts_with("Profiles:")) {
            let visible = strip_escapes(line).chars().count();
            assert!(visible <= 40, "{visible} columns: {line:?}");
        }
    }

    /// Removes the OSC 8 sequences so a line can be measured the way a
    /// terminal would show it.
    fn strip_escapes(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            // Both forms end at the string terminator, `ESC \`.
            for inner in chars.by_ref() {
                if inner == '\\' || inner == 'm' {
                    break;
                }
            }
        }
        out
    }

    /// An account name is the cheapest way into this program: anyone who
    /// follows you gets to choose one. A row must not be able to hide behind a
    /// link to somewhere else, nor rewrite the line above it.
    #[test]
    fn a_hostile_name_cannot_drive_the_terminal() {
        let link_in_a_name = User {
            username: "someone".into(),
            full_name: Some(format!(
                "{esc}]8;;http://evil.test{esc}\\Official{esc}]8;;{esc}\\",
                esc = '\x1b'
            )),
            ..user()
        };
        let out = table(&[link_in_a_name], looking_at_a_terminal());
        // The address may survive as text — it is now just characters in a
        // name — but it must not survive as a sequence the terminal obeys.
        assert!(!out.contains("\x1b]8;;http://evil.test"), "{out:?}");
        // The one link in the row is the one this table put there.
        assert_eq!(out.matches("\x1b]8").count(), 2, "{out:?}");

        let erases_the_line_above = User {
            full_name: Some(format!("clean{esc}[2K{esc}[A", esc = '\x1b')),
            ..user()
        };
        let plain = Presentation {
            hyperlinks: false,
            color: false,
            ..looking_at_a_terminal()
        };
        let out = table(&[erases_the_line_above], plain);
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(out.contains("clean"), "{out}");
    }

    /// The same attack through the field next door, which was open.
    ///
    /// The visible text went through `safe_username`, but the address beside it
    /// came from `profile_url`, which pasted the name in raw. An `ESC` there
    /// closed the sequence early and the rest of the name opened one of its
    /// own, so the cell read as a filtered name and pointed somewhere else —
    /// the split between what is shown and where it goes that the test above
    /// exists to prevent.
    #[test]
    fn a_hostile_username_cannot_drive_the_terminal_through_its_link() {
        let link_in_a_username = User {
            username: format!(
                "a{esc}\\{esc}]8;;http://evil.test{esc}\\Official",
                esc = '\x1b'
            ),
            ..user()
        };
        let out = table(&[link_in_a_username], looking_at_a_terminal());

        assert!(!out.contains("\x1b]8;;http://evil.test"), "{out:?}");
        assert_eq!(
            out.matches("\x1b]8").count(),
            2,
            "the only link in the row is the one this table put there: {out:?}"
        );
    }

    /// Taking control characters out must not take the language with them.
    #[test]
    fn ordinary_names_survive_untouched() {
        let accented = User {
            username: "jose".into(),
            full_name: Some("Jos\u{e9} Mu\u{f1}oz \u{4e2d}\u{6587}".into()),
            ..user()
        };
        let out = table(&[accented], looking_at_a_terminal());
        assert!(
            out.contains("Jos\u{e9} Mu\u{f1}oz \u{4e2d}\u{6587}"),
            "{out}"
        );
    }

    #[test]
    fn wide_characters_do_not_panic() {
        let cjk = User {
            full_name: Some("\u{4e2d}\u{6587}\u{540d}\u{5b57}".into()),
            ..user()
        };
        let out = table(&[cjk], looking_at_a_terminal());
        assert!(out.contains('─'), "{out}");
    }
}
