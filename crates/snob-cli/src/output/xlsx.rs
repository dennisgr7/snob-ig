//! Spreadsheet workbooks.
//!
//! A list of accounts is the same six columns as the csv, so the two formats
//! answer alike, plus what a spreadsheet can carry and a text file cannot: real
//! booleans, a numeric id, and a username that is a link to the profile.
//!
//! `scan`'s summary is one row rather than a list, and there the two formats
//! diverge in exactly one cell each: the moment a list was taken is epoch
//! seconds in the csv, matching the JSON, and a real date here, because a
//! column of epoch integers in a spreadsheet is unreadable. `scan::row_cells`
//! is where that is decided and this is the claim it points at, so the two say
//! the same thing about it.
//!
//! What goes in each cell is decided first, as plain data, and only then
//! handed to the writer. A workbook is a zip archive, so that split is what
//! makes the interesting half testable without unpacking anything.

use anyhow::{Context, Result};
use rust_xlsxwriter::{ExcelDateTime, Format, FormatAlign, Workbook, Worksheet};
use snob_core::Epoch;
use snob_core::Pk;
use snob_core::model::{User, printable};

use crate::output::USER_COLUMNS as HEADER;

/// Excel refuses a string longer than this.
const MAX_TEXT: usize = 32_767;

/// Excel stores at most this many hyperlinks in one worksheet. Past it the
/// usernames are still there, just not clickable.
const MAX_URLS: usize = 65_530;

/// Above 2^53 a float stops being able to hold every integer, so an id that
/// large goes in as text rather than silently losing its last digits.
const MAX_EXACT_INTEGER: u64 = 1 << 53;

/// What a cell holds, worked out before any workbook exists.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cell {
    Number(f64),
    Text(String),
    Bool(bool),
    Link {
        url: String,
        text: String,
    },
    /// A moment, as epoch seconds, written as a date the spreadsheet
    /// understands. Only `scan` uses it, and only because a column of epoch
    /// integers in a spreadsheet is unreadable where the same number in csv is
    /// exactly what a script wants.
    DateTime(Epoch),
    Empty,
}

/// One row per account.
pub(crate) fn rows(users: &[User]) -> Vec<[Cell; 6]> {
    rows_with_url_cap(users, MAX_URLS)
}

/// Split out from [`rows`] so the hyperlink cap can be exercised without
/// building sixty-five thousand accounts.
fn rows_with_url_cap(users: &[User], cap: usize) -> Vec<[Cell; 6]> {
    users
        .iter()
        .enumerate()
        .map(|(index, user)| {
            // Filtered like every other name that gets drawn. Not only because
            // a workbook is opened and read: characters below 0x20 other than
            // tab, newline and carriage return are **illegal in XML 1.0**, so a
            // hostile name here produced a file that would not open at all.
            // The address is `profile_url`'s problem, and it encodes.
            let username = if index < cap {
                Cell::Link {
                    url: user.profile_url(),
                    text: user.safe_username(),
                }
            } else {
                Cell::Text(user.safe_username())
            };
            [
                number(user.pk),
                username,
                text(user.full_name.as_deref()),
                flag(user.is_private),
                flag(user.is_verified),
                // The picture goes in as text on purpose: it is not worth
                // spending the workbook's hyperlink budget on, and a cell
                // Excel does not treat as a link cannot surprise anyone.
                text(user.pfp_url.as_deref()),
            ]
        })
        .collect()
}

fn number(pk: Pk) -> Cell {
    if pk.get() < MAX_EXACT_INTEGER {
        Cell::Number(pk.get() as f64)
    } else {
        Cell::Text(pk.to_string())
    }
}

/// Filtered before it is measured, for the reason `csv::clean` gives: a full
/// name is whatever its owner typed, and half of the control characters cannot
/// legally appear in the XML a workbook is made of.
///
/// No leading apostrophe, unlike the csv. That defuses a *formula*, and a
/// formula is a hazard the csv has because a spreadsheet re-parses the text it
/// imports. `write_string` writes a string cell, which Excel never evaluates.
fn text(value: Option<&str>) -> Cell {
    match value {
        Some(value) => Cell::Text(truncate(&printable(value))),
        None => Cell::Empty,
    }
}

