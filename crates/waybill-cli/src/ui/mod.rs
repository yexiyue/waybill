//! 事件流的输出形态与共享渲染工具。
pub(crate) mod json;
pub(crate) mod picker;
pub(crate) mod plain;
pub(crate) mod status;
mod terminal;
pub(crate) mod tui;
mod widgets;
pub(crate) use terminal::Session;

/// 全屏输出需要输入、输出和错误流都连接终端。
pub(crate) fn interactive_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}

/// 人类可读的字节数；二进制单位，与库的块尺寸约定一致。
pub(crate) fn human_bytes(value: u64) -> String {
    const UNIT: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNIT.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        let name = UNIT[unit];
        format!("{size:.1} {name}")
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn formats_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(8 * 1024 * 1024), "8.0 MiB");
    }
}
