# waybill 设计文档 —— 云端与本地之间的可恢复双向传输

> 起于 2026-10-01 的孵化讨论（源头是 SwarmDrop issue #132 的三后端集成），同日建仓。
> 状态：🟢 **M2 原型已验收 —— GDrive 上传、下载与持久恢复，未发布**
> 仓库：<https://github.com/yexiyue/waybill>
> 命名：`waybill`（运单）——回执、在途追踪与交付凭证合一；crates.io 已核未占用
> （`portage` 被 Gentoo 占用出局；`wildgoose` 有 wild-goose-chase 习语陷阱）。
> 与 SwarmDrop 的关系：issue #132 三后端（gdrive / webdav / oss）的云交付需求，
> 加上 host-fs 已有的本地暂存与发布能力，是这个库的起点；SwarmDrop 是首个消费者。
> 关联：SwarmDrop issue #132（云后端决策全记录）、openspec 变更 `cloud-oauth-login` /
> `storage-cloud-gdrive`、SwarmDrop 仓 `dev-notes/knowledge/storage-abstraction.md`。

---

## 0. 先看结论

本轮调研的候选库提供了存储访问、分片和协议客户端等基础能力，但没有直接
满足 SwarmDrop 所需的三后端持久恢复与交付记账。`object_store` 的低层
multipart API 已公开上传 ID，值得参考；这里的缺口需要按具体 API 判断，
不能归结为整个生态或维护者拒绝持久恢复。

建立独立 Rust 库：**自研统一契约和可恢复传输引擎，复用 HTTP、签名与
协议客户端，原生深做少量后端，按需桥接 OpenDAL。** 增强点聚焦跨进程恢复、
有界内存、进度与完成对账，覆盖两个方向：

- **本地 → 云端**：本地范围读取 → 有界调度 → 云上传会话 → 完成回执。
- **云端 → 本地**：云对象版本与范围读取 → 有界调度 → `.part` 随机写入 → 校验与发布。

**从第一天起，OSS、GDrive、WebDAV 与本地文件后端都是独立 service。** 我们
维护的 service 与外部开发者维护的 service 使用相同公开契约，只依赖 core；
增加后端不需要修改核心枚举或合入主仓库。OpenDAL 适配器也遵循这套契约，
在首个实际额外后端需求出现时再实现，优先验证云端下载。

`host-fs` 的通用文件部分进入独立库的本地后端；身份、配对、设备配置和
SwarmDrop 业务适配继续留在应用。当前可复用素材是已落地的 Google Drive
实现与本地文件实现，WebDAV / OSS 仍是计划。市场需求与后端验收需要 spike
验证；已实现本地稳定源到 GDrive 的上传恢复与云端到本地的下载发布；WebDAV 与 OSS 后移。真实故障恢复需独立验收，不继承 SwarmDrop 正常上传探针的结论。

## 1. 动机：三要件是怎么被逼出来的

issue #132 的云目的地需求暴露了现有访问层与交付层之间的差距：

1. **OpenDAL gdrive**：writer 是 `OneShotWriter`（普通上传），写入请求不收用户
   元数据，不暴露 Drive resumable session——标准 writer 不能直接满足这里的
   持久恢复与交付契约。
2. **OpenDAL webdav**：writer 同样 `OneShotWrite`；认证只有 basic/bearer，
   没有 digest——需要补充流式客户端与 Digest 支持，再验收 NAS 矩阵
   （群晖/威联通/坚果云/Nextcloud），不能仅凭服务名声明兼容。
3. **OpenDAL OSS**：multipart 原语齐全，但标准 writer 的上传 ID 与分片列表
   保存在内部；当前公共 writer 契约没有导出和恢复持久会话的入口。
4. **生态其余选项**（见 §3 表）：object_store 提供低层分片会话，OSS 社区
   SDK 提供原生 multipart 操作，reqwest_dav 提供 WebDAV 协议客户端；持久
   checkpoint、源对象校验和应用记账仍需单独编排。

SwarmDrop 的接收管道对云目的地提出的三条硬要求（**三要件**）：

| 要件 | 含义 | 为什么 |
|---|---|---|
| 元数据幂等 | 以回执键（receipt key）查重，命中即视为已完成 | 完成 response 丢失后重试不得产生重复对象；批量上传部分失败重试要能跳过已传文件 |
| 上传恢复 | 上传会话状态可持久化，崩溃重启后先对账服务端已收字节再续传 | 桌面应用随时会被杀；弱网下重传整个大文件不可接受（WebDAV 协议天花板除外，诚实降级） |
| 内存上界 | 流式读写暂存，禁止无界缓冲 | 传输是常驻 actor，一次 OOM 带走整条链路 |

加第四条要求：**凭证注入**——OAuth 后端由账户层提供短期 access token；
WebDAV 接收用户名 / 密码，OSS 接收 AK / STS 凭证，不把它们伪装成 OAuth。
refresh_token / client_secret 与交互授权由宿主拥有，同一凭证只由一个组件
负责刷新，驱动在请求边界获取有效凭证。

云端作为源时，还需要持久化源对象身份、版本与已写区间，并在恢复前确认
来源未变。本地文件最终发布失败时保留暂存，使重试只重做发布。

## 2. 为什么需要独立交付层（待验证的判断）

按孵化期的规矩，检索未发现直接满足需求的库，不足以证明没有等价物
或市场存在空白。以下是当前的架构判断，需要后续消费者与原型验证：

1. **访问操作与恢复任务的生命周期不同。** 统一 read / write API 可以隐藏
   协议差异；跨进程恢复还需要保存源身份、服务端会话、已确认进度和完成状态。
   这部分适合独立状态机，并可以复用已有访问层。
2. **业务恢复规则有真实抽取成本。** 上传对账、下载分块位图、本地发布和
   应用数据库提交的边界不同；需要先分清通用引擎与消费者业务，才能库化。
3. **WebDAV 服务器兼容矩阵是「谁做谁知道」的持续成本。** 库化意味着承诺
   长期扛群晖/威联通/坚果云/Nextcloud 的差异；应用内部做只需覆盖自己的
   用户碰到的服务器。
4. **已有协议能力可以复用。** reqwest_dav、OSS SDK 与 reqsign 减少底层维护量；
   库的价值要由双向恢复与本地发布的完整链路证明，避免重复建设完整云 SDK。

## 3. 生态调研结论（2026-10-01）

