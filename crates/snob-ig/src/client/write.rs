//! The follow and the unfollow, and everything only a write needs.
//!
//! **One file, so that the rule can be checked by opening it.** `AGENTS.md`
//! says [`IgClient::post`] is the only function in this workspace that sends a
//! method other than GET; this is the only file that calls it, and the two
//! mutations below are the only things that reach it.
//!
//! What can be sent is a [`graphql::Mutation`] rather than a path, so the set
//! of writes this program can make is the set of variants that enum has --
//! which is what makes a third one a compile error rather than a string
//! somebody typed.

use serde::de::DeserializeOwned;
use snob_core::Pk;

use crate::error::IgError;
use crate::graphql;
use crate::model::{FriendshipResult, FriendshipStatus};

use super::IgClient;
use super::headers::Surface;
use super::transport::{Answer, MAX_BODY_BYTES, load_of, read_capped, retry_after};

/// Whether a refused mutation is worth spending a discovery walk on.
///
/// **Narrow on purpose.** A rotated identifier and a throttled account both
/// come back as a refusal, and walking megabytes of JavaScript at an account
/// Instagram has just said no to is the opposite of what the pacing rules are
/// for. So everything that means "stop asking" is excluded: a push-back, an
/// action block, a challenge, a dead session, a cancellation. What is left is
/// the shapes a bad `doc_id` actually takes — a 400 with a body that did not
/// parse, or one that did and said nothing useful.
/// **`NotFound` is in here and is not in `worth_a_second_route`**, twenty lines
/// away, and the two are not contradicting each other. There, a 404 means the
/// account does not exist and a second lookup would be a second request spent
/// confirming it. Here, a 404 is one of the ways Instagram refuses an operation
/// it no longer serves under that identifier, and the discovery walk costs it
/// nothing: it reads the CDN, not the API, so a wrong guess is bytes rather
/// than a request against the account's budget.
///
/// **And narrow in a second direction, which is the one that matters for a
/// write.** The walk ends in a second `mutate`, and a second mutation is only
/// safe when the first is known not to have happened. A 4xx and a 404 say
/// so; a 200 carrying `errors` says so -- that is a GraphQL refusal, and the
/// shape a stale identifier actually arrives in. A 5xx does not: an edge
/// that answers 502 may have forwarded the request before it failed. Nor
/// does the redirect `post` reports as an `Unexpected` 3xx, nor a 200 whose
/// body would not decode, which is a write that very probably happened and
/// an answer this program could not read. Any of those used to pass, and
/// the recovery then sent the follow again on a budget paid once and a
/// confirmation given once -- the replay the redirect policy exists to
/// prevent, arriving by another road.
fn worth_rediscovering(error: &IgError) -> bool {
    match error {
        IgError::NotFound { .. } => true,
        IgError::Unexpected { status, .. } => {
            (200..300).contains(status) || (400..500).contains(status)
        }
        _ => false,
    }
}

