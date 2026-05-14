# Missing path returns 404.
GET {{host}}/notes/does-not-exist.md
HTTP 404
