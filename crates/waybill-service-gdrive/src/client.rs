//! 错误只保留安全的静态上下文。
use crate::{
    credential::TokenProvider,
    object::{DriveFile, FILE_FIELDS, FileList, validate_id},
};
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use std::{sync::Arc, time::Duration};
use waybill::error::{Error, ErrorKind, Result};
const METADATA_LIMIT: usize = 1024 * 1024;
/// 列表与元数据共用的字段集；下载侧需要 md5 / version / modifiedTime。
pub(crate) const LIST_FIELDS: &str = "nextPageToken,files(id,name,size,parents,appProperties,trashed,mimeType,md5Checksum,version,modifiedTime)";
pub(crate) struct DriveClient {
    pub(crate) http: reqwest::Client,
    credentials: Arc<dyn TokenProvider>,
    pub(crate) api_root: String,
    pub(crate) about_url: String,
    pub(crate) upload_root: String,
    #[cfg(test)]
    pub(crate) test_origin: Option<url::Url>,
}
impl DriveClient {
    pub(crate) fn new(credentials: Arc<dyn TokenProvider>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(60))
                .build()
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "Drive HTTP client"))?,
            credentials,
            api_root: "https://www.googleapis.com/drive/v3".into(),
            about_url: "https://www.googleapis.com/drive/v2/about".into(),
            upload_root: "https://www.googleapis.com/upload/drive/v3/files".into(),
            #[cfg(test)]
            test_origin: None,
        })
    }
    pub(crate) fn validate_session(&self, uri: &str) -> Result<()> {
        let url = url::Url::parse(uri).map_err(|_| protocol())?;
        let production = url.scheme() == "https"
            && url.host_str() == Some("www.googleapis.com")
            && url.port_or_known_default() == Some(443)
            && url.path() == "/upload/drive/v3/files";
        #[cfg(test)]
        let production = production
            || self.test_origin.as_ref().is_some_and(|base| {
                base.origin() == url.origin() && url.path() == "/upload/drive/v3/files"
            });
        if !production
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(protocol());
        }
        Ok(())
    }
    pub(crate) async fn request(
        &self,
        request: reqwest::RequestBuilder,
        idempotent: bool,
    ) -> Result<Response> {
        self.dispatch(request, idempotent, true).await
    }
    /// `authorized = false` 供跨主机媒体 URL 使用：不获取也不携带凭证。
    pub(crate) async fn dispatch(
        &self,
        request: reqwest::RequestBuilder,
        idempotent: bool,
        authorized: bool,
    ) -> Result<Response> {
        let mut token = if authorized {
            Some(
                self.credentials
                    .access_token(Duration::from_secs(90))
                    .await?,
            )
        } else {
            None
        };
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            let mut builder = request.try_clone().ok_or_else(protocol)?;
            if let Some(token) = &token {
                builder = builder.bearer_auth(token.secret());
            }
            let response = builder.send().await;
            match response {
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    if !authorized {
                        // 签名媒体 URL 拒绝通常意味着过期；重新解析即可恢复。
                        return Err(Error::new(ErrorKind::Retryable, "Drive media URL rejected"));
                    }
                    let current = token.as_ref().ok_or_else(protocol)?;
                    if refreshed {
                        self.credentials.reconnect_required(current).await?;
                        return Err(Error::new(
                            ErrorKind::Authentication,
                            "Drive authorization rejected",
                        ));
                    }
                    token = Some(self.credentials.after_rejection(current).await?);
                    refreshed = true;
                }
                Ok(response)
                    if response.status() == StatusCode::TOO_MANY_REQUESTS
                        || response.status().is_server_error() =>
                {
                    if !idempotent || attempt >= 3 {
                        return Err(Error::new(ErrorKind::Retryable, "Drive transient response"));
                    }
                    let seconds = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(1 << attempt)
                        .min(30);
                    drop(response);
                    self.delay(seconds).await;
                    attempt += 1;
                }
                Ok(response) if response.status() == StatusCode::FORBIDDEN => {
                    let body: serde_json::Value = read_json(response).await?;
                    let limited = body
                        .pointer("/error/errors")
                        .and_then(|v| v.as_array())
                        .is_some_and(|errors| {
                            errors.iter().any(|e| {
                                matches!(
                                    e.get("reason").and_then(|v| v.as_str()),
                                    Some("rateLimitExceeded" | "userRateLimitExceeded")
                                )
                            })
                        });
                    if !limited {
                        return Err(Error::new(
                            ErrorKind::Authentication,
                            "Drive permission denied",
                        ));
                    }
                    if !idempotent || attempt >= 3 {
                        return Err(Error::new(ErrorKind::Retryable, "Drive rate limited"));
                    }
                    self.delay(1 << attempt).await;
                    attempt += 1;
                }
                Ok(response) => return Ok(response),
                Err(_) if idempotent && attempt < 3 => {
                    self.delay(1 << attempt).await;
                    attempt += 1;
                }
                Err(_) => return Err(Error::new(ErrorKind::Retryable, "Drive transport failure")),
            }
        }
    }
    async fn delay(&self, seconds: u64) {
        #[cfg(test)]
        if self.test_origin.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }
    /// drive.file 不保证根目录能经 files.get 读取；v2 about 可返回真实根 ID。
    /// https://developers.google.com/workspace/drive/api/reference/rest/v2/about/get
    pub(crate) async fn root_folder_id(&self) -> Result<String> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct About {
            root_folder_id: String,
        }
        let response = self
            .request(
                self.http
                    .get(&self.about_url)
                    .query(&[("fields", "rootFolderId")]),
                true,
            )
            .await?;
        ensure_success(&response)?;
        let id = read_json::<About>(response).await?.root_folder_id;
        validate_id(&id)?;
        if id == "root" {
            return Err(protocol());
        }
        Ok(id)
    }

    pub(crate) async fn get_file(&self, id: &str) -> Result<Option<DriveFile>> {
        validate_id(id)?;
        let response = self
            .request(
                self.http
                    .get(format!("{}/files/{id}", self.api_root))
                    .query(&[("fields", FILE_FIELDS)]),
                true,
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure_success(&response)?;
        let file: DriveFile = read_json(response).await?;
        Ok((!file.trashed).then_some(file))
    }
    pub(crate) async fn find_files(&self, query: &str) -> Result<Vec<DriveFile>> {
        let mut files = Vec::new();
        let mut page = String::new();
        loop {
            let response = self
                .request(
                    self.http.get(format!("{}/files", self.api_root)).query(&[
                        ("q", query),
                        ("fields", LIST_FIELDS),
                        ("pageSize", "100"),
                        ("pageToken", &page),
                    ]),
                    true,
                )
                .await?;
            ensure_success(&response)?;
            let result: FileList = read_json(response).await?;
            if files.len() + result.files.len() > 1000 {
                return Err(Error::new(ErrorKind::ResourceBusy, "Drive listing bound"));
            }
            files.extend(result.files);
            match result.next_page_token {
                Some(next) if files.len() < 1000 && next != page && next.len() <= 8192 => {
                    page = next
                }
                Some(_) => return Err(protocol()),
                None => return Ok(files),
            }
        }
    }
    /// 有界范围读取媒体内容。媒体 URL 常以 302 指向内容 CDN：
    /// 手动跟随白名单内的 Google 域名，跨主机跳转不携带凭证，
    /// 与会话 URL 校验同一风格且不依赖 reqwest 的重定向策略。
    pub(crate) async fn read_media_range(
        &self,
        id: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        validate_id(id)?;
        if length == 0 || length > 8 * 1024 * 1024 {
            return Err(Error::new(ErrorKind::InvalidInput, "media range bound"));
        }
        let end = offset
            .checked_add(length as u64)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "media range bound"))?;
        let mut url = format!("{}/files/{id}?alt=media", self.api_root);
        let mut authorized = true;
        let mut redirects = 0;
        loop {
            let response = self
                .dispatch(
                    self.http
                        .get(&url)
                        .header("Range", format!("bytes={offset}-{end}")),
                    true,
                    authorized,
                )
                .await?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                redirects += 1;
                if redirects > 5 {
                    return Err(protocol());
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(protocol)?;
                let (next, same_host) = self.media_target(location)?;
                url = next;
                authorized = same_host;
                continue;
            }
            match status {
                206 => {
                    let header = response
                        .headers()
                        .get("content-range")
                        .and_then(|value| value.to_str().ok())
                        .ok_or_else(protocol)?;
                    if !header.starts_with(&format!("bytes {offset}-{end}/")) {
                        return Err(protocol());
                    }
                    return read_media_body(response, length).await;
                }
                200 => {
                    // 服务器忽略 Range；仅当请求覆盖整个文件时才可接受，
                    // 超出请求长度的响应按协议错误拒绝，不静默丢弃数据。
                    if offset != 0 {
                        return Err(protocol());
                    }
                    return read_media_body(response, length).await;
                }
                404 => {
                    return Err(Error::new(ErrorKind::NotFound, "Drive media unavailable"));
                }
                416 => {
                    return Err(Error::new(
                        ErrorKind::SourceChanged,
                        "media range beyond object",
                    ));
                }
                _ => {
                    ensure_success(&response)?;
                    return Err(protocol());
                }
            }
        }
    }
    /// 校验重定向目标并返回是否与 API 同源（同源保留凭证）。
    fn media_target(&self, location: &str) -> Result<(String, bool)> {
        let url = url::Url::parse(location).map_err(|_| protocol())?;
        let Some(host) = url.host_str() else {
            return Err(protocol());
        };
        let production = url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && (host == "www.googleapis.com"
                || host == "drive.google.com"
                || host.ends_with(".googleusercontent.com"));
        #[cfg(test)]
        let production = production
            || self
                .test_origin
                .as_ref()
                .is_some_and(|base| base.host_str() == Some(host) && base.scheme() == url.scheme());
        if !production || !url.username().is_empty() || url.password().is_some() {
            return Err(protocol());
        }
        let same_origin =
            url::Url::parse(&self.api_root).is_ok_and(|base| base.origin() == url.origin());
        Ok((url.to_string(), same_origin))
    }
    pub(crate) async fn generate_id(&self) -> Result<String> {
        let response = self
            .request(
                self.http
                    .get(format!("{}/files/generateIds", self.api_root))
                    .query(&[("count", "1"), ("space", "drive"), ("type", "files")]),
                true,
            )
            .await?;
        ensure_success(&response)?;
        #[derive(serde::Deserialize)]
        struct Ids {
            ids: Vec<String>,
        }
        let id = read_json::<Ids>(response)
            .await?
            .ids
            .into_iter()
            .next()
            .ok_or_else(protocol)?;
        validate_id(&id)?;
        Ok(id)
    }
}
pub(crate) async fn read_json<T: DeserializeOwned>(mut response: Response) -> Result<T> {
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::new(ErrorKind::Retryable, "Drive response transport"))?
    {
        if chunk.len() > METADATA_LIMIT - data.len() {
            return Err(Error::new(ErrorKind::Protocol, "Drive metadata bound"));
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| protocol())
}
pub(crate) async fn read_media_body(mut response: Response, length: usize) -> Result<Vec<u8>> {
    let mut data = Vec::with_capacity(length);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::new(ErrorKind::Retryable, "Drive response transport"))?
    {
        if chunk.len() > length - data.len() {
            return Err(protocol());
        }
        data.extend_from_slice(&chunk);
    }
    if data.len() != length {
        return Err(protocol());
    }
    Ok(data)
}
pub(crate) fn ensure_success(response: &Response) -> Result<()> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(Error::new(
            if response.status() == StatusCode::CONFLICT {
                ErrorKind::Conflict
            } else {
                ErrorKind::Protocol
            },
            "Drive request status",
        ))
    }
}
pub(crate) fn protocol() -> Error {
    Error::new(ErrorKind::Protocol, "invalid Drive protocol response")
}
