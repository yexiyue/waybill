//! 可恢复交付的开放契约与上传、下载状态机。
//!
//! 上传提供稳定 Source 到 UploadSink 的契约；下载提供云端
//! DownloadSource 到本地 DownloadTarget 的区间账本与发布契约。
//! service 拥有协议和 IO，宿主拥有授权、源文件冻结与业务记账。
//! 完成回执持久化后才能成功返回；会话过期默认保留记录并暂停。
//!
//! ```no_run
//! use waybill::{TransferEngine, UploadOptions, error::Result,
//!     checkpoint::CheckpointStore, source::Source, upload::UploadSink};
//!
//! async fn deliver(store: impl CheckpointStore + 'static, source: &dyn Source,
//!                  sink: &dyn UploadSink) -> Result<()> {
//!     let engine = TransferEngine::new(store);
//!     let receipt = engine.upload(source, sink,
//!         UploadOptions::new("stable-operation-id", "backup/file.zip")).await?;
//!     // 先将 receipt 写入宿主的业务账本，再调用 engine.confirm(&receipt)。
//!     Ok(())
//! }
//! ```
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod budget;
pub mod checkpoint;
pub mod content;
pub mod download;
pub mod error;
pub mod object;
pub mod service;
pub mod source;
pub mod transfer;
pub mod upload;

pub use download::DownloadOptions;
pub use transfer::TransferEngine;
pub use upload::UploadOptions;

use std::{future::Future, pin::Pin};
/// 可对象化且不绑定执行器的异步端口；原生首期要求 Send。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = error::Result<T>> + Send + 'a>>;
