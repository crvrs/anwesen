# /notes/ is the vault root listing; dot-directories are invisible.
GET {{host}}/notes/
HTTP 200
[Asserts]
jsonpath "$.path" == ""
jsonpath "$.entries[*].name" not contains ".obsidian"
jsonpath "$.entries[*].name" not contains ".trash"
jsonpath "$.entries[*].name" contains "Projects"
jsonpath "$.entries[*].name" contains "Projects-old"
jsonpath "$.entries[*].name" contains "events"
jsonpath "$.entries[*].name" contains "README.md"
