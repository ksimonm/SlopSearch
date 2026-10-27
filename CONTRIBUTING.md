# Contributing to SlopenSearch

Thanks for helping. SlopenSearch has one rule that everything else follows from:
**the test suite is the specification.** Each test encodes what a real OpenSearch 3.9.0
does, and a server is correct exactly when the tests pass.

Only Docker is needed; `make help` lists every command.

## Making a test pass (server work)

1. Pick a red area: `make go-score` (or `rust-score`) shows the pass rate per file.
2. Read what the tests expect, and what the real OpenSearch answered:
   `make ref-show T=<test name>`. No OpenSearch needs to run; the answers are recorded
   in `tests/reference/`.
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
else. Start the agent in the repository root, pick a server and a test file, and replace
`<server>` (`go` or `rust`) and `<file>` in the prompt below. Where the work is now:

| Server | Open test files |
|---|---|
| Go | `test_32_queries`, `test_71_field_types`, `test_43_scripting`, `test_51_analysis`, `test_80_languages` |
| Rust | `test_43_scripting`, `test_51_analysis`, `test_80_languages` |

`make <server>-score` shows the current numbers.

```text
You are working on SlopenSearch, a server that must answer the OpenSearch 3.9 REST API
exactly like OpenSearch does. Read README.md, CONTRIBUTING.md and
servers/<server>/README.md first.

Task: make more tests in tests/<file>.py pass against the <server> server.

How to work:
1. Run `make <server>-score` once and note the pass rate of every test file.
2. Run `make <server>-test ARGS=tests/<file>.py` and group the failures by cause.
   Pick the group that unblocks the most tests and fix it, then the next one.
3. For every failing test, read what the real OpenSearch answered:
   `make ref-show T=<test name>`, or grep tests/reference/<file>.jsonl. Match its status
   code, error type, key names and key order exactly. Don't start OpenSearch yourself.
4. Iterate with `make <server>-test TB=short ARGS="tests/<file>.py -k <expr>"`; failures
   then print the reference exchange as well.
5. At the end, run `make <server>-score` again and compare with step 1. No other file may
   lose passing tests.

Hard rules:
- Never edit anything in tests/ (including tests/reference/). If you believe a test is
  wrong, stop and tell me which test and why, quoting its recorded reference answer.
- Only change files in servers/<server>/.
- No new dependencies without asking me (the Go server uses the standard library only).
- Don't special-case test index names or test inputs; implement the real behaviour.

Finish with a short report: which behaviours you implemented, the pass rate of
<file>.py before and after, the total before and after, and anything you
left unfinished.
```

When one server already passes a file the other one doesn't, you can add to the prompt:
"servers/<other>/ already passes this file; read it as a reference for the behaviour, but
write idiomatic code for this server." Before opening a pull request, read the diff
yourself: you are the author, not the agent.

## Adding or fixing a test

A test may only encode what real OpenSearch does. This is the one kind of work that needs
the reference OpenSearch running (Docker, ~2 GB RAM). Write the test, prove it against the
reference, and re-record that file's reference answers before opening a pull request:

```bash
make ref-test ARGS=tests/test_41_aggs_bucket.py
make ref-record ARGS=tests/test_41_aggs_bucket.py
```

If you can't run the reference, open the pull request anyway and say so; a maintainer
will verify and record it.

- Use unique index names (the `index_factory` fixture does this).
- Clean up anything cluster-wide you create (templates, pipelines, cluster settings).
- Don't depend on timing or on test order; files run in parallel workers.
- Put the test in the file and marker of its area (see the table in the README).

Commit the updated `tests/reference/*.jsonl` together with the test change.

## Reporting a compatibility gap

Open an issue with the request, what OpenSearch 3.9.0 answers, and what SlopenSearch
answers. A failing test in a pull request is even better.

## Pull requests

Keep them focused on one area, describe what changed in the pass rate (`make go-score` /
`make rust-score` before and after). By contributing you agree that your contribution is
licensed under the [Apache License 2.0](LICENSE).
