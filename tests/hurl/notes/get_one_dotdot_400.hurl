# .. path segments are rejected with 400. Use percent-encoding to bypass
# the HTTP client's URL normalization; curl/hurl collapse a bare ".."
# before the request goes out.
GET {{host}}/notes/%2E%2E/etc/passwd
HTTP 400
