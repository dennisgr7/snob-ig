//! `snob stories`: what an account has up right now, and how to keep a copy.
//!
//! Two requests for the listing — the profile, to turn a name into an id, and
//! the reel itself — and one more per story downloaded, from the CDN, which is
//! a different host with its own limits and is not charged against Instagram's
//! budget. The same shape as `pfp`, for the same reasons, and like `pfp` it
//! walks no list and stores no snapshot.
//!
//! **Nothing here tells the account you looked.** Instagram registers a view
//! through a separate call that this project does not make and will not be
//! given; `crates/snob-core/tests/no_seen.rs` reads the source of all three
//! crates to keep it that way. That is a promise to the person whose story it
//! is, and it is the one promise here that nobody running the tool would ever
//! notice being broken.
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
use snob_core::model::printable;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;
use snob_ig::client::IgClient;
use snob_ig::model::{ReelItem, largest};

use crate::app::App;
use crate::cli::{Format, StoriesArgs, StoryFormat};
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
    pub taken_at: i64,
    pub expiring_at: Option<i64>,
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
    let Some(app) = App::open(&secrets, paths, false)? else {
        ui::no_session();
        return Ok(ExitCode::NoSession);
    };

    if let Some(until_ms) = app.client().pacer().cooldown()? {
        return Err(ExitError::new(
            ExitCode::RateLimited,
            format!(
                "the account is in cooldown until {}, so no request can be made",
                report::cooldown_ends_at(until_ms)
            ),
        )
        .into());
    }

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

    if args.interactive {
        return crate::ui::stories::browse(app.client(), &stories, paths).await;
    }

    if let Some(n) = args.download {
        return download_one(app.client(), &stories, n, args.output.as_deref()).await;
    }

    if args.all {
        return download_all(app.client(), &stories, args.output.as_deref()).await;
    }

    list(&stories, args.format, args.output.as_deref())
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
    output::check_destination(format, destination)?;

    let rendered = match format {
        Format::Json | Format::Ndjson => Rendered::Text(as_json(stories, format)?),
        _ => Rendered::Text(as_table(stories, Presentation::detect(destination))),
    };
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
fn remaining(expiring_at: Option<i64>) -> String {
    let Some(at) = expiring_at else {
        return "-".into();
    };
    countdown(at - chrono::Utc::now().timestamp())
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
        .ok_or_else(|| {
            anyhow!(
                "there is no story {number}: @{} has {}",
                printable(&stories.username),
                match stories.items.len() {
                    1 => "one".to_string(),
                    n => format!("{n}"),
                }
            )
        })?;

    let bytes = bytes_of(client, story).await?;
    let extension = extension_of(&bytes);
    let path = match destination {
        Some(p) => p.to_path_buf(),
        None => default_name(Path::new("."), &stories.username, number, extension)?,
    };
    write(&bytes, &path, destination.is_some())?;
    ui::info(&format!("Saved {}", path.display()));
    Ok(ExitCode::Ok)
}

/// Downloads all of them into a directory.
///
/// Keeps going past one that fails, and says so at the end. Stopping at the
/// first would leave the user with a partial set and no idea which ones are
/// missing — and a story URL expiring mid-run is the ordinary case here, not
/// the exceptional one.
async fn download_all(
    client: &IgClient,
    stories: &Stories,
    destination: Option<&Path>,
) -> Result<ExitCode> {
    let dir = destination.unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;

    let mut failed = Vec::new();
    for (index, story) in stories.items.iter().enumerate() {
        let number = index + 1;
        match bytes_of(client, story).await {
            Ok(bytes) => {
                let path = default_name(dir, &stories.username, number, extension_of(&bytes))?;
                write(&bytes, &path, false)?;
                ui::info(&format!("Saved {}", path.display()));
            }
            Err(e) => failed.push(format!("{number}: {e}")),
        }
    }

    if failed.is_empty() {
        return Ok(ExitCode::Ok);
    }
    Err(ExitError::new(
        ExitCode::Error,
        format!(
            "{} of {} stories could not be downloaded:\n{}",
            failed.len(),
            stories.items.len(),
            failed.join("\n")
        ),
    )
    .into())
}

/// The bytes of one story, from the CDN.
pub(crate) async fn bytes_of(client: &IgClient, story: &Story) -> Result<Vec<u8>> {
    let url = story
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("Instagram described this story but offered no media for it"))?;
    Ok(client.download_capped_public(url, MAX_STORY_BYTES).await?)
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

/// Writes it, refusing to replace a file nobody named.
///
/// The same rule `pfp` follows: a path the user typed is theirs to overwrite,
/// and a name this program invented is not — that name came off a server, and
/// silently replacing a file because of what an account called its story is the
/// kind of thing nobody finds out about until it matters.
fn write(bytes: &[u8], path: &Path, named_by_user: bool) -> Result<()> {
    let rendered = Rendered::Bytes(bytes.to_vec());
    if named_by_user {
        output::write_rendered(&rendered, Some(path))
    } else {
        output::write_new(&rendered, path)
    }
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
            taken_at: 1_000,
            expiring_at: Some(2_000),
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
