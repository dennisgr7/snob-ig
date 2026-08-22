//! Models of Instagram's responses.
//!
//! Everything but `pk` is optional on purpose. This is an undocumented API
//! with no contract: it adds and removes fields without notice, and one
//! unexpected
//! `null` should not bring down a walk over two thousand accounts.

use serde::Deserialize;
use snob_core::Pk;

/// An account as it appears in a followers or following list.
#[derive(Debug, Clone, Deserialize)]
pub struct UserSummary {
    #[serde(deserialize_with = "flexible_pk")]
    pub pk: Pk,
    pub username: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_verified: Option<bool>,
    #[serde(default)]
    pub profile_pic_url: Option<String>,
}

/// From the wire format to the domain one. The conversion lives here because it
/// is the only direction the dependencies allow: `snob-core` does not know
/// about `snob-ig`.
impl From<&UserSummary> for snob_core::model::User {
    fn from(u: &UserSummary) -> Self {
        Self {
            pk: u.pk,
            username: u.username.clone(),
            full_name: u.full_name.clone(),
            is_private: u.is_private,
            is_verified: u.is_verified,
            pfp_url: u.profile_pic_url.clone(),
        }
    }
}

impl From<snob_core::model::ListKind> for crate::client::Direction {
    fn from(kind: snob_core::model::ListKind) -> Self {
        match kind {
            snob_core::model::ListKind::Followers => Self::Followers,
            snob_core::model::ListKind::Following => Self::Following,
        }
    }
}

/// One page of `/api/v1/friendships/{pk}/{followers|following}/`.
#[derive(Debug, Clone, Deserialize)]
pub struct FriendshipsPage {
    #[serde(default)]
    pub users: Vec<UserSummary>,
    /// Cursor to the next page. Absent or empty means the list is done.
    #[serde(default)]
    pub next_max_id: Option<String>,
}

impl FriendshipsPage {
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_max_id.as_deref().filter(|c| !c.is_empty())
    }
}

/// Response of `/api/v1/users/{pk}/info/`. This is how a username is resolved
/// from a numeric id.
#[derive(Debug, Clone, Deserialize)]
pub struct UserInfoEnvelope {
    #[serde(default)]
    pub user: Option<UserInfo>,
}

