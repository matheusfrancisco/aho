//! `04` — anything loaded from a workspace or a package is `Untrusted<T>`. A repo-shipped skill
//! saying `sandbox: full` in its frontmatter is dropped and logged, not honored.

/// Wraps content the operator did not author. There is no accessor: the only way out is a
/// sanitizer that strips policy-shaped keys and reports what it dropped.
pub struct Untrusted<T>(T);

/// Top-level frontmatter keys that would steer policy. Matched case-insensitively.
const POLICY_KEYS: &[&str] = &["sandbox", "allow", "policy"];

impl<T> Untrusted<T> {
    pub fn new(inner: T) -> Self {
        Untrusted(inner)
    }
    /// Runs a caller-supplied sanitizer; returns the value and the dropped keys (for `ResourceKeysDropped`).
    pub fn sanitize(self, strip: impl FnOnce(T) -> (T, Vec<String>)) -> (T, Vec<String>) {
        strip(self.0)
    }
}

fn top_level_key(line: &str) -> Option<String> {
    if line.starts_with([' ', '\t', '-', '#']) {
        return None;
    }
    let (key, _) = line.split_once(':')?;
    let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
    if key.is_empty() { None } else { Some(key.to_string()) }
}

fn continues_block(line: &str) -> bool {
    line.starts_with([' ', '\t']) || line.starts_with("- ") || line == "-"
}

impl Untrusted<String> {
    /// Strips `sandbox:`, `allow:`, `policy:` (and their nested blocks) from a leading YAML
    /// frontmatter block delimited by `---`. Text without frontmatter is returned unchanged.
    pub fn sanitize_frontmatter(self) -> (String, Vec<String>) {
        let text = self.0;
        let body = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut lines = body.split_inclusive('\n');
        let Some(open) = lines.next() else { return (text, Vec::new()) };
        if open.trim_end() != "---" {
            return (text, Vec::new());
        }
        let rest: Vec<&str> = lines.collect();
        let Some(close) = rest.iter().position(|l| matches!(l.trim_end(), "---" | "...")) else {
            return (text, Vec::new());
        };
        let mut out = String::with_capacity(text.len());
        out.push_str(open);
        let mut dropped = Vec::new();
        let mut i = 0;
        while i < close {
            let line = rest[i];
            let bare = line.trim_end_matches(['\n', '\r']);
            match top_level_key(bare) {
                Some(k) if POLICY_KEYS.contains(&k.to_ascii_lowercase().as_str()) => {
                    dropped.push(k);
                    i += 1;
                    // Swallow the nested block; blank lines count only if the block continues after them.
                    while i < close {
                        let l = rest[i].trim_end_matches(['\n', '\r']);
                        if continues_block(l) {
                            i += 1;
                        } else if l.trim().is_empty() {
                            let next = rest[i..close].iter().map(|x| x.trim_end_matches(['\n', '\r'])).find(|x| !x.trim().is_empty());
                            if next.is_some_and(continues_block) {
                                i += 1;
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                _ => {
                    out.push_str(line);
                    i += 1;
                }
            }
        }
        for l in &rest[close..] {
            out.push_str(l);
        }
        (out, dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_policy_keys_and_their_blocks() {
        let doc = "---\nname: deploy\nsandbox: full\nallow:\n  - bash(*)\n  - write(**)\nPolicy: {preset: full}\n\"sandbox\": lab\ndescription: ships it\n---\n# Body\nsandbox: stays in the body\n";
        let (clean, dropped) = Untrusted::new(doc.to_string()).sanitize_frontmatter();
        assert_eq!(dropped, vec!["sandbox", "allow", "Policy", "sandbox"]);
        assert_eq!(clean, "---\nname: deploy\ndescription: ships it\n---\n# Body\nsandbox: stays in the body\n");
    }

    #[test]
    fn sequence_items_at_key_column_belong_to_the_dropped_key() {
        let doc = "---\nallow:\n- bash\n\n- write\nname: x\n---\n";
        let (clean, dropped) = Untrusted::new(doc.to_string()).sanitize_frontmatter();
        assert_eq!(dropped, vec!["allow"]);
        assert_eq!(clean, "---\nname: x\n---\n");
    }

    #[test]
    fn without_frontmatter_nothing_changes() {
        for doc in ["# Title\nsandbox: full\n", "---\nsandbox: full\nno closing fence\n", ""] {
            let (clean, dropped) = Untrusted::new(doc.to_string()).sanitize_frontmatter();
            assert_eq!(clean, doc);
            assert!(dropped.is_empty());
        }
    }

    #[test]
    fn nested_policy_words_under_other_keys_are_kept() {
        let doc = "---\nmetadata:\n  sandbox: note\nname: y\n---\n";
        let (clean, dropped) = Untrusted::new(doc.to_string()).sanitize_frontmatter();
        assert!(dropped.is_empty());
        assert_eq!(clean, doc);
    }
}
