# crates.io 发布

统一版本以根 `Cargo.toml` 的 `workspace.package.version` 为准。发布 tag 为 `v<版本>`，
例如 `v0.1.0`；全部 6 个公开 crate 同步升级，示例的 `publish = false` 保持不变。

## 工作流

`.github/workflows/release.yml` 在 GitHub Release **published** 时执行，包括 prerelease。
先检验 tag、每个 crate 的版本、内部 path 依赖的版本约束及 `docs/releases/<tag>.md`，
再运行 fmt、check、Clippy、测试和完整发布 dry-run。只有全部通过才进入发布 job。
发布说明由对应版本的人工说明文件加精确 tag 范围 git log 生成；首次发布包含全部历史，
后续使用当前 tag 历史中最近的版本 tag。用户 Release 正文保留，自动区段重跑时替换。

发布通过 `cargo publish --workspace --locked --registry crates-io`；Cargo 1.90 起原生支持
多包依赖发布。本项目 Rust 下限 1.91，工作流用 stable；Cargo 处理依赖顺序和索引可见性：

1. `waybill` 核心。
2. `waybill-service-fs`。
3. `waybill-service-gdrive`、`waybill-service-webdav`、`waybill-service-opendal`。
4. `waybill-cli`（`wb`）。

service 的测试依赖也可能要求 fs 先完成，因此不要并行触发多个单 crate 发布工作流。
整个发布工作流使用固定并发组，前一次发布不会因新 Release 被取消。

## 凭证

仓库 `yexiyue/waybill` 的 Actions Secret 名为 `CARGO_REGISTRY_TOKEN`。
crates.io API token 只授予 `publish-new` / `publish-update`，限制到上述 6 个具体名称，
不授予 yank、所有者修改或其他项目权限。令牌仅注入实际 publish 步骤。
初始令牌采用 90 天有效期，过期前在 crates.io 轮换并更新同名 Secret；不要提交令牌或
执行 `cargo login` 把 CI 令牌保存在本机。2026-10-02 已创建上述范围的 90 天令牌并配置
Secret，页面显示到期日为 2026-12-31；令牌值未写入本机文件或仓库。

## 发版步骤

1. 更新 workspace 版本及 5 个内部 workspace 依赖的版本约束，刷新 Cargo.lock。
2. 编写 `docs/releases/v<版本>.md`，区分已实现、已验证和未覆盖边界。
3. 执行 `python3 scripts/releases/notes.py check v<版本>` 及工作区检查。
4. 执行 `cargo publish --workspace --dry-run --locked --registry crates-io`。
5. 提交并推送通过验证的改动，创建指向该提交的 `v<版本>` tag，发布对应 GitHub Release。
   先确认令牌 Secret 已配置；仅创建 draft 不会发布 crates。
6. 检查 Publish crates 工作流和 6 个 crates.io 版本，不把 GitHub Release 已创建当作发布成功。

本机若用 registry 镜像，首次发布的 workspace dry-run 可能无法在镜像解析尚未发布的
内部包。请用不含 source replacement 的独立 `CARGO_HOME` 验证官方 crates.io；不要
为了绕过错误关闭编译验证或修改用户全局 Cargo 配置。GitHub runner 使用官方 registry。

Cargo 发布不具备跨 crate 原子性。如果中途失败，先确认 crates.io 上已经可见的版本，
不要更换同一版本内容或 yank 成功包。失败于上传后等待索引时，上传可能已生效；
等待索引可见，再用相同 tag 的 checkout 对尚未发布的包执行 `cargo publish -p <name>`，
按依赖顺序补发。也可手动运行 Publish crates 工作流，输入原 release tag 和一个尚未发布的
crate；它会重新校验该 tag、执行检查和单包 dry-run，再只发布选定包。凭证仍仅由 GitHub
Secret 注入。若收到 crates.io 429 新包限流，等待错误中的允许重试时间后再补发。
不要直接重复整套发布以掩盖“版本已存在”的错误。

参考：[Cargo 发布命令](https://doc.rust-lang.org/cargo/commands/cargo-publish.html)、
[Cargo 1.90 workspace 发布](https://doc.rust-lang.org/cargo/CHANGELOG.html#cargo-190-2025-09-18)、
[API token 范围](https://rust-lang.github.io/rfcs/2947-crates-io-token-scopes.html)。
