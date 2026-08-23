//! `snob highlights`: the reels an account chose to keep on its profile, and
//! how to keep a copy.
//!
//! Two lists, one inside the other, which is the one way this differs from
//! `stories`. The tray is the row of covers under the bio — a title, a count
//! and some dates each — and every entry holds items that are stories in all
//! but expiry. So the command is `stories` twice over: `snob highlights
//! someone` numbers the tray the way `stories` numbers a reel, and `snob
//! highlights someone 2` opens the second entry and numbers what it holds,
//! with `-d`, `-o`, `--format` and `-i` meaning on that listing exactly what
//! they mean on `stories`. AGENTS.md settled the shape before this file
//! existed, including why the two are not one command with a flag.
//!
//! Without an entry's number, `-d` takes whole entries — `-d 2` is every item
//! of the second, `-d all` the whole profile — because that is what a number
//! means in the listing that is actually on screen. The two readings cannot
//! collide: one is at the tray, the other inside an entry, and each refuses
//! numbers its own listing does not have.
//!
//! The requests: one to turn the name into an id, one for the tray, and one
//! more per highlight *opened* — the tray itself does not carry the items.
//! Each is paid for inside the client like every other read. Downloads come
//! from the CDN, a different host that is deliberately not paced; the
//! reasoning is on `IgClient::download_capped`.
//!
//! **Reading a highlight does not mark it as seen.** The browser registers a
//! view of a highlight item through the very mutation it uses for a story,
//! with the highlight as the reel; this project has no code that could send
//! it, and `crates/snob-core/tests/no_seen.rs` reads the source of all four
//! crates — the prefixed reel id included — to keep it that way.
//!
//! **A private account the viewer does not follow is told apart from an
//! account with nothing kept.** The tray endpoint answers both with an empty
//! list; `web_profile_info`, which the name is resolved through anyway, says
//! which is which — the same two sentences `profile` keeps apart, because
//! "none" and "not shown to you" are different answers and printing the first
//! for the second would be wrong.

use std::path::Path;

use anyhow::{Result, anyhow};
use comfy_table::{Attribute as Style, Cell, ContentArrangement, Table, presets};
use snob_core::Epoch;
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{DownloadSelection, Format, HighlightsArgs, StoryFormat};
use crate::commands::common;
use crate::commands::stories::{Saved, Story, bytes_of, download_many, save_story, story_from};
use crate::exit::{ExitCode, ExitError};
use crate::output::{self, Presentation, Rendered};
use crate::report;
use crate::ui;

/// One tray entry: the cover the profile shows, not yet its items.
#[derive(Debug, Clone)]
pub struct Entry {
    /// `highlight:<id>`, the spelling the items are fetched with.
    pub id: String,
    /// Filtered: it came off somebody else's profile and it is going to a
    /// terminal. Empty when the highlight has none, which happens.
    pub title: String,
    /// The count the tray declares. Not a promise: what the reel answers is
    /// what is downloadable, and the two have been seen to disagree.
    pub declared_items: Option<u64>,
    pub created_at: Option<Epoch>,
    /// When something was last added to it.
    pub updated_at: Option<Epoch>,
}

/// The tray, gathered before anything is printed.
#[derive(Debug, Clone)]
pub struct Tray {
    /// As Instagram spells it, not as it was typed.
    pub username: String,
    pub entries: Vec<Entry>,
}

/// What the tray request could see.
pub enum Fetched {
    Tray(Tray),
    /// Private, and the viewer does not follow it. Carried as its own case
    /// for the same reason `profile::Visibility` exists: "no highlights" and
    /// "highlights you may not see" are different sentences.
    Hidden {
        username: String,
    },
}

