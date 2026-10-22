//! These fixtures exercise the real HTTP/SSE transport on loopback only.
//! They use no credentials and make no request to any vendor.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use runeengine::provider::{live::Live, CallRef, Chunk, Flow, Message, Provider, Request};
use runeengine::settings::providers::{preset, ModelInfo};
use runeengine::settings::{ModelSettings, Settings};
use serde_json::{json, Value};

const APIS: &[&str] = &[
    "moonshot",
    "alibaba",
    "minimax",
    "tencent",
    "zai",
    "meta",
    "stepfun",
    "inclusionai",
    "nvidia",
    "inference",
    "cloudflare",
    "fireworks",
    "together",
    "siliconflow",
    "deepinfra",
    "novita",
    "tokenrouter",
    "cerebras",
    "huggingface",
    "compatible",
];

fn read_request(socket: &mut TcpStream) -> Value {
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut header = Vec::new();
    let mut byte = [0u8];
    while !header.ends_with(b"\r\n\r\n") {
        socket.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    let header = String::from_utf8(header).unwrap();
    assert!(header.starts_with("POST /v1/chat/completions HTTP/1.1"));
    let len: usize = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse().unwrap())
        })
        .unwrap();
    let mut body = vec![0; len];
    socket.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn answer(socket: &mut TcpStream, round: usize) {
    let frames = if round == 0 {
        vec![
            json!({"id":"response-1","choices":[{"delta":{
                "reasoning_content":"plan","tool_calls":[{"index":0,"id":"call-1",
                "type":"function","function":{"name":"inspect_file","arguments":"{\"action\":"}}]
            },"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,
                "function":{"arguments":"\"read\",\"path\":\"file.txt\"}"}}]},
                "finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":100,"completion_tokens":20,
                "prompt_tokens_details":{"cached_tokens":50},
                "completion_tokens_details":{"reasoning_tokens":10}}}),
        ]
    } else {
        vec![
            json!({"id":"response-2","choices":[{"delta":{"content":"complete"},
            "finish_reason":"stop"}],"usage":{"prompt_tokens":140,
            "completion_tokens":5,"cached_tokens":60}}),
        ]
    };
    let mut response = String::new();
    for frame in frames {
        response.push_str(&format!("data: {frame}\n\n"));
    }
    response.push_str("data: [DONE]\n\n");
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
}

#[test]
fn added_adapters_complete_two_http_tool_rounds_and_preserve_usage_and_history() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in APIS {
            for round in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                requests.push(read_request(&mut socket));
                answer(&mut socket, round);
            }
        }
        requests
    });
    for api in APIS {
        let mut config = preset(api).unwrap().build("");
        config.base_url = base.clone();
        config.secret = None;
        config.models = vec![ModelInfo {
            id: "fixture-model".into(),
            thinking: true,
            ..Default::default()
        }];
        config.settle_thinking();
        config.check().unwrap();
        let mut settings = Settings::empty();
        settings.providers.list = vec![config.clone()];

        let live = Live::new(Arc::new(Mutex::new(settings)));
        let mut request = Request {
            control: runeengine::provider::RequestControl {
                timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            },
            system: "Rune system stays exact".into(),
            messages: vec![Message::user("read the file")],
            tools: vec![runeengine::tools::ToolDecl {
                name: "inspect_file".into(),
                description: "read one file".into(),
                parameters: json!({"type":"object","properties":{"action":{"type":"string"},"path":{"type":"string"}},"required":["action","path"]}),
            }],
            settings: ModelSettings::for_provider(&config),
        };
        let mut first = Vec::new();
        live.stream(&request, &mut |chunk| {
            first.push(chunk);
            Flow::Continue
        })
        .unwrap_or_else(|error| panic!("{api}: {error}"));
        let calls: Vec<_> = first
            .iter()
            .filter_map(|chunk| match chunk {
                Chunk::ToolCall { id, name, args } => Some(CallRef {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.to_string(),
                }),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "{api}");
        assert_eq!(
            serde_json::from_str::<Value>(&calls[0].args).unwrap(),
            json!({"action":"read","path":"file.txt"})
        );
        let mut assistant = Message::assistant("").with_calls(calls);
        assistant.continuation = first.iter().find_map(|chunk| match chunk {
            Chunk::Continuation(value) => Some(value.clone()),
            _ => None,
        });
        assert_eq!(
            first
                .iter()
                .filter(|chunk| matches!(chunk, Chunk::Usage { .. }))
                .count(),
            1
        );
        assert!(first.iter().any(|chunk| matches!(chunk,Chunk::Usage{input:100,output:20,cached:50,details} if details.reasoning==Some(10) && details.complete)),"{api}");
        request.messages.push(assistant);
        request.messages.push(Message::tool_result(
            "call-1",
            "inspect_file",
            "literal result",
        ));
        let mut second = Vec::new();
        live.stream(&request, &mut |chunk| {
            second.push(chunk);
            Flow::Continue
        })
        .unwrap_or_else(|error| panic!("{api}: {error}"));
        assert!(
            second
                .iter()
                .any(|chunk| matches!(chunk,Chunk::Text(text) if text=="complete")),
            "{api}"
        );
        assert!(
            second.iter().any(|chunk| matches!(
                chunk,
                Chunk::Usage {
                    input: 140,
                    output: 5,
                    cached: 60,
                    ..
                }
            )),
            "{api}"
        );
    }
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), APIS.len() * 2);
    for pair in requests.chunks_exact(2) {
        assert_eq!(
            pair[0]["messages"][0],
            json!({"role":"system","content":"Rune system stays exact"})
        );
        assert_eq!(pair[1]["messages"][0], pair[0]["messages"][0]);
        assert_eq!(pair[1]["tools"], pair[0]["tools"]);
        assert_eq!(pair[1]["messages"][2]["reasoning_content"], "plan");
        assert_eq!(
            pair[1]["messages"][3],
            json!({"role":"tool","tool_call_id":"call-1","content":"literal result"})
        );
        assert!(pair[0].get("extra_body").is_none());
    }
}
