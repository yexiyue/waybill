//! 子命令入口。
mod browse;
mod entry;
mod get;
mod list;
mod login;
mod put;
mod session;
pub(crate) mod status;

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
    let mut ui = crate::ui::Session::default();
    match command {
        Command::Login {
            provider,
            client,
            no_browser,
            account,
        } => login::run(provider, client, no_browser, account, json).await,
        Command::Put(args) => entry::put(&mut ui, args, drive.as_deref(), json, verbose).await,
        Command::Get(args) => entry::get(&mut ui, args, drive.as_deref(), json, verbose).await,
        Command::List(args) => entry::list(&mut ui, args, drive.as_deref(), json).await,
        Command::Drive { command } => entry::drive(&mut ui, command, json).await,
        Command::Status { no_tui } => status::run(&mut ui, json, no_tui).await,
    }
}
