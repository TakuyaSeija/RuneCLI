//! End-to-end real I/O and process testing for Rune attachments and tool truncation.
//!
//! Spawns the real `rune` binary, creates real files on disk (text, huge text >60k chars,
//! PNG images, Excel binary, oversized images), feeds real protocol messages via stdin,
//! and asserts on real stdout events and tool execution.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

use runeengine::media::Attachment;
use runeengine::settings::providers::{Dialect, ModelInfo};

struct RealHost {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    project: PathBuf,
}

fn start_real_host(name: &str, fixture: Option<Value>) -> RealHost {
    let base = std::env::temp_dir().join("rune-real-io").join(format!(
        "test-{}-{}",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();

    let script = base.join("fixture.json");
    let fixture = fixture.unwrap_or_else(|| json!({ "responses": [] }));
    std::fs::write(&script, serde_json::to_string_pretty(&fixture).unwrap()).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rune"))
        .env("RUNE_HOME", &base)
        .arg("-host")
        .arg("-db")
        .arg(base.join("rune.db"))
        .arg("-script")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("rune binary must start");

    let stdin = child.stdin.take().unwrap();
    let out = BufReader::new(child.stdout.take().unwrap());
    RealHost {
        child,
        stdin,
        out,
        project,
    }
}

impl RealHost {
    fn send(&mut self, cmd: Value) {
        writeln!(self.stdin, "{cmd}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn next(&mut self) -> Value {
        let mut line = String::new();
        let n = self.out.read_line(&mut line).expect("read host stdout");
        assert!(n > 0, "the host closed its output early");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("host wrote non-JSON line: {line:?} ({e})"))
    }

    fn until(&mut self, kind: &str) -> Value {
        loop {
            let e = self.next();
            if e["t"] == "permission_request" {
                let req = e["req"].as_str().unwrap().to_string();
                self.send(json!({
                    "cmd": "permission.reply",
                    "id": 90,
                    "req": req,
                    "decision": "allow_once"
                }));
            }
            if e["t"] == kind {
                return e;
            }
        }
    }

    fn finish(mut self) {
        self.send(json!({ "cmd": "shutdown", "id": 999 }));
        let status = self.child.wait().expect("host exits");
        assert!(status.success(), "host exited with {status}");
    }
}

#[test]
fn real_tool_inspect_text_and_binary_truncation() {
    // 1. Set up fixture so the model calls inspect_file on real files
    let fixture = json!({
        "responses": [
            { "chunks": [
                { "tool_call": {
                    "id": "call_inspect_huge",
                    "name": "inspect_file",
                    "args": { "action": "inspect long file", "path": "huge.txt" }
                }}
            ]},
            { "chunks": [
                { "tool_call": {
                    "id": "call_inspect_binary",
                    "name": "inspect_file",
                    "args": { "action": "inspect binary image", "path": "logo.png" }
                }}
            ]},
            { "chunks": [
                { "text": "All files inspected." }
            ]}
        ]
    });

    let mut h = start_real_host("tool-inspect", Some(fixture));
    h.next(); // ready event

    // 2. Create real files in project workspace
    // Create real text file with 1,200 lines and >65,000 characters
    let mut huge_content = String::with_capacity(75_000);
    for i in 1..=1200 {
        huge_content.push_str(&format!(
            "Line {i:04}: The quick brown fox jumps over the lazy dog repeatedly.\n"
        ));
    }
    assert!(huge_content.chars().count() > 60_000);
    std::fs::write(h.project.join("huge.txt"), &huge_content).unwrap();

    // Create real binary PNG file
    let png_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x10\x00\x00\x00\x10\x08\x06\x00\x00\x00\x1f\xf3\xff\xa4binary_payload";
    std::fs::write(h.project.join("logo.png"), png_bytes).unwrap();

    let project_path = h.project.to_string_lossy().to_string();
    h.send(json!({ "cmd": "session.open", "id": 1, "project": project_path }));
    h.until("ack");

    // 3. Send real prompt to trigger inspect_file calls
    h.send(json!({ "cmd": "prompt", "id": 2, "session": 1, "text": "inspect the files" }));

    // 4. Verify tool results for inspect_file on huge text file:
    // Must display "… cut at line {n} of {total} ({remaining} lines more)"
    let res1 = h.until("tool_result");
    let text1 = res1["content"].as_str().unwrap_or("");
    assert!(
        text1.contains("later lines are not shown"),
        "Real inspect_file output on huge text must say how much is left: {text1}"
    );
    assert!(
        text1.contains("is cut and") && text1.contains("Continue with view_range"),
        "Real inspect_file output on huge text must say where it was cut and how to go on: {text1}"
    );

    // 5. Verify tool results for inspect_file on binary file:
    // With multimodal attachment support, it reports attached binary file and format
    let res2 = h.until("tool_result");
    let text2 = res2["content"].as_str().unwrap_or("");
    assert!(
        text2.contains("Attached logo.png (image/png"),
        "Real inspect_file output on binary file must report the attachment: {text2}"
    );
    assert!(
        !text2.contains("line"),
        "Real inspect_file output on binary file must never mention lines: {text2}"
    );

    let done = h.until("done");
    assert_eq!(done["stop"], "end");
    h.finish();
}

#[test]
fn real_prompt_with_live_attachments() {
    let fixture = json!({
        "responses": [
            { "chunks": [
                { "text": "Received your attachments successfully." },
                { "usage": { "input": 500, "output": 10 } }
            ]}
        ]
    });

    let mut h = start_real_host("attachments-live", Some(fixture));
    h.next(); // ready event

    // Create real files to attach
    // 1. Text file exceeding 60k chars
    let mut huge_text = String::with_capacity(70_000);
    for i in 1..=1100 {
        huge_text.push_str(&format!(
            "Row {i:04}: Important documentation and log content.\n"
        ));
    }
    let huge_path = h.project.join("big_notes.txt");
    std::fs::write(&huge_path, &huge_text).unwrap();

    // 2. Real PNG image
    let img_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90wS\xde";
    let img_path = h.project.join("avatar.png");
    std::fs::write(&img_path, img_bytes).unwrap();

    // 3. Real XLSX / ZIP binary file
    let xlsx_bytes = b"PK\x03\x04\x14\x00\x00\x00\x08\x00fake_excel_bytes";
    let xlsx_path = h.project.join("financials.xlsx");
    std::fs::write(&xlsx_path, xlsx_bytes).unwrap();

    let project_path = h.project.to_string_lossy().to_string();
    h.send(json!({ "cmd": "session.open", "id": 1, "project": project_path }));
    h.until("ack");

    // Send real prompt attaching all 3 files via the protocol
    h.send(json!({
        "cmd": "prompt",
        "id": 2,
        "session": 1,
        "text": "Please examine the attached files.",
        "attach": [
            huge_path.to_string_lossy().to_string(),
            img_path.to_string_lossy().to_string(),
            xlsx_path.to_string_lossy().to_string()
        ]
    }));

    // Observe real event stream from binary
    let mut events = Vec::new();
    loop {
        let e = h.next();
        let kind = e["t"].as_str().unwrap_or("").to_string();
        events.push(kind.clone());
        if kind == "done" {
            break;
        }
    }

    assert!(events.contains(&"turn_start".to_string()));
    assert!(events.contains(&"text".to_string()));
    assert!(events.contains(&"usage".to_string()));
    assert!(events.contains(&"done".to_string()));

    h.finish();
}

#[test]
fn real_wire_payload_inspection_with_real_files() {
    use runeengine::media::Attachment;
    use runeengine::settings::providers::{Dialect, ModelInfo};

    let base = std::env::temp_dir()
        .join("rune-real-wire")
        .join(format!("wire-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    // 1. Create real huge text file (>60k chars)
    let mut huge_text = String::with_capacity(75_000);
    for i in 1..=1200 {
        huge_text.push_str(&format!("Line {i:04}: Content that makes this text file exceed sixty thousand characters limit.\n"));
    }
    let huge_path = base.join("large_doc.txt");
    std::fs::write(&huge_path, &huge_text).unwrap();
    let huge_att = Attachment::at(&huge_path).unwrap();

    // 2. Create real normal text file (<60k chars)
    let normal_path = base.join("small_note.txt");
    std::fs::write(&normal_path, "Hello world, this is normal text.").unwrap();
    let normal_att = Attachment::at(&normal_path).unwrap();

    // 3. Create real Excel binary file
    let xlsx_path = base.join("sheet.xlsx");
    std::fs::write(&xlsx_path, b"PK\x03\x04\x14\x00\x00\x00\x08\x00some_bytes").unwrap();
    let xlsx_att = Attachment::at(&xlsx_path).unwrap();

    // 4. Create real small PNG image
    let small_img_path = base.join("icon.png");
    let small_img_bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x02\x00\x00\x00\x90wS\xde";
    std::fs::write(&small_img_path, small_img_bytes).unwrap();
    let small_img_att = Attachment::at(&small_img_path).unwrap();

    // 5. Create real 25MB PNG image (exceeds Anthropic 32MB payload budget of ~23.8MB)
    let huge_img_path = base.join("huge_photo.png");
    let mut huge_img_bytes = Vec::with_capacity(25 * 1024 * 1024);
    huge_img_bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR");
    huge_img_bytes.resize(25 * 1024 * 1024, 0x42);
    std::fs::write(&huge_img_path, &huge_img_bytes).unwrap();
    let huge_img_att = Attachment::at(&huge_img_path).unwrap();

    let all_attachments = vec![huge_att, normal_att, xlsx_att, small_img_att, huge_img_att];

    let empty = std::collections::HashSet::new();

    // ── Test Anthropic Wire Shaping ──
    let mut claude_model = ModelInfo::named("claude-sonnet-4-20250514", false);
    claude_model.modalities = vec!["image".into(), "file".into()];

    let anthropic_parts = runeengine::media::parts_for_message(
        &all_attachments,
        "Check all attachments",
        Some(&claude_model),
        Dialect::Anthropic,
        &runeengine::media::MediaLimits {
            max_payload_bytes: 32 * 1024 * 1024,
            ..Default::default()
        },
        true,
        0,
        &empty,
    );

    // Verify huge text block: status="too_long", char_limit="60000", truncated 1200 lines more
    match &anthropic_parts[0] {
        runeengine::media::Inlined::Text(xml) => {
            assert!(
                xml.contains("status=\"too_long\""),
                "must be status=too_long: {xml}"
            );
            assert!(
                xml.contains("char_limit=\"60000\""),
                "must contain char_limit=60000: {xml}"
            );
            assert!(
                xml.contains("truncated 1200 lines more"),
                "must report truncated lines: {xml}"
            );
            assert!(
                xml.contains("inspect_file"),
                "must advise inspect_file: {xml}"
            );
        }
        other => panic!("expected Inlined::Text, got {other:?}"),
    }

    // Verify normal text block: inlined text
    match &anthropic_parts[1] {
        runeengine::media::Inlined::Text(xml) => {
            assert!(xml.contains("<file_attachment path="));
            assert!(xml.contains("Hello world, this is normal text."));
        }
        other => panic!("expected Inlined::Text, got {other:?}"),
    }

    // Verify xlsx block: status="binary_not_injected"
    match &anthropic_parts[2] {
        runeengine::media::Inlined::Text(xml) => {
            assert!(
                xml.contains("status=\"binary_not_injected\""),
                "must be binary_not_injected: {xml}"
            );
            assert!(
                xml.contains("inspect_file or terminal commands")
                    || xml.contains("inspect_file or terminal commands"),
                "must advise inspect_file or terminal: {xml}"
            );
        }
        other => panic!("expected Inlined::Text, got {other:?}"),
    }

    // Verify small image block: base64 inlined
    assert!(matches!(
        anthropic_parts[4],
        runeengine::media::Inlined::Bytes { .. }
    ));

    // Verify huge 25MB image on Anthropic: status="too_large" due to payload budget!
    match &anthropic_parts[6] {
        runeengine::media::Inlined::Text(xml) => {
            assert!(
                xml.contains("status=\"too_large\""),
                "must be status=too_large: {xml}"
            );
            assert!(
                xml.contains("payload limit"),
                "must state payload limit: {xml}"
            );
            assert!(
                !xml.contains("line"),
                "binary file must never mention lines: {xml}"
            );
        }
        other => panic!("expected Inlined::Text, got {other:?}"),
    }

    // ── Test Google Wire Shaping (Model Switching) ──
    let mut gemini_model = ModelInfo::named("gemini-2.5-flash", false);
    gemini_model.modalities = vec![
        "image".into(),
        "audio".into(),
        "video".into(),
        "file".into(),
    ];

    let google_parts = runeengine::media::parts_for_message(
        &all_attachments,
        "Check all attachments",
        Some(&gemini_model),
        Dialect::OpenAi,
        &runeengine::media::MediaLimits {
            max_payload_bytes: 100 * 1024 * 1024,
            ..Default::default()
        },
        true,
        0,
        &empty,
    );

    // On Google: 6MB image is NOT rejected (within Google's 70MB cap, no 5MB limit). It must be inlined as Bytes!
    assert!(
        matches!(google_parts[7], runeengine::media::Inlined::Bytes { .. }),
        "Google must inline 6MB image as Bytes successfully without status=too_large"
    );
    // Its files are tens of megabytes: a run must not leave them behind.
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn real_model_switching_with_attachments_resilience() {
    let tmp = std::env::temp_dir().join(format!("rune-test-switch-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    let img_path = tmp.join("test_shot.png");
    std::fs::write(
        &img_path,
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRreal_png_data",
    )
    .unwrap();
    let img_att = Attachment::at(&img_path).unwrap();

    let audio_path = tmp.join("voice_note.wav");
    std::fs::write(
        &audio_path,
        b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00real_wav",
    )
    .unwrap();
    let audio_att = Attachment::at(&audio_path).unwrap();

    let video_path = tmp.join("clip.mp4");
    std::fs::write(
        &video_path,
        b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00real_mp4_bytes",
    )
    .unwrap();
    let video_att = Attachment::at(&video_path).unwrap();

    let attachments = vec![img_att, audio_att, video_att];
    let empty = std::collections::HashSet::new();

    // ── Phase 1: Model X (Multimodal Gemini: image + audio + video) ──
    let mut model_x = ModelInfo::named("gemini-2.5-flash", false);
    model_x.modalities = vec![
        "image".into(),
        "audio".into(),
        "video".into(),
        "file".into(),
    ];

    let parts_x = runeengine::media::parts_for_message(
        &attachments,
        "Analyze these files",
        Some(&model_x),
        Dialect::OpenAi,
        &runeengine::media::MediaLimits {
            max_payload_bytes: 100 * 1024 * 1024,
            ..Default::default()
        },
        true,
        0,
        &empty,
    );

    // All 3 binary attachments must be inlined as Bytes
    assert!(
        matches!(
            parts_x[1],
            runeengine::media::Inlined::Bytes {
                modality: "image",
                ..
            }
        ),
        "Model X must inline image"
    );
    assert!(
        matches!(
            parts_x[4],
            runeengine::media::Inlined::Bytes {
                modality: "audio",
                ..
            }
        ),
        "Model X must inline audio"
    );
    assert!(
        matches!(
            parts_x[7],
            runeengine::media::Inlined::Bytes {
                modality: "video",
                ..
            }
        ),
        "Model X must inline video"
    );

    // ── Phase 2: User switches to Model Y (Text-Only: e.g. DeepSeek Coder / o1-mini) ──
    let mut model_y = ModelInfo::named("text-only-model", false);
    model_y.modalities = vec!["text".into()]; // No image, audio, or video!

    let parts_y = runeengine::media::parts_for_message(
        &attachments,
        "Analyze these files",
        Some(&model_y),
        Dialect::OpenAi,
        &runeengine::media::MediaLimits::default(),
        true,
        0,
        &empty,
    );

    // None can be inlined as Bytes — all must degrade safely to status="unsupported_by_model"
    for (idx, name) in [(0, "test_shot.png"), (1, "voice_note.wav"), (2, "clip.mp4")] {
        match &parts_y[idx] {
            runeengine::media::Inlined::Text(xml) => {
                assert!(
                    xml.contains("status=\"unsupported_by_model\""),
                    "Model Y must mark {name} as unsupported_by_model: {xml}"
                );
                assert!(
                    xml.contains("The active model cannot read this file type"),
                    "Must provide clear switch advice: {xml}"
                );
                assert!(
                    !xml.contains("base64"),
                    "{name} payload must NOT contain raw base64 bytes for text-only Model Y"
                );
            }
            other => panic!("expected Inlined::Text for {name} on Model Y, got {other:?}"),
        }
    }

    // ── Phase 3: User switches to Model Z (Anthropic Claude: image + pdf, NO audio/video) ──
    let mut model_z = ModelInfo::named("claude-3-5-sonnet", false);
    model_z.modalities = vec!["image".into(), "file".into()];

    let parts_z = runeengine::media::parts_for_message(
        &attachments,
        "Analyze these files",
        Some(&model_z),
        Dialect::Anthropic,
        &runeengine::media::MediaLimits {
            max_payload_bytes: 32 * 1024 * 1024,
            ..Default::default()
        },
        true,
        0,
        &empty,
    );

    // Image IS inlined:
    assert!(
        matches!(
            parts_z[1],
            runeengine::media::Inlined::Bytes {
                modality: "image",
                ..
            }
        ),
        "Claude must inline image"
    );

    // Audio & Video are rejected by Anthropic endpoint (status="unsupported_by_model"):
    match &parts_z[3] {
        runeengine::media::Inlined::Text(xml) => {
            assert!(
                xml.contains("status=\"unsupported_by_model\""),
                "Claude must mark audio as unsupported_by_model: {xml}"
            );
        }
        other => panic!("expected audio to be unsupported on Anthropic, got {other:?}"),
    }
    match &parts_z[4] {
        runeengine::media::Inlined::Text(xml) => {
            assert!(
                xml.contains("status=\"unsupported_by_model\""),
                "Claude must mark video as unsupported_by_model: {xml}"
            );
        }
        other => panic!("expected video to be unsupported on Anthropic, got {other:?}"),
    }

    // ── Phase 4: User switches BACK to Model X (Multimodal) ──
    let parts_back = runeengine::media::parts_for_message(
        &attachments,
        "Analyze these files",
        Some(&model_x),
        Dialect::OpenAi,
        &runeengine::media::MediaLimits {
            max_payload_bytes: 100 * 1024 * 1024,
            ..Default::default()
        },
        true,
        0,
        &empty,
    );
    assert!(
        matches!(
            parts_back[1],
            runeengine::media::Inlined::Bytes {
                modality: "image",
                ..
            }
        ),
        "Switching back to Model X must restore image inlining"
    );
    assert!(
        matches!(
            parts_back[4],
            runeengine::media::Inlined::Bytes {
                modality: "audio",
                ..
            }
        ),
        "Switching back to Model X must restore audio inlining"
    );
    assert!(
        matches!(
            parts_back[7],
            runeengine::media::Inlined::Bytes {
                modality: "video",
                ..
            }
        ),
        "Switching back to Model X must restore video inlining"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
