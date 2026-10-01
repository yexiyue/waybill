//! 与输出方式无关的生命周期事件。
use waybill::transfer::Receipt;
/// 发往输出 sink 的事件流；字段只含可展示数据。
pub(crate) enum Event {
    /// 单个文件开始传输。
    Started {
        index: usize,
        name: String,
        target: String,
        size: u64,
        operation: String,
    },
    /// 引擎进度；persisted 为已持久确认的字节数（上传远端确认、下载本地同步）。
    Progress {
        index: usize,
        persisted: u64,
        sent: u64,
        total: u64,
        epoch: u32,
        complete: bool,
    },
    /// 单个文件取得回执。
    Completed { index: usize, receipt: Receipt },
    /// 单个文件失败；message 为库错误的受控 Display。
    Failed {
        index: usize,
        name: String,
        message: String,
        paused: bool,
    },
    /// 全部队列结束。
    Done {
        receipts: usize,
        failures: usize,
        stopped: bool,
    },
}
