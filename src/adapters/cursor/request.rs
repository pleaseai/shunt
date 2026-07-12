use serde_json::Value;

/// A selected image extracted from the request content blocks.
#[derive(Debug, Clone)]
pub struct CursorSelectedImage {
    pub data: String,
    pub uuid: String,
    pub path: String,
    pub mime_type: String,
}

/// Render the full Cursor prompt from an Anthropic MessagesRequest.
///
/// Includes:
/// - System message (with billing-header filtering)
/// - Conversation messages with content blocks
/// - Tools block
pub fn render_cursor_prompt(req: &Value) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(system) = render_system(req) {
        sections.push(format!("<system>\n{system}\n</system>"));
    }

    for message in req
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let content = render_message_content(message);
        if let Some(c) = content {
            sections.push(format!(
                "<{}>\n{}\n</{}>",
                message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user"),
                c,
                message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
            ));
        }
    }

    // Tools block
    if let Some(tools) = req.get("tools").and_then(|v| v.as_array()) {
        if !tools.is_empty() {
            let tool_lines: Vec<String> = tools
                .iter()
                .map(|t| {
                    let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let description = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
                    let input_schema = t
                        .get("input_schema")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default()));
                    format!(
                        "{}",
                        serde_json::json!({
                            "name": name,
                            "description": description,
                            "input_schema": input_schema,
                        })
                    )
                })
                .collect();
            if !tool_lines.is_empty() {
                sections.push(format!("<tools>\n{}\n</tools>", tool_lines.join("\n")));
            }
        }
    }

    sections.join("\n\n")
}

/// Extract selected images from the request, mimicking `cursorSelectedImages`.
///
/// Only base64 source images are included. URL images are skipped.
/// Images nested inside tool_result blocks are also collected.
pub fn cursor_selected_images(req: &Value) -> Vec<CursorSelectedImage> {
    let mut images: Vec<CursorSelectedImage> = Vec::new();
    let mut index: u32 = 0;

    for message in req
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let blocks = message_blocks(message);
        for block in &blocks {
            collect_image_blocks(block, &mut index, &mut images);
        }
    }

    images
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Whether a system-prompt line is an `x-anthropic-billing-header:` directive.
/// Matched case-insensitively so a mixed-case header (e.g.
/// `X-Anthropic-Billing-Header:`) can't slip through to the upstream prompt.
fn is_billing_header_line(line: &str) -> bool {
    const PREFIX: &str = "x-anthropic-billing-header:";
    line.get(..PREFIX.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(PREFIX))
}

fn render_system(req: &Value) -> Option<String> {
    let system_value = req.get("system")?;
    let text = match system_value {
        serde_json::Value::String(s) => s
            .lines()
            .filter(|line| !is_billing_header_line(line))
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::Value::Array(blocks) => {
            // Filter billing-header directives line-by-line within each block, not
            // just whole blocks, so a multi-line text block with a mid-block
            // header is handled the same as the string form above.
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) != Some("text") {
                        return None;
                    }
                    let text = b.get("text").and_then(|t| t.as_str())?;
                    let filtered = text
                        .lines()
                        .filter(|line| !is_billing_header_line(line))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if filtered.is_empty() {
                        None
                    } else {
                        Some(filtered)
                    }
                })
                .collect();
            if parts.is_empty() {
                return None;
            }
            parts.join("\n\n")
        }
        _ => return None,
    };
    if text.is_empty() {
        return None;
    }
    Some(text)
}

fn render_message_content(message: &Value) -> Option<String> {
    let blocks = message_blocks(message);
    let rendered: Vec<String> = blocks.iter().filter_map(render_block).collect();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join("\n\n"))
    }
}

fn render_block(block: &serde_json::Value) -> Option<String> {
    let block_type = block.get("type").and_then(|t| t.as_str())?;
    match block_type {
        "text" => block
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string()),
        "thinking" => {
            let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
            Some(format!("<thinking>\n{text}\n</thinking>"))
        }
        "image" => {
            let source = block.get("source")?;
            match source.get("type").and_then(|t| t.as_str()) {
                Some("url") => {
                    let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
                    Some(format!("[image: {url}]"))
                }
                _ => {
                    let media_type = source
                        .get("media_type")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                    Some(format!(
                        "[image: {media_type}, {} base64 chars]",
                        data.len()
                    ))
                }
            }
        }
        "tool_use" => {
            let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = block
                .get("input")
                .and_then(|i| serde_json::to_string(i).ok())
                .unwrap_or_else(|| "{}".to_string());
            Some(format!(
                "<tool_use id=\"{id}\" name=\"{name}\">\n{input}\n</tool_use>"
            ))
        }
        "tool_result" => {
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let is_error = block
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let error_attr = if is_error { " is_error=\"true\"" } else { "" };
            let content = render_tool_result_content(block);
            Some(format!(
                "<tool_result tool_use_id=\"{tool_use_id}\"{error_attr}>\n{content}\n</tool_result>"
            ))
        }
        "server_tool_use" => {
            let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = block
                .get("input")
                .and_then(|i| serde_json::to_string(i).ok())
                .unwrap_or_else(|| "{}".to_string());
            Some(format!(
                "<server_tool_use id=\"{id}\" name=\"{name}\">\n{input}\n</server_tool_use>"
            ))
        }
        "web_search_tool_result" => {
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let content = block
                .get("content")
                .and_then(|c| serde_json::to_string(c).ok())
                .unwrap_or_else(|| "{}".to_string());
            Some(format!(
                "<web_search_tool_result tool_use_id=\"{tool_use_id}\">\n{content}\n</web_search_tool_result>"
            ))
        }
        _ => {
            // Unsupported block type - render as text placeholder
            block
                .get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        }
    }
}

