# A literal comma inside an __in value goes in percent-encoded as %2C.
# Smoke: confirm the request shape is accepted (no match is fine).
GET {{host}}/query?title__in=PDR-001%2CIntro
HTTP 200
[Asserts]
jsonpath "$.total" == 0
