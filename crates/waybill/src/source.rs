//! 可重读稳定源；不可重读流不继承这一恢复保证。
use crate::{
    BoxFuture,
    content::{Digest, DigestAlgorithm, valid_digest_value},
    error::{Error, ErrorKind, Result},
};
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
impl SourceIdentity {
    /// 校验有界的引用、版本与 BLAKE3 身份摘要。
    pub fn validate(&self) -> Result<()> {
        if self.reference.is_empty()
            || self.reference.len() > 4096
            || self.reference.chars().any(char::is_control)
            || self.revision.is_empty()
            || self.revision.len() > 256
            || self.revision.chars().any(char::is_control)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid source identity",
            ));
        }
        valid_digest_value(&Digest {
            algorithm: DigestAlgorithm::Blake3,
            value: self.blake3.clone(),
        })
    }
}
/// 宿主须冻结源，service 须在重新读取前确认身份。
pub trait Source: Send + Sync {
    /// 单次范围读取的最大字节数；必须非零。
    fn max_read_size(&self) -> usize;
    /// 完整核验并返回当前身份。
    fn identity(&self) -> BoxFuture<'_, SourceIdentity>;
    /// 返回恰好 length 字节；不得超出调用方预算。
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>>;
}
