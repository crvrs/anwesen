# Anwesen

Read-only HTTP daemon over a markdown vault. Walks notes in place, parses
YAML frontmatter, watches the directory for changes, and answers three kinds
of question over JSON: read one note, list a folder, query by frontmatter.

See the User Manual and ADRs in the project's design vault for the contract
and rationale.

## Subcommands

```
anwesen serve  --vault <path> [--bind <addr:port>] [--log-level <level>]
anwesen doctor --vault <path> [--log-level <level>]
anwesen version
```

Each flag has a matching `ANWESEN_<UPPER>` environment variable; CLI wins
over env.

## Development

```
scripts/ci.sh    # fmt + clippy + test
```

## License

Dual-licensed under MIT or Apache-2.0 at your option.
