-- Register :MdpOpen / :MdpClose / :MdpToggle / :MdpStatus commands at startup.
-- Requires Neovim 0.9+ for vim.fs.dirname.

if vim.g.loaded_mdp == 1 then
  return
end
vim.g.loaded_mdp = 1

local function cmd(name, handler, opts)
  vim.api.nvim_create_user_command(name, handler, opts or {})
end

cmd("MdpOpen", function(args)
  local dir = args.args ~= "" and args.args or nil
  require("mdp").open({ dir = dir })
end, { nargs = "?", complete = "dir", desc = "Serve <dir> (defaults to current buffer's directory) with mdp" })

cmd("MdpClose", function()
  require("mdp").close()
end, { desc = "Stop the running mdp serve job" })

cmd("MdpToggle", function()
  require("mdp").toggle()
end, { desc = "Toggle mdp serve for the current buffer's directory" })

cmd("MdpStatus", function()
  print(require("mdp").status())
end, { desc = "Print mdp serve status" })

cmd("MdpLog", function()
  require("mdp").log()
end, { desc = "Open captured mdp serve stdout/stderr log" })
