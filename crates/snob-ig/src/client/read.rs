//! The endpoints this tool reads, one method per URL.
//!
//! Every one of them is a GET through [`IgClient::get`] -- or, for the one that
//! wants HTML rather than JSON, through [`IgClient::get_body`] -- which is
//! where the budget is spent and the headers are built. What is here is the
//! address, the query, the page a browser would have called from, and how the
//! answer is read.

use snob_core::Pk;

use crate::error::IgError;
use crate::model::{
    FriendshipsPage, Identity, Reel, ReelsMedia, SearchUser, TopSearch, UserInfo, UserInfoEnvelope,
    WebProfileInfo, WebProfileInfoEnvelope,
};

use super::IgClient;
use super::headers::Surface;

/// Which side of the relationship is being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Followers,
    Following,
}

impl Direction {
    fn segment(self) -> &'static str {
        match self {
            Self::Followers => "followers",
            Self::Following => "following",
        }
    }
}

impl IgClient {
    /// Checks the session works, with the cheapest request that exercises the
    /// very endpoint the walk will depend on.
    pub async fn validate(&self) -> Result<(), IgError> {
        let _: FriendshipsPage = self
            .get(
                &format!(
                    "/api/v1/friendships/{}/{}/",
                    self.session.ds_user_id,
                    Direction::Following.segment()
                ),
                &[("count", "1")],
                // The account's own page: the only one we can name before the
                // username has been resolved.
                "",
            )
            .await?;
        Ok(())
    }

    /// Identity of the authenticated account. The id comes from the cookie
    /// itself; the username is only requested if we do not have it already.
    pub async fn whoami(&self) -> Result<Identity, IgError> {
        let pk = self.session.ds_user_id;
        if let Some(username) = &self.session.username {
            self.validate().await?;
            return Ok(Identity {
                pk,
                username: Some(username.clone()),
            });
        }
        let username = self.resolve_username(pk).await?;
        Ok(Identity { pk, username })
    }

    /// Resolves a username from an id. Returns `None` if Instagram answers but
    /// does not carry the field: it is cosmetic information and must never make
    /// a login fail.
    pub async fn resolve_username(&self, pk: Pk) -> Result<Option<String>, IgError> {
        Ok(self.user_info(pk).await?.map(|u| u.username))
    }

    /// Everything `/api/v1/users/{pk}/info/` says about an account. One request.
    ///
    /// Worth going to separately for the full-size profile picture, which no
    /// other endpoint offers.
    pub async fn user_info(&self, pk: Pk) -> Result<Option<UserInfo>, IgError> {
        let envelope: UserInfoEnvelope = self
            .get(&format!("/api/v1/users/{pk}/info/"), &[], "")
            .await?;
        Ok(envelope.user)
    }

