# Windows bring-up (first build and test on a real Windows PC)

For an agent (or person) on a Windows machine. Nothing below has been run on Windows yet: the code was developed on
Linux and macOS, so treat every step as a check and fix what breaks. Never commit media files; keep fixes on a
branch named `windows` and ask before pushing.

## 1. Tools

Install with `winget` (accept the administrator prompts):

- `Git.Git`
- `Rustlang.Rustup`, then `rustup default stable` and `rustup target add x86_64-pc-windows-msvc`
- `Microsoft.VisualStudio.2022.BuildTools` with the *Desktop development with C++* workload
  (`--override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`)

Optional: clone <https://github.com/storytold/craft-fonts> next to this repository and set `CRAFT_FONTS_DIR` to it, so
Chinese and Japanese text has glyphs.

## 2. Build

```powershell
cargo build --release -p lightcraft -p lightcraft-cli
```

Fix anything that fails only on Windows (paths, `cfg(unix)` code, line endings). Then `cargo xtask ci`.

## 3. Check real files

Put one or two Canon CR3 files in a folder (not in the repository), then:

```powershell
target\release\lightcraft-cli render IMG.CR3 -o out.jpg
target\release\lightcraft-cli render IMG.CR3 -o out.tif
target\release\lightcraft-cli render IMG.CR3 -o nr.jpg --set detail.nrLuminance=50 --set detail.nrColor=50
```

Open the results and check them: the picture is upright, colours are plausible, noise reduction cleans a high-ISO
file without smearing texture. Note the time of each render (a 24 MP file took about 17 s on a Linux CPU without a GPU).

Then start the app (`target\release\lightcraft.exe`) and check: the window opens, a CR3 loads, sliders respond, the
sky mask (Masking ▸ Sky) selects the sky of a landscape, export works. Report GPU adapter and render speed
(`LIGHTCRAFT_PROFILE=1` prints per-stage timings).

## 4. Lens profiles (Canon)

Lens profiles come from lensfun's XML files, which are CC BY-SA 3.0 and not shipped here. Put `mil-canon.xml` and
`slr-canon.xml` (from <https://github.com/lensfun/lensfun>, `data/db`) in a `lensfun` folder next to
`lightcraft.exe`, or set `LIGHTCRAFT_LENSFUN_DIR`. Check that Optics ▸ Enable Profile Corrections finds the lens of
the CR3 and straightens the frame.

## 5. Report

List what failed, what was fixed (with commits on the `windows` branch) and the timings.
