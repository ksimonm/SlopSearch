# SlopenSearch — Go implementation

Standard library only (`net/http`, `encoding/json`), no third-party modules.
Everything is in `package main`; the server keeps all data in memory.

## Loop

```bash
make go-test ARGS="-m aggs"                       # rebuild image, restart, run one area
make go-test ARGS=tests/test_41_aggs_bucket.py    # one file
make go-test ARGS="-k terms_order"                # matching tests only
make go-score                                     # pass rate per test file
```

Without Docker (needs Go 1.25 and a Python venv with `requirements.txt`):

```bash
make go-dev                                       # serves :9291
OS_PORT=9291 .venv/bin/pytest -m aggs
```

To see what OpenSearch really answers for a request, run it against the reference:
`make ref-up`, then `curl localhost:9290/...`.

## Layout

| File | Area |
|---|---|
| `server.go` | HTTP entry point, routing, ordered JSON rendering |
| `model.go` | indices, documents, node/cluster constants |
| `meta.go` | index pattern resolution, cluster/meta routes |
| `admin.go` | index create/delete/get, settings |
| `maps.go`, `fields.go` | mappings, index-time field extraction |
| `alias.go`, `templates.go`, `datastreams.go`, `rollover.go` | aliases, index templates, data streams, rollover |
| `resize.go`, `reindex.go`, `resolve_index.go`, `maintenance.go` | clone/shrink/split, `_reindex`, `_resolve/index`, refresh/flush/forcemerge |
| `documents_v2.go`, `bulk_v2.go`, `byquery.go`, `termvectors.go` | document CRUD, `_bulk`, by-query APIs, term vectors |
| `search.go`, `eval.go`, `eval_extra.go`, `highlight.go`, `explain.go`, `validate.go` | search API, query evaluation, highlighting, `_explain`, `_validate` |
| `scroll.go`, `pit.go`, `msearch.go`, `search_templates.go`, `rank_eval.go` | scroll, point in time, `_msearch`, search templates, `_rank_eval` |
| `search_pipelines.go`, `search_shards.go`, `shard_routing.go`, `field_caps.go` | search pipelines, `_search_shards`, routing, `_field_caps` |
| `aggs.go`, `aggs_bucket.go`, `aggs_metric.go`, `aggs_significant.go`, `aggs_extra.go`, `suggest.go` | aggregations, suggesters |
| `ingest.go`, `pipeline.go`, `ingest_processors.go`, `ingest_extra.go` | ingest pipelines and processors |
| `cat.go`, `nodes.go`, `resp.go` | `_cat`, `_nodes`/`_cluster`, response formats (`filter_path`, YAML, ...) |
| `painless.go`, `script_eval.go`, `scripts.go` | Painless/expression subset, stored scripts, `script_fields` |
| `analysis.go` | `_analyze` and analyzers (mostly open work) |

## Rules

- The test suite is the spec. Match OpenSearch byte for byte where tests check it:
  status codes, `error.type`, key names and order.
- Don't edit `tests/` to make a test pass. If you believe a test is wrong, prove it with
  `make ref-test ARGS=<test>` against the reference.
- Go-only changes stay in `servers/go/`.
