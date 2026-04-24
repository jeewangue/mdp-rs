# mdp.nvim

Companion Neovim Lua plugin for the [mdp](https://gitlab.com/julian.jee/mdp-rs)
CLI. Spawns `mdp serve <dir>` as a backgrounded job so you can preview the
current buffer's directory in a browser while you edit.

No buffer-level live sync — on `:w`, mdbook's file watcher reloads the browser
(usually < 200 ms). If you want preview without saving, use a scratch file or
`:set autowrite`.

## Install (lazy.nvim)

```lua
{
  url = "https://gitlab.com/julian.jee/mdp-rs",
  -- the nvim plugin lives in the nvim/ subdirectory of the repo
  config = function()
    -- adjust rtp if your plugin manager doesn't auto-discover nvim/
    vim.opt.rtp:append(vim.fn.stdpath("data") .. "/lazy/mdp-rs/nvim")
    require("mdp").setup({
      bin = "mdp",
      port = 3456,
      host = "127.0.0.1",
      open_browser = false,
    })
  end,
  cmd = { "MdpOpen", "MdpClose", "MdpToggle", "MdpStatus" },
  ft = { "markdown" },
  keys = {
    { "<leader>mp", "<cmd>MdpToggle<cr>", desc = "Toggle mdp preview" },
  },
}
```

## Commands

| Command | Behavior |
|---|---|
| `:MdpOpen [dir]` | Start `mdp serve` on `dir` (or current buffer's dir) |
| `:MdpClose` | Stop the running job |
| `:MdpToggle` | Start if stopped, stop if running |
| `:MdpStatus` | Print current state (serving/stopped + URL) |

## Requirements

- Neovim 0.9+
- `mdp` binary in `$PATH` (install via `cargo install --git https://gitlab.com/julian.jee/mdp-rs`)
- The mdbook preprocessors `mdp install-deps` installs
