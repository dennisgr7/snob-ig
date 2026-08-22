//! Writing the result.
//!
//! Data goes to standard output and everything else to standard error, so that
//! redirecting to a file or piping into another process works without any
//! coordination with the progress bar.
//!
//! Each format is rendered by a pure function that returns the finished bytes,
//! and every command hands them to the one sink below.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use snob_core::model::{User, printable};

use crate::cli::Format;
use crate::ui;

pub(crate) mod csv;
pub(crate) mod md;
pub(crate) mod table;
pub(crate) mod xlsx;

/// How the result will be seen.
///
/// Worked out once per run and passed down, so the renderers stay pure
/// functions a test can drive without touching the environment or the
/// terminal it happens to be running in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// The result lands on a terminal a person is looking at: no `-o`, and
    /// standard output is not redirected.
    pub interactive: bool,
    /// That terminal understands OSC 8 hyperlinks.
    pub hyperlinks: bool,
    /// Styling is welcome there.
    pub color: bool,
    /// Width to lay a table out in. `None` lets the table library measure the
    /// terminal itself; tests set it to stay deterministic.
    pub width: Option<u16>,
}

impl Presentation {
    /// Reads the environment. The destination comes first: a file is never
    /// interactive, whatever the terminal supports.
    ///
    /// `FORCE_HYPERLINK` and `NO_COLOR` are honored by the two crates behind
    /// this, so neither variable is read here.
    pub fn detect(destination: Option<&Path>) -> Self {
        let interactive =
            destination.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout());
        Self {
            interactive,
            hyperlinks: interactive && supports_hyperlinks::on(supports_hyperlinks::Stream::Stdout),
            color: interactive && console::colors_enabled(),
            width: None,
        }
    }

    /// Everything off: what a pipe, a file and most tests want.
    pub fn plain() -> Self {
        Self {
            interactive: false,
            hyperlinks: false,
            color: false,
            width: None,
        }
    }
}

/// What a renderer produces. Text for everything a terminal or an editor can
/// read; bytes for a spreadsheet, which is a zip archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendered {
    Text(String),
    Bytes(Vec<u8>),
}

impl Rendered {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Bytes(bytes) => bytes,
        }
    }
}

/// Picks the format: whatever was asked for, then what the destination's
/// extension implies, and failing both a table on a terminal and JSON in a
/// pipe.
///
/// The extension matters because `-o list.csv` with no `--format` used to
/// write one name per line into a file named like a spreadsheet.
pub fn effective_format(requested: Option<Format>, destination: Option<&Path>) -> Format {
    if let Some(format) = requested {
        return format;
    }
    if let Some(format) = destination.and_then(format_from_extension) {
        return format;
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        Format::Table
    } else {
        Format::Json
    }
}

fn format_from_extension(path: &Path) -> Option<Format> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "csv" => Some(Format::Csv),
        "xlsx" => Some(Format::Xlsx),
        "md" | "markdown" => Some(Format::Md),
        "json" => Some(Format::Json),
        "ndjson" | "jsonl" => Some(Format::Ndjson),
        // Anything else keeps the old behavior: `-o notes.txt` is still a
        // plain list, not a guess.
        _ => None,
    }
}

/// Refuses up front what would only fail once the walk had already been paid
/// for. Called before the first request, never after.
pub fn check_destination(format: Format, destination: Option<&Path>) -> Result<()> {
    if format == Format::Xlsx && destination.is_none() {
        return Err(anyhow!(
            "the \"xlsx\" format is a binary file; write it with -o (for example -o result.xlsx)"
        ));
    }
    Ok(())
}

