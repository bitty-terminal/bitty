# Harness fixtures (CTX-2413 expanded matrix)

These files are harness inputs, never product source. They are copied into the
isolated output `assets/component-fixtures/` by
`harness.prepare_component_fixtures`, with `__PORT__` replaced by a fresh
loopback port for the run.

- `component/server.py` — loopback HTTP fixture serving any GET with
  `hello-component`.
- `component/allowed/` — plugin with `network.connect:127.0.0.1:<port>` and an
  egress grant; queues a component request and polls the ticket.
- `component/denied/` — plugin with no network capability; requests
  `http://127.0.0.1:<port>/denied` and expects a typed denial.

The fixture package is removed and reinstalled before each net configuration so
a previous run's embedded port cannot be reused silently.
