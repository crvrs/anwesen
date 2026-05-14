# Accept: text/markdown returns the raw file bytes verbatim.
GET {{host}}/notes/README.md
Accept: text/markdown
HTTP 200
[Asserts]
header "Content-Type" startsWith "text/markdown"
header "ETag" exists
body startsWith "---\ntags: [demo, readme]\ndate: 2026-05-01\nstatus: published\n---\n"
body contains "Fixture vault README. Used by the hurl contract harness."
