"""Data survives a server restart.

Opt-in: OS_RESTART_CMD must restart the server under test, e.g.
    OS_RESTART_CMD='docker compose restart go' OS_PORT=9291
    OS_RESTART_CMD='servers/restart.sh --signal KILL 9200 /path/to/opensearch -Ehttp.port=9200 ...'
"""

import json
import os
import subprocess
import time
import uuid

import httpx
import pytest

RESTART_CMD = os.getenv("OS_RESTART_CMD")
RESTART_TIMEOUT = float(os.getenv("OS_RESTART_TIMEOUT", "180"))

pytestmark = [
    pytest.mark.persistence,
    pytest.mark.skipif(not RESTART_CMD, reason="OS_RESTART_CMD is not set"),
]

PFX = f"slopsearch-persist-{uuid.uuid4().hex[:8]}"
MAIN = f"{PFX}-main"
CLOSED = f"{PFX}-closed"
TEMPLATE = f"{PFX}-tpl"
AUTO = f"{PFX}-auto-1"
SCRIPT = f"{PFX}-script"
PIPELINE = f"{PFX}-pipe"
ALIAS = f"{PFX}-alias"
FILTERED_ALIAS = f"{PFX}-fruit"

SETTINGS = {
    "number_of_shards": 1,
    "number_of_replicas": 0,
    "analysis": {
        "analyzer": {
            "folding": {
                "type": "custom",
                "tokenizer": "standard",
                "filter": ["lowercase", "asciifolding"],
            }
        }
    },
}

MAPPING = {
    "properties": {
        "title": {
            "type": "text",
            "analyzer": "folding",
            "fields": {"raw": {"type": "keyword"}},
        },
        "tag": {"type": "keyword"},
        "price": {"type": "integer"},
        "published": {"type": "date"},
        "reviews": {
            "type": "nested",
            "properties": {"user": {"type": "keyword"}, "stars": {"type": "integer"}},
        },
    }
}

TAGS = ["fruit", "veg", "tool", "fruit", "veg"]
DOCS = {
    str(i): {
        "title": f"Café item {i} {'crème brûlée' if i % 3 == 0 else 'plain'}",
        "tag": TAGS[i % len(TAGS)],
        "price": i * 10,
        "published": f"2020-01-{i + 1:02d}",
        "reviews": [{"user": f"u{i % 4}", "stars": i % 5 + 1}],
    }
    for i in range(1, 21)
}

QUERIES = {
    "match_folded": {"query": {"match": {"title": "creme brulee"}}, "sort": [{"price": "asc"}]},
    "term_tag": {"query": {"term": {"tag": "veg"}}, "sort": [{"price": "desc"}]},
    "range_price": {"query": {"range": {"price": {"gte": 50, "lt": 120}}}, "sort": [{"price": "asc"}]},
    "date_range": {
        "query": {"range": {"published": {"gte": "2020-01-05", "lte": "2020-01-10"}}},
        "sort": [{"published": "asc"}],
    },
    "nested": {
        "query": {"nested": {"path": "reviews", "query": {"term": {"reviews.stars": 5}}}},
        "sort": [{"price": "asc"}],
    },
    "keyword_sort": {"query": {"match_all": {}}, "sort": [{"title.raw": "asc"}], "size": 50},
    "aggs": {
        "size": 0,
        "aggs": {
            "tags": {"terms": {"field": "tag"}, "aggs": {"avg_price": {"avg": {"field": "price"}}}},
            "price_stats": {"stats": {"field": "price"}},
            "per_week": {"date_histogram": {"field": "published", "calendar_interval": "week"}},
        },
    },
}

# docs written after the snapshot carry one of these tags and are kept out of snapshot queries
NEW_TAGS = ["late", "post"]


def _pre_restart(body):
    query = {"bool": {"must": [body.get("query", {"match_all": {}})], "must_not": [{"terms": {"tag": NEW_TAGS}}]}}
    return {**body, "query": query}


def _ndjson(lines):
    return ("\n".join(json.dumps(l, ensure_ascii=False) for l in lines) + "\n").encode()


