# Anwesen

> _Anwesen_ (German): the premises, the estate -- and, in Heidegger, a coming-to-presence. A read-only HTTP daemon that brings a markdown vault into presence over the network.

Anwesen serves an Obsidian-style markdown vault read-only over HTTP. It walks the notes in place, parses their YAML frontmatter, and lets remote clients query notes by frontmatter fields, read individual notes, and list folders -- with no database, no copy, and no migration of the underlying directory. The same query and merge engine is also available offline as a one-shot CLI.

## Why

Obsidian is the odd one out among knowledge stores in not shipping a native HTTP API. Notion, Confluence, Wiki.js, Outline and the rest expose one out of the box; an Obsidian vault is just a directory of markdown files on disk. Anwesen brings that vault up to the same bar without converting it into something else: the files stay exactly where Obsidian wrote them and remain editable in Obsidian with no coordination. Programs that need the vault's contents stop re-implementing "walk the directory, parse the YAML, filter the notes" -- they ask Anwesen.

## What it does

```
Obsidian vault  ->  Anwesen (walk + frontmatter index)  ->  HTTP/JSON  ->  consumers
```

Anwesen answers three kinds of question over HTTP:

- _Give me this one note_ -- `GET /notes/<path>`
- _List the contents of this folder_ -- `GET /notes/<folder>/`
- _Find every note whose frontmatter matches these predicates_ -- `GET /query?...`

The frontmatter index is built once at startup and kept current by watching the vault directory. The index lives in memory; a restart rebuilds it, and there is nothing on disk to corrupt or migrate.

