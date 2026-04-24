# mdp-rs

**m**ark**d**own **p**reviewer — any directory of `.md` files → browser, in one command.

Powered by [mdBook](https://rust-lang.github.io/mdBook/) under the hood, with a
heading-level sidebar TOC, KaTeX, Mermaid, PlantUML, native GFM alerts, and built-in
full-text search.

## Install

```sh
cargo install --git https://github.com/jeewangue/mdp-rs
mdp install-deps     # cargo installs required mdbook preprocessors
```

## Usage

```sh
mdp serve ./docs                    # serve on http://127.0.0.1:3456
mdp serve ~/notes -p 4000 --open    # pick port, open browser
mdp build ./docs -o ./out           # static HTML
```

## Features

- Left sidebar: chapter tree **plus** nested heading TOC (via `mdbook-pagetoc`)
- KaTeX math, Mermaid and PlantUML diagrams
- GFM alerts (`> [!NOTE]`, `> [!WARNING]` …) via mdbook's native support
- Built-in mark.js full-text search
- Live reload on file save (via `mdbook serve --watch`)
- "julian.jee" theme — palette ported from Julian's
  `~/.config/nvim/assets/markdown-preview/markdown.css`

## Roadmap

- [ ] `mdp pdf` via [mdbook-pandoc](https://github.com/max-heller/mdbook-pandoc)
- [ ] Replace stock search with [pagefind](https://pagefind.app) (better UX, smaller
      index for big trees)
- [ ] nvim companion (`:MdpOpen` — opens current dir in mdp)
- [ ] AUR package
- [ ] IdempoTent `mdp install-deps` (skip if already present — already done)

## License

MIT © Julian Jee
