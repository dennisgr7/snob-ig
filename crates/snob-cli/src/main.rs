use clap::Parser;
use snob_cli::cli::{Cli, Command};
use snob_cli::commands;
use snob_cli::commands::sets::SetOp;
use snob_cli::exit::ExitCode;
use snob_core::model::ListKind;
use snob_core::paths::AppPaths;
use snob_core::secrets::SecretStore;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match run(cli).await {
        Ok(code) => code.into(),
        Err(e) => {
            eprintln!("error: {e}");
            for cause in e.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
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
