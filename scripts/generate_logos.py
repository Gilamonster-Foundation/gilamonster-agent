#!/usr/bin/env python3
"""Generate gila's terminal splash art — a solid gold gilamonster silhouette.

gila inherits newt-agent's TUI, whose splash art is brandable at runtime via the
brand seam (NEWT_BRAND_LOGO_DIR / NEWT_BRAND_LOGO_PREFIX; see newt-agent#355).
This produces the `<prefix>-<stem>.txt` ANSI art the seam loads, so `gila code`
shows the gilamonster instead of the newt.

Why a silhouette, not a photo reduction: rendering the full-color mascot through
chafa's braille is muddy — braille keys on per-dot luminance, so the mascot's
dark internal detail punches holes and only an outline survives. Instead we trace
the source *alpha mask* (chafa `--colors=none`) for a clean, solid shape, then
colorize it gold over newt's dark cell background.

Sources are pinned in-script (SMALL_SRC / LARGE_SRC) so a run reproduces the
committed art exactly: small sizes from the simple mascot mark, the larger
splashes from the high-res hero render.

Usage:
    ~/venv/bin/python scripts/generate_logos.py

Requires: Pillow + numpy and chafa on PATH (brew install chafa).
"""
from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
LOGO_DIR = ROOT / "docs" / "logos"
PREFIX = "gilly"             # NEWT_BRAND_LOGO_PREFIX gila sets at startup

FILL = (20, 20, 20)          # newt's dark cell background (#141414)
GOLD = (235, 195, 70)        # gilamonster gold — the silhouette ink
ALPHA_CUTOFF = 96            # alpha >= this counts as subject for the crop bbox
CROP_PAD = 0.04              # padding around the subject bbox (fraction)
THRESHOLD = "0.5"           # chafa luminance midpoint for the 1-bit mask

# Two sources, by size (cols, rows). Rows are newt-tui's per-logo height budget
# in logo_for_size(); chafa fits within the box so width binds. The small sizes
# read as a clean blob at any res, so they keep the simple mascot mark; the
# larger splashes carry real detail, so they use the high-res hero render.
SMALL_SRC = LOGO_DIR / "gilly-512.png"
LARGE_SRC = LOGO_DIR / "gilamonster_logo_source.png"
SMALL_SIZES = {"10": (10, 5), "20": (20, 10)}
LARGE_SIZES = {"40": (40, 20), "full": (80, 40), "120": (126, 61), "160": (166, 81)}
PLAIN_COLS, PLAIN_ROWS = 40, 20   # <prefix>-ascii-40.txt (LOGO_PLAIN, monochrome; from LARGE_SRC)
CURSOR_RE = re.compile(r"\x1b\[\?\d+[hl]")
SGR_RE = re.compile(r"\x1b\[[0-9;]*m")


def subject_mask(src: Path) -> Path:
    """Crop the source to its subject (by alpha) and return a square grayscale
    mask (subject bright on black) for chafa to trace."""
    img = Image.open(src).convert("RGBA")
    alpha = np.asarray(img)[..., 3]
    ys, xs = np.where(alpha > ALPHA_CUTOFF)
    if len(xs):
        px = int((xs.max() - xs.min()) * CROP_PAD)
        py = int((ys.max() - ys.min()) * CROP_PAD)
        img = img.crop((max(0, xs.min() - px), max(0, ys.min() - py),
                        min(img.width, xs.max() + px), min(img.height, ys.max() + py)))
    side = max(img.size)
    canvas = Image.new("L", (side, side), 0)
    canvas.paste(img.split()[3], ((side - img.width) // 2, (side - img.height) // 2))
    tmp = Path(tempfile.mkdtemp()) / "mask.png"
    canvas.save(tmp)
    return tmp


def _trace(mask: Path, cols: int, rows: int) -> list[str]:
    out = subprocess.run(
        ["chafa", "--format=symbols", "--symbols=braille", "--colors=none",
         "--dither=none", "--threshold", THRESHOLD, f"--size={cols}x{rows}",
         "--animate=off", str(mask)],
        capture_output=True, text=True, check=True).stdout
    out = CURSOR_RE.sub("", out)
    lines = [SGR_RE.sub("", ln) for ln in out.split("\n")]
    while lines and lines[-1].strip() == "":
        lines.pop()
    return lines


def silhouette(mask: Path, cols: int, rows: int, *, color: bool) -> str:
    lines = _trace(mask, cols, rows)
    if not color:
        return "\n".join(lines) + "\n"
    fg = f"\x1b[38;2;{GOLD[0]};{GOLD[1]};{GOLD[2]}m"
    bg = f"\x1b[48;2;{FILL[0]};{FILL[1]};{FILL[2]}m"
    return "\n".join(f"{fg}{bg}{ln}\x1b[0m" for ln in lines) + "\n"


def main() -> None:
    if not shutil.which("chafa"):
        sys.exit("chafa not on PATH — `brew install chafa`")
    for src in (SMALL_SRC, LARGE_SRC):
        if not src.exists():
            sys.exit(f"source image not found: {src}")
    small, large = subject_mask(SMALL_SRC), subject_mask(LARGE_SRC)
    for tag, (cols, rows) in SMALL_SIZES.items():
        dest = LOGO_DIR / f"{PREFIX}-ansi-{tag}.txt"
        dest.write_text(silhouette(small, cols, rows, color=True))
        print(f"  ansi  {tag:>4}  {cols}x{rows}  [{SMALL_SRC.name}] -> {dest.name}")
    for tag, (cols, rows) in LARGE_SIZES.items():
        dest = LOGO_DIR / f"{PREFIX}-ansi-{tag}.txt"
        dest.write_text(silhouette(large, cols, rows, color=True))
        print(f"  ansi  {tag:>4}  {cols}x{rows}  [{LARGE_SRC.name}] -> {dest.name}")
    plain = LOGO_DIR / f"{PREFIX}-ascii-40.txt"
    plain.write_text(silhouette(large, PLAIN_COLS, PLAIN_ROWS, color=False))
    print(f"  ascii   40  {PLAIN_COLS}x{PLAIN_ROWS}  [{LARGE_SRC.name}] -> {plain.name}")
    print("done.")


if __name__ == "__main__":
    main()
