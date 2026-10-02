//! HTTP 正文上界与请求期限；转发宿主 transporter，保留其 TLS / 凭证配置。
use futures_util::StreamExt;
use http::{Request, Response};
use opendal::{Buffer, Error, ErrorKind, HttpBody, HttpTransport, HttpTransporter, Result};
use std::time::Duration;
pub(crate) struct BoundedTransport(pub HttpTransporter);
impl HttpTransport for BoundedTransport {
    async fn fetch(&self, request: Request<Buffer>) -> Result<Response<HttpBody>> {
        let range = request
            .headers()
            .get("range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let read_etag = request
            .headers()
            .get("if-match")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let method = request.method().clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let response = tokio::time::timeout_at(deadline, self.0.fetch(request))
            .await
            .map_err(|_| timeout())??;
        let (parts, body) = response.into_parts();
        let limit = if method == http::Method::GET && parts.status.is_success() && range.is_some() {
            let range = range
                .as_deref()
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.split_once('-'))
                .and_then(|(start, end)| {
                    Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?))
                })
                .filter(|(start, end)| end >= start)
                .ok_or_else(protocol)?;
            let length = range
                .1
                .checked_sub(range.0)
                .and_then(|v| v.checked_add(1))
                .ok_or_else(protocol)?;
            // 后端不得把忽略 Range 的整文件响应送入 reader；ETag 条件必须绑定响应版本。
            if parts.status != http::StatusCode::PARTIAL_CONTENT
                || length > 8 * 1024 * 1024
                || parts
                    .headers
                    .get("content-encoding")
                    .is_some_and(|v| v != "identity")
                || parts
                    .headers
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .is_none_or(|v| {
                        let prefix = format!("bytes {}-{}/", range.0, range.1);
                        !v.strip_prefix(&prefix)
                            .is_some_and(|total| total.parse::<u64>().is_ok_and(|n| n > range.1))
                    })
                || read_etag.as_deref().is_some_and(|tag| {
                    parts.headers.get("etag").and_then(|v| v.to_str().ok()) != Some(tag)
                })
            {
                return Err(protocol());
            }
            length
        } else if method == http::Method::HEAD {
            0
        } else {
            1024 * 1024
        };
        let body = body.map_inner(|stream| {
            Box::new(Box::pin(futures_util::stream::try_unfold(
                (stream, 0u64),
                move |(mut stream, consumed)| async move {
                    let next = tokio::time::timeout_at(deadline, stream.next())
                        .await
                        .map_err(|_| timeout())?;
                    let Some(item) = next else {
                        return Ok(None);
                    };
                    let item = item?;
                    let consumed = consumed
                        .checked_add(item.len() as u64)
                        .ok_or_else(protocol)?;
                    if consumed > limit {
                        return Err(protocol());
                    }
                    Ok(Some((item, (stream, consumed))))
                },
            )))
        });
        Ok(Response::from_parts(parts, body))
    }
}
fn protocol() -> Error {
    Error::new(ErrorKind::Unexpected, "bounded object response invalid")
}
fn timeout() -> Error {
    Error::new(ErrorKind::Unexpected, "object request timed out").set_temporary()
}
