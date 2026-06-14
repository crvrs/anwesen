# [ANW-26] __anw-kind=kind passes when every matched note carries the same
# value (both PDRs have kind: PDR).
GET {{host}}/query?__anw-path=Projects&kind=PDR&__anw-kind=kind
Accept: text/markdown
HTTP 200
[Asserts]
body contains "First PDR."
body contains "Second PDR."
