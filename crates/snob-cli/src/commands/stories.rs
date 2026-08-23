//! `snob stories`: what an account has up right now, and how to keep a copy.
//!
//! Two requests for the listing — the profile, to turn a name into an id, and
//! the reel itself — and one more per story downloaded, from the CDN, which is
//! a different host with its own limits and is not charged against Instagram's
//! budget. The same shape as `pfp`, for the same reasons, and like `pfp` it
//! walks no list and stores no snapshot.
//!
//! **Downloading a story does not mark it as seen.** Registering a view is a
//! separate call, and a write — so it falls under the two-write rule and this
//! project has no code that could send it; `crates/snob-core/tests/no_seen.rs`
//! reads the source of all three crates to keep it that way. The consequence
//! belongs to the person whose story it is, whose viewer list stays a record of
//! people who opened it in the app, and it is the one property here that nobody
//! running the tool would ever notice being broken.
//!
//! **It does not ask for consent, and that is deliberate.** The consent rule
//! covers enumerating somebody — walking their followers, which is thousands of
//! requests about thousands of people who did not ask to be in a database.
//! Reading one reel is one request about one account, and `pfp` does not ask
//! either. Asking here would make the question routine, and a question that is
//! always asked stops being read.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use comfy_table::{Attribute as Style, Cell, ContentArrangement, Table, presets};
use snob_core::Epoch;
use snob_core::model::printable;
use snob_ig::client::IgClient;
use snob_ig::error::IgError;
use snob_ig::model::{ReelItem, largest};
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::cli::{Format, StoriesArgs, StoryFormat};
use crate::commands::common;
use crate::exit::{ExitCode, ExitError};
use crate::output::{self, Presentation, Rendered};
use crate::report;
use crate::ui;

/// Ceiling on one downloaded story.
///
/// Higher than the profile-picture ceiling, and it has to be: a fifteen-second
/// story video at Instagram's own bitrate lands in the single-digit megabytes
/// and the 8 MB cap on `IgClient::download` refuses some of them. Sixty-four is
/// far above anything Instagram serves for a format capped at fifteen seconds,
/// so it is still a ceiling rather than a formality — its job is that a
/// redirect to something else cannot make this read until memory runs out.
pub const MAX_STORY_BYTES: usize = 64 * 1024 * 1024;

/// A photo or a video, as Instagram's `media_type` integer means it.
///
/// The integer stops here. Nothing downstream compares a number to 1 or 2, and
/// an unknown value is its own case rather than being folded into either — a
/// third kind arriving should read as "unknown" and be downloadable, not be
/// mislabeled as a photo and given a `.jpg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Photo,
    Video,
    Unknown,
}

impl Kind {
    fn of(media_type: u8) -> Self {
        match media_type {
            1 => Self::Photo,
            2 => Self::Video,
            _ => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Unknown => "unknown",
        }
    }
}

/// One story, reduced to what this command needs.
#[derive(Debug, Clone)]
pub struct Story {
    pub kind: Kind,
    pub taken_at: Epoch,
    pub expiring_at: Option<Epoch>,
    /// Where the best copy is. Absent when Instagram described a story and
    /// offered no version of it, which happens and must not be a crash.
    pub url: Option<String>,
    /// Accounts tagged in it, already filtered for a terminal.
    pub mentions: Vec<String>,
}

/// Everything the listing needs, gathered before anything is printed.
#[derive(Debug, Clone)]
pub struct Stories {
    /// As Instagram spells it, not as it was typed.
    pub username: String,
    pub items: Vec<Story>,
}

