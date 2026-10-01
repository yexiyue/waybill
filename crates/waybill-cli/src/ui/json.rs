//! `--json`：stdout 每行一个 JSON 事件，进度按时间节流。
use crate::{error::CliError, transfer::Event};
use serde_json::{Value, json};
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) async fn run(mut rx: UnboundedReceiver<Event>) -> Result<(), CliError> {
    let mut stdout = BufWriter::new(std::io::stdout().lock());
    let mut last_progress: Option<Instant> = None;
    while let Some(event) = rx.recv().await {
        if let Event::Progress {
            persisted,
            total,
            complete,
            ..
        } = event
        {
            // 节流例外：完成与对齐到总数的事件必须送达，保证脚本可见终态。
            let due = last_progress.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL);
            if !due && !complete && persisted < total {
                continue;
            }
            last_progress = Some(Instant::now());
        }
        let line = encode(event);
        writeln!(stdout, "{line}")?;
    }
    stdout.flush()?;
    Ok(())
}

/// 事件 → 单行 JSON；不出现凭证、会话或驱动 payload。
fn encode(event: Event) -> Value {
    match event {
        Event::Started {
            index,
            name,
            target,
            size,
            operation,
        } => json!({
            "type": "started",
            "index": index,
            "name": name,
            "target": target,
            "size": size,
            "operation": operation,
        }),
        Event::Progress {
            index,
            persisted,
            sent,
            total,
            epoch,
            complete,
        } => json!({
            "type": "progress",
            "index": index,
            "persisted": persisted,
            "sent": sent,
            "total": total,
            "epoch": epoch,
            "complete": complete,
        }),
        Event::Completed { index, receipt } => json!({
            "type": "completed",
            "index": index,
            "operation": receipt.operation,
            "service": receipt.service.service.as_str(),
            "instance": receipt.service.instance,
            "target": receipt.target,
            "object": receipt.object,
            "size": receipt.size,
        }),
        Event::Failed {
            index,
            name,
            message,
            paused,
        } => json!({
            "type": "failed",
            "index": index,
            "name": name,
            "message": message,
            "paused": paused,
        }),
        Event::Done {
            receipts,
            failures,
            stopped,
        } => json!({
            "type": "done",
            "receipts": receipts,
            "failures": failures,
            "stopped": stopped,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waybill::{
        service::{ServiceId, ServiceIdentity},
        upload::Receipt,
    };

    #[test]
    fn completed_event_serializes_receipt() {
        let receipt = Receipt {
            operation: "wb-abc".into(),
            service: ServiceIdentity {
                service: ServiceId::parse("waybill:gdrive").unwrap(),
                instance: "inst".into(),
            },
            target: "backup/a.iso".into(),
            object: "obj-1".into(),
            size: 42,
        };
        let value = encode(Event::Completed { index: 0, receipt });
        assert_eq!(value["type"], "completed");
        assert_eq!(value["object"], "obj-1");
        assert_eq!(value["service"], "waybill:gdrive");
    }
}
