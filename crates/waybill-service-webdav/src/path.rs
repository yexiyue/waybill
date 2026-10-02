//! URL 仅在协议边界编码；对象引用始终为配置根内的字面相对路径。
use percent_encoding::percent_decode_str;
use url::Url;
use waybill::error::{Error, ErrorKind, Result};

#[derive(Clone)]
pub(crate) struct Paths {
    pub root: Url,
}
impl Paths {
    pub fn new(endpoint: &str) -> Result<Self> {
        if endpoint.len() > 4096 {
            return Err(invalid());
        }
        let mut root = Url::parse(endpoint).map_err(|_| invalid())?;
        if !matches!(root.scheme(), "http" | "https")
            || root.host_str().is_none()
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
        {
            return Err(invalid());
        }
        if !root.path().ends_with('/') {
            root.set_path(&format!("{}/", root.path()));
        }
        // 根路径同样必须由有效的字面段构成，避免编码后的斜杠和 traversal。
        decoded_segments(&root)?;
        Ok(Self { root })
    }
    pub fn url(&self, reference: &str) -> Result<Url> {
        let path = reference.trim_end_matches('/');
        if reference != "/" && !waybill::object::valid_object_path(path) {
            return Err(invalid());
        }
        if path.split('/').count() > 32 {
            return Err(invalid());
        }
        let mut url = self.root.clone();
        if reference != "/" {
            let mut segments = url.path_segments_mut().map_err(|_| invalid())?;
            segments.pop_if_empty();
            for segment in path.split('/') {
                segments.push(segment);
            }
            if reference.ends_with('/') {
                segments.push("");
            }
        }
        Ok(url)
    }
    pub fn reference(&self, href: &str, directory: bool) -> Result<String> {
        let url = self.root.join(href).map_err(|_| protocol())?;
        if url.origin() != self.root.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(protocol());
        }
        let root = decoded_segments(&self.root)?;
        let path = decoded_segments(&url)?;
        if !path.starts_with(&root) {
            return Err(protocol());
        }
        let relative = path[root.len()..].join("/");
        if relative.is_empty() {
            return Ok("/".into());
        }
        if !waybill::object::valid_object_path(&relative) {
            return Err(protocol());
        }
        Ok(if directory {
            format!("{relative}/")
        } else {
            relative
        })
    }
}
fn decoded_segments(url: &Url) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut segments = url.path_segments().ok_or_else(invalid)?.peekable();
    while let Some(encoded) = segments.next() {
        if encoded.is_empty() {
            if segments.peek().is_some() {
                return Err(protocol());
            }
            continue;
        }
        let decoded = percent_decode_str(encoded)
            .decode_utf8()
            .map_err(|_| protocol())?
            .into_owned();
        if !waybill::object::valid_object_path(&decoded) || decoded.contains('/') {
            return Err(protocol());
        }
        result.push(decoded);
    }
    Ok(result)
}
fn invalid() -> Error {
    Error::new(ErrorKind::InvalidInput, "invalid WebDAV endpoint or path")
}
fn protocol() -> Error {
    Error::new(ErrorKind::Protocol, "WebDAV href escapes configured root")
}