/// Builds a file name for a result nobody named, and refuses the ones that
/// would not behave as files.
///
/// The stem comes from a server rather than from what anyone typed, so it is
/// checked before it becomes a path: no separators, no leading dot, and none of
/// the names Windows resolves to devices whatever extension they carry — that
/// last one is how a download reports success while going to the null device.
///
/// An existing file is never overwritten here. With `-o` the user picked the
/// name and replacing it is their call; with this one they did not.
///
/// `in_dir` is where that check looks, and it is a parameter because it used to
/// be the **process's** working directory. That is shared state: the test for
/// this walked into a temporary directory with `set_current_dir` while a
/// sibling in the same binary called it expecting to be somewhere else, and
/// `cargo test` runs those on threads of one process. An unreproducible red
/// build, and worse if a panic left the whole binary rooted in a tempdir that
/// `TempDir::drop` then could not remove on Windows.
///
/// What comes back is still the bare name, because that is what gets printed
/// and what the caller writes to.
pub fn default_path(in_dir: &Path, stem: &str, extension: &str) -> Result<PathBuf> {
    #[rustfmt::skip]
    const RESERVED: [&str; 24] = [
        "con", "prn", "aux", "nul", "conin$", "conout$",
        "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
        "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];

    // Windows decides a device name from what comes before the first dot, so
    // `con.x.jpg` opens the console just as `con` does. Comparing the whole
    // stem would let any account with a dot in its name walk past this.
    let device = stem.split('.').next().unwrap_or(stem).to_ascii_lowercase();

    let usable = !stem.is_empty()
        && stem.len() <= 64
        && !stem.starts_with('.')
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
        && !RESERVED.contains(&device.as_str());

    if !usable {
        // Filtered where it is quoted, not where it is checked: the test above
        // has to see the name Instagram sent, and this sentence is printed to a
        // terminal by `report::print_error`. Every name that fails the test for
        // carrying a control character reaches exactly this line, so it was the
        // one refusal guaranteed to hand one straight through.
        return Err(anyhow!(
            "\"{}\" cannot be used as a file name here. \
             Use -o to say where the result should go",
            printable(stem)
        ));
    }

    let name = format!("{stem}.{extension}");
    if in_dir.join(&name).exists() {
        return Err(anyhow!(
            "{name} already exists here. Use -o to say where the result should go"
        ));
    }
    Ok(name.into())
}

pub fn write(
    users: &[User],
    format: Format,
    presentation: Presentation,
    destination: Option<&Path>,
) -> Result<()> {
    write_rendered(&render(users, format, presentation)?, destination)
}

/// Writes to a name **this program chose**, refusing to touch anything that is
/// already there.
///
/// `write_rendered` is for a path the user named, where replacing what is there
/// is their call. This one is for a name derived from a username that came off
/// a server, and the check has to be the creation itself: looking first and
/// writing afterwards leaves a gap — a whole network download wide, in `pfp` —
/// in which the name can become a symlink to somewhere else.
pub fn write_new(rendered: &Rendered, path: &Path) -> Result<()> {
    create_new(path)?
        .write_all(rendered.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;

    ui::info(&format!("Written to {}", path.display()));
    Ok(())
}

/// Opens a file that must not already exist, readable by this account only.
///
/// The creation is the check: looking first and writing afterwards leaves a
/// gap in which the name can become a link to somewhere else. Shared by
/// [`write_new`] and by the story browser, which writes into its scratch
/// directory and says nothing about it.
pub fn create_new(path: &Path) -> Result<std::fs::File> {
    use std::fs::OpenOptions;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    // Everything else this tool writes is 0600 or 0700 -- the session, the
    // database, the directories -- and an export is a list of real people's
    // names. It was the one file left at whatever the umask allowed, which on
    // a shared machine is usually world-readable. Only for the name snob
    // chooses; an explicit `-o` is the user's own decision about where their
    // data goes, and `write_rendered` leaves that alone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))
}

/// Writes an already-rendered result to the destination, with the same
/// file-vs-stdout behavior every command shares.
pub fn write_rendered(rendered: &Rendered, destination: Option<&Path>) -> Result<()> {
    match destination {
        Some(path) => {
            std::fs::write(path, rendered.as_bytes())
                .with_context(|| format!("could not write {}", path.display()))?;
            // The path goes as plain text, not as a file:// link, which some
            // terminals highlight but cannot open.
            ui::info(&format!("Written to {}", path.display()));
        }
        None => {
            let stdout = std::io::stdout();
            let mut locked = stdout.lock();
            // Not `.ok()` on either call. Standard output is line-buffered, so
            // at this point the tail of the result is still in the buffer and
            // the flush is where a full disk reports itself. Discarded, `snob
            // pfp someone > face.jpg` on a full filesystem printed nothing,
            // exited 0, and left a truncated JPEG behind.
            //
            // A closed reader is the one exception, and it is not a failure:
            // `snob followers | head -20` is a reader that has finished, and on
            // Windows there is no SIGPIPE to end the process the way it does on
            // Unix. Reporting "could not write the result" and exiting non-zero
            // there would make a normal shell idiom look like an error.
            for step in [locked.write_all(rendered.as_bytes()), locked.flush()] {
                match step {
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                    other => other.context("could not write the result")?,
                }
            }
        }
    }
    Ok(())
}

fn render(users: &[User], format: Format, presentation: Presentation) -> Result<Rendered> {
    if format == Format::Xlsx {
        return Ok(Rendered::Bytes(xlsx::workbook(users)?));
    }
    Ok(Rendered::Text(match format {
        // The drawn table is for someone watching it appear. Down a pipe or
        // into a file it stays one name per line, which is what scripts have
        // always got.
        Format::Table if presentation.interactive => table::table(users, presentation),
        Format::Table => table::plain(users),
        Format::Json => {
            let mut s = serde_json::to_string_pretty(users)?;
            s.push('\n');
            s
        }
        Format::Ndjson => {
            let mut s = String::new();
            for u in users {
                s.push_str(&serde_json::to_string(u)?);
                s.push('\n');
            }
            s
        }
        Format::Csv => csv::rows(users)?,
        Format::Md => md::table(users),
        // Handled above: it is the one format that is not text.
        Format::Xlsx => unreachable!(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Vec<User> {
        vec![
            User {
                pk: 1,
                username: "one".into(),
                full_name: Some("One".into()),
                is_private: None,
                is_verified: Some(true),
                pfp_url: None,
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

    fn text(users: &[User], format: Format) -> String {
        match render(users, format, Presentation::plain()).unwrap() {
            Rendered::Text(text) => text,
            Rendered::Bytes(_) => panic!("expected text, got bytes"),
        }
    }

    #[test]
    fn the_table_is_one_name_per_line() {
        assert_eq!(text(&users(), Format::Table), "one\ntwo\n");
    }

    #[test]
    fn ndjson_is_one_object_per_line() {
        let out = text(&users(), Format::Ndjson);
        assert_eq!(out.lines().count(), 2);
        assert!(out.lines().all(|l| l.starts_with('{')));
    }

    #[test]
    fn json_omits_the_empty_fields() {
        let out = text(&users(), Format::Json);
        assert!(out.contains("\"is_verified\": true"));
        assert!(!out.contains("null"));
    }

    #[test]
    fn an_empty_list_writes_nothing() {
        assert_eq!(text(&[], Format::Table), "");
        assert_eq!(text(&[], Format::Ndjson), "");
    }

    /// Every format a list can be asked for now produces something.
    #[test]
    fn a_list_renders_in_every_format() {
        for f in [
            Format::Table,
            Format::Json,
            Format::Ndjson,
            Format::Csv,
            Format::Xlsx,
            Format::Md,
        ] {
            assert!(
                render(&users(), f, Presentation::plain()).is_ok(),
                "{f:?} should render"
            );
        }
    }

    #[test]
    fn a_spreadsheet_reaches_the_file_as_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.xlsx");
        write(&users(), Format::Xlsx, Presentation::plain(), Some(&path)).unwrap();

        let written = std::fs::read(&path).unwrap();
        assert_eq!(&written[..4], b"PK\x03\x04", "a workbook is a zip archive");
    }

    #[test]
    fn csv_reaches_the_file_it_was_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.csv");
        write(&users(), Format::Csv, Presentation::plain(), Some(&path)).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, text(&users(), Format::Csv));
        assert!(written.starts_with("pk,username"), "{written}");
    }

    #[test]
    fn an_explicit_format_beats_the_extension() {
        let path = Path::new("list.csv");
        assert_eq!(
            effective_format(Some(Format::Json), Some(path)),
            Format::Json
        );
    }

    #[test]
    fn the_extension_picks_the_format_when_nothing_was_asked_for() {
        for (name, expected) in [
            ("list.csv", Format::Csv),
            ("LIST.CSV", Format::Csv),
            ("list.xlsx", Format::Xlsx),
            ("list.md", Format::Md),
            ("list.markdown", Format::Md),
            ("list.json", Format::Json),
            ("list.ndjson", Format::Ndjson),
            ("list.jsonl", Format::Ndjson),
        ] {
            let got = effective_format(None, Some(Path::new(name)));
            assert_eq!(got, expected, "{name}");
        }
    }

    /// An extension nobody claimed keeps the old behavior rather than
    /// guessing: `-o notes.txt` is still a plain list.
    #[test]
    fn an_unknown_extension_does_not_guess() {
        for name in ["notes.txt", "notes"] {
            let got = effective_format(None, Some(Path::new(name)));
            assert!(
                matches!(got, Format::Table | Format::Json),
                "{name} gave {got:?}"
            );
        }
    }

    #[test]
    fn a_spreadsheet_needs_a_file_to_go_to() {
        let error = check_destination(Format::Xlsx, None).unwrap_err();
        assert!(error.to_string().contains("-o"), "{error}");
        assert!(check_destination(Format::Xlsx, Some(Path::new("x.xlsx"))).is_ok());
    }

    #[test]
    fn the_text_formats_are_happy_on_standard_output() {
        for f in [Format::Table, Format::Json, Format::Ndjson, Format::Csv] {
            assert!(check_destination(f, None).is_ok(), "{f:?}");
        }
    }

    #[test]
    fn a_file_is_never_interactive() {
        let presentation = Presentation::detect(Some(Path::new("out.csv")));
        assert!(!presentation.interactive);
        assert!(!presentation.hyperlinks);
        assert!(!presentation.color);
    }

    #[test]
    fn a_destination_that_cannot_be_written_says_which_one() {
        let dir = tempfile::tempdir().unwrap();
        // A directory is the everyday version of this: `-o` pointed at a
        // folder rather than a file inside it.
        let error = write_rendered(&Rendered::Text("x".into()), Some(dir.path())).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("could not write"), "{message}");
    }

    /// `snob pfp nul` used to report success on Windows while the picture went
    /// to the null device, and the name comes from a server rather than from
    /// anything the user typed.
    #[test]
    fn a_name_that_cannot_be_a_file_is_refused_rather_than_written() {
        // An empty directory of its own, so nothing here depends on what
        // happens to be next to the test binary or on where the suite was
        // started from.
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path();

        for bad in [
            "nul",
            "CON",
            "aux",
            "com1",
            "../escape",
            "a/b",
            "",
            ".hidden",
            // Windows reads the device name up to the first dot, so these are
            // the console too, extension or no extension.
            "con.x",
            "NUL.a",
            "com3",
            "lpt9",
            "conout$",
            &"x".repeat(200),
        ] {
            assert!(
                default_path(here, bad, "jpg").is_err(),
                "{bad:?} should not become a file name"
            );
        }
        for good in ["someone", "some.one", "some_one", "user123"] {
            assert!(
                default_path(here, good, "jpg").is_ok(),
                "{good:?} should be fine"
            );
        }
    }

    /// The refusal every unusable name arrives at must not carry the name
    /// through unfiltered.
    ///
    /// A control character is one of the things that makes a name unusable
    /// here, so this refusal is where such a name always ends up — and it is
    /// printed to a terminal. `snob pfp` takes the stem from what Instagram
    /// sent, which is the side of the boundary nothing on this machine chose.
    #[test]
    fn the_refusal_does_not_print_the_name_it_is_refusing() {
        let error = default_path(Path::new("."), "gh\u{1b}[2K\u{1b}[A", "jpg")
            .unwrap_err()
            .to_string();
        assert!(!error.contains('\u{1b}'), "{error:?}");
        assert!(error.contains("gh[2K[A"), "{error:?}");
    }

    /// Nothing here moves the process's working directory. It used to: this
    /// walked into a tempdir with `set_current_dir` while a sibling in the same
    /// binary resolved relative paths expecting to be elsewhere, and `cargo
    /// test` runs those on threads of one process.
    #[test]
    fn an_existing_file_is_not_overwritten_behind_the_users_back() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            default_path(dir.path(), "someone", "jpg").unwrap(),
            Path::new("someone.jpg")
        );

        std::fs::write(dir.path().join("someone.jpg"), b"something already here").unwrap();
        let error = default_path(dir.path(), "someone", "jpg")
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn bytes_reach_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let payload = vec![0x50, 0x4b, 0x03, 0x04, 0x00, 0xff];
        write_rendered(&Rendered::Bytes(payload.clone()), Some(&path)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }
}