    /// Public profile data, including the follower and following counters and
    /// the high-resolution picture.
    ///
    /// **One request in the ordinary case, and two only when the first fails in
    /// one particular way.** Nothing that works today costs anything extra:
    /// [`IgClient::profile_by_name`] is tried first and its answer is returned
    /// as it always was.
    ///
    /// The fallback exists because this endpoint answers **400** for certain
    /// business accounts, with
    /// `Asset asset://laser.provider/ig_business_category_subvertical has been
    /// deleted. You cannot use this schema` — Instagram failing to serialize
    /// its own reply, reproducible, and nothing to do with the request. It
    /// takes down every command that names an account, because they all start
    /// by turning a username into an id. Verified against the live API in
    /// August 2026: 400 for `elrubiuswtf`, 200 for an ordinary account, which is
    /// why the second route is reached only from the failure and never
    /// replaces the first.
    ///
    /// [`IgError::worth_a_second_route`] is what keeps this from becoming a
    /// retry loop. A 429, an action block, a challenge, an expired session, a
    /// cancel and a 404 all answer `false` there, so the one rule that matters
    /// — when a service says no, stop asking — is not weakened by having a
    /// second route at all.
    ///
    /// What comes back from search is **less**: an identity and the two
    /// friendship flags, and no counters. It is not padded out.
    /// [`WebProfileInfo::counters_are_knowable`] is how a caller tells the
    /// difference, and `engine::target` says so out loud, because a walk with
    /// no declared size is a walk `pager::verify_completion` cannot check for
    /// truncation.
    pub async fn web_profile_info(&self, username: &str) -> Result<WebProfileInfo, IgError> {
        let failure = match self.profile_by_name(username).await {
            Ok(profile) => return Ok(profile),
            Err(e) => e,
        };
        if !failure.worth_a_second_route() {
            return Err(failure);
        }

        tracing::debug!(
            error = %failure,
            "the profile endpoint would not answer; trying search"
        );

        match self.search_user_id(username).await {
            Ok(Some(user)) => {
                tracing::debug!(
                    pk = user.pk.get(),
                    "search resolved the account the profile lost"
                );
                Ok(WebProfileInfo::from_search(user))
            }
            // Search answered and knows no such account. The original failure
            // is still the truthful thing to report: this route not finding it
            // is not evidence that the name is free, and a 400 reported as
            // "no such account" sends somebody hunting for a typo that is not
            // there.
            Ok(None) => {
                tracing::debug!("search knows no such account either");
                Err(failure)
            }
            // The fallback's own failure must not replace the real one --
            // **except** when it is one the user has to act on. A cooldown or a
            // dead session recorded on this request is a fact about the account
            // that would otherwise be swallowed by an error about serialization.
            Err(second) => {
                if crate::error::cooldown_for(&second).is_some() || second.invalidates_session() {
                    Err(second)
                } else {
                    Err(failure)
                }
            }
        }
    }

    /// The profile endpoint on its own, with no fallback behind it.
    async fn profile_by_name(&self, username: &str) -> Result<WebProfileInfo, IgError> {
        let missing = || IgError::NotFound {
            what: Some(username.to_string()),
        };
        // Instagram says the same thing two ways: a 404 for the page, or a 200
        // whose envelope carries no user. Both mean the name is not taken, and
        // the person asking should read one answer, not two.
        let envelope: WebProfileInfoEnvelope = self
            .get(
                "/api/v1/users/web_profile_info/",
                &[("username", username)],
                &format!("{}/", snob_core::model::in_a_path(username)),
            )
            .await
            .map_err(|e| match e {
                IgError::NotFound { .. } => missing(),
                other => other,
            })?;
        envelope.data.user.ok_or_else(missing)
    }

    /// Resolves a username to an id through the web client's search box.
    ///
    /// **A fallback, never a first choice.** It exists because
    /// `web_profile_info` answers 400 for certain business accounts with a
    /// serialization failure of Instagram's own -- see `resolve_id` -- and it
    /// is used only after that has happened.
    ///
    /// Search matches loosely, so the answer is filtered to an exact,
    /// case-insensitive match on the name asked for. Without that, asking about
    /// a name that does not exist hands back whatever the search box would have
    /// suggested instead, and the run then walks a stranger's followers under
    /// the name that was typed. That is the failure this whole route could
    /// introduce, and it is the only reason the comparison is here rather than
    /// left to the caller.
    ///
    /// `None` means search knows no such account, which is what a caller should
    /// report as "no such account" rather than as a failure of the fallback.
    pub async fn search_user_id(&self, username: &str) -> Result<Option<SearchUser>, IgError> {
        let found: TopSearch = self
            .get(
                "/web/search/topsearch/",
                &[("context", "blended"), ("query", username), ("count", "1")],
                // In a browser this is called from whatever page the search box
                // is open on. The site's own address is the truthful one.
                "",
            )
            .await?;

        Ok(found
            .users
            .into_iter()
            .map(|hit| hit.user)
            .find(|user| user.username.eq_ignore_ascii_case(username)))
    }

