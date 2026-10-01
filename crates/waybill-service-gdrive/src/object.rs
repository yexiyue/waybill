//! 仅保留通用操作属性。
use crate::{Gdrive, client::protocol};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use waybill::{
    checkpoint::DriverState,
    error::{Error, ErrorKind, Result},
    source::SourceIdentity,
    upload::UploadIntent,
};
pub(crate) const FILE_FIELDS: &str =
    "id,name,size,parents,appProperties,trashed,mimeType,md5Checksum,version,modifiedTime";
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DriveFile {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) size: Option<String>,
    #[serde(default)]
    pub(crate) parents: Vec<String>,
    #[serde(default)]
    pub(crate) app_properties: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) trashed: bool,
    #[serde(default)]
    pub(crate) mime_type: String,
    /// 服务器计算的内容摘要；下载侧作为预期证据使用。
    #[serde(default)]
    pub(crate) md5_checksum: Option<String>,
    /// 内容版本号；随内容变化递增。
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) modified_time: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileList {
    pub(crate) files: Vec<DriveFile>,
    pub(crate) next_page_token: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Directory {
    pub(crate) id: String,
    pub(crate) parent: String,
    pub(crate) name: String,
    pub(crate) key: String,
}
// 私有类型不派生 Debug，session_uri 是能力 URL。
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct State {
    pub(crate) object_id: String,
    pub(crate) parent_id: String,
    pub(crate) remote_name: String,
    pub(crate) directories: Vec<Directory>,
    pub(crate) session_uri: Option<String>,
}
impl State {
    pub(crate) fn decode(state: &DriverState) -> Result<Self> {
        if state.version != 1 {
            return Err(Error::new(
                ErrorKind::IncompatibleVersion,
                "Drive state version",
            ));
        }
        if state.payload.len() > 256 * 1024 {
            return Err(protocol());
        }
        let decoded: Self = serde_json::from_slice(&state.payload).map_err(|_| protocol())?;
        validate_id(&decoded.object_id)?;
        validate_id(&decoded.parent_id)?;
        if decoded.remote_name.is_empty() || decoded.directories.len() > 32 {
            return Err(protocol());
        }
        for dir in &decoded.directories {
            validate_id(&dir.id)?;
            validate_id(&dir.parent)?;
        }
        Ok(decoded)
    }
    pub(crate) fn encode(&self) -> Result<DriverState> {
        Ok(DriverState {
            version: 1,
            payload: serde_json::to_vec(self).map_err(|_| protocol())?,
        })
    }
}
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "invalid Drive object id",
        ));
    }
    Ok(())
}
pub(crate) fn escape_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}
pub(crate) fn operation_key(intent: &UploadIntent) -> String {
    blake3::hash(intent.operation.as_bytes())
        .to_hex()
        .to_string()
}
impl Gdrive {
    pub(crate) fn properties(
        &self,
        intent: &UploadIntent,
        source: &SourceIdentity,
    ) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("waybill_operation".into(), operation_key(intent)),
            ("waybill_instance".into(), self.identity.instance.clone()),
            (
                "waybill_target".into(),
                blake3::hash(intent.target.as_bytes()).to_hex().to_string(),
            ),
            ("waybill_blake3".into(), source.blake3.clone()),
        ])
    }
    pub(crate) fn verify_file(
        &self,
        file: &DriveFile,
        state: &State,
        intent: &UploadIntent,
        source: &SourceIdentity,
    ) -> Result<()> {
        if file.id != state.object_id
            || file.trashed
            || !file.parents.contains(&state.parent_id)
            || file.name != state.remote_name
            || self
                .properties(intent, source)
                .iter()
                .any(|(k, v)| file.app_properties.get(k) != Some(v))
            || file.size.as_deref().and_then(|s| s.parse::<u64>().ok()) != Some(source.size)
        {
            return Err(Error::new(
                ErrorKind::ResultUnknown,
                "Drive completion identity mismatch",
            ));
        }
        Ok(())
    }
}
