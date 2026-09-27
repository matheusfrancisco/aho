//! Model-backed [`Judge`] (listwise rerank) and [`Extractor`] over an `llm:<route>` service.
//! Plain requests with no tools: the extraction "agent" runs under policy `Nothing`.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use aho_llm::{LlmRoute, Request, joined_text};
use aho_memory::{Draft, Entry, Extractor, Judge, MIN_POOL, MemoryError, Trigger};
use aho_types::{BoxFuture, ContentBlock, Message, Role, Value};

const RERANK_SYSTEM: &str = "You rank stored long-term memories by relevance to a user's request. \
Reply with only a JSON array of the ids of the memories that help with the request, most relevant first, for example [12, 3]. \
Leave out memories that do not help. Reply [] when none do.";

const EXTRACT_SYSTEM: &str = "You maintain an assistant's long-term memory. From the conversation window, extract durable facts \
worth recalling in future sessions: decisions, preferences, project conventions, environment facts, and how problems were resolved. \
Skip small talk, transient state, and anything already listed under existing memories. If a new fact contradicts an existing memory, \
put that memory's id in `supersedes`. Reply with only a JSON array of objects \
{\"text\": string, \"tags\": [string], \"confidence\": number between 0 and 1, \"supersedes\": [id]}. Reply [] when nothing is worth keeping.";

/// Confidence for a model-extracted fact that states none.
pub const EXTRACTED_CONFIDENCE: f32 = 0.6;
const WINDOW_CHARS: usize = 24_000;

pub(crate) fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// The last `max` chars of `s` (the recent end of a long window matters most).
fn clip_head(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = s.char_indices().nth(n - max).map(|(i, _)| i).unwrap_or(0);
    format!("…{}", &s[skip..])
}

fn tags(e: &Entry) -> String {
    if e.tags.is_empty() { String::new() } else { format!(" (tags: {})", e.tags.join(", ")) }
}

/// The JSON array between the first `[` and the last `]` of a model reply.
fn json_array(text: &str) -> Option<Vec<Value>> {
    let (a, b) = (text.find('[')?, text.rfind(']')?);
    if b < a {
        return None;
    }
    serde_json::from_str(&text[a..=b]).ok()
}

fn as_id(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().trim_start_matches('#').parse().ok()))
}

/// Ids from a rerank reply, in order, deduped; `None` when the reply holds no JSON list.
pub fn parse_id_list(text: &str) -> Option<Vec<u64>> {
    let mut seen = BTreeSet::new();
    Some(json_array(text)?.iter().filter_map(as_id).filter(|id| seen.insert(*id)).collect())
}

/// Drafts from an extraction reply; ids in `supersedes` are kept only if they name `existing` entries.
pub fn parse_drafts(text: &str, existing: &[Entry]) -> Option<Vec<Draft>> {
    let known: BTreeSet<u64> = existing.iter().map(|e| e.id).collect();
    let items = json_array(text)?;
    Some(
        items
            .iter()
            .filter_map(|o| {
                let t = o.get("text")?.as_str()?.trim();
                if t.is_empty() {
                    return None;
                }
                let tags = o
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let confidence =
                    o.get("confidence").and_then(Value::as_f64).map(|c| c as f32).unwrap_or(EXTRACTED_CONFIDENCE).clamp(0.0, 1.0);
                let supersedes = o
                    .get("supersedes")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(as_id).filter(|id| known.contains(id)).collect())
                    .unwrap_or_default();
                Some(Draft { text: t.to_string(), tags, confidence, supersedes })
            })
            .collect(),
    )
}

async fn ask(route: &LlmRoute, model: &str, section: &str, system: &str, prompt: String, max_tokens: u32) -> Result<String, String> {
    let req = Request {
        model: model.to_string(),
        system: vec![(section.to_string(), system.to_string(), false)],
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
        max_tokens,
        cache_key: None,
    };
    let resp = route.0.stream(req, Box::new(|_| {})).await.map_err(|e| e.to_string())?;
    Ok(joined_text(&resp.content))
}

pub struct LlmJudge {
    route: LlmRoute,
    model: String,
}

impl LlmJudge {
    pub fn new(route: LlmRoute, model: impl Into<String>) -> LlmJudge {
        LlmJudge { route, model: model.into() }
    }
}

