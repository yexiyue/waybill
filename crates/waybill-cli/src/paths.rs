//! 本机私有状态布局；凭证与 checkpoint 永不进入仓库或配置目录。
use std::path::PathBuf;
use waybill::error::{Error, ErrorKind, Result};

/// waybill 的本机目录集合。
#[derive(Debug, Clone)]
pub struct Layout {
    /// 私有状态根；Linux 为 ~/.local/state/waybill。
    state: PathBuf,
}

impl Layout {
    #[cfg(test)]
    pub(crate) fn for_test(state: PathBuf) -> Self {
        Self { state }
    }

    /// 发现平台目录；HOME 缺失属于不可恢复的环境错误。
    pub fn discover() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("com", "yexiyue", "waybill")
            .ok_or_else(|| Error::new(ErrorKind::Io, "platform directories unavailable"))?;
        // macOS 没有独立的 state 目录，落到 Application Support 下的 state。
        let state = dirs
            .state_dir()
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs.data_local_dir().join("state"));
        Ok(Self { state })
    }

    /// 非敏感盘配置，与凭证分开保存。
    pub fn drive_config(&self) -> PathBuf {
        self.state.join("drives.json")
    }

    /// 引擎 checkpoint 存储根，交给 FileCheckpointStore。
    pub fn checkpoints(&self) -> PathBuf {
        self.state.join("checkpoints")
    }

    /// 单个 Drive 账户的私有目录；调用方需先用 uri::safe_account 校验账户名。
    pub fn gdrive_account(&self, account: &str) -> PathBuf {
        self.state.join("credentials").join("gdrive").join(account)
    }
}
