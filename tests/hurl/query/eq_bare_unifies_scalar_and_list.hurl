# Bare-Eq scalar/list unification per ADR-005: tag=python matches both
# `tag: python` and `tag: [python, go]`.
GET {{host}}/query?tag=python
HTTP 200
[Asserts]
jsonpath "$.total" == 2
jsonpath "$.results[*].path" contains "drift/scalar-tag.md"
jsonpath "$.results[*].path" contains "drift/list-tag.md"