| 库 | 已有能力 | 需要补充的交付能力 | 用法建议 |
|---|---|---|---|
| OpenDAL | 多后端访问、范围读取、能力描述、重试与并发；OSS multipart | 标准 writer 的持久会话恢复；WebDAV 流式上传与 Digest 差异 | 可选访问后端与组织方式参考 |
| object_store 0.14.2 | `MultipartStore` 公开上传 ID、分片 ID、完成 / 取消接口 | 接口没有 `list_parts` 和 checkpoint 持久化；不直接覆盖三后端组合 | 参考低层会话设计，按需求适配 |
| reqwest_dav 0.3.3 | Basic / Digest、PROPFIND；PUT 接收 `reqwest::Body` | 请求重建、服务端兼容、完成对账；普通 WebDAV 不承诺上传续传 | WebDAV 客户端候选，可传入流式请求体 |
| ali-oss-rs 0.2.5 等社区 SDK | 原生 multipart 初始化、分片、ListParts、完成 / 取消；文件区间上传 | checkpoint、调度、凭证生命周期；依赖日志与错误处理需审查 | OSS 驱动候选，尚未完成生产选型 |
| reqsign-aliyun-oss | OSS 签名与凭证提供方，支持 V4 / STS | HTTP 协议操作与恢复编排 | 配合 reqwest 实现必要的薄适配器，复用签名算法 |

一手资料：