The same query-and-merge engine also runs offline, with no server: `anwesen merge` walks a directory, evaluates a query, and writes the merged markdown to stdout (see [Local generation](#local-generation)).

## Quick start

Build (Rust, 2024 edition, stable toolchain):

```
cargo build --release
```

Run:

```
anwesen serve --vault /path/to/vault
```

Anwesen binds `127.0.0.1:8080` by default, walks the vault once, then serves the API and watches for changes. Stop it with `SIGINT`/`SIGTERM`; there is nothing on disk to clean up.

Check a vault before serving it:

```
anwesen doctor --vault /path/to/vault
```

Build one file out of many notes, without starting the daemon:

```
anwesen merge --vault /path/to/vault --query 'tags=adr&__anw-order=title' > ADRs.md
```

## CLI

```
anwesen serve  --vault <path> [--bind <addr:port>] [--log-level <level>]
               [--otlp-slow-request-ms <n>]
anwesen doctor --vault <path>
anwesen merge  --vault <path> --query <query-string>
anwesen version
```

| Flag                        | Env var                        | Default                | Meaning                                                                       |
| --------------------------- | ------------------------------ | ---------------------- | ----------------------------------------------------------------------------- |
| `--vault <path>`            | `ANWESEN_VAULT`                | _required_             | Path to the vault root.                                                       |
| `--bind <addr:port>`        | `ANWESEN_BIND`                 | `127.0.0.1:8080`       | Listen address for `serve`.                                                   |
| `--log-level <level>`       | `ANWESEN_LOG_LEVEL`            | `info`                 | `error`, `warn`, `info`, `debug`, or `trace`.                                  |
| `--query <query-string>`    | --                             | _required for `merge`_ | A `/query` query string: frontmatter predicates plus `__anw-` controls.       |
| `--otlp-slow-request-ms`    | `ANWESEN_OTLP_SLOW_REQUEST_MS` | `500`                  | Requests at or over this duration, or answering 5xx, also export a span.      |

Every flag has a matching `ANWESEN_<UPPER>` environment variable. CLI flags win
over env vars.

`--bind` and `--otlp-slow-request-ms` apply to `serve` only. Where telemetry is
exported is configured entirely through the standard `OTEL_` variables below.

- **`serve`** -- run the daemon: walk the vault, build the index, watch for changes, serve the API.
- **`doctor`** -- walk the vault once and report what would stop clean ingestion: unreadable files, unparseable YAML, path collisions on the HTTP surface, and frontmatter type drift (the same key carrying incompatible types across notes). Read-only; non-zero exit if any issue is found.
- **`merge`** -- one-shot local generation: walk the vault, evaluate `--query`, and write the merged markdown document to stdout. No server, no HTTP. See [Local generation](#local-generation).
- **`version`** -- print version and exit.

## Telemetry

anwesen exports OTLP metrics for every request, and a span for requests at or
over `--otlp-slow-request-ms` or answering a 5xx. Export is configured through
the standard OpenTelemetry environment variables, which the SDK reads directly:

| Variable                               | Meaning                                                          |
| -------------------------------------- | ---------------------------------------------------------------- |
| `OTEL_EXPORTER_OTLP_ENDPOINT`          | Base URL. The SDK appends `/v1/metrics` and `/v1/traces`.        |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`  | Full metrics URL, used as given. Overrides the base for metrics. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`   | Full traces URL, used as given. Overrides the base for traces.   |
| `OTEL_EXPORTER_OTLP_HEADERS`           | Export headers, `key=value` comma-separated.                     |
| `OTEL_EXPORTER_OTLP_PROTOCOL`          | `http/protobuf`, the default and the only value this build speaks. |
| `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` | Override the `anwesen` service identity.                |

The per-signal `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` and
`OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` are read the same way.

With none of the three endpoint variables set, telemetry is off: no exporter is
built and the request middleware is not installed.

Two settings fail at startup rather than export nowhere in silence:

- A query or a fragment on `OTEL_EXPORTER_OTLP_ENDPOINT`. The base URL must be
  a base URL; the SDK appends the signal path after whatever it is given.
- A protocol other than `http/protobuf`. The binary ships the OTLP/HTTP
  transport alone, so `grpc` would keep exporting over HTTP with nothing in the
  log. Point the endpoint at the collector's HTTP port, not its gRPC one.

uptrace:

```
OTEL_EXPORTER_OTLP_ENDPOINT=https://api.uptrace.dev
OTEL_EXPORTER_OTLP_HEADERS=uptrace-dsn=https://TOKEN@api.uptrace.dev?grpc=4317
```

Paste the DSN from Project Settings into the header verbatim, tail and all --
it is a credential there, not an address. The endpoint is the host alone.

Removed in 0.3.0: `--uptrace-dsn`, `--otlp-endpoint`, `--otlp-header` and their
`ANWESEN_` variables. Passing any of them fails at startup with the `OTEL_`
replacement to use.

## HTTP API

All endpoints are `GET` and return JSON unless noted.

### `GET /notes/<path>` -- read one note

Returns `{ path, frontmatter, body, last_modified, etag, size }`, where `body` is the markdown with the frontmatter block stripped.

- `Accept: text/markdown` returns the raw file verbatim, frontmatter intact.
- `If-None-Match: "<etag>"` returns `304 Not Modified` when the note is unchanged. ETags are strong -- the BLAKE3 hash of the file's raw bytes -- so they are stable across restarts and unaffected by identical-byte rewrites (format-on-save, sync clients, `git checkout` of the same revision).
- `..` segments in the path are rejected with `400`.

### `GET /notes/<folder>/` -- directory listing

Note the trailing slash; that is how a folder index is distinguished from a note read. Returns the immediate children only (non-recursive), each with name, type (`file`/`dir`), `last_modified`, and size.

### `GET /query?...` -- frontmatter query

Returns `{ results, total, truncated }`. Each query-string `key=value` is a predicate on a frontmatter field; different keys combine with AND.

```
GET /query?tags=anwesen&kind=adr
```

**Field operators** (suffix on the key):

| Suffix                        | Example                    | Meaning                                                    |
| ----------------------------- | -------------------------- | ---------------------------------------------------------- |
| _(none)_                      | `tags=python`              | Exact match on scalars; "contains" if the field is a list. |
| `__in`                        | `tags__in=python,go`       | Any of the listed values matches.                          |
| `__all`                       | `tags__all=python,fastapi` | List contains all listed values.                           |
| `__not`                       | `status__not=draft`        | Negation.                                                  |
| `__exists`                    | `deprecated__exists=false` | Presence / absence of the key.                             |
| `__regex`                     | `title__regex=^PDR-\d+`    | Regex on a scalar (anchor it for speed).                   |
| `__prefix`                    | `path__prefix=Projects`    | String prefix.                                             |
| `__gt` `__gte` `__lt` `__lte` | `date__gte=2026-01-01`     | Ordered comparison; numbers and ISO dates.                 |

ISO-8601 dates and RFC 3339 datetimes are coerced to typed dates at read time, so range operators work on them. Nested keys use dots: `author.name=...`. Unknown operators return `400` rather than being silently ignored. Unanchored substring (`__contains`) is intentionally not provided; use `__regex`.

**Control parameters** (`__anw-` prefix; configure the query rather than constrain matches):

| Parameter                        | Default    | Meaning                                                                                        |
| -------------------------------- | ---------- | ---------------------------------------------------------------------------------------------- |
| `__anw-recursive=<bool>`         | `true`     | Recurse into subdirectories.                                                                   |
| `__anw-path=<prefix>`            | vault root | Restrict matches to a path prefix.                                                             |
| `__anw-limit=<n>`                | no limit   | Cap the result list; `total` still reports the full match count.                               |
| `__anw-order=<key>[:asc\|:desc]` | path order | Order fragments (merge mode only).                                                             |
| `__anw-kind=<key>`               | off        | Refuse a mixed merge unless every matched note shares one value for the key (merge mode only). |

By default `/query` returns metadata only; fetch bodies with `/notes/<path>`.

#### Markdown-merge mode

`Accept: text/markdown` on `/query` returns the **bodies** of all matched notes concatenated into one markdown document, each fragment preceded by an HTML-comment source marker and separated by a blank line. This is the single-request path for building one file out of many notes:

```
<!-- source: ADR-001 Language and Foundation Libraries.md -->
...body...

<!-- source: ADR-002 Filesystem Change Tracking.md -->
...body...
```

`__anw-order` sets fragment order; `__anw-kind` guards against merging notes that disagree on a key (returns `400` naming the offenders). Both evaluate over the full match set before `__anw-limit` truncates.

The same merged document is available offline, without the daemon, via `anwesen merge` (see [Local generation](#local-generation)).

### `GET /health` -- liveness and index freshness

Returns vault path, note count, last index/event timestamps, watcher state, an in-flight-rescan flag, and supervisor restart counters. Always returns `200` while the process is up; for a hard liveness signal use TCP connectivity.

### Status codes

`200` ok | `304` ETag matched | `400` bad request | `404` not found | `500` internal | `503` index not ready (retry shortly).

## Local generation

`anwesen merge` produces the markdown-merge document on the command line, with no server and no HTTP round-trip. It walks the vault, evaluates the query, and writes the merged document to stdout:

```
anwesen merge --vault /path/to/vault --query 'tags=adr&__anw-order=title&__anw-kind=kind'
```

The `--query` string is the exact `/query` grammar: frontmatter predicates plus the `__anw-` controls, including `__anw-order` for fragment order and `__anw-kind` for the homogeneity guard. The output is byte-identical to the HTTP merge mode for the same vault and query -- both run the same engine. A `__anw-kind` violation exits non-zero and names the offending values on stderr; an empty match set writes nothing and exits `0`.

This is the materialization path: build a `CLAUDE.md`, a skill bundle, or any single file assembled from many notes, driven from a script or a one-off shell.

## Design notes

- **In place, read-only.** Anwesen reads the same directory Obsidian writes to and never writes back. The vault stays editable in Obsidian with no coordination, and there is no write API by design.
- **Frontmatter is the index.** Filtering is server-side and first-class; clients never walk-and-parse. Note _bodies_ are served verbatim but not full-text indexed.
- **No authentication.** Anwesen trusts every request it accepts and binds `127.0.0.1` by default. Put a reverse proxy (nginx, caddy, warpgate) in front for TLS and access control. The contract is simple: if you can reach Anwesen, you can read everything it indexes.
- **Standalone.** Anwesen is its own repository with its own release cycle, reusable by any consumer -- not a sub-package of the first thing that used it.

## Implementation

Rust (2024 edition) on Tokio: `axum`/`tower` for HTTP, `clap` for the CLI, `tracing` for logs, `serde` (with `serde_yaml` and `serde_json`) for the data model, `notify` for filesystem watching, and BLAKE3 for ETags. The frontmatter index is evaluated in memory; there is no external search engine and no on-disk index.

## A Note on Agentic Engineering

LLM agents were heavily involved in writing this code. Every change has nonetheless been reviewed by a human, who made a best effort to understand, test, and validate it before it landed. Where the agents and the human disagreed, the human had the final say -- and bears responsibility for the result, bugs included. Read it with the same scrutiny you'd apply to any code: trust, but verify.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
