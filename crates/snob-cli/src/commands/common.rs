//! The three things every list command does before it does anything else.
//!
//! Open the session, settle where the result is going, and build the filter.
//! They live together because the order between them matters: the destination
//! is checked **before** the session is opened and long before a request is
//! spent, so `-o` pointing somewhere impossible costs nothing to find out.

use std::path::PathBuf;

use anyhow::{Context, Result};
use snob_core::filters::{Attribute, Filter, parse_username_list};
use snob_core::model::{ListKind, User};
use snob_store::paths::AppPaths;
use snob_store::secrets::SecretStore;

use crate::app::App;
use crate::cli::{Attr, FilterArgs, Format, ListArgs, OutputArgs, ScanArgs, WalkArgs};
use crate::engine::{self, ListOutcome};
use crate::exit::{ExitCode, ExitError};
use crate::output::{self, Presentation, Rendered};
use crate::report;

/// What opening a session produced, for the one caller that has something
/// to do without one.
///
/// `snob watch check` reports a missing session as one finding among
/// several rather than stopping at it. Everything else wants [`app`], which
/// turns the empty case into the refusal every command gives.
pub enum Session {
    Open(Box<App>),
    Missing,
}

/// Opens the app for a list command, or refuses because there is no session.
pub fn open(walk: &WalkArgs, secrets: &SecretStore, paths: &AppPaths) -> Result<Box<App>> {
    // No bar when the answer comes out of storage: there is nothing to watch.
    app(secrets, paths, !walk.no_progress && !walk.cache)
}

/// Opens the app, or refuses because there is no session.
///
/// Every command printed the same line and returned the same code, by hand,
/// and what that cost was not drift but shape: a refusal printed as a line
/// and returned as `Ok` went round `report::print_error`, so a caller that
/// had asked for JSON got English on standard error for the one failure it
/// is likeliest to meet. It is an error now, with the code and the hint
/// every other refusal carries, and the printer decides how to say it.
pub fn app(secrets: &SecretStore, paths: &AppPaths, with_progress: bool) -> Result<Box<App>> {
    match open_with_progress(with_progress, secrets, paths)? {
        Session::Open(app) => Ok(app),
        Session::Missing => Err(no_session()),
    }
}

/// The refusal every command gives when there is no session.
pub fn no_session() -> anyhow::Error {
    ExitError::new(ExitCode::NoSession, "no session is stored")
        .with_hint("run \"snob login\"")
        .into()
}

/// Opens the app, and says whether there was a session to open it with.
pub fn open_with_progress(
    with_progress: bool,
    secrets: &SecretStore,
    paths: &AppPaths,
) -> Result<Session> {
    match App::open(secrets, paths, with_progress)? {
        Some(app) => Ok(Session::Open(Box::new(app))),
        None => Ok(Session::Missing),
    }
}

/// The part of the command line the engine is asked with.
///
/// Here and not in `engine`, so the engine knows nothing about clap: this is
/// the one place the parser's structs are read for what the engine needs.
/// Two commands carry a walk, and both become the same query.
fn query(target: &Option<String>, walk: &WalkArgs) -> engine::ListQuery {
    engine::ListQuery {
        target: target.clone(),
        yes: walk.yes,
        refresh: walk.refresh,
        cache: walk.cache,
        max_age: walk.max_age,
        no_resume: walk.no_resume,
        max_pages: walk.max_pages,
    }
}

impl From<&ListArgs> for engine::ListQuery {
    fn from(args: &ListArgs) -> Self {
        query(&args.target, &args.walk)
    }
}

impl From<&ScanArgs> for engine::ListQuery {
    fn from(args: &ScanArgs) -> Self {
        query(&args.target, &args.walk)
    }
}

/// What is left of a list after the filter and the cap, and how many there
/// were at each step -- the three numbers the summary line is built from.
///
/// Four lines, written twice, in the two commands that print a list. The
/// numbers have to be taken in this order -- total before the filter, kept
/// after it, shown after the cap -- and two copies of an order are two
/// places to get it wrong.
pub struct Narrowed {
    pub shown: Vec<User>,
    /// After the filter, before the cap.
    pub kept: usize,
    /// Before the filter.
    pub total: usize,
}

pub fn narrow(users: Vec<User>, filter: &Filter, limit: Option<usize>) -> Narrowed {
    let total = users.len();
    let mut shown = filter.apply(users);
    let kept = shown.len();
    if let Some(cap) = limit {
        shown.truncate(cap);
    }
    Narrowed { shown, kept, total }
}

/// Refuses a command outright while the account is in cooldown.
///
/// For the commands that have nothing stored to serve instead -- a picture,
/// a story, a write. The list commands do not come here: `engine::cooldown`
/// answers them out of storage, which a refusal cannot. The gate is still
/// explicit at each call site, before anything is asked of a person and
/// before anything is spent; what is shared is the sentence, which three
/// commands had written out in two spellings.
pub fn refuse_during_cooldown(app: &App, doing: &str) -> Result<()> {
    if let Some(until_ms) = app.client().pacer().cooldown()? {
        return Err(ExitError::new(
            ExitCode::RateLimited,
            format!(
                "the account is in cooldown until {}, so {doing}",
                report::cooldown_ends_at(until_ms)
            ),
        )
        .into());
    }
    Ok(())
}

/// Where the result goes and what shape it takes.
///
/// Worked out once, up front, and carried around: a destination that cannot
/// hold the format must not cost a walk — or two — to find out about.
pub struct Destination {
    format: Format,
    presentation: Presentation,
    path: Option<PathBuf>,
}

