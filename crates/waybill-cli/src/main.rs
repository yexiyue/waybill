//! `wb` —— waybill 运单命令行宿主。
//!
//! OAuth 授权、凭证存储与业务记账属于宿主，这里只通过库的公开端口组装；
//! 核心与 service 不反向依赖本 crate。凭证与会话细节不进入输出。
mod cli;
mod cloud_host;
mod cmd;
mod credentials;
mod drives;
mod error;
mod gdrive_host;
mod oauth;
mod paths;
mod transfer;
mod ui;
mod uri;
mod webdav_host;

use clap::Parser;
use cli::Cli;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(error) = cmd::dispatch(cli).await {
        eprintln!("wb: {error}");
        return ExitCode::from(error::exit_code(&error));
    }
    ExitCode::SUCCESS
}
