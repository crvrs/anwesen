# __gte / __lte against ISO-8601 dates.
GET {{host}}/query?date__gte=2026-02-01&date__lte=2026-04-01
HTTP 200
[Asserts]
jsonpath "$.total" == 1
jsonpath "$.results[0].path" == "events/b.md"
