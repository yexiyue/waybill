# WebDAV 验收记录

验证日期：2026-10-02。环境：macOS 原生 Rust 客户端、Docker Desktop，服务器仅
绑定本机回环地址。架构和恢复语义见 [设计文档 §5](DESIGN.zh-CN.md#5-设计方案)，
本文记录实际验证范围，不扩展为所有 WebDAV 产品的保证。

## 服务器矩阵

| 服务器 | 端口 | 认证 | 浏览和下载 | 整文件上传与发布 | 中断后显式重传 |
|---|---:|---|---|---|---|
| Apache httpd 2.4.65 / mod_dav_fs | 18765 | Basic、Digest | 通过 | 通过 | 通过 |
| WsgiDAV 4.3.5 / Cheroot 11.1.2 | 18766 | Basic、Digest | 通过 | 通过 | 通过 |

两套服务器均验证了空文件、中文及百分号文件名、嵌套目录创建、条件 PUT、
禁止覆盖的 MOVE、同名冲突、稳定操作后缀、重复执行对账和命名根目录隔离。
上传回执中的 BLAKE3 来自对远端内容的独立读取与源摘要比对，不来自客户端
提交的自定义属性。下载缺少服务端内容摘要时，回执仅声明长度校验。
范围下载另验证了暂停后重建引擎和源、从持久账本恢复；读取记录确认引擎
不重新请求已记账区间，最终文件与源字节一致。

协议故障替身另外覆盖：PUT / PROPPATCH / MOVE 已生效但返回失败、最终对象
缺少当前操作标记、条件版本冲突、自定义属性不可用、错误范围响应、源版本
变化、重定向、有界 XML 和响应读取。响应丢失测试核对实际 PUT、PROPPATCH、
MOVE 次数，恢复不重复发送已验证的整文件。这些是故障注入结果，不能称为
真实服务器上的故障注入验收。

## 架构审查后的回归范围

普通 HTTP 请求默认 60 秒总时限，整文件 PUT 默认一小时，可由宿主分别调整。
本机 HTTP 测试验证响应头等待、持续滴流正文的总截止和慢请求体的 PUT 独立
时限；上传仍按原有持久化尝试 → 内容核验 → 条件发布顺序执行。
元数据统一为 1 MiB / 1000 个子项；无父项响应同样不能超过子项预算。
私有上传状态改为带条件版本的阶段枚举，格式为 v2，不迁移旧格式。

## 云端应用账户验收（2026-10-02）

本次只记录用户提供的两个应用账户，不把结果外推到产品的所有权限或版本。
凭证仅保存到本机私有账户目录，不写入本记录；默认盘未改变。

| 命名盘 | 端点与根目录 | 登录 / 浏览 | CLI 下载 | CLI 上传 |
|---|---|---|---|---|
| `jianguoyun` | `https://dav.jianguoyun.com/dav/`，根 `我的坚果云/` | 通过 | 当前不兼容 | 当前不兼容 |
| `alipan` | `https://openapi.alipan.com/dav/`，根 `/` | 通过 | 当前不兼容 | 服务端拒绝写入 |

坚果云 `/dav/` 是同步文件夹入口；在其下创建测试目录返回 409，进入
`我的坚果云/` 后临时 PUT 成功，独立 HTTP GET 的 6144 字节与本地源一致。
但 HEAD / PROPFIND 没有可用强 ETag，GET 的 ETag 未加引号，错误的
`If-Match` 返回 206 而非拒绝；PROPPATCH 返回 207 / 属性状态 200 后自定义
属性仍读不回来。`wb put / get` 因恢复版本要求拒绝，未产生完成回执。
本次专用测试目录已经删除，不改变原有文件。

阿里云盘的 Depth 1 响应只含子项。客户端现改为在缺少父项时另做 Depth 0
校验，仍要求父目录存在、子项属于直接层级且引用不越界；实际目录浏览通过。
本次凭证的 MKCOL 和 PUT 均返回 403，OPTIONS 仅声明 OPTIONS / PROPFIND。
文件 HEAD 返回弱 ETag；原始 GET 返回跨域 302，重定向后的对象存储响应才有
强 ETag 和范围数据。不能把该对象存储响应当成原始 WebDAV 响应的保证；
当前客户端不跟随跨域跳转，CLI 下载未通过，也未完成远端恢复验收。

两个命名盘的 PTY 浏览与取消退出已验证：各一次全屏进入 / 退出，终端
termios 恢复；取消返回 130。此结果只证明浏览交互，不证明传输兼容。

云端传输限制需要单独的兼容策略，不能通过默认放宽版本、条件请求或
发布归属校验来声称支持。上述目录解析修复另有 HTTP 替身回归测试。

## 复现

在仓库根目录执行。测试使用固定的非生产账户 `tester` / `fixture-password`；
Apache 和 Python 基础镜像固定到 compose / Dockerfile 中的 digest。
WsgiDAV 使用持久属性存储，以免服务器重启丢失操作标记。

```sh
docker compose -p waybill-webdav-test -f tests/webdav/compose.yml up -d --build
cargo test -p waybill-service-webdav
cargo test -p waybill-service-webdav docker_ -- --ignored
WAYBILL_WEBDAV_TEST_PORT=18766 cargo test -p waybill-service-webdav docker_ -- --ignored
```

服务端就绪后再运行测试；每次创建独立目录，测试互不覆盖。可用 HEAD 的 401
响应确认服务已监听。测试目录留在容器内部，不挂载用户文件。清理此测试栈：

```sh
docker compose -p waybill-webdav-test -f tests/webdav/compose.yml down
```

CLI 的 Apache Basic / Digest 登录、列表、上传和下载也已实际运行，下载内容
与测试源核对一致。WebDAV 命名盘的 PTY 多选上传和下载均只有一次全屏进入 /
退出，termios 恢复且两文件字节核对一致；没有改动用户默认盘。
命令形式如下（密码通过交互或 stdin 输入）：

```sh
wb login --account local-dav webdav \
  --endpoint http://127.0.0.1:18765/basic/ --username tester
wb drive add local-dav --provider webdav --account local-dav
wb list --drive local-dav /
wb put --drive local-dav ./example.bin --to uploads/ --no-tui
wb get --drive local-dav uploads/example.bin ./received.bin --no-tui
```

中断的普通 PUT 需要 `wb put ... --allow-restart` 才能重传整文件；已验证的暂存
只重试发布，无须该选项。测试账户及命名盘与用户现有 GDrive 配置分开保存。

## 已观察的差异与边界

- Apache 新写入文件短暂返回弱 ETag。上传对账最多等待 1.5 秒取得强版本；
  永久弱 ETag 仍拒绝恢复。空文件 HEAD 可省略长度，须以同一强版本的属性补足。
- WsgiDAV 的 PROPFIND ETag 属性省略引号，HTTP ETag 有引号。仅规范化属性
  表示，并要求它与 HEAD 的强版本一致。未授权 HEAD 在本次配置下发送额外
  正文；认证挑战使用关闭连接，避免污染连接池。
- 认证、内容与自定义属性权限都由服务端决定。缺少精确范围读取、强 ETag、
  持久操作属性或条件 MOVE 时，不能承诺本实现的上传恢复保证。
- 普通 WebDAV PUT 没有偏移续传；Nextcloud 分块扩展、NAS、反向代理、TLS
  部署及云端限流环境未在此矩阵中验收。MOVE 的服务器持久性不能外推为
  跨后端统一原子发布或断电保证。

服务器配置依据 [WsgiDAV 4.3.5 默认配置](https://github.com/mar10/wsgidav/blob/v4.3.5/wsgidav/default_conf.py)
和 [持久属性管理器](https://github.com/mar10/wsgidav/blob/v4.3.5/wsgidav/prop_man/property_manager.py)。