impl Destination {
    pub fn format(&self) -> Format {
        self.format
    }

    pub fn presentation(&self) -> Presentation {
        self.presentation
    }

    /// Whether a person is watching this appear, which is what advice and
    /// decoration are for. A file and a pipe are neither.
    pub fn is_interactive(&self) -> bool {
        self.presentation.interactive
    }

    pub fn write(&self, users: &[User]) -> Result<()> {
        output::write(users, self.format, self.presentation, self.path.as_deref())
    }

    pub fn write_rendered(&self, rendered: &Rendered) -> Result<()> {
        output::write_rendered(rendered, self.path.as_deref())
    }
}

/// Settles the destination and refuses up front what would only fail later.
pub fn destination(args: &OutputArgs) -> Result<Destination> {
    let path = args.path.clone();
    let format = output::effective_format(args.format, path.as_deref());
    output::check_destination(format, path.as_deref())?;

    Ok(Destination {
        format,
        presentation: Presentation::detect(path.as_deref()),
        path,
    })
}

/// Builds the filter from the arguments.
pub fn filter_from(args: &FilterArgs) -> Result<Filter> {
    let mut hide: Vec<Attribute> = args.hide.iter().copied().map(attribute).collect();
    if args.no_verified && !hide.contains(&Attribute::Verified) {
        hide.push(Attribute::Verified);
    }

    let excluded = match &args.exclude_list {
        Some(path) => {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            parse_username_list(&contents)
        }
        None => Default::default(),
    };

    Ok(Filter {
        hide,
        only: args.only.iter().copied().map(attribute).collect(),
        excluded,
    })
}

fn attribute(a: Attr) -> Attribute {
    match a {
        Attr::Verified => Attribute::Verified,
        Attr::Private => Attribute::Private,
        Attr::NoPfp => Attribute::NoPfp,
    }
}

/// One walk, with the bar named while it runs and taken down before anything
/// gives up.
///
/// The rule this holds is "finish the bar before the `?`". `indicatif` leaves
/// its last line on screen when it is dropped, so a run ending in a cooldown
/// refusal, a private account or an incomplete list printed the error
/// underneath a spinner that had stopped spinning. It was written out at four
/// call sites, twice as a seven-line `match` whose only job was to call
/// `finish` on the way past — the kind of rule AGENTS.md says belongs in the
/// one place that cannot be bypassed, because the fifth caller is the one that
/// forgets.
///
/// `check` runs inside the guarded region rather than after it. A crossing has
/// to know its first list is complete before spending the second walk, and that
/// refusal leaves through the same door as any other.
///
/// It does **not** finish on success: a crossing walks two lists through one
/// bar, and clearing it in between would make the second half start from a
/// blank line. The caller ends it when the run is over.
pub async fn walk_named(
    app: &mut App,
    args: impl Into<engine::ListQuery>,
    kind: ListKind,
    subject: &str,
    check: impl FnOnce(&ListOutcome) -> Result<()>,
) -> Result<(Vec<User>, ListOutcome)> {
    app.progress().begin(&report::walking(kind, subject));

    // Both failures leave through one door, so the rule this function exists to
    // hold is written once inside it too. Two copies of `finish()` here would
    // make a third failure point added between them one more place to remember.
    let result = engine::list(app, args, kind).await.and_then(|pair| {
        check(&pair.1)?;
        Ok(pair)
    });
    if result.is_err() {
        app.progress().finish();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> FilterArgs {
        FilterArgs::default()
    }

    #[test]
    fn with_no_flags_the_filter_is_empty() {
        assert!(filter_from(&args()).unwrap().is_empty());
    }

    #[test]
    fn no_verified_is_shorthand_for_hiding_verified() {
        let mut a = args();
        a.no_verified = true;
        let f = filter_from(&a).unwrap();
        assert_eq!(f.hide, vec![Attribute::Verified]);
    }

    #[test]
    fn the_shorthand_does_not_duplicate_what_was_already_there() {
        let mut a = args();
        a.no_verified = true;
        a.hide = vec![Attr::Verified];
        assert_eq!(filter_from(&a).unwrap().hide.len(), 1);
    }

    #[test]
    fn it_translates_all_three_attributes() {
        let mut a = args();
        a.hide = vec![Attr::Verified, Attr::Private, Attr::NoPfp];
        let f = filter_from(&a).unwrap();
        assert_eq!(
            f.hide,
            vec![Attribute::Verified, Attribute::Private, Attribute::NoPfp]
        );
    }

    #[test]
    fn an_exclusion_file_that_does_not_exist_gives_a_clear_error() {
        let mut a = args();
        a.exclude_list = Some(PathBuf::from("no-such-file.txt"));
        let error = filter_from(&a).unwrap_err().to_string();
        assert!(error.contains("could not read"));
    }

    /// The extension decides the format when nothing else did, and it has to
    /// be settled before the first request rather than after the walk.
    #[test]
    fn the_destination_is_settled_from_the_arguments() {
        let to_file = OutputArgs {
            format: None,
            path: Some(PathBuf::from("result.csv")),
        };
        assert_eq!(destination(&to_file).unwrap().format(), Format::Csv);

        // A spreadsheet on standard output is refused here, before anything is
        // spent finding out.
        let binary = OutputArgs {
            format: Some(Format::Xlsx),
            path: None,
        };
        assert!(destination(&binary).is_err());
    }
}