- [OpenDAL WebDAV 实现](https://opendal.apache.org/docs/rust/src/opendal_service_webdav/backend.rs.html)
  与 [multipart writer](https://opendal.apache.org/docs/rust/src/opendal_core/raw/oio/write/multipart_write.rs.html)
  （另核对发布包 0.59.3；网站源码可能领先于发布版）。
- [object_store MultipartStore](https://docs.rs/object_store/0.14.2/object_store/multipart/trait.MultipartStore.html)。
- [reqwest_dav Client](https://docs.rs/reqwest_dav/0.3.3/reqwest_dav/struct.Client.html)。
- [ali-oss-rs Client](https://docs.rs/ali-oss-rs/0.2.5/ali_oss_rs/struct.Client.html)。
- [reqsign-aliyun-oss](https://docs.rs/reqsign-aliyun-oss/3.2.0/reqsign_aliyun_oss/)。
- [Nextcloud 分块上传扩展](https://docs.nextcloud.com/server/stable/developer_manual/client_apis/WebDAV/chunking.html)。
- [OpenDAL Reader](https://opendal.apache.org/docs/rust/opendal/struct.Reader.html)、
  [Capability](https://opendal.apache.org/docs/rust/opendal/struct.Capability.html) 与
  [Writer](https://opendal.apache.org/docs/rust/opendal/struct.Writer.html)。

以上是 API / 源码调研，真实服务端兼容性、吞吐量与崩溃恢复仍待原型验收。

## 4. 定位与范畴校准

定位为独立的双向交付库，复用 OpenDAL 等访问层：

| | OpenDAL / object_store | 本库 |
|---|---|---|
| 范畴 | 统一的对象**访问**层 | 云端与本地之间的可靠**交付**层（兼访问面） |
| 核心价值 | 一套 API 访问多个后端，按能力提供操作 | 上传 / 下载跨进程恢复、重试对账、本地暂存与发布 |
| 差异处理 | 统一操作与能力描述 | **类型化差异**，进一步描述恢复与发布策略 |
| 类比 | 文件系统 API | rsync / rclone 的传输引擎（库形态） |

把上传恢复、范围读取、源版本校验、随机写入与最终发布分别做成 capability，
调用方按矩阵选择策略。Drive ID、OSS key、WebDAV 路径用后端对象引用表达，
不强制模拟 POSIX 文件系统。回执键是消费者提供的通用操作 ID，不能把设备、
会话或收件箱字段固化成公共类型。

远端完成、本地 checkpoint 和应用数据库之间没有统一事务。库提供持久回执
与重试对账机制，具体幂等保证由后端能力决定，不承诺跨后端 exactly-once。

## 5. 设计方案

### 5.1 访问面与双向交付面

访问面由具体 service 提供对象查询、目录列表与范围读取；交付面围绕明确的
源、目标与 `TransferEngine` 组织。引擎持有 checkpoint 存储与共享预算，上传、
下载分别通过 `upload` / `download` 开始或恢复，完成后统一通过 `confirm` 清理。
无需 Operator、registry 或运行时后端枚举；宿主直接注入已构造的公开端口。

```rust
use waybill::{TransferEngine, UploadOptions, DownloadOptions};

let engine = TransferEngine::new(checkpoint_store);
let uploaded = engine.upload(source.as_ref(), sink.as_ref(),
    UploadOptions::new("stable-upload-operation", "backup/file.zip")).await?;
let downloaded = engine.download(remote.as_ref(), local.as_ref(),
    DownloadOptions::new("stable-download-operation", "/absolute/file.zip")).await?;
// 宿主先提交业务账本，再分别 engine.confirm(&receipt).await?。
```

选项构造器默认拒绝同名目标、不重建过期会话、不订阅进度。需要取消或进度时，
宿主通过 `options.stop` 与 `options.progress` 显式接入；`StopToken` 是可克隆的
共享停止信号，回调引用只在调用期间借用，不要求宿主先建立后台事件任务。
CLI 的有界事件通道属于宿主，核心不强制绑定 UI 或异步消息通道。

`transfer` 拥有引擎资源、回执、冲突策略与停止信号；`content` 拥有摘要与验证
证据。`upload` 与 `download` 各自拥有意图、进度、端口和状态机，互不引用
对方的类型。共享确认逻辑不依赖传输方向。

稳定上传 `Source` 的身份核验包含完整内容摘要，宿主冻结源；云端
`DownloadSource` 的身份核验查询当前版本，宿主不冻结远端对象。两者具有
不同的成本与一致性责任，因此保留两个明确端口，不强行合并为一种 reader。

checkpoint 由公共版本化信封与驱动私有状态组成：信封包含操作 ID、后端 / 账户
命名空间、源身份与进度，驱动状态保存 session URI 或 upload_id / 分片列表。
消费者不依赖私有状态结构。由 `CheckpointStore` 负责持久化，文件或数据库
实现由使用者选择；session URI 等敏感状态需要适当保护，0600 只是文件实现
的一项要求。

双向数据流：

```mermaid
flowchart LR
    LS["本地文件 Source：范围读取"] --> TE["Transfer：有界调度、恢复、取消、进度"]
    CS["云对象 Source：版本校验、范围读取"] --> TE
    TE --> FS["本地 Sink：.part、随机写入、发布"]
    TE --> US["云 Sink：偏移续传、分片或整文件写入"]
    TE --> CP["CheckpointStore：持久恢复记录"]
```

| 方向 | 源端契约 | 目标端契约 |
|---|---|---|
| 本地 → 云端 | 稳定源身份、精确范围读取 | 上传会话、远端进度对账、完成回执 |
| 云端 → 本地 | 对象身份 / 版本、范围读取或顺序流 | 暂存、按偏移写入、同步、校验后发布 |

首期聚焦这两条链路。云对象作为 P2P 发送源时，SwarmDrop 通过读取端口桥接；
P2P 网络协议与 bao 验签继续归传输域。不可重读的流若要恢复，需要额外暂存
或声明只能重新开始，不能套用稳定随机读源的恢复保证。

### 5.2 能力模型：类型化降级

| 能力 | 策略或约束 |
|---|---|
| 上传方式 | Drive 连续偏移、OSS 分片、普通 WebDAV 整文件流式写入；Nextcloud 分块扩展单独适配 |
| 上传恢复 | Durable / Restart / Unsupported，按实际驱动能力声明 |
| 源读取 | 范围读取 / 顺序流；声明是否支持对象版本条件校验 |
| 目标写入 | 本地精确偏移随机写 / 连续写 / 分片提交，不用一个 writer 伪装全部行为 |
| 幂等对账 | 元数据回执、确定性对象键、服务端条件操作等；明确可验证范围 |
| 发布 | 同文件系统重命名、远端完成操作、复制到外部位置；分别声明可见性与持久性保证 |

WebDAV 的 MOVE 支持及其实际发布保证需要按服务端验收；对象长度与 ETag
只能作为一致性证据，不能普遍证明内容相同。同 key 覆盖也不能自动防止并发
上传相互覆盖或完成状态不明，必须结合操作 ID、对象版本或预期校验和对账。

OpenDAL 桥接层只暴露实际支持的能力，不统一假定所有后端不可恢复，也不
因为支持范围读取就承诺远端上传会话恢复。

### 5.3 独立仓库的 crate 分层

起步采用一个 Cargo workspace，核心与各 service 独立成 crate；第三方 service
可以在其他仓库独立发布。下列名称仅作示意。各包按实际里程碑创建，提前确定
依赖边界，首期无需为尚未实现的后端创建空包。

| crate | 职责 | 依赖 |
|---|---|---|
| waybill | Service / Source / Sink 契约、capability、访问入口、传输状态机与调度、进度、CheckpointStore、结构化错误；凭证端口属于具体 service | 不依赖 service、SwarmDrop、HTTP 或本地文件实现 |
| waybill-service-fs | 本地范围读取、随机写入、暂存、同步与最终发布 | core + 原生文件实现，按 target / feature 隔离 |
| waybill-service-gdrive | Drive 对象访问、偏移上传会话与恢复对账 | core + HTTP / 协议依赖 |
| waybill-service-webdav | WebDAV 访问、流式上传、服务端差异与可选分块扩展 | core + WebDAV / HTTP 依赖 |
| waybill-service-oss | OSS 访问、分片会话与恢复对账 | core + SDK 或 HTTP / 签名依赖 |
| waybill-service-opendal（条件扩展） | 已配置 Operator 的访问与交付能力适配 | core + OpenDAL；有实际需求再创建 |
| 外部 service crate | 新后端或现有后端的其他实现 | core + 自选协议依赖；可以独立发布 |

```mermaid
flowchart TB
    APP["消费者组装：SwarmDrop / CLI / 其他应用"] --> CORE["waybill：公开契约、访问入口、双向传输引擎"]
    APP --> FS["service-fs"]
    APP --> GD["service-gdrive"]
    APP --> WD["service-webdav"]
    APP --> OSS["service-oss"]
    APP --> EXT["外部 service / 可选 service-opendal"]
    FS --> CORE
    GD --> CORE
    WD --> CORE
    OSS --> CORE
    EXT --> CORE
    HOST["宿主凭证与 checkpoint 实现"] --> CORE
    CONF["公开契约验收工具"] -.-> FS
    CONF -.-> GD
    CONF -.-> WD
    CONF -.-> OSS
    CONF -.-> EXT
```

独立库的公共契约不依赖 SwarmDrop、Tauri、Specta、收件箱或设备身份模型。
端口与状态机保持平台中立；原生 service 先落地，浏览器的存储、权限与持久化
保证单独声明，不能把 native-only 实现包装成所有 target 都可用。

模块按 object、upload、download、checkpoint、credential、drivers 等职责组织，
类型与行为放在所属模块。库拥有调度和恢复，协议驱动只执行其能力范围内的
操作；HTTP / TLS、签名与 XML 解析优先复用。OpenDAL 桥接减少后端客户端
维护量，新增后端的交付保证仍需单独验收。

未来若提供聚合入口包，可以用 feature 重导出常用 service；它只负责便利性，
不持有后端实现或封闭分派逻辑。service 开发者不必依赖这个入口包。

### 5.4 三后端语义对照（= 各 checkpoint 模块的需求差异）

| | 恢复机制 | 发布与完成确认 | 幂等判据 |
|---|---|---|---|
| Google Drive | resumable session URI + 服务器字节数查询 | 完成后确认对象，响应丢失时重新查询 | appProperties 回执键与对象 ID 对账 |
| 普通 WebDAV | 重试重传整文件；Nextcloud 等扩展另行声明 | PUT 临时对象 → MOVE，实际保证按服务端验收 | 操作对应路径、长度 / ETag 等证据；无可信哈希时不宣称内容一致 |
| OSS | multipart upload_id + ListParts 对账 | CompleteMultipartUpload 后查询完成状态 | 确定性 key + 操作身份 / 预期校验和 / 对象版本，不能只凭同 key |

### 5.5 云端 → 本地：恢复与发布生命周期

1. 获取云对象引用、长度、版本 / 校验和，生成通用传输 ID。对象 ID 和版本
   分开记录，恢复时确认版本未变；不支持条件读取的后端降低一致性保证。
2. 创建或重新打开本地 `.part`，关联源身份与已完成区间。不能仅凭文件长度
   判断已收齐，因为暂存可能已预设长度而尚有未写区间。
3. 对缺失区间做有界范围预读，按精确偏移写入；不支持 Range 时采用顺序
   下载或整文件重传。并发数、分片大小与预读窗口共同约束内存及在途任务。
4. 数据写入并同步后，才持久记录相应区间已完成；崩溃窗口中未确认的数据
   可以重读重写，不能让 checkpoint 宣称尚未持久化的数据已完成。
5. 校验完成后进入可发布状态。有可信预期哈希时验证内容；P2P 路径可以接收
   bao 已完成校验的结果；普通云下载若只有长度 / 版本，只声明相应一致性
   级别，ETag 不统一视作内容哈希，也不把本地算出的哈希当作来源证明。
6. 发布到用户目标位置：同文件系统走重命名，跨文件系统或系统文档提供方
   走明确的复制策略。关闭 / 排空写任务、同步与发布的顺序需由契约保证。
   发布失败保留暂存与完成状态，只重试发布，避免再次下载整个文件。
7. 持久化完成回执，由消费者确认业务记账后清理恢复状态。最终路径 / URI
   以目标实现返回值为准；发布成功但回执未写入的窗口需能查询对账。

上传和下载共用操作身份、checkpoint 存储、错误分类、取消与进度模型；
恢复状态分别保存远端已确认偏移 / 分片，以及本地已持久化区间。区分已发送
或已读取字节、确认持久化字节与最终完成，不让进度 100% 代替发布成功。

### 5.6 从 host-fs 抽取通用文件能力

现有素材（位于 SwarmDrop 仓库）：`crates/host-fs/src/local_fs/mod.rs`、
`crates/host-fs/src/local_fs/part_file.rs`、`crates/host-fs/src/local_fs/sink_ops.rs`。
它们已实现本地范围读取、精确偏移写入、`.part` 重开与发布；完整的下载
恢复 / 校验链路仍需新增。

| 后续抽取到 service-fs（首期仅稳定源与 checkpoint） | 保留在 SwarmDrop |
|---|---|
| 范围读取、按偏移写入、暂存创建 / 重开、同步、发布与清理 | 身份、配对设备、设备配置的 JSON 存储 |
| 通用路径边界检查与目标冲突策略 | FileAccess 端口适配、CoreSaveLocation 和 UI 选址 |
| 通用操作 ID、源身份、恢复记录与完成回执 | P2P、bao、ReceiveFileIdentity、收件箱记账 |

`LocalFileAccess` 后续变成新库的薄适配层，现有暂存与发布保证必须保留。
Android SAF / iOS 外部位置在发布阶段适配，不能假定其句柄适合随机写；
Web 存储也有独立实现与能力声明。

当前 SwarmDrop 仓 `crates/storage-cloud/src/file_access.rs` 的
source_metadata / read_source_chunk 仍委托本地实现，云对象作为发送源尚未
接入。后续同时补云读取适配和本地接收适配，避免独立库只覆盖上传一半。

### 5.7 第三方 service 的公开扩展契约

扩展性是首期设计要求。官方维护的 service 也只通过公开 API 接入，不使用
core 私有模块、特殊分支或后门。外部开发者应能创建一个只依赖 core 的 crate，
完成自己的协议实现，再由消费者直接注入统一入口与传输引擎。

| 扩展点 | 开发者需要实现或提供的内容 |
|---|---|
| Service / ServiceInfo | 稳定 service 标识、实例命名空间、当前配置下的 capability，构造 Source / Sink 的入口 |
| Source | 元信息、顺序或范围读取、可用的源版本校验；只实现声明支持的模式 |
| Sink / 上传会话 | 对应连续偏移、分片或整文件模式的写入、完成与取消；持久恢复作为独立可选契约 |
| 恢复协议 | 私有状态的版本化编码 / 解码、服务端对账、会话过期与结果未知的处理 |
| 对象访问 | 可选 stat / list / delete 等操作；未支持操作返回结构化 Unsupported |
| 凭证与请求配置 | typed builder 与凭证提供方；HTTP 连接配置由具体 service 暴露，核心不绑定某个 SDK |
| 错误映射 | 转换为 core 的稳定错误类别，同时保留可诊断的后端来源与请求标识 |

最小 service 可以只支持读取；支持上传后再按协议加入恢复与对账契约。不能
要求每个开发者实现所有方法，也不能用默认成功掩盖未实现能力。capability
反映当前实例与服务端实际保证，传输策略要求的能力不足时，在开始前明确拒绝；
只有消费者显式允许时才采用整文件重传等降级策略。

为避免扩展受主仓库限制，公共模型需要满足：

- **标识开放**：ServiceId 使用带命名空间的可校验标识，不把 provider 封闭
  为 Gdrive / Webdav / Oss 枚举；新 service 无需给 core 增加 enum 分支。
- **契约可在外部实现**：扩展 trait 不 sealed，必需的参数、结果类型和构造器
  均公开。核心调度只依赖契约与能力，不能按 service 名称切换算法。
- **对象与恢复状态可携带厂商信息**：对象引用、checkpoint 公共信封包含
  service / 实例身份和格式版本，厂商定位信息与私有状态保持不透明。恢复
  不匹配、缺失驱动或版本不兼容时返回明确错误并保留记录。
- **实例边界明确**：同一 service 的不同账户、端点、bucket 或根目录必须
  隔离恢复记录与对象引用；私有状态不进入通用日志。
- **写入模式分开扩展**：偏移、分片、整文件与本地随机写遵循各自契约。
  支持新后端可复用现有模式；出现新的传输语义时，通过演进核心契约承载。

Rust 宿主直接构造具体 service，取得公开端口后注入 `TransferEngine`。
最小只读 service 可只实现 `Service::source`，其余默认入口明确返回 Unsupported。
动态后端选择由宿主通过 `Arc<dyn Service>` 完成；凭证接口属于具体 service。
当前不提供 registry、配置工厂或动态共享库 ABI；出现实际配置宿主需求时再设计，
不要求外部开发者先依赖尚不存在的统一访问包装。

core、service 契约及公共信封使用明确的版本兼容规则，新增能力通过独立可选
契约或兼容的默认 Unsupported 演进；稳定版破坏性修改提升主版本，0.x 阶段
明确次版本兼容边界与迁移说明。具体 Rust trait
的异步形式、类型擦除方式与跨 target 约束首期采用对象安全 boxed future，确保配置构造出的实例
也能注入引擎，避免只有编译期泛型路径可用。

### 5.8 第三方 service 的开发与验收入口

从 M0 准备最小只读 service 示例、完整会话示例和 service 开发指南；在 M1
用独立消费者 crate 演示仅通过公开 API 接入，作为公开边界的验收依据。
共享契约验收工具按声明的能力选择场景，不强制只读 service 通过上传测试。

开发者接入流程：

1. 新建 service crate，依赖兼容版本 core，实现 typed builder 与实例能力。
2. 实现需要的 Source / Sink 和协议操作，映射结构化错误。
3. 按能力实现 checkpoint 与恢复对账；复用核心调度、持久化和进度模型。
4. 使用公开验收工具检查范围读取、源版本、写入 / 同步顺序、取消与资源上界。
   声明持久恢复的 service 还需覆盖重启、凭证变化、会话过期和完成响应丢失。
5. 发布独立 crate，并提供支持的平台、协议扩展、恢复保证及已验证服务端矩阵。
   消费者直接注入或显式注册，无需等待主仓库收录。

## 6. 消费者场景（库存在的产品理由）

| 消费者 | 用法 | 对库的约束 |
|---|---|---|
| SwarmDrop 桌面 / CLI | 接收目的地落云、云对象作为发送源、本地暂存与发布 | 会话 / 凭证分层；宿主端口适配；capability 驱动 UI 文案 |
| 独立 CLI 工具 | 批量上传、云文件下载到本地目录、弱网下重启恢复 | URI scheme 表达后端与对象；TTY 进度；凭证配置；目标冲突策略 |
| ComfyUI / agent | 生成的图片上传云端：CLI 子进程（最便宜）或 MCP server（agent 原生，`cloud_upload(files, destination_uri)` 一个工具参数即 URI） | **批量小文件是一等公民**：receipt 幂等让批量部分失败重试自动跳过已完成项 |
| CI / 无头场景 | 宿主提供 OAuth token、WebDAV 密码或 OSS AK / STS 凭证 | OAuth 流程与秘密持久化归应用层，库通过凭证接口执行请求 |

## 7. 里程碑

```mermaid
flowchart LR
    M0["M0 开放契约与 checkpoint"] --> M1["M1 本地稳定源 → GDrive
上传、重启对账、完成回执"]
    M1 --> M2["M2 GDrive 下载
.part 恢复与本地发布"]
    M2 --> M3["M3 WebDAV
流式上传与真实服务端矩阵"]
    M3 --> M4["M4 OSS
分片恢复与完成对账"]
    M1 -.-> APP["SwarmDrop 正式回接
另立 OpenSpec"]
    NEED["额外后端的实际需求"] -.-> OD["可选 OpenDAL service"]
```

M1 交付独立库与公开 API 消费者示例，不替换 SwarmDrop 主线。首期验证 Linux / macOS，
rust-version 1.88（2026-10-01 起统一，不再维护更低版本）。API 仍为 0.x 原型；完整稳定化须经过双向链路与更多后端验证。
真实 GDrive 重启、会话过期及完成响应丢失测试与本地 HTTP 替身测试分别记录。

## 8. 与 OpenDAL 的关系（三重）

1. **参考设计**：capability、后端组织、服务矩阵、结构化错误；同时参考
   object_store 的低层 multipart 契约。按本库的双向传输需求组织模块与 crate。
2. **桥接**：额外后端的实际需求出现后，提供独立、可选的 service-opendal，
   与其他 service 使用相同公开契约，普通访问与交付能力分别声明。
3. **上游协作**：原型成立后可以讨论版本化持久会话与进度对账。上游是否
   接受尚未确认，本库设计不依赖该结果。

明确**不做**：OpenDAL API 兼容层（覆盖其用法，不克隆其签名——追别人的 API
尾巴永远追不完）；多语言绑定；presign；Layer 中间件体系（v2 再议）。

### 8.1 桥接的收益按传输方向判断

| 用途 | 桥接收益与边界 |
|---|---|
| 查询 / 列表 / 删除 | 复用协议操作；按 capability 映射可选接口 |
| 云端 → 本地 | 范围读取与稳定版本校验可复用；本地已持久化区间由我们的引擎记录，补读缺失区间实现下载恢复 |
| 本地 → 云端 | 支持有界流式写入时可复用；恢复原远端会话、完成对账仍须逐后端判断 |

下载恢复状态可以由本地引擎拥有，上传恢复还依赖远端会话。源端支持范围读取
及可靠版本校验时，桥接 service 可组合出持久下载恢复；只有 stat、长度或
不稳定 ETag 时，不能宣称同等保证。标准 Writer 的 write / close / abort 接口
不自动提供跨进程上传会话恢复。

### 8.2 桥接的准入与维护边界

- 使用者可以传入已配置的 Operator，负责提供其端点、凭证与有效的更新机制。
  适配器不要求 core 识别 OpenDAL 的每个 builder，也不假设所有后端都能通过
  同一个凭证钩子刷新；恢复记录需要额外绑定稳定的 service / 实例命名空间。
- 下载恢复、上传恢复、完成对账、发布保证分别声明并验收。消费者要求持久
  续传而后端不满足时明确拒绝，不能自动改为整文件重传。
- 范围预读、上传分片与 OpenDAL 内部并发共同计入资源预算。明确调度与重试
  的拥有者，限制内部并发、缓存与重试次数，避免两层调度放大在途请求。
- 桥接节省协议实现工作；凭证更新、错误映射、取消、源版本与真实服务端
  兼容性仍需维护。优先在首个额外后端上验收下载，再按需求扩大上传能力。

OpenDAL 的原生 / 完整 capability 可以作为映射输入，适配器还需表达本库
组合后的交付保证。验收支持范围和动态服务端约束要显式声明，不能把原生
capability 原样复制为所有交付能力。

## 9. 风险明账

1. **API 面规模**：双向交付涉及云源、本地源、本地暂存与云会话。缓解：访问面
   做窄（range_read / list / stat / delete），首期只覆盖云与本地之间的传输，
   不扩展为同步工具或完整文件系统。
2. **真机矩阵是持续成本**：WebDAV 服务器兼容矩阵 + OSS 签名版本跟进 + issue
   响应。这是第二主线，SwarmDrop 主线（传输）的排期不被它阻塞——里程碑已按
   此原则编排。
3. **OpenDAL 版本漂移**：桥接层钉版本 + 升级核对。
4. **过早抽象**：API 先通过 Drive + WebDAV + 本地 Sink 的双向契约验收再稳定化，
   OSS 的分片对账进一步检验恢复模型；驱动不能统一伪装成随机写文件。
5. **本地恢复的持久性**：写入、同步、checkpoint、发布与回执有多个崩溃窗口。
   需要按边界验证恢复行为，文件长度和 rename 成功不能代替完整的持久性保证。
6. **性能与资源预算**：范围预读、分片并发、重排缓存与磁盘任务必须有上界。
   分开测量云读取 / 写入、本地 IO 与 P2P 吞吐，记录网络、区域、限流和基线；
   并发配置不代表固定带宽承诺。
7. **扩展 API 维护成本**：公开 trait、状态信封与 service 版本有兼容性责任。
   以独立消费者示例和按能力运行的契约验收约束演进，不让官方后端依赖私有
   API，避免第三方只能 fork core 才能适配。

## 10. 未决问题

- ~~**命名**~~ **已定稿（2026-10-01）**：`waybill`（运单）——回执、在途追踪、
  交付凭证三语义合一，双向中性，CLI 工学友好（`wb put / get`）。核查：
  crates.io 未占用（404）；`portage` 被 Gentoo 包管理器占用出局；
  `wildgoose` / `snowgoose` 空闲但有 *wild goose chase* 习语陷阱；`godwit`、
  `courier`、`lading`、`yam` 均已被占用。吉祥物 **Bill**（雁哥）：腿绑运单筒
  的邮差鸿雁——双向迁徙、湿地补给、年年归巢、雁阵领飞分别对应双向传输、
  checkpoint、回执幂等与分片并发；Bill 同时双关雁喙（beak）与 way**bill**。
  M0 契约稳定后尽早 `cargo publish` 占名。
- **建仓与首期范围**：~~尚未创建仓库~~ **已建仓**（本仓库）。独立 Cargo
  workspace，core 与各 service 独立成包；以 service-fs + service-gdrive
  验证上传恢复及外部接入，再按 GDrive 下载 → WebDAV → OSS 接入；OpenDAL 桥接按实际需求触发。
- **checkpoint 状态格式**：不透明 blob 的版本化策略（库升级后旧 state 能否
  restore）——当前公共信封为 v2，驱动版本独立；未知版本拒绝恢复并保留记录。
- **校验与冲突策略**：来源缺少可信预期哈希时如何向消费者表达保证；同名目标
  的拒绝 / 覆盖 / 重命名规则；跨盘与外部文档提供方的发布回执如何对账。
- **平台范围**：首期原生实现为 Linux / macOS；SAF / Web 适配时机与能力边界待定。
- **公开扩展签名**：首期采用开放 trait 和 Send boxed future；工厂 / registry 后续设计。
  项目未发布，允许直接调整公开 API；发布后再建立版本与迁移承诺。
- **SwarmDrop 回切时机**：M1 后可评估局部接入，完整接入另立 OpenSpec 变更。

## 11. 首期落实契约与源码证据（2026-10-01）

本节描述 M1 与 M2 的实现约束；M1 已通过真机验收，M2 的真实 Drive 下载与恢复也已验收。
当前公共入口为 `TransferEngine`；§5.1 与 service 开发指南使用同一套实际签名。

- 核心定义开放 Service、Source、UploadSink、CheckpointStore、资源预算与上传、下载
  状态机；由同一个 `TransferEngine` 持有存储与预算。公共异步返回 Send boxed future，不绑定 Tokio、HTTP、本地路径或应用身份；
  引擎内并发调度仅引入执行器无关的 futures-util，运行时仍归宿主。
- service-fs 提供稳定文件源、精确范围读取、BLAKE3 身份、文件 checkpoint，以及下载
  本地目标（独占 `.part` 随机写、MD5 / BLAKE3 校验、同盘无覆盖发布）。宿主必须在上传期间
  冻结源文件。
- service-gdrive 使用短期凭证端口；OAuth 和刷新持久化由宿主负责。实例绑定账户、
  OAuth 应用与根目录。使用通用操作 ID，默认拒绝同名目标；显式选择可加操作后缀另存。
- checkpoint 公共格式版本与驱动私有格式版本分别验证，绑定源、目标与实例。先保存
  对象 ID 再初始化上传；远端完成后先保存回执，消费者确认记账后才清理恢复记录。
- 会话过期先查完成对象；未完成默认返回 SessionExpired 并保留记录。显式允许
  重建时每次运行最多两次，进度表达重新开始。暂停停止后续块，先对账在途结果。
- 默认上传块 8 MiB，共享预算最多两条上传、16 MiB 数据缓冲；校验块 256 KiB，
  单个元数据响应及 checkpoint 上限 1 MiB，元数据列表最多 1000 项。无目录缓存。
  HTTP 超时 60 秒；限流 / 5xx 最多三次重试、单次等待最多 30 秒。上传 PUT
  结果未知后先查偏移，不盲目重发；初始化 POST 使用已持久化 ID 收敛重试。
- 文件 checkpoint 以 0600 临时文件、同步、替换、目录同步落盘；操作锁覆盖整个
  恢复和确认过程。源、凭证、私有状态不写 Debug / 日志。hash 属性是操作对账证据，
  不证明远端内容完整性，也不承诺跨进程并发同名目标的统一原子发布。

### 公开 API 架构复核（2026-10-02）

- `TransferEngine::new(store)` 接受具体存储或 `Arc<dyn CheckpointStore>`；引擎克隆
  共享存储与预算，不克隆持久数据。`with_budget` 可在多个引擎间显式共享预算。
- 上传源声明 `max_read_size`，上传目标声明 `UploadChunkLimits`（上限与非末块
  对齐）；下载源和目标分别声明读写上限。引擎取预算与端口限制的有效交集，
  对齐不足或零上限在准备对象 / 暂存前拒绝。Drive 的 8 MiB / 256 KiB 约束
  属于 service 声明，核心不包含 Drive 名称或协议分支。
- 上传与下载意图的 `target` 都是目标 service 解释的不透明引用。核心只检查操作 ID、
  引用长度与控制字符；相对云端路径规则归 service-gdrive，原生绝对路径与段约束归 service-fs。CLI 在授权前规范化
  本地路径，Rust 宿主也须提供稳定绝对路径，避免工作目录变化改变恢复目标。
- 实例、稳定源与云端源的身份在开始 / 恢复时统一验证；账户、应用与根目录
  继续隔离恢复命名空间。根目录约束路径解析，不能替代服务端权限检查。
- 下载账本校验由 `DownloadFlow::persisted_bytes` 共用；恢复引擎与 CLI status
  都拒绝越界、反向、重叠或过多区间，不能对损坏数据直接做减法和求和。
  已完成下载在本地对账后也再次复核远端版本，版本变化保留记录并返回 SourceChanged。
- 生命周期仍要求先保存决定再执行外部写入、同步数据后记账、保存完成回执后
  报告成功。对账与失败处理保留各方向独立状态机，不以通用 writer 隐藏差异。

### CLI 宿主边界

`wb` 是公开上传、下载契约的消费者。`cli` 定义参数，`cmd` 做本地预检与组装；
`transfer::upload` 顺序执行上传队列，`transfer::download` 顺序编排多文件取回，
`transfer::events` 定义两种方向共用的生命周期事件。`cmd::session` 统一拥有
输出选择、停止信号、传输任务收割及退出判定；会话结束时取消信号监听。
JSON、行式输出和全屏面板只消费事件。
`status` 最多扫描 1000 条记录，每条至多 1 MiB。事件通道最多 64 条：生命周期事件等待交付，拥堵时丢弃进度样本；上传进度仍以
引擎持久确认量为准。首次停止等待在途请求对账，二次停止中止任务并保留最近记录。

`oauth` 负责桌面 PKCE、回环回调和 Google 身份查询；`credentials` 原子保存应用与
token 并拥有刷新。账户别名只用于本机凭证目录，service 命名空间使用实际 Google
`permissionId`，因此同一别名改绑账户不能复用旧恢复状态。CLI 请求 `drive.file` + `drive.readonly`，前者上传本应用文件，后者为后续云盘列表与
下载提供只读权限，`wb get` / `wb list` 已随 M2 落地。`gdrive_host` 将其与 OAuth
应用、根目录组装成实例，具体 Drive 协议仍归 service。仅 `drive.file` 的真机授权下读取
`files/root` 返回 404；默认根通过 [v2 about.rootFolderId](https://developers.google.com/workspace/drive/api/reference/rest/v2/about)
解析真实 ID（2026-10-01 已验证），无需扩大 scope；显式根 ID 保留可访问性与目录校验。

CLI 保留带回执的完成 checkpoint 作为本机幂等记录，不在输出事件后立即 `confirm`。
重跑同一命令先复核源与实例，再返回原回执；`status` 显示 `in_flight` / `completed`。
删除本机完成记录后不保证重复投递自动跳过：当前 service 不跨操作搜索远端回执。
上传期间仍要求消费者冻结源文件；文件版本变化按公开契约明确拒绝。

### M2 下载与本地发布（2026-10-01 实现并完成真实 Drive 验收）

核心 `download` 模块与上传镜像：`DownloadSource` 提供廉价元数据复核
（`RemoteIdentity`：源 `ServiceIdentity`、对象引用、不透明 revision、长度、
可选服务端摘要）与有界
范围读取；`DownloadTarget` 按生命周期划分 `prepare / probe / initialize /
write_chunk / verify / publish`。`write_chunk` 按精确偏移寻址且不要求调用
顺序——乱序容忍是目标契约属性，引擎调度只是使用者。能力位新增
`range_download / random_write / durable_publish`，开始前显式拒绝，不降级。
恢复记录同时绑定源、目标 service 身份；源实例改变返回 IdentityMismatch，
同一实例内对象版本改变返回 SourceChanged，均保留原记录。

引擎以 `FuturesUnordered` 做有界并行补洞：各流领取最小缺失区间，并发与
缓冲受 `chunk × concurrency` 预算约束；`write_chunk` 由引擎串行调用，区间
在写入并同步落盘后才记入公共信封账本（合并有序，上限 4096 条）。加载
记录时先验证账本非空区间、顺序、无重叠及源长度边界，再计算覆盖量；非法
账本返回 Checkpoint，不能以整数溢出或归一化掩盖损坏。单流失败留洞返回，
已记账区间保留；全部收齐后先校验再发布。完成回执持久化成功后才发送
`progress.complete = true`，发布成功但保存回执失败仍不报告完成。

service-fs 私有目标状态版本为 v2。每次初始化用 `tempfile` 在目标目录创建
独占随机 `.wb-*.part` 文件，把路径、设备号与 inode 写入私有状态；恢复和
写入复核同一文件身份，不再截断目标旁共享的固定 `.part`。精确偏移写入与
同步完成后才能更新账本；完成度始终由账本决定，预分配长度不构成完成证据。

发布使用 `tempfile::TempPath::persist_noclobber`：Linux / macOS 优先使用无覆盖 rename，必要时以硬链接原子
占用目标名称，目标已存在或被另一操作抢先创建均返回冲突，不覆盖已有文件；
失败保留原暂存文件与完成账本。恢复已校验暂存时仍重新校验内容；暂存缺失而
目标存在时，probe 也重新校验目标来复原回执，不能信任旧 verified 标志或
同长度替换文件。发布窗口对账仍同步目标父目录；硬链接回退留下暂存原名时，
通过同 inode 核验发布事实。服务端提供 MD5 / BLAKE3 时验证全文件摘要；没有摘要仅检查
长度，并按 `Verification` 声明实际证据。发布失败重跑不重新读取云端数据，
但重新核验本地内容后再发布。

checkpoint 信封采用 v2：`flow` 标签区分上传 / 下载记录，两种方向共用
同一操作租约与存储目录。未发布阶段不维护 v1 平铺格式的兼容包装；旧记录
拒绝解码并保留，未知信封版本拒绝恢复。驱动私有状态仍独立验证版本。
回执 `verified` 只声明实际证据：GDrive 上传核对远端长度与身份属性，声明
`Length`；客户端写入的 BLAKE3 属性不是 Google 内容校验结果。下载按真实
MD5 / BLAKE3 或长度校验声明，源内容摘要继续作为上传恢复身份保存。

service-gdrive 下载侧：`files/{id}?alt=media` 精确 Range 读取（206 必须精确
回显区间；200 仅接受全文件请求，超长即协议错误；416 映射 SourceChanged）。
媒体重定向手动跟随 Google HTTPS 白名单（googleapis / drive.google /
*.googleusercontent，最多五跳），跨源不携带凭证。revision 复合
`version:md5:size`；Google 原生文档（无二进制内容）明确拒绝。`resolve`
按配置根解析路径（中间段必须目录、同名多义拒绝），`list` 列目录直接子项。

CLI：`wb get <SRC-URI> <DEST>` 先校验 URI 与本地目标，再加载凭证。
本地路径使用 `soft-canonicalize` 得到稳定绝对路径，支持 `.`、`..`、现有路径
的符号链接与尚不存在的父目录；无法表示为 UTF-8 的路径明确拒绝，不能有损
改写。`DownloadIntent`（含显式 operation）在授权前验证。已存在目录可带
结尾 `/`，解析云端文件后展开单个安全文件名；文件名中的路径分隔符、控制
字符和 `.` / `..` 不得改变目标位置。不以 `/` 结尾的不存在路径作为目标文件，
不存在目录且以 `/` 结尾时明确拒绝。

`wb list <DIR-URI>` 浏览目录；`status` 按信封方向分列展示，下载行汇总区间
账本。默认操作 ID 使用流式 BLAKE3、方向标签与各字段长度前缀派生；下载
绑定源及目标 service 类型与实例、云对象引用、源版本与绝对本地目标路径，
上传绑定目标 service 类型与实例、云端目标与内容摘要。不同账户、根目录或
本地目标保持隔离，远端更新即新运单。本地跨盘发布（EXDEV）明确不支持，
不自动降级为复制。当前重构后的真实 Drive 验收已通过，具体环境与结果
以 [VALIDATION.zh-CN.md](VALIDATION.zh-CN.md) 的独立记录为准。

### CLI 盘配置与交互补全（2026-10-01）

CLI 把盘配置与凭证分离：`drives` 保存有界 JSON 的名称 → 账户 / 默认根目录
映射及默认盘，使用 tempfile 原子替换并同步目录；配置不包含 token 或客户端
secret。`drive add / use / root / list / remove` 管理配置，root 未给 ID 时使用
目录选择器；删除配置不删除凭证或 checkpoint。显式 URI 与 `--drive` 互斥，
短路径相对配置根解析，`--root` 只覆盖本次调用。第一个配置盘自动成为默认盘。

`cmd::entry` 负责参数补全和盘选择；解析后的输入交给 put / get / list。
完整参数保持非交互，缺参仅在 stdin / stdout / stderr 都为 TTY 且未设置
`--json` / `--no-tui` 时补全；没有默认盘时从配置盘或已登录账户选择。
`put` 的本地源允许多个，目标用 `--to`，仍接受末尾完整 URI；`get` 支持
`--into` 单独提供本地目标并进入云端多选。云端目录路径作为 get 源时打开
选择器，不解释为递归下载。无参数 `list` 浏览根目录，提供路径时输出列表。

`cmd::browse` 拥有网络 / 本地目录读取与导航栈；`ui::picker` 只拥有终端输入
和渲染，使用 ratatui / crossterm。终端由命令会话统一拥有，RAII 恢复。Enter 导航，Space 多选，
c 确认；不同目录的文件选择以真实对象 ID / 本地绝对路径保留。云端范围继续
由 service 控制，浏览导航最多 32 层；本地单目录最多 10000 项，选中集合最多
4096 项，超限明确拒绝。Google 原生文档不提供虚假的二进制下载选项。

交互上传选中的目录 ID 是宿主显式选择的本次 service 根，上传不依赖目录显示
名重新解析，能使用已有且允许上传的目录，仍遵循权限与冲突契约。下载队列
通过对象 ID 构造 source，逐文件占用同一有界预算并发送独立索引事件；全部
本地目标在开始前验证，同名映射拒绝，不隐式覆盖。恢复身份和默认操作 ID
仍绑定实际 service 实例、对象版本与绝对目标；文件选择器不拥有恢复状态。


### CLI 全屏会话与运单浏览（2026-10-02）

`cmd::dispatch` 为每次命令创建惰性的 `ui::Session`，只在首帧进入 raw mode
与 alternate screen。盘选择、本地和云端选择器、传输面板显式借用同一个会话；
目录导航、等待网络和步骤切换保持全屏，命令结束或错误时恢复。输入使用
crossterm `EventStream`，与传输事件通过 `tokio::select!` 驱动，没有阻塞输入线程
或进程级 UI 资源槽。直接使用 ratatui / crossterm，移除会自行初始化和恢复
终端的 ratatui-kit 运行时，避免多套终端所有权。

`cmd::status` 扫描公共 checkpoint 信封并提供安全展示行；`ui::status` 负责
只读列表、详情、读取问题和手动刷新。刷新保留选中操作，不取写租约、不请求
云端或擅自恢复任务。TUI 默认仅在三个标准流均为终端时启用；`--no-tui`
与非终端保持行式输出，`--json` 保持原有 JSON 数组。存在不可读取记录仍返回
非零退出码，离开全屏后输出诊断，不能让不完整结果伪装成成功。


抽取基线：SwarmDrop `0a81f133214958f7b01ae9a0e4624dc87e16014b`，
`crates/storage-cloud/src/{gdrive,staging,persistence,publish}` 与
`crates/host-fs/src/local_fs/{part_file,sink_ops}`（M2 的 `.part` 机制来源）。
MIT 来源声明随衍生代码保留。
只复用协议和恢复机制，不复制设备目录、接收记录模型、CloudAccountManager 或 UI 类型。
SwarmDrop 的 17 MiB 真机探针确认正常分块上传、属性查询及重复接收复用；重启、过期、
完成响应丢失尚未验收。目录源码的 let chains 当时为兼容 Rust 1.85 改写；
2026-10-01 起 rust-version 统一为 1.88，新代码可直接使用 let chains。

本轮源码复核：本机缓存 OpenDAL core 0.59.2 的 MultipartWriter 上传 ID 为私有状态；
[公开 GDrive backend](https://opendal.apache.org/docs/rust/src/opendal_service_gdrive/backend.rs.html)
使用 OneShotWriter（网站源码未固定发布版本，不外推所有版本）。
[object_store 公开 multipart 源码](https://github.com/apache/arrow-rs-object-store/blob/main/src/multipart.rs)
提供低层上传 ID，但不直接提供本项目 checkpoint 编排；首期均不作为依赖。
[Google resumable 协议](https://developers.google.com/workspace/drive/api/guides/manage-uploads)
定义 308 偏移查询、404 过期与非末块 256 KiB 对齐。
未在本轮重新核验的 §3 其他库版本、CLI 市场数据及占名状态保留原日期，仅作历史调研。

waybill 独立库的本轮分层验收见 [VALIDATION.zh-CN.md](VALIDATION.zh-CN.md)：
本地 HTTP 故障替身与真实 Drive 验收分别记录，不继承上游探针的故障恢复结论。
