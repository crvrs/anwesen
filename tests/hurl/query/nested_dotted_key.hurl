# Nested key addressed via dotted path: author.name=brn.
GET {{host}}/query?author.name=brn
HTTP 200
[Asserts]
jsonpath "$.total" == 1
jsonpath "$.results[0].path" == "nested/author-info.md"
jsonpath "$.results[0].frontmatter.author.name" == "brn"
jsonpath "$.results[0].frontmatter.author.role" == "engineer"
