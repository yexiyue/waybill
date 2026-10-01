//! `wb status`：枚举本机在途 checkpoint。
//!
//! 只读公共信封字段；驱动 payload 可能携带会话凭证，绝不渲染。
//! 不获取排他租约：存储的原子替换保证读到旧或新的完整记录。
use crate::{error::CliError, paths::Layout, ui::human_bytes};
use serde::Serialize;
use std::path::Path;
use waybill::checkpoint::{Checkpoint, FORMAT_VERSION};

/// 单条在途会话的展示行；字段与 JSON 输出共用。
#[derive(Serialize)]
struct SessionRow {
    operation: String,
    target: String,
    service: String,
    instance: String,
    acknowledged: u64,
    total: u64,
    restarts: u32,
    /// in_flight = 上传未完成；awaiting_receipt = 远端已完成、待确认清理。
    state: &'static str,
}

/// 无法读取的记录与原因；文件名安全，原因只保留静态分类。
struct Unreadable {
    file: String,
    reason: &'static str,
}

pub async fn run(json: bool) -> Result<(), CliError> {
    let layout = Layout::discover()?;
    let dir = layout.checkpoints();
    let (rows, unreadable) = scan(&dir);
    if json {
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        render(&dir, &rows);
    }
    for entry in &unreadable {
        eprintln!("wb: 无法读取 {}: {}", entry.file, entry.reason);
    }
    Ok(())
}

fn scan(dir: &Path) -> (Vec<SessionRow>, Vec<Unreadable>) {
    let mut rows = Vec::new();
    let mut unreadable = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (rows, unreadable);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        let Ok(bytes) = std::fs::read(&path) else {
            unreadable.push(Unreadable {
                file,
                reason: "read failed",
            });
            continue;
        };
        match serde_json::from_slice::<Checkpoint>(&bytes) {
            Ok(checkpoint) if checkpoint.version == FORMAT_VERSION => rows.push(SessionRow {
                operation: checkpoint.intent.operation,
                target: checkpoint.intent.target,
                service: checkpoint.service.service.as_str().to_string(),
                instance: checkpoint.service.instance,
                acknowledged: checkpoint.acknowledged,
                total: checkpoint.source.size,
                restarts: checkpoint.restarts,
                state: if checkpoint.receipt.is_some() {
                    "awaiting_receipt"
                } else {
                    "in_flight"
                },
            }),
            Ok(_) => unreadable.push(Unreadable {
                file,
                reason: "incompatible version",
            }),
            Err(_) => unreadable.push(Unreadable {
                file,
                reason: "decode failed",
            }),
        }
    }
    rows.sort_by(|a, b| a.operation.cmp(&b.operation));
    (rows, unreadable)
}

fn render(dir: &Path, rows: &[SessionRow]) {
    println!("在途运单 {} 条  checkpoint: {}", rows.len(), dir.display());
    for row in rows {
        let percent = (row.acknowledged * 100).checked_div(row.total).unwrap_or(0);
        let state = if row.state == "awaiting_receipt" {
            "待确认回执"
        } else {
            "在途"
        };
        let operation: String = row.operation.chars().take(16).collect();
        let acknowledged = human_bytes(row.acknowledged);
        let total = human_bytes(row.total);
        println!(
            "{operation:<16} {target:<32} {acknowledged:>12} / {total:<12} {percent:>3}%  restarts={restarts}  {state}",
            target = row.target,
            restarts = row.restarts,
            state = state,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use waybill::{
        checkpoint::{Checkpoint, DriverState},
        service::{ServiceId, ServiceIdentity},
        source::SourceIdentity,
        upload::{ConflictPolicy, UploadIntent},
    };

    fn checkpoint(operation: &str, acknowledged: u64, with_receipt: bool) -> Checkpoint {
        Checkpoint {
            version: FORMAT_VERSION,
            intent: UploadIntent {
                operation: operation.into(),
                target: "backup/a.iso".into(),
                conflict: ConflictPolicy::Reject,
            },
            service: ServiceIdentity {
                service: ServiceId::parse("waybill:gdrive").unwrap(),
                instance: "0123456789abcdef".into(),
            },
            source: SourceIdentity {
                reference: "/tmp/a.iso".into(),
                revision: "r1".into(),
                size: 16 * 1024 * 1024,
                blake3: "0".repeat(64),
            },
            acknowledged,
            restarts: 1,
            driver: DriverState {
                version: 1,
                payload: b"session".to_vec(),
            },
            receipt: with_receipt.then(|| waybill::upload::Receipt {
                operation: operation.into(),
                service: ServiceIdentity {
                    service: ServiceId::parse("waybill:gdrive").unwrap(),
                    instance: "0123456789abcdef".into(),
                },
                target: "backup/a.iso".into(),
                object: "obj-1".into(),
                size: 16 * 1024 * 1024,
            }),
        }
    }

    #[test]
    fn scan_decodes_envelope_and_skips_noise() {
        let dir = tempfile::tempdir().unwrap();
        let root: PathBuf = dir.path().into();
        std::fs::write(
            root.join("aaa.json"),
            serde_json::to_vec(&checkpoint("op-aaa", 8 * 1024 * 1024, false)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("bbb.json"),
            serde_json::to_vec(&checkpoint("op-bbb", 16 * 1024 * 1024, true)).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("bbb.lock"), b"").unwrap();
        std::fs::write(root.join("broken.json"), b"{").unwrap();

        let (rows, unreadable) = scan(&root);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].operation, "op-aaa");
        assert_eq!(rows[0].state, "in_flight");
        assert_eq!(rows[1].state, "awaiting_receipt");
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].reason, "decode failed");
    }
}
