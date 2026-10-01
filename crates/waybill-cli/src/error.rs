//! CLI 边界错误与退出码；Display 链路不得引入敏感来源。
use waybill::error::{Error, ErrorKind};

/// 宿主侧错误；库错误的受控上下文直接透出。
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// 库或 service 错误。
    #[error(transparent)]
    Waybill(#[from] Error),
    /// 本机 IO。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// 序列化。
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// CLI 自身说明。
    #[error("{0}")]
    Message(String),
    /// URI 解析。
    #[error("url: {0}")]
    Url(#[from] url::ParseError),
    /// HTTP 传输；错误文本不含请求头或 token。
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}

/// 退出码：用户主动暂停用 130（128+SIGINT），其余失败为 1；用法错误由 clap 用 2。
pub fn exit_code(error: &CliError) -> u8 {
    match error {
        CliError::Waybill(e) if e.kind == ErrorKind::Paused => 130,
        _ => 1,
    }
}
