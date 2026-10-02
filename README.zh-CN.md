# waybill · 运单

<p align="center">
  <img src="assets/brand/readme-banner-v1.png" alt="waybill：腿绑运单筒的邮差鸿雁 Bill" width="960">
</p>

<p align="center">
  <strong>可恢复交付，往返皆可。</strong><br>
  Rust 文件传输库与命令行工具，连接本地存储与云端。
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="docs/DESIGN.zh-CN.md">设计</a> ·
  <a href="docs/STATUS.zh-CN.md">项目进度</a> ·
  <a href="https://github.com/yexiyue/waybill/actions/workflows/ci.yml">CI</a>
</p>

waybill 为上传和下载保存持久进度与完成回执。传输中断后，重新运行同一命令，
即可核验源文件和已保存状态，按后端策略恢复或复用完成结果。

你可以用 `wb` 管理 Google Drive、WebDAV 与对象存储文件，也可以通过 Rust 公开接口把传输能力嵌入应用。
项目名称来自随货同行的「运单」；[Bill · 雁哥](docs/BRAND.zh-CN.md) 是我们的邮差鸿雁。

## 功能

- **双向传输**：Google Drive、WebDAV 与对象存储上传、下载及目录浏览，按后端能力开放。
- **持久恢复**：保存 checkpoint，恢复时对账源版本、远端会话或本地暂存数据。
- **交互选择**：命名盘、默认根目录、本地与云端文件多选，以及全屏传输面板。
- **脚本集成**：完整参数直接执行，支持逐行 JSON 事件与行式进度。
- **安全发布**：下载校验、同文件系统无覆盖发布；发布失败保留可重试状态。
- **开放扩展**：核心与后端独立，service 通过公开契约声明能力与恢复保证。

waybill 面向文件交付；目录同步、冲突合并与多设备同步不在当前功能范围内。

## 安装

支持 Linux 和 macOS，需要 **Rust 1.91 或更高版本**。从源码安装 `wb`：

```sh
git clone https://github.com/yexiyue/waybill.git
cd waybill
cargo install --path crates/waybill-cli --locked
wb --help
```

也可以在仓库中用 `cargo run -p waybill-cli -- <命令>` 运行。
项目尚未发布稳定版本，公开 API 可能调整；发布计划与验证范围见
[项目进度](docs/STATUS.zh-CN.md)。

## 快速开始

准备 Google OAuth **桌面应用**客户端 JSON，并在对应 Google Cloud 项目启用 Drive API。
登录后使用账户邮箱配置一个盘：

```sh
wb login gdrive --client /path/to/desktop.json
wb drive add personal --account account@example.com
```

把 `account@example.com` 替换为登录账户邮箱；如果登录时设置了 `--account`，使用该别名。
第一个盘自动成为默认盘。可以选择一个云端目录作为之后所有短路径的起点：

```sh
wb drive root personal
```

现在不必反复填写完整云盘 URI：

```sh
wb list                       # 浏览默认盘
wb put                        # 多选本地文件，再选择云端目录
wb get --into ~/Downloads     # 多选云端文件，下载到已有本地目录
wb status                     # 查看在途记录与完成回执
```

授权使用 `drive.file` 创建和修改本应用文件，使用 `drive.readonly` 读取云盘。
凭证保存在本机私有目录；上传目标仍须满足 Google Drive 的应用访问权限。

### WebDAV

配置端点和用户名；密码交互输入，也可用 `--password-stdin` 从标准输入读取。

```sh
wb login --account nas webdav --endpoint https://dav.example.com/files/ --username alice
wb drive add nas --provider webdav --account nas
wb --drive nas list
wb --drive nas put ./a.zip --to backup/
wb --drive nas get backup/a.zip ./a.zip
```

`--auth digest` 使用 Digest，`--auth anonymous` 使用匿名访问。端点包含服务端
根路径，盘的 `--root` 是此端点下的相对目录；`wb drive root nas` 可以交互选择。
所有后端共用文件选择器和传输面板。普通 WebDAV PUT 不支持偏移续传：中断后
必须用 `--allow-restart` 显式允许整文件重传；已校验暂存可只重试发布。
服务端要求与兼容矩阵见 [WebDAV 验收记录](docs/webdav-acceptance.zh-CN.md)。

### 对象存储（Apache OpenDAL）

新增独立 `waybill-service-opendal`，默认启用 S3、OSS、COS、OBS、TOS、GCS、Azure Blob、
B2、Swift、Upyun 和 Vercel Blob；库消费者可按 feature 裁剪。

```sh
wb login --account cloud object --config object.json
wb drive add cloud --provider object --account cloud
wb --drive cloud put ./model.bin --to models/ --no-tui
wb --drive cloud get models/model.bin ./downloaded.bin --no-tui
wb --drive cloud list models/ --no-tui
```

