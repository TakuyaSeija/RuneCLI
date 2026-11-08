#!/usr/bin/env python3
"""Write the documentation's settings reference from three sources that must agree.

    python3 -I Website/Tools/build_settings_reference.py --bin <rune binary>

Every key comes from the application's own field table (Configuration.rs).
Its factory value is read three times: from that table, from the installed
binary (`rune -config explain <key>` in a throwaway home) and from the
official help's reference tables. One disagreement stops the build: the
page is never written from a single source. The explanation and the moment a
change applies are the binary's own words; accepted values come from the
official help. Nothing here reads the person's real settings or keys.
"""

import argparse
import html
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

STUDIO = Path(__file__).resolve().parents[2]
FIELDS = STUDIO / "Engine/src/settings/Configuration.rs"
HELP = STUDIO / ".rune/skills/rune-settings-guide"
OUT = STUDIO / "Website/UI/Pages/Docs/Reference.html"

# Tables in reading order: (heading, anchor, article that explains them, tables).
GROUPS = [
    ("Terminal", "terminal", "docs/terminal", ["terminal"]),
    ("Background processes", "processes", "docs/terminal", ["process_control"]),
    ("Files and search", "files", "docs/tools", ["inspect_file", "find_files", "search_text", "scan"]),
    ("Tool calls, plans and questions", "tool-calls", "docs/tools", ["tools", "tasks", "questions", "permissions"]),
    ("Web search and fetch", "web", "docs/web", ["web_search", "web_fetch"]),
    ("Skills and MCP", "capabilities", "docs/mcp", ["skills", "capabilities", "discover_capabilities", "read_mcp_resource"]),
    ("Network, retries and session lifetime", "runtime", "docs/runtime", ["network", "retry", "host"]),
    ("Generation", "generation", "docs/generation", ["generation"]),
    ("Classifier", "classifier", "docs/classifier", ["classifier"]),
    ("Auxiliary jobs", "auxiliary", "docs/auxiliary", ["sanitizer", "compact", "title", "auxiliary"]),
    ("Undo and file snapshots", "undo", "docs/undo", ["checkpoint"]),
    ("Attachments, instructions and memory", "context-files", "docs/instructions", ["attachments", "instructions", "memory"]),
    ("Storage and spend", "storage", "docs/sessions", ["storage", "usage"]),
    ("Display", "display", "docs/appearance", ["ui"]),
]

# The public site never names these (Server_test.go TestPublicContentRevealsNoInternals).
DENY = ["perm::", "write_guard", "toolctx", "net::", "proto::", "engine/", "cli/", "crate", "cargo", "tokio",
        "apparmor", "sqlite", "jsonl", "ledger", "arc-swap", "pbkdf", "argon", "rune.db", "master.key",
        "secrets.vault", "claude.md", "upgrade/v", "protocol ", "schema", "/home/", "localhost", "github"]


def fail(message):
    sys.exit("build_settings_reference: " + message)


def fields():
    src = FIELDS.read_text(encoding="utf-8")
    start = src.index("pub const FIELDS")
    end = src.index("];", start)
    rows = re.findall(r'key\s*:\s*"([^"]+)"\s*,\s*default\s*:\s*"((?:\\.|[^"\\])*)"', src[start:end])
    if len(rows) < 100:
        fail(f"read only {len(rows)} fields; the table moved")
    return [(key, json.loads('"' + raw + '"')) for key, raw in rows]


def fixed():
    """The keys that are Rune's own constants, not settings (owner, 2026-11-08).

    They stay in the field table, but the page documents only what a person may
    change: a value that cannot be set must not be published as if it could.
    """
    src = FIELDS.read_text(encoding="utf-8")
    start = src.index("pub const FIXED")
    end = src.index("];", start)
    return set(re.findall(r'"([^"]+)"', src[start:end]))


def help_tables():
    out = {}
    for path in sorted(HELP.rglob("*.md")):
        text = path.read_text(encoding="utf-8")
        for section in re.split(r"\n## ", text):
            if "| Global key | Factory value | Accepted value / note |" not in section:
                continue
            for keys, value, note in re.findall(r"^\| (`[^|]+`) \| (.*?) \| (.*?) \|$", section, re.M):
                for key in re.findall(r"`([^`]+)`", keys):
                    out[key] = (value.strip().strip("`"), note.strip())
    return out


def help_purposes():
    """The official help's own one-line purpose of each key, from its settings tables."""
    out = {}
    for path in sorted(HELP.rglob("*.md")):
        for keys, rest in re.findall(r"^\| (`[^|]+`) \|([^\n]*)\|$", path.read_text(encoding="utf-8"), re.M):
            cells = [c.strip() for c in rest.split("|")]
            if len(cells) >= 2 and cells[-1]:
                for key in re.findall(r"`([^`]+)`", keys):
                    out.setdefault(key, cells[-1])
    return out


def explain(binary, key, home):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, "RUNE_HOME": os.path.join(home, "rune"),
           "NO_COLOR": "1", "LANG": "C.UTF-8"}
    run = subprocess.run([binary, "-config", "explain", key], env=env, capture_output=True, text=True, timeout=30)
    lines = [l for l in run.stdout.splitlines() if l.strip()]
    if run.returncode != 0 or len(lines) < 3 or not lines[0].startswith(key + " = "):
        fail(f"explain {key}: {run.stdout!r} {run.stderr!r}")
    timing = re.search(r"Timing: (.*)$", lines[-1])
    if not timing:
        fail(f"explain {key}: no timing line")
    return lines[0][len(key) + 3:], " ".join(lines[1:-1]), timing.group(1).rstrip(".")