pub async fn run(args: HighlightsArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let app = common::app(&secrets, paths, false)?;
    common::refuse_during_cooldown(&app, "no request can be made")?;

    // No target is the viewer's own account, like `stories` and the lists.
    let typed = match args.target.as_deref() {
        Some(t) => t.to_string(),
        None => app.viewer().username.clone().ok_or_else(|| {
            anyhow!("this session does not know its own username; name an account")
        })?,
    };

    let tray = match fetch_tray(app.client(), &typed, app.viewer().pk).await? {
        Fetched::Tray(tray) => tray,
        Fetched::Hidden { username } => {
            // An answer, not a failure: the account was found and this is
            // what it shows the viewer. The wording is `profile`'s.
            ui::info(&format!(
                "the highlights of @{} are not visible: the account is private and you do not \
                 follow it",
                printable(&username)
            ));
            return Ok(ExitCode::Ok);
        }
    };

    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }

    if tray.entries.is_empty() {
        ui::info(&format!(
            "@{} has no highlights.",
            printable(&tray.username)
        ));
        return Ok(ExitCode::Ok);
    }

    match args.highlight {
        None => at_the_tray(&app, &tray, &args, paths).await,
        Some(number) => inside_one(&app, &tray, number as usize, &args, paths).await,
    }
}

/// The commands that act on the tray listing: browse it, download whole
/// entries by their numbers, or print it.
async fn at_the_tray(
    app: &crate::app::App,
    tray: &Tray,
    args: &HighlightsArgs,
    paths: &AppPaths,
) -> Result<ExitCode> {
    if args.action.interactive {
        return crate::ui::highlights::browse(app.client(), tray, None, paths).await;
    }

    if let Some(selection) = args.action.selection() {
        return download_entries(app, tray, selection, args.action.output.as_deref()).await;
    }

    list_tray(tray, args.list.format, args.action.output.as_deref())
}

/// The commands that act inside one entry, named by its tray number.
async fn inside_one(
    app: &crate::app::App,
    tray: &Tray,
    number: usize,
    args: &HighlightsArgs,
    paths: &AppPaths,
) -> Result<ExitCode> {
    // Checked against the tray before any further request, so `snob
    // highlights someone 9` against a tray of five costs the two requests
    // already spent and no more.
    if number > tray.entries.len() {
        return Err(no_such_highlight(tray, number));
    }

    if args.action.interactive {
        return crate::ui::highlights::browse(app.client(), tray, Some(number - 1), paths).await;
    }

    let items = items_of_entry(app.client(), &tray.entries[number - 1]).await?;
    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }
    if items.is_empty() {
        ui::info(&empty_highlight(tray, number));
        return Ok(ExitCode::Ok);
    }

    if let Some(selection) = args.action.selection() {
        return download_items(
            app,
            tray,
            number,
            &items,
            selection,
            args.action.output.as_deref(),
        )
        .await;
    }

    list_items(
        tray,
        number,
        &items,
        args.list.format,
        args.action.output.as_deref(),
    )
}

/// The network half of the tray, kept apart from the session and the
/// filesystem so a test can drive it against a mock server.
pub async fn fetch_tray(client: &IgClient, typed: &str, viewer: snob_core::Pk) -> Result<Fetched> {
    let info = client
        .web_profile_info(crate::engine::target::clean(typed))
        .await?;

    // The same three-way rule as `profile`: a private account serves its
    // reels only to its followers, and asking would spend a request on an
    // empty answer and then say "none" for an account that has some.
    let own = info.id == viewer;
    let is_private = info.is_private.unwrap_or(false);
    let you_follow = info.followed_by_viewer.unwrap_or(false);
    if !(own || !is_private || you_follow) {
        return Ok(Fetched::Hidden {
            username: info.username,
        });
    }

    let tray = client.highlights_tray(info.id, &info.username).await?;
    Ok(Fetched::Tray(Tray {
        username: info.username,
        entries: tray
            .into_iter()
            .map(|h| Entry {
                id: h.id,
                title: printable(h.title.as_deref().unwrap_or("")),
                declared_items: h.media_count,
                created_at: h.created_at,
                updated_at: h.updated_timestamp,
            })
            .collect(),
    }))
}

