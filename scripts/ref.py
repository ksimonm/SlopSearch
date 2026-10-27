"""What the reference OpenSearch 3.9.0 answers, recorded per test in tests/reference/.

    python scripts/ref.py show terms_order             # tests whose name contains the text
    python scripts/ref.py show test_41_aggs_bucket.py::test_terms_order_by_count
    python scripts/ref.py record                       # re-record everything (needs the reference)
    python scripts/ref.py record tests/test_41_aggs_bucket.py

Each line of tests/reference/<file>.jsonl is one HTTP exchange of one test: the request,
its body, the status code and the response body (long bodies are cut at 8000 chars).
Index names contain random suffixes, so they differ from run to run.
"""

import argparse
import glob
import json
import os
import subprocess
import sys

REFERENCE_DIR = os.path.join("tests", "reference")


def pretty(text):
    try:
        return json.dumps(json.loads(text), indent=2, ensure_ascii=False)
    except ValueError:
        return text


def show(expr):
    file_filter, _, name = expr.rpartition("::")
    file_filter = os.path.basename(file_filter)[:-3] if file_filter.endswith(".py") else file_filter
    found = 0
    current = None
    for path in sorted(glob.glob(os.path.join(REFERENCE_DIR, "*.jsonl"))):
        stem = os.path.basename(path)[:-6]
        if file_filter and file_filter != stem:
            continue
        with open(path, encoding="utf-8") as f:
            for line in f:
                e = json.loads(line)
                if name not in e["test"]:
                    continue
                test = f"{stem}.py::{e['test']}"
                if test != current:
                    current = test
                    found += 1
                    print(f"\n=== {test}")
                print(f"\n{e['request']}")
                if "body" in e:
                    print(pretty(e["body"]))
                print(f"-> {e['status']}")
                print(pretty(e["response"]))
    if not found:
        sys.exit(f"no recorded test matches {expr!r} (see {REFERENCE_DIR}/)")


def record(files, jobs):
    """Run the suite against the reference with OS_RECORD set (OS_HOST/OS_PORT pick it)."""
    os.makedirs(REFERENCE_DIR, exist_ok=True)
    targets = files or sorted(glob.glob(os.path.join("tests", "test_*.py")))
    for t in targets:
        out = os.path.join(REFERENCE_DIR, os.path.basename(t)[:-3] + ".jsonl")
        if os.path.exists(out):
            os.remove(out)
    cmd = [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", "--tb=line",
           "-n", str(jobs), "--dist", "loadfile", *(files or [])]
    env = dict(os.environ, OS_RECORD=REFERENCE_DIR)
    sys.exit(subprocess.call(cmd, env=env))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_show = sub.add_parser("show", help="print the recorded exchanges of matching tests")
    p_show.add_argument("expr", help="part of a test name, optionally prefixed with <file>.py::")
    p_rec = sub.add_parser("record", help="re-record against the reference OpenSearch")
    p_rec.add_argument("--jobs", type=int, default=4)
    p_rec.add_argument("files", nargs="*", help="test files (default: all)")
    args = parser.parse_args()
    if args.cmd == "show":
        show(args.expr)
    else:
        record(args.files, args.jobs)


if __name__ == "__main__":
    main()
