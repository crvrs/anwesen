---
purpose: fixture-guide
---
# Fixture vault

Hand-crafted for the hurl contract harness; every note here exists to
exercise a specific clause of the [[User Manual]]. Touch with care; many
`*.hurl` files match exact body bytes or count results.

## Coverage map

| Note(s) | Exercised |
|---|---|
| `README.md` | Top-level `application/json` read, ETag, `Accept: text/markdown`, `If-None-Match` 304. |
| `no-frontmatter.md` | Notes without a frontmatter block. |
| `Projects/anwesen.md`, `Projects/alpha.md` | bare-Eq, `__not`, `__exists`, `__in`, `__all` on tags. |
| `Projects/PDR-001-intro.md`, `Projects/PDR-002-followup.md` | `__regex`, `__prefix`, `kind=PDR` grouping. |
| `Projects-old/legacy.md` | `__anw-path` segment-boundary check (`Projects` vs `Projects-old`). |
| `events/a.md`, `events/b.md`, `events/c.md` | `__gt`/`__gte`/`__lt`/`__lte` on ISO-8601 dates and RFC 3339 datetimes. |
| `drift/scalar-tag.md`, `drift/list-tag.md` | Bare-Eq scalar/list unification carve-out per ADR-005. |
| `nested/author-info.md` | Dotted nested-key predicates (`author.name=...`). |
| `.obsidian/`, `.trash/` | Dot-directory ignore rule. |
