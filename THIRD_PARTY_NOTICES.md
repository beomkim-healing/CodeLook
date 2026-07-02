# Third‑party notices

CodeLook bundles the following third‑party assets. Their licenses apply to those
files and are retained here.

## JetBrains Mono (font)

- Files: `assets/JetBrainsMono-Regular.ttf`, `assets/JetBrainsMono-Medium.ttf`, `assets/JetBrainsMono-Bold.ttf`
- License: SIL Open Font License 1.1 (OFL‑1.1)
- Copyright: © 2020 The JetBrains Mono Project Authors
- Source: https://github.com/JetBrains/JetBrainsMono
- The OFL permits bundling and redistribution provided the license accompanies the fonts
  and the fonts are not sold by themselves. See the upstream `OFL.txt` for full terms.

## IntelliJ (expUI) icons

- Files: `assets/icons/svg/*.svg` (sources), `assets/icons/png/*.png` (pre-rasterized)
- License: Apache License 2.0
- Copyright: © 2000–2023 JetBrains s.r.o. and contributors
- Source: https://github.com/JetBrains/intellij-community
  (`platform/icons/src/expui/`, Kotlin & Python plugin icon sets)
- A few icons without an upstream equivalent (`typescript`, `rust`, `go`, `ruby`)
  were drawn for this project in the same style and are covered by this
  project's MIT license.

## Darcula TextMate theme

- File: `assets/darcula.tmTheme`
- A Darcula‑style color scheme in TextMate `.tmTheme` format, used for syntect fallback highlighting.

## Rust crates

This project depends on open‑source crates (see `Cargo.toml` / `Cargo.lock`), including
`eframe`/`egui`, `syntect`, the `tree-sitter` family, `git2`/`libgit2`, `ignore`, `regex`,
and `rfd`. Each is distributed under its own permissive license (MIT/Apache‑2.0 or similar).
