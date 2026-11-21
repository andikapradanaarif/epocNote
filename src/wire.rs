use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;

pub(crate) const RESULT_BUDGET: usize = 48 * 1024;

pub(crate) fn success(value: &impl Serialize) -> Result<CallToolResult, serde_json::Error> {
    let value = serde_json::to_value(value)?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(value.to_string())]);
    result.structured_content = Some(value);
    Ok(result)
}
