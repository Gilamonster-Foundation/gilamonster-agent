#!/usr/bin/env python3
"""Generate gila's terminal splash art — mode-by-size, one hero source.

gila inherits newt-agent's TUI, whose splash art is brandable at runtime via the
brand seam (NEWT_BRAND_LOGO_DIR / NEWT_BRAND_LOGO_PREFIX; newt-agent#355). This
writes the `<prefix>-<stem>.txt` ANSI art the seam loads.

Each size plays to its strength (one source: gilamonster_logo_source.png):
  - 10, 20  → **silhouette** braille (gold on dark). A photo-reduction is mud at
              this size — braille keys on per-dot luminance, so detail punches
              holes. The alpha-mask silhouette stays solid and reads as a gila.
  - 40      → **half-block** truecolor (plain). Enough cells for a real image.
  - 80+     → **half-block** truecolor, colors exaggerated (saturation/contrast)
              so the gold bands / red mouth / eyes pop on a terminal.
  - ascii-40 (no-color fallback) → monochrome silhouette.

Half-block (`▀`/`▄`) paints each cell as two stacked truecolor pixels — a true
low-res image, not a dithered dot field — which is why it doesn't go muddy.

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
from PIL import Image, ImageEnhance

ROOT = Path(__file__).resolve().parent.parent
LOGO_DIR = ROOT / "docs" / "logos"
SRC = LOGO_DIR / "gilamonster_logo_source.png"
PREFIX = "gilly"             # NEWT_BRAND_LOGO_PREFIX gila sets at startup

FILL = (20, 20, 20)          # newt's dark cell background (#141414)
GOLD = (235, 195, 70)        # gilamonster gold — the silhouette ink
ALPHA_CUTOFF = 96            # alpha >= this counts as subject for the crop bbox
CROP_PAD = 0.04              # padding around the subject bbox (fraction)
THRESHOLD = "0.5"           # chafa luminance midpoint for the 1-bit silhouette
SATURATION = 1.9             # color exaggeration for the large (80+) splashes
CONTRAST = 1.15

# (cols, rows): rows = newt-tui's per-logo height budget in logo_for_size().
SILHOUETTE_SIZES = {"10": (10, 5), "20": (20, 10)}      # braille silhouette, gold
HALFBLOCK_PLAIN = {"40": (40, 20)}                       # half-block, true colors
HALFBLOCK_EXAG = {"full": (80, 40), "120": (126, 61), "160": (166, 81)}  # + exaggerated
PLAIN_COLS, PLAIN_ROWS = 40, 20   # <prefix>-ascii-40.txt (LOGO_PLAIN, monochrome)
CURSOR_RE = re.compile(r"\x1b\[\?\d+[hl]")
SGR_RE = re.compile(r"\x1b\[[0-9;]*m")


def _clean(out: str) -> str:
    out = CURSOR_RE.sub("", out)
    lines = out.split("\n")
    while lines and SGR_RE.sub("", lines[-1]).strip() == "":
        lines.pop()
    return "\n".join(lines) + "\n"


# ---- silhouette (braille over the alpha mask) -------------------------------

def subject_mask(src: Path) -> Path:
    """Crop to the subject (by alpha) and return a square grayscale mask."""
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


def silhouette(mask: Path, cols: int, rows: int, *, color: bool) -> str:
    out = subprocess.run(
        ["chafa", "--format=symbols", "--symbols=braille", "--colors=none",
         "--dither=none", "--threshold", THRESHOLD, f"--size={cols}x{rows}",
         "--animate=off", str(mask)],
        capture_output=True, text=True, check=True).stdout
    lines = [SGR_RE.sub("", ln) for ln in _clean(out).rstrip("\n").split("\n")]
    if not color:
        return "\n".join(lines) + "\n"
    fg = f"\x1b[38;2;{GOLD[0]};{GOLD[1]};{GOLD[2]}m"
    bg = f"\x1b[48;2;{FILL[0]};{FILL[1]};{FILL[2]}m"
    return "\n".join(f"{fg}{bg}{ln}\x1b[0m" for ln in lines) + "\n"


# ---- half-block (true-color image over a dark, squared canvas) --------------

def flattened(src: Path, *, saturate: bool) -> Path:
    """Crop to subject, square-letterbox onto the dark fill, optionally
    exaggerate colors — the source for a half-block render."""
    img = Image.open(src).convert("RGBA")
    arr = np.asarray(img)
    ys, xs = np.where(arr[..., 3] > ALPHA_CUTOFF)
    if len(xs):
        px = int((xs.max() - xs.min()) * CROP_PAD)
        py = int((ys.max() - ys.min()) * CROP_PAD)
        img = img.crop((max(0, xs.min() - px), max(0, ys.min() - py),
                        min(img.width, xs.max() + px), min(img.height, ys.max() + py)))
    side = max(img.size)
    canvas = Image.new("RGBA", (side, side), (*FILL, 255))
    canvas.paste(img, ((side - img.width) // 2, (side - img.height) // 2), img)
    rgb = canvas.convert("RGB")
    if saturate:
        rgb = ImageEnhance.Color(rgb).enhance(SATURATION)
        rgb = ImageEnhance.Contrast(rgb).enhance(CONTRAST)
    tmp = Path(tempfile.mkdtemp()) / "flat.png"
    rgb.save(tmp)
    return tmp


def halfblock(flat: Path, cols: int, rows: int) -> str:
    out = subprocess.run(
        ["chafa", "--symbols=half", "--colors=truecolor", "--dither=none",
         "--optimize=0", f"--size={cols}x{rows}", "--animate=off",
         "--bg=141414", str(flat)],
        capture_output=True, text=True, check=True).stdout
    return _clean(out)


def main() -> None:
    if not shutil.which("chafa"):
        sys.exit("chafa not on PATH — `brew install chafa`")
    if not SRC.exists():
        sys.exit(f"source image not found: {SRC}")

    mask = subject_mask(SRC)
    for tag, (cols, rows) in SILHOUETTE_SIZES.items():
        dest = LOGO_DIR / f"{PREFIX}-ansi-{tag}.txt"
        dest.write_text(silhouette(mask, cols, rows, color=True))
        print(f"  ansi  {tag:>4}  {cols}x{rows}  silhouette        -> {dest.name}")

    flat_plain = flattened(SRC, saturate=False)
    for tag, (cols, rows) in HALFBLOCK_PLAIN.items():
        dest = LOGO_DIR / f"{PREFIX}-ansi-{tag}.txt"
        dest.write_text(halfblock(flat_plain, cols, rows))
        print(f"  ansi  {tag:>4}  {cols}x{rows}  half-block        -> {dest.name}")

    flat_exag = flattened(SRC, saturate=True)
    for tag, (cols, rows) in HALFBLOCK_EXAG.items():
        dest = LOGO_DIR / f"{PREFIX}-ansi-{tag}.txt"
        dest.write_text(halfblock(flat_exag, cols, rows))
        print(f"  ansi  {tag:>4}  {cols}x{rows}  half-block (exag) -> {dest.name}")

    plain = LOGO_DIR / f"{PREFIX}-ascii-40.txt"
    plain.write_text(silhouette(mask, PLAIN_COLS, PLAIN_ROWS, color=False))
    print(f"  ascii   40  {PLAIN_COLS}x{PLAIN_ROWS}  silhouette (mono) -> {plain.name}")
    print("done.")


if __name__ == "__main__":
    main()
