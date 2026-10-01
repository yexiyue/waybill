//! 目标 URI：`gdrive://<account>/<path>`。
//!
//! 不用 url crate 解析：账户通常是电子邮件，`@` 会被误拆成 userinfo/host。
//! 路径按字面使用，不做百分号解码；shell 引号已覆盖转义需求。
use waybill::error::{Error, ErrorKind, Result};

const SCHEME: &str = "gdrive://";

/// 解析后的上传目标；首期仅 Google Drive。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// 账户标识，对应 `wb login` 保存的凭证目录。
    pub account: String,
    /// 相对根目录的目标或目录前缀；不以 `/` 开头，可为空（仅目录前缀）。
    pub target: String,
    /// 是否目录前缀（URI 以 `/` 结尾）；多源投递必须为 true。
    pub directory: bool,
}

impl Destination {
    /// 目录前缀拼接文件名，得到单个源的目标路径。
    pub fn join(&self, file_name: &str) -> Result<String> {
        if !self.directory {
            return Err(invalid());
        }
        if self.target.is_empty() {
            return Ok(file_name.to_string());
        }
        Ok(format!("{}/{}", self.target, file_name))
    }
}

/// 解析并做结构校验；路径段的语义规则由引擎的 UploadIntent::validate 复核。
pub fn parse(uri: &str) -> Result<Destination> {
    let rest = uri.strip_prefix(SCHEME).ok_or_else(invalid)?;
    let (account, path) = rest.split_once('/').ok_or_else(invalid)?;
    if !safe_account(account) || path.contains("//") {
        return Err(invalid());
    }
    // split_once 消耗了账户后的第一个 `/`：空路径同样代表根目录前缀。
    let directory = path.is_empty() || path.ends_with('/');
    let target = path.trim_end_matches('/').to_string();
    if !directory && target.is_empty() {
        return Err(invalid());
    }
    Ok(Destination {
        account: account.to_string(),
        target,
        directory,
    })
}

/// 账户会同时作为凭证目录名：拒绝路径分量与控制字符，其余（含 `@`）放行。
pub fn safe_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 256
        && account != "."
        && account != ".."
        && account
            .chars()
            .all(|c| !c.is_control() && c != '\\' && c != '/')
}

fn invalid() -> Error {
    Error::new(ErrorKind::InvalidInput, "invalid destination URI")
}

#[cfg(test)]
mod tests {
    use super::*;
    use waybill::error::ErrorKind;

    #[test]
    fn parses_account_and_prefix() {
        let dest = parse("gdrive://me@gmail.com/backup/").unwrap();
        assert_eq!(dest.account, "me@gmail.com");
        assert_eq!(dest.target, "backup");
        assert!(dest.directory);
        assert_eq!(dest.join("a.iso").unwrap(), "backup/a.iso");
        assert_eq!(
            parse("gdrive://me@gmail.com/")
                .unwrap()
                .join("a.iso")
                .unwrap(),
            "a.iso"
        );
    }

    #[test]
    fn parses_exact_target() {
        let dest = parse("gdrive://me@gmail.com/backup/a.iso").unwrap();
        assert_eq!(dest.target, "backup/a.iso");
        assert!(!dest.directory);
        assert!(dest.join("b.iso").is_err());
    }

    #[test]
    fn rejects_structural_errors() {
        for uri in [
            "webdav://x/y",
            "gdrive://me@gmail.com",
            "gdrive://me@gmail.com/backup//a",
            "gdrive:///backup/",
            "gdrive://../backup/",
        ] {
            assert_eq!(
                parse(uri).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{uri}"
            );
        }
    }
}
