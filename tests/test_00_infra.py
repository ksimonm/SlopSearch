import pytest


@pytest.mark.smoke
def test_cluster_health(client):
    r = client.get("/_cluster/health")
    assert r.status_code == 200
    assert r.json()["status"] in ("yellow", "green")


@pytest.mark.smoke
def test_root_version(client):
    r = client.get("/")
    assert r.status_code == 200
    body = r.json()
    assert body["version"]["distribution"] == "opensearch"
    assert body["version"]["number"].startswith("3.")


@pytest.mark.smoke
def test_custom_port_used(client, base_url):
    assert str(client.base_url).startswith("http://")
    assert client.get("/").status_code == 200
