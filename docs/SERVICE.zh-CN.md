# service 开发与宿主接入

当前接口为 0.x 上传与下载原型；架构与兼容策略以 DESIGN.zh-CN.md §11 为准。
Operator、registry 与配置工厂仍属后续设计。

## 公开边界

`Service` 返回带命名空间的 `ServiceIdentity` 与实例能力，读取和上传入口独立。
`Source` 提供完整身份核验与精确范围读；只读 service 的上传默认返回 Unsupported。
外部实现示例见 `examples/consumer/src/bin/readonly.rs`，不依赖任何私有 helper。

上传消费者使用 `UploadEngine::run(source, sink, store, RunOptions { ... })`。
核心只依赖公开端口，异步方法返回 `BoxFuture`，没有 Tokio / reqwest / SwarmDrop 依赖。
Tokio 属于原生 service 与宿主；核心的 Send 约束尚不代表浏览器支持。

`UploadSink` 的责任按生命周期划分：

1. `prepare` 只分配状态和对象 ID，不能创建文件对象；核心先持久保存。
2. `probe` 核对完成对象与服务端进度；结果未知时返回 ResultUnknown，不能假定失败。
3. `initialize` 在已持久状态上初始化上传；重建由核心的显式策略触发。
4. `write_chunk` 接收有界缓冲所有权，返回服务端确认偏移或完成证据，不按本机发送量确认。

最小实现只支持 Source；核心当前上传引擎要求 durable offset upload，其他写入模式
明确拒绝，后续按实际后端增加契约，不为 OSS / WebDAV 写默认成功的假实现。

## 下载源与本地目标

下载方向与上传入口对称：`Service::download_source(reference)` 打开云端源，
`Service::download_target()` 返回本地目标。核心 `DownloadEngine::run(source,
target, store, DownloadOptions)` 驱动；上传与下载共用操作 ID、checkpoint 存储、
错误分类与停止信号，`confirm` 以回执清理记录。

`DownloadSource` 每次调用 `identity()` 都应反映服务端最新元数据（宿主不冻结
云对象）；`read_range` 返回恰好 length 字节。`DownloadTarget` 责任划分：

1. `prepare` 只决定目标侧状态（如冲突后缀名），不创建暂存文件。
2. `probe` 对账 `.part` 与目标位置；发布窗口（目标已存在、回执未落盘）在此
   复原回执，长度不符按冲突处理。
3. `initialize` 创建或重建 `.part`；账本清零后旧内容全部作废。
4. `write_chunk` 在精确偏移写入并同步，不要求调用顺序；返回后区间才可记账。
5. `verify` 按服务端摘要全文件校验；无摘要仅核对长度，回执声明较低级别。
6. `publish` 同盘原子发布并同步父目录；失败保留暂存，重跑只做发布。

`.part` 预分配长度，长度不构成完成证据；完成度以公共信封的区间账本为准。
本地目标当前仅 Linux / macOS；跨设备发布（EXDEV）明确拒绝，不自动复制。
service-fs 的 `LocalTarget` 实现上述契约；Google 原生文档没有二进制内容，
下载源按 InvalidInput 拒绝，不退化为导出。

## 源、恢复与完成

源文件须由宿主冻结，`FileSource` 核验内容哈希、文件版本及路径身份；每块读取
前后检查文件元信息，恢复和返回成功前重新核验内容。不对恶意并发修改者提供快照保证。
源 reference 是路径摘要，公共端口不承载原生路径。

检查点绑定公共格式版本、源、目标、操作 ID 和 service 实例。操作 ID 须跨进程稳定。
未知公共 / 驱动版本、源变化、目标或实例不匹配均保留记录并返回明确错误。
文件型存储使用进程锁，锁文件不删除，防止不同 inode 导致锁失效；根目录须由宿主独占。
存储断电持久性取决于本地文件系统的 fsync / rename 保证；不声称网络盘具有相同保证。

会话过期默认暂停；`UploadPolicy { allow_restart: true }` 明确允许整文件重传，
每次运行最多重建两次。`StopToken::stop()` 只暂停后续块；在途结果先对账。
丢弃运行 future 不等于删除远端操作，下一次运行仍需对账。

