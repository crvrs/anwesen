# A stale ETag returns the full body (not 304).
GET {{host}}/notes/README.md
If-None-Match: "stale0000000000000000000000000000000000000000000000000000000000"
HTTP 200
[Asserts]
jsonpath "$.path" == "README.md"
