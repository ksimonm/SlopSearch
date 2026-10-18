import pytest

MAPPING = {
    "properties": {
        "title": {"type": "text"},
        "category": {"type": "keyword"},
        "price": {"type": "integer"},
    }
}

DOCS = [
    {"title": "red apple", "category": "fruit", "price": 10},
    {"title": "green apple", "category": "fruit", "price": 15},
    {"title": "banana", "category": "fruit", "price": 7},
    {"title": "hammer", "category": "tool", "price": 30},
    {"title": "screwdriver", "category": "tool", "price": 25},
]


@pytest.fixture
def seeded(client, index_factory, bulk_index):
    name = index_factory(mappings=MAPPING)
    bulk_index(name, DOCS)
    return name


def _search(client, index, body):
    r = client.post(f"/{index}/_search", json=body)
    assert r.status_code == 200, r.text
    return r.json()


@pytest.mark.smoke
def test_match_query(client, seeded):
    res = _search(client, seeded, {"query": {"match": {"title": "apple"}}})
    assert res["hits"]["total"]["value"] == 2


def test_term_query_on_keyword(client, seeded):
    res = _search(client, seeded, {"query": {"term": {"category": "tool"}}})
    assert res["hits"]["total"]["value"] == 2


def test_range_query(client, seeded):
    res = _search(client, seeded, {"query": {"range": {"price": {"gte": 15, "lte": 25}}}})
    assert res["hits"]["total"]["value"] == 2


def test_bool_query(client, seeded):
    body = {
        "query": {
            "bool": {
                "must": [{"match": {"title": "apple"}}],
                "filter": [{"term": {"category": "fruit"}}],
            }
        }
    }
    res = _search(client, seeded, body)
    assert res["hits"]["total"]["value"] == 2


def test_sort_and_pagination(client, seeded):
    body = {
        "query": {"match_all": {}},
        "sort": [{"price": "asc"}],
        "size": 2,
        "from": 0,
    }
    res = _search(client, seeded, body)
    prices = [h["_source"]["price"] for h in res["hits"]["hits"]]
    assert prices == [7, 10]


def test_highlight(client, seeded):
    body = {"query": {"match": {"title": "apple"}}, "highlight": {"fields": {"title": {}}}}
    res = _search(client, seeded, body)
    assert "highlight" in res["hits"]["hits"][0]


def test_source_filtering(client, seeded):
    body = {"query": {"match_all": {}}, "_source": ["title"], "size": 1}
    res = _search(client, seeded, body)
    assert set(res["hits"]["hits"][0]["_source"].keys()) == {"title"}


def test_no_hits(client, seeded):
    res = _search(client, seeded, {"query": {"term": {"category": "unknown"}}})
    assert res["hits"]["total"]["value"] == 0
    assert res["hits"]["hits"] == []


@pytest.mark.smoke
def test_aggregation_terms(client, seeded):
    body = {"size": 0, "aggs": {"by_cat": {"terms": {"field": "category"}}}}
    res = _search(client, seeded, body)
    buckets = {b["key"]: b["doc_count"] for b in res["aggregations"]["by_cat"]["buckets"]}
    assert buckets == {"fruit": 3, "tool": 2}


def test_aggregation_metric(client, seeded):
    body = {"size": 0, "aggs": {"avg_price": {"avg": {"field": "price"}}}}
    res = _search(client, seeded, body)
    assert res["aggregations"]["avg_price"]["value"] == pytest.approx(17.4)
