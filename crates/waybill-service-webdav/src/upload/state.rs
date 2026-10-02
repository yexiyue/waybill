//! 私有上传记录：阶段携带对应条件版本，避免布尔组合形成非法状态。
use serde::{Deserialize, Serialize};
use waybill::{
    checkpoint::DriverState,
    error::{Error, ErrorKind, Result},
    service::ServiceIdentity,
    source::SourceIdentity,
    transfer::ConflictPolicy,
    upload::UploadIntent,
};

const VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(super) enum Phase {
    Prepared,
    Writing { etag: Option<String> },
    Staged { etag: String },
}
impl Phase {
    fn etag(&self) -> Option<&str> {
        match self {
            Self::Prepared => None,
            Self::Writing { etag } => etag.as_deref(),
            Self::Staged { etag } => Some(etag),
        }
    }
}
#[derive(Serialize, Deserialize)]
pub(super) struct State {
    binding: String,
    nonce: String,
    pub temporary: String,
    pub destination: String,
    #[serde(flatten)]
    pub phase: Phase,
}
impl State {
    pub fn new(
        service: &ServiceIdentity,
        intent: &UploadIntent,
        source: &SourceIdentity,
        destination: String,
    ) -> Result<Self> {
        let mut entropy = [0u8; 16];
        getrandom::getrandom(&mut entropy)
            .map_err(|_| Error::new(ErrorKind::Io, "upload nonce unavailable"))?;
        let nonce = entropy
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(Self {
            binding: binding(service, intent, source)?,
            temporary: temporary(&destination, &nonce),
            destination,
            nonce,
            phase: Phase::Prepared,
        })
    }
    pub fn decode(
        service: &ServiceIdentity,
        intent: &UploadIntent,
        source: &SourceIdentity,
        saved: &DriverState,
    ) -> Result<Self> {
        if saved.version != VERSION || saved.payload.len() > 16384 {
            return Err(invalid_state());
        }
        let state: Self = serde_json::from_slice(&saved.payload).map_err(|_| invalid_state())?;
        if state.binding != binding(service, intent, source)?
            || state.nonce.len() != 32
            || !state.nonce.bytes().all(|b| b.is_ascii_hexdigit())
            || state.temporary != temporary(&state.destination, &state.nonce)
            || (state.destination != intent.target
                && (intent.conflict != ConflictPolicy::OperationSuffix
                    || state.destination != suffixed(&intent.target, &intent.operation)))
            || state
                .phase
                .etag()
                .is_some_and(|tag| !crate::download::strong_etag(tag))
        {
            return Err(invalid_state());
        }
        Ok(state)
    }
    pub fn encode(&self) -> Result<DriverState> {
        Ok(DriverState {
            version: VERSION,
            payload: serde_json::to_vec(self).map_err(|_| invalid_state())?,
        })
    }
    pub fn marker(&self) -> String {
        format!("{}:{}", self.nonce, self.binding)
    }
}
fn binding(
    service: &ServiceIdentity,
    intent: &UploadIntent,
    source: &SourceIdentity,
) -> Result<String> {
    let bytes = serde_json::to_vec(&(service, intent, source)).map_err(|_| invalid_state())?;
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
pub(super) fn invalid_state() -> Error {
    Error::new(ErrorKind::Checkpoint, "invalid WebDAV upload state")
}
