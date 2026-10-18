import pytest

pytestmark = pytest.mark.parity

MAPPING = {
    "properties": {
        "title": {"type": "text"},
        "category": {"type": "keyword"},
        "price": {"type": "integer"},
    }
}

DOCS = [
    {"title": "Red Apple", "category": "fruit", "price": 10},
    {"title": "Green Apple", "category": "fruit", "price": 20},
    {"title": "Banana", "category": "tool", "price": 30},
]


@pytest.fixture
def idx(client, index_factory, bulk_index):
    name = index_factory(mappings=MAPPING)
    bulk_index(name, DOCS)
    return name


def _search(client, index, body):
    return client.post(f"/{index}/_search", json=body)


def _total(resp):
    return resp.json()["hits"]["total"]["value"]


def test_count_respects_query_body(client, idx):
    r = client.post(f"/{idx}/_count", json={"query": {"term": {"category": "fruit"}}})
    assert r.status_code == 200
    assert r.json()["count"] == 2


def test_match_operator_and(client, idx):
    body = {"query": {"match": {"title": {"query": "red apple", "operator": "and"}}}}
    assert _total(_search(client, idx, body)) == 1


def test_term_on_text_is_exact_and_case_sensitive(client, idx):
    assert _total(_search(client, idx, {"query": {"term": {"title": "Apple"}}})) == 0
    assert _total(_search(client, idx, {"query": {"term": {"title": "apple"}}})) == 2


def test_match_on_keyword_is_case_sensitive(client, idx):
    assert _total(_search(client, idx, {"query": {"match": {"category": "fruit"}}})) == 2
    assert _total(_search(client, idx, {"query": {"match": {"category": "Fruit"}}})) == 0


def test_analyze_keyword_analyzer(client, idx):
    r = client.post(f"/{idx}/_analyze", json={"analyzer": "keyword", "text": "Hello World"})
    assert r.status_code == 200
    assert [t["token"] for t in r.json()["tokens"]] == ["Hello World"]


def test_second_put_doc_is_update(client, idx):
    r1 = client.put(f"/{idx}/_doc/1", json={"title": "A"}, params={"refresh": "true"})
    assert r1.status_code == 201
    assert r1.json()["result"] == "created"
    r2 = client.put(f"/{idx}/_doc/1", json={"title": "B"}, params={"refresh": "true"})
    assert r2.status_code == 200
    assert r2.json()["result"] == "updated"


def test_bulk_create_conflict(client, idx):
    payload = (
        '{"create":{"_index":"%s","_id":"dup"}}\n{"title":"A"}\n'
        '{"create":{"_index":"%s","_id":"dup"}}\n{"title":"B"}\n'
    ) % (idx, idx)
    r = client.post(
        "/_bulk",
        content=payload.encode(),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200
    body = r.json()
    assert body["errors"] is True
    assert body["items"][0]["create"]["status"] == 201
    assert body["items"][1]["create"]["status"] == 409


def test_update_missing_doc_returns_404(client, idx):
    r = client.post(f"/{idx}/_update/missing-doc", json={"doc": {"title": "x"}})
    assert r.status_code == 404


def test_prefix_query_supported(client, idx):
    assert _total(_search(client, idx, {"query": {"prefix": {"title": "app"}}})) == 2


def test_source_false_omits_source(client, idx):
    r = _search(client, idx, {"query": {"match_all": {}}, "size": 1, "_source": False})
    assert "_source" not in r.json()["hits"]["hits"][0]


def test_terms_agg_respects_size(client, idx):
    body = {"size": 0, "aggs": {"c": {"terms": {"field": "category", "size": 1}}}}
    assert len(_search(client, idx, body).json()["aggregations"]["c"]["buckets"]) == 1


def test_terms_agg_on_text_field_requires_fielddata(client, idx):
    r = _search(client, idx, {"size": 0, "aggs": {"t": {"terms": {"field": "title"}}}})
    assert r.status_code == 400


def test_sort_on_text_field_requires_fielddata(client, idx):
    r = _search(client, idx, {"query": {"match_all": {}}, "sort": [{"title": "asc"}]})
    assert r.status_code == 400
