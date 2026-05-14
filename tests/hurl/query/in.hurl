# __in: one-of, against a list field.
GET {{host}}/query?tags__in=alpha,anwesen
HTTP 200
[Asserts]
jsonpath "$.total" == 2
jsonpath "$.results[*].path" contains "Projects/anwesen.md"
jsonpath "$.results[*].path" contains "Projects/alpha.md"
