# waybill · 运单

<p align="center">
  <img src="assets/brand/readme-banner-v1.png" alt="waybill 品牌头图：腿绑运单筒的邮差鸿雁 Bill" width="960">
</p>

<p align="center">
  <strong>可恢复交付，往返皆可。</strong><br>
  用 Rust 构建云端与本地之间的双向文件传输。
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="docs/DESIGN.zh-CN.md">设计文档</a> ·
  <a href="docs/BRAND.zh-CN.md">品牌与雁哥</a> ·
  <a href="https://github.com/yexiyue/waybill/actions/workflows/ci.yml">CI</a>
</p>

> **早期开发 · M1 上传恢复原型，未发布。** 已实现开放上传契约、本地稳定源与文件
> checkpoint、GDrive 分块上传和恢复对账，以及独立消费者示例。下载与本地发布尚未实现。
> 本地故障测试与真实 Google 验收分别记录，见 [验收记录](docs/VALIDATION.zh-CN.md)。

## 让交付可以接着完成

大文件传输可能跨越一次进程生命周期。网络中断、应用重启，或者文件已经收齐，
目标目录却暂时无法写入。waybill 的设计目标是保存足够的状态，让后续尝试能够
确认进度并继续完成。

**waybill** 的意思是「运单」：随货物同行，记录身份、在途状态与交付凭证。

| 设计目标 | 含义 |
|---|---|
| **持久恢复** | 版本化 checkpoint 绑定源、目标和进度；重启后先确认本地持久数据或远端会话状态。 |
| **回执对账** | 用通用操作 ID 与完成证据收敛重试，具体保证由 service 能力决定。 |
| **资源上界** | 分片大小、并发、预读与缓存共用明确预算。 |
| **准确声明能力** | 上传恢复、下载恢复、版本校验和发布保证分别表达。 |

下载计划采用「补读缺失区间 → 写入本地 .part → 持久记录进度 → 校验 → 发布」
的生命周期。发布失败保留暂存数据，下一次只需重试发布。

上传则按 service 选择连续偏移、分片会话或整文件重传。调用方要求持久续传时，
可以在开始前拒绝不满足条件的 service。

## 一个引擎，各自独立的 service

```mermaid
flowchart LR
    LS[本地文件源] --> E[传输引擎]
    CS[云对象源] --> E
    E --> FS[本地暂存与发布]
    E --> US[云上传会话]
    E --> CP[Checkpoint 存储]
```

核心已提供 Service、Source、UploadSink、能力、checkpoint 与上传调度。
GDrive 的短期 token 端口属于该 service；授权和刷新由宿主负责。
每个 service 通过公开契约实现一个后端。

**我们维护的 service 与其他开发者编写的 service 使用同一套扩展边界。**
开发者可以独立发布 crate，依赖 waybill，由消费者直接注入，不需要修改核心
provider 枚举，也不需要等待主仓库收录。

| service / 状态 | 职责 |
|---|---|
| waybill-service-fs · 原型 | 本地稳定源、范围读取、BLAKE3 核验和文件 checkpoint；下载发布后续实现 |
| waybill-service-gdrive · 原型 | Google Drive 上传、偏移续传、会话与完成对象对账 |
| waybill-service-webdav · 计划 | 流式上传、范围下载与服务端能力差异 |
| waybill-service-oss · 计划 | 阿里云 OSS 访问、分片上传与已完成分片对账 |
| 外部 service crate | 使用相同公开契约接入其他后端 |

只读 service 可以直接参与；上传与持久恢复按需实现。现有
[独立消费者示例](examples/consumer/) 展示公开契约接入，
[service 开发指南](docs/SERVICE.zh-CN.md) 说明实际接口与验收要求。
API 仍是 0.x 原型，设计中的 Operator / registry 草图不是当前接口。

首期使用 `UploadEngine`，同一操作重试先核验源、实例和远端状态。会话过期默认返回
`SessionExpired`，保留 checkpoint；消费者显式允许后最多重建两次。
完成回执先落盘，业务记账之后才调用 `confirm` 清理记录。源文件由宿主冻结并保留。

## 与现有存储库协作

设计参考 OpenDAL、object_store 的访问与分片能力，研究资料见
[设计文档](docs/DESIGN.zh-CN.md)。waybill 计划复用 HTTP、签名和协议实现，
负责交付生命周期、资源调度与持久恢复状态。

OpenDAL 适配器由实际额外后端需求触发，优先验证下载：范围读取和源版本校验结合
本地 checkpoint 可以实现下载恢复；上传会话恢复仍须按后端单独判断。

每个 service 声明实际保证。WebDAV MOVE、确定性对象键或 ETag 本身，不能统一
证明发布原子性、内容完整性或跨后端 exactly-once。

首期实现本地到 GDrive 的上传，长期聚焦本地与云端之间的文件交付，目录同步、冲突合并与多设备同步不在首期范围。

## 路线图

| 阶段 | 计划结果 |
|---|---|
| **M0** | 开放契约与版本化 checkpoint |
| **M1 · 当前** | 本地稳定源 + GDrive 上传恢复，独立消费者接入 |
| **CLI · 开发中** | `wb` 运单命令行（login / put / status）：GDrive 上传闭环，全屏面板 + `--json` 事件流；下载与 MCP 待 M2 后 |
| **M2** | GDrive 下载与本地暂存、恢复和发布 |
| **M3** | WebDAV 与真实服务端兼容矩阵 |
| **M4** | OSS 分片恢复与完成对账 |
| **按需扩展** | OpenDAL 适配器，从具体额外后端的下载路径开始 |

首个计划消费者是 [SwarmDrop](https://github.com/swarm-apps/SwarmDrop)。其已有本地
文件与 Drive 交付实现提供初始素材；waybill 的公共契约独立于应用 UI、设备身份
和 P2P 协议。

## 认识 Bill · 雁哥

<p align="center">
  <img src="assets/brand/bill-mascot-v1.png" alt="Bill：米白与灰色羽毛、橙色喙和脚、腿绑青绿色运单筒的鸿雁" width="240">
</p>

Bill 是一只带着运单筒的邮差鸿雁，中文叫 **雁哥**。双向迁徙对应往返交付，
腿上的运单筒让交付凭证始终伴随旅程。

吉祥物、头像、README 头图、社交封面与生成提示词见
[品牌指南](docs/BRAND.zh-CN.md)。

## 参与开发

从 [设计文档](docs/DESIGN.zh-CN.md) 开始。当前公开 API 为上传原型；
本地故障测试及真实 Drive 上传、重启恢复已通过，范围与缺口见
[验收记录](docs/VALIDATION.zh-CN.md)。长期 API 草图还会调整。

工作区使用 **Rust 2024**，声明的最低 Rust 版本为 **1.88**。检查当前实现：

```sh
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
```

## 许可证

采用 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双许可证，由使用者选择。
