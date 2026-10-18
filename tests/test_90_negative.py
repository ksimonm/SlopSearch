import pytest

MAPPING = {"properties": {"title": {"type": "text"}, "price": {"type": "integer"}}}


@pytest.mark.smoke
def test_search_missing_index_returns_404(client):
    r = client.post("/slopsearch-no-such-index/_search", json={"query": {"match_all": {}}})
    assert r.status_code == 404
    assert r.json()["error"]["type"] == "index_not_found_exception"


def test_invalid_query_clause_returns_400(client, index_factory):
    name = index_factory(mappings=MAPPING)
    r = client.post(f"/{name}/_search", json={"query": {"bogus_clause": {}}})
    assert r.status_code == 400


def test_malformed_json_returns_400(client, index_factory):
    name = index_factory(mappings=MAPPING)
    r = client.post(
        f"/{name}/_search",
        content=b"{not valid json",
        headers={"Content-Type": "application/json"},
    )
    assert r.status_code == 400


def test_wrong_field_type_in_range_returns_400(client, index_factory):
    name = index_factory(mappings=MAPPING)
    r = client.post(f"/{name}/_search", json={"query": {"range": {"price": {"gte": "abc"}}}})
    assert r.status_code == 400


def test_invalid_mapping_returns_400(client):
    r = client.put(
        "/slopsearch-bad-mapping",
        json={"mappings": {"properties": {"x": {"type": "nonexistent_type"}}}},
    )
    assert r.status_code == 400


def test_get_missing_document_returns_404(client, index_factory):
    name = index_factory(mappings=MAPPING)
    r = client.get(f"/{name}/_doc/does-not-exist")
    assert r.status_code == 404
    assert r.json()["found"] is False


def test_delete_missing_index_returns_404(client):
    r = client.delete("/slopsearch-no-such-index-delete")
    assert r.status_code == 404
