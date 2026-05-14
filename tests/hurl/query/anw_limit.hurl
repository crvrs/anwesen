# __anw-limit caps results; total still reflects the full match count.
GET {{host}}/query?__anw-limit=1&__anw-path=Projects
HTTP 200
[Asserts]
jsonpath "$.results" count == 1
jsonpath "$.total" == 4
jsonpath "$.truncated" == true
