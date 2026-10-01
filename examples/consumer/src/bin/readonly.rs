//! 外部只读 service 无须实现上传，也不修改核心枚举。
use std::sync::Arc;
use waybill::{
    BoxFuture,
    error::{Error, ErrorKind, Result},
    service::{Capabilities, Service, ServiceId, ServiceIdentity, ServiceInfo},
    source::{Source, SourceIdentity},
};
struct MemorySource(Arc<[u8]>);
impl Source for MemorySource {
    fn identity(&self) -> BoxFuture<'_, SourceIdentity> {
        Box::pin(async {
            Ok(SourceIdentity {
                reference: "demo".into(),
                revision: "immutable".into(),
                size: self.0.len() as u64,
                blake3: blake3::hash(&self.0).to_hex().to_string(),
            })
        })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async move {
            let start = usize::try_from(offset)
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "range overflow"))?;
            let end = start
                .checked_add(length)
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "range overflow"))?;
            self.0
                .get(start..end)
                .map(|v| v.to_vec())
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "range bounds"))
        })
    }
}
struct Readonly {
    identity: ServiceIdentity,
}
impl Readonly {
    fn new() -> Result<Self> {
        Ok(Self {
            identity: ServiceIdentity {
                service: ServiceId::parse("example:memory")?,
                instance: "immutable-demo".into(),
            },
        })
    }
}
impl Service for Readonly {
    fn info(&self) -> ServiceInfo {
        ServiceInfo {
            identity: self.identity.clone(),
            capabilities: Capabilities {
                range_source: true,
                ..Capabilities::default()
            },
        }
    }
    fn source<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Arc<dyn Source>> {
        Box::pin(async move {
            if reference != "demo" {
                return Err(Error::new(ErrorKind::InvalidInput, "unknown reference"));
            }
            Ok(Arc::new(MemorySource(Arc::from(&b"example"[..]))) as Arc<dyn Source>)
        })
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let service: Arc<dyn Service> = Arc::new(Readonly::new()?);
    let source = service.source("demo").await?;
    println!("readonly bytes={}", source.identity().await?.size);
    assert_eq!(source.read_range(0, 7).await?, b"example");
    assert!(matches!(service.upload_sink(),Err(e) if e.kind==ErrorKind::Unsupported));
    Ok(())
}
#[cfg(test)]
async fn main_contract() {
    let service: Arc<dyn Service> = Arc::new(Readonly::new().unwrap());
    assert!(service.info().capabilities.range_source);
    assert_eq!(
        service
            .source("demo")
            .await
            .unwrap()
            .read_range(0, 7)
            .await
            .unwrap(),
        b"example"
    );
    assert!(matches!(service.upload_sink(),Err(e) if e.kind==ErrorKind::Unsupported));
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn third_party_readonly_uses_only_public_contracts() {
        super::main_contract().await;
    }
}
