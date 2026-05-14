# Bare equality on a scalar value.
GET {{host}}/query?status=active
HTTP 200
[Asserts]
jsonpath "$.results" count == 1
jsonpath "$.results[0].path" == "Projects/anwesen.md"
jsonpath "$.results[0].frontmatter.status" == "active"
jsonpath "$.results[0].body" not exists
jsonpath "$.total" == 1
jsonpath "$.truncated" == false
