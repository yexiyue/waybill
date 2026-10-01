# waybill 开发指南

本文件是 Coding Agent 的仓库入口。架构决策的事实源是
[docs/DESIGN.zh-CN.md](docs/DESIGN.zh-CN.md)；修改架构时同步该文档，
不要另建一份重复的架构说明。

## 开始之前

1. 阅读 [README.zh-CN.md](README.zh-CN.md)，确认项目定位与当前阶段。
2. 阅读设计文档中与任务有关的章节；公共契约重点看 §5，里程碑看 §7。
3. 检查当前工作区差异，保留与任务无关的改动。
4. 涉及品牌时阅读 [docs/BRAND.zh-CN.md](docs/BRAND.zh-CN.md)。

## 当前仓库

项目处于 **M1 GDrive 上传恢复原型阶段，未发布**。核心已提供公开上传契约，
service-fs 提供稳定源和 checkpoint，service-gdrive 提供上传与对账。下载与本地发布
后续实现；Operator / registry 等长期设计草图不是既有 API。

| 路径 | 用途 |
|---|---|
| `Cargo.toml` | 工作区、共享包信息与构建配置 |
| `crates/waybill/` | 公开契约与上传状态机 |
| `crates/waybill-service-fs/` | 稳定本地源与持久 checkpoint |
| `crates/waybill-service-gdrive/` | 原生 GDrive 上传协议与对账 |
| `examples/consumer/` | 独立公开 API 接入示例 |
| `docs/DESIGN.zh-CN.md` | 架构、恢复语义、扩展契约与路线图 |
| `docs/market-cli.zh-CN.md` | CLI 消费者的市场调研，尚无 CLI 实现 |
| `README.md` / `README.zh-CN.md` | 英文与中文项目入口 |
| `assets/brand/` / `docs/BRAND.zh-CN.md` | Bill 吉祥物与品牌资产 |
| `.github/workflows/ci.yml` | Linux 与 macOS 的 Rust CI |

Rust edition 为 **2024**，声明的 MSRV 为 **1.85**，许可证为
**MIT OR Apache-2.0**。新增依赖、语言特性和工具链配置须考虑这一版本约束。

## 架构约束

- 核心负责公开契约与交付生命周期；协议操作和本地 IO 放在各 service 中。
  核心不依赖具体 service、HTTP 客户端、SwarmDrop、Tauri 或应用身份模型。
- 本地、GDrive、WebDAV、OSS 计划作为独立 `waybill-service-*` crate 接入。
  开发到对应里程碑再创建，不预留空 crate 或假实现。
- 自维护与外部 service 使用同一套公开契约。扩展 trait 不封闭，
  新增 service 无须修改核心 provider 枚举、访问私有 helper 或加入特殊分支。
- 最小只读 service 应能参与。上传、恢复与其他操作按实际能力声明；
  未支持的操作返回明确错误，不能用默认成功掩盖缺失实现。
- 凭证刷新与交互授权由宿主拥有。service 在请求边界获取有效凭证；
  不把 OAuth、WebDAV 用户密码和 OSS AK / STS 强行统一成同一种凭证。
- OpenDAL 适配按具体额外后端需求启动，优先验证下载，遵循相同公开边界。
  桥接访问成功不能代替恢复与交付保证的验收。
- 公共端口保持平台中立；原生 IO、执行器及其保证按 target / feature 隔离。
  浏览器支持仍需单独设计和验证，不能从原生实现推断。

完整语义与设计取舍见设计文档 §5、§8；遇到冲突先更新设计，再落实代码。

## Rust 模块与代码风格

- 按职责组织模块，类型与行为放在所属领域模块中。不要集中堆进
  `types.rs`、`utils.rs`，也不要只因结构体数量增长就拆文件。
- 目录、模块名和 API 标识符用英文；注释与内部设计文档用简体中文。
  英文 README 与中文 README 的项目状态、范围及路线图保持一致。
- 注释说明约束、取舍和原因；涉及后端差异时记录证据，避免复述代码。
- 优先复用 HTTP、签名与协议库，保持 service 适配范围明确。
  引入依赖前检查它是否支持所需恢复操作，而非只看支持的后端名称。
- 错误在所属模块定义，在 service 边界映射为稳定的核心错误类别，
  保留可诊断的来源。生产库路径避免 `unwrap()`、`expect()` 和 panic。
- 保持核心的 `#![forbid(unsafe_code)]`。首期公共异步端口使用 Send boxed future，
  core 不绑定执行器；service-fs 明确只支持 Linux / macOS，浏览器仍待设计。
- 图表使用 Mermaid。

## 恢复与资源约束

- checkpoint 绑定操作、service 实例、源与目标身份及格式版本；
  驱动私有状态不透明，同一 service 的不同账户、端点或根目录须隔离。
- 已发送、已确认、已持久化和已完成分别表达。数据写入并同步后，
  才能持久记录对应本地区间完成；不能凭 `.part` 长度判断文件已收齐。
- 恢复前对账源版本和远端会话。版本不匹配、会话过期或结果未知时，
  保留恢复记录并返回明确状态。
- 本地发布失败保留暂存数据和完成状态，允许只重试发布。
  重命名、跨文件系统复制和远端完成操作的保证分别声明。
- 内存、预读、并发、重排缓存及重试均有明确上界；多层调度共用资源预算。
- ETag 不统一视作内容哈希，不承诺跨后端 exactly-once、统一原子发布
  或固定吞吐量。降级为整文件重传需要消费者显式允许。
- 不将凭证、会话 URI 或完整 checkpoint 私有状态写入日志、示例或仓库。

## 开发与验证

在仓库根目录执行。按任务范围和用户要求选择验证命令：

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

需要格式化时使用 `cargo fmt --all`。现有 CI 在 Linux 与 macOS 上执行
fmt、Clippy 和测试；它使用 stable 工具链并增加 Rust 1.85 检查，尚无浏览器验收任务。
不要把现有 CI 通过描述为所有 target、MSRV 或真实后端均已验证。

真实后端验证需记录环境与能力，区分本地契约验证和服务端验收。
恢复逻辑尤其关注进程中断、完成响应丢失、源版本变化和发布失败窗口。

## 文档与交付

- 项目介绍区分计划、已实现与已验证。能力或阶段变化同步两份 README。
- 新架构决策落入设计文档；调研结论附来源和日期，推断与事实分别说明。
- 品牌沿用 Bill / 雁哥及腿绑运单筒设定；生成记录保存在
  `assets/brand/PROMPTS.md`，变体用版本化文件名。
- Commit 使用 Conventional Commits，message 用英文。
- 提交前检查暂存范围；提交、推送与发布按用户授权执行。
- 收尾说明修改内容、实际执行的验证和未完成事项，避免把设计骨架报告为实现。
