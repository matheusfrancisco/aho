use aho_llm::LlmError;
use aho_types::Value;

/// Classify gateway HTTP envelopes and terminal stream errors without echoing response bodies:
/// upstream diagnostics can contain credentials, request headers, or prompt text.
pub fn classify(status: Option<u16>, retry_after_ms: Option<u64>, body: &str, has_images: bool) -> LlmError {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = value["error"]["message"].as_str().or_else(|| value["errorMessage"].as_str()).unwrap_or("").to_lowercase();
    let code = value["error"]["code"].as_str().or_else(|| value["error"]["type"].as_str()).unwrap_or("");
    let at = status
        .map(|status| format!("Radius gateway answered HTTP {status}"))
        .unwrap_or_else(|| "Radius gateway reported a stream error".into());
    if matches!(status, Some(401 | 403)) || matches!(code, "authentication_error" | "permission_error" | "unauthorized") {
        return LlmError::Auth(format!("{at}; run `aho login radius` or set `provider.radius.key`"));
    }
    if status == Some(429) || matches!(code, "rate_limit_error" | "rate_limited") || message.contains("rate limit") {
        return LlmError::RateLimited { retry_after_ms: retry_after_ms.unwrap_or(1000) };
    }
    if code == "context_length_exceeded"
        || message.contains("context window")
        || message.contains("prompt is too long")
        || message.contains("maximum context length")
        || message.contains("too many tokens")
    {
        return LlmError::ContextWindowExceeded { requested: 0, available: 0 };
    }
    if (status == Some(413) && has_images) || (message.contains("image") && (message.contains("too large") || message.contains("exceeds")))
    {
        return LlmError::ImageOffloadRequired;
    }
    if (message.contains("max_tokens") || message.contains("maxtokens") || message.contains("max_output_tokens"))
        && (message.contains("maximum") || message.contains("limit"))
    {
        return LlmError::MaxOutputTokens;
    }
    if value["reason"] == "aborted" {
        return LlmError::Transient("Radius gateway aborted the stream".into());
    }
    if matches!(status, Some(408 | 500..=599))
        || matches!(code, "overloaded_error" | "api_error" | "timeout_error")
        || message.contains("overloaded")
        || message.contains("timed out")
    {
        return LlmError::Transient(at);
    }
    LlmError::Fatal(format!("{at}; the gateway rejected the model request"))
}

pub(crate) fn malformed() -> LlmError {
    LlmError::Transient("Radius gateway sent a malformed pi-messages event; the partial response was discarded".into())
}