/// The account as this one endpoint describes it.
///
/// Kept apart from [`UserSummary`] rather than adding fields to it: that one is
/// deserialized once per account on every page of a walk, and the picture
/// details below would ride along thousands of times over for nothing.
#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    pub username: String,
    /// The full-size picture. This endpoint is the only one that offers it:
    /// `web_profile_info` calls its field `_hd` but hands back a URL the CDN
    /// has been told to downscale, and the instruction is covered by the
    /// signature, so it cannot be removed.
    #[serde(default)]
    pub hd_profile_pic_url_info: Option<PictureVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PictureVersion {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// Response of `/api/v1/users/web_profile_info/?username=`.
#[derive(Debug, Clone, Deserialize)]
pub struct WebProfileInfoEnvelope {
    pub data: WebProfileInfoData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebProfileInfoData {
    pub user: Option<WebProfileInfo>,
}

/// Which route answered.
///
/// It exists because the two do not answer the same question.
/// `web_profile_info` carries the follower and following counters; search does
/// not, and there is no third endpoint that would fill them in for free. A
/// caller that needs a counter has to be able to tell "nobody asked" from
/// "asked, and this route cannot say" — [`WebProfileInfo::counters_are_knowable`]
/// is that question, and `engine::target` is where it is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Via {
    /// `/api/v1/users/web_profile_info/`, which answers with everything.
    #[default]
    Profile,
    /// The search box, which answers with an identity and no counters.
    Search,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebProfileInfo {
    /// Not deserialized: no response carries it. It is set by whichever route
    /// built the value, and defaults to the one that parses from JSON.
    #[serde(skip)]
    pub via: Via,
    #[serde(deserialize_with = "flexible_pk")]
    pub id: Pk,
    pub username: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_verified: Option<bool>,
    /// Whether the logged-in viewer follows this account. Absent on some
    /// responses; `None` means unknown and must never block anything.
    #[serde(default)]
    pub followed_by_viewer: Option<bool>,
    /// Whether the viewer has a pending follow request to this account.
    #[serde(default)]
    pub requested_by_viewer: Option<bool>,
    #[serde(default)]
    pub profile_pic_url: Option<String>,
    /// Despite the name, measured at 320x320: the URL carries an instruction to
    /// the CDN to downscale, and the signature covers it, so it cannot be
    /// stripped. The full 1080x1080 comes from [`UserInfo`] instead.
    #[serde(default)]
    pub profile_pic_url_hd: Option<String>,
    #[serde(default, rename = "edge_followed_by")]
    pub followers: Option<CountEdge>,
    #[serde(default, rename = "edge_follow")]
    pub following: Option<CountEdge>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct CountEdge {
    pub count: u64,
}

impl WebProfileInfo {
    pub fn follower_count(&self) -> Option<u64> {
        self.followers.map(|e| e.count)
    }

    pub fn following_count(&self) -> Option<u64> {
        self.following.map(|e| e.count)
    }

    /// Whether this route could have answered with counters at all.
    ///
    /// **Not the same question as whether there are any.** `None` from
    /// `web_profile_info` means Instagram left the field out of an answer that
    /// carries it for everybody else; `None` from search means the endpoint has
    /// no such field. The first is worth reporting as odd, the second is
    /// ordinary — and neither is zero, which is the reading that would turn a
    /// short walk into a silent truncation nobody was warned about.
    pub fn counters_are_knowable(&self) -> bool {
        self.via == Via::Profile
    }

    /// Builds one from what search knows.
    ///
    /// Everything absent stays absent. The counters in particular are **not**
    /// defaulted to zero: `pager::verify_completion` compares a walk against
    /// the declared size, and a declared zero would make every walk look
    /// complete.
    pub fn from_search(user: SearchUser) -> Self {
        Self {
            via: Via::Search,
            id: user.pk,
            username: user.username,
            full_name: user.full_name,
            is_private: user.is_private,
            is_verified: user.is_verified,
            // Search spells the same two facts differently, and they are the
            // two the private-account refusal in `engine::target` turns on, so
            // they are worth carrying across rather than dropping.
            followed_by_viewer: user.friendship_status.as_ref().map(|f| f.following),
            requested_by_viewer: user.friendship_status.as_ref().map(|f| f.outgoing_request),
            profile_pic_url: user.profile_pic_url,
            // Search has no high-resolution URL. `pfp` does not need one: it
            // asks `/users/{pk}/info/` for the full size, and that works from
            // the id alone.
            profile_pic_url_hd: None,
            followers: None,
            following: None,
        }
    }
}

/// Identity of the account we are authenticated as.
#[derive(Debug, Clone)]
pub struct Identity {
    pub pk: Pk,
    pub username: Option<String>,
}

/// Instagram returns the id sometimes as a number and sometimes as a string,
/// depending on the endpoint.
fn flexible_pk<'de, D>(deserializer: D) -> Result<Pk, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u64),
        Text(String),
    }

    match Raw::deserialize(deserializer)? {
        Raw::Number(n) => Ok(Pk::new(n)),
        Raw::Text(s) => s.parse().map_err(D::Error::custom),
    }
}

/// What Instagram answers to a follow or an unfollow.
///
/// **Two shapes, because there are two of these endpoints and only one of them
/// is reachable from the web.** `/web/friendships/{pk}/follow/` answers with a
/// single word — `"following"`, `"requested"`, `"unfollowed"` — under `result`;
/// the mobile `/api/v1/friendships/create/` answers with the whole
/// `friendship_status` object. Both are read, so that this keeps working if
/// Instagram moves the web client onto the other one, which is the direction it
/// has been moving everything else.
///
/// The interesting distinction either way is **requested versus following**.
/// Following a private account does not follow it — it asks — and reporting a
/// request as a follow would be the command lying about the one thing it was
/// run to do.
#[derive(Debug, Clone, Deserialize)]
pub struct FriendshipResult {
    #[serde(default)]
    pub friendship_status: Option<FriendshipStatus>,
    #[serde(default)]
    pub result: Option<String>,
    /// The GraphQL envelope, which is the one that actually arrives.
    ///
    /// **Reading it was the last thing to get right, and getting it wrong was
    /// invisible from the request's side.** The mutation succeeded — the
    /// account really was followed, confirmed by reading the relationship back
    /// — and this said "Instagram accepted the request, but @nasa is still not
    /// followed", because none of the shapes below were the shape that came.
    /// A command that does the thing and then reports that it did not is worse
    /// than one that fails.
    #[serde(default)]
    pub data: Option<MutationData>,
}

/// `{"data":{"xdt_create_friendship":{"friendship_status":{…}}}}`.
///
/// One field per verb, both optional, because a response carries whichever one
/// it is answering about.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MutationData {
    #[serde(default)]
    pub xdt_create_friendship: Option<Box<FriendshipResult>>,
    #[serde(default)]
    pub xdt_destroy_friendship: Option<Box<FriendshipResult>>,
}

