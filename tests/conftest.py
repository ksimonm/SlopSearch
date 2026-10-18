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


@pytest.fixture(scope="session")
def base_url():
    return BASE_URL


@pytest.fixture(scope="session")
def client():
    with httpx.Client(base_url=BASE_URL, timeout=TIMEOUT) as c:
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
        name = name or f"slopsearch-test-{uuid.uuid4().hex[:8]}"
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
