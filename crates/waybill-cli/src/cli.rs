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
    /// 显示行式上传进度，即使标准错误不是终端。
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
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
    Put(PutArgs),
    /// 从云端取回文件到本地；重跑同一命令即续传或按回执幂等跳过。
    Get(GetArgs),
    /// 列出云盘目录内容。
    List(ListArgs),
    /// 列出本机恢复记录与已完成回执。
    Status,
}

/// 文件投递参数；由 clap 校验后交给命令编排。
#[derive(Args)]
pub struct PutArgs {
    /// 本地源文件；允许多个。
    #[arg(value_name = "SRC", required = true)]
    pub sources: Vec<PathBuf>,
    /// 目标 URI，如 gdrive://account@example.com/backup/
    #[arg(value_name = "DEST-URI")]
    pub dest: String,
    /// 覆盖默认操作 ID（默认由实例、目标与内容摘要推导）。
    #[arg(long, value_name = "ID")]
    pub operation: Option<String>,
    /// 同名目标冲突策略。
    #[arg(long, value_enum, default_value_t = Conflict::Reject)]
    pub conflict: Conflict,
    /// Drive 根目录对象 ID；缺省为 root。
    #[arg(long, value_name = "ID")]
    pub root: Option<String>,
    /// 禁用全屏面板，使用行式输出（非终端自动生效）。
    #[arg(long)]
    pub no_tui: bool,
}

/// 云服务提供方。
#[derive(Subcommand)]
pub enum Provider {
    /// Google Drive（读取云盘文件，创建和修改本应用文件）。
    Gdrive,
}

/// 文件取回参数；源为云盘路径，目标为本地文件或目录。
#[derive(Args)]
pub struct GetArgs {
    /// 源 URI，如 gdrive://account@example.com/backup/a.zip
    #[arg(value_name = "SRC-URI")]
    pub source: String,
    /// 本地目标文件路径；为已存在目录时使用远端文件名。
    #[arg(value_name = "DEST")]
    pub dest: PathBuf,
    /// 覆盖默认操作 ID（默认由实例、源版本与目标推导）。
    #[arg(long, value_name = "ID")]
    pub operation: Option<String>,
    /// 同名本地目标冲突策略。
    #[arg(long, value_enum, default_value_t = Conflict::Reject)]
    pub conflict: Conflict,
    /// Drive 根目录对象 ID；缺省为 root。
    #[arg(long, value_name = "ID")]
    pub root: Option<String>,
    /// 禁用全屏面板，使用行式输出（非终端自动生效）。
    #[arg(long)]
    pub no_tui: bool,
}

/// 目录列表参数。
#[derive(Args)]
pub struct ListArgs {
    /// 目录 URI，如 gdrive://account@example.com/backup/；根目录可省略路径。
    #[arg(value_name = "DIR-URI")]
    pub uri: String,
    /// Drive 根目录对象 ID；缺省为 root。
    #[arg(long, value_name = "ID")]
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
impl From<Conflict> for waybill::upload::ConflictPolicy {
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
        assert_eq!(args.sources, vec![PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(args.dest, "gdrive://bill/backup/");
        assert!(args.no_tui);
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
        assert_eq!(args.source, "gdrive://bill/backup/a.iso");
        assert_eq!(args.dest, PathBuf::from("./a.iso"));
        assert_eq!(args.conflict, Conflict::OperationSuffix);
        assert_eq!(args.root, None);
    }

    #[test]
    fn list_takes_a_directory_uri() {
        let cli = Cli::try_parse_from(["wb", "list", "gdrive://bill/backup/"]).unwrap();
        let Command::List(args) = cli.command else {
            panic!("expected list");
        };
        assert_eq!(args.uri, "gdrive://bill/backup/");
    }
}
