//! Getting a report to a receiver.
//!
//! One file, because the rules here are about one another and were six hundred
//! lines apart. Whether a stored credential may be attached is decided in
//! [`plan`]; whether the address it would go to can be posted to at all is
//! decided by `watch::webhook::check`, which [`delivery_from`] calls in the very
//! next statement — and they lived in different modules, under a header about
//! `diff` and `once`. What a queued report may be handed to on a later run is
//! [`destination_of`] and [`drain`], and that rule is the same rule again, one
//! process later.
//!
//! The split is by question rather than by layer: [`super`] decides what a
//! report says, this decides where it goes and what travels with it.

use super::*;

/// Where reports go, once the arguments have been checked.
pub(super) struct Delivery {
    pub(super) client: WebhookClient,
    pub(super) heartbeat: bool,
    /// Whether the body will carry `X-Snob-Signature`. Held so `check` can say
    /// what it just sent rather than guessing at what the client did.
    pub(super) signed: bool,
    /// The address this run posts to, as [`destination_of`] spells it. Written
    /// onto every report it queues and used to filter the outbox, so a queued
    /// report can only ever be sent to the address it was addressed to — and
    /// read back by `snob watch status`, which has to ask the queue the same
    /// question a run would.
    pub(super) destination: String,
}

/// What this run would send, worked out without touching the keyring or the
/// network.
///
/// Split out of [`delivery_from`] so the four rules it holds can be reached by a
/// test at all. They could not be: that function takes a `&SecretStore` and
/// hands back a live HTTP client, so nothing in the suite could ask it a
/// question -- and replacing the origin comparison below with `true` left all
/// 656 tests green, on the guard whose own comment records a team's `X-Api-Key`
/// and the stored bearer token arriving at `webhook.site`.
struct Planned {
    webhook: Webhook,
    heartbeat: bool,
}

