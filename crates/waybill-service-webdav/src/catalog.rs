//! 有界 PROPFIND 与属性合并；请求和 href 必须留在同一个配置根中。
use crate::{
    Webdav,
    client::{bounded, network, success},
};
use serde::Deserialize;
use waybill::{
    error::{Error, ErrorKind, Result},
    object::{ObjectKind, ObjectMetadata},
};

const XML_LIMIT: usize = 1024 * 1024;
const ENTRY_LIMIT: usize = 1000;
const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:w="urn:waybill"><d:prop><d:resourcetype/><d:getcontentlength/><d:getetag/><d:getlastmodified/><w:delivery/></d:prop></d:propfind>"#;

#[derive(Deserialize)]
struct MultiStatus {
    #[serde(rename = "response", default)]
    responses: Vec<DavResponse>,
}
#[derive(Deserialize)]
struct DavResponse {
    href: String,
    #[serde(rename = "propstat", default)]
    properties: Vec<PropStat>,
}
#[derive(Deserialize)]
struct PropStat {
    status: String,
    prop: Properties,
}
#[derive(Deserialize, Default)]
struct Properties {
    delivery: Option<String>,
    #[serde(rename = "resourcetype")]
    resource_type: Option<ResourceType>,
    #[serde(rename = "getcontentlength")]
    length: Option<String>,
    #[serde(rename = "getetag")]
    etag: Option<String>,
    #[serde(rename = "getlastmodified")]
    modified: Option<String>,
}
#[derive(Deserialize)]
struct ResourceType {
    collection: Option<Collection>,
}
#[derive(Deserialize)]
struct Collection {}

