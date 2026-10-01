//! 目标 URI：`gdrive://<account>/<path>`。
//!
//! 不用 url crate 解析：账户通常是电子邮件，`@` 会被误拆成 userinfo/host。
//! 路径按字面使用，不做百分号解码；shell 引号已覆盖转义需求。
use waybill::error::{Error, ErrorKind, Result};
use waybill::object::valid_object_path;

const SCHEME: &str = "gdrive://";

/// 解析后的云对象位置；上传、下载与列表共用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveUri {
    /// 账户标识，对应 `wb login` 保存的凭证目录。
    pub account: String,
    /// 相对根目录的目标或目录前缀；不以 `/` 开头，可为空（仅目录前缀）。
    pub target: String,
    /// 是否目录前缀（URI 以 `/` 结尾）；多源投递必须为 true。
    pub directory: bool,
}

impl DriveUri {
    pub fn to_uri(&self) -> String {
        let suffix = if self.directory && !self.target.is_empty() {
            "/"
        } else {
            ""
        };
        format!("gdrive://{}/{}{suffix}", self.account, self.target)
    }
    /// 目录前缀拼接文件名，得到单个源的目标路径。
    pub fn join(&self, file_name: &str) -> Result<String> {
        if !self.directory {
            return Err(invalid());
        }
        let target = if self.target.is_empty() {
            file_name.to_string()
        } else {
            format!("{}/{}", self.target, file_name)
        };
        validate_target(&target)?;
        Ok(target)
    }
}

/// 解析并复用公开对象路径约束，在读取凭证之前拒绝无效目标。
pub fn parse(uri: &str) -> Result<DriveUri> {
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
    if !target.is_empty() {
        validate_target(&target)?;
    }
    Ok(DriveUri {
        account: account.to_string(),
        target,
        directory,
    })
}

/// 账户会同时作为凭证目录名：拒绝路径分量与控制字符，其余（含 `@`）放行。
pub fn safe_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 255
        && account != "."
        && account != ".."
        && account
            .chars()
            .all(|c| !c.is_control() && c != '\\' && c != '/')
}

fn validate_target(target: &str) -> Result<()> {
    if valid_object_path(target) {
        Ok(())
    } else {
        Err(invalid())
    }
}

fn invalid() -> Error {
    Error::new(ErrorKind::InvalidInput, "invalid Drive URI")
}

#[cfg(test)]
mod tests {
    use super::*;
    use waybill::error::ErrorKind;

    #[test]
    fn formatting_preserves_root_and_directory_without_double_slashes() {
        for value in [
            "gdrive://bill/",
            "gdrive://bill/backup/",
            "gdrive://bill/a.bin",
        ] {
            assert_eq!(parse(value).unwrap().to_uri(), value);
        }
    }

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
            "gdrive://me@gmail.com/../backup/",
            "gdrive://me@gmail.com/backup/./a",
            "gdrive://me@gmail.com/backup\\a",
        ] {
            assert_eq!(
                parse(uri).unwrap_err().kind,
                ErrorKind::InvalidInput,
                "{uri}"
            );
        }
    }

    #[test]
    fn rejects_invalid_names_and_filesystem_component_overflow() {
        assert!(!safe_account(&"a".repeat(256)));
        assert!(safe_account(&"a".repeat(255)));
        let dest = parse("gdrive://me@gmail.com/backup/").unwrap();
        assert!(dest.join(&"a".repeat(256)).is_err());
        assert!(dest.join("..").is_err());
    }
}
