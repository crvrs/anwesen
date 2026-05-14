# __exists=true: matches every note that defines the key.
GET {{host}}/query?deprecated__exists=true
HTTP 200
[Asserts]
jsonpath "$.total" == 2
jsonpath "$.results[*].path" contains "Projects/anwesen.md"
jsonpath "$.results[*].path" contains "Projects/alpha.md"
