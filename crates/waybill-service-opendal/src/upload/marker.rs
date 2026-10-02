//! 操作归属标记与 OSS multipart 元数据兼容；所有属性均在签名前设置。
use crate::error;
use opendal::{Capability, Metadata, Operator, Writer};
use waybill::error::Result;

const MARKER: &str = "waybill-delivery";

pub(crate) fn supported(operator: &Operator, capabilities: Capability) -> bool {
    capabilities.write_with_user_metadata
        && (operator.info().scheme() != "oss" || capabilities.write_with_content_disposition)
}

pub(super) async fn writer(
    operator: &Operator,
    key: &str,
    marker: String,
    chunk: usize,
) -> Result<Writer> {
    let mut writer = operator
        .writer_with(key)
        .if_not_exists(true)
        .concurrent(1)
        .chunk(chunk);
    // OpenDAL 0.59.3 OSS multipart 初始化遗漏 user_metadata，但会传递 Content-Disposition。
    // 合法扩展参数补充同一标记，不修改签名后的 HTTP 头。
    if operator.info().scheme() == "oss" {
        writer = writer.content_disposition(&disposition(&marker));
    }
    writer
        .user_metadata([(MARKER.into(), marker)])
        .await
        .map_err(error::write)
}

pub(super) fn owned(operator: &Operator, metadata: &Metadata, marker: &str) -> bool {
    !metadata.is_dir()
        && (metadata
            .user_metadata()
            .and_then(|values| values.get(MARKER))
            == Some(marker)
            || (operator.info().scheme() == "oss"
                && metadata.content_disposition() == Some(disposition(marker).as_str())))
}

fn disposition(marker: &str) -> String {
    format!("inline; waybill-delivery=\"{marker}\"")
}
