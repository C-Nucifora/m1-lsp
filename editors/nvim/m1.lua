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
  local found =
    vim.fs.find({ "Project.m1prj", ".git" }, { upward = true, path = fname })[1]
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

-- 5. Buffer-local keymaps + completion when m1-lsp attaches.
--
-- The server loads the project model (channels/parameters/functions/...) from
-- the `Project.m1prj` at/above `root_dir`, so hover/goto/completion and the T001
-- "unresolved reference" diagnostic only light up when `root_dir` points at or
-- above the directory containing `Project.m1prj` (step 2 already does this).
-- Editing `Project.m1prj` or any `*.m1cfg` triggers a server-side reload via
-- watched files, so the project model and T001/hover/goto refresh without
-- restarting the editor.
vim.api.nvim_create_autocmd("LspAttach", {
  callback = function(args)
    local client = vim.lsp.get_client_by_id(args.data.client_id)
    if not client or client.name ~= "m1-lsp" then
      return
    end
    local opts = { buffer = args.buf, silent = true }
    vim.keymap.set("n", "K", vim.lsp.buf.hover, opts)
    vim.keymap.set("n", "gd", vim.lsp.buf.definition, opts)
    vim.keymap.set("n", "gO", vim.lsp.buf.document_symbol, opts)
    -- Completion (Nvim 0.11+ built-in; otherwise use <C-x><C-o> via omnifunc).
    if vim.lsp.completion and vim.lsp.completion.enable then
      vim.lsp.completion.enable(true, client.id, args.buf, { autotrigger = false })
    end
  end,
})
