# GET /notes/<path> returns the documented JSON shape.
GET {{host}}/notes/README.md
HTTP 200
[Asserts]
header "Content-Type" startsWith "application/json"
header "ETag" exists
jsonpath "$.path" == "README.md"
jsonpath "$.frontmatter.status" == "published"
jsonpath "$.frontmatter.tags" count == 2
jsonpath "$.frontmatter.tags[0]" == "demo"
jsonpath "$.frontmatter.tags[1]" == "readme"
jsonpath "$.frontmatter.date" == "2026-05-01"
jsonpath "$.body" contains "Fixture vault README"
jsonpath "$.size" > 0
jsonpath "$.etag" matches "^\"[0-9a-f]{64}\"$"
jsonpath "$.last_modified" matches "^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}Z$"
