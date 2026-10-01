//! 独立消费者只通过公开 API 组装；它不是完整 CLI 或 OAuth 工具。
use std::{sync::Arc, time::Duration};
use waybill::{
    BoxFuture,
    budget::ResourceBudget,
    error::{Error, ErrorKind},
    service::Service,
    upload::{ConflictPolicy, RunOptions, StopToken, UploadEngine, UploadIntent, UploadPolicy},
};
use waybill_service_fs::{FileCheckpointStore, FsService};
use waybill_service_gdrive::{
    Gdrive, GdriveConfig,
    credential::{AccessToken, TokenProvider},
};
struct HostToken;
impl TokenProvider for HostToken {
    fn access_token(&self, _: Duration) -> BoxFuture<'_, AccessToken> {
        Box::pin(async {
            std::env::var("WAYBILL_GDRIVE_TOKEN")
                .map(|token| AccessToken::new(token, "host-env"))
                .map_err(|_| Error::new(ErrorKind::Authentication, "host token not configured"))
        })
    }
    fn after_rejection<'a>(&'a self, _: &'a AccessToken) -> BoxFuture<'a, AccessToken> {
        Box::pin(async {
            Err(Error::new(
                ErrorKind::Authentication,
                "host must refresh token",
            ))
        })
    }
    fn reconnect_required<'a>(&'a self, _: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 8 && args.len() != 9 {
        return Err("usage: consumer <source> <checkpoint-dir> <operation> <account-namespace> <oauth-application> <drive-folder-id> <target> [--confirm]".into());
    }
    let fs = FsService::new("consumer-local")?;
    let source = fs.source(&args[1]).await?;
    let drive = Gdrive::new(
        GdriveConfig {
            account: args[4].clone(),
            oauth_application: args[5].clone(),
            root: args[6].clone(),
        },
        Arc::new(HostToken),
    )?;
    let sink = drive.upload_sink()?;
    let store = FileCheckpointStore::new(&args[2]);
    let engine = UploadEngine::new(Arc::new(ResourceBudget::default()));
    let receipt = engine
        .run(
            source.as_ref(),
            sink.as_ref(),
            &store,
            RunOptions {
                intent: UploadIntent {
                    operation: args[3].clone(),
                    target: args[7].clone(),
                    conflict: ConflictPolicy::Reject,
                },
                policy: UploadPolicy::default(),
                stop: &StopToken::default(),
                progress: &|p| {
                    println!(
                        "persisted={}/{} complete={} epoch={}",
                        p.persisted, p.total, p.complete, p.epoch
                    )
                },
            },
        )
        .await?;
    println!(
        "receipt operation={} object={} size={}",
        receipt.operation, receipt.object, receipt.size
    );
    // 实际应用必须先提交业务账本；示例要求用户显式确认才能清理恢复记录。
    if args.get(8).is_some_and(|v| v == "--confirm") {
        engine.confirm(&store, &receipt).await?;
    }
    Ok(())
}
