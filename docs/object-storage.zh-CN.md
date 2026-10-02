# 对象存储接入与验收

更新日期：2026-10-02。实现为 `waybill-service-opendal`，使用 Apache OpenDAL 0.59.3。
架构与恢复语义以 [DESIGN §8.3](DESIGN.zh-CN.md#83-桥接策略定稿2026-10-02) 为准。

## CLI 配置

创建私有配置文件 `object.json`（权限建议 0600），凭证使用环境变量引用：

```json
{
  "backend": "s3",
  "namespace": "account-id/bucket-name",
  "conditional_writes": true,
  "options": {
    "bucket": "bucket-name",
    "region": "us-east-1",
    "endpoint": "https://s3.us-east-1.amazonaws.com",
    "access_key_id": "${OBJECT_ACCESS_KEY}",
    "secret_access_key": "${OBJECT_SECRET_KEY}"
  }
}
```

```sh
wb login --account cloud object --config object.json
wb drive add cloud --provider object --account cloud
wb --drive cloud list / --no-tui
wb --drive cloud put ./model.bin --to models/ --no-tui
wb --drive cloud get models/model.bin ./model.downloaded.bin --no-tui
wb list object://cloud/models/ --no-tui
```

也可通过 `--config -` 导入。`options` 的值全部为字符串，字段直接传给对应 OpenDAL
后端；只有整个值形如 `${ENV}` 时展开。登录保存引用，后续命令重新读取环境值。
`namespace` 标识实际账户，换账户须更换；端点、bucket、container、region、root 等也参与
恢复实例隔离。不要将凭证放进 namespace。登录只验证目录访问，不能当成上传验收。
CLI 状态默认沿用本机私有目录；`WAYBILL_STATE_DIR` 可指定独立的绝对路径，适合隔离测试。

OSS 示例替换 `backend` 和 `options`：

```json
{
  "backend": "oss",
  "namespace": "aliyun-account-id/bucket-name",
  "conditional_writes": true,
  "options": {
    "bucket": "bucket-name",
    "endpoint": "https://oss-cn-hangzhou.aliyuncs.com",
    "access_key_id": "${OSS_ACCESS_KEY_ID}",
    "access_key_secret": "${OSS_ACCESS_KEY_SECRET}"
  }
}
```

COS 对应 `backend: "cos"`，原生字段为 `bucket`、`endpoint`、`secret_id`、`secret_key`，
临时凭证还需对应后端的 token 字段。参考 [OSS 配置](https://opendal.apache.org/docs/rust/opendal/services/struct.Oss.html)、
[COS 配置](https://opendal.apache.org/docs/rust/opendal/services/struct.Cos.html) 和
[OpenDAL 服务目录](https://opendal.apache.org/services)。凭证刷新沿用后端 provider；
CLI 的静态 STS 引用需宿主在后续命令前更新环境，不能据此声称长时间进程的 STS 刷新已验收。

## 能力与边界

默认注册以下对象存储后端；这个列表不包括 OpenDAL 的文件系统、数据库等其他类别。
库使用者可设置 `default-features = false` 并选择所需 feature；也可自行构造 Operator。

| 后端 / feature | waybill 条件下载 | waybill 流式上传 |
|---|---|---|
| s3、oss、cos、tos、gcs、azblob | capability 满足后开放 | capability 满足且确认条件写入后开放 |
| obs、swift | capability 满足后开放 | 当前缺少所需条件创建能力，不开放 |
| b2、upyun、vercel-blob | 当前缺少所需条件读取能力，不开放 | 不开放 |

目录访问按原生支持情况提供。库的 `operator()` 暴露宿主原生 OpenDAL API，供应用直接
读取、写入、删除或签名等；其操作没有 waybill checkpoint、预算和完成回执保证。
以上为锁定版本的能力映射，不代替真实服务端验收。错误明确返回 Unsupported、Conflict、
SourceChanged、SessionExpired 或 ResultUnknown，不静默降级覆盖。

上传默认关闭。设置 `conditional_writes: true` 表示宿主已经确认当前服务端与 bucket
支持条件创建，特别是 OSS / COS 的防覆盖语义可能受 bucket 版本控制影响。
临时对象读回校验后，仅在可固定非 null 源版本时优先条件 copy 发布；否则重新上传完整内容到最终 key，
因此可能增加完整上传流量。最终内容校验后才返回完成回执。

写入中断只能显式整文件重传：`wb put ... --allow-restart`。OpenDAL 私有 multipart 会话
不进入 checkpoint；已经校验的暂存可单独重试发布。对象命名中 `.waybill-` 前缀保留给
暂存；支持条件删除的后端在成功后清理当前暂存。其他暂存、进程中断留下的旧对象和未完成
multipart 会话需设置 bucket lifecycle。不要把 ETag 当成通用内容哈希。

## Docker 验收

依赖 Docker 和 Python 3.9+，在仓库根目录运行：

```sh
cargo build -p waybill-cli --locked
python3 tests/object-storage/acceptance.py
```

脚本固定 RustFS 与 MinIO mc 的镜像摘要，生成一次性本地凭证、动态 loopback 端口、
独立 bucket 与状态目录；结束时删除本次容器及匿名卷，不修改其他 Docker 容器或登录账户。
可用 `WAYBILL_TEST_BIN` 指定待测 `wb` 可执行文件。

macOS / arm64，Docker 29.4，Rust 1.98.1 的本地 S3 兼容服务器验收覆盖：
17 MiB multipart 上传与下载校验、回执复用、目录和根目录、冲突与操作后缀、Unicode、
空文件，以及写入意图落盘后 SIGKILL、默认拒绝重传、显式重传 128 MiB 和下载 SHA-256。
HTTP 替身另外覆盖版本变化、忽略 Range、超大响应、预算不足、发布完成响应丢失、
条件 writer 发布降级与 checkpoint 身份绑定。

Docker 场景仅代表 S3 兼容服务器；另见下文真实 OSS 验收。尚未验证 COS、公有云 STS 轮换、版本控制桶、限流、
跨区域性能或生产吞吐。具体后端交付前仍需独立验收。

## 真实 OSS 验收与复测

2026-10-02 在深圳 `cn-shenzhen` 创建专用 bucket
`waybill-test-20261002-e00c45e2`：标准存储、LRS、私有访问、阻止公共访问开启，
版本控制从未开启。创建选项参照[阿里云文档](https://help.aliyun.com/zh/oss/user-guide/create-a-bucket-4)。
17 MiB multipart、下载 SHA-256、回执复用、根目录、Unicode / 空文件、同名拒绝与
操作后缀均通过；128 MiB 在 Writing checkpoint 落盘后 SIGKILL，默认拒绝重传，
显式 `--allow-restart` 后整文件重传成功。OSS 缺少条件 copy，实测发布走完整读回与
条件重上传；暂存完成后插入竞争对象，发布拒绝且竞争内容不变。

OpenDAL 0.59.3 的 OSS multipart 初始化没有发送用户元数据。适配器通过
`Content-Disposition` 的 `waybill-delivery` 扩展参数额外保存操作标记，修复暂存归属
误判；协议替身与真实 OSS 重测均通过。它不是文件摘要，内容仍需读回校验。
待对账期间不要移除此属性。

以下脚本需要一个新建、配置相同的专用测试 bucket 和未使用的绝对工作目录。
脚本不创建 bucket、不修改 RAM；同一目录中的完成 checkpoint 不适合再次注入中断。复测已有 bucket 时，可用
`--prefix scratch/<新运行名>/` 隔离本次样本，并使用新的工作目录；清理脚本会删除
该 scratch 下的对象，保留原 `samples/` 成功样本。
CSV 只在父进程中读取，凭证通过子进程环境传递；连接配置只保存环境变量引用。

```sh
cargo build -p waybill-cli --locked
python3 tests/object-storage/oss-acceptance.py \
  --credentials /absolute/path/AccessKey.csv \
  --bucket waybill-test-YYYYMMDD-1234abcd \
  --work-dir /absolute/path/oss-test
```

验收后使用[官方 ossutil 2](https://help.aliyun.com/zh/oss/developer-reference/ossutil-overview/)
清理本次测试的暂存、scratch 和未完成 multipart；脚本先核对 key 范围，不删除成功样本：

```sh
python3 tests/object-storage/oss-cleanup.py \
  --credentials /absolute/path/AccessKey.csv \
  --bucket waybill-test-YYYYMMDD-1234abcd \
  --work-dir /absolute/path/oss-test \
  --ossutil /absolute/path/ossutil
```

本次删除 7 个临时对象、取消 1 个未完成 multipart，清理后未完成会话为 0。
保留 bucket 和 `samples/` 下 4 个成功对象，详细结果与截图位置见
[真实 OSS 验收记录](VALIDATION.zh-CN.md#真实-oss-验收2026-10-02)。
