//! 多文件顺序投递编排：只产出事件，不感知终端形态。
//!
//! 回执在发送 Completed 事件后立即确认清理：回执本身已持久化在 checkpoint，
//! 即使消费端输出中断，重跑同一命令经服务端对账仍可再次取得。
use std::path::PathBuf;
use tokio::sync::mpsc;
use waybill::{
    error::ErrorKind,
    source::Source,
    upload::{ConflictPolicy, Receipt, RunOptions, StopToken, UploadEngine, UploadIntent},
};
use waybill_service_fs::{FileCheckpointStore, FileSource};

/// 发往输出 sink 的事件流；字段只含可展示数据。
pub(crate) enum Event {
    /// 单个文件开始投递。
    Started {
        index: usize,
        name: String,
        target: String,
        size: u64,
        operation: String,
    },
    /// 引擎进度；persisted 为服务端确认并已落盘的字节数。
    Progress {
        index: usize,
        persisted: u64,
        sent: u64,
        total: u64,
        epoch: u32,
        complete: bool,
    },
    /// 单个文件取得回执。
    Completed { index: usize, receipt: Receipt },
    /// 单个文件失败；message 为库错误的受控 Display。
    Failed {
        index: usize,
        name: String,
        message: String,
        paused: bool,
    },
    /// 全部队列结束。
    Done {
        receipts: usize,
        failures: usize,
        stopped: bool,
    },
}

/// 一个待投递文件：本地路径与解析后的远端目标。
pub(crate) struct Job {
    pub path: PathBuf,
    pub name: String,
    pub target: String,
}

/// 队列的最终结果；成功计数经 Done 事件交付，此处只保留退出判定所需。
pub(crate) struct Outcome {
    pub failures: usize,
    pub stopped: bool,
}

/// 顺序投递全部任务；事件发完（含 Done）后返回。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    jobs: Vec<Job>,
    sink: std::sync::Arc<dyn waybill::upload::UploadSink>,
    store: FileCheckpointStore,
    engine: UploadEngine,
    stop: StopToken,
    conflict: ConflictPolicy,
    operation_override: Option<String>,
    events: mpsc::UnboundedSender<Event>,
) -> Outcome {
    let instance = sink.identity().instance;
    // 显式操作 ID 只对单文件队列有意义；多文件由调用方先行拒绝。
    let single_operation = if jobs.len() == 1 {
        operation_override
    } else {
        None
    };
    let mut receipts = 0;
    let mut failures = 0;
    let mut stopped = false;
    for (index, job) in jobs.into_iter().enumerate() {
        if stop.is_stopped() {
            stopped = true;
            break;
        }
        let outcome = deliver(
            index,
            &job,
            sink.as_ref(),
            &store,
            &engine,
            &stop,
            conflict,
            &instance,
            single_operation.as_deref(),
            &events,
        )
        .await;
        match outcome {
            Ok(()) => receipts += 1,
            Err(Failure { message, kind }) => {
                let _ = events.send(Event::Failed {
                    index,
                    name: job.name.clone(),
                    message,
                    paused: kind == ErrorKind::Paused,
                });
                failures += 1;
                if kind == ErrorKind::Paused {
                    // 用户请求停止：剩余文件保留为未开始状态。
                    stopped = true;
                    break;
                }
                if kind == ErrorKind::Authentication {
                    // 凭证失效时继续尝试只会重复失败，交还用户重新登录。
                    stopped = true;
                    break;
                }
            }
        }
    }
    let _ = events.send(Event::Done {
        receipts,
        failures,
        stopped,
    });
    Outcome { failures, stopped }
}

struct Failure {
    message: String,
    kind: ErrorKind,
}

#[allow(clippy::too_many_arguments)]
async fn deliver(
    index: usize,
    job: &Job,
    sink: &dyn waybill::upload::UploadSink,
    store: &FileCheckpointStore,
    engine: &UploadEngine,
    stop: &StopToken,
    conflict: ConflictPolicy,
    instance: &str,
    operation_override: Option<&str>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<(), Failure> {
    let source = FileSource::open(&job.path).await.map_err(|e| failure(&e))?;
    let identity = source.identity().await.map_err(|e| failure(&e))?;
    let operation = operation_override
        .map(str::to_string)
        .unwrap_or_else(|| derive_operation(instance, &job.target, &identity.blake3));
    let _ = events.send(Event::Started {
        index,
        name: job.name.clone(),
        target: job.target.clone(),
        size: identity.size,
        operation: operation.clone(),
    });
    let tx = events.clone();
    let receipt = engine
        .run(
            &source,
            sink,
            store,
            RunOptions {
                intent: UploadIntent {
                    operation,
                    target: job.target.clone(),
                    conflict,
                },
                policy: Default::default(),
                stop,
                progress: &move |p| {
                    let _ = tx.send(Event::Progress {
                        index,
                        persisted: p.persisted,
                        sent: p.sent,
                        total: p.total,
                        epoch: p.epoch,
                        complete: p.complete,
                    });
                },
            },
        )
        .await
        .map_err(|e| failure(&e))?;
    let _ = events.send(Event::Completed {
        index,
        receipt: receipt.clone(),
    });
    engine
        .confirm(store, &receipt)
        .await
        .map_err(|e| failure(&e))?;
    Ok(())
}

fn failure(error: &waybill::error::Error) -> Failure {
    Failure {
        message: error.to_string(),
        kind: error.kind,
    }
}

/// 默认操作 ID：实例、目标与内容摘要联合派生。
///
/// 同一命令重跑得到同一操作（续传或回执幂等）；文件改动即新运单。
fn derive_operation(instance: &str, target: &str, digest: &str) -> String {
    let mut input = Vec::with_capacity(instance.len() + target.len() + digest.len() + 2);
    input.extend_from_slice(instance.as_bytes());
    input.push(0);
    input.extend_from_slice(target.as_bytes());
    input.push(0);
    input.extend_from_slice(digest.as_bytes());
    format!("wb-{}", &blake3::hash(&input).to_hex()[..16])
}

#[cfg(test)]
mod tests {
    use super::derive_operation;

    #[test]
    fn operation_is_stable_and_bound_to_identity() {
        let a = derive_operation("instance", "backup/a.iso", &"0".repeat(64));
        assert_eq!(
            a,
            derive_operation("instance", "backup/a.iso", &"0".repeat(64))
        );
        assert_ne!(
            a,
            derive_operation("instance", "backup/b.iso", &"0".repeat(64))
        );
        assert_ne!(
            a,
            derive_operation("other", "backup/a.iso", &"0".repeat(64))
        );
        assert!(a.starts_with("wb-") && a.len() == 19);
    }
}