impl FriendshipResult {
    /// The relationship as it stands now, whichever shape said so.
    ///
    /// The object wins when it is there, because it says more. The word is read
    /// only as a fallback, and an unrecognized one produces the default — every
    /// flag false — which the command reports as "Instagram accepted it but
    /// nothing changed". That is the honest reading of a word nobody here knows:
    /// better than guessing it meant success.
    pub fn status(self) -> FriendshipStatus {
        // The envelope first, because it is the one Instagram actually sends
        // today and the two flatter shapes are what it used to.
        if let Some(inner) = self
            .data
            .and_then(|d| d.xdt_create_friendship.or(d.xdt_destroy_friendship))
        {
            return inner.status();
        }
        if let Some(status) = self.friendship_status {
            return status;
        }
        match self.result.as_deref() {
            Some("following") => FriendshipStatus {
                following: true,
                ..Default::default()
            },
            Some("requested") => FriendshipStatus {
                outgoing_request: true,
                ..Default::default()
            },
            // What an unfollow answers, and it is the default: not following,
            // nothing outstanding.
            _ => FriendshipStatus::default(),
        }
    }
}

/// The relationship, as two different endpoints describe it.
///
/// **One type for both, and the merge is where that mattered.** The write path
/// reads it out of a mutation's answer; the search fallback reads it out of a
/// search result, where these two fields are what `web_profile_info` spells
/// `followed_by_viewer` and `requested_by_viewer` — so a private account is
/// still refused before a page is walked when search was the route that
/// answered. Two structures of the same name would have been two spellings of
/// the same fact, and the compiler said so.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FriendshipStatus {
    #[serde(default)]
    pub following: bool,
    #[serde(default)]
    pub outgoing_request: bool,
    #[serde(default)]
    pub followed_by: bool,
    /// Whether the other account is private, which is what decides whether a
    /// follow became a follow or a request.
    #[serde(default)]
    pub is_private: bool,
}

/// Response of `/api/v1/feed/reels_media/?reel_ids={pk}`.
///
/// Two shapes are accepted because Instagram serves both, and which one arrives
/// has moved between versions: `reels_media` is a list of reels and `reels` is a
/// map keyed by the account id. They carry the same reel. Reading only the one
/// that happened to arrive during development is how this breaks silently six
/// months later, so both are read and [`Self::reel`] is the only way in.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReelsMedia {
    #[serde(default)]
    pub reels_media: Vec<Reel>,
    #[serde(default)]
    pub reels: std::collections::HashMap<String, Reel>,
}

impl ReelsMedia {
    /// The one reel that was asked for, whichever shape it came back in.
    ///
    /// An account with nothing up answers with both collections empty rather
    /// than with a 404, so `None` here means "no stories", not "no such
    /// account". The caller has already resolved the account by then.
    pub fn reel(self) -> Option<Reel> {
        self.reels_media
            .into_iter()
            .next()
            .or_else(|| self.reels.into_values().next())
    }
}

/// One account's stories: the tray entry plus the items themselves.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reel {
    #[serde(default)]
    pub items: Vec<ReelItem>,
    #[serde(default)]
    pub user: Option<UserSummary>,
}

