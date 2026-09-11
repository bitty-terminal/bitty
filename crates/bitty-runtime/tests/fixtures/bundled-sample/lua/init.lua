-- Fixture entry point mirroring the accepted Plugin API v1 surface.
local helper = require("helper")

bitty.commands.register({
  id = "summary",
  title = "Sample: summary",
  description = "Exercise the sync host services.",
  run = function(_args)
    local snapshot = bitty.terminal.snapshot({ scope = "semantic" })
    bitty.store.set("seen", {
      count = helper.bump(1),
      zones = #snapshot.zones,
    })
    bitty.notify.show({ title = "Sample", body = "ok", urgency = "low" })
    return "ok"
  end,
})

bitty.events.subscribe("terminal.opened", function(event)
  bitty.store.set("last", event.payload.terminal_id)
end)

return {}
