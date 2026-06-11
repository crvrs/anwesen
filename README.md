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

## Deployment

A systemd unit and an example reverse-proxy config live in [`deploy/`](deploy/).
Anwesen binds `127.0.0.1` and ships no authentication -- the proxy is the
access boundary. See [`deploy/README.md`](deploy/README.md).

## Development

```
scripts/ci.sh    # fmt + clippy + test + hurl contract harness
```

### HTTP contract tests

End-to-end HTTP behaviour is verified by [hurl](https://hurl.dev/) per
ADR-008. Install once:

```
# Ubuntu / Debian
curl -sSL -o /tmp/hurl.deb https://github.com/Orange-OpenSource/hurl/releases/download/8.0.0/hurl_8.0.0_amd64.deb
sudo dpkg -i /tmp/hurl.deb

# Pinned at hurl 8.0.0. Other recent 8.x releases also work but are not
# what CI runs.
```

Then `scripts/ci.sh` -- or `tests/run-hurl.sh` to run just the contract
suite (it builds the debug binary if missing, boots `anwesen serve` on
`127.0.0.1:18086`, waits for `/health`, runs every `tests/hurl/**/*.hurl`,
and tears the daemon down on exit). Override the port with
`ANWESEN_TEST_PORT=<n>` if 18086 clashes locally.

## License

Dual-licensed under MIT or Apache-2.0 at your option.