    /// One page of followers or following. Pagination is the caller's job.
    ///
    /// `username` is only used to name the page a browser would have made this
    /// call from. It is allowed to be empty — the walk still works — but the
    /// caller knows it, so it may as well say it.
    pub async fn friendships_page(
        &self,
        pk: Pk,
        username: &str,
        direction: Direction,
        count: u32,
        cursor: Option<&str>,
    ) -> Result<FriendshipsPage, IgError> {
        let count = count.to_string();
        let mut query: Vec<(&str, &str)> = vec![("count", &count)];
        if let Some(c) = cursor {
            query.push(("max_id", c));
        }
        let segment = direction.segment();
        self.get(
            &format!("/api/v1/friendships/{pk}/{segment}/"),
            &query,
            // In a browser this call only ever comes from the modal that opens
            // over the account's own page.
            &if username.is_empty() {
                String::new()
            } else {
                format!("{}/{segment}/", snob_core::model::in_a_path(username))
            },
        )
        .await
    }

    /// A page, as HTML, exactly as a browser navigating to it would get it.
    ///
    /// The one reader here that does not want JSON. It exists because the two
    /// tokens a mutation needs — `fb_dtsg` and `lsd` — are only ever handed out
    /// inside a rendered page; see [`crate::graphql`].
    ///
    /// It costs a request like everything else, and it is a **large** one: a
    /// profile page is around six hundred kilobytes of bootstrapped Relay
    /// state. That is why the caller caches what it finds rather than reading
    /// the page per write.
    pub async fn page(&self, path: &str) -> Result<String, IgError> {
        let answer = self.get_body(path, &[], "", Surface::Document).await?;
        if !(200..300).contains(&answer.status) {
            self.note_push_back(&answer);
            return Err(self.classify_and_record(answer.status, &answer.body));
        }
        Ok(answer.body)
    }