pub fn rerank_prompt(query: &str, candidates: &[Entry]) -> String {
    let mut s = format!("Request:\n{}\n\nMemories:\n", clip(query, 2000));
    for e in candidates.iter().take(MIN_POOL) {
        let _ = writeln!(s, "[{}] {}{}", e.id, clip(&e.text, 400), tags(e));
    }
    s
}

impl Judge for LlmJudge {
    fn rerank<'a>(&'a self, query: &'a str, candidates: &'a [Entry]) -> BoxFuture<'a, Result<Vec<u64>, MemoryError>> {
        Box::pin(async move {
            let reply = ask(&self.route, &self.model, "memory-rerank", RERANK_SYSTEM, rerank_prompt(query, candidates), 400)
                .await
                .map_err(|e| MemoryError::Judge(format!("the rerank model call failed: {e}")))?;
            let shown: BTreeSet<u64> = candidates.iter().take(MIN_POOL).map(|e| e.id).collect();
            let ids = parse_id_list(&reply)
                .ok_or_else(|| MemoryError::Judge(format!("the rerank model replied without a JSON id list: {}", clip(&reply, 200))))?;
            Ok(ids.into_iter().filter(|id| shown.contains(id)).collect())
        })
    }
}

pub struct LlmExtractor {
    route: LlmRoute,
    model: String,
}

impl LlmExtractor {
    pub fn new(route: LlmRoute, model: impl Into<String>) -> LlmExtractor {
        LlmExtractor { route, model: model.into() }
    }
}

/// A transcript rendering of `window` for the extraction prompt, newest end kept when long.
pub fn render_window(window: &[Message]) -> String {
    let mut s = String::new();
    for m in window {
        let who = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        };
        for b in &m.content {
            match b {
                ContentBlock::Text { text } => {
                    let _ = writeln!(s, "{who}: {}", clip(text, 4000));
                }
                ContentBlock::ToolUse { name, args, .. } => {
                    let _ = writeln!(s, "{who} called {name}({})", clip(&args.to_string(), 400));
                }
                ContentBlock::ToolResult { content, is_error, .. } => {
                    let body: String = content.iter().filter_map(|c| c.as_text()).collect::<Vec<_>>().join("\n");
                    let _ = writeln!(s, "{who} {}: {}", if *is_error { "error" } else { "result" }, clip(&body, 1500));
                }
                ContentBlock::Image { .. } => {
                    let _ = writeln!(s, "{who}: [image]");
                }
                ContentBlock::Thinking { .. } | ContentBlock::ProviderMetadata { .. } => {}
            }
        }
    }
    clip_head(&s, WINDOW_CHARS)
}

pub fn extract_prompt(trigger: Trigger, window: &[Message], existing: &[Entry]) -> String {
    let mut s = format!("Trigger: {trigger}\n\nExisting memories:\n");
    if existing.is_empty() {
        s.push_str("(none)\n");
    }
    for e in existing {
        let _ = writeln!(s, "[{}] {}", e.id, clip(&e.text, 300));
    }
    let _ = write!(s, "\nConversation window:\n{}", render_window(window));
    s
}