/// The items of one entry. One request.
///
/// An id the reel no longer answers for — deleted since the tray was fetched —
/// comes back as no items, and the caller says "empty" for it; there is
/// nothing to be downloaded either way, and the tray is seconds old.
pub async fn items_of_entry(client: &IgClient, entry: &Entry) -> Result<Vec<Story>> {
    let Some(reel) = client.highlight(&entry.id).await? else {
        return Ok(Vec::new());
    };
    Ok(reel.items.iter().map(story_from).collect())
}

/// The sentence for a tray number nobody has, shared by every path that
/// takes one so they cannot drift.
fn no_such_highlight(tray: &Tray, number: usize) -> anyhow::Error {
    anyhow!(
        "there is no highlight {number}: @{} has {}",
        printable(&tray.username),
        match tray.entries.len() {
            1 => "one".to_string(),
            n => format!("{n} highlights"),
        }
    )
}

/// And the one for an entry that answered with nothing.
fn empty_highlight(tray: &Tray, number: usize) -> String {
    format!(
        "highlight {number} of @{} {}is empty.",
        printable(&tray.username),
        titled(&tray.entries[number - 1])
    )
}

/// `("Trip") `, or nothing for an untitled entry — a parenthetical that can
/// sit in the middle of a sentence either way.
fn titled(entry: &Entry) -> String {
    if entry.title.is_empty() {
        String::new()
    } else {
        format!("(\"{}\") ", entry.title)
    }
}

/// `someone-2`, the stem every file of entry 2 is named under, so that
/// `-d 3` inside it and D in the browser write the very same name.
fn stem_of(tray: &Tray, number: usize) -> String {
    format!("{}-{number}", printable(&tray.username))
}

/// Downloads whole entries: each one fetched and then saved the way
/// `stories -d all` saves a reel, into one directory.
///
/// Keeps going past an entry that fails, like the story loop keeps going past
/// a story, and for the same reason: stopping at the first would leave a
/// partial set with no say about which ones are missing. The items requests
/// stay sequential — they are Instagram, paced inside the client — while each
/// entry's CDN downloads overlap the way `stories` overlaps them.
async fn download_entries(
    app: &crate::app::App,
    tray: &Tray,
    selection: DownloadSelection,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let numbers: Vec<usize> = match selection {
        DownloadSelection::All => (1..=tray.entries.len()).collect(),
        DownloadSelection::These(numbers) => numbers,
    };
    for &number in &numbers {
        if number > tray.entries.len() {
            return Err(no_such_highlight(tray, number));
        }
    }

    let mut failed: Vec<String> = Vec::new();
    for &number in &numbers {
        if app.cancel().is_canceled() {
            return Err(ExitError::new(ExitCode::Interrupted, "stopped").into());
        }
        let entry = &tray.entries[number - 1];
        let items = match items_of_entry(app.client(), entry).await {
            Ok(items) => items,
            Err(e) => {
                failed.push(format!("highlight {number}: {e}"));
                continue;
            }
        };
        if items.is_empty() {
            ui::info(&empty_highlight(tray, number));
            continue;
        }
        ui::info(&format!(
            "Highlight {number} {}- {}",
            titled(entry),
            match items.len() {
                1 => "1 item".to_string(),
                n => format!("{n} items"),
            }
        ));
        let all: Vec<usize> = (1..=items.len()).collect();
        if let Err(e) = download_many(
            app.client_shared(),
            stem_of(tray, number),
            items,
            all,
            destination,
        )
        .await
        {
            if crate::commands::stories::was_canceled(&e) {
                return Err(e);
            }
            failed.push(format!("highlight {number}: {e}"));
        }
    }

    if failed.is_empty() {
        return Ok(ExitCode::Ok);
    }
    Err(ExitError::new(
        ExitCode::Error,
        format!(
            "{} of {} highlights could not be fully saved:\n{}",
            failed.len(),
            numbers.len(),
            failed.join("\n")
        ),
    )
    .into())
}