    /// The stories an account has up right now. One request.
    ///
    /// **This does not tell anybody you looked.** Instagram registers a view
    /// through a separate call, which this project does not implement and which
    /// `crates/snob-core/tests/no_seen.rs` checks has not appeared. Fetching the
    /// reel is a read like any other.
    ///
    /// An account with nothing up answers 200 with an empty envelope rather
    /// than 404, so `None` means there are no stories and not that the account
    /// is missing — the caller resolved it before getting here.
    pub async fn stories(&self, pk: Pk, username: &str) -> Result<Option<Reel>, IgError> {
        let ids = pk.to_string();
        let envelope: ReelsMedia = self
            .get(
                "/api/v1/feed/reels_media/",
                &[("reel_ids", ids.as_str())],
                // In a browser this call comes from the story viewer, which
                // opens over the account's own page.
                &if username.is_empty() {
                    String::new()
                } else {
                    format!("{}/", snob_core::model::in_a_path(username))
                },
            )
            .await?;
        Ok(envelope.reel())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use snob_core::session::{Session, SessionOrigin};
    use url::Url;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::harness::{Recording, SID, UA, client};

    /// The referer names the page a browser would have called from. A generic
    /// one next to a followers endpoint is an incoherence that costs nothing
    /// to avoid.
    #[tokio::test]
    async fn the_referer_names_the_page_the_call_would_come_from() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
            .mount(&server)
            .await;

        client(&server)
            .await
            .friendships_page(Pk::new(7), "someone", Direction::Followers, 50, None)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0]
                .headers
                .get("referer")
                .unwrap()
                .to_str()
                .unwrap(),
            "https://www.instagram.com/someone/followers/"
        );
    }

    /// A name that cannot go in a header verbatim must still get an answer
    /// about the name.
    ///
    /// `target::clean` strips a leading `@` and nothing else, and `watch.toml`
    /// does not validate a username at all, so the referer is the one
    /// name-in-a-URL in this crate that arrives as typed. Any byte below 0x20
    /// makes the header unbuildable; reqwest holds that failure until `send()`,
    /// where `?` reads it as `Network` — a *retryable* fault, so the pager
    /// sends the same doomed request three more times and the pacer charges
    /// for four requests that never left the machine. The name it was asking
    /// about is never mentioned.
    #[tokio::test]
    async fn a_hostile_name_still_reaches_the_not_found_arm() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"user":null}}"#))
            .mount(&server)
            .await;

        let hostile = "gh\u{1b}[2K";
        let error = client(&server)
            .await
            .web_profile_info(hostile)
            .await
            .unwrap_err();

        assert!(
            matches!(&error, IgError::NotFound { what: Some(name) } if name == hostile),
            "{error:?}"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            1,
            "the request the budget paid for went out"
        );
        assert_eq!(
            requests[0]
                .headers
                .get("referer")
                .unwrap()
                .to_str()
                .unwrap(),
            "https://www.instagram.com/gh%1B%5B2K/",
            "encoded, not filtered: a name with a character removed is a different account"
        );
    }

    #[tokio::test]
    async fn it_reads_a_page_of_followers() {
        let server = MockServer::start().await;
        let body = r#"{"users":[
            {"pk":"1","username":"one","full_name":"One","is_verified":true,"is_private":false},
            {"pk":2,"username":"two"}
        ],"next_max_id":"QVFB","status":"ok"}"#;
        Mock::given(method("GET"))
            .and(path("/api/v1/friendships/42/followers/"))
            .and(query_param("count", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let page = client(&server)
            .await
            .friendships_page(Pk::new(42), "someone", Direction::Followers, 50, None)
            .await
            .unwrap();

        assert_eq!(page.users.len(), 2);
        assert_eq!(page.users[0].username, "one");
        assert_eq!(page.users[0].is_verified, Some(true));
        assert_eq!(page.users[1].full_name, None);
        assert_eq!(page.next_cursor(), Some("QVFB"));
    }

    #[tokio::test]
    async fn the_cursor_is_sent_as_max_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("max_id", "QVFB"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"users":[]}"#))
            .mount(&server)
            .await;

        client(&server)
            .await
            .friendships_page(
                Pk::new(42),
                "someone",
                Direction::Following,
                50,
                Some("QVFB"),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn it_resolves_a_username_from_an_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/42/info/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"user":{"pk":42,"username":"whoever"},"status":"ok"}"#),
            )
            .mount(&server)
            .await;

        let name = client(&server)
            .await
            .resolve_username(Pk::new(42))
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some("whoever"));
    }

    /// The live answer to a name nobody owns: a 404 carrying a web page. What
    /// reaches the terminal must be the account name, not the markup.
    #[tokio::test]
    async fn a_profile_that_does_not_exist_is_named_not_dumped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .respond_with(ResponseTemplate::new(404).set_body_string(
                "<!DOCTYPE html><html><head><title>Page Not Found</title></head></html>",
            ))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .web_profile_info("nobody")
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("nobody"), "{message}");
        assert!(!message.contains("DOCTYPE"), "{message}");
    }

    /// The other half of the same answer: a 200 whose envelope has no user.
    /// It must read exactly like the 404 above.
    #[tokio::test]
    async fn an_empty_profile_envelope_reads_like_the_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{"user":null}}"#))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .web_profile_info("nobody")
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "the account \"nobody\" does not exist");
    }

    /// The body Instagram really sends for the accounts this fallback exists
    /// for. Captured live in August 2026.
    const BROKEN_PROFILE: &str = r#"{"message":"Asset asset://laser.provider/ig_business_category_subvertical has been deleted. You cannot use this schema","status":"fail"}"#;

    /// One search hit, shaped like the live answer: `pk` as a string, no
    /// counters anywhere, and the relationship under `friendship_status`.
    fn search_body(username: &str, pk: &str) -> String {
        format!(
            r#"{{"users":[{{"position":0,"user":{{"pk":"{pk}","username":"{username}",
               "full_name":"Someone","is_private":false,"is_verified":true,
               "profile_pic_url":"https://cdninstagram.com/p.jpg",
               "friendship_status":{{"following":true,"outgoing_request":false,
               "is_private":false}}}}}}],"status":"ok"}}"#
        )
    }

    fn mount_profile(server: &MockServer, response: ResponseTemplate) -> impl Future<Output = ()> {
        Mock::given(method("GET"))
            .and(path("/api/v1/users/web_profile_info/"))
            .respond_with(response)
            .mount(server)
    }

    /// The ordinary account is untouched: one request, the full answer, and the
    /// route it came from says so.
    ///
    /// This is the half that is easy to break while fixing the other one. The
    /// fallback must not cost anybody who does not need it a second request, so
    /// the charge is asserted rather than assumed.
    #[tokio::test]
    async fn an_ordinary_profile_still_costs_one_request() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(200).set_body_string(
                r#"{"data":{"user":{"id":"7","username":"ann",
                   "edge_followed_by":{"count":10},"edge_follow":{"count":4}}}}"#,
            ),
        )
        .await;

        let client = client(&server).await;
        let profile = client.web_profile_info("ann").await.unwrap();

        assert_eq!(profile.id, Pk::new(7));
        assert_eq!(profile.via, crate::model::Via::Profile);
        assert!(profile.counters_are_knowable());
        assert_eq!(profile.follower_count(), Some(10));
        assert_eq!(client.pacer().spent(), 1, "the fallback was not needed");

        let asked = server.received_requests().await.unwrap();
        assert_eq!(asked.len(), 1, "search was reached on a working account");
    }

    /// Instagram failing to serialize its own reply does not take the account
    /// down with it.
    ///
    /// The 400 here is verbatim from the live API. Every command that names an
    /// account starts by turning the name into an id, so without the fallback
    /// this one body stops `pfp`, `scan` and every set command.
    #[tokio::test]
    async fn a_profile_instagram_cannot_serialize_is_resolved_by_search() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_body("rubius", "1506")))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let profile = client.web_profile_info("rubius").await.unwrap();

        assert_eq!(profile.id, Pk::new(1506));
        assert_eq!(profile.username, "rubius");
        assert_eq!(profile.via, crate::model::Via::Search);

        // The two facts the private-account refusal turns on survive the
        // change of route, under different names.
        assert_eq!(profile.followed_by_viewer, Some(true));
        assert_eq!(profile.requested_by_viewer, Some(false));

        // And the counters do not. **`None`, never `Some(0)`**: a declared zero
        // is what would make `pager::verify_completion` call every short walk
        // complete.
        assert!(!profile.counters_are_knowable());
        assert_eq!(profile.follower_count(), None);
        assert_eq!(profile.following_count(), None);

        assert_eq!(client.pacer().spent(), 2, "the failure and the fallback");
    }

    /// A push-back is never worked around.
    ///
    /// This is the rule the whole fallback is written around: when a service
    /// says no, the answer is to stop asking. A 429 already carries a cooldown
    /// by the time the fallback would be considered, and sending a second
    /// request into an endpoint that has just refused is exactly how a
    /// momentary limit becomes a lasting one.
    #[tokio::test]
    async fn a_push_back_is_not_worked_around() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(429).set_body_string(r#"{"message":"feedback_required"}"#),
        )
        .await;

        let budget = Arc::new(Recording::default());
        let session = Session::from_sessionid(SID, UA, SessionOrigin::Paste).unwrap();
        let client = IgClient::new(session, crate::pace::Pacer::new(budget.clone()))
            .unwrap()
            .with_base_url(Url::parse(&server.uri()).unwrap());

        let error = client.web_profile_info("ann").await.unwrap_err();
        assert!(matches!(error, IgError::FeedbackRequired), "{error:?}");
        assert_eq!(
            client.pacer().spent(),
            1,
            "a second request was sent anyway"
        );

        let asked = server.received_requests().await.unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(budget.calls().len(), 1, "the cooldown still gets recorded");
    }

    /// Every answer that means "no" means no, and a 404 is a real answer.
    ///
    /// Asking search about a name nobody owns spends a request to be told the
    /// same thing, and the two session errors and the cancel must not be
    /// retried at all.
    #[test]
    fn only_a_broken_answer_earns_a_second_route() {
        assert!(
            IgError::Unexpected {
                status: 400,
                body: BROKEN_PROFILE.into()
            }
            .worth_a_second_route()
        );

        for refused in [
            IgError::RateLimited,
            IgError::FeedbackRequired,
            IgError::Challenge { url: None },
            IgError::Checkpoint { url: None },
            IgError::SessionExpired,
            IgError::UserAgentMismatch,
            IgError::Canceled,
            IgError::NotFound { what: None },
            IgError::Decode("a captive portal".into()),
            // The server being unwell is what `Reaction::Retry` is for, and a
            // second route there would hide an outage behind a worse answer.
            IgError::Unexpected {
                status: 503,
                body: String::new(),
            },
        ] {
            assert!(
                !refused.worth_a_second_route(),
                "{refused:?} would be worked around"
            );
        }
    }

    /// Search matches loosely, and a loose match is a different account.
    ///
    /// This is the failure the fallback could introduce and the reason the
    /// exact-name comparison is in `search_user_id` rather than left to a
    /// caller: without it, asking about a name that Instagram would not serve
    /// hands back whatever the search box suggested instead, and the run then
    /// walks a stranger's followers under the name that was typed.
    #[tokio::test]
    async fn search_does_not_hand_back_somebody_else() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(search_body("rubius_fanpage", "999")),
            )
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client.web_profile_info("rubius").await.unwrap_err();

        // The original failure, not a wrong account and not a "no such
        // account": search not finding it is no evidence the name is free.
        assert!(
            matches!(&error, IgError::Unexpected { status: 400, .. }),
            "{error:?}"
        );
    }

    /// Case is not identity, but it is not a different account either.
    #[tokio::test]
    async fn search_matches_the_name_whatever_its_case() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(search_body("Rubius", "12")))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let profile = client.web_profile_info("rubius").await.unwrap();
        assert_eq!(profile.id, Pk::new(12));
    }

    /// When the fallback is the one that hits the wall, the wall is what gets
    /// reported.
    ///
    /// A cooldown recorded on the second request is a fact about the account
    /// that the user has to act on, and letting the first failure stand would
    /// bury it under a message about serialization.
    #[tokio::test]
    async fn a_cooldown_on_the_fallback_is_not_swallowed() {
        let server = MockServer::start().await;
        mount_profile(
            &server,
            ResponseTemplate::new(400).set_body_string(BROKEN_PROFILE),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/web/search/topsearch/"))
            .respond_with(ResponseTemplate::new(429).set_body_string(r#"{"message":"spam"}"#))
            .mount(&server)
            .await;

        let client = client(&server).await;
        let error = client.web_profile_info("rubius").await.unwrap_err();
        assert!(matches!(error, IgError::RateLimited), "{error:?}");
    }

    /// Both envelope shapes carry the same reel, and the caller cannot tell
    /// which arrived. Reading only the one seen during development is how this
    /// breaks quietly when Instagram switches.
    #[tokio::test]
    async fn stories_are_read_out_of_either_envelope() {
        let item = r#"{"pk":"1","media_type":1,"taken_at":100,"expiring_at":200,
            "image_versions2":{"candidates":[{"url":"https://x/s.jpg","width":640,"height":1136},
            {"url":"https://x/b.jpg","width":1080,"height":1920}]}}"#;

        for envelope in [
            format!(r#"{{"reels_media":[{{"items":[{item}]}}]}}"#),
            format!(r#"{{"reels":{{"42":{{"items":[{item}]}}}}}}"#),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/v1/feed/reels_media/"))
                .and(query_param("reel_ids", "42"))
                .respond_with(ResponseTemplate::new(200).set_body_string(&envelope))
                .mount(&server)
                .await;

            let reel = client(&server)
                .await
                .stories(Pk::new(42), "someone")
                .await
                .unwrap()
                .expect("a reel");
            assert_eq!(reel.items.len(), 1);
            assert_eq!(
                crate::model::largest(&reel.items[0].image_versions2.clone().unwrap().candidates)
                    .unwrap()
                    .url,
                "https://x/b.jpg",
                "the biggest candidate wins, not the first"
            );
        }
    }

    /// An account with nothing up is not a missing account.
    #[tokio::test]
    async fn an_account_with_no_stories_is_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/feed/reels_media/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"reels_media":[]}"#))
            .mount(&server)
            .await;

        assert!(
            client(&server)
                .await
                .stories(Pk::new(42), "someone")
                .await
                .unwrap()
                .is_none()
        );
    }
}
