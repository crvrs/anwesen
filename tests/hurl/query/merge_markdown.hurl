# [ANW-26] Accept: text/markdown on /query merges matched note bodies into
# one document, each fragment preceded by its source marker.
GET {{host}}/query?__anw-path=Projects&kind=PDR
Accept: text/markdown
HTTP 200
[Asserts]
header "Content-Type" == "text/markdown; charset=utf-8"
body contains "<!-- source: Projects/PDR-001-intro.md -->"
body contains "<!-- source: Projects/PDR-002-followup.md -->"
body contains "First PDR."
body contains "Second PDR."
