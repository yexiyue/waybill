//! 宿主盘配置：名称映射账户与默认根目录，不包含凭证。
use crate::{error::CliError, paths::Layout, uri};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

/// 宿主支持的 service 工厂；核心 service 标识仍是开放命名空间。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Gdrive,
    Webdav,
}
impl ProviderKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Gdrive => "gdrive",
            Self::Webdav => "webdav",
        }
    }
    pub fn default_root(self) -> &'static str {
        match self {
            Self::Gdrive => "root",
            Self::Webdav => "/",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Drive {
    pub provider: ProviderKind,
    pub account: String,
    pub root: String,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Drives {
    pub default: Option<String>,
    pub drives: BTreeMap<String, Drive>,
}
impl Drives {
    pub fn load(layout: &Layout) -> Result<Self, CliError> {
        match std::fs::File::open(layout.drive_config()) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(65537).read_to_end(&mut bytes)?;
                if bytes.len() > 65536 {
                    return Err(CliError::Message("盘配置超过大小上限".into()));
                }
                let value: Self = serde_json::from_slice(&bytes)?;
                for (name, drive) in &value.drives {
                    validate(name, drive)?;
                }
                if value
                    .default
                    .as_ref()
                    .is_some_and(|name| !value.drives.contains_key(name))
                {
                    return Err(CliError::Message("默认盘不存在".into()));
                }
                Ok(value)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn save(&self, layout: &Layout) -> Result<(), CliError> {
        let path = layout.drive_config();
        let parent = path
            .parent()
            .ok_or_else(|| CliError::Message("配置目录无效".into()))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() > 65536 {
            return Err(CliError::Message("盘配置超过大小上限".into()));
        }
        file.write_all(&bytes)?;
        file.flush()?;
        file.as_file().sync_all()?;
        file.persist(&path).map_err(|error| error.error)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
    pub fn select(&self, name: Option<&str>) -> Result<Drive, CliError> {
        let name = name.or(self.default.as_deref()).ok_or_else(|| {
            CliError::Message(
                "没有默认盘；运行 wb drive add <名称> --account <账户>，或使用完整 gdrive:// URI"
                    .into(),
            )
        })?;
        self.drives
            .get(name)
            .cloned()
            .ok_or_else(|| CliError::Message(format!("盘 {name} 不存在")))
    }
}
pub fn validate(name: &str, drive: &Drive) -> Result<(), CliError> {
    let root_valid = match drive.provider {
        ProviderKind::Gdrive => {
            !drive.root.is_empty()
                && drive.root.len() <= 256
                && drive
                    .root
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        }
        ProviderKind::Webdav => {
            drive.root == "/"
                || waybill::object::valid_object_path(
                    drive.root.strip_suffix('/').unwrap_or(&drive.root),
                )
        }
    };
    if !uri::safe_account(name) || !uri::safe_account(&drive.account) || !root_valid {
        return Err(CliError::Message("盘名称、账户或根目录引用无效".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_roots_and_default_are_persisted_independently() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::for_test(dir.path().into());
        let mut config = Drives::default();
        config.drives.insert(
            "personal".into(),
            Drive {
                provider: ProviderKind::Gdrive,
                account: "a@example.com".into(),
                root: "folder-a".into(),
            },
        );
        config.drives.insert(
            "work".into(),
            Drive {
                provider: ProviderKind::Gdrive,
                account: "b@example.com".into(),
                root: "folder-b".into(),
            },
        );
        config.default = Some("work".into());
        config.save(&layout).unwrap();
        let loaded = Drives::load(&layout).unwrap();
        assert_eq!(loaded.select(None).unwrap().root, "folder-b");
        assert_eq!(
            loaded.select(Some("personal")).unwrap().account,
            "a@example.com"
        );
        assert!(loaded.select(Some("missing")).is_err());
    }
    #[test]
    fn oversized_and_invalid_default_configs_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::for_test(dir.path().into());
        std::fs::write(layout.drive_config(), vec![b' '; 65537]).unwrap();
        assert!(Drives::load(&layout).is_err());
        std::fs::write(
            layout.drive_config(),
            br#"{"default":"missing","drives":{}}"#,
        )
        .unwrap();
        assert!(Drives::load(&layout).is_err());
        assert!(
            validate(
                "../bad",
                &Drive {
                    provider: ProviderKind::Gdrive,
                    account: "a".into(),
                    root: "root".into()
                }
            )
            .is_err()
        );
    }
}
