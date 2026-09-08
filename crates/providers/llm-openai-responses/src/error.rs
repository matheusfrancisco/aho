use aho_llm::LlmError;
use aho_types::Value;

/// Bodies are inspected for classification but never included in diagnostics: OAuth gateways
/// can echo credential-bearing request details, even in nominally human-readable errors.
pub fn classify(status: Option<u16>, retry_after_ms: Option<u64>, body: &str, has_images: bool, route: &str) -> LlmError {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = if parsed["response"]["error"].is_object() {
        &parsed["response"]["error"]
    } else if parsed["error"].is_object() {
        &parsed["error"]
    } else {
        &parsed
    };
    let code = error["code"].as_str().unwrap_or("");
    let kind = error["type"].as_str().unwrap_or("");
    let message = error["message"].as_str().unwrap_or("").to_ascii_lowercase();
    let at =
        status.map_or_else(|| format!("route `{route}` reported an error mid-stream"), |s| format!("route `{route}` answered HTTP {s}"));
    if code == "context_length_exceeded"
        || code == "context_window_exceeded"
        || message.contains("maximum context length")
        || message.contains("exceeds the context window")
        || message.contains("input is too long")
    {
        return LlmError::ContextWindowExceeded { requested: number(error, "n_prompt_tokens"), available: number(error, "n_ctx") };
    }
    if (code == "max_output_tokens" || message.contains("max_output_tokens"))
        && (message.contains("too large") || message.contains("maximum") || message.contains("supports at most"))
    {
        return LlmError::MaxOutputTokens;
    }
    if status == Some(401)
        || status == Some(403)
        || matches!(code, "invalid_api_key" | "authentication_error" | "token_expired")
        || kind == "authentication_error"
    {
        return LlmError::Auth(format!("{at}; sign in again with `aho login {route}` or set `provider.{route}.key`"));
    }
    if code == "insufficient_quota" || kind == "insufficient_quota" {
        return LlmError::Fatal(format!("{at}; the account quota is exhausted; add credit or switch providers"));
    }
    if status == Some(429) || matches!(code, "rate_limit_exceeded" | "usage_limit_reached") || kind == "rate_limit_error" {
        return LlmError::RateLimited { retry_after_ms: retry_after_ms.unwrap_or(1000) };
    }
    if status == Some(413) && has_images {
        return LlmError::ImageOffloadRequired;
    }
    if matches!(status, Some(408 | 409 | 500..=599))
        || matches!(code, "server_error" | "internal_error" | "overloaded_error")
        || matches!(kind, "server_error" | "overloaded_error")
    {
        return LlmError::Transient(at);
    }
    LlmError::Fatal(format!("{at}; the Responses request was rejected"))
}

fn number(value: &Value, field: &str) -> u32 {
    value[field].as_u64().and_then(|number| u32::try_from(number).ok()).unwrap_or(0)
}