def _hits(resp):
    body = resp.json()
    return {
        "total": body["hits"]["total"],
        "hits": [(h["_id"], h["_source"], h.get("sort")) for h in body["hits"]["hits"]],
        "aggregations": body.get("aggregations"),
    }


def _node_pid(client):
    r = client.get("/_nodes/_local/process")
    assert r.status_code == 200, r.text
    (node,) = r.json()["nodes"].values()
    return node["process"]["id"]


def _snapshot(client):
    snap = {
        "mapping": client.get(f"/{MAIN}/_mapping").json()[MAIN],
        "settings": client.get(f"/{MAIN}/_settings", params={"flat_settings": "true"}).json()[MAIN],
        "aliases": client.get(f"/{MAIN}/_alias").json()[MAIN],
        "count": client.get(f"/{MAIN}/_count").json()["count"],
        "queries": {},
    }
    for name, body in QUERIES.items():
        r = client.post(f"/{MAIN}/_search", json=_pre_restart(body))
        assert r.status_code == 200, r.text
        snap["queries"][name] = _hits(r)
    return snap


def _seed(client):
    r = client.put(
        f"/_index_template/{TEMPLATE}",
        json={
            "index_patterns": [f"{PFX}-auto-*"],
            "template": {
                "settings": {"number_of_shards": 1, "number_of_replicas": 0},
                "mappings": {"properties": {"code": {"type": "keyword"}}},
            },
        },
    )
    assert r.status_code == 200, r.text
    r = client.put(
        f"/_scripts/{SCRIPT}",
        json={"script": {"lang": "painless", "source": "ctx._source.price += params.delta"}},
    )
    assert r.status_code == 200, r.text
    r = client.put(
        f"/_ingest/pipeline/{PIPELINE}",
        json={"processors": [{"set": {"field": "ingested", "value": True}}]},
    )
    assert r.status_code == 200, r.text

    r = client.put(
        f"/{MAIN}",
        json={
            "settings": SETTINGS,
            "mappings": MAPPING,
            "aliases": {ALIAS: {}, FILTERED_ALIAS: {"filter": {"term": {"tag": "fruit"}}}},
        },
    )
    assert r.status_code == 200, r.text
    lines = []
    for doc_id, doc in DOCS.items():
        lines += [{"index": {"_id": doc_id}}, doc]
    r = client.post(
        f"/{MAIN}/_bulk",
        content=_ndjson(lines),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200 and not r.json()["errors"], r.text

    # history: an update via stored script, a delete, a pipeline write
    r = client.post(
        f"/{MAIN}/_update/3",
        json={"script": {"id": SCRIPT, "params": {"delta": 5}}},
        params={"refresh": "true"},
    )
    assert r.status_code == 200, r.text
    assert client.delete(f"/{MAIN}/_doc/4", params={"refresh": "true"}).status_code == 200
    r = client.put(
        f"/{MAIN}/_doc/piped",
        json={"title": "Piped", "tag": "pipe", "price": 1},
        params={"pipeline": PIPELINE, "refresh": "true"},
    )
    assert r.status_code == 201, r.text

    r = client.put(
        f"/{MAIN}/_settings",
        json={"index": {"refresh_interval": "7s", "max_result_window": 123}},
    )
    assert r.status_code == 200, r.text

    r = client.put(f"/{CLOSED}", json={"settings": {"number_of_shards": 1, "number_of_replicas": 0}})
    assert r.status_code == 200, r.text
    r = client.post(
        f"/{CLOSED}/_bulk",
        content=_ndjson([{"index": {"_id": "c1"}}, {"v": 1}, {"index": {"_id": "c2"}}, {"v": 2}]),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200 and not r.json()["errors"], r.text
    assert client.post(f"/{CLOSED}/_close").status_code == 200


def _restart(client):
    r = subprocess.run(RESTART_CMD, shell=True, capture_output=True, text=True, timeout=RESTART_TIMEOUT)
    assert r.returncode == 0, f"restart failed: {r.stdout}{r.stderr}"
    deadline = time.time() + RESTART_TIMEOUT
    last = "no response"
    while time.time() < deadline:
        try:
            r = client.get(
                f"/_cluster/health/{MAIN}",
                params={"wait_for_status": "yellow", "timeout": "5s"},
            )
            if r.status_code == 200 and r.json()["status"] in ("yellow", "green"):
                return
            last = f"{r.status_code}: {r.text}"
        except httpx.HTTPError as exc:
            last = repr(exc)
        time.sleep(1)
    pytest.fail(f"server did not come back after restart: {last}")


@pytest.fixture(scope="module")
def restarted(client):
    try:
        _seed(client)
        before = _snapshot(client)
        before["doc3"] = client.get(f"/{MAIN}/_doc/3").json()
        before["node"] = _node_pid(client)

        # acknowledged but never refreshed or flushed: must survive via the translog
        r = client.put(f"/{MAIN}/_doc/unrefreshed", json={"title": "Late arrival", "tag": "late", "price": 999})
        assert r.status_code == 201, r.text
        before["unrefreshed_seq_no"] = r.json()["_seq_no"]

        _restart(client)
        yield before
    finally:
        for index in (MAIN, CLOSED, AUTO):
            client.delete(f"/{index}", params={"ignore_unavailable": "true"})
        client.delete(f"/_index_template/{TEMPLATE}")
        client.delete(f"/_scripts/{SCRIPT}")
        client.delete(f"/_ingest/pipeline/{PIPELINE}")


def test_node_process_was_really_restarted(client, restarted):
    assert _node_pid(client) != restarted["node"]


def test_index_still_exists(client, restarted):
    assert client.head(f"/{MAIN}").status_code == 200


def test_mapping_unchanged(client, restarted):
    assert client.get(f"/{MAIN}/_mapping").json()[MAIN] == restarted["mapping"]


def test_settings_unchanged_including_dynamic_updates(client, restarted):
    settings = client.get(f"/{MAIN}/_settings", params={"flat_settings": "true"}).json()[MAIN]
    assert settings == restarted["settings"]
    flat = settings["settings"]
    assert flat["index.refresh_interval"] == "7s"
    assert flat["index.max_result_window"] == "123"
    assert flat["index.analysis.analyzer.folding.filter"] == ["lowercase", "asciifolding"]


def test_index_uuid_preserved(client, restarted):
    flat = client.get(f"/{MAIN}/_settings", params={"flat_settings": "true"}).json()[MAIN]["settings"]
    assert flat["index.uuid"] == restarted["settings"]["settings"]["index.uuid"]


def test_custom_analyzer_still_works(client, restarted):
    r = client.post(f"/{MAIN}/_analyze", json={"analyzer": "folding", "text": "Crème BRÛLÉE"})
    assert [t["token"] for t in r.json()["tokens"]] == ["creme", "brulee"]


def test_aliases_unchanged(client, restarted):
    assert client.get(f"/{MAIN}/_alias").json()[MAIN] == restarted["aliases"]


def test_filtered_alias_still_filters(client, restarted):
    r = client.post(f"/{FILTERED_ALIAS}/_count")
    assert r.status_code == 200
    expected = sum(1 for i, d in DOCS.items() if d["tag"] == "fruit" and i != "4")
    assert r.json()["count"] == expected


def test_search_via_alias(client, restarted):
    r = client.post(f"/{ALIAS}/_search", json={"query": {"ids": {"values": ["1", "piped"]}}})
    assert sorted(h["_id"] for h in r.json()["hits"]["hits"]) == ["1", "piped"]


@pytest.mark.parametrize("name", sorted(QUERIES))
def test_queries_return_same_results(client, restarted, name):
    r = client.post(f"/{MAIN}/_search", json=_pre_restart(QUERIES[name]))
    assert r.status_code == 200, r.text
    assert _hits(r) == restarted["queries"][name]


def test_folded_match_finds_expected_docs(client, restarted):
    r = client.post(f"/{MAIN}/_search", json=QUERIES["match_folded"])
    assert [h["_id"] for h in r.json()["hits"]["hits"]] == ["3", "6", "9", "12", "15", "18"]


def test_updated_doc_keeps_new_value_and_version(client, restarted):
    doc = client.get(f"/{MAIN}/_doc/3").json()
    assert doc["found"] is True
    assert doc["_source"]["price"] == 35
    assert doc["_version"] == 2
    for key in ("_version", "_seq_no", "_source"):
        assert doc[key] == restarted["doc3"][key]


def test_deleted_doc_stays_deleted(client, restarted):
    assert client.get(f"/{MAIN}/_doc/4").status_code == 404
    r = client.post(f"/{MAIN}/_search", json={"query": {"ids": {"values": ["4"]}}})
    assert r.json()["hits"]["total"]["value"] == 0


def test_pipeline_written_doc_preserved(client, restarted):
    assert client.get(f"/{MAIN}/_doc/piped").json()["_source"]["ingested"] is True


def test_unrefreshed_acknowledged_write_survives(client, restarted):
    r = client.get(f"/{MAIN}/_doc/unrefreshed")
    assert r.status_code == 200
    body = r.json()
    assert body["_source"]["title"] == "Late arrival"
    assert body["_seq_no"] == restarted["unrefreshed_seq_no"]
    client.post(f"/{MAIN}/_refresh")
    r = client.post(f"/{MAIN}/_search", json={"query": {"term": {"tag": "late"}}})
    assert [h["_id"] for h in r.json()["hits"]["hits"]] == ["unrefreshed"]


def test_count_includes_unrefreshed_doc(client, restarted):
    client.post(f"/{MAIN}/_refresh")
    query = {"bool": {"must_not": [{"term": {"tag": "post"}}]}}
    assert client.post(f"/{MAIN}/_count", json={"query": query}).json()["count"] == restarted["count"] + 1


def test_seq_no_continues_after_restart(client, restarted):
    r = client.put(f"/{MAIN}/_doc/after", json={"title": "after restart", "tag": "post"}, params={"refresh": "true"})
    assert r.status_code == 201
    assert r.json()["_seq_no"] > restarted["unrefreshed_seq_no"]


def test_create_conflict_still_detected(client, restarted):
    r = client.put(f"/{MAIN}/_create/1", json={"title": "dup"})
    assert r.status_code == 409
    assert r.json()["error"]["type"] == "version_conflict_engine_exception"


def test_max_result_window_still_enforced(client, restarted):
    r = client.post(f"/{MAIN}/_search", json={"from": 120, "size": 10})
    assert r.status_code == 400


def test_closed_index_stays_closed_and_keeps_data(client, restarted):
    r = client.post(f"/{CLOSED}/_search")
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "index_closed_exception"
    assert client.post(f"/{CLOSED}/_open", params={"wait_for_active_shards": "1"}).status_code == 200
    assert client.get(f"/{CLOSED}/_count").json()["count"] == 2


def test_index_template_still_applied(client, restarted):
    r = client.put(f"/{AUTO}/_doc/1", json={"code": "X-1"}, params={"refresh": "true"})
    assert r.status_code == 201, r.text
    mapping = client.get(f"/{AUTO}/_mapping").json()[AUTO]["mappings"]
    assert mapping["properties"]["code"] == {"type": "keyword"}


def test_stored_script_still_usable(client, restarted):
    r = client.put(f"/{MAIN}/_doc/scripted", json={"tag": "post", "price": 10})
    assert r.status_code == 201, r.text
    r = client.post(
        f"/{MAIN}/_update/scripted",
        json={"script": {"id": SCRIPT, "params": {"delta": 1}}},
        params={"refresh": "true"},
    )
    assert r.status_code == 200, r.text
    assert client.get(f"/{MAIN}/_doc/scripted").json()["_source"]["price"] == 11


def test_ingest_pipeline_still_usable(client, restarted):
    r = client.put(f"/{MAIN}/_doc/piped2", json={"title": "x", "tag": "post"}, params={"pipeline": PIPELINE})
    assert r.status_code == 201, r.text
    assert client.get(f"/{MAIN}/_doc/piped2").json()["_source"]["ingested"] is True
