# __not: negation on a scalar.
GET {{host}}/query?status__not=draft&kind__exists=false
HTTP 200
[Asserts]
jsonpath "$.results[*].path" contains "Projects/anwesen.md"
jsonpath "$.results[*].path" not contains "Projects/alpha.md"
