# waybill

```
       \\
       (o>
    \\_//)
     \_/_)
      _|_
```

**Reliable bidirectional delivery between local files and cloud storage.**
Resumable transfers that survive crashes, retries that converge instead of
duplicating, and publishes with atomic boundaries — as a Rust library.

> *waybill* (n.) 运单 — the document that accompanies a shipment: its receipt,
> its tracking state, and its proof of delivery. The mascot is **Bill**, a
> courier goose with a waybill tube on his leg — more on him below.

## Why

Rust has excellent *access* layers (OpenDAL, `object_store`): one API, many
backends, operations assumed to complete within a process. It has **nothing**
for the case every long-running app actually hits — the process dies
mid-upload, the network drops, the user hits retry:

| | Access layers (`opendal`, `object_store`) | `waybill` |
|---|---|---|
| Assumption | single-process operations | processes die, networks drop, users retry |
| Core value | one API across backends | crash-safe resume, receipt-idempotent retry, atomic publish |
| Backend differences | flattened away | **typed** in an honest capability matrix |

The three requirements `waybill` exists to enforce:

1. **Receipt idempotency** — dedupe by a receipt key; a retry after a lost
   completion response never creates a duplicate object; batch retries skip
   already-delivered files.
2. **Durable resume** — upload/download session state is serializable
   (an opaque, versioned checkpoint the caller stores with 0600 discipline);
   after a crash, reconcile the committed extent against the server before
   continuing.
3. **Bounded memory** — sources and sinks are read/written by precise ranges.
   No unbounded buffers, ever.

A capability matrix types the degradation, so callers decide instead of
assuming:

```text
resume        : Durable (Drive resumable, OSS multipart) | Restart (plain
                WebDAV: retransmit, but .part + MOVE keeps the publish atomic)
                | Unsupported (bridged long tail)
idempotency   : metadata receipt | deterministic key | server-side conditional
atomic publish: rename | remote complete | copy — declared, not assumed
```

Explicitly **not** goals: a bidirectional sync engine (unison/syncthing-style,
see the design doc §2), an OpenDAL API compatibility layer, or multi-language
bindings.

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** (now) | Open `Service` / `Source` / `Sink` contracts, capability model, checkpoint envelope, third-party service examples |
| **M1** | `service-fs` + `service-gdrive`: full bidirectional chain — Drive object → local `.part` → crash → resume → publish |
| **M2** | `service-webdav`: streamed upload, range download, real-server matrix (Synology / QNAP / Nutstore / Nextcloud) |
| **M3** | `service-oss`: multipart, `ListParts` reconciliation, completion accounting |
| later | optional `service-opendal` bridge for the long tail — honestly declared `Unsupported` where durable resume doesn't exist |

The full design — motivation, ecosystem survey, API sketches, the open
extension contract for third-party services — lives in
[docs/DESIGN.zh-CN.md](docs/DESIGN.zh-CN.md) (Chinese, English translation
welcome as a contribution). The CLI market scan — why this doesn't fight
rclone head-on — lives in
[docs/market-cli.zh-CN.md](docs/market-cli.zh-CN.md).

## Bill, the courier goose

Bill migrates both ways every year (bidirectional transfer), refuels at the
same wetlands (checkpoints), returns to the same nest (idempotent receipts),
and flies in a V formation taking turns leading (concurrent parts). He is
stubborn about not losing your package. 中文圈叫他**雁哥**.

## Status & license

Pre-M0: contracts under design, no public API yet, not published to
crates.io. Dual-licensed under MIT or Apache-2.0, at your option. First
consumer: [SwarmDrop](https://github.com/yexiyue/SwarmDrop) — cross-network,
end-to-end encrypted file transfer (the in-tree Drive/local delivery code is
the seed this library generalizes).

---

## 中文简介

**waybill（运单）**：云端与本地之间的可恢复双向传输库。

- **回执幂等**：以回执键查重，完成响应丢失后的重试不产生重复对象；批量上传
  部分失败，重试自动跳过已完成项。
- **持久断点**：上传 / 下载会话状态可序列化（版本化信封 + 驱动私有状态），
  崩溃重启后先对账服务端已确认区间再续传。
- **内存上界**：源 / 目标一律区间读写，禁止无界缓冲。
- **类型化降级**：恢复策略、幂等判据、发布保证都在能力矩阵上诚实声明，
  能力不足在开始前明确拒绝，不静默降级。

定位：OpenDAL / object_store 是「访问层」，waybill 是「交付层」——假设进程会死、
网络会断、用户会重试。设计文档（动机、生态调研、API 草图、第三方 service
开放契约）见 [docs/DESIGN.zh-CN.md](docs/DESIGN.zh-CN.md)。

吉祥物 **Bill（雁哥）**：一只腿绑运单筒的邮差鸿雁——双向迁徙、湿地补给、
年年归巢、雁阵领飞，分别对应双向传输、checkpoint、回执幂等与分片并发。
