# waybill CLI 市场扫描（消费者立项前调研）

> 日期：2026-10-01（建仓同日）
> 性质：CLI 消费者的市场调研记录，回答「市场上有没有这样的工具、做成 CLI
> 差异化在哪」。库层面的生态调研见 [DESIGN.zh-CN.md](DESIGN.zh-CN.md) §3。
> 结论先行：**CLI 这边不是空白，是巨兽领地——但「多后端 × 跨进程持久断点 ×
> 回执幂等」这个交集没有人占。**

## 1. 市场对照（2026-10-01 查证）

| 工具 | 多后端广度 | 跨进程持久断点 | 回执幂等 | 备注 |
|---|---|---|---|---|
| **rclone**（Go，50k+★） | ✅ 70+ 后端 | ❌ 整文件重传 | ⚠️ 仅 size/modtime 级跳过 | 生态巨兽：sync / mount / serve / crypt 全家桶 |
| **azcopy**（Azure 官方） | ❌ 单云 | ✅ journal + `jobs resume` | ⚠️ job 语义 | |
| **gsutil / gcloud storage** | ❌ 单云 | ✅ tracker 文件持久化上传进度 | ⚠️ | |
| **ossutil**（阿里官方） | ❌ 单云 | ✅ checkpoint 文件断点续传 | ⚠️ | 国内场景成熟 |
| **snapdir**（Rust，小众） | ⚠️ local / S3 / GCS | ✅ 内容寻址快照 | ✅ | 最接近的 Rust 同类，但走 sync 路线、无云盘后端 |
| **curl / aria2** | ❌ 通用 HTTP | ✅（下载续传） | ❌ | 下载器，无云后端概念 |
| **waybill CLI（设想）** | ✅（经库） | ✅ checkpoint 信封 | ✅ 回执键 | 本文档论证的对象 |

一手证据：

- [rclone#3019](https://github.com/rclone/rclone/issues/3019)：B2 大文件中断后
  恢复上传从 0% 重新开始、另起一个新的大文件对象——长期开放。
- rclone 维护者在论坛的表述：重启后**跳过已完成文件，但不恢复半成品**
  （"will not resume half-finished files"）；用户侧原话 "does not resume
  anything"。个别后端有零星恢复 flag（`--oos-attempt-resume-upload`、
  `--koofr-upload-resume-limit`），属后端特例而非引擎能力。
- azcopy 的 journal、gsutil 的 tracker、ossutil 的 checkpoint
  文件：三家官方 CLI **不约而同各自造了「持久断点」这个轮子**。

## 2. 两个关键读数

1. **「持久断点」的需求被官方 CLI 各自证明过。** 三家单云官方工具都自己
   实现了跨进程恢复（journal / tracker / checkpoint），说明这是真实刚需，
   只是没人把它做成跨后端的通用能力。
2. **rclone 的广度 × 官方 CLI 的可靠性，这个交集空着。** rclone 的架构是
   「每文件一个幂等事务、中断即整文件重来」，升级到持久会话等于重写传输
   引擎——六年没做不是不想做，是架构性的改不动。这恰好是 waybill 的
   全部立足点：checkpoint 信封 + 回执模型从第一天就是核心，不是后加的 flag。

## 3. 定位：不与 rclone 打全面战争

rclone 的护城河是生态（mount / serve / crypt / 几十个后端），不是可靠性。
waybill CLI 的身份首先是**库的第一个消费者**：产品职责是让库的价值可见，
顺带服务作者自己的场景（ComfyUI 产物上传、坚果云、闲置 OSS）。

差异化压在窄门上：

```text
rclone 的广度 × 官方 CLI 的断点可靠性 × 回执幂等 × agent 原生
```

动词集（v1 刻意窄）：

```text
wb login <provider>        # 凭证配置（OAuth / 静态凭证，落 0600）
wb put <src...> <dest-uri> # 投递：回执查重 → 会话上传 → 完成回执；
                           #   中断后 wb resume 续传
wb get <src-uri> <dest>    # 下载：云 → 本地 .part，崩溃恢复后续传
wb status                  # 列出在途会话与可恢复 checkpoint
wb mcp                     # 起 MCP server，agent 直接获得 cloud_upload 能力
```

`--json` 机器可读输出是一等公民（CI / agent 场景），TTY 进度给人看。

**v1 明确不做**：sync / mirror / mount / serve——rclone 的地盘，也是设计文档
写死的「不做同步引擎」边界（单向 mirror 是否进 v2 届时另议，见
DESIGN.zh-CN.md 风险节）。

## 4. 目标用户

- 弱网下传大文件的人：国内移动网络 + 坚果云限速是天然主场；
- 把产物可靠投递到云的 agent 工作流（ComfyUI 出图、MCP 客户端）；
- 对 rclone 半成品重传忍无可忍的人（#3019 里的常客）。

## 5. 风险与成功标准

1. **rclone 哪天真做了 durable resume**——它有后端级恢复 flag 的先例，但
   架构升级等于重写引擎。waybill 的护城河是架构性的（会话与回执是核心
   而非附加），这是防御而非侥幸。
2. **CLI 的成功标准要定对**：不是抢 rclone 用户，是库的展示窗口 + 自用
   工具。用户数是滞后指标，先看「作者自己的 ComfyUI / 坚果云 / OSS 流程
   是否天天在用它」。
