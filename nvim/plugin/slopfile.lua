
local M = {}

local ABORT_MARKER = "<!-- slopfile:abort -->"

local function is_session_buf(bufnr)
  local name = vim.api.nvim_buf_get_name(bufnr or 0)
  return name:match("%.md$") ~= nil
end

function M._refresh()
  if not is_session_buf(0) then
    return
  end
  local buf = vim.api.nvim_get_current_buf()
  local last_before = vim.api.nvim_buf_line_count(buf)
  local cursor_before = vim.api.nvim_win_get_cursor(0)[1]
  pcall(vim.cmd, "checktime")
  local last_after = vim.api.nvim_buf_line_count(buf)
  if last_after ~= last_before and cursor_before == last_before then
    vim.api.nvim_win_set_cursor(0, { last_after, 0 })
  end
end

local function checktime()
  vim.o.autoread = true
  M._refresh()
end

local function start_poll()
  if M.timer ~= nil then
    return
  end
  local interval = vim.g.slopfile_poll_ms or 1000
  M.timer = vim.fn.timer_start(interval, function()
    M._refresh()
  end, { ["repeat"] = -1 })
end

local function setup_autoread()
  if vim.g.slopfile_autoread == 0 then
    return
  end
  vim.o.autoread = true
  local group = vim.api.nvim_create_augroup("slopfile_autoread", { clear = true })
  vim.api.nvim_create_autocmd(
    { "CursorHold", "CursorHoldI", "BufEnter", "FocusGained", "BufWritePost" },
    { group = group, pattern = "*.md", callback = checktime }
  )
  start_poll()
end

function M.init()
  if not is_session_buf(0) then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  checktime()
  vim.notify("slopfile: init requested (daemon formats empty sessions)", vim.log.levels.INFO)
end

function M.send()
  if not is_session_buf(0) then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  checktime()
  vim.notify("slopfile: sent", vim.log.levels.INFO)
end

function M.resume()
  if not is_session_buf(0) then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  checktime()
  vim.notify("slopfile: resume requested", vim.log.levels.INFO)
end

function M.abort()
  if not is_session_buf(0) then
    vim.notify("slopfile: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
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

function M.setup(opts)
  opts = opts or {}
  if opts.autoread ~= nil then
    vim.g.slopfile_autoread = opts.autoread and 1 or 0
  end
  if opts.poll_ms ~= nil then
    vim.g.slopfile_poll_ms = opts.poll_ms
  end
  if opts.bin ~= nil then
    vim.g.slopfile_bin = opts.bin
  end
  setup_autoread()
end

function M.tokens()
  checktime()
  local path = vim.api.nvim_buf_get_name(0)
  if path == "" then
    vim.notify("slopfile: buffer has no file name", vim.log.levels.WARN)
    return
  end
  local bin = vim.g.slopfile_bin or "slopfile"
  local out = vim.fn.system({ bin, "tokens", path })
  if vim.v.shell_error ~= 0 then
    vim.notify("slopfile: " .. out:gsub("%s+$", ""), vim.log.levels.ERROR)
    return
  end
  vim.notify(out:gsub("%s+$", ""), vim.log.levels.INFO)
end

vim.api.nvim_create_user_command("Init", M.init, {})
vim.api.nvim_create_user_command("Send", M.send, {})
vim.api.nvim_create_user_command("Resume", M.resume, {})
vim.api.nvim_create_user_command("Abort", M.abort, {})
vim.api.nvim_create_user_command("Status", M.status, {})
vim.api.nvim_create_user_command("Tokens", M.tokens, {})

vim.g.slopfile_plugin = M

return M
