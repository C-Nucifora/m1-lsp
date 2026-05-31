local M = {}

M.defaults = {
  filetypes = { "m1scr" },
  root_dir = function(fname)
    local util = require("lspconfig.util")
    return util.root_pattern("Project.m1prj", ".git")(fname) or util.path.dirname(fname)
  end,
  settings = {},
}

function M.setup(opts)
  opts = vim.tbl_deep_extend("force", M.defaults, opts or {})

  local lspconfig = require("lspconfig")
  local configs = require("lspconfig.configs")

  if not configs.m1_lsp then
    -- locate the binary relative to this plugin's install dir
    local plugin_dir = vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":h:h:h")
    local bin = plugin_dir .. "/target/release/m1-lsp"

    configs.m1_lsp = {
      default_config = {
        cmd = { bin },
        filetypes = opts.filetypes,
        root_dir = opts.root_dir,
        single_file_support = true,
        settings = opts.settings,
      },
    }
  end

  lspconfig.m1_lsp.setup(opts)
end

return M
