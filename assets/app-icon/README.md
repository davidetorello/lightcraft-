# Lumen app icon

A film roll with one lit frame (amber glow) on a near-black ground.

## Provenance

`source/lumen-source.png` (1254 px) is the picture supplied by the project owner for Lumen (file "logo lumeno.png",
October 2026). Every other file here is derived from it by `packaging/icons.py` (Pillow; run it after changing the source).
Licence: `LICENSE.txt` (MIT OR Apache-2.0, like the repo).

## Geometry

Windows, Linux and web files use the full-bleed tile with rounded corners (22 %). macOS files (`.icns`,
`lightcraft-macos-512.png`) add Apple's transparent margin (an 824/1024 body).

## Files

| File | Use |
|---|---|
| `source/lumen-source.png` | the supplied picture |
| `lightcraft.svg`, `lightcraft-small.svg` | SVG wrappers that embed the artwork as a PNG (the picture is a raster) |
| `lightcraft-1024.png` | 1024 px render (store listings, docs) |
| `lightcraft-macos-512.png` | runtime window/Dock icon on macOS (embedded by `apps/lightcraft/src/main.rs`) |
| `lightcraft.icns` | macOS app icon |
| `lightcraft.ico` | Windows icon (16-256 px) |
| `hicolor/` | Linux icon theme (16-512 px and scalable) |

The file names keep the old project name on purpose: binaries, the desktop file and the bundle identifier
(`ai.storyteller.lightcraft`) did not change, so installed copies update in place.
