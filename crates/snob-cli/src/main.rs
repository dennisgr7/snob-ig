use clap::Parser;
use snob_cli::cli::{Cli, Command};
use snob_cli::commands;
use snob_cli::commands::sets::SetOp;
use snob_cli::exit::ExitCode;
use snob_core::model::ListKind;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;

/// Two workers, always two, whatever the machine has.
///
/// The default is one per core, which is both too many and — on the machines
/// this project explicitly supports — too few. Too many because the program
/// makes one request at a time and spends most of its life asleep between them,
/// so thirty-two worker threads on a thirty-two core desktop are thirty-one
/// doing nothing. Too few because on a one-vCPU server or container the default
/// is **one**, and that is the case that breaks something.
///
/// It breaks Ctrl+C. `tokio::signal::ctrl_c()` permanently disables the process
/// default handler from the first call onwards, so the only thing that can stop
/// a run is the task waiting on that signal. Give it a single worker and let
/// anything block that worker — the consent prompt waiting on a human, a
/// request budget waiting out the busy timeout — and the signal task cannot be
/// scheduled at all. Ctrl+C then does nothing, twice, and the run cannot be
/// stopped. A homelab is a first-class place to run this, and a one-vCPU box is
/// what a homelab is.
///
/// The second worker is what guarantees there is always somewhere for that task
/// to go.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    restore_terminal_on_panic();

    match run(cli).await {
        Ok(code) => code.into(),
        Err(e) => {
            snob_cli::report::print_error(&e);
            exit_code_for(&e).into()
        }
    }
}

/// Looks for an Instagram error in the cause chain so the exit code is one the
/// v2 service can interpret without reading text.
fn exit_code_for(error: &anyhow::Error) -> ExitCode {
    // An error that already knows its code wins: it was set by whoever refused
    // the result, which is more specific than anything reconstructed from an
    // Instagram error further down.
    if let Some(code) = ExitCode::from_chain(error) {
        return code;
    }

    error
        .chain()
        .find_map(|cause| {
            cause
                .downcast_ref::<snob_ig::error::IgError>()
                .or_else(|| {
                    cause
                        .downcast_ref::<snob_ig::login::LoginError>()
                        .and_then(|e| e.as_instagram())
                })
                .map(ExitCode::from_ig_error)
        })
        .unwrap_or(ExitCode::Error)
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let paths = AppPaths::discover()?;
    let store = SecretStore::new(paths.clone(), cli.no_keyring);

    match cli.command {
        Command::Login(args) => commands::login::run(args, store, &paths).await,
        Command::Whoami(args) => commands::whoami::run(args, store, &paths).await,
        Command::Logout(args) => commands::logout::run(args, store, &paths),
        Command::Purge(args) => commands::purge::run(args, store, &paths),
        Command::Followers(args) => {
            commands::lists::run(args, store, &paths, ListKind::Followers).await
        }
        Command::Following(args) => {
            commands::lists::run(args, store, &paths, ListKind::Following).await
        }
        Command::Scan(args) => commands::scan::run(args, store, &paths).await,
        Command::Unfollowers(args) => {
            commands::sets::run(args, store, &paths, SetOp::Unfollowers).await
        }
        Command::Fans(args) => commands::sets::run(args, store, &paths, SetOp::Fans).await,
        Command::Friends(args) => commands::sets::run(args, store, &paths, SetOp::Friends).await,
        Command::Pfp(args) => commands::pfp::run(args, store, &paths).await,
    }
}

/// Gives the cursor back if the process dies with a menu on screen.
///
/// The release profile is `panic = "abort"`, so nothing runs on the way out —
/// and `dialoguer` hides the cursor while a prompt is up. A panic during the
/// login menu therefore left the user with an invisible cursor for the rest of
/// their shell session, which reads as the terminal being broken rather than
/// as this program having failed.
///
/// The hook still runs under an aborting runtime: `set_hook` is documented to
/// run with both runtimes, which is the same property `cdp::kill_on_panic`
/// depends on.
fn restore_terminal_on_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        snob_cli::ui::restore_terminal();
        previous(info);
    }));
}

fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose {
        EnvFilter::new("snob=debug,snob_ig=debug,snob_core=debug,snob_cli=debug")
    } else {
        EnvFilter::try_from_env("SNOB_LOG").unwrap_or_else(|_| EnvFilter::new("warn"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}
