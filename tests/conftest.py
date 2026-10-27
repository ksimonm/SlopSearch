import json
import os
import time
import uuid

import httpx
import pytest

OS_HOST = os.getenv("OS_HOST", "localhost")
OS_PORT = int(os.getenv("OS_PORT", "9200"))
BASE_URL = f"http://{OS_HOST}:{OS_PORT}"
TIMEOUT = float(os.getenv("OS_TIMEOUT", "30"))

# OS_RECORD=<dir>: write every request/response of each test to <dir>/<test file>.jsonl.
# `make ref-record` uses it to snapshot what the reference OpenSearch answers.
RECORD_DIR = os.getenv("OS_RECORD")
# Where `make ref-record` keeps the snapshot; failing tests show the reference exchange.
REFERENCE_DIR = os.path.join(os.path.dirname(__file__), "reference")
MAX_BODY = 8000
_current_test = None


def _text(raw):
    text = raw.decode("utf-8", "replace")
    return text if len(text) <= MAX_BODY else text[:MAX_BODY] + f"... [{len(text)} chars]"


def _record(response):
    if _current_test is None:
        return
    response.read()
    request = response.request
    url = request.url
    entry = {
        "test": _current_test.split("::", 1)[1],
        "request": f"{request.method} {url.raw_path.decode()}",
        "status": response.status_code,
    }
    if request.content:
        entry["body"] = _text(request.content)
    entry["response"] = _text(response.content)
    name = os.path.basename(_current_test.split("::", 1)[0])[:-3]
    with open(os.path.join(RECORD_DIR, f"{name}.jsonl"), "a", encoding="utf-8") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_protocol(item, nextitem):
    global _current_test
    _current_test = item.nodeid
    yield
    _current_test = None


def _reference_exchange(nodeid):
    path, _, test = nodeid.partition("::")
    file = os.path.join(REFERENCE_DIR, os.path.basename(path)[:-3] + ".jsonl")
    if not os.path.exists(file):
        return None
    lines = []
    with open(file, encoding="utf-8") as f:
        for line in f:
            e = json.loads(line)
            if e["test"] == test:
                lines.append(f"{e['request']}\n" + (f"{e['body']}\n" if "body" in e else "")
                             + f"-> {e['status']} {e['response']}")
    return "\n\n".join(lines) if lines else None


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    outcome = yield
    report = outcome.get_result()
    if report.failed and not RECORD_DIR:
        exchange = _reference_exchange(item.nodeid)
        if exchange:
            report.sections.append(("what the reference OpenSearch 3.9.0 answered", exchange))


@pytest.fixture(scope="session")
def base_url():
    return BASE_URL


@pytest.fixture(scope="session")
def client():
    hooks = {"response": [_record]} if RECORD_DIR else {}
    with httpx.Client(base_url=BASE_URL, timeout=TIMEOUT, event_hooks=hooks) as c:
        yield c


@pytest.fixture(scope="session", autouse=True)
def wait_for_cluster(client):
    deadline = time.time() + 60
    last = "no response"
    while time.time() < deadline:
        try:
            r = client.get(
                "/_cluster/health",
                params={"wait_for_status": "yellow", "timeout": "5s"},
            )
            if r.status_code == 200:
                return r.json()
            last = f"{r.status_code}: {r.text}"
        except httpx.HTTPError as exc:
            last = str(exc)
        time.sleep(2)
    pytest.fail(f"OpenSearch is unavailable at {BASE_URL}: {last}")


@pytest.fixture
def index_factory(client):
    created = []

    def _create(name=None, body=None, mappings=None, settings=None):
        name = name or f"slopensearch-test-{uuid.uuid4().hex[:8]}"
        payload = dict(body or {})
        if mappings is not None:
            payload["mappings"] = mappings
        if settings is not None:
            payload["settings"] = settings
        r = client.put(f"/{name}", json=payload)
        assert r.status_code in (200, 201), r.text
        created.append(name)
        return name

    yield _create

    for name in created:
        client.delete(f"/{name}", params={"ignore_unavailable": "true"})


@pytest.fixture
def bulk_index(client):
    def _bulk(index, docs, refresh=True):
        lines = []
        for doc in docs:
            lines.append('{"index":{}}')
            lines.append(json.dumps(doc, ensure_ascii=False))
        payload = "\n".join(lines) + "\n"
        r = client.post(
            f"/{index}/_bulk",
            content=payload.encode(),
            headers={"Content-Type": "application/x-ndjson"},
            params={"refresh": "true" if refresh else "false"},
        )
        assert r.status_code == 200, r.text
        return r.json()

    return _bulk
