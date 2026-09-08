#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use aho_llm::{Llm, LlmError, Request, StreamEvent};
use aho_types::{CallId, ContentBlock, Message, Role, StopReason};
use llm_openai_responses::{Flavor, ResponsesRoute, RouteConfig, http_client};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve(status: u16, headers: &str, response_body: &str) -> (String, tokio::task::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let response = format!("HTTP/1.1 {status} Test\r\nConnection: close\r\n{headers}\r\n{response_body}");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        let header_end = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let length: usize = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse()
            .unwrap();
        while bytes.len() < header_end + length {
            let read = socket.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
        for chunk in response.as_bytes().chunks(7) {
            socket.write_all(chunk).await.unwrap();
        }
        socket.shutdown().await.unwrap();
        (header, body)
    });
    (base, task)
}

#[tokio::test]
async fn responses_route_decodes_fragmented_sse_and_uses_the_complete_endpoint_once() {
    let sse = concat!(
        "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello — 世界\"}\r\n\r\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":9,\"output_tokens\":3}}}\n\n",
    );
    let (base, server) = serve(200, "Content-Type: text/event-stream\r\n", sse).await;
    let route = ResponsesRoute::new(
        RouteConfig::new("meta", &format!("{base}/v1/responses/"), Some("static-key".into()), Flavor::Generic),
        http_client().unwrap(),
    );
    let mut events = Vec::new();
    let response = route
        .stream(
            Request { model: "muse".into(), messages: vec![Message::user("hello")], ..Default::default() },
            Box::new(|event| events.push(event)),
        )
        .await
        .unwrap();
    assert_eq!(response.content, [ContentBlock::text("Hello — 世界")]);
    assert_eq!(response.stop, StopReason::EndTurn);
    assert_eq!(response.usage.output_tokens, 3);
    assert_eq!(events[0], StreamEvent::Text("Hello — 世界".into()));
    let (header, _) = server.await.unwrap();
    assert!(header.starts_with("POST /v1/responses HTTP/1.1\r\n"));
    assert!(header.to_ascii_lowercase().contains("authorization: bearer static-key\r\n"));
}

#[tokio::test]
async fn retry_after_and_auth_failures_do_not_become_successful_empty_streams() {
    for (status, expected) in [(429, true), (401, false)] {
        let (base, server) =
            serve(status, "Content-Type: application/json\r\nRetry-After: 2.5\r\n", r#"{"error":{"message":"secret-token"}}"#).await;
        let route = ResponsesRoute::new(RouteConfig::new("xai", &base, Some("static-key".into()), Flavor::Generic), http_client().unwrap());
        let error = route.stream(Request::default(), Box::new(|_| {})).await.unwrap_err();
        if expected {
            assert_eq!(error, LlmError::RateLimited { retry_after_ms: 2500 });
        } else {
            assert!(matches!(&error, LlmError::Auth(_)));
        }
        assert!(!error.to_string().contains("secret-token"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn copilot_tool_only_user_turns_are_agent_requests_but_real_user_input_is_not() {
    for (with_user_text, initiator) in [(false, "agent"), (true, "user")] {
        let (base, server) = serve(
            200,
            "Content-Type: text/event-stream\r\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        )
        .await;
        let mut content = vec![ContentBlock::ToolResult {
            id: CallId("a".into()),
            is_error: false,
            content: vec![ContentBlock::Image { media_type: "image/png".into(), data: vec![1, 2, 3] }],
        }];
        if with_user_text {
            content.push(ContentBlock::text("new user instruction"));
        }
        let route = ResponsesRoute::new(
            RouteConfig::new("github-copilot", &base, Some("static-key".into()), Flavor::Copilot),
            http_client().unwrap(),
        );
        route
            .stream(
                Request { model: "gpt-5".into(), messages: vec![Message { role: Role::User, content }], ..Default::default() },
                Box::new(|_| {}),
            )
            .await
            .unwrap();
        let (header, body) = server.await.unwrap();
        let header = header.to_ascii_lowercase();
        assert!(header.contains(&format!("x-initiator: {initiator}\r\n")));
        assert!(header.contains("copilot-vision-request: true\r\n"));
        assert_eq!(body["input"][0]["type"], "function_call_output");
    }
}
