//! `wb status`：枚举本机恢复记录与已完成回执。
//!
//! 只读公共信封字段；驱动 payload 可能携带会话凭证，绝不渲染。
//! 不获取排他租约：存储的原子替换保证读到旧或新的完整记录。
use crate::{error::CliError, paths::Layout, ui::human_bytes};
use serde::Serialize;
use std::{io::Read, path::Path};
use waybill::checkpoint::{Checkpoint, Flow};

// 与 FileCheckpointStore 的记录大小上限一致，避免损坏文件导致无界读取。
const CHECKPOINT_LIMIT: usize = 1024 * 1024;
const RECORD_LIMIT: usize = 1000;

/// 单条在途会话的展示行；字段与 JSON 输出共用。
#[derive(Serialize)]
pub(crate) struct SessionRow {
    pub(crate) operation: String,
    /// upload = 上传；download = 下载。
    pub(crate) direction: &'static str,
    pub(crate) target: String,
    pub(crate) service: String,
    pub(crate) instance: String,
    /// 上传为服务端确认偏移；下载为已持久化区间和。
    pub(crate) acknowledged: u64,
    pub(crate) total: u64,
    pub(crate) restarts: u32,
    /// in_flight = 传输未完成；completed = 已完成，保留回执用于重复投递对账。
    pub(crate) state: &'static str,
}

/// 无法读取的记录与原因；文件名安全，原因只保留静态分类。
pub(crate) struct Unreadable {
    pub(crate) file: String,
    pub(crate) reason: &'static str,
}

pub async fn run(ui: &mut crate::ui::Session, json: bool, no_tui: bool) -> Result<(), CliError> {
    let dir = Layout::discover()?.checkpoints();
    let unreadable = if !json && !no_tui && crate::ui::interactive_terminal() {
        crate::ui::status::run(ui, || scan(&dir)).await?
    } else {
        let (rows, unreadable) = scan(&dir)?;
        if json {
            println!("{}", serde_json::to_string(&rows)?);
        } else {
            render(&dir, &rows);
        }
        unreadable
    };
    ui.close();
    for entry in &unreadable {
        eprintln!("wb: 无法读取 {}: {}", entry.file, entry.reason);
    }
    if !unreadable.is_empty() {
        return Err(CliError::Message(format!(
            "{} 条运单记录无法读取；结果不完整",
            unreadable.len()
        )));
    }
    Ok(())
}