/// Decides what to send, and what to say about it.
///
/// **The warnings come back rather than being printed**, because a decision that
/// prints is a decision that cannot be checked. They are in the order they have
/// to be said: either the no-webhook one on its own, or the withheld-headers one
/// and then the withheld-token one.
///
/// The two stored secrets are read by the caller rather than in here, which is
/// what makes this reachable without a keyring. One consequence, written down
/// rather than left to be found: a machine whose keyring is broken now reports
/// the keyring's error where it used to report the clearer one about an empty
/// `--sign-with`. The session opened moments later would have reported it anyway.
fn plan(
    args: &WebhookArgs,
    from_file: Option<&WebhookConfig>,
    stored_token: Stored,
    stored_key: Stored,
) -> Result<(Option<Planned>, Vec<String>)> {
    let mut warnings: Vec<String> = Vec::new();

    // Whether the credential store could be consulted at all, asked before the
    // secrets are unwrapped.
    //
    // "Nothing is stored" and "this process cannot reach the store" used to be
    // the same answer, and both of the warning arms below sit inside a `Some`,
    // so two absent secrets produced no warning and a client with neither an
    // `Authorization` nor a signing key. The report went out naming the user's
    // followers, unauthenticated and unsigned, and the only trace was a
    // `debug!` line nobody sees. `setup` stores the token keyring-only whatever
    // `--no-keyring` said, so the box where the session is in the file fallback
    // and the keyring is not reachable — a login with `--no-keyring`, then
    // `setup`, then cron with no session bus — is the reachable one.
    let store_unreadable = stored_token.is_unreachable() || stored_key.is_unreachable();
    let (stored_token, stored_key) = (stored_token.found(), stored_key.found());

    let Some(url) = args
        .webhook
        .clone()
        .or_else(|| from_file.map(|w| w.url.clone()))
    else {
        if !args.header.is_empty() || args.sign_with.is_some() || args.heartbeat {
            warnings.push(
                "there is no webhook, so nothing is sent and those options do nothing".to_string(),
            );
        }
        return Ok((None, warnings));
    };

    let url = Url::parse(&url).with_context(|| format!("\"{url}\" is not an address"))?;

    // Whether this run is posting to the address the configuration was written
    // for.
    //
    // Nothing used to ask. `--webhook` replaced the URL and the file's headers
    // and the keyring token came along regardless, so
    // `snob watch once --webhook https://webhook.site/<id>` -- which is precisely
    // what somebody does to see what the payload looks like -- sent the team's
    // `X-Api-Key` and the bearer token `setup` had stored to a host nobody had
    // configured. Not malice, debugging. `check` was happy because the new
    // address was https.
    //
    // Compared by origin rather than by string, so a path or a query on the same
    // host is still the same destination. The project already has the pattern:
    // `IgClient::check_downloadable` exists so the CDN cannot be handed a
    // credential meant for somewhere else.
    // **An origin there is nothing to compare against is not a match.** With
    // `is_none_or`, absence read as sameness, so the guard above held only in
    // the one case it was written for and fell open in two others: a keyring
    // token with no `[webhook]` in the file at all -- which `setup` can leave
    // behind, since it stored the secrets before writing the file -- and a
    // `[webhook]` whose `url` does not parse, which nothing validates because
    // the file is documented as safe to hand-edit. Either one sent the stored
    // token to whatever `--webhook` named.
    //
    // The question is really "did anything override the configured address",
    // so that is what is asked. Without `--webhook` the address came from the
    // file and is the configured one by construction; with it, sameness has to
    // be demonstrated rather than assumed.
    let configured_origin = from_file
        .and_then(|w| Url::parse(&w.url).ok())
        .map(|configured| configured.origin());
    let same_destination = match &configured_origin {
        Some(origin) => *origin == url.origin(),
        None => args.webhook.is_none(),
    };

    // Every message below names the address, and the address may carry a
    // password: `check` refuses one, but these are built before it runs, so a
    // refusal that printed the secret three times first is not a refusal.
    let shown = crate::watch::webhook::shown(&url);

    /// Names the origin a stored credential belongs to, for a warning about
    /// not sending it. "the configuration" when the file has no usable address
    /// to name — which is one of the ways the guard used to fall open, so the
    /// warning has to be able to say it.
    fn configured_for(origin: Option<&url::Origin>) -> String {
        origin.map_or_else(
            || "the configuration".to_string(),
            |o| o.ascii_serialization(),
        )
    }

    // Headers from the file first, then the ones typed, so a flag can override
    // a configured one of the same name -- the last one wins at the request.
    let mut headers: Vec<(String, String)> = from_file
        .filter(|_| same_destination)
        .map(|w| {
            w.headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !same_destination && from_file.is_some_and(|w| !w.headers.is_empty()) {
        warnings.push(format!(
            "{shown} is not the address in the configuration, so the headers configured there are \
             not sent with it. Pass what this one needs with --header."
        ));
    }

    for raw in &args.header {
        let (name, value) = raw.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("\"{raw}\" is not a header; write it as \"Name: value\"")
        })?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    // Stored secrets fill in what was not passed. This is the whole point of
    // `setup` having put them in the keyring: a systemd unit runs `snob watch`
    // with no arguments and the token is not in the unit file, the process
    // table, or anybody's shell history.
    //
    // The guard looks at the **merged** list, not only at the flags. Inspecting
    // `args.header` alone meant a configured `Authorization` and a stored token
    // both went out -- and the second one was the secret `setup` had put away.
    //
    // And only to the address it was stored for. A token is a credential like
    // the session cookie, and the one guard this module is arranged around -- the
    // client cannot carry the session -- said nothing about this one.
    let authorization_given = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    if !authorization_given && let Some(token) = stored_token {
        if same_destination {
            headers.push(("Authorization".to_string(), token.expose().to_string()));
        } else {
            warnings.push(format!(
                "the stored token was set up for {}, so it is not sent to {shown}. Pass one with \
                 --header \"Authorization: ...\" if this address needs it.",
                configured_for(configured_origin.as_ref()),
            ));
        }
    }

    let key = match args.sign_with.clone() {
        // An empty `--sign-with` is not a request to sign with nothing: it is
        // `--sign-with ${SNOB_KEY}` in a unit file where the variable is unset.
        // Taken literally it signs with a zero-length key -- a well-formed
        // signature anybody can forge -- and, because this arm wins over the
        // keyring, it would silently replace a key that was configured
        // correctly. `ask_webhook` already refuses an empty one; the flag has
        // to agree with it.
        Some(given) if given.trim().is_empty() => bail!(
            "--sign-with was given an empty value. If that came from an environment variable \
             that is not set, leave the flag out: the key stored by \"snob watch setup\" is \
             used when it is absent."
        ),
        Some(given) => Some(Secret::from(given)),
        // The same gate as the token, and it had none at all. The bearer token
        // was correctly withheld from a foreign address *and then the body was
        // signed with the stored key anyway*, so the warning misdescribed what
        // had happened and the third party got a body and its MAC — everything
        // needed to guess a human-chosen shared secret offline, at leisure.
        //
        // A signature is a credential in the same way a token is: it is not
        // what the message says, it is proof of who it is from.
        None if same_destination => stored_key,
        None => {
            if stored_key.is_some() {
                warnings.push(format!(
                    "the stored signing key was set up for {}, so the report sent to {shown} is \
                     not signed. Pass one with --sign-with if this address checks the signature.",
                    configured_for(configured_origin.as_ref()),
                ));
            }
            None
        }
    };

    // Said whatever the file asks for, because the file is what says a
    // credential was ever meant to travel. It cannot be asked whether one was
    // stored -- that is the question that could not be answered.
    if store_unreadable && from_file.is_some() {
        warnings.push(
            "the system keyring could not be read, so any token or signing key stored by \
             \"snob watch setup\" is not being used: this report goes out unauthenticated and \
             unsigned. Pass them with --header and --sign-with, or run where the keyring is \
             reachable."
                .to_string(),
        );
    }

    Ok((
        Some(Planned {
            webhook: Webhook { url, headers, key },
            heartbeat: args.heartbeat || from_file.is_some_and(|w| w.heartbeat),
        }),
        warnings,
    ))
}

/// Reads the webhook arguments, or explains what is wrong with them.
///
/// Called before anything is opened or spent. A run that would have shouted a
/// token over plain HTTP fails while somebody is still there to read the
/// message, rather than six hours later into a log.
pub(super) fn delivery_from(
    args: &WebhookArgs,
    configured: Option<&WatchConfig>,
    secrets: &SecretStore,
) -> Result<Option<Delivery>> {
    let (planned, warnings) = plan(
        args,
        configured.and_then(|c| c.webhook.as_ref()),
        secrets.load_secret(Kind::WatchToken)?,
        secrets.load_secret(Kind::WatchSigningKey)?,
    )?;

    for warning in &warnings {
        ui::warn(warning);
    }

    let Some(planned) = planned else {
        return Ok(None);
    };
    webhook::check(&planned.webhook)?;

    Ok(Some(Delivery {
        // The address this run posts to, kept beside the client so the outbox
        // can be filtered by it: a queued report belongs to the address it was
        // addressed to, and `--webhook` must not flush a backlog somewhere else.
        destination: destination_of(&planned.webhook.url),
        signed: planned.webhook.key.is_some(),
        client: WebhookClient::new(planned.webhook)?,
        heartbeat: planned.heartbeat,
    }))
}

/// The address a report is addressed to, as the outbox records it.
///
/// **The whole URL, and deliberately not the origin `plan` compares.** Two
/// questions look alike here and want different units:
///
/// - *May this credential travel there?* The origin. A path that changed is the
///   same destination and the same credential, where a host that changed is
///   neither. That question is asked in [`plan`], against `Url::origin()`, and
///   it is right as it stands.
/// - *May this queued report go there?* The address it was addressed to, which
///   is a URL. This used to answer with the origin too, and an origin cannot
///   tell two workflows on one host apart — which is exactly the shape n8n
///   ships with: `https://n8n.local/webhook/snob` in the file and
///   `https://n8n.local/webhook-test/snob` typed to see what the payload looks
///   like. `plan` correctly attaches the token, because the origin really is
///   the same; then `drain` handed the production backlog to the test workflow
///   with that token on it and marked it delivered. Verbatim the failure
///   `deliveries::due` documents and `005_destination.sql` was written to
///   close, with the guard drawn one URL level too high.
pub(crate) fn destination_of(url: &Url) -> String {
    url.as_str().to_string()
}

/// Commits the report and gets it to the webhook, if there is one.
///
/// The order is the whole of it: the report is queued and the marks retire in
/// **one transaction**, and only then is anything sent. A send that fails
/// leaves a row for the next run to retry; a mark that moved without the row
/// would lose the change for good.
pub(super) async fn deliver(
    app: &mut crate::app::App,
    tick: &TickReport,
    delivery: Option<&Delivery>,
) -> Result<()> {
    let changes = tick.report.changes();

    // Silence when nothing happened, unless somebody asked to hear it anyway.
    // An automation where every message means something is the point of that;
    // a heartbeat is for the opposite case, where the absence of messages is
    // the signal and "quiet" has to be told from "stopped".
    let body = match delivery.and_then(|d| event_for(&changes, d.heartbeat)) {
        Some(event) => {
            let run_id = run_id(snob_core::store::now(), tick.report.account_pk);
            // Serialized once, here, and stored as the string that goes on the
            // wire. `serde_json` may render one value two ways, and the
            // signature covers bytes — so a retry that rendered it again could
            // be rejected after the first attempt was accepted.
            Some((
                run_id.clone(),
                serde_json::to_string(&payload(tick, &run_id, event))?,
            ))
        }
        None => None,
    };

    let queued = crate::engine::watch::commit(
        app,
        tick,
        // A report and the address it was made for are one thing: the body is
        // built only when there is a delivery to build it for, so the pair is
        // taken together rather than the address being fetched again and allowed
        // to come back empty.
        delivery
            .zip(body.as_ref())
            .map(|(to, (run_id, body))| Queued {
                run_id,
                body,
                destination: to.destination.as_str(),
            }),
    )?;

    let Some(delivery) = delivery else {
        return Ok(());
    };

    // This run's report, when it made one.
    if let (Some(id), Some((run_id, body))) = (queued, body.as_ref()) {
        send_one(app, delivery, id, run_id, body, 1).await;
    }

    // Whatever is still owed from earlier runs is NOT drained here. It is
    // drained once per run, after every watched account, by the caller. Doing
    // it here meant once per account: the backoff is a stored wall-clock
    // moment while a tick takes minutes, so an owed report burned most of its
    // eight attempts inside a single run, and one run could make
    // `accounts * DRAIN_LIMIT` requests at somebody's server at once.
    Ok(())
}

/// Sends one queued report and records what came of it.
/// `run_id` is what the receiver is told to deduplicate on, and it is **not**
/// the row id.
///
/// The row id is a SQLite rowid with no AUTOINCREMENT, so it is reused after
/// `prune` empties the table — which happens on any account quiet for longer
/// than `deliveries::KEEP_SETTLED_FOR_SECS`, the default case. A receiver doing
/// what AGENTS.md, the CHANGELOG and the tests tell it to do would then drop a
/// real report as a repeat. `run_id` is `UNIQUE` in the schema and already
/// inside the body, so it is the one value that means what the header claims.
async fn send_one(
    app: &crate::app::App,
    delivery: &Delivery,
    id: i64,
    run_id: &str,
    body: &str,
    attempt: i64,
) {
    let now = snob_core::store::now();
    let outcome = delivery
        .client
        .post(body, event_of(body), run_id, attempt)
        .await;

    // Failing to write down what happened is not worth failing the run over:
    // the report either arrived or it did not, and the row is still there.
    let recorded = match &outcome {
        Attempt::Delivered { status } => {
            deliveries::delivered(app.db().conn(), id, *status, now).map(|()| None)
        }
        Attempt::Failed { status, error } => {
            deliveries::failed(app.db().conn(), id, *status, error, false, now).map(Some)
        }
        // `permanent` is for a request that could not be sent at all, and
        // nothing else. Every HTTP answer is retried within the attempt and age
        // budget now, because a 4xx used to expire the row after **zero**
        // retries — and the mark had already moved, so the arrivals and
        // departures in that report were gone for good. Many 4xx are transient:
        // n8n answers 404 for a workflow that is not currently registered, a
        // reverse proxy answers 404 or 403 while it reloads, and an expired
        // bearer token answers 401. None of those is a reason to throw the only
        // copy of a change away, and the far end is the user's own server, so
        // knocking again costs nothing that matters.
        // No status: the request was never built, so no server answered
        // anything. It used to pass `Some(0)` -- a code nothing can send, into a
        // column whose comment is "the HTTP code, when there was one", where
        // NULL already meant exactly that.
        Attempt::Refused { error } => {
            deliveries::failed(app.db().conn(), id, None, error, true, now).map(Some)
        }
    };
    let settled = match recorded {
        Ok(settled) => settled,
        Err(e) => {
            ui::warn(&format!(
                "could not record what happened to the report: {e}"
            ));
            None
        }
    };

    // What `failed` decided, said out loud. Its answer used to be dropped and
    // the sentence written from the HTTP result instead, so the eighth failure
    // — the one that throws the report away — was announced as "it is queued
    // and will be tried again", and `status` then showed nothing owed.
    match (&outcome, settled) {
        (Attempt::Delivered { .. }, _) => {}
        (_, Some(deliveries::Outcome::Retrying(at))) => ui::warn(&format!(
            "the report could not be delivered ({}); it is queued and will be tried again {}",
            outcome.error(),
            describe_when(at, now)
        )),
        (_, Some(deliveries::Outcome::GaveUp(reason))) => ui::warn(&format!(
            "the report could not be delivered ({}). {} It will not be tried again, and what it \
             said is not reported a second time: the next run compares against what this one \
             already counted.",
            outcome.error(),
            match reason {
                deliveries::GaveUp::Refused => "The request could not be sent at all.",
                deliveries::GaveUp::OutOfAttempts => "Every attempt was refused.",
                deliveries::GaveUp::TooOld => "It is too old to be news now.",
            }
        )),
        // `failed` itself would not record. The row is untouched, so the next
        // run tries it again.
        (_, None) => ui::warn(&format!(
            "the report could not be delivered ({})",
            outcome.error()
        )),
    }
}

/// The far end's answer, whatever shape the attempt came back in.
/// "in 4m", for a moment in the near future.
fn describe_when(at: i64, now: i64) -> String {
    match at.checked_sub(now) {
        Some(seconds) if seconds > 0 => format!(
            "in {}",
            snob_core::duration::format(std::time::Duration::from_secs(seconds as u64))
        ),
        _ => "on the next run".to_string(),
    }
}

/// Whether this run has anything to say, and what it would be called.
///
/// One function rather than a guard and a name computed from the same fact ten
/// lines apart: `None` means the run is quiet and nothing is queued at all.
/// Collapsing the guard so every run speaks turns a `--every 30m` monitor from
/// a handful of messages a week into forty-eight a day, which is the opposite
/// of what "nothing is sent when nothing changed" promises -- and it is the
/// point of `--heartbeat` that the *absence* of a message means something.
fn event_for(changes: &Changes, heartbeat: bool) -> Option<&'static str> {
    if !changes.is_empty() {
        Some("watch.changes")
    } else if heartbeat {
        Some("watch.heartbeat")
    } else {
        None
    }
}

