# Contributing to SlopSearch

Thanks for helping. SlopSearch has one rule that everything else follows from:
**the test suite is the specification.** Each test encodes what a real OpenSearch 3.9.0
does, and a server is correct exactly when the tests pass.

Only Docker is needed; `make help` lists every command.

## Making a test pass (server work)

1. Pick a red area: `make go-score` (or `rust-score`) shows the pass rate per file.
2. Read what the tests expect, and what the reference really answers:
   `make ref-up`, then `curl localhost:9290/...`.
3. Make it green: `make go-test ARGS="-k <expr>"`.
4. Check nothing else broke: `make go-score` and compare with the numbers before.

Rules:

- Don't edit `tests/` to make a server pass. If you believe a test is wrong, prove it with
  `make ref-test ARGS=<test>` and fix the test in a separate change.
- Match OpenSearch byte for byte where tests check it: status codes, `error.type`, key
  names and order.
- The Go and Rust servers share nothing but the suite. Keep Go-only changes in
  `servers/go/` and Rust-only changes in `servers/rust/`.
- Go: standard library only, `gofmt` clean. Rust: keep `cargo build` free of new warnings
  in the code you touch.

Good areas to pick up: text analysis (`test_51`, `test_80`) and Painless (`test_43`)
are the open work in the Rust server. In the Go server, query types (`test_32`) and field types (`test_71`) are open too, and the Rust
server is a ready-made reference for how they behave.

## Adding or fixing a test

A test may only encode what real OpenSearch does. Write it, then prove it against the
reference before opening a pull request:

```bash
make ref-test ARGS=tests/test_41_aggs_bucket.py
```

- Use unique index names (the `index_factory` fixture does this).
- Clean up anything cluster-wide you create (templates, pipelines, cluster settings).
- Don't depend on timing or on test order; files run in parallel workers.
- Put the test in the file and marker of its area (see the table in the README).

CI runs every change to `tests/` against the reference OpenSearch.

## Reporting a compatibility gap

Open an issue with the request, what OpenSearch 3.9.0 answers, and what SlopSearch
answers. A failing test in a pull request is even better.

## Pull requests

Keep them focused on one area, describe what changed in the pass rate, and make sure CI
is green. By contributing you agree that your contribution is licensed under the
[Apache License 2.0](LICENSE).