impl IgClient {
    /// **The only function in this workspace that sends anything other than a
    /// GET to Instagram.** Everything the write rule in `AGENTS.md` promises is
    /// enforced on the way through here.
    ///
    /// Four things it does that [`IgClient::get`] does not, each of them there
    /// because a write is not a read:
    ///
    /// - **It pays the write budget**, not the read one, and like `get` it pays
    ///   before it sends. There is no argument that selects between the two:
    ///   `get` calls `clear_to_send` and this calls `clear_to_send_write`, so
    ///   the choice is made by which function the caller reached rather than by
    ///   a value it passed.
    /// - **It refuses without a CSRF token instead of finding out.** Instagram
    ///   would answer 403, and that 403 would cost a request, a slot of write
    ///   budget and — because `classify` reads a 403 as a dead session — a
    ///   message telling the user to log in again when their session is fine.
    ///   A `snob login --paste` session has no `csrftoken` unless one was given,
    ///   so this is the common case rather than the odd one.
    /// - **It sends `Origin`**, which the Fetch standard requires on a POST even
    ///   when the request is same-origin. See the comment in
    ///   [`IgClient::dressed`] for the half of that rule which lives on
    ///   the read side.
    /// - **It follows no redirect at all**, through [`IgClient::writer`].
    /// - **It is not abandoned once it is in flight**, and that is the one
    ///   place this program does not do what `AGENTS.md` says about Ctrl+C.
    ///   Every read races the cancel token against the socket, because giving
    ///   up on a read costs nothing: the answer was going to be thrown away.
    ///   Giving up on a write costs the one thing worth having, which is
    ///   knowing whether it happened — the request has already gone, Instagram
    ///   may well act on it, and reporting "canceled" to somebody who is now
    ///   following an account is worse than making them wait out the timeout.
    ///
    ///   The boundary is exact rather than convenient: `clear_to_send_write`
    ///   reads the token before it reserves and again inside the wait, so a
    ///   write is cancelable up to the moment it is sent and not after it. The
    ///   uncancelable part is the part that must not be abandoned.
    /// - **It cannot be pointed anywhere.** It takes a
    ///   [`graphql::Mutation`], not a path, so the set of things this program
    ///   can write is the set of variants that enum has. It used to take a
    ///   `&str`, and then "there is no third write" rested on nobody typing a
    ///   third string -- which a source-reading guard cannot check, because the
    ///   identifier Instagram acts on is a `doc_id` and no list of words
    ///   contains a number.
    ///
    /// The two doc links below name [`IgClient::dressed`], which is where the
    /// shared header list actually lives; they said `browser_headers` for a
    /// while after it was renamed.
    ///
    /// `referer` is the profile page the button would have been clicked on, in
    /// the same spelling `get` wants: a path with no leading slash.
    async fn post<T: DeserializeOwned>(
        &self,
        write: graphql::Mutation,
        form: &[(&str, &str)],
        referer: &str,
        style: Surface<'_>,
    ) -> Result<T, IgError> {
        // Before the budget is charged, so that a session which cannot write
        // does not spend a slot discovering it. The token itself is put on the
        // request by `dressed`, which adds it whenever the session has
        // one; this guard is what makes "whenever" mean "always" on this path.
        if self.session.csrftoken.is_none() {
            return Err(IgError::NoCsrfToken);
        }

        let url = self.base.join(write.path())?;
        tracing::debug!(%url, operation = write.friendly_name(), "POST");

        self.pacer.clear_to_send_write().await?;

        let request = self
            .dressed(self.writer()?.post(url), referer, style)
            .header("Origin", self.base.as_str().trim_end_matches('/'))
            .form(form);

        let response = request.send().await?;
        self.remember_claim(&response);

        let status = response.status();
        // Read before the body, for the same reason `get_body` reads it there:
        // the header is on the response, and a body that will not read must not
        // take the one measurement worth having away with it.
        let retry_after = retry_after(&response);
        let load = load_of(&response);

        // A redirect reaches here as a status rather than as a new request,
        // because the policy is `none()`. It is not success and it is not
        // something to replay, so it is reported as what it is.
        if status.is_redirection() {
            return Err(IgError::Unexpected {
                status: status.as_u16(),
                body: "Instagram redirected a write, which is never followed".into(),
            });
        }

        let body = match read_capped(response, MAX_BODY_BYTES).await {
            Ok(body) => body,
            Err(_) if !status.is_success() => String::new(),
            Err(e) => return Err(e),
        };

        self.decode(&Answer {
            status: status.as_u16(),
            body,
            retry_after,
            load,
        })
    }

    /// Follows an account. **A write.** See [`IgClient::post`].
    pub async fn follow(
        &self,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        self.friendship(graphql::Mutation::Follow, pk, username, ids)
            .await
    }

    /// Unfollows an account. **A write.** See [`IgClient::post`].
    pub async fn unfollow(
        &self,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        self.friendship(graphql::Mutation::Unfollow, pk, username, ids)
            .await
    }

