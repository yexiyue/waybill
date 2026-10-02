//! 命令行接口定义；帮助文案即对外承诺，不提及未实现的能力。
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// 运单命令行工具：可恢复的云端投递。
#[derive(Parser)]
#[command(name = "wb", version, arg_required_else_help = true)]
pub struct Cli {
    /// 输出机器可读的 JSON（每行一个事件或记录）。
    #[arg(long, global = true)]
    pub json: bool,
    /// 显示行式传输进度，即使标准错误不是终端。
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
    /// 使用配置盘；缺省使用默认盘。
    #[arg(long, global = true, value_name = "NAME")]
    pub drive: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// 登录云服务，凭证保存在本机私有目录。
    Login {
        #[command(subcommand)]
        provider: Provider,
        /// Google OAuth 桌面应用凭证（desktop.json）路径；
        /// 缺省读取环境变量 WAYBILL_GDRIVE_CLIENT。
        #[arg(long, global = true, value_name = "FILE")]
        client: Option<PathBuf>,
        /// 不自动打开浏览器，仅打印授权 URL。
        #[arg(long, global = true)]
        no_browser: bool,
        /// 账户目录别名；缺省使用 Google 账户邮箱。
        #[arg(long, global = true)]
        account: Option<String>,
    },
    /// 投递文件到云端；重跑同一命令即续传或按回执幂等跳过。
    Put(PutInput),
    /// 从云端取回文件到本地；重跑同一命令即续传或按回执幂等跳过。
    Get(GetInput),
    /// 列出云盘目录内容。
    List(ListInput),
    /// 列出本机恢复记录与已完成回执。
    Status {
        /// 禁用全屏面板，使用行式输出。
        #[arg(long)]
        no_tui: bool,
    },
    /// 配置盘名称、账户和默认根目录。
    Drive {
        #[command(subcommand)]
        command: DriveCommand,
    },
}

#[derive(Args)]
pub struct PutInput {
    /// 允许过期会话或中断的流式上传从头重传整文件。
    #[arg(long)]
    pub allow_restart: bool,
    /// 本地文件；省略时多选。末尾完整 URI 仍可用作目标。
    #[arg(value_name = "SRC")]
    pub sources: Vec<PathBuf>,
    /// 默认盘下的目标路径，目录以 / 结尾；省略时选择目录。
    #[arg(long = "to", value_name = "PATH")]
    pub dest: Option<String>,
    #[command(flatten)]
    pub transfer: TransferArgs,
}
#[derive(Args)]
pub struct GetInput {
    /// 默认盘下的文件路径或完整 URI；省略时云端多选。
    pub source: Option<String>,
    /// 本地文件或目录；省略时选择本地目录。
    pub dest: Option<PathBuf>,
    /// 本地目标；便于省略源路径时直接进入云端多选。
    #[arg(long, conflicts_with = "dest")]
    pub into: Option<PathBuf>,
    #[command(flatten)]
    pub transfer: TransferArgs,
}
#[derive(Args)]
pub struct ListInput {
    /// 默认盘下的目录路径或完整 URI；省略时浏览云盘。
    pub path: Option<String>,
    /// 临时根目录：GDrive 对象 ID 或 WebDAV / 对象存储相对目录路径。
    #[arg(long, value_name = "ROOT")]
    pub root: Option<String>,
    /// 禁用交互及全屏传输面板。
    #[arg(long)]
    pub no_tui: bool,
}
#[derive(Args)]
pub struct TransferArgs {
    /// 固定操作 ID，仅支持单个文件。
    #[arg(long, value_name = "ID")]
    pub operation: Option<String>,
    /// 同名目标的处理策略。
    #[arg(long, value_enum, default_value_t = Conflict::Reject)]
    pub conflict: Conflict,
    /// 临时根目录：GDrive 对象 ID 或 WebDAV / 对象存储相对目录路径。
    #[arg(long, value_name = "ROOT")]
    pub root: Option<String>,
    /// 禁用交互及全屏传输面板。
    #[arg(long)]
    pub no_tui: bool,
}
#[derive(Subcommand)]
pub enum DriveCommand {
    /// 添加或更新盘；第一个盘自动成为默认盘。
    Add {
        name: String,
        /// 使用的云服务。
        #[arg(long, value_enum, default_value_t = crate::drives::ProviderKind::Gdrive)]
        provider: crate::drives::ProviderKind,
        #[arg(long)]
        account: String,
        #[arg(long)]
        root: Option<String>,
        #[arg(long)]
        default: bool,
    },
    /// 设置默认盘。
    Use { name: String },
    /// 设置默认根目录；省略引用时进入云端目录选择器。
    Root { name: String, root: Option<String> },
    /// 删除盘配置，保留登录凭证与恢复记录。
    Remove { name: String },
    /// 显示已配置的盘。
    List,
}

/// 文件投递参数；由 clap 校验后交给命令编排。
pub struct PutArgs {
    /// 显式允许整文件重传。
    pub allow_restart: bool,
    /// 本地源文件；允许多个。
    pub sources: Vec<PathBuf>,
    /// 目标 URI，如 gdrive://account@example.com/backup/
    pub dest: String,
    /// 覆盖默认操作 ID（默认由实例、目标与内容摘要推导）。
    pub operation: Option<String>,
    /// 同名目标冲突策略。
    pub conflict: Conflict,
    /// 根目录：GDrive 缺省为 root，WebDAV / 对象存储缺省为 /。
    pub root: Option<String>,
    /// 禁用全屏面板，使用行式输出（非终端自动生效）。
    pub no_tui: bool,
}

