//! 跨引擎共享的内存预算；块形状约束归各后端校验。
use crate::error::{Error, ErrorKind, Result};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// 多个引擎共享的资源门禁。繁忙返回 ResourceBusy，宿主负责等待策略。
pub struct ResourceBudget {
    chunk_size: usize,
    active: AtomicUsize,
    concurrency: usize,
}
impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            chunk_size: 8 * 1024 * 1024,
            active: AtomicUsize::new(0),
            concurrency: 2,
        }
    }
}
impl ResourceBudget {
    /// 核心只约束共享内存上界：块 1 B..=32 MiB、并发 1..=16。
    /// 块的对齐与后端专属上限（如 Drive 要求 256 KiB 的倍数且不超过 8 MiB）
    /// 由对应 service 在写入边界自行校验，不进核心预算。
    pub fn new(chunk_size: usize, concurrency: usize) -> Result<Self> {
        if chunk_size == 0 || chunk_size > 32 * 1024 * 1024 || !(1..=16).contains(&concurrency) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid resource budget",
            ));
        }
        Ok(Self {
            chunk_size,
            concurrency,
            active: AtomicUsize::new(0),
        })
    }
    /// 共享上传数据缓冲的最大字节数，不包括有界校验 / 元数据开销。
    pub fn max_data_bytes(&self) -> usize {
        self.chunk_size * self.concurrency
    }
    /// 单块上限；引擎按此值切分读取，宿主可据此预配源缓冲。
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }
    /// 共享并发上界；下载引擎据此限制在途请求数。
    pub fn concurrency_limit(&self) -> usize {
        self.concurrency
    }
    pub(crate) fn acquire(self: &Arc<Self>) -> Result<Permit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.concurrency).then_some(n + 1)
            })
            .map_err(|_| Error::new(ErrorKind::ResourceBusy, "upload budget exhausted"))?;
        Ok(Permit(self.clone()))
    }
}
pub(crate) struct Permit(Arc<ResourceBudget>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}
