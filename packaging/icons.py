#!/usr/bin/env python3
"""Regenerate every app icon file from assets/app-icon/source/lumen-source.png.

Needs Pillow (pip install 'pillow>=12'); works on any OS (Pillow writes the .ico and the .icns). The
outputs are committed, so building and packaging never need this tool.

    python3 packaging/icons.py
"""
import base64
import io
import pathlib

from PIL import Image, ImageDraw

ROOT = pathlib.Path(__file__).resolve().parent.parent
DIR = ROOT / "assets" / "app-icon"
ID = "ai.storyteller.lightcraft"  # the desktop file / bundle id keeps its name (it identifies the installed app)

src = Image.open(DIR / "source" / "lumen-source.png").convert("RGB")


def square(size):
    """The full-bleed tile (Windows, Linux, web): the artwork with rounded corners (rx = 22 %)."""
    img = src.resize((size, size), Image.LANCZOS).convert("RGBA")
    scale = 4
    mask = Image.new("L", (size * scale, size * scale), 0)
    r = round(size * scale * 0.22)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, size * scale - 1, size * scale - 1), r, fill=255)
    img.putalpha(mask.resize((size, size), Image.LANCZOS))
    return img


def mac(size):
    """macOS grid: an 824/1024 body with Apple's transparent margin."""
    body = round(size * 824 / 1024)
    tile = square(body)
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(tile, ((size - body) // 2, (size - body) // 2), tile)
    return out


def save(img, path):
    path.parent.mkdir(parents=True, exist_ok=True)
    img.save(path, optimize=True)


save(square(1024), DIR / "lightcraft-1024.png")
save(mac(512), DIR / "lightcraft-macos-512.png")

for s in (16, 24, 32, 48, 64, 128, 256, 512):
    save(square(s), DIR / "hicolor" / f"{s}x{s}" / "apps" / f"{ID}.png")

# Windows .ico (PNG-compressed entries up to 256 px).
sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
square(256).save(DIR / "lightcraft.ico", sizes=[(s, s) for s in sizes])

# macOS .icns.
mac(1024).save(DIR / "lightcraft.icns", format="ICNS")


# The scalable files: an SVG that carries the artwork as an embedded PNG (the picture is a raster).
def svg(px):
    buf = io.BytesIO()
    square(px).save(buf, "PNG", optimize=True)
    data = base64.b64encode(buf.getvalue()).decode()
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" '
        'width="512" height="512" viewBox="0 0 512 512"><title>Lumen</title>'
        f'<image width="512" height="512" xlink:href="data:image/png;base64,{data}"/></svg>\n'
    )


(DIR / "lightcraft.svg").write_text(svg(1024))
(DIR / "lightcraft-small.svg").write_text(svg(512))
(DIR / "hicolor" / "scalable" / "apps").mkdir(parents=True, exist_ok=True)
(DIR / "hicolor" / "scalable" / "apps" / f"{ID}.svg").write_text(svg(512))
print("icons written to", DIR)
