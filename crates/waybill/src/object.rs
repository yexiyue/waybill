//! 引用的公共边界与可选的相对对象路径规则。
/// 目标路径：段非空且不含 `.`、`..`、反斜杠或控制字符。
pub fn valid_object_path(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 4096
        && target.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.len() <= 255
                && !segment.chars().any(|c| c.is_control() || c == '\\')
        })
}

/// 不透明对象引用：非空、最长 4096 字节、不含控制字符。
/// 具体引用语法由 service 解释，不在核心强制套用路径分段规则。
pub fn valid_reference(reference: &str) -> bool {
    !reference.is_empty() && reference.len() <= 4096 && !reference.chars().any(char::is_control)
}