/// The event name a queued body carries.
///
/// Read back out of the body rather than remembered alongside it, so the header
/// and the body cannot disagree — which they did, the header saying
/// `watch.changes` over a heartbeat. A retry days later reads the same string
/// from the same bytes, so it stays true then too.
fn event_of(body: &str) -> &str {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("event")
                .and_then(|e| e.as_str())
                // The borrow has to outlive the parsed value, so the answer is
                // matched back to one of the names this tool emits rather than
                // returned from inside it. Anything else is a body this version
                // did not write.
                .map(|event| match event {
                    "watch.heartbeat" => "watch.heartbeat",
                    _ => "watch.changes",
                })
        })
        .unwrap_or("watch.changes")
}

/// Tries whatever is owed from earlier runs.
///
/// **Once per run, after every account**, not once per account. The backoff is
/// a stored wall-clock moment while a tick takes minutes, so draining inside
/// the per-account loop burned an owed report's whole retry ladder inside a
/// single run: six accounts was five of the eight attempts spent, nine was
/// `expired` before the run finished. It also made up to `accounts ×
/// DRAIN_LIMIT` POSTs at somebody's server in one go, which is the thing
/// `DRAIN_LIMIT` exists to bound.
///
/// **And it stops when the user does.** The caller checks the token before
/// entering, with a comment naming the cost, and then nothing looked at it
/// again: not this loop, not `send_one`, not `WebhookClient::post`, which holds
/// no token at all. So a Ctrl+C during the second of ten POSTs at a receiver
/// that accepts and stalls bought several more minutes of a run the terminal
/// had already said was stopping, and the only way out was a second Ctrl+C,
/// which is `exit(130)`. Nothing is gained by finishing: the rows stay pending
/// and the next run drains them, which is the whole point of the queue.
pub(super) async fn drain(app: &crate::app::App, delivery: &Delivery) {
    let now = snob_core::store::now();
    let owed = match deliveries::due(app.db().conn(), now, DRAIN_LIMIT, &delivery.destination) {
        Ok(owed) => owed,
        Err(e) => {
            ui::warn(&format!("could not read the queue of owed reports: {e}"));
            return;
        }
    };

    for report in owed {
        // Asked before each one rather than only before the first. A POST has
        // its own timeout, so the token can be set at any point in this loop,
        // and the answer has to be read where the next request would go out.
        if app.cancel().is_canceled() {
            return;
        }
        send_one(
            app,
            delivery,
            report.id,
            &report.run_id,
            &report.body,
            report.attempts + 1,
        )
        .await;
    }
}

