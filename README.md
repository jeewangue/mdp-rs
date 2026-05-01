# mdp-rs

**m**ark**d**own **p**reviewer — any directory of `.md` files → browser, in one command.

Powered by [mdBook](https://rust-lang.github.io/mdBook/) under the hood, with a
hierarchical sidebar + breadcrumb, KaTeX, Mermaid, PlantUML, native GFM alerts,
and built-in full-text search.

## Install

From local source (use this if you've patched mdp-rs and haven't pushed yet):

```sh
cd path/to/mdp-rs
cargo install --path . --locked
asdf reshim rust          # asdf users only — refreshes the shim
mdp install-deps          # cargo-installs mdbook + 5 preprocessors
```

Or from upstream:

```sh
cargo install --git https://github.com/jeewangue/mdp-rs --locked
mdp install-deps
```

`mdp pdf` additionally needs `lualatex`. On Arch:

```sh
sudo pacman -S texlive-luatex texlive-langkorean
```

`mdp` itself preflights every dep and fails fast with an install hint, so you
can always discover what's missing by running the subcommand once.

## Usage

```sh
mdp serve ./docs                    # serve on http://127.0.0.1:3456 with live reload
mdp serve ~/notes -p 4000 --open    # pick port, open browser
mdp build ./docs -o ./out           # static HTML
mdp pdf   ./docs -o ./book.pdf      # single PDF via lualatex
mdp pdf   ./docs --pandoc-to html5  # self-contained HTML instead of PDF
```

## Features

- Hierarchical sidebar (per-dir `index.md` / `README.md` fallback) + breadcrumb
- Heading-level page TOC via `mdbook-pagetoc`
- KaTeX math, Mermaid + PlantUML diagrams (PlantUML inlined as data-URI SVG for PDF)
- GFM alerts (`> [!NOTE]`, `> [!WARNING]` …) via mdbook native
- Full-text search (mark.js)
- `mdp serve` regenerates SUMMARY automatically on `.md` add / rename / delete
- "julian.jee" theme — palette ported from Julian's nvim markdown preview

## Configuration

| Env var                  | Default | Effect                                                                    |
| ------------------------ | ------- | ------------------------------------------------------------------------- |
| `MDP_PORT`               | `3456`  | Override `--port`.                                                        |
| `MDP_HOST`               | `127.0.0.1` | Override `--host`. Non-loopback requires `MDP_ALLOW_NON_LOOPBACK`.    |
| `MDP_ALLOW_NON_LOOPBACK` | _unset_ | Set to `1` to let `MDP_HOST` bind to a non-loopback address.              |
| `MDP_BOOK_LANG`          | `$LANG` → `en` | `<html lang>` value; first BCP-47 segment of `$LANG`/`$LC_ALL`.    |
| `MDP_AUTHOR`             | `$USER` | Author string in generated `book.toml`.                                   |
| `MDP_PLANTUML_SERVER`    | plantuml.com | `http://` or `https://` PlantUML server. Path-style refused.         |
| `MDP_PDF_TIMEOUT`        | `600`   | Overall PDF build timeout (seconds; `0` to disable).                      |
| `MDP_PDF_STALL_TIMEOUT`  | `60`    | Kill PDF build after N seconds of no output (lualatex hang detection).    |
| `MDP_KEEP_WORKSPACE`     | _unset_ | Preserve the temp workspace on PDF failure for inspection.                |

## Development

```sh
make ci    # cargo build --release + cargo test --release + scripts/smoke.sh
make smoke # clean-room artifact-as-shipped gate (mktemp + run + assert)
```

`scripts/smoke.sh` runs the release binary against a synthesised fixture in a
fresh `mktemp` dir and asserts on the rendered HTML — catches asset-embedding
regressions that unit tests can miss.

## Roadmap

- [ ] Replace stock search with [pagefind](https://pagefind.app)
- [ ] AUR package
- [ ] PDF with `xelatex` / `tectonic` fallback when lualatex is missing

## License

MIT © Julian Jee
