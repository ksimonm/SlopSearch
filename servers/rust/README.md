# SlopSearch — Rust implementation

`axum` + `tokio` + `serde_json` (plus `regex` and `base64`); the server keeps all data
in memory behind one `Arc<Mutex<Store>>`. Much of the query and aggregation code is a port
of the Go server, which remains a useful second reading of the expected behaviour.

## Loop

```bash
make rust-test ARGS="-m aggs"                       # rebuild image, restart, run one area
make rust-test ARGS=tests/test_41_aggs_bucket.py    # one file
make rust-test ARGS="-k terms_order"                # matching tests only
make rust-score                                     # pass rate per test file
```

Without Docker (needs Rust stable and a Python venv with `requirements.txt`):

```bash
make rust-dev                                       # serves :9292
OS_PORT=9292 .venv/bin/pytest -m aggs
```

To see what OpenSearch really answers for a request, run it against the reference:
`make ref-up`, then `curl localhost:9290/...`.

## Layout

| File | Area |
|---|---|
| `main.rs` | entry point, request abstraction (`Req`), routing |
| `store.rs` | indices, documents, node/cluster constants |
| `shared.rs`, `errors.rs`, `resp.rs` | name validation and index resolution, error shapes, response formats (`filter_path`, YAML, ...) |
| `meta.rs`, `cluster.rs`, `nodes.rs`, `cat.rs` | `_cluster`, `_nodes`, `_tasks`, `_remote`, `_cat` |
| `admin.rs`, `fields.rs` | index create/delete/get, settings, mappings, index-time field extraction |
| `alias.rs`, `templates.rs`, `datastreams.rs`, `rollover.rs` | aliases, index templates, data streams, rollover |
| `resize.rs`, `reindex.rs`, `resolve_index.rs`, `maintenance.rs` | clone/shrink/split, `_reindex`, `_resolve/index`, refresh/flush/forcemerge |
| `docs.rs` | document CRUD, `_mget`, `_bulk`, by-query APIs |
| `search.rs`, `eval.rs`, `eval_extra.rs`, `highlight.rs`, `suggest.rs` | search API, query evaluation, highlighting, suggesters |
| `aggs.rs`, `hdr.rs` | aggregations; HDR histogram for percentiles |
| `ingest.rs`, `ingest_script.rs`, `hashes.rs` | ingest pipelines and processors, their script subset, fingerprint hashes |
| `painless.rs`, `painless_exec.rs`, `scripts.rs`, `render.rs` | Painless subset, `_scripts/painless/_execute`, stored scripts, Mustache templates |

## Status

64.5% of the suite passes (see the table in the [main README](../../README.md#status)).
The weakest areas are text analysis (`_analyze`, analyzers, token filters — `test_51`,
`test_80`), Painless scripting (`test_43`) and the rarer query and field types
(`test_31`, `test_32`, `test_71`); `make rust-score` shows where to start. The build
still has dead-code warnings; cleaning them up is welcome.

## Rules

- The test suite is the spec. Match OpenSearch byte for byte where tests check it:
  status codes, `error.type`, key names and order.
- Don't edit `tests/` to make a test pass. If you believe a test is wrong, prove it with
  `make ref-test ARGS=<test>` against the reference.
- Rust-only changes stay in `servers/rust/`.
