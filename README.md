# waybill

<p align="center">
  <img src="assets/brand/readme-banner-v1.png" alt="waybill: Bill the courier goose with a waybill tube on his leg" width="960">
</p>

<p align="center">
  <strong>Resumable delivery. Both ways.</strong><br>
  A Rust file transfer library and CLI connecting local storage with the cloud.
</p>

<p align="center">
  <a href="README.zh-CN.md">简体中文</a> ·
  <a href="docs/DESIGN.zh-CN.md">Design</a> ·
  <a href="docs/STATUS.zh-CN.md">Project status</a> ·
  <a href="https://github.com/yexiyue/waybill/actions/workflows/ci.yml">CI</a>
</p>

waybill saves durable progress and completion receipts for uploads and downloads.
After an interruption, rerun the same command to validate the source and saved
state, then resume according to the backend policy or reuse the completed result.

Use `wb` to manage Google Drive and WebDAV files, or embed transfers through
the public Rust interfaces. The name comes from the document that travels with a
shipment; [Bill](docs/BRAND.zh-CN.md) is our courier goose.

## Features

- **Two-way transfers**: Google Drive and WebDAV uploads, downloads, and directory browsing.
- **Durable recovery**: checkpoints reconcile source versions, remote sessions, or local staging data before resuming.
- **Interactive selection**: named drives, default roots, local and cloud file selection, and a fullscreen transfer dashboard.
- **Scripting**: complete arguments run directly, with line-delimited JSON events and plain progress output.
- **Safe publication**: download verification and same-filesystem publication without overwriting; failed publication retains retryable state.
- **Open extensions**: independent services declare their capabilities and recovery guarantees through public contracts.

waybill handles file delivery. Directory synchronization, conflict merging, and
multi-device synchronization are outside the current feature set.

## Installation

Supports Linux and macOS. Requires **Rust 1.88 or newer**. Install `wb` from source:

```sh
git clone https://github.com/yexiyue/waybill.git
cd waybill
cargo install --path crates/waybill-cli --locked
wb --help
```

Inside the repository, you can also use `cargo run -p waybill-cli -- <command>`.
No stable release has been published, and public APIs may change. Release plans
and validation coverage are recorded in [project status](docs/STATUS.zh-CN.md).

## Quick start

Prepare a Google OAuth **desktop application** client JSON and enable the Drive
API in its Google Cloud project. Sign in, then configure a drive with your account:

```sh
wb login gdrive --client /path/to/desktop.json
wb drive add personal --account account@example.com
```

Replace `account@example.com` with your Google account email, or the alias supplied
with `login --account`. The first drive becomes the default. Choose a cloud folder
as the starting point for subsequent short paths:

```sh
wb drive root personal
```

You can now work without repeating a full cloud URI:

```sh
wb list                       # Browse the default drive
wb put                        # Select local files, then a cloud destination
wb get --into ~/Downloads     # Select cloud files for an existing local directory
wb status                     # Inspect pending records and completion receipts
```

Authorization uses `drive.file` to create and modify app-owned files and
`drive.readonly` to read Drive. Credentials stay in a private local directory;
upload destinations must still satisfy Google Drive's app access permissions.

### WebDAV

Configure an endpoint and username. Enter the password interactively, or use
`--password-stdin` to read it from standard input.

```sh
wb login --account nas webdav --endpoint https://dav.example.com/files/ --username alice
wb drive add nas --provider webdav --account nas
wb --drive nas list
wb --drive nas put ./a.zip --to backup/
wb --drive nas get backup/a.zip ./a.zip
```

Use `--auth digest` for Digest or `--auth anonymous` for anonymous access. The
endpoint includes the server root; a drive's `--root` is a relative directory below
it. `wb drive root nas` selects it interactively. Backends share the same pickers
and dashboard. Standard WebDAV PUT cannot resume at an offset: interrupted requests
require `--allow-restart` to retransmit the whole file. Verified staging can retry
publication alone. See the [WebDAV validation notes](docs/webdav-acceptance.zh-CN.md)
for server requirements and the tested matrix.

## Commands and interaction

| Command | Purpose |
|---|---|
| `wb login gdrive / webdav` | Sign in to Google Drive or WebDAV |
| `wb drive add / list / use / root / remove` | Manage named drives, the default drive, and roots |
| `wb list [PATH]` | List a directory, or browse interactively without a path |
| `wb put [SRC…] --to PATH` | Upload files; select missing sources or destinations interactively |
| `wb get [SOURCE] [DEST]` | Download files; select missing sources or destinations interactively |
| `wb status` | List local recovery records and completion receipts |

