//! Linux / macOS 的稳定文件源、上传 checkpoint 存储与下载本地目标。
//! 上传源文件由宿主冻结；下载暂存与发布由本 crate 的目标实现承担。
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("waybill-service-fs currently supports Linux and macOS only");
mod checkpoint;
mod source;
pub use checkpoint::{FileCheckpointStore, decode_checkpoint};
pub use source::{FileSource, FsService};