/// An unknown attribute leaves the cell empty, never `FALSE`: not knowing
/// whether an account is private is not the same as knowing it is not.
fn flag(value: Option<bool>) -> Cell {
    match value {
        Some(value) => Cell::Bool(value),
        None => Cell::Empty,
    }
}

/// Cuts on a character boundary. A name long enough to hit this is nonsense
/// anyway, and losing its tail beats failing to write the file.
fn truncate(value: &str) -> String {
    if value.len() <= MAX_TEXT {
        return value.to_string();
    }
    value.chars().take(MAX_TEXT).collect()
}

/// The finished workbook, as the bytes of a zip archive.
pub(crate) fn workbook(users: &[User]) -> Result<Vec<u8>> {
    build(&HEADER, &rows(users))
}

/// A header and one row, for the summary, which is counts rather than
/// accounts.
pub(crate) fn single_row_workbook(header: &[&str], row: Vec<Cell>) -> Result<Vec<u8>> {
    build(header, &[row])
}

fn build<R: AsRef<[Cell]>>(header: &[&str], rows: &[R]) -> Result<Vec<u8>> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    write_header(sheet, header)?;

    for (row, cells) in rows.iter().enumerate() {
        // Row zero is the header, so the data starts one below it.
        let row = row as u32 + 1;
        for (column, cell) in cells.as_ref().iter().enumerate() {
            write_cell(sheet, row, column as u16, cell)?;
        }
    }

    sheet.autofit();
    workbook
        .save_to_buffer()
        .context("could not build the spreadsheet")
}

fn write_header(sheet: &mut Worksheet, header: &[&str]) -> Result<()> {
    let style = Format::new().set_bold().set_align(FormatAlign::Left);
    for (column, name) in header.iter().enumerate() {
        sheet
            .write_string_with_format(0, column as u16, *name, &style)
            .context("could not write the spreadsheet header")?;
    }
    // Keeps the header in view while scrolling a long list.
    sheet
        .set_freeze_panes(1, 0)
        .context("could not freeze the header")?;
    Ok(())
}

/// Turns epoch seconds into what the spreadsheet writer takes, in UTC like
/// every other timestamp this tool prints.
///
/// The writer converts this itself, and range-checks it against the years a
/// spreadsheet can hold (1900-9999) on the way. Doing the arithmetic here meant
/// the "outside what a spreadsheet can hold" fallback rested on two chained
/// `.ok()?` calls rather than on one documented check.
fn datetime(at: Epoch) -> Option<ExcelDateTime> {
    ExcelDateTime::from_timestamp(at.get()).ok()
}

