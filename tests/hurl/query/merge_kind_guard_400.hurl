# [ANW-26] __anw-kind=status rejects a mixed merge: alpha (draft) and anwesen
# (active) carry distinct status values, so the guard returns 400.
GET {{host}}/query?__anw-path=Projects&status__exists=true&__anw-kind=status
Accept: text/markdown
HTTP 400
[Asserts]
body contains "draft"
body contains "active"