In a picker, Enter opens a folder, Space selects files, `c` confirms, ← / Backspace
goes up, and `q` / Esc / Ctrl-C cancels. Selections persist across folders.

`wb status` opens a fullscreen local-record browser in a terminal. Use Enter / `d`
for details, `r` to refresh, `e` for unreadable records (↑↓ to browse), and `q` to exit.
`wb status --no-tui` prints a list; `wb --json status` retains the JSON array.
Pickers and transfer dashboards share one fullscreen session across every step.

Transfers show a fullscreen dashboard by default; `--no-tui` uses plain output.
The first Ctrl-C stops gracefully, and the second aborts immediately. Keep the
source files, drive configuration, and local records to resume with the same command.

### Direct commands and scripts

Complete arguments run a transfer directly. Paths are relative to the selected
drive's default root; `/` means that root. A batch upload destination ends in `/`.

```sh
wb put ./a.zip ./b.zip --to backup/
wb list backup/
wb get backup/a.zip ./a.zip
wb --drive work list /
wb --json put ./a.zip --to backup/
wb get backup/a.zip ./a.zip --no-tui
```

`--drive NAME` selects a drive; `--root ROOT` overrides its root
(a GDrive object ID or relative WebDAV directory).
`wb drive use NAME` changes the default. `wb drive remove NAME` keeps credentials
and recovery records.

An explicit account and full URI also work:

```sh
wb get 'gdrive://account@example.com/backup/a.zip' ./a.zip
wb get 'webdav://nas/backup/a.zip' ./a.zip
```

Full URIs start at the account root (Google root or WebDAV endpoint) unless
overridden with `--root`, and cannot be
combined with `--drive`. JSON output, `--no-tui`, and non-terminal sessions never
open a picker; missing required arguments produce an error. See
`wb <command> --help` for all options.

### Files and publication

Batch downloads require an existing local directory. Identical names from
different cloud folders are rejected before transferring. Google native documents
can be browsed but are not exported, and folders are not downloaded recursively.
Downloads publish on the same filesystem without overwriting existing files;
cross-filesystem copy publication is unsupported. Use `--conflict operation-suffix`
to keep another copy when a name conflicts.

## Rust integration

| Crate | Responsibility |
|---|---|
| [`waybill`](crates/waybill/) | Upload and download contracts, transfer engines, capabilities, and checkpoints |
| [`waybill-service-fs`](crates/waybill-service-fs/) | Stable local sources, file checkpoints, download staging, and publication |
| [`waybill-service-gdrive`](crates/waybill-service-gdrive/) | Google Drive protocol, upload reconciliation, ranged reads, and directory access |
| [`waybill-service-webdav`](crates/waybill-service-webdav/) | Directory access, ranged reads, whole-file streams, content reconciliation, and conditional MOVE |
| [`waybill-cli`](crates/waybill-cli/) | `wb`: authorization, drive configuration, interaction, and transfer orchestration |

```rust
let engine = waybill::TransferEngine::new(checkpoint_store);
let receipt = engine.upload(source.as_ref(), sink.as_ref(),
    waybill::UploadOptions::new("stable-operation-id", "backup/file.zip")).await?;
// Commit the receipt to your application's ledger before engine.confirm(&receipt).
```

The core is independent of backends and executors. Hosts own authorization and
credential refresh; services obtain valid credentials at request boundaries.
External services use the same public interfaces as the services in this workspace,
implementing uploads, downloads, and recovery according to their capabilities.
Use `engine.upload` for offset uploads or `engine.upload_stream` for whole-file
streams with an `Arc<dyn Source>`. Both share storage, budgets, and receipt handling.

Start with the [independent consumer example](examples/consumer/), or read the
[service guide](docs/SERVICE.zh-CN.md) and [design document](docs/DESIGN.zh-CN.md).
The detailed documentation is currently in Simplified Chinese.

## Contributing

Issues and pull requests are welcome. For recovery changes or new backends,
describe the use case, backend capabilities, and expected behavior after failure.
The [design document](docs/DESIGN.zh-CN.md) records architectural constraints.

The workspace uses Rust 2024. Before submitting a change, run:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Use dedicated files and folders for live backend tests. Keep credentials and
private session state out of contributions. Existing validation and remaining
test scenarios are recorded in the [validation notes](docs/VALIDATION.zh-CN.md).

## License

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
