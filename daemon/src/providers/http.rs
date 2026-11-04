use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::header::HeaderValue;
use reqwest::{Client, Response};

use crate::feed::ErrorCode;

pub(crate) fn build_client(
    request_timeout: Duration,
    connect_timeout: Duration,
) -> Result<Client, ErrorCode> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(request_timeout)
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|_| ErrorCode::ProviderError)
}

pub(crate) fn parse_retry_after(
    value: Option<&HeaderValue>,
    received_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let value = value?.to_str().ok()?.trim();
    let deadline = if let Ok(seconds) = value.parse::<u64>() {
        let seconds = i64::try_from(seconds).ok()?;
        received_at.checked_add_signed(ChronoDuration::try_seconds(seconds)?)?
    } else {
        DateTime::<Utc>::from(httpdate::parse_http_date(value).ok()?)
    };
    (deadline > received_at).then_some(deadline)
}

pub(crate) async fn read_bounded_body(
    mut response: Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ErrorCode> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(ErrorCode::ProviderError);
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ErrorCode::NetworkError)?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ErrorCode::ProviderError);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
