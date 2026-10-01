//! 相对 service 根目录的对象路径约束，供宿主与传输意图共同校验。
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
