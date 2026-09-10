//! The read-only REST endpoints: one page of a room's messages, and the roster
//! the sidebar shows offline members from.

use prost::Message as _;
use reqwest::header::AUTHORIZATION;
use vorcall_proto::v1::{Member, MessagePage, UserList};

use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// One page of `room_id`, ascending by id. `before` is exclusive: without it
/// the page is the newest `limit` messages.
pub async fn fetch_page(
    endpoints: &Endpoints,
    access_token: &str,
    room_id: &str,
    limit: u32,
    before: Option<i64>,
) -> Result<MessagePage, ApiFailure> {
    let mut url = endpoints
        .http_base
        .join("/api/messages")
        .map_err(|e| ApiFailure::Malformed(e.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("room", room_id);
        query.append_pair("limit", &limit.to_string());
        if let Some(before) = before {
            query.append_pair("before", &before.to_string());
        }
    }

    let body = get(endpoints, url, access_token).await?;
    let page = MessagePage::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    tracing::debug!(
        room = %room_id,
        count = page.messages.len(),
        has_more = page.has_more,
        "fetched a history page"
    );

    Ok(page)
}

/// Every registered user, ordered by username; who is online comes from the
/// presence frames instead.
pub async fn fetch_users(
    endpoints: &Endpoints,
    access_token: &str,
) -> Result<Vec<Member>, ApiFailure> {
    let url = endpoints
        .http_base
        .join("/api/users")
        .map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    let body = get(endpoints, url, access_token).await?;
    let users = UserList::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    tracing::debug!(count = users.users.len(), "fetched the user list");

    Ok(users.users)
}

async fn get(
    endpoints: &Endpoints,
    url: url::Url,
    access_token: &str,
) -> Result<prost::bytes::Bytes, ApiFailure> {
    let response = http::client()?
        .get(url)
        .header(http::KEY_HEADER, &endpoints.key)
        .header(AUTHORIZATION, http::bearer(access_token))
        .send()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))?;

    if !response.status().is_success() {
        return Err(http::failure_from(response).await);
    }

    response
        .bytes()
        .await
        .map_err(|e| ApiFailure::Transport(e.to_string()))
}
