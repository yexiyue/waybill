# waybill · 运单

<p align="center">
  <img src="assets/brand/readme-banner-v1.png" alt="waybill：腿绑运单筒的邮差鸿雁 Bill" width="960">
</p>

<p align="center">
  <strong>可恢复交付，往返皆可。</strong><br>
  Rust 可恢复传输库：为上传与下载提供持久进度、完成回执与开放的后端契约。
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="docs/DESIGN.zh-CN.md">设计</a> ·
  <a href="docs/SERVICE.zh-CN.md">Service 开发指南</a> ·
  <a href="docs/STATUS.zh-CN.md">项目进度</a> ·
  <a href="https://github.com/yexiyue/waybill/actions/workflows/ci.yml">CI</a>
</p>

waybill 把一次文件传输建模为一张「运单」：开工前先落盘一条恢复记录，
途中只把**实际确认**的进度记入 checkpoint，完成后返回可记账的回执。
中断后用同一操作 ID 重跑，引擎先对账源版本、远端会话或本地暂存，
再决定续传、重试发布或直接复用完成结果。

[`wb`](#wb第一个集成应用) 命令行是基于这套公开 API 的第一个集成应用；
你也可以在自己的程序里以同样的方式组装传输能力。项目名称来自随货同行的
「运单」；[Bill · 雁哥](docs/BRAND.zh-CN.md) 是我们的邮差鸿雁。

## 设计要点

- **先记账后推进**：数据写入并同步后才记录对应偏移或区间；百分比到 100 不等于完成，完成以持久回执为准。
- **不猜远端结果**：请求结果未知时返回 `ResultUnknown` 并保留记录，恢复前必须对账；会话过期等待显式决策，不盲目重建。
- **证据分级**：回执携带 `Unverified / Length / Digest` 三级验证证据；客户端提交的哈希不冒充服务端内容校验。
- **处处有界**：块大小、并发、checkpoint 体积、目录响应均有上界，多个引擎可共享同一资源预算。
- **开放契约**：核心不认识任何后端；service 以开放命名空间接入并按实例声明能力，未实现的端口明确拒绝而不是默认成功。

目录同步、冲突合并与多设备同步不在当前功能范围内。

## Crate 布局

| Crate | 职责 |
|---|---|
| [`waybill`](crates/waybill/) | 公开契约、上传 / 下载状态机、checkpoint、回执与资源预算 |
| [`waybill-service-fs`](crates/waybill-service-fs/) | 稳定本地源、文件 checkpoint、下载暂存与发布（Linux / macOS） |
| [`waybill-service-gdrive`](crates/waybill-service-gdrive/) | Google Drive 协议、上传会话对账、目录访问与范围读取下载 |
| [`waybill-service-webdav`](crates/waybill-service-webdav/) | WebDAV 目录与范围读取、整文件流式上传、内容对账与条件 MOVE |
| [`waybill-service-opendal`](crates/waybill-service-opendal/) | OpenDAL 对象存储浏览、条件下载、流式上传与完成对账 |
| [`waybill-cli`](crates/waybill-cli/) | `wb`：授权、盘配置、交互与传输编排（见下文） |

核心依赖只有 `serde / thiserror / futures-util`，不绑定 HTTP 客户端、
执行器或任何后端。宿主负责授权与凭证刷新；service 在请求边界获取有效凭证。

## 库接入

0.1.0 已发布到 crates.io，项目处于 0.x，公开 API 可能调整；
需要 **Rust 1.91+**（edition 2024）。当前 service 均为原生实现，宿主自带
Tokio 运行时：

```toml
[dependencies]
waybill = "0.1.0"
waybill-service-fs = "0.1.0"
waybill-service-gdrive = "0.1.0"
# waybill-service-webdav / waybill-service-opendal 按需引入
tokio = { version = "1", features = ["full"] }
```

`waybill-service-opendal` 默认启用 S3、OSS、COS、OBS、TOS、GCS、Azure Blob、
B2、Swift、Upyun 和 Vercel Blob；库消费者可用 `default-features = false`
加单个 feature 裁剪。

## 核心模型

三个角色各司其职：

- **宿主（你的应用）**：拥有 OAuth 与凭证刷新、业务账本、暂停信号与进度消费。
- **`waybill` 核心**：拥有传输状态机、checkpoint、回执校验与资源预算。
- **service**：拥有协议与 IO，按 `prepare / probe / initialize / write / verify / publish` 生命周期实现端口。

一次构造 `TransferEngine::new(store)` 即可重复使用，四个入口覆盖全部操作：

| 入口 | 用途 |
|---|---|
| `engine.upload(source, sink, options)` | 连续偏移上传（GDrive 等可续传后端） |
| `engine.upload_stream(source, sink, options)` | 有界整文件流式上传（WebDAV / 对象存储） |
| `engine.download(source, target, options)` | 下载到本地目标 |
| `engine.confirm(&receipt)` | 业务记账完成后清理恢复记录（幂等） |

完成协议是两阶段的：引擎先把回执**持久化进 checkpoint** 再返回成功，
宿主提交业务账本后以同一回执调用 `confirm` 清理记录。

```mermaid
flowchart LR
    A["engine.upload / download<br/>返回 Receipt"] --> B["① 宿主提交业务账本"]
    B --> C["② engine.confirm(&receipt)<br/>清理恢复记录"]
    C --> D["幂等：记录不存在也成功"]
```

### 上传到 Google Drive

```rust
use std::sync::Arc;
use waybill::{TransferEngine, UploadOptions, service::Service};
use waybill_service_fs::{FileCheckpointStore, FsService};
use waybill_service_gdrive::{Gdrive, GdriveConfig, credential::TokenProvider};

async fn deliver(
    tokens: Arc<dyn TokenProvider>,
    ledger: &mut Ledger,
) -> waybill::error::Result<()> {
    let local = FsService::new("my-host")?;
    // 打开并完整哈希核验源文件；上传期间宿主须保证源不被修改
    let source = local.source("/data/report.zip").await?;

    // 宿主拥有 OAuth；service 在每次请求边界获取有效凭证
    let drive = Gdrive::new(
        GdriveConfig {
            account: "user@example.com".into(),
            oauth_application: "my-app".into(),
            root: "1AbC_folder-id".into(),
        },
        tokens,
    )?;

    let engine = TransferEngine::new(FileCheckpointStore::new("/var/lib/myapp/waybill"));
    let receipt = engine
        .upload(
            source.as_ref(),
            drive.upload_sink()?.as_ref(),
            UploadOptions::new("report-20261002", "backup/report.zip"),
        )
        .await?;

    ledger.commit(&receipt)?;        // ① 先写业务账本
    engine.confirm(&receipt).await?; // ② 再清理恢复记录
    Ok(())
}
```

中断后用同一 operation 重跑 `engine.upload` 即恢复：引擎先对账远端会话与
完成对象，再从服务端确认的偏移继续。需要暂停、进度或冲突策略时改选项：

```rust
use waybill::transfer::{ConflictPolicy, StopToken};

let mut options = UploadOptions::new("report-20261002", "backup/report.zip");
options.progress = Some(&|p| {
    println!("persisted {}/{} (complete: {})", p.persisted, p.total, p.complete);
});
options.stop = stop_token.clone(); // 触发后保留记录并返回 Paused
options.intent.conflict = ConflictPolicy::OperationSuffix; // 同名目标改用后缀名
```

### 从云端下载

```rust
use std::sync::Arc;
use waybill::download::DownloadOptions;
use waybill::service::Service;

let backend: Arc<dyn Service> = Arc::new(drive); // 以 trait 对象持有，后端中立
let target = local.download_target()?; // LocalTarget：.part 随机写、校验与原子发布
let entries = backend.list(&root_folder_id).await?; // Vec<ObjectMetadata>
let entry = entries.iter().find(|o| o.name == "report.zip").unwrap();

let receipt = engine
    .download(
        backend.download_source(&entry.reference).await?.as_ref(),
        target.as_ref(),
        DownloadOptions::new("fetch-report-0001", "/data/downloads/report.zip"),
    )
    .await?;
```

- 下载目标由目标 service 解释；service-fs 要求**绝对路径**，同文件系统发布、不覆盖已有文件，不支持跨文件系统复制发布。
- 引擎按共享预算有界并行补洞；`.part` 长度不代表完成度，区间账本是唯一事实源，发布失败只重试发布。

### 整文件流式上传（WebDAV / 对象存储）

普通 WebDAV PUT 与对象存储 multipart 不提供偏移续传，走独立的整文件端口：

```rust
use waybill_service_webdav::{Webdav, WebdavConfig};

let dav = Webdav::new(
    WebdavConfig::new("https://dav.example.com/files/", "alice"),
    credentials, // Arc<dyn credential::CredentialProvider>
)?;
let mut options = UploadOptions::new("report-0007", "backup/report.zip");
options.policy.allow_restart = true; // 中断后显式允许整文件重传
let receipt = engine
    .upload_stream(source, dav.stream_upload_sink()?.as_ref(), options) // source: Arc<dyn Source>
    .await?;
```

已验证的暂存只重试发布，无须重传；默认不允许中断后静默重传整个文件。

### 对象存储（Apache OpenDAL）

宿主按 OpenDAL 常规方式构造 `Operator`（凭证刷新由 Operator 拥有），
再包成本 service，之后与其它后端共用同一套 `Service` 端口：

```rust
use waybill_service_opendal::{ObjectStorage, ObjectStorageConfig};

let storage = ObjectStorage::new(operator, ObjectStorageConfig::new("prod-namespace"))?;
let source = storage.download_source("backup/report.zip").await?;
let meta = storage.resolve("backup/").await?; // ObjectMetadata
```

上传默认关闭；确认服务端条件写入生效后在配置中设置 `conditional_writes: true`。
后端注册不代表交付能力齐备，配置示例与能力边界见
[对象存储接入](docs/object-storage.zh-CN.md)。

### 浏览与只读访问

`Service::resolve(path)` 与 `Service::list(reference)` 提供可选目录访问，
返回 `ObjectMetadata`（`reference` 是恢复身份，展示名不参与）。最小 service
可以只实现 `Source`：

```rust
let service: Arc<dyn Service> = my_readonly_service();
let source = service.source("demo").await?;
let identity = source.identity().await?; // 大小、版本与 BLAKE3
```

### 恢复、错误与资源

- **操作 ID**：1..=128 字节的 ASCII 字母数字与 `-_.:`；同一操作不得更换源或目标，checkpoint 绑定双方身份与格式版本。
- **恢复动作**：按 `ErrorKind`（non_exhaustive）匹配——`Paused` 保留记录、`SourceChanged` 拒绝恢复、`SessionExpired` 等待 `allow_restart`、`ResultUnknown` 先对账、`Authentication` 交回宿主刷新凭证。
- **诊断安全**：错误消息只含受控静态描述，细节在 source 链中，不携带凭证或会话 URI。
- **资源预算**：`ResourceBudget` 约束共享内存上界（块 ≤ 32 MiB、并发 ≤ 16）；`engine.with_budget(Arc::new(budget))` 让多个引擎共用同一门禁。

### 编写自己的 service

外部 crate 只依赖公开契约即可接入新后端，不需要修改核心的任何枚举或
私有 helper。从 `Service` + `Source` 起步，上传 / 下载按实际能力实现，
未支持的端口返回 `Unsupported`。见
[service 开发指南](docs/SERVICE.zh-CN.md)、[设计文档](docs/DESIGN.zh-CN.md)
与 [examples/consumer](examples/consumer/)（含第三方只读 service 示例）。

## `wb`：第一个集成应用

`wb` 完全通过上述公开 API 组装：OAuth 授权、盘配置、交互选择与传输面板
都在宿主层，不使用核心私有接口。它既是日常可用的命令行工具，
也是宿主集成的参考实现。

### 安装

支持 Linux 和 macOS。从 crates.io 安装：

```sh
cargo install waybill-cli --version 0.1.0 --locked
wb --help
```

仓库内也可以用 `cargo run -p waybill-cli -- <命令>` 运行。

### 快速开始（Google Drive）

准备 Google OAuth **桌面应用**客户端 JSON，并在对应 Google Cloud 项目启用
Drive API。登录后使用账户邮箱配置一个盘：

```sh
wb login gdrive --client /path/to/desktop.json
wb drive add personal --account account@example.com
wb drive root personal   # 选择默认根目录
```

把 `account@example.com` 替换为登录账户邮箱；如果登录时设置了 `--account`，
使用该别名。第一个盘自动成为默认盘：

```sh
wb list                       # 浏览默认盘
wb put                        # 多选本地文件，再选择云端目录
wb get --into ~/Downloads     # 多选云端文件，下载到已有本地目录
wb status                     # 查看在途记录与完成回执
```

授权使用 `drive.file` 创建和修改本应用文件，使用 `drive.readonly` 读取云盘。
凭证保存在本机私有目录；上传目标仍须满足 Google Drive 的应用访问权限。

### WebDAV

```sh
wb login --account nas webdav --endpoint https://dav.example.com/files/ --username alice
wb drive add nas --provider webdav --account nas
wb --drive nas put ./a.zip --to backup/
wb --drive nas get backup/a.zip ./a.zip
```

密码交互输入，或用 `--password-stdin` 从标准输入读取；`--auth digest` /
`--auth anonymous` 切换认证方式。端点包含服务端根路径，盘的 `--root` 是
端点下的相对目录。普通 PUT 中断后须用 `--allow-restart` 显式允许整文件重传；
已校验暂存可只重试发布。服务端要求与兼容矩阵见
[WebDAV 验收记录](docs/webdav-acceptance.zh-CN.md)。

### 对象存储

```sh
wb login --account cloud object --config object.json
wb drive add cloud --provider object --account cloud
wb --drive cloud put ./model.bin --to models/ --no-tui
wb --drive cloud get models/model.bin ./downloaded.bin --no-tui
```

配置沿用各 OpenDAL 后端字段，支持 `${ENV}` 凭证引用。本地 RustFS / S3
Docker 与阿里云 OSS 真实验收已通过；COS 等其他公有云尚未验收，详见
[对象存储接入](docs/object-storage.zh-CN.md)。

### 命令与交互

| 命令 | 用途 |
|---|---|
| `wb login gdrive / webdav / object` | 登录 Google Drive、WebDAV 或导入对象存储配置 |
| `wb drive add / list / use / root / remove` | 管理命名盘、默认盘与根目录 |
| `wb list [PATH]` | 列出指定目录；省略路径时交互浏览 |
| `wb put [SRC…] --to PATH` | 上传文件；缺少源或目标时交互选择 |
| `wb get [SOURCE] [DEST]` | 下载文件；缺少源或目标时交互选择 |
| `wb status` | 列出本机恢复记录和完成回执 |

选择器中，Enter 进入目录，Space 选择文件，`c` 确认，← / Backspace 返回上级，
`q` / Esc / Ctrl-C 取消；选择跨目录保留。`wb status` 在终端默认打开本机
运单面板（`--no-tui` 输出列表，`--json` 输出 JSON 数组）。传输时默认显示
全屏面板，`--no-tui` 使用行式输出；首次 Ctrl-C 优雅停止，再次立即中止，
保留源文件、盘配置与本机记录，重跑同一命令即恢复。

### 直接执行与脚本

指定完整参数即可直接传输；路径相对于所选盘的默认根目录，`/` 表示该根目录，
多文件上传的目标目录以 `/` 结尾：

```sh
wb put ./a.zip ./b.zip --to backup/
wb get backup/a.zip ./a.zip
wb --drive work list /
wb --json put ./a.zip --to backup/
```

`--drive NAME` 临时切换盘，`--root ROOT` 临时覆盖根目录。也可以直接指定
账户与完整 URI：

```sh
wb get 'gdrive://account@example.com/backup/a.zip' ./a.zip
wb get 'webdav://nas/backup/a.zip' ./a.zip
```

完整 URI 从账户根目录开始，不能与 `--drive` 同时使用；`--json`、`--no-tui`
和非终端环境不会自动进入选择器。具体选项见 `wb <命令> --help`。

### 文件与发布规则

多选下载需要已有本地目录；来自不同云端目录的同名文件会在传输前报错。
Google 原生文档可浏览，但不支持导出下载；目录不做递归下载。下载采用
同文件系统发布，不覆盖已有文件；同名冲突可用 `--conflict operation-suffix`
保留另一份文件。

## 参与贡献

欢迎提交 Issue 和 Pull Request。涉及恢复语义或新后端时，请说明使用场景、
后端能力与失败后的预期行为；架构约束见[设计文档](docs/DESIGN.zh-CN.md)。

工作区使用 Rust 2024。提交前运行：

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

真实后端测试请使用专用测试文件与目录，不要提交凭证或私有会话状态。
已有验证与待补场景见[验收记录](docs/VALIDATION.zh-CN.md)。

## 许可证

采用 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双许可证，由使用者选择。
