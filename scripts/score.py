"""Compatibility score: run every test file against one server and print pass rates.

    python scripts/score.py --label go            # uses OS_HOST / OS_PORT
    python scripts/score.py --label rust tests/test_41_aggs_bucket.py

JUnit reports land in .score/<label>/ so two runs can be compared afterwards.
"""

import argparse
import concurrent.futures
import glob
import os
import subprocess
import sys
import xml.etree.ElementTree as ET


def run_file(path, out_dir, timeout):
    name = os.path.basename(path)[:-3]
    xml = os.path.join(out_dir, f"{name}.xml")
    # --tb=no: detailed failure reports cost ~0.3 s each and only the counts matter here
    cmd = [sys.executable, "-m", "pytest", path, "-q", "-p", "no:cacheprovider", "--tb=no", f"--junitxml={xml}"]
    with open(os.path.join(out_dir, f"{name}.log"), "w") as log:
        try:
            subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, timeout=timeout)
        except subprocess.TimeoutExpired:
            log.write(f"\nTIMEOUT after {timeout}s\n")
    if not os.path.exists(xml):
        return name, None
    suite = ET.parse(xml).getroot()
    suite = suite if suite.tag == "testsuite" else suite[0]
    total, failures, errors, skipped = (int(suite.get(k)) for k in ("tests", "failures", "errors", "skipped"))
    ran = total - skipped
    return name, (ran, ran - failures - errors)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--label", required=True, help="name of the server under test (go, rust, opensearch)")
    parser.add_argument("--timeout", type=int, default=900, help="per-file timeout in seconds")
    parser.add_argument("--jobs", type=int, default=1, help="test files run in parallel (0 or 1: one at a time)")
    parser.add_argument("files", nargs="*", help="test files (default: tests/test_*.py)")
    args = parser.parse_args()

    files = args.files or sorted(glob.glob("tests/test_*.py"))
    out_dir = os.path.join(".score", args.label)
    os.makedirs(out_dir, exist_ok=True)

    host = os.getenv("OS_HOST", "localhost")
    port = os.getenv("OS_PORT", "9200")
    print(f"{args.label} @ {host}:{port}\n")
    print(f"{'file':26} {'passed':>7} {'ran':>6} {'rate':>7}")
    passed_all = ran_all = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(args.jobs, 1)) as pool:
        results = pool.map(lambda path: run_file(path, out_dir, args.timeout), files)
    for name, result in results:
        if result is None:
            print(f"{name:26} {'—':>7} {'—':>6}   no report (see {out_dir}/{name}.log)")
            continue
        ran, passed = result
        if ran == 0:
            print(f"{name:26} {'—':>7} {'—':>6}   skipped")
            continue
        passed_all += passed
        ran_all += ran
        print(f"{name:26} {passed:>7} {ran:>6} {100 * passed / ran:>6.1f}%", flush=True)
    if ran_all:
        print(f"\n{'TOTAL':26} {passed_all:>7} {ran_all:>6} {100 * passed_all / ran_all:>6.1f}%")


if __name__ == "__main__":
    main()
