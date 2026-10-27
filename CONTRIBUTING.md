# Contributing to SlopenSearch

Thanks for helping. SlopenSearch has one rule that everything else follows from:
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
are the open work in the Rust server. In the Go server, query types (`test_32`) and
field types (`test_71`) are open too, and the Rust server is a ready-made reference for
how they behave.

## Working with an AI coding agent

Much of SlopenSearch is written with AI coding agents, and you are welcome to use one.
The suite keeps the work honest, as long as the agent follows the same rules as everyone
else. Start the agent in the repository root and adapt this prompt: pick the server, the
test file and, optionally, a narrower `-k` expression.

```text
You are working on SlopenSearch, a server that must answer the OpenSearch 3.9 REST API
exactly like OpenSearch does. Read README.md, CONTRIBUTING.md and servers/rust/README.md
first.

Task: make more tests in tests/test_51_analysis.py pass against the Rust server.

How to work:
1. Run `make rust-score` once and note the pass rate of every test file.
2. Run `make rust-test ARGS=tests/test_51_analysis.py` and group the failures by cause.
   Pick the group that unblocks the most tests and fix it, then the next one.
3. When it is unclear what OpenSearch answers, ask the reference: run `make ref-up`
   and `curl localhost:9290/...`. Match its status code, error type, key names and key
   order exactly.
4. Iterate with `make rust-test ARGS="tests/test_51_analysis.py -k <expr>"`.
5. At the end, run `make rust-score` again and compare with step 1. No other file may
   lose passing tests.

Hard rules:
- Never edit anything in tests/. If you believe a test is wrong, stop and tell me which
  test and why, with the reference answer from `make ref-test ARGS=<test>`.
- Only change files in servers/rust/.
- No new dependencies without asking me.
- Don't special-case test index names or test inputs; implement the real behaviour.

Finish with a short report: which behaviours you implemented, the pass rate of
test_51_analysis.py before and after, the total before and after, and anything you
left unfinished.
```

For the Go server, replace `rust` with `go` in the commands and `servers/rust/` with
`servers/go/` (standard library only, no dependencies at all). Before opening a pull
request, read the diff yourself: you are the author, not the agent.

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

Mention in the pull request that you ran the changed tests against the reference.

## Reporting a compatibility gap

Open an issue with the request, what OpenSearch 3.9.0 answers, and what SlopenSearch
answers. A failing test in a pull request is even better.

## Pull requests

Keep them focused on one area, describe what changed in the pass rate (`make go-score` /
`make rust-score` before and after). By contributing you agree that your contribution is licensed under the
[Apache License 2.0](LICENSE).
