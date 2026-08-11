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
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;

use crate::app::App;
use crate::cli::{Attr, Format, ListArgs};
use crate::engine::{self, ListOutcome};
use crate::output::{self, Presentation, Rendered};
use crate::report;
use crate::ui;

/// What opening a session produced.
///
/// An enum rather than an `Option` because the empty case is not "nothing
/// happened": it has already told the user what to do, and the caller's only
/// job is to return the matching code.
pub enum Session {
    Open(Box<App>),
    Missing,
}

/// Opens the app, or explains that there is no session and says so once.
///
/// Every command printed this same line and returned this same code. Having one
/// copy is what stops them drifting into three different ways of saying it.
pub fn open(args: &ListArgs, secrets: &SecretStore, paths: &AppPaths) -> Result<Session> {
    // No bar when the answer comes out of storage: there is nothing to watch.
    let with_progress = !args.no_progress && !args.cache;

    match App::open(secrets, paths, with_progress)? {
        Some(app) => Ok(Session::Open(Box::new(app))),
        None => {
            ui::no_session();
            Ok(Session::Missing)
        }
    }
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
pub fn destination(args: &ListArgs) -> Result<Destination> {
    let path = args.output.clone();
    let format = output::effective_format(args.format, path.as_deref());
    output::check_destination(format, path.as_deref())?;

    Ok(Destination {
        format,
        presentation: Presentation::detect(path.as_deref()),
        path,
    })
}

/// Builds the filter from the arguments.
pub fn filter_from(args: &ListArgs) -> Result<Filter> {
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
    args: &ListArgs,
    kind: ListKind,
    subject: &str,
    check: impl FnOnce(&ListOutcome) -> Result<()>,
) -> Result<(Vec<User>, ListOutcome)> {
    app.progress().begin(&report::walking(kind, subject));

    let (found, outcome) = match engine::list(app, args, kind).await {
        Ok(pair) => pair,
        Err(e) => {
            app.progress().finish();
            return Err(e);
        }
    };

    if let Err(e) = check(&outcome) {
        app.progress().finish();
        return Err(e);
    }

    Ok((found, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ListArgs {
        ListArgs {
            target: None,
            hide: vec![],
            only: vec![],
            no_verified: false,
            exclude_list: None,
            format: None,
            output: None,
            limit: None,
            refresh: false,
            cache: false,
            max_age: std::time::Duration::from_secs(6 * 3600),
            no_resume: false,
            max_pages: None,
            no_progress: true,
            yes: true,
        }
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
        let mut a = args();
        a.output = Some(PathBuf::from("result.csv"));
        assert_eq!(destination(&a).unwrap().format(), Format::Csv);

        // A spreadsheet on standard output is refused here, before anything is
        // spent finding out.
        let mut binary = args();
        binary.format = Some(Format::Xlsx);
        assert!(destination(&binary).is_err());
    }
}
