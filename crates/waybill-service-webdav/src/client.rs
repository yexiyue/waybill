//! HTTP 请求、认证和有界响应；不记录 URL、密码或服务端错误正文。
use crate::{
    WebdavConfig,
    credential::{Authentication, CredentialProvider},
    path::Paths,
};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use std::{sync::Arc, time::Duration};
use waybill::error::{Error, ErrorKind, Result};

pub(crate) struct DavClient {
    pub paths: Paths,
    http: Client,
    credentials: Arc<dyn CredentialProvider>,
    account: String,
    upload_timeout: Duration,
}
impl DavClient {
    pub fn account(&self) -> &str {
        &self.account
    }
    pub fn at_root(&self, paths: Paths) -> Self {
        Self {
            paths,
            http: self.http.clone(),
            credentials: Arc::clone(&self.credentials),
            account: self.account.clone(),
            upload_timeout: self.upload_timeout,
        }
    }
    pub fn new(
        paths: Paths,
        config: WebdavConfig,
        credentials: Arc<dyn CredentialProvider>,
    ) -> Result<Self> {
        for timeout in [config.request_timeout, config.upload_timeout] {
            if timeout.is_zero() || timeout > Duration::from_secs(24 * 3600) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "invalid WebDAV timeout",
                ));
            }
        }
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(config.request_timeout)
            .build()
            .map_err(network)?;
        Ok(Self {
            paths,
            http,
            credentials,
            account: config.account,
            upload_timeout: config.upload_timeout,
        })
    }
    pub async fn request(&self, method: Method, reference: &str) -> Result<RequestBuilder> {
        let url = self.paths.url(reference)?;
        let credentials = self.credentials.credentials().await?;
        if credentials.authentication != Authentication::Anonymous
            && credentials.username != self.account
        {
            return Err(Error::new(
                ErrorKind::IdentityMismatch,
                "WebDAV credential principal changed",
            ));
        }
        let mut request = self
            .http
            .request(method.clone(), url.clone())
            .header("Accept-Encoding", "identity");
        if method == Method::PUT {
            request = request.timeout(self.upload_timeout);
        }
        match credentials.authentication {
            Authentication::Anonymous => Ok(request),
            Authentication::Basic => {
                Ok(request.basic_auth(&credentials.username, Some(&credentials.password)))
            }
            Authentication::Digest => {
                // 只用安全的 HEAD 获取 challenge；绝不为了认证先发送空 PUT / MOVE。
                // 部分服务器在 HEAD 401 后错误地发送正文；挑战连接不复用，避免污染连接池。
                let challenge = self
                    .http
                    .head(url.clone())
                    .header("Connection", "close")
                    .send()
                    .await
                    .map_err(network)?;
                if challenge.status() != StatusCode::UNAUTHORIZED {
                    return Err(auth());
                }
                let header = challenge
                    .headers()
                    .get_all("www-authenticate")
                    .iter()
                    .filter_map(|value| value.to_str().ok())
                    .find(|value| value.starts_with("Digest "))
                    .ok_or_else(auth)?;
                let mut digest = digest_auth::parse(header).map_err(|_| auth())?;
                if digest
                    .qop
                    .as_ref()
                    .is_some_and(|qop| !qop.contains(&digest_auth::Qop::AUTH))
                {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "Digest auth-int requires body hashing",
                    ));
                }
                let mut context = digest_auth::AuthContext::new(
                    &credentials.username,
                    &credentials.password,
                    url.path(),
                );
                context.method = digest_auth::HttpMethod::from(method.as_str());
                let header = digest
                    .respond(&context)
                    .map_err(|_| auth())?
                    .to_header_string();
                Ok(request.header("Authorization", header))
            }
        }
    }
    pub async fn head(&self, reference: &str) -> Result<Response> {
        let response = self
            .request(Method::HEAD, reference)
            .await?
            .send()
            .await
            .map_err(network)?;
        success(&response)?;
        Ok(response)
    }
}
pub(crate) fn success(response: &Response) -> Result<()> {
    if response.status().is_success() {
        return Ok(());
    }
    Err(Error::new(
        match response.status().as_u16() {
            401 | 403 => ErrorKind::Authentication,
            404 | 410 => ErrorKind::NotFound,
            405 | 501 => ErrorKind::Unsupported,
            409 | 412 | 423 => ErrorKind::Conflict,
            408 | 429 | 500..=599 => ErrorKind::Retryable,
            _ => ErrorKind::Protocol,
        },
        "WebDAV request rejected",
    ))
}
pub(crate) async fn bounded(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::new(
            ErrorKind::Protocol,
            "WebDAV response exceeds bound",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(Error::new(
                ErrorKind::Protocol,
                "WebDAV response exceeds bound",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub(crate) fn network(error: reqwest::Error) -> Error {
    Error::new(ErrorKind::Retryable, "WebDAV transport failed").with_source(error.without_url())
}
fn auth() -> Error {
    Error::new(ErrorKind::Authentication, "WebDAV authentication failed")
}

#[cfg(test)]
mod tests;
