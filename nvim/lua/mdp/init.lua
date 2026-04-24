-- mdp.nvim — companion plugin for the `mdp` CLI
--
-- Provides `:MdpOpen`, `:MdpClose`, `:MdpToggle` that spawn `mdp serve <dir>`
-- as a background job scoped to the current nvim session.
--
-- We don't try to do buffer-level live sync — saving the file is enough for
-- mdbook's file watcher to reload the browser.

local M = {}

---@class MdpConfig
---@field bin string       # mdp executable (default: "mdp", resolved via $PATH)
---@field port integer     # port to serve on (default: 3456)
---@field host string      # bind host (default: "127.0.0.1")
---@field open_browser boolean  # auto-open default browser (default: false)
---@field root_finder fun(buf:integer):string|nil  # resolve preview root from buffer; default: cwd of buffer file
M.config = {
  bin = "mdp",
  port = 3456,
  host = "127.0.0.1",
  open_browser = false,
  root_finder = nil,
}

---@type integer|nil
local job_id = nil

---@type string|nil
local serving_dir = nil

local function default_root_finder(buf)
  local bufname = vim.api.nvim_buf_get_name(buf)
  if bufname == "" then
    return vim.fn.getcwd()
  end
  return vim.fs.dirname(bufname)
end

local function resolve_root(buf)
  local finder = M.config.root_finder or default_root_finder
  return finder(buf)
end

local function is_running()
  return job_id ~= nil and job_id > 0
end

local function url()
  return string.format("http://%s:%d/", M.config.host, M.config.port)
end

--- Start `mdp serve` in a background job.
---@param opts? { dir?: string }
function M.open(opts)
  opts = opts or {}
  if is_running() then
    vim.notify(
      string.format("[mdp] already serving %s at %s", serving_dir or "?", url()),
      vim.log.levels.INFO
    )
    return
  end

  local dir = opts.dir or resolve_root(0)
  if not dir or dir == "" then
    vim.notify("[mdp] could not determine preview directory", vim.log.levels.ERROR)
    return
  end

  if vim.fn.executable(M.config.bin) == 0 then
    vim.notify(
      string.format("[mdp] `%s` not found in $PATH. Install: cargo install --git https://gitlab.com/julian.jee/mdp-rs", M.config.bin),
      vim.log.levels.ERROR
    )
    return
  end

  local cmd = {
    M.config.bin,
    "serve",
    dir,
    "--port", tostring(M.config.port),
    "--host", M.config.host,
  }
  if M.config.open_browser then
    table.insert(cmd, "--open")
  end

  job_id = vim.fn.jobstart(cmd, {
    detach = false,
    on_stdout = function(_, data, _)
      for _, line in ipairs(data or {}) do
        if line ~= "" then
          vim.schedule(function()
            vim.notify("[mdp] " .. line, vim.log.levels.INFO)
          end)
        end
      end
    end,
    on_stderr = function(_, data, _)
      for _, line in ipairs(data or {}) do
        if line ~= "" then
          vim.schedule(function()
            -- mdbook uses stderr for INFO logs — show as info, not error.
            vim.notify("[mdp] " .. line, vim.log.levels.INFO)
          end)
        end
      end
    end,
    on_exit = function(_, code, _)
      vim.schedule(function()
        if code ~= 0 then
          vim.notify(
            string.format("[mdp] serve exited with code %d", code),
            vim.log.levels.WARN
          )
        end
        job_id = nil
        serving_dir = nil
      end)
    end,
  })

  if job_id <= 0 then
    vim.notify("[mdp] failed to start `mdp serve`", vim.log.levels.ERROR)
    job_id = nil
    return
  end

  serving_dir = dir
  vim.notify(string.format("[mdp] serving %s at %s", dir, url()), vim.log.levels.INFO)
end

--- Stop the running `mdp serve` job.
function M.close()
  if not is_running() then
    vim.notify("[mdp] no running job", vim.log.levels.INFO)
    return
  end
  vim.fn.jobstop(job_id)
  -- on_exit handler clears job_id / serving_dir.
end

--- Toggle serve state for the current buffer's root.
function M.toggle()
  if is_running() then
    M.close()
  else
    M.open()
  end
end

--- Current status (for statusline or :MdpStatus).
function M.status()
  if is_running() then
    return string.format("serving %s at %s", serving_dir or "?", url())
  end
  return "stopped"
end

--- Setup hook — call from your plugin manager's `config` block.
---@param opts? table
function M.setup(opts)
  if opts then
    for k, v in pairs(opts) do
      M.config[k] = v
    end
  end
end

return M
