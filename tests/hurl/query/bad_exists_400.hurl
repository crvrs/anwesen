# __exists value that isn't a bool is rejected with 400.
GET {{host}}/query?deprecated__exists=maybe
HTTP 400
