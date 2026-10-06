local M = require("app.mod")
local util = require 'util.helpers'

function greet(name)
  return util.upper(name)
end

local function local_helper(v)
  return greet(v)
end

function M.foo(x)
  return local_helper(x)
end

function M:bar()
  return self:foo("x")
end
