# __gt against an RFC 3339 datetime; verifies coerced typed-date comparison.
GET {{host}}/query?when__gt=2026-05-14T00:00:00Z
HTTP 200
[Asserts]
jsonpath "$.total" == 1
jsonpath "$.results[0].path" == "events/c.md"
jsonpath "$.results[0].frontmatter.when" == "2026-05-14T10:14:22Z"
