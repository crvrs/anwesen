# GET /health returns 200, JSON, and the shape promised by the User Manual.
GET {{host}}/health
HTTP 200
[Asserts]
header "Content-Type" startsWith "application/json"
jsonpath "$.note_count" exists
jsonpath "$.note_count" >= 14
jsonpath "$.watcher_state" matches "^(running|degraded)$"
jsonpath "$.in_flight_rescan" isBoolean
jsonpath "$.vault_path" exists
jsonpath "$.supervisor.restarts.vault_scanner" exists
jsonpath "$.supervisor.restarts.filesystem_watcher" exists
jsonpath "$.supervisor.restarts.index_writer" exists
jsonpath "$.supervisor.restarts.http_server" exists