pub async fn run(args: StoriesArgs, secrets: SecretStore, paths: &AppPaths) -> Result<ExitCode> {
    let app = common::app(&secrets, paths, false)?;
    common::refuse_during_cooldown(&app, "no request can be made")?;

    // No target is the viewer's own account, like the list commands. The
    // viewer's username is already known from the session, so this costs
    // nothing extra.
    let typed = match args.target.as_deref() {
        Some(t) => t.to_string(),
        None => app.viewer().username.clone().ok_or_else(|| {
            anyhow!("this session does not know its own username; name an account")
        })?,
    };

    let stories = fetch(app.client(), &typed).await?;

    if app.cancel().is_canceled() {
        return Ok(ExitCode::Interrupted);
    }

    if stories.items.is_empty() {
        ui::info(&format!(
            "@{} has no stories up right now.",
            printable(&stories.username)
        ));
        return Ok(ExitCode::Ok);
    }

    if args.action.interactive {
        return crate::ui::stories::browse(app.client(), &stories, paths).await;
    }

    if let Some(selection) = args.action.selection() {
        return download_selected(
            app.client_shared(),
            &stories,
            selection,
            args.action.output.as_deref(),
        )
        .await;
    }

    list(&stories, args.list.format, args.action.output.as_deref())
}

/// Turns the parsed selection into story numbers and refuses any the tray
/// does not have -- before the first request, so `-d 3,9` against a tray of
/// five downloads nothing rather than three stories and an error.
async fn download_selected(
    client: std::sync::Arc<IgClient>,
    stories: &Stories,
    selection: crate::cli::DownloadSelection,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let numbers: Vec<usize> = match selection {
        crate::cli::DownloadSelection::All => (1..=stories.items.len()).collect(),
        crate::cli::DownloadSelection::These(numbers) => numbers,
    };
    for &number in &numbers {
        if number > stories.items.len() {
            return Err(no_such_story(stories, number));
        }
    }
    // One story keeps the single-download contract: `-o` names a file, and a
    // failure is the run's failure. Several go the way `--all` always went:
    // `-o` names a directory and the loop keeps going past one that fails.
    if let [number] = numbers[..] {
        return download_one(&client, stories, number, destination).await;
    }
    download_many(client, stories, numbers, destination).await
}

/// The sentence for a number the tray does not have, shared by the single
/// and the many paths so they cannot drift.
fn no_such_story(stories: &Stories, number: usize) -> anyhow::Error {
    anyhow!(
        "there is no story {number}: @{} has {}",
        printable(&stories.username),
        match stories.items.len() {
            1 => "one".to_string(),
            n => format!("{n} stories"),
        }
    )
}

/// The network half, kept apart from the session and the filesystem so a test
/// can drive it against a mock server.
///
/// Two requests, each paid for inside the client. Spending both against one
/// reservation is how a burst allowance quietly stops meaning what it says.
async fn fetch(client: &IgClient, typed: &str) -> Result<Stories> {
    let profile = client
        .web_profile_info(crate::engine::target::clean(typed))
        .await?;

    let Some(reel) = client.stories(profile.id, &profile.username).await? else {
        return Ok(Stories {
            username: profile.username,
            items: Vec::new(),
        });
    };

    Ok(Stories {
        username: profile.username,
        items: reel.items.iter().map(story_from).collect(),
    })
}

/// The wire item, reduced.
///
/// The largest version wins rather than the first. Instagram lists candidates
/// in an order it does not promise, and the web client picks the one that fits
/// its viewport — nothing here draws in a terminal, so what is wanted is simply
/// the biggest. `image_versions2` is read even for a video, because a video
/// item carries its poster frame there and an item with no `video_versions` is
/// then still downloadable as the picture Instagram has of it.
fn story_from(item: &ReelItem) -> Story {
    let kind = Kind::of(item.media_type);
    let video = largest(&item.video_versions).map(|v| v.url.clone());
    let image = item
        .image_versions2
        .as_ref()
        .and_then(|c| largest(&c.candidates))
        .map(|v| v.url.clone());

    Story {
        kind,
        taken_at: item.taken_at,
        expiring_at: item.expiring_at,
        url: match kind {
            Kind::Video => video.or(image),
            _ => image.or(video),
        },
        mentions: item
            .reel_mentions
            .iter()
            .filter_map(|m| m.user.as_ref())
            .map(|u| printable(&u.username))
            .collect(),
    }
}

