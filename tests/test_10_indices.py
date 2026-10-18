import pytest

MAPPING = {
    "properties": {
        "title": {"type": "text"},
        "price": {"type": "integer"},
        "category": {"type": "keyword"},
        "created_at": {"type": "date"},
    }
}


@pytest.mark.smoke
def test_create_index_with_mapping(client, index_factory):
    name = index_factory(mappings=MAPPING)

    r = client.get(f"/{name}/_mapping")
    assert r.status_code == 200
    props = r.json()[name]["mappings"]["properties"]
    assert props["title"]["type"] == "text"
    assert props["category"]["type"] == "keyword"


def test_create_index_with_settings(client, index_factory):
    name = index_factory(
        mappings=MAPPING,
        settings={"number_of_shards": 1, "number_of_replicas": 0},
    )
    s = client.get(f"/{name}/_settings").json()[name]["settings"]["index"]
    assert s["number_of_replicas"] == "0"


def test_duplicate_index_returns_400(client, index_factory):
    name = index_factory(mappings=MAPPING)
    r = client.put(f"/{name}", json={})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "resource_already_exists_exception"


def test_delete_index_then_404(client, index_factory):
    name = index_factory(mappings=MAPPING)
    assert client.delete(f"/{name}").status_code == 200
    assert client.get(f"/{name}").status_code == 404


def test_custom_analyzer(client, index_factory):
    settings = {
        "analysis": {
            "analyzer": {
                "lowercase_analyzer": {
                    "type": "custom",
                    "tokenizer": "standard",
                    "filter": ["lowercase"],
                }
            }
        }
    }
    name = index_factory(settings=settings, mappings=MAPPING)
    r = client.post(f"/{name}/_analyze", json={"analyzer": "lowercase_analyzer", "text": "Hello World"})
    assert r.status_code == 200
    assert [t["token"] for t in r.json()["tokens"]] == ["hello", "world"]
