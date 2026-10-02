//! 私有状态只保存对象引用与对账身份，不序列化凭证或 OpenDAL multipart 会话。
use serde::{Deserialize, Serialize};
use waybill::{
    checkpoint::DriverState,
    error::{Error, ErrorKind, Result},
    service::ServiceIdentity,
    source::SourceIdentity,
    transfer::ConflictPolicy,
    upload::UploadIntent,
};
#[derive(Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(super) enum Phase {
    Prepared,
    Writing,
    Staged {
        etag: String,
        version: Option<String>,
        read_limit: usize,
    },
}
#[derive(Serialize, Deserialize)]
pub(super) struct State {
    binding: String,
    nonce: String,
    pub temporary: String,
    pub destination: String,
    pub phase: Phase,
}
impl State {
    pub fn new(
        service: &ServiceIdentity,
        intent: &UploadIntent,
        source: &SourceIdentity,
        destination: String,
    ) -> Result<Self> {
        let nonce = nonce()?;
        Ok(Self {
            binding: binding(service, intent, source)?,
            temporary: temporary(&destination, &nonce),
            destination,
            nonce,
            phase: Phase::Prepared,
        })
    }
    pub fn restart(&mut self) -> Result<()> {
        self.nonce = nonce()?;
        self.temporary = temporary(&self.destination, &self.nonce);
        self.phase = Phase::Writing;
        Ok(())
    }
    pub fn decode(
        service: &ServiceIdentity,
        intent: &UploadIntent,
        source: &SourceIdentity,
        saved: &DriverState,
    ) -> Result<Self> {
        if saved.version != 1 || saved.payload.len() > 16384 {
            return Err(invalid());
        }
        let state: Self = serde_json::from_slice(&saved.payload).map_err(|_| invalid())?;
        if state.binding != binding(service, intent, source)?
            || state.nonce.len() != 32
            || !state.nonce.bytes().all(|c| c.is_ascii_hexdigit())
            || state.temporary != temporary(&state.destination, &state.nonce)
            || (state.destination != intent.target
                && (intent.conflict != ConflictPolicy::OperationSuffix
                    || state.destination != suffixed(&intent.target, &intent.operation)))
        {
            return Err(invalid());
        }
        if let Phase::Staged {
            etag,
            version,
            read_limit,
        } = &state.phase
            && (*read_limit == 0
                || *read_limit > 8 * 1024 * 1024
                || !bounded(etag)
                || version.as_deref().is_some_and(|v| !bounded(v)))
        {
            return Err(invalid());
        }
        Ok(state)
    }
    pub fn encode(&self) -> Result<DriverState> {
        Ok(DriverState {
            version: 1,
            payload: serde_json::to_vec(self).map_err(|_| invalid())?,
        })
    }
    pub fn marker(&self) -> String {
        format!("{}:{}", self.nonce, self.binding)
    }
}
fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| Error::new(ErrorKind::Io, "object upload nonce unavailable"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn binding(
    service: &ServiceIdentity,
    intent: &UploadIntent,
    source: &SourceIdentity,
) -> Result<String> {
    let bytes = serde_json::to_vec(&(service, intent, source)).map_err(|_| invalid())?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}
fn temporary(destination: &str, nonce: &str) -> String {
    let parent = destination
        .rsplit_once('/')
        .map(|(parent, _)| format!("{parent}/"))
        .unwrap_or_default();
    format!("{parent}.waybill-{nonce}.part")
}
pub(super) fn suffixed(target: &str, operation: &str) -> String {
    let suffix = blake3::hash(operation.as_bytes()).to_hex();
    match target.rsplit_once('.') {
        Some((stem, extension)) if !extension.contains('/') => {
            format!("{stem}-{}.{extension}", &suffix[..12])
        }
        _ => format!("{target}-{}", &suffix[..12]),
    }
}
fn invalid() -> Error {
    Error::new(ErrorKind::Checkpoint, "invalid object upload state")
}
