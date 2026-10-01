//! 不把 OAuth 与其他 service 的凭证强行统一；此端口仅属于 Drive。
use std::{fmt, time::Duration};
use waybill::BoxFuture;
/// 短期 token 租约。Debug 永远不输出 secret。
#[derive(Clone)]
pub struct AccessToken {
    secret: String,
    generation: String,
}
impl AccessToken {
    /// generation 由宿主提供，用于避免旧请求覆盖新授权。
    pub fn new(secret: impl Into<String>, generation: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            generation: generation.into(),
        }
    }
    /// 仅供 HTTP 请求边界读取，禁止日志输出。
    pub fn secret(&self) -> &str {
        &self.secret
    }
    /// 被拒绝的 token 代次，宿主据此决定是否刷新。
    pub fn generation(&self) -> &str {
        &self.generation
    }
}
impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken { <redacted> }")
    }
}
/// 账户管理器是唯一刷新所有者，驱动只请求有效租约。
pub trait TokenProvider: Send + Sync {
    /// 在每个请求边界取得有效 token。
    fn access_token(&self, min_validity: Duration) -> BoxFuture<'_, AccessToken>;
    /// 第一次 401 后，宿主核对代次、刷新并返回租约；同一请求最多调用一次。
    fn after_rejection<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, AccessToken>;
    /// 新租约仍被拒绝时通知宿主重连，禁止旧请求覆盖较新授权。
    fn reconnect_required<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, ()>;
}
