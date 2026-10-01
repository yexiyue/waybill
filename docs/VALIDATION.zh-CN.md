# 首期分层验收记录（2026-10-01）

当前交付是未发布的 0.x GDrive 上传原型。范围为稳定本地源、分块上传、持久
checkpoint、跨进程对账与完成回执；下载 service、本地发布与 SwarmDrop 正式回接
不在本期。架构事实源为 [DESIGN.zh-CN.md](DESIGN.zh-CN.md)，接入方法见
[SERVICE.zh-CN.md](SERVICE.zh-CN.md)。

## 构建与平台

| 环境 | 实际结果 |
|---|---|
| macOS 26.6.2 / arm64 / Rust 1.98.1 | fmt、all-targets check、Clippy `-D warnings`、test、rustdoc `-D warnings` 通过 |
| macOS / arm64 / Rust 1.85.0 | all-targets check、test 通过 |
| Debian bookworm Linux 容器 / aarch64 / Rust 1.85.0 | fmt、all-targets check、Clippy `-D warnings`、test、rustdoc `-D warnings` 通过 |
| GitHub Actions | 已配置 Linux / macOS stable 门禁（2026-10-01 起不再维护独立 MSRV 检查）；未提交，未运行远端 CI |

Linux 在本机 OrbStack 中运行官方 `rust:1.85.0-bookworm`，镜像摘要
`sha256:0ff31c9ffa641a62e48d543fb00b4960955ea375f40776f40f585b89e654cc5e`；
代码只读挂载，编译产物使用独立 volume。未据此宣称 x86_64、Windows、浏览器、
网络文件系统或真实断电恢复已经验收。

工作区测试为 22 个普通测试：核心 3、独立只读消费者 1、fs 3、GDrive 15。
另有两个标为 ignored 的子进程辅助入口，由父测试明确启动并通过，不是遗漏场景。
`cargo tree --workspace --locked` 不含 SwarmDrop；核心依赖树仅包含 serde、thiserror
及其派生依赖，没有 Tokio、reqwest 或具体 service。

## 契约、存储与协议替身

| 场景 | 实际验证 |
|---|---|
| 外部 service | 独立消费者只依赖公开 API；第三方只读 service 无须实现上传或修改核心枚举 |
| 共享预算 | 超额操作返回 ResourceBusy；丢弃运行 future 释放预算；块与并发配置拒绝越界 |
| 文件 checkpoint | 0600 文件、独占锁、子进程写入排斥、损坏 / 超大记录拒绝且保留；超大保存不覆盖旧记录 |
| 恢复绑定 | 公共 / 私有版本不兼容、源变化、不同实例和错误确认回执均拒绝，保留记录 |
| 服务端偏移 | 对账领先本机持久偏移、服务端偏移回退均按服务端结果恢复，不按本地发送量确认 |
| 上传失败窗口 | HTTP 替身断开完成响应后查询原对象；完成后回执保存失败，后续运行重新查询并补存回执 |
| 暂停与重启 | 暂停后续块；读取期间收到停止信号不再发块；新子进程加载真实文件 checkpoint 后续传 |
| 协议 | 空文件、跨块、异常 Range、401 单次刷新 / 二次拒绝通知、429 / 403 限流及 5xx 有界重试 |
| 会话过期 | 默认 SessionExpired 且保留；显式重建复用对象 ID；每次运行最多重建两次 |
| 目录与重名 | 不复用无归属目录；拒绝文件重名；显式操作后缀；目录创建响应丢失后按原目录 ID 对账 |
| 端点与脱敏 | 生产 session URL 限 Google HTTPS 固定路径；测试端点仅 cfg(test)；token / 私有状态 Debug 脱敏 |

HTTP 替身使用可控本机 TCP 服务，包含实际断连接；不能替代 Google 服务端行为验收。
进程测试验证重启和锁释放，不模拟文件系统断电、磁盘固件缓存或所有 fsync 失败窗口。

## 真实 Google Drive

