# Trailing slash is a folder listing -- immediate children only.
GET {{host}}/notes/Projects/
HTTP 200
[Asserts]
header "Content-Type" startsWith "application/json"
jsonpath "$.path" == "Projects"
jsonpath "$.entries" count == 4
jsonpath "$.entries[0].name" == "PDR-001-intro.md"
jsonpath "$.entries[0].type" == "file"
jsonpath "$.entries[0].size" > 0
jsonpath "$.entries[1].name" == "PDR-002-followup.md"
jsonpath "$.entries[2].name" == "alpha.md"
jsonpath "$.entries[3].name" == "anwesen.md"
