-- CTX-1344 expanded matrix broker request fixture (no network grant).
local function note(body)
  pcall(bitty.notify.show, { title = "component-denial", body = body })
end

bitty.commands.register({
  id = "deny",
  title = "Component: denied request",
  description = "Request a component without network authority; expect the typed denial.",
  run = function()
    local ok, result = pcall(bitty.component.request, {
      id = "net",
      method = "GET",
      url = "http://127.0.0.1:__PORT__/denied",
      timeout_ms = 2000,
    })
    if ok then
      local body = "UNEXPECTED_TICKET " .. tostring(result)
      note(body)
      return body
    end
    local body = "DENIED " .. tostring(result.code) .. " " .. tostring(result.message)
    note(body)
    return body
  end,
})

return {}
