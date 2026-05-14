# __all: array contains all listed values.
GET {{host}}/query?tags__all=anwesen,project
HTTP 200
[Asserts]
jsonpath "$.total" == 1
jsonpath "$.results[0].path" == "Projects/anwesen.md"
