import pytest

pytestmark = pytest.mark.mapping


def _search(client, index, body):
    r = client.post(f"/{index}/_search", json=body)
    assert r.status_code == 200, r.text
    return r.json()


def _total(resp):
    return resp["hits"]["total"]["value"]


def test_explicit_mapping_roundtrip(client, index_factory):
    mapping = {
        "properties": {
            "title": {
                "type": "text",
                "fields": {"keyword": {"type": "keyword", "ignore_above": 256}},
            },
            "tag": {"type": "keyword"},
            "count": {"type": "integer"},
            "rating": {"type": "float"},
            "active": {"type": "boolean"},
            "published": {"type": "date"},
            "author": {"properties": {"name": {"type": "text"}, "age": {"type": "short"}}},
            "comments": {"type": "nested", "properties": {"stars": {"type": "byte"}}},
        }
    }
    name = index_factory(mappings=mapping)
    props = client.get(f"/{name}/_mapping").json()[name]["mappings"]["properties"]
    assert props["title"]["type"] == "text"
    assert props["title"]["fields"]["keyword"]["type"] == "keyword"
    assert props["title"]["fields"]["keyword"]["ignore_above"] == 256
    assert props["tag"]["type"] == "keyword"
    assert props["count"]["type"] == "integer"
    assert props["rating"]["type"] == "float"
    assert props["active"]["type"] == "boolean"
    assert props["published"]["type"] == "date"
    assert props["author"]["properties"]["name"]["type"] == "text"
    assert props["author"]["properties"]["age"]["type"] == "short"
    assert props["comments"]["type"] == "nested"
    assert props["comments"]["properties"]["stars"]["type"] == "byte"


def test_dynamic_mapping_infers_types(client, index_factory):
    name = index_factory()
    r = client.put(
        f"/{name}/_doc/1",
        json={
            "s": "hello",
            "i": 3,
            "f": 1.5,
            "b": True,
            "dt": "2024-01-01",
            "o": {"k": "v"},
        },
        params={"refresh": "true"},
    )
    assert r.status_code in (200, 201), r.text
    props = client.get(f"/{name}/_mapping").json()[name]["mappings"]["properties"]
    assert props["s"]["type"] == "text"
    assert props["s"]["fields"]["keyword"]["type"] == "keyword"
    assert props["i"]["type"] == "long"
    assert props["f"]["type"] == "float"
    assert props["b"]["type"] == "boolean"
    assert props["dt"]["type"] == "date"
    assert props["o"]["properties"]["k"]["type"] == "text"


MULTI_MAPPING = {
    "properties": {
        "title": {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
        "n": {"type": "integer"},
    }
}


def test_multifield_keyword_aggregation(client, index_factory, bulk_index):
    name = index_factory(mappings=MULTI_MAPPING)
    bulk_index(name, [{"title": "Hello World"}, {"title": "Hello World"}, {"title": "Bye"}])
    res = _search(client, name, {"size": 0, "aggs": {"t": {"terms": {"field": "title.keyword"}}}})
    buckets = {b["key"]: b["doc_count"] for b in res["aggregations"]["t"]["buckets"]}
    assert buckets == {"Hello World": 2, "Bye": 1}


def test_multifield_keyword_term_query(client, index_factory, bulk_index):
    name = index_factory(mappings=MULTI_MAPPING)
    bulk_index(name, [{"title": "Hello World"}, {"title": "Hello"}])
    assert _total(_search(client, name, {"query": {"term": {"title.keyword": "Hello World"}}})) == 1
    assert _total(_search(client, name, {"query": {"term": {"title.keyword": "hello"}}})) == 0


def test_multifield_keyword_sort(client, index_factory):
    name = index_factory(mappings=MULTI_MAPPING)
    for i, title in enumerate(["b", "a", "c"]):
        client.put(f"/{name}/_doc/{i}", json={"title": title}, params={"refresh": "true"})
    res = _search(client, name, {"_source": ["title"], "sort": [{"title.keyword": "asc"}]})
    assert [h["_source"]["title"] for h in res["hits"]["hits"]] == ["a", "b", "c"]


def test_date_field_range(client, index_factory, bulk_index):
    name = index_factory(mappings={"properties": {"d": {"type": "date"}}})
    bulk_index(name, [{"d": "2024-01-01"}, {"d": "2024-06-15"}, {"d": "2025-01-01"}])
    res = _search(client, name, {"query": {"range": {"d": {"gte": "2024-01-01", "lt": "2025-01-01"}}}})
    assert _total(res) == 2


def test_date_field_sort(client, index_factory):
    name = index_factory(mappings={"properties": {"d": {"type": "date"}}})
    for i, d in enumerate(["2024-03-01", "2024-01-01", "2024-02-01"]):
        client.put(f"/{name}/_doc/{i}", json={"d": d}, params={"refresh": "true"})
    res = _search(client, name, {"_source": ["d"], "sort": [{"d": "asc"}]})
    assert [h["_source"]["d"] for h in res["hits"]["hits"]] == [
        "2024-01-01",
        "2024-02-01",
        "2024-03-01",
    ]


def test_boolean_field_validation(client, index_factory):
    name = index_factory(mappings={"properties": {"b": {"type": "boolean"}}})
    bad = client.put(f"/{name}/_doc/1", json={"b": "yes"}, params={"refresh": "true"})
    assert bad.status_code == 400
    ok = client.put(f"/{name}/_doc/2", json={"b": True}, params={"refresh": "true"})
    assert ok.status_code == 201


def test_text_vs_keyword_behaviour(client, index_factory, bulk_index):
    name = index_factory(mappings={"properties": {"t": {"type": "text"}, "k": {"type": "keyword"}}})
    bulk_index(name, [{"t": "Quick Brown Fox", "k": "Quick Brown Fox"}])
    assert _total(_search(client, name, {"query": {"match": {"t": "quick"}}})) == 1
    assert _total(_search(client, name, {"query": {"match": {"t": "QUICK"}}})) == 1
    assert _total(_search(client, name, {"query": {"match": {"k": "quick"}}})) == 0
    assert _total(_search(client, name, {"query": {"match": {"k": "Quick Brown Fox"}}})) == 1