/// Downloads items of one entry, by the numbers its listing printed. The
/// mirror of `stories::download_selected`, with the entry's number in the
/// sentences and in the file names.
async fn download_items(
    app: &crate::app::App,
    tray: &Tray,
    number: usize,
    items: &[Story],
    selection: DownloadSelection,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let numbers: Vec<usize> = match selection {
        DownloadSelection::All => (1..=items.len()).collect(),
        DownloadSelection::These(numbers) => numbers,
    };
    for &n in &numbers {
        if n > items.len() {
            return Err(no_such_item(tray, number, items, n));
        }
    }
    if let [n] = numbers[..] {
        return download_one_item(app, tray, number, items, n, destination).await;
    }
    download_many(
        app.client_shared(),
        stem_of(tray, number),
        items.to_vec(),
        numbers,
        destination,
    )
    .await
}

/// The sentence for an item number the entry does not hold.
fn no_such_item(tray: &Tray, number: usize, items: &[Story], asked: usize) -> anyhow::Error {
    anyhow!(
        "there is no item {asked}: highlight {number} of @{} holds {}",
        printable(&tray.username),
        match items.len() {
            1 => "one".to_string(),
            n => format!("{n} items"),
        }
    )
}

/// One item, kept to the single-download contract exactly as `stories -d N`
/// keeps it: `-o` names a file and a failure is the run's failure; without
/// `-o` the file lands in the working directory under the listing's name,
/// and one already there is an answer rather than a second copy.
async fn download_one_item(
    app: &crate::app::App,
    tray: &Tray,
    number: usize,
    items: &[Story],
    n: usize,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let story = &items[n - 1];
    match destination {
        Some(path) => {
            let bytes = bytes_of(app.client(), story).await?;
            output::write_bytes(&bytes, Some(path))?;
            ui::info(&format!("Saved {}", path.display()));
        }
        None => match save_story(
            app.client(),
            &stem_of(tray, number),
            items,
            n,
            Path::new("."),
        )
        .await?
        {
            Saved::Now(path) => ui::info(&format!("Saved {}", path.display())),
            Saved::Already(path) => ui::info(&format!("Already saved {}", path.display())),
        },
    }
    Ok(ExitCode::Ok)
}

/// Prints the tray. The numbers here are what the second positional and a
/// tray-level `-d` take.
fn list_tray(
    tray: &Tray,
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = checked_format(format, destination, "a highlight listing")?;

    let mut text = match format {
        Format::Json | Format::Ndjson => tray_json(tray, format)?,
        _ => tray_table(tray, Presentation::detect(destination)),
    };
    text.push('\n');
    output::write_rendered(&Rendered::Text(text), destination)?;

    if destination.is_none() && Presentation::detect(None).interactive {
        ui::info(&format!(
            "snob highlights {} <number> to look inside one, -d <number> to save one whole, or \
             -i to browse",
            printable(&tray.username)
        ));
    }
    Ok(ExitCode::Ok)
}

/// Prints what one entry holds. The numbers here are what `-d` takes.
fn list_items(
    tray: &Tray,
    number: usize,
    items: &[Story],
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = checked_format(format, destination, "a highlight's listing")?;

    let mut text = match format {
        Format::Json | Format::Ndjson => items_json(tray, number, items, format)?,
        _ => items_table(tray, number, items, Presentation::detect(destination)),
    };
    text.push('\n');
    output::write_rendered(&Rendered::Text(text), destination)?;

    if destination.is_none() && Presentation::detect(None).interactive {
        ui::info("--download <number> to save one, or --interactive to move through them");
    }
    Ok(ExitCode::Ok)
}

/// The three forms either listing has, and the refusal for the others —
/// `stories`' rule, applied to both levels here so `-o out.xlsx` cannot fall
/// through to a table with a spreadsheet's name on it.
fn checked_format(
    format: Option<StoryFormat>,
    destination: Option<&Path>,
    what: &str,
) -> Result<Format> {
    let format = output::effective_format(format.map(Into::into), destination);
    if matches!(format, Format::Csv | Format::Xlsx | Format::Md) {
        anyhow::bail!(
            "{what} has no {} form; it can be a table, json or ndjson",
            format!("{format:?}").to_ascii_lowercase()
        );
    }
    output::check_destination(format, destination)?;
    Ok(format)
}