使用用户提供的外部 desktop OAuth 配置，通过 oauth2 crate 完成宿主 PKCE 授权。
权限为 `drive.file`，新建专用验收目录，只操作生成的测试对象。凭证、session、
checkpoint 和对象 ID 保存在仓库外的私有目录；本记录不包含这些内容。
原始结果位于本机 `~/Library/Application Support/waybill/acceptance-20261001/`，
目录 0700、凭证和结果文件 0600。测试数据与恢复记录保留供审计，未删除用户源文件。

| 场景 | 实际结果 |
|---|---|
| 17 MiB 跨块上传与暂停 | 首个 8 MiB 确认并持久化后暂停；新进程从该偏移续传至完成 |
| 直接退出后的恢复 | 首块持久化后 `process::exit(75)`，不执行析构；独立进程从 8 MiB 继续，完成后下载 BLAKE3 一致 |
| 完成结果丢失 | 真实 Drive 完成后，在消费者驱动边界丢弃完成结果；核心重新查询真实对象并持久化回执，下载 BLAKE3 一致 |
| 重复操作 | 保留 checkpoint 后重复运行返回同一对象 ID；目录查询确认只有四个预期对象、每个目标一个对象 |
| 256 MiB 文件 | 32 个 8 MiB 上传块完成，流式下载后 BLAKE3 与本地一致 |

完成结果丢失注入发生在驱动返回核心之前，不宣称已经在 Google 网络传输层制造
响应丢包。网络层丢包由本机协议替身覆盖。最初一次 17 MiB 恢复上传已成功持久
完成回执，随后消费者下载校验遇到瞬时 TLS EOF；重复运行对账原对象后校验通过。
验收下载加入三次有界只读重试；库本身仍未提供下载 API。

**真实 Google 会话自然过期及过期后整文件重传未验证。** 默认暂停和显式重建仅
在协议替身中通过。真实授权拒绝、限流、回执磁盘失败及嵌套目录故障也未主动在
Google 环境制造；不得将替身结果标为真机通过。

## 资源样本

环境为上述 macOS / arm64、debug 构建、单路上传、Google Drive、当时本机网络；
未控制地理区域、网络出口、缓存或服务端负载。`/usr/bin/time -l` 测量直接运行的
探针进程，含源身份哈希、上传、对账和验收下载，不含 cargo 编译。

| 样本 | 总耗时 | 峰值 RSS | macOS peak memory footprint |
|---|---:|---:|---:|
| 17 MiB 完成结果丢失注入与校验 | 13.28 s | 26,460,160 bytes | 15,810,992 bytes |
| 17 MiB 退出后恢复与校验 | 7.38 s | 26,279,936 bytes | 15,712,688 bytes |
| 256 MiB 新上传与校验 | 73.73 s | 26,001,408 bytes | 14,909,848 bytes |

256 MiB 样本按整个探针耗时折算约 3.47 MiB/s，**不是纯上传吞吐或固定速率承诺**。
单路持有的上传块上界 8 MiB，共享调度预算上界 16 MiB；由所有权与预算门禁验证，
未用分配器追踪测量数据缓冲峰值，也未做真机双路峰值测量。进程 RSS 包含 TLS、
执行器、校验与 IO 复制等额外开销，不等于上传块预算。大文件样本采集后进一步将
Tokio 文件 IO 内部复制缓冲限制为 256 KiB；该收紧经两平台测试，但资源样本未重采。

## 后续验收与交付边界

- 在真实 Drive 会话自然过期后验收默认暂停和显式重建；补充真实网络层完成响应丢失。
- 补充双路上传、长时运行与更多网络 / 文件系统环境的资源样本。
- SwarmDrop 正式回接另立 OpenSpec；本期仅同步 D3 的 staged_complete 设计及孵化记录。
- 下载及本地最终发布进入 M2；WebDAV、OSS 和按需 OpenDAL 后移。
- 保留两仓原有未提交改动；没有 commit、push、发布或稳定 API 声明。