fn scan(dir: &Path) -> Result<(Vec<SessionRow>, Vec<Unreadable>), CliError> {
    let mut rows = Vec::new();
    let mut unreadable = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((rows, unreadable));
        }
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        if rows.len() + unreadable.len() >= RECORD_LIMIT {
            return Err(CliError::Message(
                "运单记录超过 1000 条；请归档不再需要的本机记录后重试".into(),
            ));
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        let Ok(bytes) = read_checkpoint(&path) else {
            unreadable.push(Unreadable {
                file,
                reason: "read failed or record too large",
            });
            continue;
        };
        match waybill_service_fs::decode_checkpoint(&bytes) {
            Ok(checkpoint) if Checkpoint::supported(checkpoint.version) => match row(&checkpoint) {
                Ok(row) => rows.push(row),
                Err(_) => unreadable.push(Unreadable {
                    file,
                    reason: "invalid progress",
                }),
            },
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
    Ok((rows, unreadable))
}

/// 只读取公共信封字段；下载行 acknowledged 汇总区间账本。
fn row(checkpoint: &Checkpoint) -> waybill::error::Result<SessionRow> {
    Ok(match &checkpoint.flow {
        Flow::Upload(flow) if flow.acknowledged > flow.source.size => {
            return Err(waybill::error::Error::new(
                waybill::error::ErrorKind::Checkpoint,
                "invalid saved offset",
            ));
        }
        Flow::Upload(flow) => SessionRow {
            operation: flow.intent.operation.clone(),
            direction: "upload",
            target: flow.intent.target.clone(),
            service: flow.service.service.as_str().to_string(),
            instance: flow.service.instance.clone(),
            acknowledged: flow.acknowledged,
            total: flow.source.size,
            restarts: flow.restarts,
            state: if flow.receipt.is_some() {
                "completed"
            } else {
                "in_flight"
            },
        },
        Flow::Download(flow) => SessionRow {
            operation: flow.intent.operation.clone(),
            direction: "download",
            target: flow.intent.target.clone(),
            service: flow.service.service.as_str().to_string(),
            instance: flow.service.instance.clone(),
            acknowledged: flow.persisted_bytes()?,
            total: flow.source.size,
            restarts: 0,
            state: if flow.receipt.is_some() {
                "completed"
            } else {
                "in_flight"
            },
        },
    })
}

fn read_checkpoint(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((CHECKPOINT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > CHECKPOINT_LIMIT {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    Ok(bytes)
}

fn render(dir: &Path, rows: &[SessionRow]) {
    println!("运单记录 {} 条  checkpoint: {}", rows.len(), dir.display());
    for row in rows {
        let percent = (u128::from(row.acknowledged) * 100)
            .checked_div(u128::from(row.total))
            .unwrap_or(0);
        let state = if row.state == "completed" {
            "已完成"
        } else {
            "在途"
        };
        let direction = if row.direction == "download" {
            "下载"
        } else {
            "上传"
        };
        let operation: String = row.operation.chars().take(16).collect();
        let acknowledged = human_bytes(row.acknowledged);
        let total = human_bytes(row.total);
        println!(
            "{operation:<16} {direction:<4} {target:<32} {acknowledged:>12} / {total:<12} {percent:>3}%  restarts={restarts}  {state}",
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
        checkpoint::{Checkpoint, DownloadFlow, DriverState, UploadFlow},
        content::{Digest, DigestAlgorithm},
        download::{DownloadIntent, Interval, RemoteIdentity},
        service::{ServiceId, ServiceIdentity},
        source::SourceIdentity,
        transfer::{ConflictPolicy, Receipt},
        upload::UploadIntent,
    };

    fn upload_checkpoint(operation: &str, acknowledged: u64, with_receipt: bool) -> Checkpoint {
        UploadFlow {
            mode: waybill::upload::UploadMode::Offset,
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
            receipt: with_receipt.then(|| Receipt {
                operation: operation.into(),
                service: ServiceIdentity {
                    service: ServiceId::parse("waybill:gdrive").unwrap(),
                    instance: "0123456789abcdef".into(),
                },
                target: "backup/a.iso".into(),
                object: "obj-1".into(),
                size: 16 * 1024 * 1024,
                verified: Default::default(),
            }),
        }
        .checkpoint()
    }

    fn download_checkpoint(operation: &str) -> Checkpoint {
        DownloadFlow {
            intent: DownloadIntent {
                operation: operation.into(),
                target: "/tmp/restore/a.iso".into(),
                conflict: ConflictPolicy::Reject,
            },
            service: ServiceIdentity {
                service: ServiceId::parse("waybill:fs").unwrap(),
                instance: "local".into(),
            },
            source: RemoteIdentity {
                service: ServiceIdentity {
                    service: ServiceId::parse("test:remote").unwrap(),
                    instance: "source-account".into(),
                },
                reference: "drive-file-1".into(),
                revision: "3:abc:16".into(),
                size: 16 * 1024 * 1024,
                digest: Some(Digest {
                    algorithm: DigestAlgorithm::Md5,
                    value: "0".repeat(32),
                }),
            },
            persisted: vec![Interval {
                start: 0,
                end: 8 * 1024 * 1024,
            }],
            driver: DriverState {
                version: 1,
                payload: b"part".to_vec(),
            },
            receipt: None,
        }
        .checkpoint()
    }

    #[test]
    fn scan_decodes_envelope_and_skips_noise() {
        let dir = tempfile::tempdir().unwrap();
        let root: PathBuf = dir.path().into();
        std::fs::write(
            root.join("aaa.json"),
            serde_json::to_vec(&upload_checkpoint("op-aaa", 8 * 1024 * 1024, false)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("bbb.json"),
            serde_json::to_vec(&upload_checkpoint("op-bbb", 16 * 1024 * 1024, true)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("ccc.json"),
            serde_json::to_vec(&download_checkpoint("op-ccc")).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("bbb.lock"), b"").unwrap();
        std::fs::write(root.join("broken.json"), b"{").unwrap();

        let (rows, unreadable) = scan(&root).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].operation, "op-aaa");
        assert_eq!(rows[0].state, "in_flight");
        assert_eq!(rows[1].state, "completed");
        assert_eq!(rows[2].direction, "download");
        assert_eq!(rows[2].acknowledged, 8 * 1024 * 1024);
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].reason, "decode failed");
    }

    /// 旧平铺记录作为不可读条目保留，不伪装成当前格式。
    #[test]
    fn scan_reports_legacy_flat_records_without_deleting_them() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = format!(
            "{{\"version\":1,\"intent\":{{\"operation\":\"legacy\",\"target\":\"a.bin\",\
             \"conflict\":\"Reject\"}},\"service\":{{\"service\":\"waybill:gdrive\",\
             \"instance\":\"inst\"}},\"source\":{{\"reference\":\"/tmp/a\",\
             \"revision\":\"r\",\"size\":10,\"blake3\":\"{}\"}},\"acknowledged\":4,\
             \"restarts\":0,\"driver\":{{\"version\":1,\"payload\":[]}},\"receipt\":null}}",
            "0".repeat(64)
        );
        std::fs::write(dir.path().join("legacy.json"), v1).unwrap();
        let (rows, unreadable) = scan(dir.path()).unwrap();
        assert!(rows.is_empty());
        assert_eq!(unreadable.len(), 1);
        assert!(dir.path().join("legacy.json").exists());
    }

    #[test]
    fn missing_directory_is_empty_but_invalid_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(scan(&missing).unwrap().0.is_empty());
        let file = dir.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(scan(&file).is_err());
    }

    #[test]
    fn record_inventory_has_a_bound() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..=RECORD_LIMIT {
            std::fs::write(dir.path().join(format!("{index}.json")), b"{").unwrap();
        }
        assert!(scan(dir.path()).is_err());
    }

    #[test]
    fn oversized_records_are_reported_without_unbounded_reading() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("large.json"),
            vec![b' '; CHECKPOINT_LIMIT + 1],
        )
        .unwrap();
        let (rows, unreadable) = scan(dir.path()).unwrap();
        assert!(rows.is_empty());
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].reason, "read failed or record too large");
    }
    #[test]
    fn corrupt_download_progress_is_reported_without_panicking_or_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let mut checkpoint = download_checkpoint("bad-progress");
        let Flow::Download(flow) = &mut checkpoint.flow else {
            panic!("download flow")
        };
        flow.persisted = vec![Interval {
            start: u64::MAX,
            end: 0,
        }];
        let path = dir.path().join("bad.json");
        std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let (rows, unreadable) = scan(dir.path()).unwrap();
        assert!(rows.is_empty());
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].reason, "invalid progress");
        assert!(path.exists());
    }
}
