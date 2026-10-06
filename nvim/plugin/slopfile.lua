
local M = {}

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

function M.bin()
  return vim.g.sf_bin or vim.g.slopfile_bin or "sf"
end

local function session_name()
  local path = vim.api.nvim_buf_get_name(0)
  if path == "" then
    return nil, "buffer has no file name"
  end
  return vim.fn.fnamemodify(path, ":t")
end

local function notify_out(level, out)
  vim.notify(out:gsub("%s+$", ""), level)
end

function M.sf(args, level)
  local out = vim.fn.system(vim.list_extend({ M.bin() }, args))
  if vim.v.shell_error ~= 0 then
    notify_out(level == nil and vim.log.levels.ERROR or level, out)
    return false
  end
  if out:gsub("%s+$", "") ~= "" then
    notify_out(level == nil and vim.log.levels.INFO or level, out)
  end
  return true
end

function M.init()
  local path = vim.api.nvim_buf_get_name(0)
  if not is_session_buf(0) then
    vim.notify("sf: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  M.sf({ "init", path })
end

local function send_cmd(label)
  local name, why = session_name()
  if not name then
    vim.notify("sf: " .. why, vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  checktime()
  M.sf({ "send", name }, vim.log.levels.INFO)
end

function M.send()
  if not is_session_buf(0) then
    vim.notify("sf: not a session .md buffer", vim.log.levels.WARN)
    return
  end
  send_cmd("sent")
end

function M.resume()
  M.send()
end

function M.abort()
  local name, why = session_name()
  if not name then
    vim.notify("sf: " .. why, vim.log.levels.WARN)
    return
  end
  vim.cmd("write")
  M.sf({ "abort", name }, vim.log.levels.INFO)
end

function M.stop()
  local name, why = session_name()
  if not name then
    vim.notify("sf: " .. why, vim.log.levels.WARN)
    return
  end
  M.sf({ "stop", name }, vim.log.levels.INFO)
end

function M.status()
  local name, why = session_name()
  if not name then
    vim.notify("sf: " .. why, vim.log.levels.WARN)
    return
  end
  checktime()
  M.sf({ "status", name }, vim.log.levels.INFO)
end

function M.question()
  local name, why = session_name()
  if not name then
    vim.notify("sf: " .. why, vim.log.levels.WARN)
    return
  end
  vim.ui.input({ prompt = "answers (comma separated): " }, function(input)
    if not input or input == "" then
      return
    end
    vim.cmd("write")
    local answers = vim.split(input, ",", { plain = true, trimempty = true })
    M.sf(vim.list_extend({ "question", name }, answers), vim.log.levels.INFO)
  end)
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
    vim.g.sf_bin = opts.bin
  end
  setup_autoread()
end

function M.tokens()
  local path = vim.api.nvim_buf_get_name(0)
  if path == "" then
    vim.notify("sf: buffer has no file name", vim.log.levels.WARN)
    return
  end
  checktime()
  M.sf({ "tokens", path })
end

vim.api.nvim_create_user_command("Init", M.init, {})
vim.api.nvim_create_user_command("Send", M.send, {})
vim.api.nvim_create_user_command("Resume", M.resume, {})
vim.api.nvim_create_user_command("Abort", M.abort, {})
vim.api.nvim_create_user_command("Stop", M.stop, {})
vim.api.nvim_create_user_command("Status", M.status, {})
vim.api.nvim_create_user_command("Question", M.question, {})
vim.api.nvim_create_user_command("Tokens", M.tokens, {})

vim.g.slopfile_plugin = M

return M
