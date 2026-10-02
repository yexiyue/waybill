//! 引用的公共边界与可选的相对对象路径规则。
use serde::{Deserialize, Serialize};

/// 目录项种类；没有可下载字节长度的文件仍可供浏览。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    /// 普通对象，是否可下载由长度和下载端口决定。
    File,
    /// 可列举直接子项的目录。
    Directory,
}

/// 安全展示的对象元信息；reference 由对应 service 解释，不包含凭证。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectMetadata {
    /// 稳定对象引用，供 list / download_source 使用。
    pub reference: String,
    /// 展示名称，不用于替代对象引用。
    pub name: String,
    /// 文件或目录。
    pub kind: ObjectKind,
    /// 字节长度；目录或不提供二进制内容的对象为空。
    pub size: Option<u64>,
    /// 服务端修改时间，仅用于显示，不作为通用版本证据。
    pub modified: Option<String>,
}

impl ObjectMetadata {
    /// 是否为目录。
    pub fn is_directory(&self) -> bool {
        self.kind == ObjectKind::Directory
    }
}

/// 目标路径：段非空且不含 `.`、`..`、反斜杠或控制字符。
pub fn valid_object_path(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 4096
        && target.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.len() <= 255
                && !segment.chars().any(|c| c.is_control() || c == '\\')
        })
}

/// 不透明对象引用：非空、最长 4096 字节、不含控制字符。
/// 具体引用语法由 service 解释，不在核心强制套用路径分段规则。
pub fn valid_reference(reference: &str) -> bool {
    !reference.is_empty() && reference.len() <= 4096 && !reference.chars().any(char::is_control)
}
