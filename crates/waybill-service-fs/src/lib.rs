//! Linux / macOS 的稳定文件源与上传 checkpoint 存储。
//! 源文件由宿主冻结；本 crate 不接管 P2P 暂存或下载发布。
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("waybill-service-fs currently supports Linux and macOS only");
mod checkpoint;
mod source;
pub use checkpoint::FileCheckpointStore;
pub use source::{FileSource, FsService};
