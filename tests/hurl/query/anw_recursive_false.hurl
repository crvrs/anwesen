# __anw-recursive=false restricts to direct children of __anw-path.
GET {{host}}/query?__anw-path=Projects&__anw-recursive=false
HTTP 200
[Asserts]
# Four notes live directly under Projects/; none under deeper folders here.
jsonpath "$.total" == 4
jsonpath "$.results[*].path" contains "Projects/anwesen.md"
jsonpath "$.results[*].path" contains "Projects/alpha.md"
jsonpath "$.results[*].path" contains "Projects/PDR-001-intro.md"
jsonpath "$.results[*].path" contains "Projects/PDR-002-followup.md"
