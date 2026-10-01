//! 可重读稳定源；不可重读流不继承这一恢复保证。
use crate::BoxFuture;
use serde::{Deserialize, Serialize};
/// 恢复锚点；digest 是源内容身份，不代表远端已校验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    /// 非敏感、稳定对象引用。
    pub reference: String,
    /// 源版本或文件指纹。
    pub revision: String,
    /// 精确长度。
    pub size: u64,
    /// BLAKE3 小写十六进制摘要。
    pub blake3: String,
}
/// 宿主须冻结源，service 须在重新读取前确认身份。
pub trait Source: Send + Sync {
    /// 完整核验并返回当前身份。
    fn identity(&self) -> BoxFuture<'_, SourceIdentity>;
    /// 返回恰好 length 字节；不得超出调用方预算。
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>>;
}
