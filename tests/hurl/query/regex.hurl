# __regex: anchored pattern on a scalar.
GET {{host}}/query?title__regex=^PDR-%5Cd%2B
HTTP 200
[Asserts]
jsonpath "$.total" == 2
jsonpath "$.results[*].path" contains "Projects/PDR-001-intro.md"
jsonpath "$.results[*].path" contains "Projects/PDR-002-followup.md"
