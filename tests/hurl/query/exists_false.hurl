# __exists=false: matches every note that lacks the key.
GET {{host}}/query?deprecated__exists=false&kind__exists=false&author__exists=false&date__exists=false&purpose__exists=false&tag__exists=false&when__exists=false&tags__exists=false&title__exists=false&status__exists=false
HTTP 200
[Asserts]
# no-frontmatter.md is the only note with no keys at all.
jsonpath "$.total" == 1
jsonpath "$.results[0].path" == "no-frontmatter.md"
