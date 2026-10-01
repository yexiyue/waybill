//! 命令行接口定义；帮助文案即对外承诺，不提及未实现的能力。
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// 运单命令行工具：可恢复的云端投递。
#[derive(Parser)]
#[command(name = "wb", version, arg_required_else_help = true)]
pub struct Cli {
    /// 输出机器可读的 JSON（每行一个事件或记录）。
    #[arg(long, global = true)]
    pub json: bool,
    /// 提高输出详细度；可叠加。
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
        #[arg(long, value_name = "FILE")]
        client: Option<PathBuf>,
        /// 不自动打开浏览器，仅打印授权 URL。
        #[arg(long)]
        no_browser: bool,
        /// 账户目录别名；缺省使用 Google 账户邮箱。
        #[arg(long)]
        account: Option<String>,
    },
    /// 投递文件到云端；重跑同一命令即续传或按回执幂等跳过。
    Put {
        /// 本地源文件；允许多个。
        #[arg(value_name = "SRC", required = true)]
        sources: Vec<PathBuf>,
        /// 目标 URI，如 gdrive://account@example.com/backup/
        #[arg(value_name = "DEST-URI")]
        dest: String,
        /// 覆盖默认操作 ID（默认由实例、目标与内容摘要推导）。
        #[arg(long, value_name = "ID")]
        operation: Option<String>,
        /// 同名目标冲突策略。
        #[arg(long, value_enum, default_value_t = Conflict::Reject)]
        conflict: Conflict,
        /// Drive 根目录对象 ID；缺省为 root。
        #[arg(long, value_name = "ID")]
        root: Option<String>,
        /// 禁用全屏面板，使用行式输出（非终端自动生效）。
        #[arg(long)]
        no_tui: bool,
    },
    /// 列出本机在途运单与可恢复的 checkpoint。
    Status,
}

/// 云服务提供方。
#[derive(Subcommand)]
pub enum Provider {
    /// Google Drive。
    Gdrive,
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
