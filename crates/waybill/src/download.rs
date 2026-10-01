//! 下载状态机：本地区间账本驱动有界并行补洞，先落盘后记账，发布失败只重试发布。
//!
//! 云端源提供廉价元数据复核与精确范围读取；本地目标按精确偏移随机写
//! `.part` 并在同步后才确认区间。乱序容忍是目标契约的属性，引擎的
//! 并发调度只是它的一个使用者。完成进度以区间账本为唯一事实源，
//! `.part` 长度与输出事件的百分比都不构成完成证据。
use crate::{
    BoxFuture,
    budget::ResourceBudget,
    checkpoint::{Checkpoint, CheckpointStore, DownloadFlow, DriverState, Flow},
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    upload::{ConflictPolicy, Receipt, StopToken, valid_operation},
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 服务端声明的预期内容摘要；算法与值成对出现，不跨算法比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigestAlgorithm {
    /// Drive 等后端返回的 MD5 小写十六进制。
    Md5,
    /// 本库上传契约使用的 BLAKE3 小写十六进制。
    Blake3,
}
/// 服务端提供的预期摘要；缺失时下载只声明长度一致性。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    /// 摘要算法。
    pub algorithm: DigestAlgorithm,
    /// 小写十六进制值；长度由算法决定。
    pub value: String,
}
/// 云端源的当前身份；revision 由 service 内部约定，核心只比较相等。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteIdentity {
    /// 源 service 类型与实例，隔离不同账户、端点和根目录。
    pub service: ServiceIdentity,
    /// 非敏感、稳定的对象引用（如 Drive 文件 ID）。
    pub reference: String,
    /// 版本证据；恢复与完成前必须与首次观测一致。
    pub revision: String,
    /// 精确长度。
    pub size: u64,
    /// 服务端声明的预期摘要；可以为空。
    pub digest: Option<Digest>,
}
impl RemoteIdentity {
    /// 校验引用、版本与摘要形态。
    pub fn validate(&self) -> Result<()> {
        let bounded = |value: &str| !value.is_empty() && value.len() <= 256;
        if !bounded(&self.service.instance)
            || self.service.instance.chars().any(char::is_control)
            || !bounded(&self.reference)
            || !bounded(&self.revision)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid remote identity",
            ));
        }
        match &self.digest {
            Some(digest) => valid_digest_value(digest),
            None => Ok(()),
        }
    }
}
fn valid_digest_value(digest: &Digest) -> Result<()> {
    let expected = match digest.algorithm {
        DigestAlgorithm::Md5 => 32,
        DigestAlgorithm::Blake3 => 64,
    };
    if digest.value.len() != expected
        || !digest
            .value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(Error::new(ErrorKind::InvalidInput, "invalid digest value"));
    }
    Ok(())
}
/// 逐块范围读取的云端源；宿主不冻结云对象，靠 identity 复核版本。
pub trait DownloadSource: Send + Sync {
    /// 实际能力；不支持范围读取的源在开始前被拒绝。
    fn capabilities(&self) -> Capabilities;
    /// 廉价复核并返回当前身份；每次调用都应反映服务端最新元数据。
    fn identity(&self) -> BoxFuture<'_, RemoteIdentity>;
    /// 返回恰好 length 字节；不得超出调用方预算。
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>>;
}
/// 本地暂存对账结果。
#[derive(Debug)]
pub enum DownloadStatus {
    /// 暂存缺失、不匹配或从未初始化；引擎应清空区间账本后 initialize。
    NeedsReset(DriverState),
    /// 暂存可用；`verified` 表示目标侧已确认内容，可跳过重复校验。
    Ready {
        /// 驱动状态。
        state: DriverState,
        /// 目标侧状态是否已通过内容校验。
        verified: bool,
    },
    /// 目标侧已完成（发布窗口对账或重复运行），返回原回执。
    Complete {
        /// 驱动状态。
        state: DriverState,
        /// 完成证据。
        receipt: Receipt,
    },
}
/// 消费者指定的下载意图；target 是宿主解析后的本地目标文件路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadIntent {
    /// 持久稳定的操作 ID；同一操作不得更换源或目标。
    pub operation: String,
    /// 本地目标文件路径；目录展开由宿主完成。
    pub target: String,
    /// 同名目标策略。
    pub conflict: ConflictPolicy,
}
impl DownloadIntent {
    /// 校验操作标识与本地目标路径。
    pub fn validate(&self) -> Result<()> {
        if !valid_operation(&self.operation) || !valid_local_target(&self.target) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid download intent",
            ));
        }
        Ok(())
    }
}
/// 本地目标路径：非空、无控制字符、总长 ≤4096 字节；段非空且不为
/// `.`、`..`，允许以 `/` 起始的绝对路径；反斜杠在 Linux / macOS 是
/// 合法文件名字节，不按远端路径规则禁止。
fn valid_local_target(target: &str) -> bool {
    if target.is_empty()
        || target.len() > 4096
        || target.ends_with('/')
        || target.chars().any(|c| c.is_control())
    {
        return false;
    }
    let mut segments = target.split('/');
    if target.starts_with('/') {
        segments.next();
    }
    segments.all(|segment| {
        !segment.is_empty() && segment != "." && segment != ".." && segment.len() <= 255
    })
}
/// 半开字节区间 `[start, end)`；账本中合并有序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
    /// 起始偏移。
    pub start: u64,
    /// 结束偏移（不含）。
    pub end: u64,
}
/// 区间账本条目上限；并行调度下瞬时条目数不超过并发数 +1，
/// 顺序合并后回到 1 条。超限说明账本被外部写入破坏。
pub const INTERVAL_LIMIT: usize = 4096;
/// 合并插入一个区间；返回 false 表示账本超限。调用方保证区间在文件内。
fn merge_interval(persisted: &mut Vec<Interval>, interval: Interval) -> bool {
    let mut start = interval.start;
    let mut end = interval.end;
    let mut merged = Vec::with_capacity(persisted.len() + 1);
    let mut inserted = false;
    for existing in persisted.iter() {
        if existing.end < start || existing.start > end {
            if !inserted && existing.start > end {
                merged.push(Interval { start, end });
                inserted = true;
            }
            merged.push(*existing);
        } else {
            start = start.min(existing.start);
            end = end.max(existing.end);
        }
    }
    if !inserted {
        merged.push(Interval { start, end });
    }
    if merged.len() > INTERVAL_LIMIT {
        return false;
    }
    *persisted = merged;
    true
}
/// 账本覆盖的字节数。
fn covered(persisted: &[Interval]) -> u64 {
    persisted
        .iter()
        .map(|interval| interval.end - interval.start)
        .sum()
}
/// 账本是否完整覆盖 `[0, size)`。
fn fully_covered(persisted: &[Interval], size: u64) -> bool {
    (size == 0 && persisted.is_empty())
        || (covered(persisted) == size
            && persisted
                .first()
                .is_some_and(|interval| interval.start == 0))
}
/// 找出尚未领取的最小缺失区间首块；`claimed` 为在途区间。
fn next_missing(
    persisted: &[Interval],
    claimed: &[Interval],
    size: u64,
    chunk: u64,
) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let mut combined = persisted.to_vec();
    for interval in claimed {
        if !merge_interval(&mut combined, *interval) {
            return None;
        }
    }
    let mut cursor = 0u64;
    for interval in combined.iter() {
        if interval.start > cursor {
            let end = cursor.saturating_add(chunk).min(size).min(interval.start);
            return Some((cursor, end));
        }
        cursor = cursor.max(interval.end);
    }
    if cursor < size {
        Some((cursor, cursor.saturating_add(chunk).min(size)))
    } else {
        None
    }
}
/// 完成回执记录的内容验证证据；由目标侧按实际校验结果声明。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Verification {
    /// 无内容证据；v1 上传回执的兼容默认值。
    #[default]
    Unverified,
    /// 仅长度一致。
    Length,
    /// 与服务端预期摘要一致。
    Digest {
        /// 摘要算法。
        algorithm: DigestAlgorithm,
        /// 小写十六进制值。
        value: String,
    },
}
/// 不将读取、持久化和发布混为一谈的下载进度。
#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    /// 本次运行尝试读取字节数，重取可超过总长。
    pub read: u64,
    /// 已写入并同步后记账的字节数（区间和）。
    pub persisted: u64,
    /// 源长度。
    pub total: u64,
    /// 回执已持久化；百分比为 100 不代表此标志为 true。
    pub complete: bool,
}
/// 单次下载的意图与宿主控制端口。
pub struct DownloadOptions<'a> {
    /// 稳定下载意图。
    pub intent: DownloadIntent,
    /// 宿主暂停信号。
    pub stop: &'a StopToken,
    /// 有界同步通知；回调不能阻塞或保存数据缓冲。
    pub progress: &'a (dyn Fn(DownloadProgress) + Send + Sync),
}
/// 本地下载目标：`.part` 随机写、同步、校验与最终发布。
///
/// `write_chunk` 按精确偏移寻址且不要求调用顺序——乱序容忍是契约属性。
/// 数据写入并同步之后方法才能返回成功；驱动状态由引擎串行更新。
pub trait DownloadTarget: Send + Sync {
    /// 稳定实例身份。
    fn identity(&self) -> ServiceIdentity;
    /// 实际能力。
    fn capabilities(&self) -> Capabilities;
    /// 仅分配目标侧状态（冲突名等），不创建暂存；核心先保存再 initialize。
    fn prepare<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
    ) -> BoxFuture<'a, DriverState>;
    /// 对账 `.part` 与目标位置；发布窗口（目标已存在）在此对账。
    fn probe<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus>;
    /// 创建或重建 `.part`；复用已持久化的目标侧决策。
    fn initialize<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus>;
    /// 在精确偏移写入并同步；完成后区间才可记账。
    fn write_chunk<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
        offset: u64,
        data: Vec<u8>,
    ) -> BoxFuture<'a, DriverState>;
    /// 校验暂存内容与预期摘要；无可信摘要时核对长度并声明低一致性。
    fn verify<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DriverState>;
    /// 同盘原子发布；失败保留暂存与完成状态，重跑只做发布。
    fn publish<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus>;
}
/// 只依赖公开端口的下载引擎。
pub struct DownloadEngine {
    budget: Arc<ResourceBudget>,
}
impl DownloadEngine {
    /// 注入共享预算；在途读取数与缓冲受 `chunk × concurrency` 约束。
    pub fn new(budget: Arc<ResourceBudget>) -> Self {
        Self { budget }
    }
    /// 开始或恢复下载。数据写入并同步后才记账；回执持久化后才返回成功。
    pub async fn run(
        &self,
        source: &dyn DownloadSource,
        target: &dyn DownloadTarget,
        store: &dyn CheckpointStore,
        options: DownloadOptions<'_>,
    ) -> Result<Receipt> {
        options.intent.validate()?;
        let caps = target.capabilities();
        if !caps.random_write || !caps.durable_publish {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "durable local publish target required",
            ));
        }
        if !source.capabilities().range_download {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "ranged download source required",
            ));
        }
        let lease = store.acquire(&options.intent.operation).await?;
        if options.stop.is_stopped() {
            return Err(Error::new(ErrorKind::Paused, "download paused"));
        }
        let identity = source.identity().await?;
        identity.validate()?;
        let mut flow = match lease.load().await? {
            Some(saved) => {
                if !Checkpoint::supported(saved.version, &saved.flow) {
                    return Err(Error::new(
                        ErrorKind::IncompatibleVersion,
                        "checkpoint version",
                    ));
                }
                match saved.flow {
                    Flow::Upload(_) => {
                        return Err(Error::new(
                            ErrorKind::IdentityMismatch,
                            "checkpoint binding",
                        ));
                    }
                    Flow::Download(flow) => {
                        if flow.intent != options.intent
                            || flow.service != target.identity()
                            || flow.source.service != identity.service
                        {
                            return Err(Error::new(
                                ErrorKind::IdentityMismatch,
                                "checkpoint binding",
                            ));
                        }
                        if flow.source != identity {
                            return Err(Error::new(
                                ErrorKind::SourceChanged,
                                "source identity changed",
                            ));
                        }
                        if flow.persisted.len() > INTERVAL_LIMIT
                            || flow
                                .persisted
                                .iter()
                                .any(|i| i.start >= i.end || i.end > identity.size)
                            || flow
                                .persisted
                                .windows(2)
                                .any(|pair| pair[0].end >= pair[1].start)
                        {
                            return Err(Error::new(ErrorKind::Checkpoint, "invalid saved ledger"));
                        }
                        flow
                    }
                }
            }
            None => {
                let flow = DownloadFlow {
                    intent: options.intent.clone(),
                    service: target.identity(),
                    source: identity.clone(),
                    persisted: Vec::new(),
                    driver: target.prepare(&options.intent, &identity).await?,
                    receipt: None,
                };
                lease.save(&flow.checkpoint()).await?;
                flow
            }
        };
        // 对账：已发布记录直接回执；暂存丢失清账本重建。
        let mut verified = false;
        match target
            .probe(&flow.intent, &flow.source, &flow.driver)
            .await?
        {
            DownloadStatus::Complete { state, receipt } => {
                validate_receipt(&receipt, &flow)?;
                flow.driver = state;
                flow.receipt = Some(receipt.clone());
                lease.save(&flow.checkpoint()).await?;
                report(&flow, 0, true, options.progress);
                return Ok(receipt);
            }
            DownloadStatus::NeedsReset(state) => {
                if flow.receipt.is_some() {
                    return Err(Error::new(
                        ErrorKind::ObjectUnavailable,
                        "published target unavailable",
                    ));
                }
                flow.driver = state;
                flow.persisted.clear();
                lease.save(&flow.checkpoint()).await?;
                if options.stop.is_stopped() {
                    return Err(Error::new(ErrorKind::Paused, "download paused"));
                }
                match target
                    .initialize(&flow.intent, &flow.source, &flow.driver)
                    .await?
                {
                    DownloadStatus::Ready { state, .. } => flow.driver = state,
                    _ => {
                        return Err(Error::new(
                            ErrorKind::Protocol,
                            "target initialization failed",
                        ));
                    }
                }
                lease.save(&flow.checkpoint()).await?;
            }
            DownloadStatus::Ready {
                state,
                verified: ready_verified,
            } => {
                if flow.receipt.is_some() {
                    return Err(Error::new(
                        ErrorKind::ObjectUnavailable,
                        "published target unavailable",
                    ));
                }
                flow.driver = state;
                verified = ready_verified;
                lease.save(&flow.checkpoint()).await?;
            }
        }
        // 有界并行补洞：各流领取最小缺失区间，write_chunk 由引擎串行调用。
        let concurrency = self.budget.concurrency_limit();
        let chunk = self.budget.chunk_size() as u64;
        let mut in_flight = FuturesUnordered::new();
        let mut claimed: Vec<Interval> = Vec::new();
        let mut read = 0u64;
        while !fully_covered(&flow.persisted, flow.source.size) {
            while in_flight.len() < concurrency && !options.stop.is_stopped() {
                let Some((start, end)) =
                    next_missing(&flow.persisted, &claimed, flow.source.size, chunk)
                else {
                    break;
                };
                let Ok(permit) = self.budget.acquire() else {
                    break;
                };
                claimed.push(Interval { start, end });
                let future = source.read_range(start, (end - start) as usize);
                in_flight.push(async move {
                    let data = future.await;
                    (start, end - start, permit, data)
                });
            }
            if in_flight.is_empty() {
                if options.stop.is_stopped() {
                    return Err(Error::new(ErrorKind::Paused, "download paused"));
                }
                return Err(Error::new(
                    ErrorKind::ResourceBusy,
                    "download budget exhausted",
                ));
            }
            let Some((offset, length, _permit, outcome)) = in_flight.next().await else {
                continue;
            };
            // 单流失败立即返回：已记账区间保留，在途读取放弃，洞保持缺失。
            let data = outcome?;
            if data.len() != length as usize {
                return Err(Error::new(ErrorKind::Protocol, "short source range"));
            }
            read = read.saturating_add(length);
            flow.driver = target
                .write_chunk(&flow.intent, &flow.source, &flow.driver, offset, data)
                .await?;
            if !merge_interval(
                &mut flow.persisted,
                Interval {
                    start: offset,
                    end: offset + length,
                },
            ) {
                return Err(Error::new(ErrorKind::Checkpoint, "ledger overflow"));
            }
            claimed.retain(|interval| interval.start != offset);
            verified = false;
            lease.save(&flow.checkpoint()).await?;
            report(&flow, read, false, options.progress);
            if fully_covered(&flow.persisted, flow.source.size) {
                break;
            }
            if options.stop.is_stopped() && in_flight.is_empty() {
                lease.save(&flow.checkpoint()).await?;
                return Err(Error::new(ErrorKind::Paused, "download paused"));
            }
        }
        // 已收齐但请求停止：发布属于后续步骤，保留已校验前的状态。
        if options.stop.is_stopped() {
            return Err(Error::new(ErrorKind::Paused, "download paused"));
        }
        if !verified {
            flow.driver = target
                .verify(&flow.intent, &flow.source, &flow.driver)
                .await?;
            lease.save(&flow.checkpoint()).await?;
        }
        if source.identity().await? != flow.source {
            return Err(Error::new(
                ErrorKind::SourceChanged,
                "source changed during download",
            ));
        }
        let receipt = match target
            .publish(&flow.intent, &flow.source, &flow.driver)
            .await?
        {
            DownloadStatus::Complete { state, receipt } => {
                flow.driver = state;
                receipt
            }
            _ => {
                return Err(Error::new(ErrorKind::Protocol, "publish did not complete"));
            }
        };
        validate_receipt(&receipt, &flow)?;
        flow.receipt = Some(receipt.clone());
        lease.save(&flow.checkpoint()).await?;
        report(&flow, read, true, options.progress);
        Ok(receipt)
    }
    /// 消费者已提交业务记账后，以准确回执删除 checkpoint。
    pub async fn confirm(&self, store: &dyn CheckpointStore, receipt: &Receipt) -> Result<()> {
        let lease = store.acquire(&receipt.operation).await?;
        if let Some(checkpoint) = lease.load().await? {
            if !Checkpoint::supported(checkpoint.version, &checkpoint.flow) {
                return Err(Error::new(
                    ErrorKind::IncompatibleVersion,
                    "checkpoint version",
                ));
            }
            let matched = match &checkpoint.flow {
                Flow::Upload(flow) => flow.receipt.as_ref() == Some(receipt),
                Flow::Download(flow) => flow.receipt.as_ref() == Some(receipt),
            };
            if !matched {
                return Err(Error::new(ErrorKind::IdentityMismatch, "receipt mismatch"));
            }
            lease.remove().await?;
        }
        Ok(())
    }
}
/// 回执必须与当前操作、实例、目标与源长度一致。
fn validate_receipt(receipt: &Receipt, flow: &DownloadFlow) -> Result<()> {
    if receipt.operation != flow.intent.operation
        || receipt.service != flow.service
        || receipt.target != flow.intent.target
        || receipt.size != flow.source.size
        || receipt.object.is_empty()
    {
        return Err(Error::new(
            ErrorKind::Protocol,
            "invalid completion receipt",
        ));
    }
    Ok(())
}
fn report(
    flow: &DownloadFlow,
    read: u64,
    complete: bool,
    progress: &(dyn Fn(DownloadProgress) + Send + Sync),
) {
    progress(DownloadProgress {
        read,
        persisted: covered(&flow.persisted),
        total: flow.source.size,
        complete,
    });
}
