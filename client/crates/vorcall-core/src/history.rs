//! The read-only history endpoint: one page of a channel's messages. Who the
//! members are comes from the snapshot the Hello sequence sends, not from REST.

use prost::Message as _;
use vorcall_proto::v1::MessagePage;

use crate::endpoints::Endpoints;
use crate::http::{self, ApiFailure};

/// One page of `channel_id`, ascending by id. `before` is exclusive: without it
/// the page is the newest `limit` messages.
pub async fn fetch_page(
    endpoints: &Endpoints,
    access_token: &str,
    channel_id: i64,
    limit: u32,
    before: Option<i64>,
) -> Result<MessagePage, ApiFailure> {
    let mut url = http::api_url(endpoints, "/api/messages")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("channel", &channel_id.to_string());
        query.append_pair("limit", &limit.to_string());
        if let Some(before) = before {
            query.append_pair("before", &before.to_string());
        }
    }

    let body = http::get_bytes(endpoints, Some(access_token), url).await?;
    let page = MessagePage::decode(body).map_err(|e| ApiFailure::Malformed(e.to_string()))?;

    tracing::debug!(
        channel = channel_id,
        count = page.messages.len(),
        has_more = page.has_more,
        "fetched a history page"
    );

    Ok(page)
}
