//! 子命令入口。
mod get;
mod list;
mod login;
mod put;
mod status;

use crate::{
    cli::{Cli, Command},
    error::CliError,
};

pub async fn dispatch(cli: Cli) -> Result<(), CliError> {
    let Cli {
        json,
        verbose,
        command,
    } = cli;
    match command {
        Command::Login {
            provider,
            client,
            no_browser,
            account,
        } => login::run(provider, client, no_browser, account, json).await,
        Command::Put(args) => put::run(args, json, verbose).await,
        Command::Get(args) => get::run(args, json, verbose).await,
        Command::List(args) => list::run(args, json).await,
        Command::Status => status::run(json).await,
    }
}
