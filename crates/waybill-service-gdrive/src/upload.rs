//! 核心拥有持久化和过期重建决策。
use crate::{
    Gdrive,
    client::{ensure_success, protocol, read_json},
    object::{DriveFile, FILE_FIELDS, State},
};
use reqwest::{Response, StatusCode};
use serde_json::json;
use waybill::{
    BoxFuture,
    checkpoint::DriverState,
    content::Verification,
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    source::SourceIdentity,
    transfer::Receipt,
    upload::{SessionStatus, UploadIntent, UploadSink},
};
impl UploadSink for Gdrive {
    fn chunk_limits(&self) -> waybill::upload::UploadChunkLimits {
        waybill::upload::UploadChunkLimits {
            max_size: 8 * 1024 * 1024,
            alignment: 256 * 1024,
        }
    }

    fn identity(&self) -> ServiceIdentity {
        self.identity.clone()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            offset_upload: true,
            durable_upload: true,
            range_download: true,
            ..Capabilities::default()
        }
    }
    fn prepare<'a>(
        &'a self,
        intent: &'a UploadIntent,
        _source: &'a SourceIdentity,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move { self.plan_target(intent).await?.encode() })
    }
    fn probe<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus> {
        Box::pin(async move {
            let decoded = State::decode(state)?;
            if let Some(file) = self.api.get_file(&decoded.object_id).await? {
                return self.completed(intent, source, decoded, file);
            }
            match &decoded.session_uri {
                None => Ok(SessionStatus::Uninitialized(state.clone())),
                Some(uri) => {
                    self.api.validate_session(uri)?;
                    let response = self
                        .api
                        .request(
                            self.api
                                .http
                                .put(uri)
                                .header("Content-Length", "0")
                                .header("Content-Range", format!("bytes */{}", source.size))
                                .body(Vec::<u8>::new()),
                            true,
                        )
                        .await?;
                    self.response(intent, source, decoded, response).await
                }
            }
        })
    }
    fn initialize<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus> {
        Box::pin(async move {
            let mut decoded = State::decode(state)?;
            self.ensure_directories(&decoded).await?;
            if let Some(file) = self.api.get_file(&decoded.object_id).await? {
                return self.completed(intent, source, decoded, file);
            }
            self.check_collision(&decoded).await?;
            let metadata = json!({"id":decoded.object_id,"name":decoded.remote_name,"mimeType":"application/octet-stream","parents":[decoded.parent_id],"appProperties":self.properties(intent,source)});
            let response = if source.size == 0 {
                self.api
                    .request(
                        self.api
                            .http
                            .post(format!("{}/files", self.api.api_root))
                            .query(&[("fields", FILE_FIELDS)])
                            .json(&metadata),
                        true,
                    )
                    .await?
            } else {
                self.api
                    .request(
                        self.api
                            .http
                            .post(&self.api.upload_root)
                            .query(&[("uploadType", "resumable"), ("fields", FILE_FIELDS)])
                            .header("X-Upload-Content-Type", "application/octet-stream")
                            .header("X-Upload-Content-Length", source.size)
                            .json(&metadata),
                        true,
                    )
                    .await?
            };
            if response.status() == StatusCode::CONFLICT {
                let file = self
                    .api
                    .get_file(&decoded.object_id)
                    .await?
                    .ok_or_else(|| {
                        Error::new(ErrorKind::ResultUnknown, "Drive object creation unresolved")
                    })?;
                return self.completed(intent, source, decoded, file);
            }
            ensure_success(&response)?;
            if source.size == 0 {
                return self.completed(intent, source, decoded, read_json(response).await?);
            }
            let uri = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(protocol)?;
            self.api.validate_session(uri)?;
            decoded.session_uri = Some(uri.to_owned());
            Ok(SessionStatus::Ready {
                state: decoded.encode()?,
                offset: 0,
            })
        })
    }
    fn write_chunk<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
        offset: u64,
        data: Vec<u8>,
    ) -> BoxFuture<'a, SessionStatus> {
        Box::pin(async move {
            let decoded = State::decode(state)?;
            let uri = decoded.session_uri.as_deref().ok_or_else(protocol)?;
            self.api.validate_session(uri)?;
            let end = offset.checked_add(data.len() as u64).ok_or_else(protocol)?;
            if data.is_empty()
                || data.len() > 8 * 1024 * 1024
                || end > source.size
                || (end != source.size && !data.len().is_multiple_of(256 * 1024))
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "invalid Drive upload chunk",
                ));
            }
            let response = self
                .api
                .request(
                    self.api
                        .http
                        .put(uri)
                        .header("Content-Type", "application/octet-stream")
                        .header("Content-Length", data.len())
                        .header(
                            "Content-Range",
                            format!("bytes {}-{}/{}", offset, end - 1, source.size),
                        )
                        .body(data),
                    false,
                )
                .await?;
            self.response(intent, source, decoded, response).await
        })
    }
}
impl Gdrive {
    fn completed(
        &self,
        intent: &UploadIntent,
        source: &SourceIdentity,
        state: State,
        file: DriveFile,
    ) -> Result<SessionStatus> {
        self.verify_file(&file, &state, intent, source)?;
        Ok(SessionStatus::Complete {
            state: state.encode()?,
            receipt: Receipt {
                operation: intent.operation.clone(),
                service: self.identity.clone(),
                target: intent.target.clone(),
                object: file.id,
                size: source.size,
                // appProperties 是客户端声明，不能证明 Google 校验过 BLAKE3。
                verified: Verification::Length,
            },
        })
    }
    async fn response(
        &self,
        intent: &UploadIntent,
        source: &SourceIdentity,
        state: State,
        response: Response,
    ) -> Result<SessionStatus> {
        match response.status().as_u16() {
            200 | 201 => self.completed(intent, source, state, read_json(response).await?),
            404 | 410 => {
                // 过期不等于未完成；完成响应可能丢失，必须再次查询对象。
                if let Some(file) = self.api.get_file(&state.object_id).await? {
                    self.completed(intent, source, state, file)
                } else {
                    Ok(SessionStatus::Expired(state.encode()?))
                }
            }
            308 => {
                let offset = match response.headers().get("range") {
                    None => 0,
                    Some(range) => range
                        .to_str()
                        .ok()
                        .and_then(|v| v.strip_prefix("bytes=0-"))
                        .and_then(|v| v.parse::<u64>().ok())
                        .and_then(|v| v.checked_add(1))
                        .ok_or_else(protocol)?,
                };
                if offset > source.size {
                    return Err(protocol());
                }
                Ok(SessionStatus::Ready {
                    state: state.encode()?,
                    offset,
                })
            }
            _ => Err(protocol()),
        }
    }
}
