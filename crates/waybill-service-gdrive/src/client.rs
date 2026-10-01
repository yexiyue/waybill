//! 衍生于 SwarmDrop DriveClient，MIT；错误只保留安全的静态上下文。
use crate::{
    credential::TokenProvider,
    object::{DriveFile, FILE_FIELDS, FileList, validate_id},
};
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use std::{sync::Arc, time::Duration};
use waybill::error::{Error, ErrorKind, Result};
const METADATA_LIMIT: usize = 1024 * 1024;
pub(crate) struct DriveClient {
    pub(crate) http: reqwest::Client,
    credentials: Arc<dyn TokenProvider>,
    pub(crate) api_root: String,
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
        retry_safe: bool,
    ) -> Result<Response> {
        let mut token = self
            .credentials
            .access_token(Duration::from_secs(90))
            .await?;
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            let response = request
                .try_clone()
                .ok_or_else(protocol)?
                .bearer_auth(token.secret())
                .send()
                .await;
            match response {
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    if refreshed {
                        self.credentials.reconnect_required(&token).await?;
                        return Err(Error::new(
                            ErrorKind::Authentication,
                            "Drive authorization rejected",
                        ));
                    }
                    token = self.credentials.after_rejection(&token).await?;
                    refreshed = true;
                }
                Ok(response)
                    if response.status() == StatusCode::TOO_MANY_REQUESTS
                        || response.status().is_server_error() =>
                {
                    if !retry_safe || attempt >= 3 {
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
                    if !retry_safe || attempt >= 3 {
                        return Err(Error::new(ErrorKind::Retryable, "Drive rate limited"));
                    }
                    self.delay(1 << attempt).await;
                    attempt += 1;
                }
                Ok(response) => return Ok(response),
                Err(_) if retry_safe && attempt < 3 => {
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
            let response = self.request(self.http.get(format!("{}/files", self.api_root)).query(&[("q", query), ("fields", "nextPageToken,files(id,name,size,parents,appProperties,trashed,mimeType)"), ("pageSize", "100"), ("pageToken", &page)]), true).await?;
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