pub(crate) struct Metadata {
    pub object: ObjectMetadata,
    pub etag: Option<String>,
    pub delivery: Option<String>,
}
impl Webdav {
    async fn propfind(&self, reference: &str, depth: u8) -> Result<Vec<Metadata>> {
        let response = self
            .api
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").map_err(|_| protocol())?,
                reference,
            )
            .await?
            .header("Depth", depth.to_string())
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(PROPFIND)
            .send()
            .await
            .map_err(network)?;
        success(&response)?;
        if response.status().as_u16() != 207 {
            return Err(protocol());
        }
        let xml = bounded(response, XML_LIMIT).await?;
        parse(self, &xml)
    }
    pub(crate) async fn stat(&self, path: &str) -> Result<Metadata> {
        let mut entries = self.propfind(path, 0).await?;
        if entries.len() != 1 {
            return Err(protocol());
        }
        let entry = entries.pop().ok_or_else(protocol)?;
        if entry.object.reference.trim_end_matches('/') != path.trim_end_matches('/') {
            return Err(Error::new(
                ErrorKind::Protocol,
                "WebDAV stat returned another object",
            ));
        }
        if path.ends_with('/') && !entry.object.is_directory() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV path is not a directory",
            ));
        }
        Ok(entry)
    }
    pub(crate) async fn children(&self, reference: &str) -> Result<Vec<ObjectMetadata>> {
        if !reference.ends_with('/') {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV list requires directory reference",
            ));
        }
        let prefix = if reference == "/" { "" } else { reference };
        let entries = self.propfind(reference, 1).await?;
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut parent_seen = false;
        for entry in entries {
            let object = entry.object;
            if object.reference == reference {
                if !object.is_directory() {
                    return Err(protocol());
                }
                parent_seen = true;
                continue;
            }
            let child = object
                .reference
                .strip_prefix(prefix)
                .ok_or_else(protocol)?
                .trim_end_matches('/');
            if child.is_empty() || child.contains('/') || !seen.insert(object.reference.clone()) {
                return Err(protocol());
            }
            if result.len() == ENTRY_LIMIT {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "WebDAV listing exceeds bound",
                ));
            }
            result.push(object);
        }
        // 部分服务器的 Depth: 1 只返回子项；另行确认父目录，仍校验全部 href 和层级。
        if !parent_seen && !self.stat(reference).await?.object.is_directory() {
            return Err(protocol());
        }
        Ok(result)
    }
}
fn parse(service: &Webdav, xml: &[u8]) -> Result<Vec<Metadata>> {
    // serde 按 local name 解码；先检查命名空间，避免同名外部属性冒充操作标记。
    let mut reader = quick_xml::reader::NsReader::from_reader(xml);
    let mut root_seen = false;
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(|_| protocol())?;
        match event {
            quick_xml::events::Event::Start(tag) | quick_xml::events::Event::Empty(tag) => {
                let local = tag.local_name();
                let name = local.as_ref();
                if !root_seen {
                    if name != "multistatus" {
                        return Err(protocol());
                    }
                    root_seen = true;
                }
                let expected = match name {
                    "delivery" => Some("urn:waybill"),
                    "multistatus" | "response" | "href" | "propstat" | "prop" | "status"
                    | "resourcetype" | "collection" | "getcontentlength" | "getetag"
                    | "getlastmodified" => Some("DAV:"),
                    _ => None,
                };
                if let Some(expected) = expected
                    && !matches!(namespace, quick_xml::name::ResolveResult::Bound(ns) if ns.as_ref() == expected)
                {
                    return Err(protocol());
                }
            }
            quick_xml::events::Event::DocType(_) => return Err(protocol()),
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen {
        return Err(protocol());
    }
    let parsed: MultiStatus = quick_xml::de::from_reader(xml).map_err(|_| protocol())?;
    if parsed.responses.len() > ENTRY_LIMIT + 1 {
        return Err(Error::new(
            ErrorKind::Protocol,
            "WebDAV listing exceeds bound",
        ));
    }
    parsed
        .responses
        .into_iter()
        .map(|response| {
            let mut directory = None;
            let (mut size, mut etag, mut modified) = (None, None, None);
            let mut delivery = None;
            for properties in response.properties {
                if properties.status.split_whitespace().nth(1) != Some("200") {
                    continue;
                }
                let prop = properties.prop;
                if let Some(value) = prop.delivery {
                    merge(&mut delivery, value)?;
                }
                if let Some(kind) = prop.resource_type {
                    merge(&mut directory, kind.collection.is_some())?;
                }
                if let Some(value) = prop.length.filter(|value| !value.is_empty()) {
                    merge(&mut size, value.parse::<u64>().map_err(|_| protocol())?)?;
                }
                if let Some(value) = prop.etag.filter(|value| !value.is_empty()) {
                    merge(&mut etag, property_etag(value)?)?;
                }
                if let Some(value) = prop.modified {
                    merge(&mut modified, value)?;
                }
            }
            let directory = directory.ok_or_else(protocol)?;
            let reference = service.api.paths.reference(&response.href, directory)?;
            let name = reference
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string();
            Ok(Metadata {
                object: ObjectMetadata {
                    reference,
                    name,
                    kind: if directory {
                        ObjectKind::Directory
                    } else {
                        ObjectKind::File
                    },
                    size: if directory { None } else { size },
                    modified,
                },
                etag,
                delivery,
            })
        })
        .collect()
}
fn merge<T: PartialEq>(slot: &mut Option<T>, value: T) -> Result<()> {
    if slot.as_ref().is_some_and(|current| current != &value) {
        return Err(protocol());
    }
    *slot = Some(value);
    Ok(())
}
fn protocol() -> Error {
    Error::new(ErrorKind::Protocol, "invalid WebDAV multistatus")
}
fn property_etag(value: String) -> Result<String> {
    // WsgiDAV 的属性省略引号；只修正表示，HTTP 边界仍要求真正的强 ETag。
    if crate::download::strong_etag(&value)
        || value
            .strip_prefix("W/")
            .is_some_and(crate::download::strong_etag)
    {
        return Ok(value);
    }
    if value.len() <= 254
        && !value.starts_with("W/")
        && value.bytes().all(|byte| matches!(byte, 0x21 | 0x23..=0x7e))
    {
        return Ok(format!("\"{value}\""));
    }
    Err(protocol())
}