/// Prints the listing. The numbers here are what `--download` takes.
fn list(
    stories: &Stories,
    format: Option<StoryFormat>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let format = output::effective_format(format.map(Into::into), destination);
    // `StoryFormat` keeps `--format` to the three forms a listing has, and
    // the extension of `-o` went round it: `-o out.xlsx` fell through to the
    // table and reported "Written to out.xlsx" over a box-drawing text file.
    if matches!(format, Format::Csv | Format::Xlsx | Format::Md) {
        anyhow::bail!(
            "a story listing has no {} form; it can be a table, json or ndjson",
            format!("{format:?}").to_ascii_lowercase()
        );
    }
    output::check_destination(format, destination)?;

    // Every rendering ends in a newline, like `output::render`'s do. Neither
    // `serde_json::to_string_pretty` nor comfy-table adds one, and without it
    // the shell prompt comes back glued to the last row.
    let mut text = match format {
        Format::Json | Format::Ndjson => as_json(stories, format)?,
        _ => as_table(stories, Presentation::detect(destination)),
    };
    text.push('\n');
    let rendered = Rendered::Text(text);
    output::write_rendered(&rendered, destination)?;

    // On standard error, so it does not land in a redirect. The listing is the
    // answer; this is the hint about what to do with it.
    if destination.is_none() && Presentation::detect(None).interactive {
        ui::info(&format!(
            "{} to download one, or --interactive to move through them",
            "--download <number>"
        ));
    }
    Ok(ExitCode::Ok)
}

