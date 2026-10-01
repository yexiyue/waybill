//! 一个命令拥有一个惰性全屏会话；各页面仅借用，不负责进入或恢复终端。
use crate::error::CliError;
use futures_util::StreamExt;
use std::io;
use {
    crossterm::{
        self,
        event::{Event, EventStream},
        terminal::{EnterAlternateScreen, LeaveAlternateScreen},
    },
    ratatui::{self, backend::CrosstermBackend},
};

#[derive(Default)]
pub(crate) struct Session {
    terminal: Option<ratatui::Terminal<CrosstermBackend<io::Stderr>>>,
    events: Option<EventStream>,
    entered: bool,
}
impl Session {
    pub fn draw(&mut self, draw: impl FnOnce(&mut ratatui::Frame)) -> Result<(), CliError> {
        if self.terminal.is_none() {
            crossterm::terminal::enable_raw_mode()?;
            self.entered = true;
            crossterm::execute!(io::stderr(), EnterAlternateScreen, crossterm::cursor::Hide)?;
            let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(io::stderr()))?;
            terminal.clear()?;
            self.terminal = Some(terminal);
            self.events = Some(EventStream::new());
        }
        if let Some(terminal) = &mut self.terminal {
            terminal.draw(draw)?;
        }
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Event, CliError> {
        let events = self
            .events
            .as_mut()
            .ok_or_else(|| CliError::Message("终端尚未初始化".into()))?;
        events
            .next()
            .await
            .ok_or_else(|| CliError::Message("终端输入已关闭".into()))?
            .map_err(Into::into)
    }
}
impl Session {
    pub fn close(&mut self) {
        if self.entered {
            self.events = None;
            self.terminal = None;
            let _ = crossterm::terminal::disable_raw_mode();
            let _ =
                crossterm::execute!(io::stderr(), LeaveAlternateScreen, crossterm::cursor::Show);
            self.entered = false;
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}