/// 云服务提供方。
#[derive(Subcommand)]
pub enum Provider {
    /// Google Drive（读取云盘文件，创建和修改本应用文件）。
    Gdrive,
    /// 对象存储：导入 OpenDAL 后端配置（S3 / OSS / COS 等）。
    Object {
        /// JSON 配置文件；使用 - 从 stdin 读取，凭证可引用 ${ENV_VAR}。
        #[arg(long, value_name = "FILE")]
        config: PathBuf,
    },
    /// WebDAV：使用 Basic / Digest 或匿名认证。
    Webdav {
        /// 服务端根目录 URL，不包含密码。
        #[arg(long)]
        endpoint: String,
        /// 实际用户名；匿名认证可省略。
        #[arg(long)]
        username: Option<String>,
        #[arg(long, value_enum, default_value_t = WebdavAuth::Basic)]
        auth: WebdavAuth,
        /// 从 stdin 读取密码；省略时在终端隐藏输入。
        #[arg(long)]
        password_stdin: bool,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebdavAuth {
    Basic,
    Digest,
    Anonymous,
}

/// 文件取回参数；源为云盘路径，目标为本地文件或目录。
pub struct GetArgs {
    /// 源 URI，如 gdrive://account@example.com/backup/a.zip
    pub source: String,
    /// 本地目标文件路径；为已存在目录时使用远端文件名。
    pub dest: PathBuf,
    /// 覆盖默认操作 ID（默认由源和目标实例、源版本与绝对路径推导）。
    pub operation: Option<String>,
    /// 同名本地目标冲突策略。
    pub conflict: Conflict,
    /// 根目录：GDrive 缺省为 root，WebDAV / 对象存储缺省为 /。
    pub root: Option<String>,
    /// 禁用全屏面板，使用行式输出（非终端自动生效）。
    pub no_tui: bool,
}

/// 目录列表参数。
pub struct ListArgs {
    /// 目录 URI，如 gdrive://account@example.com/backup/；根目录可省略路径。
    pub uri: String,
    /// 根目录：GDrive 缺省为 root，WebDAV / 对象存储缺省为 /。
    pub root: Option<String>,
}

/// 同名目标冲突策略，映射库的 ConflictPolicy。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Conflict {
    /// 默认拒绝同名目标。
    Reject,
    /// 冲突时追加稳定操作后缀，不覆盖其他对象。
    OperationSuffix,
}
impl From<Conflict> for waybill::transfer::ConflictPolicy {
    fn from(value: Conflict) -> Self {
        match value {
            Conflict::Reject => Self::Reject,
            Conflict::OperationSuffix => Self::OperationSuffix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_commands_parse_for_interactive_completion() {
        for command in ["list", "get", "put"] {
            assert!(Cli::try_parse_from(["wb", command]).is_ok());
        }
        let cli =
            Cli::try_parse_from(["wb", "--drive", "work", "put", "a", "b", "--to", "backup/"])
                .unwrap();
        assert_eq!(cli.drive.as_deref(), Some("work"));
        let Command::Put(input) = cli.command else {
            panic!("put");
        };
        assert_eq!(input.sources.len(), 2);
        assert_eq!(input.dest.as_deref(), Some("backup/"));
    }

    #[test]
    fn download_destination_can_be_given_without_a_source() {
        let cli = Cli::try_parse_from(["wb", "get", "--into", "downloads"]).unwrap();
        let Command::Get(input) = cli.command else {
            panic!("get");
        };
        assert!(input.source.is_none());
        assert_eq!(input.into, Some(PathBuf::from("downloads")));
        assert!(Cli::try_parse_from(["wb", "get", "a", "b", "--into", "c"]).is_err());
        assert!(Cli::try_parse_from(["wb", "drive", "root", "personal"]).is_ok());
    }

    #[test]
    fn login_accepts_options_on_either_side_of_provider() {
        for args in [
            vec![
                "wb",
                "login",
                "--client",
                "desktop.json",
                "--account",
                "bill",
                "--no-browser",
                "gdrive",
            ],
            vec![
                "wb",
                "login",
                "gdrive",
                "--client",
                "desktop.json",
                "--account",
                "bill",
                "--no-browser",
            ],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(
                matches!(cli.command, Command::Login { provider: Provider::Gdrive, client: Some(path), account: Some(account), no_browser: true } if path == std::path::Path::new("desktop.json") && account == "bill")
            );
        }
    }

    #[test]
    fn put_reserves_last_positional_for_destination() {
        let cli = Cli::try_parse_from(["wb", "put", "a", "b", "gdrive://bill/backup/", "--no-tui"])
            .unwrap();
        let Command::Put(args) = cli.command else {
            panic!("expected put");
        };
        assert_eq!(
            args.sources,
            vec![
                PathBuf::from("a"),
                PathBuf::from("b"),
                PathBuf::from("gdrive://bill/backup/")
            ]
        );
        assert!(args.transfer.no_tui);
    }

    #[test]
    fn get_takes_source_uri_then_local_destination() {
        let cli = Cli::try_parse_from([
            "wb",
            "get",
            "gdrive://bill/backup/a.iso",
            "./a.iso",
            "--conflict",
            "operation-suffix",
        ])
        .unwrap();
        let Command::Get(args) = cli.command else {
            panic!("expected get");
        };
        assert_eq!(args.source.as_deref(), Some("gdrive://bill/backup/a.iso"));
        assert_eq!(args.dest, Some(PathBuf::from("./a.iso")));
        assert_eq!(args.transfer.conflict, Conflict::OperationSuffix);
        assert_eq!(args.transfer.root, None);
    }

    #[test]
    fn list_takes_a_directory_uri() {
        let cli = Cli::try_parse_from(["wb", "list", "gdrive://bill/backup/"]).unwrap();
        let Command::List(args) = cli.command else {
            panic!("expected list");
        };
        assert_eq!(args.path.as_deref(), Some("gdrive://bill/backup/"));
    }
}