    /// The `doc_id` of a mutation, out of the cache or out of the page's own
    /// JavaScript.
    ///
    /// The walk goes through the **CDN client**, which is the right one twice
    /// over: those bundles really are on the CDN, and that client carries no
    /// cookie and no app id -- a public script has no business being fetched
    /// with a session attached. It also means they are not charged against
    /// Instagram's request budget, for the reason [`IgClient::download`]
    /// already gives about pictures.
    ///
    /// `html` is the page already fetched for the tokens, so the list of
    /// bundles costs nothing extra, and its order is the page's own -- which
    /// puts the chunk its route needs near the front.
    async fn doc_id_for(
        &self,
        mutation: graphql::Mutation,
        html: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<String, IgError> {
        /// One bundle. Instagram's largest is around five megabytes; twelve is
        /// a ceiling rather than a target, and it is here for the reason every
        /// other ceiling in this file is.
        const MAX_BUNDLE_BYTES: usize = 12 * 1024 * 1024;
        /// How many to open before giving up.
        ///
        /// **A page names around four hundred and forty of these**, so this is
        /// a real bound rather than a formality: the walk is bytes off the CDN
        /// and minutes of wall clock, and it happens once per rotation because
        /// the answer is cached. Sixty is roughly seven megabytes, measured.
        /// Past that the answer is more likely to be that the shape changed
        /// than that the right chunk is one further along.
        const MOST_BUNDLES: usize = 60;

        let name = mutation.friendly_name();

        for url in graphql::bundles_in(html).into_iter().take(MOST_BUNDLES) {
            let bytes = match self.download_capped(&url, MAX_BUNDLE_BYTES).await {
                Ok(bytes) => bytes,
                // The user stopping it is not a bundle that would not come
                // down. Read as one, an interrupt here churned through the
                // remaining sixty and then reported Instagram's original
                // refusal, with its exit code, for what was a Ctrl+C.
                Err(IgError::Canceled) => return Err(IgError::Canceled),
                // A bundle that will not come down is not the end of the
                // search: there are others, and the next may hold it.
                Err(_) => continue,
            };
            if let Some(id) = graphql::doc_id_in(&String::from_utf8_lossy(&bytes), name) {
                tracing::debug!(name, %url, "found the mutation id");
                ids.put(name, &id);
                return Ok(id);
            }
        }

        Err(IgError::MutationNotFound { name })
    }

    /// The body of both, because the two differ by one word.
    ///
    /// # Which request, and why it took three attempts to find out
    ///
    /// `POST /api/graphql`, naming the mutation. Settled by capturing the real
    /// web client in August 2026 -- see [`crate::graphql`], which carries the
    /// finding and the two routes that were sent live first and changed
    /// nothing.
    ///
    /// Two requests, and both are paid for: the page that hands out the tokens,
    /// and the mutation. The page is the expensive one at around six hundred
    /// kilobytes, and it is why there is no bulk mode to be tempted by even if
    /// the rule allowed one.
    ///
    /// The profile fetched is **the target's**, which is the page a browser
    /// would have been on when the button was pressed. Any logged-in page would
    /// hand out the same tokens, so this is coherence rather than necessity --
    /// but a request whose `Referer` names a page nobody loaded is the kind of
    /// small incoherence the rest of this module exists to avoid.
    async fn friendship(
        &self,
        mutation: graphql::Mutation,
        pk: Pk,
        username: &str,
        ids: &dyn graphql::DocIds,
    ) -> Result<FriendshipStatus, IgError> {
        // **Before the page.** `post` refuses without a CSRF token too, but by
        // then the expensive half has been spent: a profile page is around six
        // hundred kilobytes and a paid-for request. A session that cannot write
        // is the common case — every `snob login --paste` without `--csrftoken`
        // produces one — so this is the ordinary path, not the odd one.
        if self.session.csrftoken.is_none() {
            return Err(IgError::NoCsrfToken);
        }

        let referer = if username.is_empty() {
            String::new()
        } else {
            format!("{}/", snob_core::model::in_a_path(username))
        };

        let html = self.page(&format!("/{referer}")).await?;
        // A logged-out page renders too, and it has no tokens in it. Saying so
        // here is better than sending a mutation that cannot be authorized and
        // reading whatever Instagram says about it.
        let tokens = graphql::extract_tokens(&html).ok_or(IgError::SessionExpired)?;

        // What is known, and otherwise what was captured.
        // [`graphql::Mutation::seed_doc_id`] explains why there is a captured
        // value at all, and why discovery is the recovery rather than the way in.
        let name = mutation.friendly_name();
        let doc_id = ids
            .get(name)
            .unwrap_or_else(|| mutation.seed_doc_id().to_string());

        match self.mutate(mutation, &tokens, &doc_id, pk, &referer).await {
            Ok(status) => Ok(status),
            // **The recovery path.** A rotated identifier is refused, and the
            // refusal looks like several other things — so rather than trying
            // to read Instagram's mind, the walk runs and is worth something
            // only if it comes back with a *different* answer. It costs
            // megabytes off the CDN, which is why it is here and not on the way
            // in, and the original error is what the user hears if it fails.
            Err(first) if worth_rediscovering(&first) => {
                let Ok(found) = self.doc_id_for(mutation, &html, ids).await else {
                    return Err(first);
                };
                if found == doc_id {
                    return Err(first);
                }
                tracing::debug!(name, "the stored identifier was stale; trying the new one");
                self.mutate(mutation, &tokens, &found, pk, &referer).await
            }
            Err(other) => Err(other),
        }
    }

    /// One attempt at the mutation.
    async fn mutate(
        &self,
        mutation: graphql::Mutation,
        tokens: &graphql::PageTokens,
        doc_id: &str,
        pk: Pk,
        referer: &str,
    ) -> Result<FriendshipStatus, IgError> {
        let body = graphql::mutation_body(tokens, mutation, doc_id, pk);
        let form: Vec<(&str, &str)> = body.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let answer: FriendshipResult = self
            .post(
                mutation,
                &form,
                referer,
                Surface::Relay {
                    friendly_name: mutation.friendly_name(),
                    lsd: tokens.lsd.expose(),
                },
            )
            .await?;
        Ok(answer.status())
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::harness::{
        FOLLOWED, Known, LOGGED_IN_PAGE, client, instagram_that_takes_a_write, watching_as, writer,
    };
    use crate::error::declares_failure;

    /// A second mutation is sent only after an answer that says the first
    /// did not happen.
    ///
    /// The ambiguous ones are the point: a 502 from an edge that may already
    /// have forwarded the write, the redirect `post` refuses to follow, and a
    /// 200 this program could not read. Each used to earn a discovery walk
    /// and then a second `mutate` on the identifier it found.
    #[test]
    fn a_write_is_rediscovered_only_after_a_refusal_that_says_it_did_not_happen() {
        let unexpected = |status: u16| IgError::Unexpected {
            status,
            body: String::new(),
        };
        for refused in [
            unexpected(400),
            unexpected(200),
            IgError::NotFound { what: None },
        ] {
            assert!(worth_rediscovering(&refused), "{refused:?}");
        }
        for ambiguous in [
            unexpected(502),
            unexpected(302),
            unexpected(0),
            IgError::Decode("an answer that would not parse".into()),
            IgError::RateLimited,
            IgError::FeedbackRequired,
            IgError::SessionExpired,
            IgError::Canceled,
            IgError::NoCsrfToken,
        ] {
            assert!(
                !worth_rediscovering(&ambiguous),
                "{ambiguous:?} would have the write sent twice"
            );
        }
    }

    /// **What the mutation really answers**, captured live. Everything else in
    /// this file was right and this was the last thing wrong: the follow
    /// happened and the command said it had not, because the envelope was not
    /// one of the shapes being read.
    const FOLLOWED_GRAPHQL: &str = r#"{"data":{"xdt_create_friendship":{"friendship_status":{"following":true,"outgoing_request":false}}}}"#;

    /// The mobile shape, which this endpoint does not send and the client reads
    /// anyway. Instagram has been moving the web client onto the `/api/v1/`
    /// routes everywhere else, and the day it moves this one the answer changes
    /// shape without changing meaning.
    const FOLLOWED_OBJECT: &str =
        r#"{"status":"ok","friendship_status":{"following":true,"outgoing_request":false}}"#;

    /// **The request goes to `/api/graphql` and names the mutation**, which is
    /// what the real client does and what three live attempts established — see
    /// [`crate::graphql`] for the two that were sent first and changed nothing.
    #[tokio::test]
    async fn a_follow_names_the_mutation_with_the_id_it_was_given() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;

        let status = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();
        assert!(status.following);

        let requests = server.received_requests().await.unwrap();
        let write = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the mutation");
        assert_eq!(write.url.path(), "/api/graphql");

        let body = String::from_utf8_lossy(&write.body);
        assert!(
            body.contains("fb_api_req_friendly_name=usePolarisFollowMutation"),
            "{body}"
        );
        assert!(body.contains("doc_id=26508036048874888"), "{body}");
        assert!(body.contains("fb_dtsg=DTSG-TOKEN"), "{body}");
        assert!(body.contains("lsd=LSD-TOKEN"), "{body}");
        // The account, percent-encoded inside the variables object.
        assert!(body.contains("target_user_id"), "{body}");
    }

    /// **A page and a mutation, and both are paid for.** The page is the
    /// expensive half and it is why there is no bulk mode to be tempted by.
    #[tokio::test]
    async fn a_write_costs_a_page_and_a_mutation() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;
        writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "{requests:?}");
        // And the page asked for is the target's, which is where a browser
        // would have been standing.
        let page = requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::GET)
            .expect("the page");
        assert_eq!(page.url.path(), "/someone/");
    }

    /// A page with no tokens is a logged-out page, and saying so beats sending
    /// a mutation that cannot be authorized and reading whatever comes back.
    #[tokio::test]
    async fn a_logged_out_page_is_reported_as_a_dead_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>log in</html>"))
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::SessionExpired), "{error:?}");
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.method == wiremock::http::Method::GET),
            "nothing may be sent without the tokens to authorize it"
        );
    }

    /// Either answer shape means the same thing to the caller.
    #[tokio::test]
    async fn both_answer_shapes_read_the_same() {
        for body in [FOLLOWED, FOLLOWED_OBJECT, FOLLOWED_GRAPHQL] {
            let server = instagram_that_takes_a_write(body).await;
            let status = writer(&server)
                .await
                .follow(Pk::new(7), "someone", &Known)
                .await
                .unwrap();
            assert!(status.following, "{body}");
            assert!(!status.outgoing_request, "{body}");
        }
    }

    /// A private account answers `requested`, and that is not a follow.
    #[tokio::test]
    async fn a_private_account_answers_that_it_was_asked() {
        let server = instagram_that_takes_a_write(r#"{"result":"requested","status":"ok"}"#).await;

        let status = writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();
        assert!(status.outgoing_request);
        assert!(!status.following, "a request is not a follow");
    }

    /// The three headers a POST carries that a GET does not, plus the CSRF
    /// token, which a GET carries too but a write cannot go without.
    #[tokio::test]
    async fn a_write_carries_origin_a_content_type_and_the_csrf_token() {
        let server = instagram_that_takes_a_write(FOLLOWED).await;
        writer(&server)
            .await
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let headers = &requests
            .iter()
            .find(|r| r.method == wiremock::http::Method::POST)
            .expect("the mutation")
            .headers;
        assert_eq!(
            headers.get("origin").unwrap(),
            server.uri().trim_end_matches('/'),
            "a same-origin POST carries Origin, unlike a same-origin GET"
        );
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/x-www-form-urlencoded"
        );
        assert_eq!(headers.get("x-csrftoken").unwrap(), "TOKEN");
        assert!(headers.contains_key("cookie"));
        // And it does announce itself as the app, because `/api/graphql` is
        // the app's route. The surface that does not is the page fetch, and
        // `a_navigation_does_not_claim_to_be_the_app` next door asserts the
        // two against each other.
        assert!(headers.contains_key("x-ig-app-id"));
        assert!(headers.contains_key("user-agent"));
        assert!(headers.contains_key("accept-language"));
    }

    /// **Nothing is sent at all** when the session cannot sign the request.
    /// Discovering it from Instagram's 403 would cost a request, a slot of the
    /// write budget, and a message telling the user their session had expired
    /// when it had not.
    #[tokio::test]
    async fn a_session_without_a_csrf_token_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FOLLOWED))
            .mount(&server)
            .await;

        // `client` builds a pasted session, which has no token.
        let error = client(&server)
            .await
            .follow(Pk::new(7), "someone", &graphql::NoDocIds)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::NoCsrfToken), "{error:?}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "the refusal must happen before anything goes out"
        );
    }

    /// A redirect on a write is refused rather than followed. Following one
    /// would mean asking Instagram to do the thing a second time, and reqwest
    /// repeats the method and the body on a 307 or a 308.
    #[tokio::test]
    async fn a_redirected_write_is_not_replayed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", "/api/graphql"))
            .mount(&server)
            .await;

        let error = writer(&server)
            .await
            .unfollow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IgError::Unexpected { status: 307, .. }),
            "{error:?}"
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method == wiremock::http::Method::POST)
                .count(),
            1,
            "the write must have been sent exactly once"
        );
    }

    /// An action block on a write earns a cooldown, and the budget is asked to
    /// remember it. Driven through `Recording`, which does remember, rather
    /// than through `Pacer::unlimited`, which answers `Ok(0)` and forgets.
    #[tokio::test]
    async fn an_action_block_on_a_write_is_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            // No `spam` field: that one short-circuits to `RateLimited` before
            // the message is read, and what is under test here is the action
            // block, which carries the longer cooldown.
            .and(path("/api/graphql"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"message":"feedback_required","status":"fail"}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(budget.calls().len(), 1, "the cooldown was not written down");
    }

    /// **The refusal that was being read as success.**
    ///
    /// This is the shape a real block arrives in, and it is the one the earlier
    /// test above could not have caught: HTTP 200, no `status`, no `message`,
    /// the reason inside an `errors` array. Every field of `FriendshipResult`
    /// is optional, so before `declares_failure` learned this envelope the body
    /// deserialized cleanly with all of them absent, `status()` fell to its
    /// default arm, and the command reported "Instagram accepted it, but
    /// nothing changed" -- while the write budget had been spent and no
    /// cooldown had been written down.
    #[tokio::test]
    async fn a_graphql_refusal_under_a_200_is_an_action_block_and_not_a_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":null,"errors":[{"message":"feedback_required",
                    "summary":"Try Again Later",
                    "description":"We restrict certain activity to protect our community."}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, budget) = watching_as(&server.uri(), true);

        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(budget.calls().len(), 1, "the cooldown was not written down");
    }

    /// The reason may be in any of the three fields, and which one is not ours
    /// to choose. `summary` alone is enough.
    #[tokio::test]
    async fn a_graphql_reason_is_found_wherever_instagram_put_it() {
        for body in [
            r#"{"errors":[{"message":"checkpoint_required"}]}"#,
            r#"{"errors":[{"summary":"checkpoint_required"}]}"#,
            r#"{"errors":[{"description":"checkpoint_required"}]}"#,
        ] {
            assert!(declares_failure(body), "not seen as a failure: {body}");
            assert!(
                matches!(
                    crate::error::classify(200, body),
                    IgError::Checkpoint { .. }
                ),
                "not classified from: {body}"
            );
        }
    }

    /// An empty `errors` array is what a *successful* GraphQL answer may carry.
    /// Reading it as a refusal would fail every write that worked.
    #[tokio::test]
    async fn an_empty_errors_array_is_not_a_refusal() {
        assert!(!declares_failure(r#"{"data":{"x":1},"errors":[]}"#));
    }

    /// **The write path measures its own push-backs now.**
    ///
    /// It was the one class of request that never did: `note_push_back` was
    /// called from `get` and from `get_body`, never from `post`, and `post` did
    /// not so much as read the header. So the endpoint most likely to say
    /// something worth hearing was the one nobody was listening to.
    #[tokio::test]
    async fn a_write_keeps_the_retry_after_it_was_given() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/graphql"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "30")
                    .set_body_string(r#"{"message":"please wait a few minutes"}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGGED_IN_PAGE))
            .mount(&server)
            .await;

        let (client, _) = watching_as(&server.uri(), true);
        let error = client
            .follow(Pk::new(7), "someone", &Known)
            .await
            .unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
    }
}
