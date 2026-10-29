#!/usr/bin/env python3
"""Build the public replay from one session, without truncating its source.

Usage: build_session_data.py <session dir> <rune.db> <out.json>
The database is opened read-only. No provider is called. Public redaction keeps
rows in place; folding is the renderer's job, never irreversible data loss.
"""
import json
from datetime import datetime
from render_source import SourceRenderer
import os
import re
import sqlite3
import sys
from pathlib import Path

ADDRESS = re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}\b")
ABSOLUTE = re.compile(r"/(?:home|tmp|var|run)/[^\s()'\"`<>]+")
LOCAL_URL = re.compile(r"https?://(?:localhost|127\.0\.0\.1|0\.0\.0\.0)(?::\d+)?", re.I)
LOCAL_PORT = re.compile(r"(?<!\d):\d{4,5}\b")
DENY = [
    "perm::", "write_guard", "toolctx", "net::", "proto::", "engine/", "cli/",
    "crate", "cargo", "tokio", "apparmor", "sqlite", "jsonl", "ledger",
    "arc-swap", "pbkdf", "argon", "rune.db", "master.key", "secrets.vault",
    "claude.md", "upgrade/v", "protocol ", "schema", "/home/", "noire",
    "deploy/", "ops/", "localhost", ":3401", ":3501", "3403", "3503",
    "nginx", "cloudflared", "systemd", "seadrive", "github", "takuya",
    "innerhtml", "eval(", "document.write", "style=", "onclick", "onload=",
    "javascript:", ".rune", "rune_home", "classifier.md", "agents.md", "9router",
]
PRIVATE = re.compile("|".join(re.escape(x) for x in sorted(DENY, key=len, reverse=True)), re.I)


def clean(text):
    text = (text or "").replace("/home/noire/Project/Music", "~/Project/Music")
    text = LOCAL_URL.sub("https://example.invalid", text)
    text = LOCAL_PORT.sub(":…", text)
    text = ABSOLUTE.sub("…", text)
    text = ADDRESS.sub("…", text)
    return PRIVATE.sub("…", text)


def lines(text):
    # Like Rust str::lines: no phantom extra line for a final newline.
    return clean(text).splitlines()


def model_context(db, model):
    matches = []
    for row in db.execute("SELECT info FROM provider_models WHERE id=?", (model,)):
        value = json.loads(row[0]).get("context")
        if isinstance(value, int) and value > 0:
            matches.append(value)
    if len(set(matches)) != 1:
        raise ValueError("the exact recorded model needs one unambiguous saved context capacity")
    return matches[0]


def notifications(events):
    # Context notices are not transcript notifications. Only the admitted,
    # genuine background wake is drawn by the CLI, at this immutable row's time.
    opening, closing = '<system_notification>', '</system_notification>'
    result = []
    for event in events:
        content = (event.get('content') or '').strip()
        if event.get('role') == 'context' and content.startswith(opening) and content.endswith(closing):
            result.append({'at': event['created_at'], 'text': clean(content[len(opening):-len(closing)].strip())})
    return result


