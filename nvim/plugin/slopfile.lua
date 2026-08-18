-- slopfile.nvim — file-as-UI agent support.
--
-- The slopfile daemon watches session `.md` files (transcripts dir) and
-- streams deltas into them. This plugin gives you:
--   :Init    — save an empty buffer; the daemon formats it as a new session
--   :Send    — save the buffer, trigger the daemon sweep (draft → history)
--   :Resume  — save + force a re-parse of dangling output / question block
--   :Abort   — append the abort marker; daemon stops streaming, closes @done
--   :Status  — re-render the live status line
--
-- Autoread: the buffer auto-reloads on CursorHold / BufEnter so streamed
-- tokens appear live.
--
-- Install: put this file anywhere on &runtimepath (e.g.
-- `nvim/plugin/slopfile.lua` or add it with lazy.nvim), or
-- `:source slopfile.nvim` from your config.
--
-- Config: g:slopfile_autoread (default 1) enables checktime-on-hold.

local M = {}

local ABORT_MARKER = "<!-- slopfile:abort -->"

local function is_session_buf()
  local name = vim.api.nvim_buf_get_name(0)
  return name:match("%.md$") ~= nil
end

local function checktime()
  -- Pick up daemon-written changes (status line, streamed text).
  vim.o.autoread = true
  pcall(vim.cmd, "checktime")
end

function M.init()
  if not is_session_buf() then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  checktime()
  vim.notify("slopfile: init requested (daemon formats empty sessions)", vim.log.levels.INFO)
end

function M.send()
  if not is_session_buf() then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  checktime()
  vim.notify("slopfile: sent", vim.log.levels.INFO)
end

function M.resume()
  if not is_session_buf() then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  checktime()
  vim.notify("slopfile: resume requested", vim.log.levels.INFO)
end

function M.abort()
  if not is_session_buf() then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  -- Append the marker below the current last line; the daemon strips it
  -- once the stream stops. Never overwrite existing content.
  local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  table.insert(lines, ABORT_MARKER)
  vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
  vim.cmd("write")
  checktime()
  vim.notify("slopfile: abort requested", vim.log.levels.INFO)
end

function M.status()
  checktime()
  local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
  for i = #lines, 1, -1 do
    if lines[i]:find("slopfile:live") or lines[i]:find("%%-%-") then
      if lines[i]:find("slopfile:live") then
        vim.notify(lines[i], vim.log.levels.INFO)
        return
      end
    end
  end
  vim.notify("slopfile: no live status line (daemon may be down)", vim.log.levels.WARN)
end

local function setup_autoread()
  if vim.g.slopfile_autoread == 0 then
    return
  end
  vim.o.autoread = true
  local group = vim.api.nvim_create_augroup("slopfile_autoread", { clear = true })
  vim.api.nvim_create_autocmd({ "CursorHold", "CursorHoldI", "BufEnter", "FocusGained" }, {
    group = group,
    pattern = "*.md",
    callback = checktime,
  })
end

function M.setup(opts)
  opts = opts or {}
  if opts.autoread ~= nil then
    vim.g.slopfile_autoread = opts.autoread and 1 or 0
  end
  setup_autoread()
end

-- Commands (no Pi prefix, per project convention).
vim.api.nvim_create_user_command("Init", M.init, {})
vim.api.nvim_create_user_command("Send", M.send, {})
vim.api.nvim_create_user_command("Resume", M.resume, {})
vim.api.nvim_create_user_command("Abort", M.abort, {})
vim.api.nvim_create_user_command("Status", M.status, {})

return M