fn write_cell(sheet: &mut Worksheet, row: u32, column: u16, cell: &Cell) -> Result<()> {
    match cell {
        Cell::Number(value) => sheet.write_number(row, column, *value).map(|_| ()),
        Cell::Text(value) => sheet.write_string(row, column, value).map(|_| ()),
        Cell::Bool(value) => sheet.write_boolean(row, column, *value).map(|_| ()),
        Cell::Link { url, text } => sheet
            .write_url_with_text(row, column, url.as_str(), text.as_str())
            .map(|_| ()),
        // A timestamp outside what a spreadsheet can hold is written as the
        // number it is rather than dropped: wrong-looking beats absent, and
        // nothing else in this file invents a value.
        Cell::DateTime(at) => match datetime(*at) {
            Some(value) => {
                let format = Format::new().set_num_format("yyyy-mm-dd hh:mm");
                sheet
                    .write_datetime_with_format(row, column, &value, &format)
                    .map(|_| ())
            }
            None => sheet.write_number(row, column, at.get() as f64).map(|_| ()),
        },
        Cell::Empty => Ok(()),
    }
    .with_context(|| format!("could not write row {row}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Vec<User> {
        vec![
            User {
                pk: Pk::new(1),
                username: "one".into(),
                full_name: Some("One Person".into()),
                is_private: Some(false),
                is_verified: Some(true),
                pfp_url: Some("https://example.test/a.jpg".into()),
            },
            User {
                pk: Pk::new(2),
                username: "two".into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        ]
    }

    #[test]
    fn a_username_links_to_its_profile() {
        let rows = rows(&users());
        assert_eq!(
            rows[0][1],
            Cell::Link {
                url: "https://www.instagram.com/one/".into(),
                text: "one".into(),
            }
        );
    }

    /// Not only about what a reader sees. Characters below 0x20 other than tab,
    /// newline and carriage return are illegal in XML 1.0, and a workbook is
    /// XML — so a name carrying one produced a file that would not open.
    #[test]
    fn a_name_that_would_break_the_file_is_filtered_before_it_is_written() {
        let rows = rows(&[User {
            pk: Pk::new(1),
            username: format!("one{esc}[2K", esc = '\x1b'),
            full_name: Some(format!("A{esc}[A Person", esc = '\x1b')),
            is_private: None,
            is_verified: None,
            pfp_url: None,
        }]);

        let Cell::Link { url, text } = &rows[0][1] else {
            panic!("the username cell is a link: {:?}", rows[0][1]);
        };
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(!url.contains('\x1b'), "{url:?}");
        // The brackets stay, as text. `printable` removes what a terminal obeys
        // and what XML forbids, not what either of them would happily print.
        assert_eq!(rows[0][2], Cell::Text("A[A Person".into()));
    }

    #[test]
    fn an_unknown_attribute_leaves_the_cell_empty() {
        let rows = rows(&users());
        assert_eq!(rows[0][3], Cell::Bool(false));
        assert_eq!(rows[0][4], Cell::Bool(true));
        assert_eq!(rows[1][2], Cell::Empty);
        assert_eq!(rows[1][3], Cell::Empty);
        assert_eq!(rows[1][5], Cell::Empty);
    }

    #[test]
    fn the_id_is_a_number() {
        assert_eq!(rows(&users())[0][0], Cell::Number(1.0));
    }

    /// Past 2^53 a float cannot hold every integer, and an id that quietly
    /// loses its last digits is worse than one that is not a number.
    #[test]
    fn an_id_too_large_for_a_float_goes_in_as_text() {
        let user = User {
            pk: Pk::new((1 << 53) + 1),
            ..users()[0].clone()
        };
        assert_eq!(rows(&[user])[0][0], Cell::Text("9007199254740993".into()));
    }

    #[test]
    fn past_the_hyperlink_cap_the_usernames_are_plain_text() {
        let three = vec![users()[0].clone(), users()[1].clone(), users()[0].clone()];
        let rows = rows_with_url_cap(&three, 2);
        assert!(matches!(rows[0][1], Cell::Link { .. }));
        assert!(matches!(rows[1][1], Cell::Link { .. }));
        assert_eq!(rows[2][1], Cell::Text("one".into()));
    }

    #[test]
    fn an_absurd_name_is_cut_rather_than_refused() {
        let user = User {
            full_name: Some("x".repeat(40_000)),
            ..users()[0].clone()
        };
        match &rows(&[user])[0][2] {
            Cell::Text(value) => assert_eq!(value.chars().count(), MAX_TEXT),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// Multi-byte characters must not be cut in half.
    #[test]
    fn cutting_a_name_lands_on_a_character_boundary() {
        let name = "\u{4e2d}".repeat(40_000);
        let user = User {
            full_name: Some(name),
            ..users()[0].clone()
        };
        match &rows(&[user])[0][2] {
            Cell::Text(value) => assert!(value.chars().count() <= MAX_TEXT),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn the_workbook_is_a_zip_archive() {
        let bytes = workbook(&users()).unwrap();
        assert_eq!(&bytes[..4], b"PK\x03\x04");
    }

    #[test]
    fn an_empty_list_still_makes_a_readable_file() {
        let bytes = workbook(&[]).unwrap();
        assert_eq!(&bytes[..4], b"PK\x03\x04");
    }

    /// A zip stores its entry names uncompressed, so the shape of the archive
    /// can be checked without unpacking it. The relationships file only
    /// exists when the sheet has hyperlinks in it, which is what makes the
    /// usernames clickable once the file is opened.
    #[test]
    fn the_archive_holds_a_worksheet_and_its_links() {
        let bytes = workbook(&users()).unwrap();
        for entry in [
            &b"xl/worksheets/sheet1.xml"[..],
            &b"xl/worksheets/_rels/sheet1.xml.rels"[..],
        ] {
            assert!(
                bytes.windows(entry.len()).any(|w| w == entry),
                "{} is missing",
                String::from_utf8_lossy(entry)
            );
        }
    }
}