/// How many owed reports one run will try before leaving the rest.
///
/// Bounded so a queue that built up over a weekend does not turn one run into a
/// hundred requests at somebody's server all at once. The rest go on the next
/// run, and `deliveries::MAX_AGE_SECS` is what stops them lingering forever.
///
/// It is also the size of the interruption `drain` has to be able to stop in
/// the middle of: ten POSTs at thirty seconds each is five minutes of a run the
/// terminal has already said is stopping.
const DRAIN_LIMIT: usize = 10;

/// An id for this report, unique enough for a receiver to deduplicate on.
///
/// The moment and a random suffix rather than a UUID: the column is `UNIQUE`,
/// so a collision is an error rather than a silent overwrite, and this avoids a
/// dependency for a value nothing derives meaning from.
/// `now` is an argument rather than read inside, which is this project's shape
/// for anything with arithmetic in it -- and here it is also what lets the
/// uniqueness be tested without building a whole tick.
pub(super) fn run_id(now: i64, account_pk: Pk) -> String {
    format!("{}-{:08x}", now, fastrand::u32(..) ^ (account_pk as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::watch::tests::fixtures::{app_posting_to, list, report_with, user};

    /// Two workflows on one host are two addresses.
    ///
    /// The store's own filter was always an exact match, so the test there
    /// passes either way — it enqueues the value it then asks for. The unit was
    /// decided here, and an origin cannot tell `/webhook/snob` from
    /// `/webhook-test/snob`. That pair is not a corner case: it is how n8n
    /// publishes a workflow and its test run.
    #[test]
    fn the_outbox_records_the_address_not_the_origin() {
        let published = Url::parse("https://n8n.local/webhook/snob").unwrap();
        let test_run = Url::parse("https://n8n.local/webhook-test/snob").unwrap();

        assert_ne!(
            destination_of(&published),
            destination_of(&test_run),
            "the production backlog would drain into the test workflow"
        );
        assert_eq!(destination_of(&published), "https://n8n.local/webhook/snob");

        // And the other question, which is a different one: for a credential the
        // same host *is* the same destination, and `plan` still asks it that way.
        assert_eq!(
            published.origin(),
            test_run.origin(),
            "the credential gate is not what changed"
        );
    }

    /// Nothing is sent when nothing changed, and `--heartbeat` is what asks
    /// for the opposite.
    ///
    /// The guard and the name used to be computed from the same fact ten lines
    /// apart, and collapsing the guard so every run speaks was invisible: a
    /// `--every 30m` monitor would go from a handful of messages a week to
    /// forty-eight a day, and the automation reading silence as the signal
    /// would never hear it again.
    #[test]
    fn a_quiet_run_says_nothing_unless_a_heartbeat_was_asked_for() {
        let quiet = report_with(None, vec![]).changes();
        let moved = report_with(
            Some(list(
                Basis::Compare {
                    before: 1,
                    after: 2,
                },
                ListDiff {
                    gained: vec![user(7, "newcomer")],
                    lost: vec![],
                },
                Some(1_000),
            )),
            vec![],
        )
        .changes();

        assert_eq!(event_for(&quiet, false), None, "silence means something");
        assert_eq!(event_for(&quiet, true), Some("watch.heartbeat"));
        assert_eq!(event_for(&moved, false), Some("watch.changes"));
        assert_eq!(
            event_for(&moved, true),
            Some("watch.changes"),
            "a run with news is news, not a heartbeat"
        );
    }

    /// The id a receiver deduplicates on is unique, and the column is `UNIQUE`
    /// so a repeat is an error rather than a silent overwrite.
    ///
    /// Reducing it to the second alone survived every test: two accounts
    /// reported in the same second, or one account twice, then collide -- and
    /// the insert fails inside the transaction that queues the report, so the
    /// report **and** the marks roll back together.
    #[test]
    fn two_reports_never_share_an_id() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000 {
            assert!(
                seen.insert(run_id(1_700, 42)),
                "the same second and the same account produced one id twice"
            );
        }
        // And two accounts in one second, which is one run of a monitor
        // watching more than one.
        assert_ne!(run_id(1_700, 42), run_id(1_700, 43));
    }

    async fn accepting(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/hook"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(server)
            .await;
    }

    fn owe(app: &crate::app::App, delivery: &Delivery, how_many: usize, at: i64) {
        for n in 0..how_many {
            deliveries::enqueue(
                app.db().conn(),
                &format!("run-{n}"),
                42,
                r#"{"schema":1,"event":"watch.changes"}"#,
                at,
                Some(&delivery.destination),
            )
            .unwrap();
        }
    }

    /// A report owed from an earlier run goes out on a later one.
    ///
    /// The whole reason the outbox exists, and deleting the drain entirely --
    /// at both call sites -- left the suite green. The integration test that
    /// looks like it covers this reimplements the loop by hand, so it proves
    /// the store functions work and nothing about the loop production runs.
    #[tokio::test]
    async fn a_report_owed_from_an_earlier_run_goes_out_on_a_later_one() {
        let server = wiremock::MockServer::start().await;
        accepting(&server).await;
        let (app, delivery) = app_posting_to(&server);
        let now = snob_core::store::now();
        owe(&app, &delivery, 2, now);

        drain(&app, &delivery).await;

        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert_eq!(deliveries::pending(app.db().conn()).unwrap(), 0);
    }

    /// And one run does not empty a weekend's worth of queue at somebody's
    /// server all at once.
    ///
    /// `DRAIN_LIMIT` is the only thing bounding that, and raising it from ten to
    /// a thousand was invisible.
    #[tokio::test]
    async fn one_run_sends_at_most_the_drain_limit() {
        let server = wiremock::MockServer::start().await;
        accepting(&server).await;
        let (app, delivery) = app_posting_to(&server);
        let now = snob_core::store::now();
        // A literal count and a literal ceiling, not `DRAIN_LIMIT + 5` and
        // `DRAIN_LIMIT`: a test written in terms of the constant it is checking
        // passes whatever that constant becomes, which is exactly how this
        // bound came to be unguarded in the first place.
        let owed = 25;
        owe(&app, &delivery, owed, now);

        drain(&app, &delivery).await;

        let sent = server.received_requests().await.unwrap().len();
        assert!(
            sent <= 15,
            "one run made {sent} requests at somebody's server in a row"
        );
        assert_eq!(
            deliveries::pending(app.db().conn()).unwrap() as usize,
            owed - sent,
            "the rest are still owed, for the next run"
        );
    }

    /// A request that was never built records no HTTP code at all.
    ///
    /// `Attempt::Refused` carried `status: 0` and `send_one` stored it, so
    /// `watch_deliveries.last_status` -- "the HTTP code, when there was one" --
    /// held a code no server can answer with, for a request no server ever saw.
    /// NULL is what the column already had for that, and every other reader had
    /// to know the convention to avoid reporting the zero as a code.
    #[tokio::test]
    async fn a_request_that_was_never_built_records_no_status() {
        let server = wiremock::MockServer::start().await;
        let (app, _) = app_posting_to(&server);

        // A header `webhook::check` would have refused, which is the only way
        // this state is reached: a file hand-edited past the preflight.
        let url = Url::parse(&format!("{}/hook", server.uri())).unwrap();
        let delivery = Delivery {
            destination: destination_of(&url),
            signed: false,
            client: WebhookClient::new(Webhook {
                url,
                headers: vec![("Not a header".into(), "homelab".into())],
                key: None,
            })
            .unwrap(),
            heartbeat: false,
        };

        let id = deliveries::enqueue(
            app.db().conn(),
            "run-1",
            42,
            "{}",
            snob_core::store::now(),
            Some(&delivery.destination),
        )
        .unwrap();
        send_one(&app, &delivery, id, "run-1", "{}", 1).await;

        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "nothing was sent, so nothing answered"
        );
        let status: Option<i64> = app
            .db()
            .conn()
            .query_row("SELECT last_status FROM watch_deliveries", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, None, "a request nobody saw has no HTTP code");
    }

    /// A run the user stopped does not keep posting what it owes.
    ///
    /// `run_accounts` reads the token before entering the drain, with a comment
    /// naming exactly this cost -- and then nothing read it again: not the loop,
    /// not `send_one`, not `WebhookClient::post`, which holds no token. Ten owed
    /// rows coming due together at a receiver that accepts and stalls is several
    /// more minutes of a run the terminal has already said is stopping, and the
    /// only escape is a second Ctrl+C, which is `exit(130)`.
    ///
    /// Nothing is lost by stopping, which is what the second assertion is for:
    /// the rows are still owed afterwards and the next run takes them.
    /// `deliver`'s own `send_one` is deliberately not covered by this -- that one
    /// sends the report whose mark `commit` has already moved, so it has to go
    /// out whatever the user typed.
    #[tokio::test]
    async fn a_canceled_run_stops_draining_the_queue() {
        let server = wiremock::MockServer::start().await;
        accepting(&server).await;
        let (app, delivery) = app_posting_to(&server);
        let owed = 5;
        owe(&app, &delivery, owed, snob_core::store::now());

        app.cancel().cancel();
        drain(&app, &delivery).await;

        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "the run was stopped before the drain began"
        );
        assert_eq!(
            deliveries::pending(app.db().conn()).unwrap() as usize,
            owed,
            "what is owed stays owed, for the next run"
        );
    }

    /// A run drains the queue even when it had no news of its own.
    ///
    /// AGENTS.md's rule is that owed reports are retried by **any** run, and the
    /// account loop is not what decides it: a monitor whose counters have not
    /// moved is the common case, and it is exactly the run that used to leave a
    /// backlog untouched. Deleting the drain from `run_one` left the whole
    /// suite green, because the only tests that reached it called `drain`
    /// directly -- they proved the function works and nothing about anybody
    /// calling it.
    ///
    /// No account is watched here on purpose. That removes the network from the
    /// test entirely and asks the one question that is open: does a run that
    /// looked at nothing still send what it owes?
    #[tokio::test]
    async fn a_run_with_nothing_to_look_at_still_sends_what_it_owes() {
        let server = wiremock::MockServer::start().await;
        accepting(&server).await;
        let (mut app, delivery) = app_posting_to(&server);
        owe(&app, &delivery, 1, snob_core::store::now());

        let args = WatchRunArgs {
            no_progress: true,
            ..WatchRunArgs::default()
        };
        run_one(&args, &mut app, &[], Some(&delivery))
            .await
            .unwrap();

        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a run that had nothing to report still owes what it owed"
        );
        assert_eq!(deliveries::pending(app.db().conn()).unwrap(), 0);
    }

    /// The event a queued body carries is read back out of the body.
    ///
    /// It has to be, because a retry days later has only the bytes: remembering
    /// the name alongside them let the two disagree, and the header said
    /// `watch.changes` over every heartbeat -- so exactly the receiver the
    /// header exists for treated each one as a report of changes.
    ///
    /// Nothing tested this. The test that looks like it does hands the name to
    /// the client as an argument, so it pins the client and not the reading.
    #[test]
    fn the_event_is_read_back_out_of_the_body_it_describes() {
        assert_eq!(
            event_of(r#"{"schema":1,"event":"watch.heartbeat"}"#),
            "watch.heartbeat"
        );
        assert_eq!(
            event_of(r#"{"schema":1,"event":"watch.changes"}"#),
            "watch.changes"
        );

        // Anything this version did not write reads as the ordinary case rather
        // than as a heartbeat: a receiver that drops heartbeats must not be
        // handed a report of changes wearing one's name.
        assert_eq!(event_of(r#"{"event":"something.else"}"#), "watch.changes");
        assert_eq!(event_of("not json at all"), "watch.changes");
    }

    /// A `[webhook]` section as `watch.toml` would parse it.
    fn configured(url: &str, headers: &[(&str, &str)]) -> WebhookConfig {
        WebhookConfig {
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            heartbeat: false,
        }
    }

    /// Every value the planned request would send under this name.
    ///
    /// A `Vec`, not an `Option`: "the header went out twice" and "the header
    /// went out once" are different answers, and one of the defects this file
    /// has already had was exactly that.
    fn sent_as<'a>(planned: &'a Planned, name: &str) -> Vec<&'a str> {
        planned
            .webhook
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// A credential set up for one address does not follow `--webhook` to
    /// another.
    ///
    /// A keyring that cannot be read is not a keyring with nothing in it.
    ///
    /// Both warning arms below sit inside a `Some`, so two absent secrets used
    /// to produce **no** warning and a client with neither an `Authorization`
    /// nor a signing key: the report went out naming the user's followers,
    /// unauthenticated and unsigned, with the only trace a `debug!` line nobody
    /// sees at the default level. `WatchConfig` records nothing about what
    /// `setup` stored, so no later run could notice either.
    ///
    /// The reachable box is the one where the session is in the file fallback
    /// and the keyring is not there: `snob login --no-keyring`, then `setup` —
    /// whose `save_secret` is keyring-only whatever `--no-keyring` said — then
    /// cron with no session bus.
    #[test]
    fn a_keyring_that_cannot_be_read_is_said_out_loud() {
        let file = configured("https://n8n.local/webhook/snob", &[]);
        let args = WebhookArgs::default();

        let (planned, warnings) =
            plan(&args, Some(&file), Stored::Unreachable, Stored::Unreachable).unwrap();
        let planned = planned.expect("the file names an address");

        assert_eq!(sent_as(&planned, "Authorization"), Vec::<&str>::new());
        assert!(planned.webhook.key.is_none());
        assert_eq!(
            warnings.len(),
            1,
            "going out unauthenticated and unsigned is not something to do quietly: {warnings:?}"
        );
        assert!(warnings[0].contains("keyring"), "{warnings:?}");

        // And the answer that really does mean "nothing was stored" still says
        // nothing, because there is nothing to say.
        let (_, quiet) = plan(&args, Some(&file), Stored::Nothing, Stored::Nothing).unwrap();
        assert!(quiet.is_empty(), "{quiet:?}");
    }

    /// Pointing a run at a request bin to see what the payload looks like is the
    /// first thing anybody does, and it used to send the team's `X-Api-Key` and
    /// the bearer token `snob watch setup` had stored to that bin. `check` was
    /// happy, because the new address was https.
    ///
    /// Replacing the origin comparison with `true` left all 656 tests green
    /// before this existed.
    #[test]
    fn a_token_stored_for_one_host_is_not_sent_to_another() {
        let file = configured(
            "https://n8n.internal/webhook/snob",
            &[("X-Api-Key", "team")],
        );
        let args = WebhookArgs {
            webhook: Some("https://bin.example/inspect".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(
            &args,
            Some(&file),
            Stored::Found(Secret::from("Bearer stored".to_string())),
            Stored::Nothing,
        )
        .unwrap();
        let planned = planned.expect("there is an address to post to");

        assert_eq!(sent_as(&planned, "Authorization"), Vec::<&str>::new());
        assert_eq!(sent_as(&planned, "X-Api-Key"), Vec::<&str>::new());
        assert_eq!(
            warnings.len(),
            2,
            "both the headers and the token were withheld, so both are said: {warnings:?}"
        );
        assert!(warnings[0].contains("not sent with it"), "{warnings:?}");
        assert!(warnings[1].contains("stored token"), "{warnings:?}");
    }

    /// A stored token with no configured address is not sent to a typed one.
    ///
    /// The comparison used to read an absent configured origin as "the same
    /// destination", so this — a keyring entry with no `[webhook]` beside it —
    /// went straight through the guard. It is reachable rather than theoretical:
    /// `setup` stored the secrets before it wrote the file, so a write that
    /// failed left exactly this state, and the file is documented as safe to
    /// hand-edit.
    #[test]
    fn a_stored_token_with_nothing_configured_is_not_sent_anywhere_typed() {
        let args = WebhookArgs {
            webhook: Some("https://bin.example/inspect".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(
            &args,
            None,
            Stored::Found(Secret::from("Bearer stored".to_string())),
            Stored::Nothing,
        )
        .unwrap();
        let planned = planned.expect("there is an address to post to");

        assert_eq!(sent_as(&planned, "Authorization"), Vec::<&str>::new());
        assert!(
            warnings.iter().any(|w| w.contains("stored token")),
            "withholding it silently would read as never having stored one: {warnings:?}"
        );
    }

    /// A configured address that does not parse is not an address to match
    /// against, so nothing stored for it travels.
    #[test]
    fn a_configured_address_that_does_not_parse_matches_nothing() {
        let file = configured("n8n.internal/webhook/snob", &[("X-Api-Key", "team")]);
        let args = WebhookArgs {
            webhook: Some("https://bin.example/inspect".into()),
            ..Default::default()
        };

        let (planned, _) = plan(
            &args,
            Some(&file),
            Stored::Found(Secret::from("Bearer stored".to_string())),
            Stored::Found(Secret::from("k".to_string())),
        )
        .unwrap();
        let planned = planned.expect("there is an address to post to");

        assert_eq!(sent_as(&planned, "Authorization"), Vec::<&str>::new());
        assert_eq!(sent_as(&planned, "X-Api-Key"), Vec::<&str>::new());
        assert!(planned.webhook.key.is_none());
    }

    /// The signing key is a credential, and goes through the same gate.
    ///
    /// It had no gate at all: the bearer token was correctly withheld from a
    /// foreign address *and the body was signed with the stored key anyway*, so
    /// the warning misdescribed what happened and the third party received a
    /// body and its MAC — everything needed to guess a human-chosen shared
    /// secret offline. Every other `plan` test passes `None` for the key, so
    /// that arm had no coverage whatsoever.
    #[test]
    fn a_key_stored_for_one_host_does_not_sign_a_report_to_another() {
        let file = configured("https://n8n.internal/webhook/snob", &[]);
        let args = WebhookArgs {
            webhook: Some("https://bin.example/inspect".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(
            &args,
            Some(&file),
            Stored::Nothing,
            Stored::Found(Secret::from("shared-secret".to_string())),
        )
        .unwrap();
        let planned = planned.expect("there is an address to post to");

        assert!(
            planned.webhook.key.is_none(),
            "a foreign host must not be handed a body and its MAC"
        );
        assert!(
            warnings.iter().any(|w| w.contains("signing key")),
            "an unsigned report has to be announced, or the receiver's check just starts \
             failing: {warnings:?}"
        );
    }

    /// And it does sign for the address it was stored for, whatever the path.
    #[test]
    fn the_stored_key_signs_a_report_to_the_address_it_was_stored_for() {
        let file = configured("https://n8n.internal/webhook/snob", &[]);
        let args = WebhookArgs {
            webhook: Some("https://n8n.internal/webhook/other".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(
            &args,
            Some(&file),
            Stored::Nothing,
            Stored::Found(Secret::from("shared-secret".to_string())),
        )
        .unwrap();
        let planned = planned.expect("there is an address to post to");

        assert!(planned.webhook.key.is_some());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Nothing is withheld when nothing overrode the file: the address the
    /// report goes to is the one the credentials were stored for.
    #[test]
    fn the_configured_address_gets_everything_configured_for_it() {
        let file = configured(
            "https://n8n.internal/webhook/snob",
            &[("X-Api-Key", "team")],
        );

        let (planned, warnings) = plan(
            &WebhookArgs::default(),
            Some(&file),
            Stored::Found(Secret::from("Bearer stored".to_string())),
            Stored::Found(Secret::from("shared-secret".to_string())),
        )
        .unwrap();
        let planned = planned.expect("the file names an address");

        assert_eq!(sent_as(&planned, "Authorization"), vec!["Bearer stored"]);
        assert_eq!(sent_as(&planned, "X-Api-Key"), vec!["team"]);
        assert!(planned.webhook.key.is_some());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// The same address is the same destination, however the URL was written.
    #[test]
    fn the_stored_token_is_sent_to_the_address_it_was_stored_for() {
        let file = configured("https://n8n.internal/webhook/snob", &[]);
        // A different path on the same host: still where the token belongs.
        let args = WebhookArgs {
            webhook: Some("https://n8n.internal/webhook/other".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(
            &args,
            Some(&file),
            Stored::Found(Secret::from("Bearer stored".to_string())),
            Stored::Nothing,
        )
        .unwrap();

        assert_eq!(
            sent_as(&planned.unwrap(), "Authorization"),
            ["Bearer stored"]
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// A configured `Authorization` stops the stored one being added, so the
    /// two never both go out.
    ///
    /// The guard reads the **merged** list rather than the flags: looking only
    /// at `--header` meant a configured one and the keyring's one both
    /// traveled, and the second was the secret `setup` had put away.
    #[test]
    fn a_configured_authorization_stops_the_stored_token_being_added() {
        let file = configured(
            "https://n8n.internal/webhook/snob",
            &[("Authorization", "Bearer configured")],
        );

        let (planned, _) = plan(
            &WebhookArgs::default(),
            Some(&file),
            Stored::Found(Secret::from("Bearer stored".to_string())),
            Stored::Nothing,
        )
        .unwrap();

        assert_eq!(
            sent_as(&planned.unwrap(), "Authorization"),
            ["Bearer configured"],
            "the stored token was added on top of one that was already there"
        );
    }

    /// An empty `--sign-with` is an unset environment variable, not a request
    /// to sign with nothing.
    ///
    /// Taken literally it signs with a zero-length key -- a well-formed
    /// signature anybody can forge -- and, because the flag wins over the
    /// keyring, it would silently replace a key that was configured correctly.
    #[test]
    fn an_empty_sign_with_is_refused_rather_than_signing_with_nothing() {
        for given in ["", "   "] {
            let args = WebhookArgs {
                webhook: Some("https://n8n.internal/hook".into()),
                sign_with: Some(given.to_string()),
                ..Default::default()
            };
            // Mapped away rather than unwrapped: the error carries a `Planned`,
            // which holds the merged headers, and those hold the token in the
            // clear.
            let refused = plan(&args, None, Stored::Nothing, Stored::Nothing)
                .map(|_| ())
                .unwrap_err();
            assert!(refused.to_string().contains("empty value"), "{refused}");
        }
    }

    /// A typed header goes out after a configured one of the same name, which
    /// is what lets the flag override the file: the last one wins at the
    /// request.
    #[test]
    fn a_typed_header_comes_after_the_one_from_the_file() {
        let file = configured("https://n8n.internal/hook", &[("X-Source", "file")]);
        let args = WebhookArgs {
            header: vec!["X-Source: typed".into()],
            ..Default::default()
        };

        let (planned, _) = plan(&args, Some(&file), Stored::Nothing, Stored::Nothing).unwrap();

        assert_eq!(
            sent_as(&planned.unwrap(), "X-Source"),
            ["file", "typed"],
            "order is the override: `post` inserts them in turn"
        );
    }

    /// Options that need a webhook say so when there is none, rather than doing
    /// nothing quietly.
    #[test]
    fn asking_for_a_signature_with_nowhere_to_send_it_is_said_out_loud() {
        let args = WebhookArgs {
            sign_with: Some("a secret".into()),
            ..Default::default()
        };

        let (planned, warnings) = plan(&args, None, Stored::Nothing, Stored::Nothing).unwrap();

        assert!(planned.is_none());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no webhook"), "{warnings:?}");
    }
}