配置沿用各 OpenDAL 后端字段，支持 `${ENV}` 凭证引用。上传默认关闭；确认服务端条件写入
有效后设置 `conditional_writes: true`。上传中断需显式允许整文件重传，已验证暂存可重试发布。
后端名称支持不代表全部交付能力支持：当前 B2、Upyun、Vercel Blob 等缺少适配器所需条件读取，
OBS、Swift 等缺少条件上传。RustFS / S3 本地 Docker 与阿里云 OSS 真实验收已通过；COS 等其他公有云尚未验收。
配置示例、能力边界和复现步骤见[对象存储接入](docs/object-storage.zh-CN.md)。

## 命令与交互

| 命令 | 用途 |
|---|---|
| `wb login gdrive / webdav / object` | 登录 Google Drive、WebDAV 或导入对象存储配置 |
| `wb drive add / list / use / root / remove` | 管理命名盘、默认盘与根目录 |
| `wb list [PATH]` | 列出指定目录；省略路径时交互浏览 |
| `wb put [SRC…] --to PATH` | 上传文件；缺少源或目标时交互选择 |
| `wb get [SOURCE] [DEST]` | 下载文件；缺少源或目标时交互选择 |
| `wb status` | 列出本机恢复记录和完成回执 |

选择器中，Enter 进入目录，Space 选择文件，`c` 确认，← / Backspace 返回上级，
`q` / Esc / Ctrl-C 取消。文件选择跨目录保留。

`wb status` 在终端默认打开本机运单面板：Enter / `d` 查看详情，`r` 刷新，
`e` 查看读取问题（↑↓ 翻阅），`q` 退出。`wb status --no-tui` 输出列表，
`wb --json status` 输出 JSON 数组。选择器与传输面板共用全屏会话，步骤切换不退出终端。

传输时默认显示全屏面板；`--no-tui` 使用行式输出。首次 Ctrl-C 优雅停止，
再次 Ctrl-C 立即中止。保留源文件、盘配置与本机记录，按后端恢复策略重跑。

### 直接执行与脚本

指定完整参数即可直接传输；路径相对于所选盘的默认根目录，`/` 表示该根目录。
多文件上传的目标目录以 `/` 结尾。

```sh
wb put ./a.zip ./b.zip --to backup/
wb list backup/
wb get backup/a.zip ./a.zip
wb --drive work list /
wb --json put ./a.zip --to backup/
wb get backup/a.zip ./a.zip --no-tui
```

`--drive NAME` 临时切换盘，`--root ROOT` 临时覆盖根目录（GDrive 对象 ID 或 WebDAV / 对象存储相对目录）。
`wb drive use NAME` 修改默认盘；`wb drive remove NAME` 保留登录凭证和恢复记录。

也可以直接指定账户与完整 URI：

```sh
wb get 'gdrive://account@example.com/backup/a.zip' ./a.zip
wb get 'webdav://nas/backup/a.zip' ./a.zip
```

完整 URI 从账户根目录开始（GDrive 为 Google 根目录，WebDAV 为登录端点，对象存储为 Operator 配置根；可用 `--root` 覆盖），不能与 `--drive` 同时使用。
`--json`、`--no-tui` 和非终端环境不会自动进入选择器，缺少必要参数会报错。
具体选项见 `wb <命令> --help`。

### 文件与发布规则

多选下载需要已有本地目录；来自不同云端目录的同名文件会在传输前报错。
Google 原生文档可浏览，但不支持导出下载；目录不做递归下载。
下载采用同文件系统发布，不覆盖已有文件，也不支持跨文件系统复制发布。
同名冲突可用 `--conflict operation-suffix` 保留另一份文件。

## Rust 集成

| Crate | 职责 |
|---|---|
| [`waybill`](crates/waybill/) | 上传与下载契约、传输引擎、能力声明与 checkpoint |
| [`waybill-service-fs`](crates/waybill-service-fs/) | 稳定本地源、文件 checkpoint、下载暂存与发布 |
| [`waybill-service-gdrive`](crates/waybill-service-gdrive/) | Google Drive 协议、上传会话对账、范围读取与目录访问 |
| [`waybill-service-webdav`](crates/waybill-service-webdav/) | 目录和范围读取、整文件流式上传、内容对账及条件 MOVE |
| [`waybill-cli`](crates/waybill-cli/) | `wb`：授权、盘配置、交互与传输编排 |

```rust
let engine = waybill::TransferEngine::new(checkpoint_store);
let receipt = engine.upload(source.as_ref(), sink.as_ref(),
    waybill::UploadOptions::new("stable-operation-id", "backup/file.zip")).await?;
// 宿主先提交业务账本，再调用 engine.confirm(&receipt)。
```

核心不绑定具体后端或执行器。宿主负责授权与凭证刷新，service 在请求边界获取有效凭证。
外部 service 与仓库内 service 使用相同公开接口，按实际能力实现上传、下载与恢复。
偏移上传用 `engine.upload`，整文件上传用 `engine.upload_stream`（源为 `Arc<dyn Source>`）；
两者共享存储、预算与回执生命周期。

从[独立消费者示例](examples/consumer/)开始集成，或阅读
[service 开发指南](docs/SERVICE.zh-CN.md)与[设计文档](docs/DESIGN.zh-CN.md)。

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
