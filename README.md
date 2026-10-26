<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo/logo-dark.svg">
    <img src="docs/logo/logo-light.svg" alt="SlopSearch" width="420">
  </picture>
</p>

<p align="center"><b>An experimental, OpenSearch-compatible search server.</b><br>
The goal: same REST API, same responses, same errors — in a single small binary.</p>

---

> [!NOTE]
> SlopSearch is under active development and not production-ready yet: part of the API
> is still missing (see [Status](#status)), data lives in memory only, and there is no
> authentication or TLS so far. Until those land, keep it on localhost or a trusted network.

SlopSearch aims to speak the OpenSearch 3.9 REST API closely enough that an existing
client does not notice the difference. There are two independent implementations, one in
**Go** and one in **Rust**, and both are measured against the same black-box test suite.

The suite *is* the specification: about 6,000 tests, each verified against a real
OpenSearch 3.9.0, pinning down status codes, error types, response shapes and quirks.

## Status

Share of the suite passing (2026-10-26, `make go-score` / `make rust-score`):

| Area | Go | Rust |
|---|---:|---:|
| Cluster, nodes, `_cat` | 99.4% | 100% |
| Index administration | 99.4% | 100% |
| Documents, bulk, by-query | 33% | 100% |
| Query DSL (full-text, term-level, compound, geo) | 39–48% | 100% |
| Search API (sort, paging, highlight, scroll, PIT) | 27% | 100% |
| Mappings and field types | 44% | 100% |
| Aggregations | 11–14% | 100% |
| Text analysis, languages | 3–38% | 6–40% |
| Ingest pipelines | 0.5% | 100% |
| Scripting (Painless, search templates) | 1% | 29% |
| **Total** | **33.8%** | **81.1%** |

The Rust server is the one to try: everything outside text analysis and scripting passes.
The remaining work is text analysis (`_analyze`, language analyzers, stemmers, token
filters) and Painless. Both servers keep data in memory only.

## Quick start

Only Docker is needed.

```bash
make go-up                 # Go server on http://localhost:9291
curl localhost:9291
make go-test               # run the whole suite against it
make down                  # stop everything
```

Same for Rust (`make rust-up`, `:9292`) and for the reference OpenSearch
(`make ref-up`, `:9290`). `make help` lists all targets.

## Working on one implementation

The Go and Rust servers share nothing but the test suite. A Go developer never needs a
Rust toolchain and vice versa — the build happens inside Docker.

| | Go | Rust |
|---|---|---|
| Code | [`servers/go`](servers/go/README.md) | [`servers/rust`](servers/rust/README.md) |
| Run the suite | `make go-test` | `make rust-test` |
| One area | `make go-test ARGS="-m aggs"` | `make rust-test ARGS="-m aggs"` |
| One file | `make go-test ARGS=tests/test_41_aggs_bucket.py` | `make rust-test ARGS=tests/test_41_aggs_bucket.py` |
| Matching tests | `make go-test ARGS="-k terms_order"` | `make rust-test ARGS="-k terms_order"` |
| Pass rate per file | `make go-score` | `make rust-score` |
| Native, no Docker | `make go-dev` | `make rust-dev` |

`*-test` rebuilds the image, restarts the server and runs pytest in a container, so
every run sees your latest code. Test files run in 4 parallel workers; set `JOBS=0` to
run sequentially. Failures are reported one line each; add `TB=long` for full
tracebacks when debugging a few tests (`make go-test TB=long ARGS="-k terms_order"`). Reports from `*-score` are
kept in `.score/<server>/`.

The loop: pick a red area, read what the tests expect (and what the reference answers —
`make ref-up`, then `curl localhost:9290/...`), make it green, check nothing else broke.

## The test suite

Each file has a pytest marker, so an area can be run on its own (`ARGS="-m <marker>"`):

| Marker | File | What it covers |
|---|---|---|
| `cluster` | `test_01_cluster.py` | root, health, state, settings, nodes, tasks, `_cat/*` |
| `admin` | `test_11_index_admin.py` | index lifecycle, settings, aliases, templates, rollover, data streams, reindex |
| `documents` | `test_21_documents.py` | CRUD, versioning, routing, `_mget`, `_update`, `_bulk`, by-query APIs |
| `ingest` | `test_22_ingest.py` | ingest pipelines and processors, search pipelines |
| `fulltext` | `test_31_fulltext.py` | match family, multi_match, query_string, intervals, span queries |
| `queries` | `test_32_queries.py` | term-level, bool, function_score, nested, join, geo, percolate |
| `search_api` | `test_33_search_api.py` | sort, paging, scroll, PIT, highlight, collapse, rescore, msearch, templates |
| `aggs` | `test_41_aggs_bucket.py`, `test_42_aggs_metric.py` | bucket, metric and pipeline aggregations |
| `scripting` | `test_43_scripting.py` | Painless, expressions, stored scripts, mustache |
| `analysis` | `test_51_analysis.py` | analyzers, tokenizers, token and char filters |
| `types` | `test_71_field_types.py` | field types and mapping parameters |
| `i18n` | `test_80_languages.py` | ~60 scripts, language analyzers, Unicode corner cases |
| `parity`, `mapping`, `smoke` | `test_00`–`test_90` | the original smaller suites |

Opt-in suites that need more than one server process:

| Marker | File | How to run |
|---|---|---|
| `persistence` | `test_95_persistence.py` | `OS_RESTART_CMD="docker compose restart go" OS_PORT=9291 pytest -m persistence` |
| `multinode`, `crossdc` | `test_96_multinode.py`, `test_97_cross_dc.py` | `servers/opensearch-cluster.sh up` — see the script header |

### Adding a test

A test may only encode what real OpenSearch does. Write it, then prove it against the
reference before committing:

```bash
make ref-test ARGS=tests/test_41_aggs_bucket.py
```

Use unique index names (the `index_factory` fixture does this), clean up anything
cluster-wide you create, and don't depend on timing or test order.

## Reference OpenSearch

`compose.yaml` runs the official `opensearchproject/opensearch:3.9.0` image — the full
distribution with all bundled plugins — with the security plugin disabled, because the
suite talks plain HTTP, and its data kept in RAM. Its plugins make every index
create/delete cost ~0.2 s, so the full suite takes ~5 minutes against it (4 workers) —
more workers overload its cluster manager and requests start timing out.

## Layout

```
compose.yaml           reference OpenSearch, both servers, the test runner
Makefile               all developer commands (make help)
servers/go/            Go implementation
servers/rust/          Rust implementation
servers/*.sh           helpers: restart a server, run a local multi-node topology
tests/                 the compatibility suite (pytest + httpx)
scripts/score.py       pass rate per test file
docs/logo/             logo and icon (SVG)
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security issues: [SECURITY.md](SECURITY.md).

## Ports

| Port | Service | Override in `.env` |
|---|---|---|
| 9290 | reference OpenSearch | `OPENSEARCH_PORT` |
| 9291 | SlopSearch (Go) | `GO_PORT` |
| 9292 | SlopSearch (Rust) | `RUST_PORT` |

The servers themselves listen on `127.0.0.1:9200` by default; set `HOST` and `PORT` to
change that (the Docker images use `HOST=0.0.0.0`, published on localhost only).

## License

[Apache License 2.0](LICENSE).

SlopSearch is an independent project. It is not affiliated with, endorsed by or
sponsored by the OpenSearch Project or the Linux Foundation. OpenSearch is a trademark
of LF Projects, LLC; the name is used here only to describe API compatibility.