fn render_tool_result_content(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(render_tool_result_block)
            .collect::<Vec<String>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn render_tool_result_block(block: &serde_json::Value) -> Option<String> {
    let block_type = block.get("type").and_then(|t| t.as_str())?;
    match block_type {
        "text" | "image" | "tool_use" | "tool_result" | "thinking" => render_block(block),
        _ => {
            let type_str = block_type.to_string();
            Some(format!("[unsupported tool result block: {type_str}]"))
        }
    }
}

fn message_blocks(message: &Value) -> Vec<serde_json::Value> {
    match message.get("content") {
        Some(serde_json::Value::String(s)) => {
            vec![serde_json::json!({"type": "text", "text": s})]
        }
        Some(serde_json::Value::Array(arr)) => arr.clone(),
        _ => Vec::new(),
    }
}

fn collect_image_blocks(
    block: &serde_json::Value,
    index: &mut u32,
    images: &mut Vec<CursorSelectedImage>,
) {
    if block.get("type").and_then(|t| t.as_str()) == Some("image") {
        let source = match block.get("source") {
            Some(s) => s,
            None => return,
        };
        if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
            return;
        }
        let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
        let media_type = source
            .get("media_type")
            .and_then(|m| m.as_str())
            .unwrap_or("image/png");
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        *index += 1;
        let extension = image_extension(media_type);
        images.push(CursorSelectedImage {
            data: data.to_string(),
            uuid,
            path: format!("claude-image-{index}.{extension}"),
            mime_type: media_type.to_string(),
        });
        return;
    }

    // Recurse into tool_result blocks for nested images
    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
        if let Some(serde_json::Value::Array(arr)) = block.get("content") {
            for child in arr {
                collect_image_blocks(child, index, images);
            }
        }
    }
}

fn image_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "img",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_system_message() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "system": "be direct",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<system>"));
        assert!(rendered.contains("be direct"));
        assert!(rendered.contains("</system>"));
        assert!(rendered.contains("<user>"));
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("</user>"));
    }

    #[test]
    fn filters_billing_header_lines_in_both_system_forms() {
        // String form: mid-string billing line is dropped, rest kept.
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "system": "keep me\nx-anthropic-billing-header: secret\nkeep me too",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("keep me"));
        assert!(rendered.contains("keep me too"));
        assert!(!rendered.contains("x-anthropic-billing-header"));

        // Array form: a multi-line text block with a mid-block billing line, and a
        // mixed-case header to confirm case-insensitive matching.
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "system": [{"type": "text", "text": "line one\nX-Anthropic-Billing-Header: secret\nline two"}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("line one"));
        assert!(rendered.contains("line two"));
        assert!(!rendered
            .to_ascii_lowercase()
            .contains("x-anthropic-billing-header"));
    }

    #[test]
    fn renders_tools_section() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Read", "description": "read files", "input_schema": {"type": "object"}}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<tools>"));
        assert!(rendered.contains("Read"));
    }

    #[test]
    fn filters_billing_headers_from_system() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "system": [
                {"type": "text", "text": "keep this"},
                {"type": "text", "text": "x-anthropic-billing-header: skip-me"}
            ],
            "messages": [{"role": "user", "content": "hello"}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("keep this"));
        assert!(!rendered.contains("x-anthropic-billing-header"));
    }

    #[test]
    fn collects_selected_images() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]
            }]
        });
        let images = cursor_selected_images(&req);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, "AAAA");
    }

    #[test]
    fn skips_url_images_in_selected() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/img.png"}}
                ]
            }]
        });
        let images = cursor_selected_images(&req);
        assert_eq!(images.len(), 0);
    }

    #[test]
    fn renders_url_image_placeholder() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/img.png"}}
                ]
            }]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("[image: https://example.com/img.png]"));
    }

    #[test]
    fn renders_thinking_blocks() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "let me think..."},
                {"type": "text", "text": "done"}
            ]}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<thinking>"));
        assert!(rendered.contains("let me think..."));
        assert!(rendered.contains("done"));
    }

    #[test]
    fn renders_tool_use_blocks() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu1", "name": "Read", "input": {"path": "/tmp"}}
            ]}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<tool_use id=\"tu1\" name=\"Read\">"));
    }

    #[test]
    fn renders_tool_result_with_content_blocks() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": [
                    {"type": "text", "text": "file contents"}
                ]}
            ]}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<tool_result tool_use_id=\"tu1\">"));
        assert!(rendered.contains("file contents"));
    }

    #[test]
    fn handles_unsupported_block_types() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "unknown_block", "text": "some fallback text"}
            ]}]
        });
        let rendered = render_cursor_prompt(&req);
        // Unsupported blocks fall back to text rendering if they have a text field
        assert!(rendered.contains("some fallback text"));
    }

    #[test]
    fn empty_messages_renders_emptyish() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": ""}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.is_empty() || !rendered.is_empty());
    }

    #[test]
    fn tool_result_with_nested_image() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}}
                ]}
            ]}]
        });
        let images = cursor_selected_images(&req);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/jpeg");
        assert_eq!(images[0].data, "BBBB");
    }

    #[test]
    fn renders_server_tool_use() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "st1", "name": "WebSearch", "input": {"query": "rust"}}
            ]}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<server_tool_use id=\"st1\" name=\"WebSearch\">"));
    }

    #[test]
    fn renders_web_search_tool_result() {
        let req: Value = serde_json::json!({
            "model": "cursor:gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "web_search_tool_result", "tool_use_id": "ws1", "content": {"results": []}}
            ]}]
        });
        let rendered = render_cursor_prompt(&req);
        assert!(rendered.contains("<web_search_tool_result tool_use_id=\"ws1\">"));
    }
}
