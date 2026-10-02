//! OpenDAL 对象存储宿主：私有配置与环境凭证引用，不在命令行传递秘密。
use crate::{credentials::save_private, error::CliError, paths::Layout, uri};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read, path::Path};
use waybill_service_opendal::{ObjectStorage, ObjectStorageConfig, opendal::Operator};
const BACKENDS: &[&str] = &[
    "s3",
    "oss",
    "cos",
    "obs",
    "tos",
    "gcs",
    "azblob",
    "b2",
    "swift",
    "upyun",
    "vercel-blob",
];
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    backend: String,
    namespace: String,
    #[serde(default)]
    conditional_writes: bool,
    options: BTreeMap<String, String>,
}
impl Account {
    fn service(&self, root: &str) -> Result<ObjectStorage, CliError> {
        if !BACKENDS.contains(&self.backend.as_str()) {
            return Err(CliError::Message(format!(
                "不支持的对象存储后端；可用：{}",
                BACKENDS.join(", ")
            )));
        }
        let options = self
            .options
            .iter()
            .map(|(key, value)| Ok((key.clone(), expand(value)?)))
            .collect::<Result<BTreeMap<_, _>, CliError>>()?;
        if let Some(endpoint) = options.get("endpoint") {
            let url = url::Url::parse(endpoint)
                .map_err(|_| CliError::Message("对象存储 endpoint 无效".into()))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(CliError::Message(
                    "endpoint 必须为不含凭证或 query 的 HTTP(S) URL".into(),
                ));
            }
        }
        // 根和账户轮换不得复用旧 checkpoint；凭证轮换则不改变实例。
        let identity_options: BTreeMap<_, _> = options
            .iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "endpoint" | "bucket" | "container" | "region" | "root" | "account_name"
                )
            })
            .collect();
        let namespace = serde_json::to_vec(&(&self.namespace, &self.backend, identity_options))?;
        let mut config = ObjectStorageConfig::new(blake3::hash(&namespace).to_hex().to_string());
        if self.namespace.is_empty()
            || self.namespace.len() > 4096
            || self.namespace.chars().any(char::is_control)
        {
            return Err(CliError::Message(
                "对象存储 namespace 必须标识实际账户，不能为空".into(),
            ));
        }
        config.root = root.into();
        config.conditional_writes = self.conditional_writes;
        let operator = Operator::via_iter(&self.backend, options).map_err(|_| {
            CliError::Message("OpenDAL 后端配置无效，请检查对应后端的配置字段".into())
        })?;
        Ok(ObjectStorage::new(operator, config)?)
    }
}
fn expand(value: &str) -> Result<String, CliError> {
    if let Some(name) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return Err(CliError::Message("环境变量引用无效".into()));
        }
        std::env::var(name).map_err(|_| CliError::Message(format!("缺少环境变量 {name}")))
    } else {
        Ok(value.into())
    }
}
fn read(reader: impl Read) -> Result<Account, CliError> {
    let mut bytes = Vec::new();
    reader.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(CliError::Message("对象存储配置超过大小上限".into()));
    }
    serde_json::from_slice(&bytes).map_err(|_| CliError::Message("对象存储 JSON 配置无效".into()))
}
pub(crate) fn build(layout: &Layout, alias: &str, root: &str) -> Result<ObjectStorage, CliError> {
    let file = std::fs::File::open(layout.object_account(alias).join("credentials.json")).map_err(
        |e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliError::Message(format!(
                    "对象存储账户尚未配置；运行 wb login --account {alias} object --config <FILE>"
                ))
            } else {
                e.into()
            }
        },
    )?;
    read(file)?.service(root)
}
pub(crate) async fn login(path: &Path, alias: Option<String>, json: bool) -> Result<(), CliError> {
    let alias =
        alias.ok_or_else(|| CliError::Message("对象存储配置需要 --account <别名>".into()))?;
    if !uri::safe_account(&alias) {
        return Err(CliError::Message("对象存储账户别名无效".into()));
    }
    let account = if path == Path::new("-") {
        read(std::io::stdin())?
    } else {
        read(std::fs::File::open(path)?)?
    };
    let service = account.service("/")?;
    // 列表可验证配置的读权限；不把这次检查描述为上传或恢复验收。
    use waybill::service::Service;
    service.list("/").await?;
    save_private(
        &Layout::discover()?
            .object_account(&alias)
            .join("credentials.json"),
        &account,
    )?;
    if json {
        println!(
            "{}",
            serde_json::json!({"provider":"object","backend":account.backend,"account":alias,"capabilities":{"range_download":service.info().capabilities.range_download,"stream_upload":service.info().capabilities.stream_upload}})
        );
    } else {
        println!(
            "已配置对象存储 {}：{alias}\n配置盘：wb drive add <NAME> --provider object --account {alias}",
            account.backend
        );
        if !service.info().capabilities.stream_upload {
            println!("当前配置提供浏览与后端支持的下载；上传需要有效的条件写入能力。");
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_errors_do_not_disclose_secrets() {
        let bad = br#"{"backend":"s3","options":{"secret_access_key":"do-not-print"}}"#;
        assert!(
            !read(bad.as_slice())
                .err()
                .unwrap()
                .to_string()
                .contains("do-not-print")
        );
    }
    #[test]
    fn non_object_backend_and_unknown_fields_are_rejected() {
        let account: Account = serde_json::from_str(
            r#"{"backend":"fs","namespace":"local","options":{"root":"/tmp"}}"#,
        )
        .unwrap();
        assert!(account.service("/").is_err());
        assert!(
            read(br#"{"backend":"s3","namespace":"x","options":{},"wrong":true}"#.as_slice())
                .is_err()
        );
    }
    #[test]
    fn credentials_rotate_without_changing_identity_but_endpoint_and_principal_do() {
        let mut account: Account = serde_json::from_str(r#"{"backend":"s3","namespace":"tenant","options":{"bucket":"test","region":"us-east-1","endpoint":"http://localhost:9000","access_key_id":"first","secret_access_key":"secret"}}"#).unwrap();
        use waybill::service::Service;
        let first = account.service("/").unwrap().info().identity;
        account
            .options
            .insert("access_key_id".into(), "second".into());
        assert_eq!(first, account.service("/").unwrap().info().identity);
        account.namespace = "other-tenant".into();
        assert_ne!(first, account.service("/").unwrap().info().identity);
        account.namespace = "tenant".into();
        account
            .options
            .insert("endpoint".into(), "http://localhost:9001".into());
        assert_ne!(first, account.service("/").unwrap().info().identity);
    }
}
