//! Models of Instagram's responses.
//!
//! Everything but `pk` is optional on purpose. This is a private API with no
//! contract: it adds and removes fields without notice, and one unexpected
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

impl UserSummary {
    pub fn profile_url(&self) -> String {
        format!("https://www.instagram.com/{}/", self.username)
    }
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
    /// What Instagram claims the total is. Useful for estimating progress and
    /// nothing else: there are documented cases of it lying.
    #[serde(default)]
    pub big_list: Option<bool>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct WebProfileInfo {
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
        Raw::Number(n) => Ok(n),
        Raw::Text(s) => s.parse().map_err(D::Error::custom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_accepted_as_a_number_or_as_text() {
        let as_text: UserSummary = serde_json::from_str(r#"{"pk":"123","username":"a"}"#).unwrap();
        let as_number: UserSummary = serde_json::from_str(r#"{"pk":123,"username":"a"}"#).unwrap();
        assert_eq!(as_text.pk, 123);
        assert_eq!(as_number.pk, 123);
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
        assert_eq!(u.id, 42);
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
            pk: 7,
            username: "someone".into(),
            full_name: Some("Some One".into()),
            is_private: Some(false),
            is_verified: Some(true),
            profile_pic_url: Some("https://example/pic.jpg".into()),
        };
        let domain: snob_core::model::User = (&wire).into();
        assert_eq!(domain.pk, 7);
        assert_eq!(domain.pfp_url.as_deref(), Some("https://example/pic.jpg"));
    }
}
