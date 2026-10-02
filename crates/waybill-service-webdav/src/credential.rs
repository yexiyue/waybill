//! 认证材料由宿主提供；服务不持久化密码或自行进行交互授权。
use std::fmt;
use waybill::BoxFuture;

/// 支持的 HTTP 认证方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authentication {
    /// 不发送认证材料。
    Anonymous,
    /// HTTP Basic。
    Basic,
    /// HTTP Digest，使用认证库计算签名。
    Digest,
}
/// 一次请求使用的认证材料；Debug 不暴露用户名或密码。
#[derive(Clone)]
pub struct Credentials {
    /// 认证方式。
    pub authentication: Authentication,
    /// 用户名；匿名模式可为空。
    pub username: String,
    /// 密码或应用密码；匿名模式可为空。
    pub password: String,
}
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("authentication", &self.authentication)
            .finish_non_exhaustive()
    }
}
/// 每个请求边界重新获取认证材料；刷新和持久化归宿主。
pub trait CredentialProvider: Send + Sync {
    /// 返回当前有效认证材料。
    fn credentials(&self) -> BoxFuture<'_, Credentials>;
}
impl CredentialProvider for Credentials {
    fn credentials(&self) -> BoxFuture<'_, Credentials> {
        Box::pin(async { Ok(self.clone()) })
    }
}
