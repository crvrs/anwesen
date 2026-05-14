# __prefix: string prefix match on a scalar.
GET {{host}}/query?title__prefix=PDR-001
HTTP 200
[Asserts]
jsonpath "$.total" == 1
jsonpath "$.results[0].path" == "Projects/PDR-001-intro.md"