def same(a, b):
    """Two spellings of one TOML value: quotes, spacing and case of words aside.
    TOML has no null, so the help writes an unset default as inherited or unset."""
    norm = lambda v: re.sub(r"\s+", "", v).strip('"').lower()
    if norm(b) == "null":
        return norm(a) == "null" or "inherit" in norm(a) or "unset" in norm(a)
    return norm(a) == norm(b)


def inline(text):
    """Help text is markdown: keep its code spans, drop its links, escape the rest."""
    text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)
    parts = re.split(r"(`[^`]+`)", text)
    return "".join("<code>" + html.escape(p[1:-1]) + "</code>" if p.startswith("`") else html.escape(p) for p in parts)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True, help="the rune binary the page documents")
    args = parser.parse_args()
    version = subprocess.run([args.bin, "--version"], capture_output=True, text=True, timeout=30).stdout.split()
    if len(version) != 2 or version[0] != "rune":
        fail("cannot read the binary's version")
    version = version[1]

    constants = fixed()
    rows, notes, purposes = fields(), help_tables(), help_purposes()
    documented = [(key, default) for key, default in rows if key not in constants]
    if len(documented) + len(constants) != len(rows):
        fail("a fixed key is missing from the field table")
    rows = documented
    reworded = []
    tables = {table for group in GROUPS for table in group[3]}
    home = tempfile.mkdtemp(prefix="settings-reference-", dir=STUDIO / "target" / "test-tmp" if (STUDIO / "target" / "test-tmp").is_dir() else None)
    try:
        facts = {}
        for key, default in rows:
            if key.split(".")[0] not in tables:
                fail(f"{key}: no group on the page; add its table to GROUPS")
            if key not in notes:
                fail(f"{key}: the official help has no reference row")
            value, meaning, timing = explain(args.bin, key, home)
            help_value, accepted = notes[key]
            if not same(value, default) or not same(help_value, default):
                fail(f"{key}: factory value disagrees — source {default!r}, binary {value!r}, help {help_value!r}")
            # The program's words, unless they name an internal the public site never shows;
            # then the official help's user-facing purpose of the same key.
            if any(term in meaning.lower() for term in DENY):
                if key not in purposes:
                    fail(f"{key}: its explanation names an internal and the help has no purpose row")
                meaning = re.sub(r"`([^`]+)`", r"\1", purposes[key])
                reworded.append(key)
            facts[key] = (value, meaning, timing, accepted)
    finally:
        shutil.rmtree(home, ignore_errors=True)

    out = [
        "{{define \"article\"}}",
        f"<!-- Generated from Rune {version} by Website/Tools/build_settings_reference.py: the source's field table, the binary and the official help agree on every factory value. Do not edit by hand. -->",
        f"<p>Every setting you can change in Rune {html.escape(version)}, grouped the way <code>rune.toml</code> groups them: a row in Settings or a key in that file. Each entry gives the factory value, what it does in the program's own words, the values it accepts and when a change takes effect. Rune's own constants — the values that keep it running — are not settings, cannot be changed and are not listed here.</p>",
        "<p>A factory value is what applies until you choose otherwise; your own choices show with <code>rune -config explain &lt;key&gt;</code>. Reset a choice with <code>rune -config reset &lt;key&gt;</code>. Durations and sizes need their units, such as <code>\"2m\"</code> or <code>\"16MiB\"</code>; see <a href=\"{{url \"docs/configuration\"}}\">Configuration file</a> for value formats.</p>",
    ]
    for title, anchor, route, group_tables in GROUPS:
        keys = [k for k, _ in rows if k.split(".")[0] in group_tables]
        if not keys:
            fail(f"group {title} is empty")
        out.append(f'<h2 id="{anchor}">{html.escape(title)}</h2>')
        out.append(f'<p>Explained in <a href="{{{{url "{route}"}}}}">{html.escape(ARTICLES[route])}</a>.</p>')
        out.append('<dl class="ref ref-keys">')
        for key in keys:
            value, meaning, timing, accepted = facts[key]
            out.append(
                f'<div id="{key}"><dt><code>{html.escape(key)}</code></dt><dd><p>{html.escape(meaning)}</p>'
                f'<p class="ref-facts"><span><b>Default</b> {"not set" if value == "null" else "<code>" + html.escape(value) + "</code>"}</span>'
                f'<span><b>Accepts</b> {inline(accepted)}</span>'
                f'<span><b>Takes effect</b> {html.escape(timing)}</span></p></dd></div>')
        out.append("</dl>")
    out.append("{{end}}")
    page = "\n".join(out) + "\n"
    low = page.lower()
    for term in DENY:
        if term in low:
            fail(f"generated text names {term!r}, which the public site never shows; reword its source")
    if re.search(r"\b\d{1,3}(?:\.\d{1,3}){3}\b", page):
        fail("generated text contains a machine address")
    if "{{" in page.replace("{{define \"article\"}}", "").replace("{{end}}", "").replace("{{url ", ""):
        fail("generated text contains template markup")
    OUT.write_text(page, encoding="utf-8")
    print(f"wrote {OUT.relative_to(STUDIO)}: {len(rows)} settings in {len(GROUPS)} groups, Rune {version}")
    if reworded:
        print("explained in the official help's words (the binary's names an internal): " + ", ".join(reworded))


ARTICLES = {
    "docs/terminal": "Terminal & background work",
    "docs/tools": "Tools & limits",
    "docs/web": "Web search & fetch",
    "docs/mcp": "MCP connections",
    "docs/runtime": "Runtime & retries",
    "docs/generation": "Generation & reasoning",
    "docs/classifier": "Classifier policy",
    "docs/auxiliary": "Auxiliary models",
    "docs/undo": "Review, undo & redo",
    "docs/instructions": "Instructions & attachments",
    "docs/sessions": "Sessions & context",
    "docs/appearance": "Appearance",
}

if __name__ == "__main__":
    main()
