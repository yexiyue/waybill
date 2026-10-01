//! 子命令入口。
mod browse;
mod entry;
mod get;
mod list;
mod login;
mod put;
mod session;
mod status;

use crate::{
    cli::{Cli, Command},
    error::CliError,
};

pub async fn dispatch(cli: Cli) -> Result<(), CliError> {
    let Cli {
        json,
        verbose,
        drive,
        command,
    } = cli;
    match command {
        Command::Login {
            provider,
            client,
            no_browser,
            account,
        } => login::run(provider, client, no_browser, account, json).await,
        Command::Put(args) => entry::put(args, drive.as_deref(), json, verbose).await,
        Command::Get(args) => entry::get(args, drive.as_deref(), json, verbose).await,
        Command::List(args) => entry::list(args, drive.as_deref(), json).await,
        Command::Drive { command } => entry::drive(command, json).await,
        Command::Status => status::run(json).await,
    }
}
