//! GitHub's webhook protocol: `X-Hub-Signature-256`, `X-GitHub-Event`, `X-GitHub-Delivery`, and
//! the two payload encodings (`application/json`, `application/x-www-form-urlencoded` with a
//! single `payload=` field). The signature is always over the raw request body.

use aho_types::Value;

pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";
pub const EVENT_HEADER: &str = "x-github-event";
pub const DELIVERY_HEADER: &str = "x-github-delivery";

/// Why a signature was refused. The text is safe to return to the caller: it never echoes input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    Missing,
    Malformed,
    Mismatch,
}

impl SignatureError {
    pub fn message(self) -> &'static str {
        match self {
            SignatureError::Missing => {
                "the request has no X-Hub-Signature-256 header; nothing was delivered. Set the webhook secret in the GitHub hook settings to the credential aho uses."
            }
            SignatureError::Malformed => {
                "the X-Hub-Signature-256 header is not `sha256=<64 hex digits>`; nothing was delivered. Only GitHub's SHA-256 signature is accepted."
            }
            SignatureError::Mismatch => {
                "the X-Hub-Signature-256 signature does not match the body; nothing was delivered. Check that the GitHub hook secret equals the credential aho uses."
            }
        }
    }
}

/// Verifies `sha256=<hex>` against HMAC-SHA256(secret, body). `ring::hmac::verify` compares in
/// constant time, so the tag cannot be recovered byte by byte from response timing.
pub fn verify_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> Result<(), SignatureError> {
    let header = header.ok_or(SignatureError::Missing)?;
    let hex = header.trim().strip_prefix("sha256=").ok_or(SignatureError::Malformed)?;
    let tag = decode_hex(hex).ok_or(SignatureError::Malformed)?;
    if tag.len() != 32 {
        return Err(SignatureError::Malformed);
    }
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    ring::hmac::verify(&key, body, &tag).map_err(|_| SignatureError::Mismatch)
}

/// `sha256=<hex>` for `body`; what GitHub sends (and what tests sign with).
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16);
    s.as_bytes().chunks(2).map(|p| Some((digit(p[0])? * 16 + digit(p[1])?) as u8)).collect()
}

/// GitHub delivery ids are GUIDs; anything else is refused before it reaches a log line or a key.
pub fn valid_delivery(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Event names are `[a-z_]+`; the header is refused otherwise.
pub fn valid_event(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// The JSON payload, from either content type GitHub offers.
pub fn payload(content_type: Option<&str>, body: &[u8]) -> Result<Value, String> {
    let form = content_type.is_some_and(|c| c.trim().to_ascii_lowercase().starts_with("application/x-www-form-urlencoded"));
    let json = if form {
        let text = std::str::from_utf8(body).map_err(|_| "the form body is not UTF-8".to_string())?;
        let field =
            text.split('&').find_map(|kv| kv.strip_prefix("payload=")).ok_or_else(|| "the form body has no `payload` field".to_string())?;
        percent_decode(field).ok_or_else(|| "the `payload` field is not valid URL encoding".to_string())?
    } else {
        body.to_vec()
    };
    serde_json::from_slice(&json).map_err(|e| format!("the payload is not JSON ({e})"))
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(*decode_hex(hex)?.first()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    Some(out)
}

/// `@aho` in `text`, case-insensitively, not as a prefix of a longer handle (`@ahoy`). An empty
/// trigger matches everything.
pub fn mentions(text: &str, trigger: &str) -> bool {
    if trigger.is_empty() {
        return true;
    }
    let hay = text.to_lowercase();
    let needle = trigger.to_lowercase();
    let handle_char = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let mut from = 0;
    while let Some(pos) = hay[from..].find(&needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = hay[..start].chars().next_back().is_none_or(|c| !handle_char(c));
        let after_ok = hay[end..].chars().next().is_none_or(|c| !handle_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trips_and_rejects_every_malformed_shape() {
        let body = br#"{"zen":"hi"}"#;
        let good = sign(b"s3cret", body);
        assert_eq!(verify_signature(b"s3cret", body, Some(&good)), Ok(()));
        assert_eq!(verify_signature(b"other", body, Some(&good)), Err(SignatureError::Mismatch));
        assert_eq!(verify_signature(b"s3cret", b"{}", Some(&good)), Err(SignatureError::Mismatch));
        assert_eq!(verify_signature(b"s3cret", body, None), Err(SignatureError::Missing));
        assert_eq!(verify_signature(b"s3cret", body, Some(&good.replace("sha256=", "sha1="))), Err(SignatureError::Malformed));
        assert_eq!(verify_signature(b"s3cret", body, Some("sha256=zz")), Err(SignatureError::Malformed));
        assert_eq!(verify_signature(b"s3cret", body, Some(&good[..good.len() - 2])), Err(SignatureError::Malformed), "short tag");
    }

    #[test]
    fn github_documented_test_vector() {
        // docs.github.com "Validating webhook deliveries": secret "It's a Secret to Everybody", payload "Hello, World!".
        assert_eq!(
            sign(b"It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }

    #[test]
    fn form_encoded_payloads_decode() {
        let v = payload(Some("application/x-www-form-urlencoded"), b"payload=%7B%22a%22%3A%22b+c%22%7D").unwrap();
        assert_eq!(v, serde_json::json!({"a": "b c"}));
        assert!(payload(Some("application/x-www-form-urlencoded"), b"other=1").is_err());
        assert!(payload(Some("application/json"), b"not json").is_err());
    }

    #[test]
    fn mention_is_a_whole_handle() {
        assert!(mentions("hey @aho, look", "@aho"));
        assert!(mentions("@AHO please", "@aho"));
        assert!(!mentions("hey @ahoy", "@aho"));
        assert!(!mentions("mail x@aho.dev", "@aho"), "glued to a preceding word");
        assert!(!mentions("no trigger here", "@aho"));
        assert!(mentions("anything", ""));
    }

    #[test]
    fn delivery_and_event_names_are_fenced() {
        assert!(valid_delivery("72d3162e-cc78-11e3-81ab-4c9367dc0958"));
        assert!(!valid_delivery("a b"));
        assert!(!valid_delivery("\u{1b}[31m"));
        assert!(!valid_delivery(""));
        assert!(valid_event("pull_request"));
        assert!(!valid_event("Pull-Request"));
    }
}