fn as_table(stories: &Stories, presentation: Presentation) -> String {
    let mut table = Table::new();
    table.load_preset(presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    if let Some(width) = presentation.width {
        table.set_width(width);
    }
    table.set_header(
        ["#", "Kind", "Posted", "Gone in", "Mentions"]
            .iter()
            .map(|name| {
                let cell = Cell::new(name);
                if presentation.color {
                    cell.add_attribute(Style::Bold)
                } else {
                    cell
                }
            }),
    );
    if presentation.color {
        table.enforce_styling();
    }

    for (index, story) in stories.items.iter().enumerate() {
        table.add_row([
            Cell::new(index + 1),
            Cell::new(story.kind.label()),
            Cell::new(report::stored_on(story.taken_at)),
            Cell::new(remaining(story.expiring_at)),
            // Already filtered in `story_from`, because they came off somebody
            // else's profile and this is going to a terminal.
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

/// What the interactive list writes on a row.
///
/// Here rather than in `ui::stories` so that the two listings say the same
/// thing about the same story. They are different shapes — a table and a line —
/// and the words in them are one decision.
pub(crate) fn kind_label(story: &Story) -> &'static str {
    story.kind.label()
}

/// "Aug 3 at 14:12, 7h 23m left".
pub(crate) fn posted_and_left(story: &Story) -> String {
    format!(
        "{}, {} left",
        report::stored_on(story.taken_at),
        remaining(story.expiring_at)
    )
}

/// How long a story has left, or nothing when Instagram did not say.
///
/// An absent `expiring_at` prints as a dash rather than as an expired story:
/// zero would be read as "gone", which is a statement, and what is true is that
/// nobody said.
fn remaining(expiring_at: Option<Epoch>) -> String {
    let Some(at) = expiring_at else {
        return "-".into();
    };
    countdown(at - snob_core::clock::now())
}

/// "7h 23m", "12m", "expired".
///
/// Not `snob_core::duration::format`, which exists to write a duration back
/// into the monitor's configuration file the way somebody would have typed it
/// — so it uses the largest unit that divides **exactly** and falls back to
/// seconds. A story with seven hours and twenty-three minutes left divides
/// exactly by nothing, and would have printed as `26580s`.
fn countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return "expired".into();
    }
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    match (hours, minutes) {
        (0, 0) => "under a minute".into(),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

fn as_json(stories: &Stories, format: Format) -> Result<String> {
    let rows: Vec<serde_json::Value> = stories
        .items
        .iter()
        .enumerate()
        .map(|(index, story)| {
            serde_json::json!({
                "number": index + 1,
                "kind": story.kind.label(),
                "taken_at": story.taken_at,
                "expiring_at": story.expiring_at,
                "mentions": story.mentions,
                // Deliberately included: a signed CDN address is what makes the
                // JSON usable by anything else, and it is already in the reply
                // Instagram gave this session. It expires on its own.
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
            "username": stories.username,
            "stories": rows,
        }))?,
    })
}

/// Downloads the story the user named, by the number the listing printed.
async fn download_one(
    client: &IgClient,
    stories: &Stories,
    number: usize,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    // One-based because that is what the listing shows. Zero is its own message
    // rather than an underflow.
    let story = number
        .checked_sub(1)
        .and_then(|i| stories.items.get(i))
        .ok_or_else(|| no_such_story(stories, number))?;

    match destination {
        // The user named it, so replacing what is there is their call -- and
        // so is downloading it again. Held whole: a named destination may be
        // anywhere, and this keeps the one-file contract exactly as it was.
        Some(path) => {
            let bytes = bytes_of(client, story).await?;
            output::write_bytes(&bytes, Some(path))?;
            ui::info(&format!("Saved {}", path.display()));
        }
        None => match save_story(client, stories, number, Path::new(".")).await? {
            Saved::Now(path) => ui::info(&format!("Saved {}", path.display())),
            Saved::Already(path) => ui::info(&format!("Already saved {}", path.display())),
        },
    }
    Ok(ExitCode::Ok)
}

/// What [`save_story`] found: the file it wrote, or the one already there.
enum Saved {
    Now(PathBuf),
    Already(PathBuf),
}

/// Saves one story under the name the listing implies, to disk as it arrives.
///
/// **Nothing is fetched for a story already on disk.** The name depends on
/// the extension and the extension is read from the bytes, so this used to
/// download the whole story and only then discover, inside `default_name`,
/// that the name was taken -- on a second `-d all` that was every story in
/// the tray, fetched and thrown away. The four extensions `extension_of` can
/// answer are tried first; a hit is "already saved" and costs nothing.
///
/// That look is an optimization in front of the atomic check, not a
/// replacement for it: the write still goes through `output::create_new`,
/// which refuses a name that appeared between the look and the open. The
/// doc on `output::write_new` says why the creation has to be the check.
///
/// **The file is written to `<stem>.part` and renamed at the end.** The
/// bytes stream to disk through `IgClient::download_to` -- the download used
/// to sit in memory whole, then get copied once more for the write -- and a
/// stream that stops halfway must not leave a truncated file under the real
/// name, where the look above would take it for a finished one. A `.part` is
/// removed on any failure here; one left by a killed process is removed the
/// next time the same story is asked for.
async fn save_story(
    client: &IgClient,
    stories: &Stories,
    number: usize,
    dir: &Path,
) -> Result<Saved> {
    let stem = format!("{}-{number}", printable(&stories.username));
    if let Some(existing) = already_saved(dir, &stem) {
        return Ok(Saved::Already(existing));
    }

    let story = &stories.items[number - 1];
    let url = story
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("story {number} has no downloadable version"))?;

    let part = dir.join(format!("{stem}.part"));
    let _ = std::fs::remove_file(&part);
    let mut file = output::create_new(&part)?;
    let downloaded = match client.download_to(url, MAX_STORY_BYTES, &mut file).await {
        Ok(downloaded) => downloaded,
        Err(e) => {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(e.into());
        }
    };
    file.sync_all()
        .with_context(|| format!("could not finish writing {}", part.display()))?;
    drop(file);

    // `default_name` answers a bare name and checks the directory only for a
    // collision; joined here, or `-o somewhere` made the directory and the
    // file landed in the working directory.
    let name = match default_name(
        dir,
        &stories.username,
        number,
        extension_of(&downloaded.head),
    ) {
        Ok(name) => name,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    let path = dir.join(name);
    if let Err(e) = std::fs::rename(&part, &path) {
        let _ = std::fs::remove_file(&part);
        return Err(
            anyhow::Error::new(e).context(format!("could not move into place {}", path.display()))
        );
    }
    Ok(Saved::Now(path))
}

/// The file a story with this stem was saved to earlier, if any extension
/// `extension_of` can produce is already there.
fn already_saved(dir: &Path, stem: &str) -> Option<PathBuf> {
    ["mp4", "webp", "png", "jpg"]
        .into_iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|candidate| candidate.is_file())
}

/// Downloads all of them into a directory.
///
/// Keeps going past one that fails, and says so at the end. Stopping at the
/// first would leave the user with a partial set and no idea which ones are
/// missing — and a story URL expiring mid-run is the ordinary case here, not
/// the exceptional one. That covers the write as well as the fetch: a name
/// already taken in the directory is the ordinary case on a second run, and
/// it used to stop the loop at story one with the rest never attempted.
///
/// A Ctrl+C is the one failure that is not collected. Every fetch after it
/// answers `Canceled` at once, so carrying on would count them all as
/// failures and exit 1 for what the user did on purpose.
async fn download_many(
    client: std::sync::Arc<IgClient>,
    stories: &Stories,
    numbers: Vec<usize>,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let dir = destination.unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;

    // Several in flight rather than one after another. The CDN is a different
    // host with its own limits and is deliberately not paced (the reasoning
    // is on `IgClient::download_capped`), the client to it carries nothing
    // that names the account, and the connection is HTTP/2 -- so three
    // stories share one TCP+TLS connection instead of each waiting its own
    // round trip. Three is what a browser does when it opens a tray, and
    // past it a home link is the limit, not the latency.
    let stories = std::sync::Arc::new(stories.clone());
    let dir = std::sync::Arc::new(dir.to_path_buf());
    let slots = std::sync::Arc::new(Semaphore::new(STORY_DOWNLOADS_IN_FLIGHT));
    let mut tasks = JoinSet::new();
    for &number in &numbers {
        let (client, stories, dir, slots) = (
            std::sync::Arc::clone(&client),
            std::sync::Arc::clone(&stories),
            std::sync::Arc::clone(&dir),
            std::sync::Arc::clone(&slots),
        );
        tasks.spawn(async move {
            // A closed semaphore is impossible here -- nothing closes it --
            // so the only way this fails is the runtime shutting down, and
            // then there is nobody to report to.
            let _slot = slots.acquire_owned().await.ok()?;
            Some((number, save_story(&client, &stories, number, &dir).await))
        });
    }

    let total = numbers.len();
    let mut failed: Vec<(usize, String)> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        let Ok(Some((number, saved))) = joined else {
            // A task that panicked or was aborted: the runtime is going
            // down, or Ctrl+C already won below. Nothing to add.
            continue;
        };
        match saved {
            // Numbered, because they finish in whichever order the network
            // decides and the listing's numbers are what the user reads.
            Ok(Saved::Now(path)) => ui::info(&format!("Saved {number}: {}", path.display())),
            Ok(Saved::Already(path)) => {
                ui::info(&format!("Already saved {number}: {}", path.display()));
            }
            Err(e) if was_canceled(&e) => {
                // The token is shared, so every download still running has
                // already answered `Canceled` or is about to; aborting only
                // stops their reports from arriving after "stopped".
                tasks.abort_all();
                return Err(ExitError::new(ExitCode::Interrupted, "stopped").into());
            }
            Err(e) => failed.push((number, e.to_string())),
        }
    }

    if failed.is_empty() {
        return Ok(ExitCode::Ok);
    }
    failed.sort_by_key(|(number, _)| *number);
    let lines: Vec<String> = failed
        .iter()
        .map(|(number, why)| format!("{number}: {why}"))
        .collect();
    Err(ExitError::new(
        ExitCode::Error,
        format!(
            "{} of {} stories could not be downloaded:\n{}",
            failed.len(),
            total,
            lines.join("\n")
        ),
    )
    .into())
}

/// How many stories download at once under `-d all` or a set.
///
/// See the comment in [`download_many`]. Not configurable: a flag would be a
/// way to go faster, and this program's knobs only ever turn the other way.
const STORY_DOWNLOADS_IN_FLIGHT: usize = 3;

/// Whether a failed download was the user stopping it.
fn was_canceled(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|cause| matches!(cause.downcast_ref::<IgError>(), Some(IgError::Canceled)))
}

