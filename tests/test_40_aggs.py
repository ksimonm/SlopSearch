import pytest

MAPPING = {
    "properties": {
        "category": {"type": "keyword"},
        "brand": {"type": "keyword"},
        "price": {"type": "integer"},
        "created_at": {"type": "date"},
        "reviews": {
            "type": "nested",
            "properties": {"rating": {"type": "integer"}},
        },
    }
}

DOCS = [
    {"category": "fruit", "brand": "A", "price": 10, "created_at": "2024-01-01", "reviews": [{"rating": 5}, {"rating": 3}]},
    {"category": "fruit", "brand": "A", "price": 20, "created_at": "2024-01-15", "reviews": [{"rating": 4}]},
    {"category": "tool", "brand": "B", "price": 30, "created_at": "2024-02-10", "reviews": [{"rating": 2}]},
    {"category": "tool", "brand": "B", "price": 40, "created_at": "2024-03-05", "reviews": [{"rating": 5}, {"rating": 1}]},
]


@pytest.fixture
def seeded(client, index_factory, bulk_index):
    name = index_factory(mappings=MAPPING)
    bulk_index(name, DOCS)
    return name


def _aggs(client, index, aggs):
    r = client.post(f"/{index}/_search", json={"size": 0, "aggs": aggs})
    assert r.status_code == 200, r.text
    return r.json()["aggregations"]


@pytest.mark.smoke
def test_terms_agg(client, seeded):
    res = _aggs(client, seeded, {"by_cat": {"terms": {"field": "category"}}})
    buckets = {b["key"]: b["doc_count"] for b in res["by_cat"]["buckets"]}
    assert buckets == {"fruit": 2, "tool": 2}


def test_terms_buckets_sum_equals_total(client, seeded):
    res = _aggs(client, seeded, {"by_cat": {"terms": {"field": "category"}}})
    assert sum(b["doc_count"] for b in res["by_cat"]["buckets"]) == len(DOCS)


def test_sub_aggregation(client, seeded):
    res = _aggs(
        client,
        seeded,
        {"by_cat": {"terms": {"field": "category"}, "aggs": {"avg_price": {"avg": {"field": "price"}}}}},
    )
    buckets = {b["key"]: b["avg_price"]["value"] for b in res["by_cat"]["buckets"]}
    assert buckets == {"fruit": 15.0, "tool": 35.0}


def test_date_histogram(client, seeded):
    res = _aggs(
        client,
        seeded,
        {"per_month": {"date_histogram": {"field": "created_at", "calendar_interval": "month"}}},
    )
    counts = {b["key_as_string"][:7]: b["doc_count"] for b in res["per_month"]["buckets"]}
    assert counts == {"2024-01": 2, "2024-02": 1, "2024-03": 1}


def test_range_agg(client, seeded):
    res = _aggs(
        client,
        seeded,
        {"price_ranges": {"range": {"field": "price", "ranges": [{"to": 25}, {"from": 25}]}}},
    )
    assert [b["doc_count"] for b in res["price_ranges"]["buckets"]] == [2, 2]


def test_metric_aggs(client, seeded):
    res = _aggs(
        client,
        seeded,
        {
            "avg": {"avg": {"field": "price"}},
            "min": {"min": {"field": "price"}},
            "max": {"max": {"field": "price"}},
            "sum": {"sum": {"field": "price"}},
        },
    )
    assert res["avg"]["value"] == pytest.approx(25.0)
    assert res["min"]["value"] == 10
    assert res["max"]["value"] == 40
    assert res["sum"]["value"] == 100


def test_cardinality(client, seeded):
    res = _aggs(client, seeded, {"brands": {"cardinality": {"field": "brand"}}})
    assert res["brands"]["value"] == 2


def test_stats(client, seeded):
    res = _aggs(client, seeded, {"stats": {"stats": {"field": "price"}}})
    assert res["stats"] == {"count": 4, "min": 10.0, "max": 40.0, "avg": 25.0, "sum": 100.0}


def test_nested_aggregation(client, seeded):
    res = _aggs(
        client,
        seeded,
        {"reviews": {"nested": {"path": "reviews"}, "aggs": {"avg_rating": {"avg": {"field": "reviews.rating"}}}}},
    )
    assert res["reviews"]["avg_rating"]["value"] == pytest.approx(20 / 6)
