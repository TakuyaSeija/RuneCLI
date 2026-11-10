#!/usr/bin/env python3
"""Derive the site's brand files from the owner's originals in `Website/Brand/`.

    python3 -I Website/Tools/build_brand_assets.py [--check]

The originals are the three 256 px renders the owner supplied on 2026-10-31:
the mark alone (transparent), the same mark on a dark plate, and the same mark
on a light plate. They are kept unmodified here as the source of every served
file; `Brand/` is not embedded in the site binary, so only the derived copies in
`UI/Static/Images/` reach a visitor. The light plate is kept for a future light
surface; today the site is dark-only and nothing uses it.
The site never re-colours them. The header and footer use the transparent mark
at 26 and 20 px. Browser tabs use the transparent mark across four crisp
downscales (128, 96, 32, and 16 px) so the icon sits cleanly and seamlessly on
any browser tab without a dark box. The iOS touch icon retains the dark plate
because iOS rounds corners and fills transparent areas with black on the home
screen.

`--check` writes nothing and reports whether the files on disk are exactly what
this script produces (byte for byte), so a hand-edited asset is caught.

ImageMagick (`magick` or `convert`) is required; the website's Go tests do not
run this script, they hold the result's shape (`Server/Brand_test.go`).
"""

import argparse
import hashlib
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
BRAND = ROOT / "Brand"
IMAGES = ROOT / "UI" / "Static" / "Images"

MASTER = "Logo-transparent.png"
PLATE = "Logo-black.png"

# name, source, size, what it is for
DERIVED = [
    ("Logo.png", MASTER, 256, "header and footer mark, shown at 26 and 20 px (owner transparent original)"),
    ("Icon-128.png", MASTER, 128, "tab icon, extra HD (128 px integer downscale)"),
    ("Icon.png", MASTER, 96, "tab icon for large/high-density tabs"),
    ("Icon-32.png", MASTER, 32, "tab icon, standard"),
    ("Icon-16.png", MASTER, 16, "tab icon, small"),
]


def tool():
    for name in ("magick", "convert"):
        found = shutil.which(name)
        if found:
            return [found]
    sys.exit("build_brand_assets: ImageMagick is required (magick or convert)")


def render(convert, source, size, target):
    """One square PNG, metadata stripped, no colour change, exact size."""
    subprocess.run(
        [*convert, str(BRAND / source), "-filter", "Lanczos", "-resize", f"{size}x{size}",
         "-strip", "-depth", "8", "-define", "png:compression-level=9", str(target)],
        check=True,
    )


def touch_icon(convert, target):
    """iOS rounds the corners itself: a full-bleed plate with the mark at 78%.

    The plate is the owner's own dark one (#0f1115), sampled from the file, so
    the home-screen icon and the tab icon sit on the same ground. 8-bit on
    purpose: a 16-bit PNG triples the bytes for a 180 px icon.
    """
    subprocess.run(
        [*convert, "-size", "180x180", "xc:#0f1115",
         "(", str(BRAND / MASTER), "-resize", "140x140", "-strip", ")",
         "-gravity", "center", "-composite", "-strip", "-depth", "8",
         "-define", "png:compression-level=9", str(target)],
        check=True,
    )


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true",
                        help="report differences instead of writing")
    args = parser.parse_args()
    convert = tool()

    planned = [(name, source, size, why) for name, source, size, why in DERIVED]
    planned.append(("AppleTouchIcon.png", MASTER, 180, "home-screen icon"))

    missing = [p for p in (BRAND / MASTER, BRAND / PLATE) if not p.is_file()]
    if missing:
        sys.exit("build_brand_assets: missing originals: " + ", ".join(map(str, missing)))

    changed = []
    for name, source, size, why in planned:
        target = IMAGES / name
        before = digest(target) if target.is_file() else None
        if args.check:
            scratch = IMAGES / (".check-" + name)
            try:
                if name == "AppleTouchIcon.png":
                    touch_icon(convert, scratch)
                else:
                    render(convert, source, size, scratch)
                if digest(scratch) != before:
                    changed.append(f"{name}: on disk differs from {source} at {size}px")
            finally:
                scratch.unlink(missing_ok=True)
            continue
        if name == "AppleTouchIcon.png":
            touch_icon(convert, target)
        else:
            render(convert, source, size, target)
        after = digest(target)
        if after != before:
            changed.append(f"{name}: {source} at {size}px -> {after[:16]}")
        print(f"{name:20} {size:>3}px  {why}")

    if args.check:
        if changed:
            print("build_brand_assets: " + "; ".join(changed))
            return 1
        print("build_brand_assets: every derived file matches its original")
        return 0
    print("wrote", len(planned), "files into", IMAGES.relative_to(ROOT))
    for line in changed:
        print("  " + line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