/// The bytes of one story, from the CDN.
pub(crate) async fn bytes_of(client: &IgClient, story: &Story) -> Result<Vec<u8>> {
    let url = story
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("Instagram described this story but offered no media for it"))?;
    Ok(client.download_capped(url, MAX_STORY_BYTES).await?)
}

/// What arrived, read from the bytes rather than from the URL.
///
/// The URL is no guide: Instagram's signed links carry `stp=dst-jpg`, an
/// instruction to the CDN to convert, so a path ending in `.webp` regularly
/// returns JPEG. The same reasoning as `pfp::Picture::extension`, with MP4
/// added — its `ftyp` box sits at offset four, after the box length.
pub(crate) fn extension_of(bytes: &[u8]) -> &'static str {
    let webp = bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP");
    match bytes {
        _ if bytes.get(4..8) == Some(b"ftyp") => "mp4",
        _ if webp => "webp",
        [0x89, b'P', b'N', b'G', ..] => "png",
        _ => "jpg",
    }
}

/// `someone-3.mp4`, next to whatever is already there.
pub(crate) fn default_name(
    dir: &Path,
    username: &str,
    number: usize,
    extension: &str,
) -> Result<PathBuf> {
    output::default_path(dir, &format!("{}-{number}", printable(username)), extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use snob_ig::model::{Candidates, PictureVersion};

    fn version(url: &str, w: u32, h: u32) -> PictureVersion {
        PictureVersion {
            url: url.into(),
            width: Some(w),
            height: Some(h),
        }
    }

    fn item(media_type: u8, images: Vec<PictureVersion>, videos: Vec<PictureVersion>) -> ReelItem {
        ReelItem {
            pk: "1".into(),
            media_type,
            taken_at: Epoch::new(1_000),
            expiring_at: Some(Epoch::new(2_000)),
            image_versions2: Some(Candidates { candidates: images }),
            video_versions: videos,
            reel_mentions: Vec::new(),
        }
    }

    #[test]
    fn the_biggest_version_wins_not_the_first() {
        let story = story_from(&item(
            1,
            vec![version("small", 320, 320), version("big", 1080, 1920)],
            vec![],
        ));
        assert_eq!(story.url.as_deref(), Some("big"));
    }

    /// A video item carries a poster frame in `image_versions2`. Taking the
    /// first URL of either list would hand back the poster for a video, which
    /// is a picture where a video was asked for.
    #[test]
    fn a_video_prefers_its_video_over_its_poster_frame() {
        let story = story_from(&item(
            2,
            vec![version("poster", 1080, 1920)],
            vec![version("clip", 720, 1280)],
        ));
        assert_eq!(story.kind, Kind::Video);
        assert_eq!(story.url.as_deref(), Some("clip"));
    }

    /// And falls back to it rather than to nothing, because a poster is more
    /// use than a refusal.
    #[test]
    fn a_video_with_no_video_falls_back_to_the_poster() {
        let story = story_from(&item(2, vec![version("poster", 1080, 1920)], vec![]));
        assert_eq!(story.url.as_deref(), Some("poster"));
    }

    /// A media type nobody has seen before is downloadable and honestly
    /// labeled, rather than called a photo and given a `.jpg`.
    #[test]
    fn an_unknown_media_type_is_not_guessed_at() {
        let story = story_from(&item(9, vec![version("something", 100, 100)], vec![]));
        assert_eq!(story.kind, Kind::Unknown);
        assert_eq!(story.kind.label(), "unknown");
        assert_eq!(story.url.as_deref(), Some("something"));
    }

    #[test]
    fn the_extension_comes_from_the_bytes() {
        let mut mp4 = vec![0, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypmp42");
        assert_eq!(extension_of(&mp4), "mp4");
        assert_eq!(extension_of(b"\x89PNG\r\n\x1a\n"), "png");
        assert_eq!(extension_of(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
        assert_eq!(extension_of(b"\xff\xd8\xff\xe0anything"), "jpg");
    }

    /// Zero is a number somebody types, and it must not underflow into the
    /// last item.
    #[test]
    fn story_zero_is_no_story() {
        assert_eq!(0usize.checked_sub(1), None);
    }

    #[test]
    fn an_unknown_expiry_is_not_reported_as_expired() {
        assert_eq!(remaining(None), "-");
    }
}
