//! 内容摘要与验证证据；上传、下载使用同一套表示。
use crate::error::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

/// 服务端声明的预期内容摘要；算法与值成对出现，不跨算法比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigestAlgorithm {
    /// Drive 等后端返回的 MD5 小写十六进制。
    Md5,
    /// 本库上传契约使用的 BLAKE3 小写十六进制。
    Blake3,
}
/// 服务端提供的预期摘要；缺失时下载只声明长度一致性。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    /// 摘要算法。
    pub algorithm: DigestAlgorithm,
    /// 小写十六进制值；长度由算法决定。
    pub value: String,
}
/// 完成回执记录的内容验证证据；由目标侧按实际校验结果声明。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Verification {
    /// 无内容证据。
    #[default]
    Unverified,
    /// 仅长度一致。
    Length,
    /// 与预期摘要一致；证据来自服务端摘要或对远端内容的独立读取校验。
    Digest {
        /// 摘要算法。
        algorithm: DigestAlgorithm,
        /// 小写十六进制值。
        value: String,
    },
}
pub(crate) fn valid_digest_value(digest: &Digest) -> Result<()> {
    let expected = match digest.algorithm {
        DigestAlgorithm::Md5 => 32,
        DigestAlgorithm::Blake3 => 64,
    };
    if digest.value.len() != expected
        || !digest
            .value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(Error::new(ErrorKind::InvalidInput, "invalid digest value"));
    }
    Ok(())
}