/// One story.
///
/// `media_type` is Instagram's integer — 1 for a photo, 2 for a video — and it
/// is kept as the wire integer here and turned into something meaningful at the
/// boundary, like every other field in this module. Nothing downstream compares
/// it to a literal.
#[derive(Debug, Clone, Deserialize)]
pub struct ReelItem {
    pub pk: String,
    #[serde(default)]
    pub media_type: u8,
    #[serde(default)]
    pub taken_at: i64,
    /// When it disappears. Absent on some items, which is why it is optional
    /// rather than defaulted to zero: "expires at the epoch" would print as an
    /// expired story rather than as an unknown one.
    #[serde(default)]
    pub expiring_at: Option<i64>,
    #[serde(default)]
    pub image_versions2: Option<Candidates>,
    #[serde(default)]
    pub video_versions: Vec<PictureVersion>,
    #[serde(default)]
    pub reel_mentions: Vec<ReelMention>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Candidates {
    #[serde(default)]
    pub candidates: Vec<PictureVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReelMention {
    #[serde(default)]
    pub user: Option<UserSummary>,
}

/// Picks the largest of the versions Instagram offers.
///
/// The web client picks the one that fits the viewport; nothing here draws in a
/// terminal, so what is wanted is simply the biggest. Missing dimensions sort
/// last rather than first: an entry that does not say how large it is must not
/// win by default over one that does.
pub fn largest(versions: &[PictureVersion]) -> Option<&PictureVersion> {
    versions
        .iter()
        .max_by_key(|v| (v.width.unwrap_or(0) as u64) * (v.height.unwrap_or(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_accepted_as_a_number_or_as_text() {
        let as_text: UserSummary = serde_json::from_str(r#"{"pk":"123","username":"a"}"#).unwrap();
        let as_number: UserSummary = serde_json::from_str(r#"{"pk":123,"username":"a"}"#).unwrap();
        assert_eq!(as_text.pk, Pk::new(123));
        assert_eq!(as_number.pk, Pk::new(123));
    }

    #[test]
    fn missing_fields_are_tolerated() {
        let u: UserSummary = serde_json::from_str(r#"{"pk":1,"username":"a"}"#).unwrap();
        assert_eq!(u.full_name, None);
        assert_eq!(u.is_verified, None);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{"pk":1,"username":"a","field_from_the_future":{"nested":true}}"#;
        let u: UserSummary = serde_json::from_str(json).unwrap();
        assert_eq!(u.username, "a");
    }

    #[test]
    fn a_page_with_no_cursor_is_the_last_one() {
        let empty: FriendshipsPage = serde_json::from_str(r#"{"users":[]}"#).unwrap();
        assert_eq!(empty.next_cursor(), None);

        let empty_string: FriendshipsPage =
            serde_json::from_str(r#"{"users":[],"next_max_id":""}"#).unwrap();
        assert_eq!(empty_string.next_cursor(), None);

        let with_cursor: FriendshipsPage =
            serde_json::from_str(r#"{"users":[],"next_max_id":"QVFB"}"#).unwrap();
        assert_eq!(with_cursor.next_cursor(), Some("QVFB"));
    }

    #[test]
    fn the_profile_counters_are_read() {
        let json = r#"{"data":{"user":{"id":"42","username":"someone","edge_followed_by":{"count":1200},"edge_follow":{"count":340}}}}"#;
        let envelope: WebProfileInfoEnvelope = serde_json::from_str(json).unwrap();
        let u = envelope.data.user.unwrap();
        assert_eq!(u.id, Pk::new(42));
        assert_eq!(u.follower_count(), Some(1200));
        assert_eq!(u.following_count(), Some(340));
        assert_eq!(u.followed_by_viewer, None, "absent means unknown");
        assert_eq!(u.requested_by_viewer, None);
    }

    #[test]
    fn the_viewer_relationship_fields_are_read() {
        let json = r#"{"data":{"user":{"id":1,"username":"ghost","is_private":true,"followed_by_viewer":false,"requested_by_viewer":true}}}"#;
        let envelope: WebProfileInfoEnvelope = serde_json::from_str(json).unwrap();
        let u = envelope.data.user.unwrap();
        assert_eq!(u.is_private, Some(true));
        assert_eq!(u.followed_by_viewer, Some(false));
        assert_eq!(u.requested_by_viewer, Some(true));
    }

    #[test]
    fn a_missing_profile_arrives_as_a_null_user() {
        let envelope: WebProfileInfoEnvelope =
            serde_json::from_str(r#"{"data":{"user":null}}"#).unwrap();
        assert!(envelope.data.user.is_none());
    }

    #[test]
    fn the_wire_model_converts_to_the_domain_one() {
        let wire = UserSummary {
            pk: Pk::new(7),
            username: "someone".into(),
            full_name: Some("Some One".into()),
            is_private: Some(false),
            is_verified: Some(true),
            profile_pic_url: Some("https://example/pic.jpg".into()),
        };
        let domain: snob_core::model::User = (&wire).into();
        assert_eq!(domain.pk, Pk::new(7));
        assert_eq!(domain.pfp_url.as_deref(), Some("https://example/pic.jpg"));
    }
}

/// Response of the web client's search box.
///
/// Only the accounts matter here: the endpoint also answers with places and
/// hashtags, and both are ignored rather than modeled.
#[derive(Debug, Clone, Deserialize)]
pub struct TopSearch {
    #[serde(default)]
    pub users: Vec<TopSearchHit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TopSearchHit {
    pub user: SearchUser,
}

/// What search says about an account.
///
/// **Far less than [`WebProfileInfo`]**, and the gap is the point: there are no
/// counters and no `followed_by_viewer` here, so anything resolved this way has
/// to say "unknown" rather than fill a number in.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchUser {
    #[serde(deserialize_with = "flexible_pk")]
    pub pk: Pk,
    pub username: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub is_private: Option<bool>,
    #[serde(default)]
    pub is_verified: Option<bool>,
    #[serde(default)]
    pub profile_pic_url: Option<String>,
    /// What search says about the viewer's relationship to this account.
    ///
    /// The two fields that matter are the two `web_profile_info` spells
    /// `followed_by_viewer` and `requested_by_viewer`, so a private account is
    /// still refused before a page is walked when this route was the one that
    /// answered.
    #[serde(default)]
    pub friendship_status: Option<FriendshipStatus>,
}
