# [ANW-26] __anw-order=num:desc orders fragments by the `num` key descending,
# so PDR-002 (num 2) precedes PDR-001 (num 1) in the merged body.
GET {{host}}/query?__anw-path=Projects&kind=PDR&__anw-order=num:desc
Accept: text/markdown
HTTP 200
[Asserts]
body matches /(?s)Second PDR.*First PDR/
