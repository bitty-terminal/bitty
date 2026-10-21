-- CTX-1344 expanded matrix broker request fixture (allowed).
local function note(body)
  pcall(bitty.notify.show, { title = "component-acceptance", body = body })
end

bitty.commands.register({
  id = "go",
  title = "Component: queue one request",
  description = "Queue one bounded component request and return its ticket.",
  run = function()
    local ok, result = pcall(bitty.component.request, {
      id = "net",
      method = "GET",
      url = "http://127.0.0.1:__PORT__/ping",
      timeout_ms = 5000,
    })
    if not ok then
      local body = "REQUEST_ERR " .. tostring(result.code) .. " " .. tostring(result.message)
      note(body)
      return body
    end
    bitty.store.set("ticket", result)
    local body = "TICKET " .. tostring(result)
    note(body)
    return body
  end,
})

bitty.commands.register({
  id = "poll",
  title = "Component: poll the ticket",
  description = "Read the stored ticket: PENDING, RESULT ..., or POLL_ERR ....",
  run = function()
    local ticket = bitty.store.get("ticket")
    local ok, result = pcall(bitty.component.poll, ticket)
    if not ok then
      local body = "POLL_ERR " .. tostring(result.code) .. " " .. tostring(result.message)
      note(body)
      return body
    end
    if result == nil then
      note("PENDING")
      return "PENDING"
    end
    local body = "RESULT status=" .. tostring(result.status) .. " body=" .. tostring(result.body)
    note(body)
    return body
  end,
})

return {}
