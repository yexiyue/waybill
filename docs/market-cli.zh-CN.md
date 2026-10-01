# waybill CLI 市场扫描（消费者立项前调研）

> 日期：2026-10-01（建仓同日）
> 性质：CLI 消费者的市场调研记录，回答「市场上有没有这样的工具、做成 CLI
> 差异化在哪」。库层面的生态调研见 [DESIGN.zh-CN.md](DESIGN.zh-CN.md) §3。
> 当前判断：CLI 已有成熟竞争者；多后端持久恢复与完成对账的差异化仍需验证。
> 本轮未重新核验市场数据、星数与所有后端特例；以下保留原调研日期。
>
> 2026-10-01 追记：`wb` 已开始实现（`crates/waybill-cli`），首版动词为
> login / put / status（上传侧闭环；`--json` 一等公民）；`get` 等 M2 下载
> 入库。`mcp` 后置为独立里程碑（原因是排期而非工具链——同日 rust-version
> 已统一抬至 1.88，rmcp 的 1.88 下限不再构成障碍）。

## 1. 市场对照（2026-10-01 查证）

| 工具 | 多后端广度 | 跨进程持久断点 | 回执幂等 | 备注 |
|---|---|---|---|---|
| **rclone**（Go，50k+★） | ✅ 70+ 后端 | ❌ 整文件重传 | ⚠️ 仅 size/modtime 级跳过 | 生态巨兽：sync / mount / serve / crypt 全家桶 |
| **azcopy**（Azure 官方） | ❌ 单云 | ✅ journal + `jobs resume` | ⚠️ job 语义 | |
| **gsutil / gcloud storage** | ❌ 单云 | ✅ tracker 文件持久化上传进度 | ⚠️ | |
| **ossutil**（阿里官方） | ❌ 单云 | ✅ checkpoint 文件断点续传 | ⚠️ | 国内场景成熟 |
| **snapdir**（Rust，小众） | ⚠️ local / S3 / GCS | ✅ 内容寻址快照 | ✅ | 最接近的 Rust 同类，但走 sync 路线、无云盘后端 |
| **curl / aria2** | ❌ 通用 HTTP | ✅（下载续传） | ❌ | 下载器，无云后端概念 |
| **waybill CLI（设想）** | 首期 GDrive | 按能力与会话有效性 | 同操作对账 | 本文档论证的对象 |

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

1. 官方 CLI 的恢复功能说明此类场景值得验证，不能推出市场无人覆盖。
2. rclone 部分后端与版本的恢复行为需要分别核验，历史 issue 不证明架构无法演进。
   waybill 的价值应由跨进程故障验收与独立消费者使用证明。

## 3. 定位：不与 rclone 打全面战争

rclone 的护城河是生态（mount / serve / crypt / 几十个后端），同时包含多年服务端兼容与可靠性投入。
waybill CLI 的身份首先是**库的独立验收消费者与展示工具**：产品职责是让库的价值可见，
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
写死的「不做同步引擎」边界（单向 mirror 是否进 v2 届时另议，后续需独立设计）。

## 4. 目标用户

- 弱网下传大文件的人：国内移动网络 + 坚果云限速是天然主场；
- 把产物可靠投递到云的 agent 工作流（ComfyUI 出图、MCP 客户端）；
- 对 rclone 半成品重传忍无可忍的人（#3019 里的常客）。

## 5. 风险与成功标准

1. **rclone 哪天真做了 durable resume**——它有后端级恢复 flag 的先例，但
   其后端级能力可能继续演进。waybill 应以实际恢复体验与维护质量验证价值。
2. **CLI 的成功标准要定对**：不是抢 rclone 用户，是库的展示窗口 + 自用
   工具。用户数是滞后指标，先看「作者自己的 ComfyUI / 坚果云 / OSS 流程
   是否天天在用它」。
