# waybill

<p align="center">
  <img src="assets/brand/readme-banner-v1.png" alt="waybill — Bill the courier goose with a waybill tube strapped to his leg" width="960">
</p>

<p align="center">
  <strong>Resumable delivery. Both ways.</strong><br>
  A Rust library for moving files between local storage and the cloud.
</p>

<p align="center">
  <a href="README.zh-CN.md">简体中文</a> ·
  <a href="docs/DESIGN.zh-CN.md">Design</a> ·
  <a href="docs/BRAND.zh-CN.md">Meet Bill</a> ·
  <a href="https://github.com/yexiyue/waybill/actions/workflows/ci.yml">CI</a>
</p>

> **Early development · M1 upload recovery prototype, unpublished.** Public upload
> contracts, stable local sources, file checkpoints, GDrive resumable uploads and
> reconciliation, and an independent consumer example are implemented. Downloads
> and local publication remain planned. Local fault tests and live Google acceptance
> are reported separately in [validation notes](docs/VALIDATION.zh-CN.md).

## Delivery that can pick up where it stopped

A large transfer can outlive its process. A connection drops, an app restarts,
or the destination becomes unavailable just as the file is ready to publish.
waybill is being designed to preserve enough state to continue safely.

The name comes from a **waybill**: the document that travels with a shipment,
recording its identity, progress, and delivery receipt.

| Design goal | What it means |
|---|---|
| **Durable recovery** | Versioned checkpoints bind progress to a source and destination. Recovery reconciles persisted data or the remote session before continuing. |
| **Receipt-based retries** | An operation ID and completion evidence help retries converge, within each service's guarantees. |
| **Bounded resources** | Chunk sizes, concurrency, prefetch, and buffers share an explicit budget. |
| **Honest capabilities** | Upload recovery, download recovery, version checks, and publishing guarantees are declared separately. |

For downloads, the planned lifecycle is: read missing ranges, write a local
.part file, persist confirmed progress, validate, then publish. A failed publish
keeps staging data available for another attempt.

For uploads, the service determines whether recovery uses a continuous offset,
multipart state, or a full restart. A caller that requires durable recovery can
reject an unsuitable service before starting.

## One engine, independently implemented services

```mermaid
flowchart LR
    LocalSource[Local file source] --> Engine[Transfer engine]
    CloudSource[Cloud object source] --> Engine
    Engine --> LocalSink[Local staging and publish]
    Engine --> CloudSink[Cloud upload session]
    Engine --> Checkpoint[Checkpoint store]
```

The core provides Service, Source, UploadSink, capabilities, checkpoints and upload
scheduling. GDrive owns its short-lived token interface; the host owns authorization
and refresh. Each service implements a
backend through those public contracts.

**The services maintained here use the same extension boundary as services
written by other developers.** An external service should be able to live in its
own crate, depend on waybill, and be injected without adding a provider enum
variant or changing the engine.

| Service / status | Role |
|---|---|
| waybill-service-fs · prototype | Stable local sources, range reads, BLAKE3 validation, and file checkpoints; download publication is planned |
| waybill-service-gdrive · prototype | Google Drive uploads, resumable sessions, and completion reconciliation |
| waybill-service-webdav · planned | Streaming uploads, range downloads, and declared server capabilities |
| waybill-service-oss · planned | Aliyun OSS access, multipart sessions, and part reconciliation |
| External service crates | Additional backends using the same public extension contracts |

A read-only service can participate without implementing uploads. The
[independent consumer](examples/consumer/) uses only public APIs; the
[service guide](docs/SERVICE.zh-CN.md) describes the implemented contracts.
APIs are 0.x prototypes. Operator and registry sketches remain design material.

`UploadEngine` validates the source, instance, and remote state before resuming.
Expired sessions return `SessionExpired` and retain their checkpoint by default.
Explicit permission permits at most two restarts per run. Receipts are persisted
before success; call `confirm` only after committing host bookkeeping. The host
must freeze the source and owns its cleanup.

## Working with the Rust storage ecosystem

The design draws on access and multipart primitives from OpenDAL and
object_store. Protocol clients, HTTP transports, and signing libraries can be
reused while waybill owns the transfer lifecycle and its recovery state.

An optional OpenDAL service may follow a concrete demand for another backend.
Range reads and source-version checks can be combined with local checkpoints
for download recovery. Upload-session recovery needs separate backend support.

Each service declares its guarantees. A WebDAV MOVE, an object key, or an ETag
alone does not establish universal atomicity, content integrity, or exactly-once
delivery. The [design document](docs/DESIGN.zh-CN.md) records the research and
proposed boundaries.

The initial implementation uploads local files to GDrive; the long-term scope is
file delivery between local and cloud storage. Directory
sync, conflict merging, and multi-device synchronization are outside that scope.

## Roadmap

| Milestone | Planned outcome |
|---|---|
| **M0** | Open contracts and versioned checkpoints |
| **M1 — current** | Stable local sources, GDrive upload recovery, and independent consumer integration |
| **CLI — in progress** | The `wb` command line (login / put / status): GDrive upload loop with a fullscreen dashboard and a `--json` event stream; downloads and MCP wait for M2+ |
| **M2** | GDrive downloads, local staging, recovery and publication |
| **M3** | WebDAV and a real-server compatibility matrix |
| **M4** | OSS multipart recovery and completion reconciliation |
| **On demand** | OpenDAL adapter, starting with a concrete additional backend and its download path |

The first intended consumer is
[SwarmDrop](https://github.com/swarm-apps/SwarmDrop). Its existing local-file and
Drive delivery implementations provide the starting material. waybill's public
contracts remain independent of its UI, device identity, and P2P protocol.

## Meet Bill

<p align="center">
  <img src="assets/brand/bill-mascot-v1.png" alt="Bill, a cream-and-grey courier goose with an orange bill and a teal document tube on his leg" width="240">
</p>

**Bill**, known as **雁哥** in Chinese, is our courier goose. His two-way migration
fits the library's two-way delivery, and the tube on his leg keeps the waybill
close throughout the journey.

The [brand guide](docs/BRAND.zh-CN.md) includes the mascot, avatar, README banner,
social cover, and generation prompts.

## Contributing

Start with the [design document](docs/DESIGN.zh-CN.md). Contributions around the
public service boundary, recovery behavior, and real backend constraints are
especially useful at this stage. Local fault tests and live Drive upload / process
recovery have passed; see the [validation record](docs/VALIDATION.zh-CN.md) for the
exact scope and remaining gaps.

The workspace uses **Rust 2024**, with a declared minimum Rust version of **1.88**.
To check the current implementation:

```sh
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
```

Please discuss a service's capabilities and recovery guarantees before building
against the API sketches. The sketches are design material and may change.

## License

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
