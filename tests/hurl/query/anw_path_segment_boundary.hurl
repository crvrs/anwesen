# __anw-path=Projects must NOT match Projects-old/legacy.md -- segment-
# boundary anchoring per the ANW-22 fix.
GET {{host}}/query?__anw-path=Projects
HTTP 200
[Asserts]
jsonpath "$.results[*].path" contains "Projects/anwesen.md"
jsonpath "$.results[*].path" contains "Projects/alpha.md"
jsonpath "$.results[*].path" not contains "Projects-old/legacy.md"
