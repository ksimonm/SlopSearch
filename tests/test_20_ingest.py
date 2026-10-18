import json

import pytest

MAPPING = {"properties": {"title": {"type": "text"}, "n": {"type": "integer"}}}


@pytest.mark.smoke
def test_bulk_index_and_count(client, index_factory, bulk_index):
    name = index_factory(mappings=MAPPING)
    docs = [{"title": f"doc {i}", "n": i} for i in range(10)]

    res = bulk_index(name, docs)
    assert res["errors"] is False
    assert len(res["items"]) == 10

    count = client.get(f"/{name}/_count").json()
    assert count["count"] == 10


def test_bulk_partial_errors(client, index_factory):
    name = index_factory(mappings=MAPPING)
    lines = [
        '{"index":{}}',
        json.dumps({"title": "ok", "n": 1}),
        '{"index":{}}',
        json.dumps({"title": "bad", "n": "not-an-int"}),
    ]
    payload = "\n".join(lines) + "\n"
    r = client.post(
        f"/{name}/_bulk",
        content=payload.encode(),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200
    body = r.json()
    assert body["errors"] is True
    assert body["items"][0]["index"]["status"] == 201
    assert body["items"][1]["index"]["status"] == 400
    assert client.get(f"/{name}/_count").json()["count"] == 1


def test_index_document_by_id(client, index_factory):
    name = index_factory(mappings=MAPPING)
    r = client.put(f"/{name}/_doc/1", json={"title": "first", "n": 1}, params={"refresh": "true"})
    assert r.status_code == 201
    assert client.get(f"/{name}/_doc/1").json()["_source"]["title"] == "first"


def test_update_document_keeps_count(client, index_factory):
    name = index_factory(mappings=MAPPING)
    client.put(f"/{name}/_doc/1", json={"title": "old", "n": 1}, params={"refresh": "true"})

    r = client.post(f"/{name}/_update/1", json={"doc": {"title": "new"}}, params={"refresh": "true"})
    assert r.status_code == 200
    assert client.get(f"/{name}/_doc/1").json()["_source"]["title"] == "new"
    assert client.get(f"/{name}/_count").json()["count"] == 1


def test_delete_document(client, index_factory):
    name = index_factory(mappings=MAPPING)
    client.put(f"/{name}/_doc/1", json={"title": "x", "n": 1}, params={"refresh": "true"})
    assert client.delete(f"/{name}/_doc/1", params={"refresh": "true"}).status_code == 200
    assert client.get(f"/{name}/_count").json()["count"] == 0
