# Unknown operator suffix is rejected with 400 (not silently ignored).
GET {{host}}/query?title__contians=foo
HTTP 400
