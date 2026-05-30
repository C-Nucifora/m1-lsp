-- m1-lsp Neovim setup. Copy into your config (e.g. require this file from init.lua).
-- Requires the `m1-lsp` binary on $PATH (cargo install --path . or symlink the
-- release binary).

-- 1. Register the .m1scr filetype.
vim.filetype.add({
  extension = {
    m1scr = "m1scr",
    -- m1prj / m1cfg are XML; map them if you want XML tooling:
    -- m1prj = "xml", m1cfg = "xml",
  },
})

-- 2. Find the project root: nearest ancestor with Project.m1prj, else .git, else file dir.
local function root_dir(fname)
  local found = vim.fs.find({ "Project.m1prj", ".git" }, { upward = true, path = fname })[1]
  if found then
    return vim.fs.dirname(found)
  end
  return vim.fs.dirname(fname)
end

-- 3. Start the server on .m1scr buffers.
vim.api.nvim_create_autocmd("FileType", {
  pattern = "m1scr",
  callback = function(args)
    local fname = vim.api.nvim_buf_get_name(args.buf)
    vim.lsp.start({
      name = "m1-lsp",
      cmd = { "m1-lsp" },
      root_dir = root_dir(fname),
    })
  end,
})

-- 4. Optional: format on save (off by default; uncomment to enable).
-- vim.api.nvim_create_autocmd("BufWritePre", {
--   pattern = "*.m1scr",
--   callback = function() vim.lsp.buf.format({ async = false }) end,
-- })
