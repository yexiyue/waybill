//! 目录 ID 在核心保存状态后才创建；创建响应丢失可按同一 ID 对账。
use crate::{
    Gdrive,
    client::{ensure_success, protocol},
    object::{Directory, State, escape_query, operation_key},
};
use serde_json::json;
use waybill::{
    error::{Error, ErrorKind, Result},
    transfer::ConflictPolicy,
    upload::UploadIntent,
};
impl Gdrive {
    /// 解析配置根目录为已验证的文件夹 ID；root 别名经 v2 about 取真实 ID。
    pub(crate) async fn root_folder(&self) -> Result<String> {
        if self.config.root == "root" {
            return self.api.root_folder_id().await;
        }
        let root = self
            .api
            .get_file(&self.config.root)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Drive root unavailable"))?;
        if root.mime_type != "application/vnd.google-apps.folder" {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Drive root is not a folder",
            ));
        }
        if root.id != self.config.root {
            return Err(Error::new(
                ErrorKind::IdentityMismatch,
                "Drive root identity changed",
            ));
        }
        Ok(root.id)
    }
    pub(crate) async fn plan_target(&self, intent: &UploadIntent) -> Result<State> {
        intent.validate()?;
        if !waybill::object::valid_object_path(&intent.target) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid Drive target path",
            ));
        }
        let mut parts: Vec<&str> = intent.target.split('/').collect();
        let name = parts.pop().ok_or_else(protocol)?;
        if parts.len() > 32 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "Drive directory depth bound",
            ));
        }
        let mut parent = self.root_folder().await?;
        let mut directories = Vec::new();
        let mut parent_exists = true;
        for name in parts {
            let key = blake3::hash(
                format!("{}\0{}\0{}", self.identity.instance, parent, name).as_bytes(),
            )
            .to_hex()
            .to_string();
            let found = if parent_exists {
                self.api.find_files(&format!(
                    "trashed = false and '{}' in parents and mimeType = 'application/vnd.google-apps.folder' and name = '{}'",
                    escape_query(&parent), escape_query(name)
                )).await?
            } else {
                Vec::new()
            };
            if found.len() > 1 {
                return Err(Error::new(ErrorKind::Conflict, "ambiguous Drive directory"));
            }
            let id = if let Some(file) = found.first() {
                if file.app_properties.get("waybill_directory") != Some(&key)
                    || !file.parents.contains(&parent)
                {
                    return Err(Error::new(ErrorKind::Conflict, "unowned Drive directory"));
                }
                file.id.clone()
            } else {
                parent_exists = false;
                self.api.generate_id().await?
            };
            directories.push(Directory {
                id: id.clone(),
                parent,
                name: name.into(),
                key,
            });
            parent = id;
        }
        let conflicts = if parent_exists {
            self.api
                .find_files(&format!(
                    "trashed = false and '{}' in parents and name = '{}'",
                    escape_query(&parent),
                    escape_query(name)
                ))
                .await?
        } else {
            Vec::new()
        };
        let remote_name = if conflicts.is_empty() {
            name.to_owned()
        } else if intent.conflict == ConflictPolicy::OperationSuffix {
            conflict_name(name, &operation_key(intent)[..12])
        } else {
            return Err(Error::new(ErrorKind::Conflict, "Drive target exists"));
        };
        Ok(State {
            object_id: self.api.generate_id().await?,
            parent_id: parent,
            remote_name,
            directories,
            session_uri: None,
        })
    }
    pub(crate) async fn ensure_directories(&self, state: &State) -> Result<()> {
        for dir in &state.directories {
            let file = match self.api.get_file(&dir.id).await? {
                Some(file) => file,
                None => {
                    let request = self
                        .api
                        .http
                        .post(format!("{}/files", self.api.api_root))
                        .query(&[("fields", crate::object::FILE_FIELDS)])
                        .json(&json!({
                            "id": dir.id,
                            "name": dir.name,
                            "mimeType": "application/vnd.google-apps.folder",
                            "parents": [dir.parent],
                            "appProperties": {"waybill_directory": dir.key}
                        }));
                    let response = self.api.request(request, true).await?;
                    if response.status() != reqwest::StatusCode::CONFLICT {
                        ensure_success(&response)?;
                    }
                    self.api.get_file(&dir.id).await?.ok_or_else(|| {
                        Error::new(
                            ErrorKind::ResultUnknown,
                            "Drive directory creation unresolved",
                        )
                    })?
                }
            };
            if file.id != dir.id
                || file.name != dir.name
                || file.mime_type != "application/vnd.google-apps.folder"
                || !file.parents.contains(&dir.parent)
                || file.app_properties.get("waybill_directory") != Some(&dir.key)
            {
                return Err(Error::new(
                    ErrorKind::IdentityMismatch,
                    "Drive directory ownership changed",
                ));
            }
        }
        Ok(())
    }
    pub(crate) async fn check_collision(&self, state: &State) -> Result<()> {
        let files = self
            .api
            .find_files(&format!(
                "trashed = false and '{}' in parents and name = '{}'",
                escape_query(&state.parent_id),
                escape_query(&state.remote_name)
            ))
            .await?;
        if files.iter().any(|file| file.id != state.object_id) {
            return Err(Error::new(ErrorKind::Conflict, "Drive target exists"));
        }
        Ok(())
    }
}
fn conflict_name(name: &str, suffix: &str) -> String {
    let (stem, ext) = name
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .unwrap_or((name, ""));
    if ext.is_empty() {
        format!("{stem} ({suffix})")
    } else {
        format!("{stem} ({suffix}).{ext}")
    }
}
