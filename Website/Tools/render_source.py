"""Offline adapter for the CLI's pure diff and lexer, compiled with rustc.

Only serialization derives / unused paint entry points are removed. The actual
algorithms are read from their authoritative files, never copied or translated.
The tiny text transport needs no dependency and never touches a Rune home.
"""
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
INKS = ["BASE", "CODE", "CODE_COMMENT", "CODE_FUNC", "CODE_KEYWORD", "CODE_NUMBER", "CODE_STRING", "CODE_TYPE"]
CLASSES = ["", "", "syntax-comment", "syntax-function", "syntax-keyword", "syntax-number", "syntax-string", "syntax-type"]


class SourceRenderer:
    def __init__(self):
        scratch = ROOT / "target/test-tmp"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="website-source-", dir=scratch)
        folder = Path(self.temp.name)
        diff = (ROOT / "Engine/src/checkpoint/diff.rs").read_text()
        diff = diff.replace("use serde::Serialize;", "").replace(", Serialize", "").replace('#[serde(rename_all = "lowercase")]', "")
        # The lexer returns semantic ink, not terminal escape sequences. Its
        # paint/wrap entry points are irrelevant to the HTML renderer.
        lexer = (ROOT / "Cli/src/ui/Highlight.rs").read_text()
        lexer = lexer[lexer.index("#[derive(Debug, Clone, Copy, PartialEq, Eq)]"):lexer.index("/// Colour the part of")]
        palette = "type Ink=u8;" + "".join(f"const {name}:Ink={i};" for i, name in enumerate(INKS))
        (folder / "diff.rs").write_text(diff)
        (folder / "lexer.rs").write_text(palette + "\n" + lexer)
        ansi = (ROOT / "Cli/src/ui/ansi.rs").read_text()
        role = ansi[ansi.index("#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Role"):ansi.index("/// A role, ready to be written")]
        theme = (ROOT / "Cli/src/ui/theme.rs").read_text()
        mode = theme[theme.index("#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]"):theme.index("impl ThemeMode")]
        spec = theme[theme.index("const PIVOT:"):theme.index("/// Solve every role")]
        solve = theme[theme.index("fn solve("):theme.index("// ─── Asking")]
        arithmetic = theme[theme.index("pub(super) fn luminance"):theme.index("#[cfg(test)]\nmod tests")]
        (folder / "palette.rs").write_text("#[derive(Clone,Copy)] struct Rgb(u8,u8,u8);\n" + role + mode + spec + solve + arithmetic +
            '\npub fn print(name:&str){let mode=match name{"violet"=>ThemeMode::Violet,"cyan"=>ThemeMode::Cyan,_=>ThemeMode::Amber};let bg=Rgb(11,12,15); for role in Role::ALL {let c=solve(bg,&spec_of_mode(role,mode));println!("p\\t{:?}\\t#{:02x}{:02x}{:02x}",role,c.0,c.1,c.2);}}')
        main = r'''
#![allow(dead_code)]
mod diff;
mod lexer;
mod palette;
use std::io::{self,BufRead,Write};
fn decode(s:&str)->String {String::from_utf8((0..s.len()).step_by(2).map(|i|u8::from_str_radix(&s[i..i+2],16).unwrap()).collect()).unwrap()}
fn encode(s:&str)->String {s.as_bytes().iter().map(|b|format!("{b:02x}")).collect()}
fn main(){
 for line in io::stdin().lock().lines(){
  let line=line.unwrap();let fields:Vec<_>=line.split('\t').collect();
  match fields[0]{
   "diff"=>{for h in diff::hunks(&decode(fields[1]),&decode(fields[2]),diff::MAX_CELLS,diff::CONTEXT){for row in h.lines{let op=match row.op{diff::Op::Same=>" ",diff::Op::Del=>"-",diff::Op::Add=>"+"};println!("d\t{op}\t{}",encode(&row.text));}}},
   "lex"=>{let source=decode(fields[2]);for p in lexer::pieces(fields[1],&source,0){println!("l\t{}\t{}\t{}",p.start,p.end,p.ink);}},
   "palette"=>palette::print(fields.get(1).copied().unwrap_or("amber")),
   _=>panic!("bad operation"),
  }
  println!(".");io::stdout().flush().unwrap();
 }
}
'''
        (folder / "main.rs").write_text(main)
        binary = folder / "source-renderer"
        subprocess.run(["rustc", "--edition=2021", "-O", str(folder / "main.rs"), "-o", str(binary)], check=True, capture_output=True)
        self.process = subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

    def request(self, *fields):
        self.process.stdin.write("\t".join(fields) + "\n")
        self.process.stdin.flush()
        rows = []
        while True:
            line = self.process.stdout.readline().rstrip("\n")
            if line == ".":
                return rows
            if not line:
                raise RuntimeError("source renderer closed unexpectedly")
            rows.append(line.split("\t"))

    def diff(self, before, after):
        return [[op, bytes.fromhex(value).decode()] for _, op, value in self.request("diff", before.encode().hex(), after.encode().hex())]

    def tokens(self, language, source):
        data = source.encode()
        return [{"Class": CLASSES[int(ink)], "Text": data[int(start):int(end)].decode()}
                for _, start, end, ink in self.request("lex", language, data.hex())]

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=5)
        self.temp.cleanup()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()
