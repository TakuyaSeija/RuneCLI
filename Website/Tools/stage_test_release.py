#!/usr/bin/env python3
"""Stage a signed TEST release for the installers' end-to-end checks.

    python3 -I Website/Tools/stage_test_release.py --version X.Y.Z --out DIR [--license FILE] \\
        [--build linux-x86_64-gnu=PATH] [--build macos-aarch64=PATH] [--build windows-x86_64=PATH] ...

Lays out DIR exactly as Ops/Release/PublishRune.sh lays out the website's
downloads folder (SigningKey.pem, <version>/release.json, archives,
SHA256SUMS, SHA256SUMS.sig), so the real website server lists it and the
real installers verify and install it. The signing key is made for this run
only, with openssl, and dropped when the release is written: it signs
nothing else and is never the release key. CI uses this to install the
binaries it built through install.sh and install.ps1 on each system.
"""

import argparse
import datetime
import gzip
import hashlib
import io
import json
import os
import pathlib
import re
import subprocess
import sys
import tarfile
import tempfile
import zipfile

PLATFORM = re.compile(r"^(linux-(x86_64|aarch64)-(gnu|musl)|macos-(x86_64|aarch64)|windows-(x86_64|aarch64))$")
VERSION = re.compile(r"^(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})\.(0|[1-9][0-9]{0,3})$")
EPOCH = 1792800000  # a fixed time for archive entries: the bytes do not depend on when the test ran
NOTICE = "Test release staged for the installers' end-to-end checks; not a published build.\n"


def tar_gz(program: bytes, license_text: bytes) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for name, body, mode in (("rune", program, 0o755), ("LICENSE.txt", license_text, 0o644),
                                 ("THIRD-PARTY-NOTICES.txt", NOTICE.encode(), 0o644)):
            info = tarfile.TarInfo(name)
            info.size, info.mode, info.mtime, info.uid, info.gid = len(body), mode, EPOCH, 0, 0
            tar.addfile(info, io.BytesIO(body))
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=EPOCH) as gz:
        gz.write(raw.getvalue())
    return out.getvalue()


def zip_of(program: bytes, license_text: bytes) -> bytes:
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        for name, body in (("rune.exe", program), ("LICENSE.txt", license_text), ("THIRD-PARTY-NOTICES.txt", NOTICE.encode())):
            info = zipfile.ZipInfo(name, date_time=datetime.datetime.fromtimestamp(EPOCH, datetime.timezone.utc).timetuple()[:6])
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, body)
    return out.getvalue()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--license", help="license text to ship; a test placeholder when omitted")
    parser.add_argument("--build", action="append", default=[], help="PLATFORM=PATH, e.g. macos-aarch64=target/release/rune")
    args = parser.parse_args()
    if not VERSION.match(args.version):
        sys.exit("stage_test_release: --version must look like 1.2.3")
    if not args.build:
        sys.exit("stage_test_release: at least one --build is needed")
    out = pathlib.Path(args.out)
    release = out / args.version
    if release.exists():
        sys.exit(f"stage_test_release: {release} exists")
    release.mkdir(parents=True)
    license_text = pathlib.Path(args.license).read_bytes() if args.license else b"Test release: the published license comes with published builds only.\n"

    files, sums = [], []
    for spec in args.build:
        platform, _, path = spec.partition("=")
        m = PLATFORM.match(platform)
        if not m or not path:
            sys.exit(f"stage_test_release: bad --build {spec!r}")
        program = pathlib.Path(path).read_bytes()
        system = platform.split("-")[0]
        if system == "windows":
            name, body = f"rune-{args.version}-{platform}.zip", zip_of(program, license_text)
        else:
            name, body = f"rune-{args.version}-{platform}.tar.gz", tar_gz(program, license_text)
        (release / name).write_bytes(body)
        digest = hashlib.sha256(body).hexdigest()
        arch = m.group(2) or m.group(4) or m.group(5)
        files.append({"name": name, "os": system, "arch": arch, "libc": m.group(3) or "", "size": len(body), "sha256": digest})
        sums.append(f"{digest}  {name}\n")

    meta = json.dumps({"version": args.version, "date": datetime.date.today().isoformat(),
                       "notes": ["Test release for the installers' end-to-end checks."], "files": files}, indent=2).encode() + b"\n"
    (release / "release.json").write_bytes(meta)
    sums.append(f"{hashlib.sha256(meta).hexdigest()}  release.json\n")
    # Bytes, not text: on Windows text mode would end the lines with CRLF, which the
    # publisher never writes.
    (release / "SHA256SUMS").write_bytes("".join(sums).encode("ascii"))

    with tempfile.TemporaryDirectory() as keydir:
        key = os.path.join(keydir, "key.pem")
        run = lambda *cmd: subprocess.run(cmd, check=True, capture_output=True)
        run("openssl", "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", key)
        run("openssl", "ec", "-in", key, "-pubout", "-out", str(out / "SigningKey.pem"))
        run("openssl", "dgst", "-sha256", "-sign", key, "-out", str(release / "SHA256SUMS.sig"), str(release / "SHA256SUMS"))
    check = subprocess.run(["openssl", "dgst", "-sha256", "-verify", str(out / "SigningKey.pem"), "-signature",
                            str(release / "SHA256SUMS.sig"), str(release / "SHA256SUMS")], capture_output=True, text=True)
    if check.returncode != 0:
        sys.exit("stage_test_release: the staged signature does not verify: " + check.stdout + check.stderr)
    print(f"staged test release {args.version}: " + ", ".join(f["name"] for f in files))


if __name__ == "__main__":
    main()
