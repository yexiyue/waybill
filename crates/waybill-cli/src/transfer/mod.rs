//! 传输领域编排与共享事件；终端会话由 cmd::session 拥有。
mod download;
mod events;
mod upload;
pub(crate) use download::{DownloadJob, DownloadRunner};
pub(crate) use events::Event;
use tokio::sync::mpsc;
pub(crate) use upload::{UploadJob, UploadRunner};
use waybill::{error::ErrorKind, upload::StopToken};
/// 队列的最终结果；成功计数经 Done 事件交付，此处只保留退出判定所需。
pub(crate) struct Outcome {
    pub failures: usize,
    pub stopped: bool,
}

struct Failure {
    message: String,
    kind: ErrorKind,
}
impl Failure {
    async fn emit(self, events: &mpsc::Sender<Event>, index: usize, name: String) {
        let _ = events
            .send(Event::Failed {
                index,
                name,
                message: self.message,
                paused: self.kind == ErrorKind::Paused,
            })
            .await;
    }
}
async fn complete_queue(
    events: &mpsc::Sender<Event>,
    receipts: usize,
    failures: usize,
    stopped: bool,
) -> Outcome {
    let _ = events
        .send(Event::Done {
            receipts,
            failures,
            stopped,
        })
        .await;
    Outcome { failures, stopped }
}
fn failure(error: &waybill::error::Error) -> Failure {
    Failure {
        message: error.to_string(),
        kind: error.kind,
    }
}
fn output_closed() -> Failure {
    Failure {
        message: "output closed; rerun the same command to resume".into(),
        kind: ErrorKind::Paused,
    }
}
/// 同步回调不能等待；进度可合并，生命周期事件则必须交付。
fn emit_progress(events: &mpsc::Sender<Event>, stop: &StopToken, progress: Event) {
    if let Err(mpsc::error::TrySendError::Closed(_)) = events.try_send(progress) {
        stop.stop();
    }
}
/// 长度前缀避免字段边界歧义；方向与两端身份共同隔离操作。
fn operation_id(fields: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for field in fields {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    format!("wb-{}", &hasher.finalize().to_hex()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_fields_have_unambiguous_boundaries() {
        assert_ne!(operation_id(&["a", "bc"]), operation_id(&["ab", "c"]));
        assert_ne!(
            operation_id(&["upload", "x"]),
            operation_id(&["download", "x"])
        );
    }
    #[test]
    fn progress_stops_on_closed_output_but_not_backpressure() {
        let (sender, receiver) = mpsc::channel(1);
        let stop = StopToken::default();
        let progress = || Event::Progress {
            index: 0,
            persisted: 0,
            sent: 0,
            total: 1,
            epoch: 0,
            complete: false,
        };
        emit_progress(&sender, &stop, progress());
        emit_progress(&sender, &stop, progress());
        assert!(!stop.is_stopped());
        drop(receiver);
        emit_progress(&sender, &stop, progress());
        assert!(stop.is_stopped());
    }
}
