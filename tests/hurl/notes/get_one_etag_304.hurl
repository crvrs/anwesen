# Chain: fetch ETag, then send If-None-Match -> 304.
GET {{host}}/notes/README.md
HTTP 200
[Captures]
etag: header "ETag"

GET {{host}}/notes/README.md
If-None-Match: {{etag}}
HTTP 304
