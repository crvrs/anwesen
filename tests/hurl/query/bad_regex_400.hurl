# Malformed regex value is rejected with 400 at parse time.
GET {{host}}/query?title__regex=(unclosed
HTTP 400