def build(session_dir, db, renderer):
    session_dir = Path(session_dir)
    read = lambda name: [json.loads(x) for x in (session_dir / name).read_text().splitlines()]
    events = read("session.jsonl")
    calls_meta = {c["id"]: c for c in read("toolcalls.jsonl")}
    meta = json.loads((session_dir / "meta.json").read_text())
    generation = meta["settings"]
    if isinstance(generation, str):
        generation = json.loads(generation)
    tasks = json.loads((session_dir / "tasks.json").read_text())
    db.row_factory = sqlite3.Row
    usage = list(db.execute(
        "SELECT * FROM token_usage WHERE session_id=? AND role='agent' ORDER BY timestamp,id", (meta["id"],)))
    files = list(db.execute(
        "SELECT path,added,removed,change,last_at FROM session_files WHERE session_id=? ORDER BY last_at,path", (meta["id"],)))
    turns, used, steps = [], 0, 0
    cum_in = cum_out = cum_cache = 0
    cum_cost = 0.0
    for i, event in enumerate(events):
        if event.get("role") != "assistant":
            continue
        outputs = {}
        for result in events[i + 1:]:
            if result.get("role") != "tool":
                break
            outputs[result["tool_call_id"]] = result.get("content") or ""
        if used >= len(usage):
            raise ValueError("missing accounting row: do not fabricate request figures")
        row = usage[used]
        used += 1
        cum_in += row["tok_in"]
        cum_out += row["tok_out"]
        cum_cache += row["cache_in"]
        cum_cost += row["cost"]
        calls = []
        for call in event.get("tool_calls") or []:
            args = json.loads(call["args"])
            cm = calls_meta[call["id"]]
            item = {"n": call["name"], "a": clean(args.get("action")), "ok": cm["ok"],
                    "out": lines(outputs.get(call["id"], "")),
                    "ms": max(0, cm["ended_at"] - cm["started_at"]),
                    "started_at": cm["started_at"], "ended_at": cm["ended_at"]}
            for key, field in (("p", "path"), ("cmd", "command"), ("q", "query")):
                if args.get(field):
                    item[key] = clean(args[field])
            if args.get("process_id"):
                item["task"] = args["process_id"]
            if args.get("view_range"):
                item["rng"] = args["view_range"]
            if call["name"] == "terminal_use":
                command = (item.get("cmd") or "").split("\n")
                tokens = []
                for index, line in enumerate(command):
                    if index:
                        tokens.append({"Class": "", "Text": "\n"})
                    tokens.extend(renderer.tokens("bash", line))
                item["command_tokens"] = tokens
            if call["name"] == "create_file":
                item["body"] = lines(args.get("text"))
                item["total"] = len(item["body"])
                language = Path(item.get("p", "")).suffix.lstrip(".") or "code"
                item["syntax"] = [renderer.tokens(language, line) for line in item["body"]]
            elif call["name"] == "patch_file":
                old, new = args.get("old_str") or "", args.get("new_str") or ""
                item.update(rem=len(old.splitlines()), add=len(new.splitlines()), diff=renderer.diff(clean(old), clean(new)))
            elif call["name"] == "track_tasks":
                item["tasks"] = [{"t": clean(t["text"]), "s": t.get("status", "todo")} for t in args.get("tasks") or []]
            calls.append(item)
        steps += len(calls)
        turns.append({"at": event["created_at"], "accounted_at": row["timestamp"], "text": clean(event.get("content")),
                      "reasoning": clean(event.get("reasoning")), "words": len((event.get("reasoning") or "").split()),
                      "ctx": row["context_used"], "cum_in": cum_in, "cum_out": cum_out,
                      "cum_cache": cum_cache, "cum_cost": round(cum_cost, 4), "steps": steps, "calls": calls})
    if used != len(usage):
        raise ValueError("unmatched accounting rows")
    return {"title": clean(meta["title"]), "started": datetime.fromtimestamp(meta["created_at"] / 1000).strftime("%H:%M"), "active": meta["active_seconds"],
            "start_at": next(e["created_at"] for e in events if e.get("role") == "user"),
            "model": meta["model"].split("/", 1)[-1], "effort": generation["effort"], "mode": "ask", "recorded_mode": meta["mode"],
            "context": model_context(db, meta["model"]), "context_source": "Saved model settings",
            "palette": {role: color for _, role, color in renderer.request("palette")}, 
            "project": "~/Project/Music", "prompt": clean(next(e["content"] for e in events if e.get("role") == "user")),
            "response": turns[-1]["text"], "notifications": notifications(events), "stream_timing": "request_boundaries_only",
            "tasks": [{"t": clean(t["text"]), "s": t["status"]} for t in tasks],
            "files": [{"p": clean(f["path"]), "a": f["added"], "r": f["removed"], "c": f["change"], "at": f["last_at"]} for f in files],
            "turns": turns, "final": {"ctx": turns[-1]["ctx"], "in": cum_in, "out": cum_out, "cache": cum_cache,
                                       "cost": round(cum_cost, 4), "steps": steps, "turns": 1, "requests": len(turns), "files": len(files)}}


def text_values(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from text_values(item)
    elif isinstance(value, list):
        for item in value:
            yield from text_values(item)


def main(session_dir, db_path, out_path):
    with SourceRenderer() as renderer, sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True) as db:
        data = build(session_dir, db, renderer)
    raw = json.dumps(data, ensure_ascii=False, separators=(",", ":"))
    public_text = "\n".join(text_values(data))
    survived = sorted({term for term in DENY if term in public_text.lower()})
    if survived or ADDRESS.search(public_text):
        raise ValueError(f"private content survived redaction: {survived}")
    Path(out_path).parent.mkdir(parents=True, exist_ok=True)
    Path(out_path).write_text(raw, encoding="utf-8")
    print(f"{len(data['turns'])} requests, {data['final']['steps']} calls, {len(data['files'])} files; complete public recording -> {out_path}")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    main(*sys.argv[1:])
