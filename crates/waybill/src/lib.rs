//! 可恢复交付的开放契约与上传状态机。
//!
//! 首期仅提供稳定 Source 到 UploadSink 的上传。下载与本地发布尚未实现。
//! service 拥有协议和 IO，宿主拥有授权、源文件冻结与业务记账。
//! 完成回执持久化后才能成功返回；会话过期默认保留记录并暂停。
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod checkpoint;
pub mod error;
pub mod service;
pub mod source;
pub mod upload;

use std::{future::Future, pin::Pin};
/// 可对象化且不绑定执行器的异步端口；原生首期要求 Send。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = error::Result<T>> + Send + 'a>>;
