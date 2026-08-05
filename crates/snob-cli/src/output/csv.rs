//! Comma-separated rows.
//!
//! The columns are the six JSON keys of `User`, in the order the struct
//! declares them. JSON leaves an unknown attribute out; a table cannot, so
//! here it is an empty field. That keeps every row the same width, which is
//! the whole point of the format.

use std::borrow::Cow;

use anyhow::{Context, Result};
use snob_core::model::User;

/// The column names. Frozen along with the JSON keys they mirror.
const HEADER: [&str; 6] = [
    "pk",
    "username",
    "full_name",
    "is_private",
    "is_verified",
    "pfp_url",
];

/// One row per account.
pub(crate) fn rows(users: &[User]) -> Result<String> {
    let mut writer = csv::WriterBuilder::new().from_writer(Vec::new());
    writer
        .write_record(HEADER)
        .context("could not write the csv header")?;

    for user in users {
        let fields: [Cow<'_, str>; 6] = [
            Cow::Owned(user.pk.to_string()),
            defuse(&user.username),
            optional(user.full_name.as_deref()),
            flag(user.is_private),
            flag(user.is_verified),
            optional(user.pfp_url.as_deref()),
        ];
        writer
            .write_record(fields.iter().map(Cow::as_ref))
            .with_context(|| format!("could not write the row for {}", user.username))?;
    }

    finish(writer)
}

/// A header and one row, for the summary, which is counts rather than
/// accounts.
pub(crate) fn single_row(header: &[&str], row: &[String]) -> Result<String> {
    let mut writer = csv::WriterBuilder::new().from_writer(Vec::new());
    writer
        .write_record(header)
        .context("could not write the csv header")?;
    let fields: Vec<Cow<'_, str>> = row.iter().map(|field| defuse(field)).collect();
    writer
        .write_record(fields.iter().map(Cow::as_ref))
        .context("could not write the csv row")?;
    finish(writer)
}

fn finish(writer: csv::Writer<Vec<u8>>) -> Result<String> {
    let bytes = writer
        .into_inner()
        .context("could not finish the csv")?
        .to_vec();
    String::from_utf8(bytes).context("the csv came out as invalid UTF-8")
}

/// An unknown attribute is an empty field, never `false`: not knowing whether
/// an account is private is not the same as knowing it is not.
fn flag(value: Option<bool>) -> Cow<'static, str> {
    match value {
        Some(true) => Cow::Borrowed("true"),
        Some(false) => Cow::Borrowed("false"),
        None => Cow::Borrowed(""),
    }
}

fn optional(value: Option<&str>) -> Cow<'_, str> {
    value.map(defuse).unwrap_or(Cow::Borrowed(""))
}

/// Stops a spreadsheet from reading a name as a formula.
///
/// Excel and LibreOffice evaluate any cell that opens with `=`, `+`, `-` or
/// `@`, and a full name is whatever its owner typed. A leading apostrophe is
/// the marker both of them read as "this is text"; it costs one character of
/// fidelity in a field that is decoration anyway. The tab and carriage return
/// are here because they can slip past a naive check and still reach the
/// parser as the start of a cell.
fn defuse(field: &str) -> Cow<'_, str> {
    match field.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => Cow::Owned(format!("'{field}")),
        _ => Cow::Borrowed(field),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Vec<User> {
        vec![
            User {
                pk: 1,
                username: "one".into(),
                full_name: Some("One Person".into()),
                is_private: Some(false),
                is_verified: Some(true),
                pfp_url: Some("https://example.test/a.jpg".into()),
            },
            User {
                pk: 2,
                username: "two".into(),
                full_name: None,
                is_private: None,
                is_verified: None,
                pfp_url: None,
            },
        ]
    }

    fn parse(text: &str) -> Vec<Vec<String>> {
        csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(text.as_bytes())
            .records()
            .map(|record| record.unwrap().iter().map(str::to_string).collect())
            .collect()
    }

    #[test]
    fn the_header_names_the_six_frozen_keys() {
        let out = rows(&users()).unwrap();
        assert_eq!(
            out.lines().next().unwrap(),
            "pk,username,full_name,is_private,is_verified,pfp_url"
        );
    }

    #[test]
    fn an_unknown_attribute_is_empty_not_false() {
        let records = parse(&rows(&users()).unwrap());
        assert!(records.iter().all(|r| r.len() == 6), "{records:?}");

        let known = &records[1];
        assert_eq!(known[3], "false");
        assert_eq!(known[4], "true");

        let unknown = &records[2];
        assert_eq!(unknown[2], "");
        assert_eq!(unknown[3], "");
        assert_eq!(unknown[5], "");
    }

    #[test]
    fn a_full_name_with_commas_quotes_and_newlines_survives() {
        let awkward = "Comma, \"quote\" and\na newline";
        let user = User {
            full_name: Some(awkward.into()),
            ..users()[0].clone()
        };
        let records = parse(&rows(&[user]).unwrap());
        assert_eq!(records[1][2], awkward);
    }

    #[test]
    fn a_name_that_looks_like_a_formula_is_defused() {
        for dangerous in ["=1+1", "+1", "-1", "@SUM(A1)"] {
            let user = User {
                full_name: Some(dangerous.into()),
                ..users()[0].clone()
            };
            let records = parse(&rows(&[user]).unwrap());
            assert_eq!(records[1][2], format!("'{dangerous}"), "{dangerous}");
        }
    }

    #[test]
    fn an_ordinary_name_is_left_alone() {
        let records = parse(&rows(&users()).unwrap());
        assert_eq!(records[1][2], "One Person");
    }

    #[test]
    fn an_empty_list_is_just_the_header() {
        let out = rows(&[]).unwrap();
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn a_single_row_carries_its_own_header() {
        let out = single_row(&["a", "b"], &["1".into(), "2".into()]).unwrap();
        assert_eq!(out.lines().collect::<Vec<_>>(), vec!["a,b", "1,2"]);
    }
}