成功回执先持久化，再返回。消费者先提交业务账本，然后 `UploadEngine::confirm`。
确认仅清理 checkpoint，不删除源或远端对象。确认后不得复用同一操作 ID 启动新任务；
幂等范围为 checkpoint 保留期间的同一操作，不是跨进程 exactly-once。

## GDrive 实例与凭证

`GdriveConfig` 由稳定账户标识、OAuth 应用标识和目录 ID 组成；切换任何一项都会
改变实例命名空间。Drive `appProperties` 对 OAuth 应用私有，不能跨客户端复用状态。
凭证通过 `TokenProvider` 每次请求获取；401 最多请宿主核对代次并刷新一次，再次拒绝
通知宿主重连。service 不持有 refresh token 或 client secret。

目标是根目录下的相对路径。目录创建 ID 先进入 checkpoint，再请求 Google 创建。
同名现有目录仅在归属标识匹配时复用。文件默认拒绝重名；显式 OperationSuffix 在
prepare 时发现冲突后选择操作后缀，不覆盖。prepare 后新出现冲突仍拒绝，不擅自换名称。
不同进程或独立存储目录同时创建同名 Drive 对象没有原子排他保证。

会话 URI 只允许 Google HTTPS、固定上传路径、443 端口且不含用户信息或片段。
本机 HTTP 替身仅在 cfg(test) 中可用，生产 API 没有自定义端点后门。
哈希 appProperties 是消费者提交的身份数据，不是 Google 校验内容的证明。

## 资源与诊断

多个引擎共享同一 `Arc<ResourceBudget>`：默认 8 MiB 块、最多两条上传，数据缓冲
上限 16 MiB；繁忙返回 ResourceBusy，由宿主决定等待策略。恢复对账可使确认进度回退，
显式重建递增 epoch；sent、acknowledged、persisted 与 complete 分开表达。

文件校验缓冲及 Tokio 文件内部 IO 复制缓冲各限 256 KiB；元数据响应与 checkpoint 各限 1 MiB；Drive 列表最多 1000 项，
目录深度最多 32，没有目录缓存。HTTP 单次超时 60 秒，安全请求最多三次重试，
单次退避最多 30 秒。上传 PUT 网络结果未知时先 probe，避免透明重试错误推进状态。

核心错误使用 thiserror，保留稳定 ErrorKind 与受控静态上下文；必要的底层 source
供诊断，但不得直接打印来源链。reqwest 错误可能带能力 URL，因此 GDrive 边界只
保留脱敏分类。AccessToken、DriverState 与 Checkpoint 的 Debug 不输出秘密。

## 示例与验收

`waybill-consumer-example` 从宿主环境获取短期 token，演示 public API 上传。不要在
命令行参数或仓库配置中放 token。重新执行同一操作会恢复；默认保留回执，确认记账后
才显式加 `--confirm`。该示例不实现 OAuth 刷新，真实应用需注入自己的账户管理器。

`google_probe` 使用 oauth2 crate 演示宿主端授权与真实验收；读取外部 desktop.json，
将凭证保存到仓库外的 0700 目录、0600 文件。auth 打开浏览器，之后 pause 上传一个
生成的 17 MiB 文件并在第一块后暂停；resume 用新进程继续；retry 核对同一完成对象；
confirm 确认后清理检查点。crash 在首块持久化后直接退出（预期退出码 75），
resume-crash 由新进程恢复。lost-response 在真实 Drive 完成后丢弃驱动完成结果，
迫使核心查询真实对象；retry-lost-response 重复对账。large 上传并下载校验 256 MiB 文件。
这类完成结果丢失注入位于消费者驱动边界，不等同于 Google 网络层丢包。
测试目录和源文件保留供审计，不碰用户现有文件。

```sh
cargo run -p waybill-consumer-example --bin readonly
cargo run -p waybill-consumer-example --bin google_probe -- /path/desktop.json /private/probe auth
cargo run -p waybill-consumer-example --bin google_probe -- /path/desktop.json /private/probe pause
cargo run -p waybill-consumer-example --bin google_probe -- /path/desktop.json /private/probe resume
cargo run -p waybill-consumer-example --bin google_probe -- /path/desktop.json /private/probe retry
```

跨进程锁、状态恢复和 HTTP 故障测试见各 crate tests；真实 Google 结果单独见验收记录。
