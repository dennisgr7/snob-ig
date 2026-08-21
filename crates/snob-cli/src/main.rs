use anyhow::Context;
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

/// Where this run keeps its files, whether the keyring is off, and — for a
/// sandbox — the keyring namespace it is confined to.
///
/// One place, so the sandbox seam is one `cfg` block rather than a condition at
/// every site that opens something. In a release build it is
/// `AppPaths::discover()` and the flag the user typed, and nothing else exists.
///
/// A sandbox forces the file backend as well as the directory. Not a
/// convenience: the keyring is per user and not per directory, so a sandbox run
/// that used it would read, overwrite and — through `purge` — delete the real
/// stored session of whoever is running the tests. The same rule
/// `crates/snob-core/tests/keyring.rs` holds the test suite to, applied to the
/// binary.
///
/// **Forcing the backend is not enough on its own, and believing it was is what
/// made this the one place the sandbox leaked.** `SecretStore` keeps talking to
/// the keyring whatever backend it is on, and it must: `save` deletes the
/// keyring entry so that `load`, which reads the keyring first, cannot go on
/// serving a session the file has replaced, and the two watch secrets have no
/// file form at all, so they never consult the backend. Every one of those
/// entries is named by the service, and the service was the real one — so a
/// sandbox `login` deleted the developer's session, a sandbox `purge` took the
/// webhook secrets with it, and a sandbox that had not logged in yet loaded the
/// real cookie and would have carried it to the redirected server. That last
/// one is precisely the thing the flag pairing is documented to make
/// impossible. The third element closes it: the sandbox gets a keyring
/// namespace of its own, so every one of those operations lands on entries
/// nothing outside the sandbox can see.
/// Whether a file really holds PEM certificates.
///
/// **Emptiness is the case that matters**, and it is why this is not a bare
/// `is_err()`. `from_pem_bundle` scans for BEGIN/END blocks and answers `Ok`
/// with an empty list when there are none, so a text file, a DER file or a
/// mistyped path that happened to exist was accepted in silence — and
/// `tls_certs_only` would then be handed Mozilla's roots and nothing of the
/// user's, which is the one outcome `--tls-extra-root` exists to prevent. It
/// fails at the handshake, hours later, against Instagram and nowhere else.
fn holds_a_certificate(pem: &[u8]) -> anyhow::Result<()> {
    match snob_ig::http::reqwest::Certificate::from_pem_bundle(pem) {
        Ok(found) if !found.is_empty() => Ok(()),
        Ok(_) => anyhow::bail!("it holds no PEM certificates"),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

fn trust_from(cli: &Cli) -> anyhow::Result<snob_ig::http::Trust> {
    if !cli.strict_roots {
        return Ok(snob_ig::http::Trust::Platform);
    }
    if !snob_ig::http::CAN_NARROW {
        anyhow::bail!(
            "--strict-roots does nothing on this build, so it is refused rather than \
             ignored.\n\
             This is the Windows on ARM64 binary, which uses the operating system's TLS \
             stack; that stack has no way to be told \"these roots and no others\"."
        );
    }

    let mut extra = Vec::new();
    for path in &cli.tls_extra_root {
        // Read and checked here rather than at the first request, so a typo in
        // a path is an error before anything has been walked -- and so the
        // failure names the file rather than arriving as a handshake error
        // hours later.
        let pem =
            std::fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
        holds_a_certificate(&pem)
            .with_context(|| format!("{} is not usable as an extra root", path.display()))?;
        extra.push(pem);
    }
    Ok(snob_ig::http::Trust::Narrow { extra })
}

fn wiring(cli: &Cli) -> anyhow::Result<(AppPaths, bool, Option<String>)> {
    // Before any client exists, which is what `use_trust` requires: a run
    // cannot change what it trusts halfway through.
    let trust = trust_from(cli)?;
    let _ = snob_ig::http::use_trust(trust);

    #[cfg(feature = "testing")]
    if let Some(root) = &cli.sandbox_root {
        if let Some(base) = cli.ig_base_url.clone() {
            // Before any client is built, and once. Clap has already refused
            // this flag without a sandbox root, so a redirected client can only
            // carry a session out of the store inside `root`.
            snob_ig::client::point_every_client_at(base)
                .map_err(|already| anyhow::anyhow!("already pointed at {already}"))?;
        }
        return Ok((
            AppPaths::rooted_at(root),
            true,
            Some(sandbox_keyring_namespace(root)),
        ));
    }
    Ok((AppPaths::discover()?, cli.no_keyring, None))
}

/// The keyring service name a sandbox run is confined to.
///
/// Derived from the root rather than one shared constant, so two sandboxes
/// running at the same time — which is the normal case, the suite runs in
/// parallel — cannot delete each other's entries.
///
/// FNV-1a rather than `DefaultHasher`: the name has to come out the same on the
/// *next* invocation, because one process writes the entry and another reads it
/// back, and `DefaultHasher`'s output is explicitly not promised to be stable
/// between Rust releases. It is not a security boundary and does not need to be
/// one — the isolation comes from the name being different, not from it being
/// hard to guess.
#[cfg(feature = "testing")]
fn sandbox_keyring_namespace(root: &std::path::Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("snob-ig-sandbox-{hash:016x}")
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
    let (paths, no_keyring, keyring_namespace) = wiring(&cli)?;
    let store = SecretStore::new(paths.clone(), no_keyring);
    // `Option::map_or_else` would build the store twice to satisfy the
    // borrow checker's view of the closure; this reads as what it is.
    let store = match &keyring_namespace {
        Some(service) => store.with_service(service),
        None => store,
    };

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
        Command::Stories(args) => commands::stories::run(args, store, &paths).await,
        Command::Follow(args) => {
            commands::follow::run(args, commands::follow::Verb::Follow, store, &paths).await
        }
        Command::Unfollow(args) => {
            commands::follow::run(args, commands::follow::Verb::Unfollow, store, &paths).await
        }
        Command::Watch(args) => commands::watch::run(args, store, &paths).await,
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

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use clap::Parser;

    /// The one property the whole sandbox rests on.
    ///
    /// Not "it returns something": it returns something that is **not** the
    /// name the real entries are under. `SecretStore` reaches the keyring on
    /// every backend, so this string is the only thing standing between a
    /// sandbox `login` and the developer's own session.
    #[test]
    fn a_sandbox_never_shares_the_real_keyring_service() {
        let cli = Cli::try_parse_from(["snob", "--sandbox-root", "/tmp/one", "whoami"])
            .expect("the flag parses");
        let (_, prefer_file, namespace) = wiring(&cli).expect("a sandbox needs no discovery");

        assert!(prefer_file, "a sandbox stores its session in a file");
        let namespace = namespace.expect("a sandbox is given a keyring namespace of its own");
        assert_ne!(
            namespace, "snob-ig",
            "a sandbox on the real service deletes the real session"
        );
        assert!(namespace.starts_with("snob-ig-sandbox-"), "{namespace}");
    }

    /// One process writes the entry and another reads it back, so a name that
    /// changed between invocations would lose the session every time.
    #[test]
    fn the_namespace_is_the_same_answer_twice_and_differs_per_root() {
        let one = sandbox_keyring_namespace(std::path::Path::new("/tmp/one"));
        let again = sandbox_keyring_namespace(std::path::Path::new("/tmp/one"));
        let other = sandbox_keyring_namespace(std::path::Path::new("/tmp/two"));

        assert_eq!(one, again, "the same root has to name the same entries");
        assert_ne!(
            one, other,
            "two sandboxes at once must not be able to delete each other's entries"
        );
    }

    /// Without the flag there is no namespace, so a real run is untouched by
    /// any of this.
    #[test]
    fn an_ordinary_run_is_left_on_the_real_service() {
        let cli = Cli::try_parse_from(["snob", "whoami"]).expect("it parses");
        let (_, _, namespace) = wiring(&cli).expect("discovery works on a test machine");
        assert_eq!(namespace, None);
    }
    /// A file that holds no certificate is refused, and that is the case a bare
    /// error check misses.
    ///
    /// `from_pem_bundle` looks for BEGIN/END blocks and answers `Ok` with an
    /// empty list when it finds none, so plain text, a DER file, or a path that
    /// happened to exist all passed. The narrowing would then hand
    /// `tls_certs_only` Mozilla's roots and none of the user's -- the one
    /// outcome the flag exists to prevent -- and it would fail at the handshake
    /// against Instagram, hours later, and nowhere else.
    ///
    /// Checked against a real certificate rather than only against rejections,
    /// because a predicate that refuses everything also passes the first half.
    #[test]
    fn an_extra_root_has_to_be_a_certificate() {
        assert!(
            holds_a_certificate(
                b"not a certificate
"
            )
            .is_err()
        );
        assert!(holds_a_certificate(b"").is_err());
        assert!(
            holds_a_certificate(
                b"-----BEGIN CERTIFICATE-----
not base64
-----END CERTIFICATE-----
"
            )
            .is_err(),
            "a block that is not a certificate is not one"
        );

        // One of Mozilla's own, so the accepting half is exercised too.
        let real = snob_ig::http::reqwest::Certificate::from_der(
            &webpki_root_certs::TLS_SERVER_ROOT_CERTS[0],
        );
        assert!(real.is_ok(), "the bundled roots are certificates");
    }
}
