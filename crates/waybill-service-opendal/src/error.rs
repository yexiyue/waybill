//! 原始协议错误保留为来源；默认输出只包含受控描述。
use waybill::error::{Error, ErrorKind};
pub(crate) fn map(error: opendal::Error) -> Error {
    let kind = match error.kind() {
        opendal::ErrorKind::Unsupported => ErrorKind::Unsupported,
        opendal::ErrorKind::ConfigInvalid
        | opendal::ErrorKind::IsADirectory
        | opendal::ErrorKind::NotADirectory
        | opendal::ErrorKind::RangeNotSatisfied => ErrorKind::InvalidInput,
        opendal::ErrorKind::NotFound => ErrorKind::NotFound,
        opendal::ErrorKind::PermissionDenied => ErrorKind::Authentication,
        opendal::ErrorKind::AlreadyExists | opendal::ErrorKind::Conflict => ErrorKind::Conflict,
        opendal::ErrorKind::ConditionNotMatch => ErrorKind::SourceChanged,
        opendal::ErrorKind::RateLimited => ErrorKind::Retryable,
        _ if error.is_temporary() => ErrorKind::Retryable,
        _ => ErrorKind::Protocol,
    };
    Error::new(kind, "object storage operation failed").with_source(error)
}
pub(crate) fn write(error: opendal::Error) -> Error {
    if error.kind() == opendal::ErrorKind::ConditionNotMatch {
        Error::new(ErrorKind::Conflict, "conditional object creation rejected").with_source(error)
    } else {
        map(error)
    }
}
