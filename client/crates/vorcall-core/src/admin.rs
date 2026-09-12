//! The moderation REST endpoints the server-settings screens read: invites, bans
//! and the member roster.
//!
//! Each is gated server-side — `MANAGE_INVITES` for the invite calls,
//! `BAN_MEMBERS` for the ban list, any bearer for the roster — and answers
//! `403` otherwise, which arrives as [`ApiFailure::Status`]. An invite's
//! plaintext code is never logged: the server shows it once, to whoever asked
//! for it.

use prost::Message as _;
use vorcall_proto::v1::{
    Ban, BanList, CreateInviteRequest, Invite, InviteCreated, InviteList, MemberList, Profile,
};

use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// Every invite ever created, used ones included.
pub async fn list_invites(
    endpoints: &Endpoints,
    access_token: &str,
) -> Result<Vec<Invite>, ApiFailure> {
    let url = http::api_url(endpoints, "/api/invites")?;
    let body = http::get_bytes(endpoints, Some(access_token), url).await?;
    let list = InviteList::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    tracing::debug!(count = list.invites.len(), "fetched the invite list");

    Ok(list.invites)
}

/// Creates one invite valid for `days` (the server accepts 1..365). The answer
/// is the only time its code exists outside the server's hash.
pub async fn create_invite(
    endpoints: &Endpoints,
    access_token: &str,
    days: u32,
) -> Result<InviteCreated, ApiFailure> {
    let url = http::api_url(endpoints, "/api/invites")?;
    let body = http::post_proto(
        endpoints,
        Some(access_token),
        url,
        &CreateInviteRequest { days },
    )
    .await?;
    let created = InviteCreated::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    tracing::debug!(id = created.id, days, "created an invite");

    Ok(created)
}

/// Revokes an unused invite.
pub async fn revoke_invite(
    endpoints: &Endpoints,
    access_token: &str,
    id: i64,
) -> Result<(), ApiFailure> {
    let url = http::api_url(endpoints, &format!("/api/invites/{id}"))?;
    http::delete(endpoints, access_token, url).await
}

pub async fn list_bans(endpoints: &Endpoints, access_token: &str) -> Result<Vec<Ban>, ApiFailure> {
    let url = http::api_url(endpoints, "/api/bans")?;
    let body = http::get_bytes(endpoints, Some(access_token), url).await?;
    let list = BanList::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    tracing::debug!(count = list.bans.len(), "fetched the ban list");

    Ok(list.bans)
}

/// Every member, ordered by username. The snapshot already carries these; this
/// is for a screen that wants them without a live connection.
pub async fn list_members(
    endpoints: &Endpoints,
    access_token: &str,
) -> Result<Vec<Profile>, ApiFailure> {
    let url = http::api_url(endpoints, "/api/users")?;
    let body = http::get_bytes(endpoints, Some(access_token), url).await?;
    let list = MemberList::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    tracing::debug!(count = list.members.len(), "fetched the member list");

    Ok(list.members)
}