fn header_cells(names: &[&str], presentation: Presentation) -> Vec<Cell> {
    names
        .iter()
        .map(|name| {
            let cell = Cell::new(name);
            if presentation.color {
                cell.add_attribute(Style::Bold)
            } else {
                cell
            }
        })
        .collect()
}

fn tray_table(tray: &Tray, presentation: Presentation) -> String {
    let mut table = Table::new();
    table.load_preset(presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    if let Some(width) = presentation.width {
        table.set_width(width);
    }
    table.set_header(header_cells(
        &["#", "Title", "Items", "Updated"],
        presentation,
    ));
    if presentation.color {
        table.enforce_styling();
    }

    for (index, entry) in tray.entries.iter().enumerate() {
        table.add_row([
            Cell::new(index + 1),
            // Already filtered in `fetch_tray`; the dash keeps an untitled
            // entry's row from reading as a rendering defect.
            Cell::new(if entry.title.is_empty() {
                "-"
            } else {
                &entry.title
            }),
            Cell::new(
                entry
                    .declared_items
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "-".into()),
            ),
            Cell::new(
                entry
                    .updated_at
                    .map(report::dated)
                    .unwrap_or_else(|| "-".into()),
            ),
        ]);
    }
    table.to_string()
}

fn items_table(tray: &Tray, number: usize, items: &[Story], presentation: Presentation) -> String {
    let mut table = Table::new();
    table.load_preset(presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    if let Some(width) = presentation.width {
        table.set_width(width);
    }
    // No "Gone in": nothing in a highlight is going anywhere, which is what a
    // highlight is. The date carries the year instead of the hour for the
    // same reason -- see `report::dated`.
    table.set_header(header_cells(
        &["#", "Kind", "Posted", "Mentions"],
        presentation,
    ));
    if presentation.color {
        table.enforce_styling();
    }

    let _ = (tray, number);
    for (index, story) in items.iter().enumerate() {
        table.add_row([
            Cell::new(index + 1),
            Cell::new(crate::commands::stories::kind_label(story)),
            Cell::new(report::dated(story.taken_at)),
            Cell::new(
                story
                    .mentions
                    .iter()
                    .map(|m| format!("@{m}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        ]);
    }
    table.to_string()
}

fn tray_json(tray: &Tray, format: Format) -> Result<String> {
    let rows: Vec<serde_json::Value> = tray
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            serde_json::json!({
                "number": index + 1,
                // The tray's own spelling, prefix included, the way `profile`
                // already publishes it -- one spelling of the one id.
                "id": entry.id,
                "title": entry.title,
                "items": entry.declared_items,
                "created_at": entry.created_at,
                "updated_at": entry.updated_at,
            })
        })
        .collect();

    Ok(match format {
        Format::Ndjson => rows
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n"),
        _ => serde_json::to_string_pretty(&serde_json::json!({
            "username": tray.username,
            "highlights": rows,
        }))?,
    })
}

fn items_json(tray: &Tray, number: usize, items: &[Story], format: Format) -> Result<String> {
    let entry = &tray.entries[number - 1];
    let rows: Vec<serde_json::Value> = items
        .iter()
        .enumerate()
        .map(|(index, story)| {
            serde_json::json!({
                "number": index + 1,
                "kind": crate::commands::stories::kind_label(story),
                "taken_at": story.taken_at,
                "mentions": story.mentions,
                // Deliberately included, the way `stories` includes it: a
                // signed CDN address is what makes the JSON usable by
                // anything else, it is already in the reply Instagram gave
                // this session, and it expires on its own.
                "url": story.url,
            })
        })
        .collect();

    Ok(match format {
        Format::Ndjson => rows
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n"),
        _ => serde_json::to_string_pretty(&serde_json::json!({
            "username": tray.username,
            "highlight": {
                "number": number,
                "id": entry.id,
                "title": entry.title,
            },
            "items": rows,
        }))?,
    })
}
