# A note without a frontmatter block still serves; frontmatter is {}.
GET {{host}}/notes/no-frontmatter.md
HTTP 200
[Asserts]
jsonpath "$.path" == "no-frontmatter.md"
jsonpath "$.frontmatter" exists
jsonpath "$.frontmatter.tags" not exists
jsonpath "$.body" contains "no frontmatter block"