impl Extractor for LlmExtractor {
    fn extract<'a>(
        &'a self,
        trigger: Trigger,
        window: &'a [Message],
        existing: &'a [Entry],
    ) -> BoxFuture<'a, Result<Vec<Draft>, MemoryError>> {
        Box::pin(async move {
            let reply = ask(&self.route, &self.model, "memory-extract", EXTRACT_SYSTEM, extract_prompt(trigger, window, existing), 1500)
                .await
                .map_err(|e| MemoryError::Extract(format!("the extraction model call failed: {e}")))?;
            parse_drafts(&reply, existing)
                .ok_or_else(|| MemoryError::Extract(format!("the extraction model replied without a JSON list: {}", clip(&reply, 200))))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use aho_llm::{AdapterInfo, LateResults, Llm, LlmError, Protocol, Response, Sink};
    use aho_types::{StopReason, Usage};

    /// Stands in for the remote model: replies with a fixed text and records the request.
    struct Canned {
        reply: Result<String, LlmError>,
        seen: Mutex<Vec<Request>>,
    }

    impl Canned {
        fn requests(&self) -> Vec<Request> {
            self.seen.lock().map(|g| g.clone()).unwrap_or_default()
        }
    }

    impl Llm for Canned {
        fn info(&self) -> AdapterInfo {
            AdapterInfo { protocol: Protocol::OpenAiCompletions, late_results: LateResults::ToolMessage, route: "memory".into() }
        }
        fn stream<'a>(&'a self, req: Request, _sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
            if let Ok(mut s) = self.seen.lock() {
                s.push(req);
            }
            let r = self.reply.clone().map(|t| Response {
                content: vec![ContentBlock::text(t)],
                usage: Usage::default(),
                stop: StopReason::EndTurn,
            });
            Box::pin(async move { r })
        }
    }

    fn route(reply: Result<&str, LlmError>) -> (LlmRoute, Arc<Canned>) {
        let c = Arc::new(Canned { reply: reply.map(str::to_string), seen: Mutex::new(Vec::new()) });
        (LlmRoute(c.clone()), c)
    }

    fn entry(id: u64, text: &str) -> Entry {
        let mut e = Entry::new(text, vec!["t".into()]);
        e.id = id;
        e
    }

    #[test]
    fn parses_id_lists_leniently() {
        assert_eq!(parse_id_list("Relevant: [3, \"#7\", 3, \"x\"] done"), Some(vec![3, 7]));
        assert_eq!(parse_id_list("[]"), Some(vec![]));
        assert_eq!(parse_id_list("none of them"), None);
        assert_eq!(parse_id_list("] oops ["), None);
    }

    #[tokio::test]
    async fn judge_keeps_only_shown_ids_and_reports_failures() {
        let cands = vec![entry(1, "alpha"), entry(2, "beta")];
        let (r, seen) = route(Ok("[2, 99, 1]"));
        let j = LlmJudge::new(r, "judge-model");
        assert_eq!(j.rerank("which?", &cands).await.unwrap(), vec![2, 1]);
        let req = seen.requests()[0].clone();
        assert_eq!(req.model, "judge-model");
        assert!(req.tools.is_empty(), "policy Nothing: no tools offered");
        assert!(req.messages[0].text().contains("[2] beta (tags: t)"));

        let (r, _) = route(Err(LlmError::Transient("503".into())));
        assert!(matches!(LlmJudge::new(r, "m").rerank("q", &cands).await, Err(MemoryError::Judge(_))));
        let (r, _) = route(Ok("I think the second one"));
        assert!(matches!(LlmJudge::new(r, "m").rerank("q", &cands).await, Err(MemoryError::Judge(m)) if m.contains("JSON")));
    }

    #[tokio::test]
    async fn extractor_parses_drafts_and_filters_unknown_supersedes() {
        let existing = vec![entry(5, "tabs for indentation")];
        let reply = r#"Here: [{"text": "The project indents with spaces", "tags": ["style"], "confidence": 0.9, "supersedes": [5, 42]},
                              {"text": "  "}, {"text": "CI runs on push", "confidence": 7}]"#;
        let (r, seen) = route(Ok(reply));
        let x = LlmExtractor::new(r, "m");
        let window = vec![Message::user("we switched to spaces"), Message::assistant(vec![ContentBlock::text("noted")])];
        let d = x.extract(Trigger::TopicChange, &window, &existing).await.unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(
            d[0],
            Draft { text: "The project indents with spaces".into(), tags: vec!["style".into()], confidence: 0.9, supersedes: vec![5] }
        );
        assert_eq!(d[1].confidence, 1.0, "clamped");
        let prompt = seen.requests()[0].messages[0].text();
        assert!(
            prompt.contains("Trigger: topic change")
                && prompt.contains("[5] tabs for indentation")
                && prompt.contains("user: we switched to spaces")
        );
    }

    #[test]
    fn long_windows_keep_their_recent_end() {
        let window = vec![Message::user("old ".repeat(10_000)), Message::user("the newest line")];
        let r = render_window(&window);
        assert!(r.chars().count() <= WINDOW_CHARS + 1);
        assert!(r.ends_with("user: the newest line\n"));
    }
}
