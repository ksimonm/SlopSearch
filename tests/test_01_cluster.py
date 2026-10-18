import uuid

import pytest

pytestmark = pytest.mark.cluster

# Base keys every /_nodes (info) entry has, regardless of the requested metric.
NODE_INFO_BASE = {
    "name", "transport_address", "host", "ip", "version",
    "build_type", "build_hash", "roles", "attributes",
}
NODE_STATS_BASE = {"timestamp", "name", "transport_address", "host", "ip", "roles", "attributes"}

HEALTH_KEYS = {
    "cluster_name", "status", "timed_out", "number_of_nodes", "number_of_data_nodes",
    "discovered_master", "discovered_cluster_manager", "active_primary_shards",
    "active_shards", "relocating_shards", "initializing_shards", "unassigned_shards",
    "delayed_unassigned_shards", "number_of_pending_tasks", "number_of_in_flight_fetch",
    "task_max_waiting_in_queue_millis", "active_shards_percent_as_number",
}
HEALTH_INDEX_KEYS = {
    "status", "number_of_shards", "number_of_replicas", "active_primary_shards",
    "active_shards", "relocating_shards", "initializing_shards", "unassigned_shards",
}
HEALTH_SHARD_KEYS = {
    "status", "primary_active", "active_shards", "relocating_shards",
    "initializing_shards", "unassigned_shards",
}

CAT_INDICES_DEFAULT = [
    "health", "status", "index", "uuid", "pri", "rep",
    "docs.count", "docs.deleted", "store.size", "pri.store.size",
]
CAT_HEALTH_DEFAULT = [
    "epoch", "timestamp", "cluster", "status", "node.total", "node.data",
    "discovered_cluster_manager", "shards", "pri", "relo", "init", "unassign",
    "pending_tasks", "max_task_wait_time", "active_shards_percent",
]
CAT_SHARDS_DEFAULT = ["index", "shard", "prirep", "state", "docs", "store", "ip", "node"]
CAT_ALIASES_DEFAULT = ["alias", "index", "filter", "routing.index", "routing.search", "is_write_index"]
CAT_NODES_DEFAULT = [
    "ip", "heap.percent", "ram.percent", "cpu", "load_1m", "load_5m", "load_15m",
    "node.role", "node.roles", "cluster_manager", "name",
]
CAT_SEGMENTS_DEFAULT = [
    "index", "shard", "prirep", "ip", "segment", "generation", "docs.count",
    "docs.deleted", "size", "size.memory", "committed", "searchable", "version", "compound",
]
CAT_ALLOCATION_DEFAULT = [
    "shards", "disk.indices", "disk.used", "disk.avail", "disk.total",
    "disk.percent", "host", "ip", "node",
]

DOCS = [
    {"tag": "a", "n": 1, "body": "hello world"},
    {"tag": "b", "n": 2, "body": "hello there"},
    {"tag": "a", "n": 3, "body": "general kenobi"},
]


def _err_type(resp):
    return resp.json()["error"]["type"]


def _err_reason(resp):
    return resp.json()["error"]["reason"]


def _header(text):
    return text.splitlines()[0].split()


# plugins bundled with the full OpenSearch 3.9.0 distribution
FULL_DISTRIBUTION_PLUGINS = [
    "opensearch-alerting", "opensearch-anomaly-detection", "opensearch-asynchronous-search",
    "opensearch-cross-cluster-replication", "opensearch-custom-codecs", "opensearch-flow-framework",
    "opensearch-geospatial", "opensearch-index-management", "opensearch-job-scheduler",
    "opensearch-knn", "opensearch-ltr", "opensearch-ml", "opensearch-neural-search",
    "opensearch-notifications", "opensearch-notifications-core", "opensearch-observability",
    "opensearch-performance-analyzer", "opensearch-reports-scheduler", "opensearch-search-relevance",
    "opensearch-security", "opensearch-security-analytics", "opensearch-skills", "opensearch-sql",
    "opensearch-system-templates", "opensearch-ubi", "query-insights",
]


def _rows(text):
    return [line.split() for line in text.splitlines() if line.strip()]


def _only_node(resp):
    nodes = resp.json()["nodes"]
    assert len(nodes) == 1
    return next(iter(nodes.values()))


def _allow(resp):
    return set(resp.headers["allow"].split(","))


@pytest.fixture(scope="module")
def env(client):
    """Green (2p/0r, 3 docs), yellow (1p/1r, empty), closed index + aliases."""
    p = f"c01-{uuid.uuid4().hex[:8]}"
    green, yellow, closed = f"{p}-gr", f"{p}-ye", f"{p}-cl"
    created = []
    try:
        r = client.put(f"/{green}", json={
            "settings": {"number_of_shards": 2, "number_of_replicas": 0},
            "mappings": {"properties": {
                "tag": {"type": "keyword"}, "n": {"type": "integer"},
                "body": {"type": "text"}, "loc": {"type": "geo_point"},
            }},
        })
        assert r.status_code == 200, r.text
        created.append(green)
        r = client.put(f"/{yellow}", json={"settings": {"number_of_shards": 1, "number_of_replicas": 1}})
        assert r.status_code == 200, r.text
        created.append(yellow)
        r = client.put(f"/{closed}", json={"settings": {"number_of_shards": 1, "number_of_replicas": 0}})
        assert r.status_code == 200, r.text
        created.append(closed)
        for i, doc in enumerate(DOCS, 1):
            r = client.put(f"/{green}/_doc/{i}", json=doc, params={"refresh": "true"})
            assert r.status_code == 201, r.text
        r = client.post(f"/{closed}/_close")
        assert r.status_code == 200, r.text
        r = client.post("/_aliases", json={"actions": [
            {"add": {"index": green, "alias": f"{p}-al"}},
            {"add": {"index": green, "alias": f"{p}-fal", "filter": {"term": {"tag": "a"}}}},
            {"add": {"index": yellow, "alias": f"{p}-ral", "routing": "r1", "is_write_index": True}},
        ]})
        assert r.status_code == 200, r.text
        for idx in (green, closed):
            r = client.get(f"/_cluster/health/{idx}", params={"wait_for_status": "green", "timeout": "30s"})
            assert r.status_code == 200, r.text
        yield {
            "prefix": p, "green": green, "yellow": yellow, "closed": closed,
            "alias": f"{p}-al", "falias": f"{p}-fal", "ralias": f"{p}-ral",
        }
    finally:
        for idx in created:
            client.delete(f"/{idx}", params={"ignore_unavailable": "true"})


@pytest.fixture(scope="module")
def node(client):
    st = client.get("/_cluster/state/nodes,cluster_manager_node").json()
    node_id = st["cluster_manager_node"]
    return {"id": node_id, "name": st["nodes"][node_id]["name"]}


@pytest.fixture
def cluster_settings(client):
    """Tracks cluster settings set by a test and resets them to null afterwards."""
    keys = {"transient": set(), "persistent": set()}
    yield keys
    for scope, names in keys.items():
        if names:
            client.put("/_cluster/settings", json={scope: {k: None for k in names}})


# --------------------------------------------------------------------------- root


def test_root_info_top_level_keys(client):
    r = client.get("/")
    assert r.status_code == 200
    assert set(r.json()) == {"name", "cluster_name", "cluster_uuid", "version", "tagline"}


def test_root_version_block(client):
    v = client.get("/").json()["version"]
    assert set(v) == {
        "distribution", "number", "build_type", "build_hash", "build_date", "build_snapshot",
        "lucene_version", "minimum_wire_compatibility_version",
        "minimum_index_compatibility_version",
    }
    assert v["distribution"] == "opensearch"
    assert v["number"] == "3.9.0"
    assert v["build_snapshot"] is False
    assert v["lucene_version"] == "10.5.1"
    assert v["minimum_wire_compatibility_version"] == "2.19.0"
    assert v["minimum_index_compatibility_version"] == "2.0.0"


def test_root_tagline(client):
    assert client.get("/").json()["tagline"] == "The OpenSearch Project: https://opensearch.org/"


def test_root_names_consistent_with_other_apis(client, node):
    root = client.get("/").json()
    assert root["name"] == node["name"]
    assert root["cluster_name"] == client.get("/_cluster/health").json()["cluster_name"]
    assert root["cluster_uuid"] == client.get("/_cluster/state/version").json()["cluster_uuid"]


def test_root_is_pretty_by_default(client):
    r = client.get("/")
    assert r.text.startswith('{\n  "name" : ')
    assert r.text.endswith("}\n")


def test_root_pretty_false_is_compact(client):
    r = client.get("/", params={"pretty": "false"})
    assert r.text.startswith('{"name":')
    assert "\n" not in r.text


def test_root_version_header(client):
    r = client.get("/")
    assert r.headers["x-opensearch-version"] == "OpenSearch/3.9.0 (opensearch)"
    assert r.headers["content-type"] == "application/json; charset=UTF-8"


def test_head_root(client):
    r = client.head("/")
    assert r.status_code == 200
    assert r.content == b""
    assert r.headers["content-type"].startswith("application/json")


def test_head_missing_index_is_404_without_body(client):
    r = client.head(f"/c01-missing-{uuid.uuid4().hex[:8]}")
    assert r.status_code == 404
    assert r.content == b""


def test_root_filter_path(client):
    r = client.get("/", params={"filter_path": "version.number,tagline", "pretty": "false"})
    assert r.json() == {
        "version": {"number": "3.9.0"},
        "tagline": "The OpenSearch Project: https://opensearch.org/",
    }


@pytest.mark.parametrize("method", ["POST", "PUT", "PATCH"])
def test_root_wrong_method_405(client, method):
    r = client.request(method, "/")
    assert r.status_code == 405
    assert _allow(r) == {"DELETE", "HEAD", "GET"}
    body = r.json()
    assert body["status"] == 405
    assert body["error"].startswith(f"Incorrect HTTP method for uri [/] and method [{method}], allowed: [")


def test_delete_root_is_validation_error(client):
    r = client.delete("/")
    assert r.status_code == 400
    assert _err_type(r) == "action_request_validation_exception"
    assert _err_reason(r) == "Validation Failed: 1: index / indices is missing;"


def test_options_root(client):
    r = client.options("/")
    assert r.status_code == 200
    assert _allow(r) == {"DELETE", "HEAD", "GET"}
    assert r.content == b""


# --------------------------------------------------------------- cluster health


def test_health_shape(client):
    r = client.get("/_cluster/health")
    assert r.status_code == 200
    body = r.json()
    assert set(body) == HEALTH_KEYS
    assert body["status"] in ("green", "yellow", "red")
    assert body["timed_out"] is False
    assert body["number_of_nodes"] == 1
    assert body["number_of_data_nodes"] == 1
    assert body["discovered_master"] is True
    assert body["discovered_cluster_manager"] is True
    assert isinstance(body["active_shards_percent_as_number"], float)


@pytest.mark.parametrize("level", [None, "cluster", "bogus"])
def test_health_cluster_level_has_no_indices(client, env, level):
    params = {"level": level} if level else {}
    r = client.get(f"/_cluster/health/{env['green']}", params=params)
    assert r.status_code == 200
    assert "indices" not in r.json()


def test_health_green_index(client, env):
    body = client.get(f"/_cluster/health/{env['green']}").json()
    assert body["status"] == "green"
    assert body["active_primary_shards"] == 2
    assert body["active_shards"] == 2
    assert body["unassigned_shards"] == 0


def test_health_yellow_index(client, env):
    body = client.get(f"/_cluster/health/{env['yellow']}").json()
    assert body["status"] == "yellow"
    assert body["active_primary_shards"] == 1
    assert body["active_shards"] == 1
    assert body["unassigned_shards"] == 1


def test_health_multi_index_is_worst_status(client, env):
    body = client.get(f"/_cluster/health/{env['green']},{env['yellow']}").json()
    assert body["status"] == "yellow"
    assert body["active_primary_shards"] == 3
    assert body["unassigned_shards"] == 1


def test_health_level_indices(client, env):
    r = client.get(f"/_cluster/health/{env['green']},{env['yellow']}", params={"level": "indices"})
    indices = r.json()["indices"]
    assert set(indices) == {env["green"], env["yellow"]}
    for v in indices.values():
        assert set(v) == HEALTH_INDEX_KEYS
    assert indices[env["green"]] == {
        "status": "green", "number_of_shards": 2, "number_of_replicas": 0,
        "active_primary_shards": 2, "active_shards": 2, "relocating_shards": 0,
        "initializing_shards": 0, "unassigned_shards": 0,
    }
    assert indices[env["yellow"]]["status"] == "yellow"
    assert indices[env["yellow"]]["number_of_replicas"] == 1
    assert indices[env["yellow"]]["unassigned_shards"] == 1


def test_health_level_shards(client, env):
    r = client.get(f"/_cluster/health/{env['green']}", params={"level": "shards"})
    shards = r.json()["indices"][env["green"]]["shards"]
    assert set(shards) == {"0", "1"}
    for s in shards.values():
        assert set(s) == HEALTH_SHARD_KEYS
        assert s["status"] == "green"
        assert s["primary_active"] is True
        assert s["active_shards"] == 1


def test_health_level_shards_unassigned_replica(client, env):
    r = client.get(f"/_cluster/health/{env['yellow']}", params={"level": "shards"})
    shard = r.json()["indices"][env["yellow"]]["shards"]["0"]
    assert shard == {
        "status": "yellow", "primary_active": True, "active_shards": 1,
        "relocating_shards": 0, "initializing_shards": 0, "unassigned_shards": 1,
    }


def test_health_via_alias_resolves_index(client, env):
    r = client.get(f"/_cluster/health/{env['alias']}", params={"level": "indices"})
    assert r.status_code == 200
    assert list(r.json()["indices"]) == [env["green"]]


def test_health_wildcard(client, env):
    r = client.get(f"/_cluster/health/{env['prefix']}-gr*", params={"level": "indices"})
    assert set(r.json()["indices"]) == {env["green"]}


def test_health_wildcard_no_match_is_green(client):
    r = client.get(f"/_cluster/health/c01-nomatch-{uuid.uuid4().hex[:8]}*")
    assert r.status_code == 200
    body = r.json()
    assert body["status"] == "green"
    assert body["active_shards"] == 0
    assert body["unassigned_shards"] == 0
    assert body["active_shards_percent_as_number"] == 100.0


def test_health_missing_index_times_out_408_red(client):
    r = client.get(f"/_cluster/health/c01-missing-{uuid.uuid4().hex[:8]}", params={"timeout": "1s"})
    assert r.status_code == 408
    body = r.json()
    assert body["status"] == "red"
    assert body["timed_out"] is True
    assert body["active_shards"] == 0


def test_health_closed_index(client, env):
    r = client.get(f"/_cluster/health/{env['closed']}", params={"level": "indices"})
    assert r.status_code == 200
    body = r.json()
    assert body["status"] == "green"
    assert body["indices"][env["closed"]]["number_of_shards"] == 1


@pytest.mark.parametrize("status", ["green", "yellow", "GREEN", "Yellow"])
def test_health_wait_for_status_satisfied(client, env, status):
    r = client.get(f"/_cluster/health/{env['green']}", params={"wait_for_status": status, "timeout": "5s"})
    assert r.status_code == 200
    assert r.json()["timed_out"] is False


def test_health_wait_for_green_on_yellow_index_times_out(client, env):
    r = client.get(f"/_cluster/health/{env['yellow']}", params={"wait_for_status": "green", "timeout": "100ms"})
    assert r.status_code == 408
    body = r.json()
    assert body["timed_out"] is True
    assert body["status"] == "yellow"


def test_health_wait_for_yellow_on_yellow_index(client, env):
    r = client.get(f"/_cluster/health/{env['yellow']}", params={"wait_for_status": "yellow", "timeout": "5s"})
    assert r.status_code == 200
    assert r.json()["status"] == "yellow"


@pytest.mark.parametrize("params", [
    {"wait_for_active_shards": "all"},
    {"wait_for_active_shards": "1"},
    {"wait_for_active_shards": "0"},
    {"wait_for_nodes": "1"},
    {"wait_for_nodes": ">=1"},
    {"wait_for_nodes": "le(1)"},
    {"wait_for_no_relocating_shards": "true"},
    {"wait_for_no_initializing_shards": "true"},
    {"wait_for_events": "languid"},
    {"wait_for_events": "immediate"},
    {"local": "true"},
    {"cluster_manager_timeout": "10s"},
])
def test_health_wait_params_satisfied(client, env, params):
    r = client.get(f"/_cluster/health/{env['green']}", params={**params, "timeout": "5s"})
    assert r.status_code == 200
    assert r.json()["timed_out"] is False
    assert r.json()["status"] == "green"


def test_health_wait_for_two_nodes_times_out(client):
    r = client.get("/_cluster/health", params={"wait_for_nodes": ">=2", "timeout": "100ms"})
    assert r.status_code == 408
    assert r.json()["timed_out"] is True
    assert r.json()["number_of_nodes"] == 1


def test_health_wait_for_all_active_on_yellow_times_out(client, env):
    r = client.get(f"/_cluster/health/{env['yellow']}",
                   params={"wait_for_active_shards": "all", "timeout": "100ms"})
    assert r.status_code == 408


@pytest.mark.parametrize("params,reason", [
    ({"wait_for_status": "purple"}, "No enum constant org.opensearch.cluster.health.ClusterHealthStatus.PURPLE"),
    ({"wait_for_active_shards": "bogus"}, "cannot parse ActiveShardCount[bogus]"),
    ({"wait_for_events": "bogus"}, "No enum constant org.opensearch.common.Priority.BOGUS"),
    ({"timeout": "bogus"}, "failed to parse [bogus]"),
    ({"bogus_param": "1"}, "request [/_cluster/health] contains unrecognized parameter: [bogus_param]"),
])
def test_health_bad_params_400(client, params, reason):
    r = client.get("/_cluster/health", params=params)
    assert r.status_code == 400
    assert _err_type(r) == "illegal_argument_exception"
    assert _err_reason(r) == reason
    assert r.json()["status"] == 400


@pytest.mark.parametrize("method", ["PUT", "POST", "DELETE"])
def test_health_wrong_method_405(client, method):
    r = client.request(method, "/_cluster/health")
    assert r.status_code == 405
    assert r.headers["allow"] == "GET"
    assert r.json() == {
        "error": f"Incorrect HTTP method for uri [/_cluster/health] and method [{method}], allowed: [GET]",
        "status": 405,
    }


def test_health_human_adds_time_string(client):
    r = client.get("/_cluster/health", params={"human": "true", "filter_path": "task_max*"})
    body = r.json()
    assert set(body) == {"task_max_waiting_in_queue", "task_max_waiting_in_queue_millis"}
    assert isinstance(body["task_max_waiting_in_queue"], str)
    plain = client.get("/_cluster/health", params={"filter_path": "task_max*"}).json()
    assert set(plain) == {"task_max_waiting_in_queue_millis"}


# ---------------------------------------------------------------- cluster state


def test_state_version_metric_shape(client):
    r = client.get("/_cluster/state/version")
    assert r.status_code == 200
    body = r.json()
    assert set(body) == {"cluster_name", "cluster_uuid", "version", "state_uuid"}
    assert isinstance(body["version"], int)


@pytest.mark.parametrize("metric,expected", [
    ("version", {"version", "state_uuid"}),
    ("cluster_manager_node", {"cluster_manager_node"}),
    ("nodes", {"nodes"}),
    ("metadata", {"metadata"}),
    ("routing_table", {"routing_table"}),
    ("routing_nodes", {"routing_nodes"}),
    ("blocks", {"blocks"}),
    ("version,blocks", {"version", "state_uuid", "blocks"}),
    ("bogusmetric", set()),
])
def test_state_metric_filtering(client, env, metric, expected):
    r = client.get(f"/_cluster/state/{metric}")
    assert r.status_code == 200
    assert set(r.json()) == {"cluster_name", "cluster_uuid"} | expected


def test_state_full_has_all_sections(client, env):
    r = client.get(f"/_cluster/state/_all/{env['green']}")
    assert set(r.json()) >= {
        "cluster_name", "cluster_uuid", "version", "state_uuid", "master_node",
        "cluster_manager_node", "blocks", "nodes", "metadata", "routing_table", "routing_nodes",
    }


def test_state_master_node_metric_deprecated(client, node):
    r = client.get("/_cluster/state/master_node")
    assert r.status_code == 200
    assert r.json()["master_node"] == node["id"]
    # the deprecation warning header is de-duplicated by the node, so it may be absent
    if "warning" in r.headers:
        assert "[cluster_manager_node]" in r.headers["warning"]


def test_state_cluster_manager_node_no_warning(client, node):
    r = client.get("/_cluster/state/cluster_manager_node")
    assert r.json()["cluster_manager_node"] == node["id"]
    assert "warning" not in r.headers


def test_state_nodes_section(client, node):
    nodes = client.get("/_cluster/state/nodes").json()["nodes"]
    assert list(nodes) == [node["id"]]
    assert {"name", "ephemeral_id", "transport_address", "attributes"} <= set(nodes[node["id"]])


def test_state_metadata_for_one_index(client, env):
    r = client.get(f"/_cluster/state/metadata/{env['green']}")
    indices = r.json()["metadata"]["indices"]
    assert list(indices) == [env["green"]]
    meta = indices[env["green"]]
    assert meta["state"] == "open"
    assert meta["settings"]["index"]["number_of_shards"] == "2"
    assert meta["settings"]["index"]["number_of_replicas"] == "0"
    assert meta["mappings"]["_doc"]["properties"]["tag"] == {"type": "keyword"}
    assert sorted(meta["aliases"]) == sorted([env["alias"], env["falias"]])


def test_state_metadata_closed_index_state(client, env):
    r = client.get(f"/_cluster/state/metadata/{env['closed']}",
                   params={"filter_path": "metadata.indices.*.state"})
    assert r.json() == {"metadata": {"indices": {env["closed"]: {"state": "close"}}}}


def test_state_metadata_wildcard_no_match_is_empty(client):
    r = client.get(f"/_cluster/state/metadata/c01-nomatch-{uuid.uuid4().hex[:6]}*")
    assert r.status_code == 200
    assert r.json()["metadata"]["indices"] == {}


def test_state_missing_index_is_empty_not_404(client):
    r = client.get(f"/_cluster/state/metadata,routing_table/c01-missing-{uuid.uuid4().hex[:6]}")
    assert r.status_code == 200
    assert r.json()["metadata"]["indices"] == {}
    assert r.json()["routing_table"]["indices"] == {}


def test_state_routing_table_unassigned_replica(client, env, node):
    r = client.get(f"/_cluster/state/routing_table/{env['yellow']}")
    copies = r.json()["routing_table"]["indices"][env["yellow"]]["shards"]["0"]
    assert len(copies) == 2
    primary = next(c for c in copies if c["primary"])
    replica = next(c for c in copies if not c["primary"])
    assert primary["state"] == "STARTED"
    assert primary["node"] == node["id"]
    assert primary["index"] == env["yellow"]
    assert primary["shard"] == 0
    assert replica["state"] == "UNASSIGNED"
    assert replica["node"] is None
    assert replica["unassigned_info"]["reason"] == "INDEX_CREATED"
    assert replica["recovery_source"] == {"type": "PEER"}


def test_state_routing_table_green(client, env):
    shards = client.get(f"/_cluster/state/routing_table/{env['green']}").json()["routing_table"][
        "indices"][env["green"]]["shards"]
    assert set(shards) == {"0", "1"}
    assert all(c["state"] == "STARTED" and c["primary"] for copies in shards.values() for c in copies)


def test_state_version_increases_on_change(client, index_factory):
    before = client.get("/_cluster/state/version").json()["version"]
    index_factory(settings={"number_of_shards": 1, "number_of_replicas": 0})
    after = client.get("/_cluster/state/version").json()["version"]
    assert after > before


def test_state_filter_path(client, env):
    r = client.get(f"/_cluster/state/metadata/{env['green']}",
                   params={"filter_path": "metadata.indices.*.settings.index.number_of_shards"})
    assert r.json() == {"metadata": {"indices": {env["green"]: {
        "settings": {"index": {"number_of_shards": "2"}}}}}}


def test_state_extra_path_segment_no_handler(client, env):
    r = client.get(f"/_cluster/state/_all/{env['green']}/extra")
    assert r.status_code == 400
    assert r.json() == {
        "error": f"no handler found for uri [/_cluster/state/_all/{env['green']}/extra] and method [GET]"
    }


# ---------------------------------------------------------------- cluster stats


def test_cluster_stats_shape(client, env):
    r = client.get("/_cluster/stats")
    assert r.status_code == 200
    body = r.json()
    assert set(body) == {"_nodes", "cluster_name", "cluster_uuid", "timestamp", "status", "indices", "nodes"}
    assert body["_nodes"] == {"total": 1, "successful": 1, "failed": 0}
    assert body["indices"]["count"] >= 3 - 1  # closed indices are not counted
    assert {"count", "shards", "docs", "store", "fielddata", "query_cache", "completion",
            "segments", "mappings", "analysis"} <= set(body["indices"])


def test_cluster_stats_nodes_section(client):
    nodes = client.get("/_cluster/stats").json()["nodes"]
    assert nodes["count"]["total"] == 1
    assert nodes["count"]["data"] == 1
    assert nodes["count"]["cluster_manager"] == 1
    assert nodes["count"]["ingest"] == 1
    assert nodes["versions"] == ["3.9.0"]
    assert {"count", "versions", "os", "process", "jvm", "fs", "plugins"} <= set(nodes)


def test_cluster_stats_field_types_include_mapping(client, env):
    types = client.get("/_cluster/stats").json()["indices"]["mappings"]["field_types"]
    by_name = {t["name"]: t for t in types}
    assert set(by_name["geo_point"]) == {"name", "count", "index_count"}
    assert by_name["geo_point"]["count"] >= 1
    names = [t["name"] for t in types]
    assert names == sorted(names)


def test_cluster_stats_node_filter(client):
    r = client.get("/_cluster/stats/nodes/_local", params={"filter_path": "_nodes,nodes.count.total"})
    assert r.json() == {"_nodes": {"total": 1, "successful": 1, "failed": 0}, "nodes": {"count": {"total": 1}}}


# ------------------------------------------------------------- cluster settings


def test_settings_get_shape(client):
    r = client.get("/_cluster/settings")
    assert r.status_code == 200
    assert set(r.json()) == {"persistent", "transient"}


def test_settings_include_defaults_nested(client):
    r = client.get("/_cluster/settings", params={"include_defaults": "true"})
    body = r.json()
    assert set(body) == {"persistent", "transient", "defaults"}
    assert body["defaults"]["cluster"]["name"] == "opensearch"


@pytest.mark.parametrize("key,value", [
    ("cluster.name", "opensearch"),
    ("discovery.type", "single-node"),
    ("search.max_open_scroll_context", "500"),
    ("indices.query.bool.max_clause_count", "1024"),
    ("search.max_keep_alive", "24h"),
    ("node.roles", ["ingest", "remote_cluster_client", "data", "cluster_manager"]),
])
def test_settings_include_defaults_flat(client, key, value):
    r = client.get("/_cluster/settings", params={"include_defaults": "true", "flat_settings": "true"})
    assert r.json()["defaults"][key] == value


def test_settings_defaults_node_name_matches_root(client):
    r = client.get("/_cluster/settings", params={"include_defaults": "true", "flat_settings": "true"})
    assert r.json()["defaults"]["node.name"] == client.get("/").json()["name"]


def test_settings_defaults_filter_path(client):
    r = client.get("/_cluster/settings",
                   params={"include_defaults": "true", "filter_path": "defaults.cluster.name"})
    assert r.json() == {"defaults": {"cluster": {"name": "opensearch"}}}


def test_settings_put_transient_roundtrip(client, cluster_settings):
    cluster_settings["transient"].add("cluster.max_shards_per_node")
    r = client.put("/_cluster/settings", json={"transient": {"cluster.max_shards_per_node": 4321}})
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "persistent": {},
                        "transient": {"cluster": {"max_shards_per_node": "4321"}}}
    got = client.get("/_cluster/settings").json()
    assert got["transient"]["cluster"]["max_shards_per_node"] == "4321"
    flat = client.get("/_cluster/settings", params={"flat_settings": "true"}).json()
    assert flat["transient"]["cluster.max_shards_per_node"] == "4321"
    assert "cluster.max_shards_per_node" not in flat["persistent"]


def test_settings_put_persistent_roundtrip(client, cluster_settings):
    cluster_settings["persistent"].add("indices.recovery.max_bytes_per_sec")
    r = client.put("/_cluster/settings", json={"persistent": {"indices.recovery.max_bytes_per_sec": "41mb"}})
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "transient": {},
                        "persistent": {"indices": {"recovery": {"max_bytes_per_sec": "41mb"}}}}
    flat = client.get("/_cluster/settings", params={"flat_settings": "true"}).json()
    assert flat["persistent"]["indices.recovery.max_bytes_per_sec"] == "41mb"


def test_settings_put_flat_settings_response(client, cluster_settings):
    cluster_settings["transient"].add("cluster.max_shards_per_node")
    r = client.put("/_cluster/settings", params={"flat_settings": "true"},
                   json={"transient": {"cluster.max_shards_per_node": 1234}})
    assert r.json() == {"acknowledged": True, "persistent": {},
                        "transient": {"cluster.max_shards_per_node": "1234"}}


def test_settings_put_nested_object_form(client, cluster_settings):
    cluster_settings["transient"].add("cluster.max_shards_per_node")
    r = client.put("/_cluster/settings", json={"transient": {"cluster": {"max_shards_per_node": 2222}}})
    assert r.status_code == 200
    flat = client.get("/_cluster/settings", params={"flat_settings": "true"}).json()
    assert flat["transient"]["cluster.max_shards_per_node"] == "2222"


def test_settings_reset_to_null_removes(client, cluster_settings):
    cluster_settings["transient"].add("cluster.max_shards_per_node")
    client.put("/_cluster/settings", json={"transient": {"cluster.max_shards_per_node": 3333}})
    r = client.put("/_cluster/settings", json={"transient": {"cluster.max_shards_per_node": None}})
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "persistent": {}, "transient": {}}
    flat = client.get("/_cluster/settings", params={"flat_settings": "true"}).json()
    assert "cluster.max_shards_per_node" not in flat["transient"]
    defaults = client.get("/_cluster/settings",
                          params={"include_defaults": "true", "flat_settings": "true"}).json()["defaults"]
    assert defaults["cluster.max_shards_per_node"] == "1000"


def test_settings_reset_unset_key_is_noop(client):
    r = client.put("/_cluster/settings", json={"transient": {"cluster.max_shards_per_node": None}})
    assert r.status_code == 200
    assert r.json()["acknowledged"] is True


def test_settings_value_shows_in_defaults_only_when_unset(client, cluster_settings):
    cluster_settings["transient"].add("cluster.max_shards_per_node")
    client.put("/_cluster/settings", json={"transient": {"cluster.max_shards_per_node": 1500}})
    body = client.get("/_cluster/settings",
                      params={"include_defaults": "true", "flat_settings": "true"}).json()
    assert body["transient"]["cluster.max_shards_per_node"] == "1500"
    assert "cluster.max_shards_per_node" not in body["defaults"]


@pytest.mark.parametrize("body,etype,reason", [
    ({"transient": {"bogus.setting": 1}}, "settings_exception",
     "transient setting [bogus.setting], not recognized"),
    ({"persistent": {"bogus.setting": 1}}, "settings_exception",
     "persistent setting [bogus.setting], not recognized"),
    ({"persistent": {"index.number_of_shards": 3}}, "settings_exception",
     "persistent setting [index.number_of_shards], not recognized"),
    ({"transient": {"cluster.max_shards_per_node": "abc"}}, "illegal_argument_exception",
     "Failed to parse value [abc] for setting [cluster.max_shards_per_node]"),
    ({"transient": {"cluster.routing.allocation.enable": "bogus"}}, "illegal_argument_exception",
     "Illegal allocation.enable value [BOGUS]"),
    ({}, "action_request_validation_exception", "Validation Failed: 1: no settings to update;"),
    ({"foo": {}}, "action_request_validation_exception", "Validation Failed: 1: no settings to update;"),
    ({"transient": {}}, "action_request_validation_exception", "Validation Failed: 1: no settings to update;"),
])
def test_settings_invalid_put_400(client, body, etype, reason):
    r = client.put("/_cluster/settings", json=body)
    assert r.status_code == 400
    assert _err_type(r) == etype
    assert _err_reason(r) == reason


def test_settings_put_no_body(client):
    r = client.put("/_cluster/settings")
    assert r.status_code == 400
    assert _err_type(r) == "parse_exception"
    assert _err_reason(r) == "request body is required"


def test_settings_put_invalid_json(client):
    r = client.put("/_cluster/settings", content=b"notjson", headers={"Content-Type": "application/json"})
    assert r.status_code == 400
    assert _err_type(r) == "json_parse_exception"


@pytest.mark.parametrize("method", ["POST", "DELETE"])
def test_settings_wrong_method_405(client, method):
    r = client.request(method, "/_cluster/settings")
    assert r.status_code == 405
    assert _allow(r) == {"PUT", "GET"}
    assert r.json()["status"] == 405


# --------------------------------------------------------- content-type handling


@pytest.mark.parametrize("ctype", ["text/plain", "application/xml", "text/html"])
def test_unsupported_content_type_406(client, ctype):
    r = client.put("/_cluster/settings",
                   content=b'{"transient":{"cluster.max_shards_per_node":null}}',
                   headers={"Content-Type": ctype})
    assert r.status_code == 406
    assert r.json() == {"error": f"Content-Type header [{ctype}] is not supported", "status": 406}


def test_missing_content_type_406(client):
    r = client.put("/_cluster/settings", content=b'{"transient":{"cluster.max_shards_per_node":null}}')
    assert r.status_code == 406
    assert r.json() == {"error": "Content-Type header [] is not supported", "status": 406}


@pytest.mark.parametrize("ctype", [
    "application/json", "application/json; charset=UTF-8", "application/x-ndjson",
])
def test_accepted_content_types(client, ctype):
    r = client.put("/_cluster/settings",
                   content=b'{"transient":{"cluster.max_shards_per_node":null}}',
                   headers={"Content-Type": ctype})
    assert r.status_code == 200
    assert r.json()["acknowledged"] is True


def test_yaml_request_body_accepted(client):
    r = client.put("/_cluster/settings",
                   content=b"transient:\n  cluster.max_shards_per_node: null\n",
                   headers={"Content-Type": "application/yaml"})
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("application/yaml")


@pytest.mark.parametrize("params,headers,ctype", [
    ({"format": "yaml"}, {}, "application/yaml"),
    ({}, {"Accept": "application/yaml"}, "application/yaml"),
    ({}, {"Accept": "application/smile"}, "application/smile"),
    ({}, {"Accept": "application/cbor"}, "application/cbor"),
    ({}, {"Accept": "text/html"}, "application/json; charset=UTF-8"),
    ({}, {}, "application/json; charset=UTF-8"),
])
def test_response_format_negotiation(client, params, headers, ctype):
    r = client.get("/_cluster/health", params=params, headers=headers)
    assert r.status_code == 200
    assert r.headers["content-type"] == ctype


def test_yaml_response_body(client):
    r = client.get("/", params={"format": "yaml", "filter_path": "version.number"})
    assert r.text == '---\nversion:\n  number: "3.9.0"\n'


# ------------------------------------------------------- pretty / filter_path etc


@pytest.mark.parametrize("pretty", ["", "true"])
def test_pretty_param(client, pretty):
    r = client.get("/_cluster/health", params={"pretty": pretty, "filter_path": "number_of_nodes"})
    assert r.text == '{\n  "number_of_nodes" : 1\n}\n'


def test_not_pretty_by_default(client):
    r = client.get("/_cluster/health", params={"filter_path": "number_of_nodes"})
    assert r.text == '{"number_of_nodes":1}'


def test_pretty_empty_object_rendering(client):
    r = client.get("/_remote/info", params={"pretty": "true"})
    assert r.text == "{ }\n"


def test_state_blocks_closed_index(client, env):
    r = client.get(f"/_cluster/state/blocks/{env['closed']}")
    blocks = r.json()["blocks"]["indices"]
    assert blocks[env["closed"]] == {
        "4": {"description": "index closed", "retryable": False, "levels": ["read", "write"]}}
    assert env["green"] not in blocks


def test_filter_path_exclusions(client, env):
    r = client.get(f"/_cluster/health/{env['green']}", params={
        "filter_path": "-cluster_name,-number_of_*,-discovered*,-active*,-*_shards,-task_max_waiting_in_queue_millis",
    })
    assert r.json() == {"status": "green", "timed_out": False}


def test_filter_path_suffix_wildcard(client, env):
    r = client.get(f"/_cluster/health/{env['green']}", params={"filter_path": "*status"})
    assert r.json() == {"status": "green"}


def test_filter_path_multiple(client, env):
    r = client.get(f"/_cluster/health/{env['green']}",
                   params={"filter_path": "status,number_of_nodes,active_primary_shards"})
    assert r.json() == {"status": "green", "number_of_nodes": 1, "active_primary_shards": 2}


def test_filter_path_no_match_is_empty_object(client):
    r = client.get("/_cluster/health", params={"filter_path": "nothing_here"})
    assert r.status_code == 200
    assert r.json() == {}


def test_filter_path_double_wildcard(client, env):
    r = client.get(f"/_cluster/health/{env['green']}", params={"filter_path": "**.status", "level": "shards"})
    assert r.json() == {"indices": {env["green"]: {
        "status": "green", "shards": {"0": {"status": "green"}, "1": {"status": "green"}}}}}


def test_filter_path_mid_wildcard(client, env):
    r = client.get(f"/_cluster/health/{env['green']},{env['yellow']}",
                   params={"level": "indices", "filter_path": "indices.*.status"})
    assert r.json() == {"indices": {env["green"]: {"status": "green"}, env["yellow"]: {"status": "yellow"}}}


def test_filter_path_not_applied_to_error_response(client):
    r = client.get("/_cluster/health", params={"wait_for_status": "purple", "filter_path": "status"})
    assert r.status_code == 400
    assert set(r.json()) == {"error", "status"}
    assert _err_type(r) == "illegal_argument_exception"


def test_filter_path_exclusion_on_nested(client, env):
    r = client.get(f"/_cluster/health/{env['green']}",
                   params={"level": "shards", "filter_path": "indices,-indices.*.shards"})
    idx = r.json()["indices"][env["green"]]
    assert "shards" not in idx
    assert idx["status"] == "green"


def test_error_trace_adds_stack_trace(client):
    r = client.get("/_cluster/health", params={"wait_for_status": "purple", "error_trace": "true"})
    assert r.status_code == 400
    assert "stack_trace" in r.json()["error"]["root_cause"][0]
    assert "stack_trace" in r.json()["error"]


def test_no_stack_trace_by_default(client):
    r = client.get("/_cluster/health", params={"wait_for_status": "purple"})
    assert "stack_trace" not in r.json()["error"]
    assert set(r.json()) == {"error", "status"}
    assert set(r.json()["error"]) == {"root_cause", "type", "reason"}


def test_human_flag_on_nodes(client):
    r = client.get("/_nodes/_local/jvm", params={"human": "true", "filter_path": "nodes.*.jvm.mem.heap_max*"})
    mem = _only_node(r)["jvm"]["mem"]
    assert set(mem) == {"heap_max", "heap_max_in_bytes"}
    assert isinstance(mem["heap_max"], str)
    assert isinstance(mem["heap_max_in_bytes"], int)
    r = client.get("/_nodes/_local/jvm", params={"filter_path": "nodes.*.jvm.mem.heap_max*"})
    assert set(_only_node(r)["jvm"]["mem"]) == {"heap_max_in_bytes"}


# ---------------------------------------------------------- unknown endpoints


@pytest.mark.parametrize("path", [
    "/_cluster/bogus", "/_cat/bogus", "/_nodes/_local/stats/jvm/extra/more",
])
def test_unknown_endpoint_no_handler(client, path):
    r = client.get(path)
    assert r.status_code == 400
    assert r.json() == {"error": f"no handler found for uri [{path}] and method [GET]"}


def test_unknown_index_endpoint_no_handler(client, env):
    r = client.get(f"/{env['green']}/_bogus")
    assert r.status_code == 400
    assert r.json() == {"error": f"no handler found for uri [/{env['green']}/_bogus] and method [GET]"}


def test_unknown_underscore_top_level_is_index_name_error(client):
    r = client.get("/_bogus_endpoint")
    assert r.status_code == 400
    assert _err_type(r) == "invalid_index_name_exception"
    assert _err_reason(r) == "Invalid index name [_bogus_endpoint], must not start with '_'."


@pytest.mark.parametrize("method,path,allowed", [
    ("POST", "/_cat/indices", {"GET"}),
    ("DELETE", "/_cat/health", {"GET"}),
    ("PUT", "/_cluster/state", {"GET"}),
    ("POST", "/_cluster/stats", {"GET"}),
    ("DELETE", "/_nodes", {"GET"}),
    ("PUT", "/_cluster/pending_tasks", {"GET"}),
    ("DELETE", "/_remote/info", {"GET"}),
])
def test_wrong_method_405(client, method, path, allowed):
    r = client.request(method, path)
    assert r.status_code == 405
    assert _allow(r) == allowed
    assert r.json()["status"] == 405
    assert r.json()["error"].startswith(f"Incorrect HTTP method for uri [{path}] and method [{method}]")


# ------------------------------------------------------- allocation explain


def test_allocation_explain_unassigned_replica(client, env, node):
    r = client.post("/_cluster/allocation/explain",
                    json={"index": env["yellow"], "shard": 0, "primary": False})
    assert r.status_code == 200
    body = r.json()
    assert body["index"] == env["yellow"]
    assert body["shard"] == 0
    assert body["primary"] is False
    assert body["current_state"] == "unassigned"
    assert body["unassigned_info"]["reason"] == "INDEX_CREATED"
    assert body["can_allocate"] == "no"
    decision = body["node_allocation_decisions"][0]
    assert decision["node_id"] == node["id"]
    assert decision["node_decision"] == "no"
    assert any(d["decider"] == "same_shard" and d["decision"] == "NO" for d in decision["deciders"])


def test_allocation_explain_started_primary(client, env, node):
    r = client.request("GET", "/_cluster/allocation/explain",
                       json={"index": env["green"], "shard": 1, "primary": True})
    assert r.status_code == 200
    body = r.json()
    assert body["current_state"] == "started"
    assert body["current_node"]["id"] == node["id"]
    assert body["current_node"]["name"] == node["name"]
    assert body["can_remain_on_current_node"] == "yes"


def test_allocation_explain_no_body_picks_unassigned(client, env):
    r = client.get("/_cluster/allocation/explain")
    assert r.status_code == 200
    assert r.json()["current_state"] == "unassigned"


def test_allocation_explain_missing_index(client):
    name = f"c01-missing-{uuid.uuid4().hex[:6]}"
    r = client.post("/_cluster/allocation/explain", json={"index": name, "shard": 0, "primary": True})
    assert r.status_code == 404
    assert _err_type(r) == "index_not_found_exception"
    assert _err_reason(r) == f"no such index [{name}]"


def test_allocation_explain_missing_shard(client, env):
    r = client.post("/_cluster/allocation/explain", json={"index": env["green"], "shard": 7, "primary": True})
    assert r.status_code == 404
    assert _err_type(r) == "shard_not_found_exception"
    assert r.json()["error"]["shard"] == "7"


def test_allocation_explain_incomplete_request(client, env):
    r = client.post("/_cluster/allocation/explain", json={"index": env["green"]})
    assert r.status_code == 400
    assert _err_type(r) == "action_request_validation_exception"
    assert _err_reason(r) == "Validation Failed: 1: shard must be specified;2: primary must be specified;"


def test_allocation_explain_unknown_field(client):
    r = client.post("/_cluster/allocation/explain", json={"bogus": 1})
    assert r.status_code == 400
    assert _err_type(r) == "x_content_parse_exception"
    assert _err_reason(r) == "[1:2] [cluster/allocation/explain] unknown field [bogus]"


# ------------------------------------------------------------------- nodes


def test_nodes_info_shape(client, node):
    r = client.get("/_nodes")
    assert r.status_code == 200
    body = r.json()
    assert set(body) == {"_nodes", "cluster_name", "nodes"}
    assert body["_nodes"] == {"total": 1, "successful": 1, "failed": 0}
    assert list(body["nodes"]) == [node["id"]]
    info = body["nodes"][node["id"]]
    assert info["name"] == node["name"]
    assert info["version"] == "3.9.0"
    assert info["roles"] == ["cluster_manager", "data", "ingest", "remote_cluster_client"]
    assert {"settings", "os", "process", "jvm", "thread_pool", "transport", "http",
            "plugins", "modules", "ingest", "aggregations", "search_pipelines"} <= set(info)


def test_nodes_modules_present(client):
    info = _only_node(client.get("/_nodes/plugins"))
    names = {m["name"] for m in info["modules"]}
    assert {"analysis-common", "lang-painless", "lang-mustache", "parent-join", "percolator",
            "reindex", "mapper-extras", "ingest-common", "geo", "aggs-matrix-stats",
            "rank-eval", "search-pipeline-common"} <= names
    assert [p["name"] for p in info["plugins"]] == FULL_DISTRIBUTION_PLUGINS
    assert [m["name"] for m in info["modules"]] == sorted(names)


@pytest.mark.parametrize("metric,extra", [
    ("jvm", {"jvm"}),
    ("os", {"os"}),
    ("process", {"process"}),
    ("os,process", {"os", "process"}),
    ("settings", {"settings"}),
    ("thread_pool", {"thread_pool"}),
    ("transport", {"transport"}),
    ("http", {"http"}),
    ("ingest", {"ingest"}),
    ("aggregations", {"aggregations"}),
    ("search_pipelines", {"search_pipelines"}),
    ("plugins", {"plugins", "modules"}),
    ("indices", {"total_indexing_buffer_in_bytes"}),
    ("_none", set()),
])
def test_nodes_info_metric_filter(client, metric, extra):
    info = _only_node(client.get(f"/_nodes/_local/{metric}"))
    assert set(info) == NODE_INFO_BASE | extra


@pytest.mark.parametrize("selector", [
    "_local", "_all", "_cluster_manager", "_master", "*", "data:true", "cluster_manager:true",
    "ingest:true", "master:true", "_local,_all",
])
def test_nodes_selectors(client, selector, node):
    r = client.get(f"/_nodes/{selector}/jvm")
    assert r.status_code == 200
    assert list(r.json()["nodes"]) == [node["id"]]


def test_nodes_select_by_id_and_name(client, node):
    for sel in (node["id"], node["name"]):
        r = client.get(f"/_nodes/{sel}/os")
        assert list(r.json()["nodes"]) == [node["id"]]
    r = client.get(f"/_nodes/{node['name'][:2]}*/os")
    assert list(r.json()["nodes"]) == [node["id"]]


@pytest.mark.parametrize("path", [
    "/_nodes/info", "/_nodes/bogusnode", "/_nodes/data:false", "/_nodes/_data:true",
])
def test_nodes_unknown_selector_matches_nothing(client, path):
    # NB: "/_nodes/info" is parsed as a node-id selector, not as a metric.
    r = client.get(path)
    assert r.status_code == 200
    assert r.json() == {"_nodes": {"total": 0, "successful": 0, "failed": 0},
                        "cluster_name": "opensearch", "nodes": {}}


def test_nodes_unknown_metric_ignored(client):
    info = _only_node(client.get("/_nodes/_local/bogusmetric"))
    assert set(info) == NODE_INFO_BASE


def test_nodes_jvm_info(client):
    jvm = _only_node(client.get("/_nodes/_local/jvm"))["jvm"]
    assert {"pid", "version", "vm_name", "mem", "gc_collectors", "start_time_in_millis"} <= set(jvm)
    assert jvm["mem"]["heap_max_in_bytes"] > 0


def test_nodes_filter_path(client, node):
    r = client.get("/_nodes", params={"filter_path": "_nodes,nodes.*.version"})
    assert r.json() == {"_nodes": {"total": 1, "successful": 1, "failed": 0},
                        "nodes": {node["id"]: {"version": "3.9.0"}}}


def test_nodes_stats_shape(client):
    r = client.get("/_nodes/stats")
    assert r.status_code == 200
    assert set(r.json()) == {"_nodes", "cluster_name", "nodes"}
    stats = _only_node(r)
    assert NODE_STATS_BASE <= set(stats)
    assert {"indices", "os", "process", "jvm", "thread_pool", "fs", "transport", "http",
            "breakers", "script", "discovery", "ingest"} <= set(stats)


@pytest.mark.parametrize("metric", [
    "jvm", "os", "process", "fs", "indices", "http", "transport", "thread_pool",
    "ingest", "script", "discovery", "breaker", "adaptive_selection", "indexing_pressure",
])
def test_nodes_stats_metric_filter(client, metric):
    stats = _only_node(client.get(f"/_nodes/_local/stats/{metric}"))
    expected = "breakers" if metric == "breaker" else metric
    assert expected in stats
    other = {"jvm", "os", "fs", "indices", "http", "thread_pool", "ingest"} - {expected}
    assert not (other & set(stats))


def test_nodes_stats_multi_metric(client):
    stats = _only_node(client.get("/_nodes/stats/jvm,os"))
    assert {"jvm", "os"} <= set(stats)
    assert "fs" not in stats and "indices" not in stats


def test_nodes_stats_index_metric(client):
    stats = _only_node(client.get("/_nodes/stats/indices/docs,store"))
    # status_counter is always included in 3.x indices stats
    assert set(stats["indices"]) == {"docs", "store", "status_counter"}
    assert set(stats["indices"]["docs"]) == {"count", "deleted"}


@pytest.mark.parametrize("path,reason", [
    ("/_nodes/stats/bogusmetric", "request [/_nodes/stats/bogusmetric] contains unrecognized metric: [bogusmetric]"),
    ("/_nodes/stats/breakers",
     "request [/_nodes/stats/breakers] contains unrecognized metric: [breakers] -> did you mean [breaker]?"),
    ("/_nodes/stats/indices/bogus", "request [/_nodes/stats/indices/bogus] contains unrecognized index metric: [bogus]"),
    ("/_nodes/stats/_all/bogus", "request [/_nodes/stats/_all/bogus] contains index metrics [bogus] but all stats requested"),
])
def test_nodes_stats_bad_metric_400(client, path, reason):
    r = client.get(path)
    assert r.status_code == 400
    assert _err_type(r) == "illegal_argument_exception"
    assert _err_reason(r) == reason


def test_nodes_stats_level_indices(client, env):
    r = client.get("/_nodes/stats/indices/docs", params={
        "level": "indices", "filter_path": f"nodes.*.indices.indices.{env['green']}.docs"})
    stats = _only_node(r)
    assert stats["indices"]["indices"][env["green"]]["docs"] == {"count": 3, "deleted": 0}


def test_nodes_stats_bad_level_is_node_failure(client):
    r = client.get("/_nodes/stats", params={"level": "bogus"})
    assert r.status_code == 200
    body = r.json()
    assert body["nodes"] == {}
    assert body["_nodes"]["total"] == 1
    assert body["_nodes"]["successful"] == 0
    assert body["_nodes"]["failed"] == 1
    failure = body["_nodes"]["failures"][0]
    assert failure["type"] == "failed_node_exception"
    assert failure["caused_by"]["type"] == "illegal_argument_exception"


def test_nodes_usage(client):
    r = client.get("/_nodes/usage")
    assert r.status_code == 200
    usage = _only_node(r)
    assert {"timestamp", "since", "rest_actions"} <= set(usage)


def test_nodes_hot_threads_plain_text(client, node):
    r = client.get("/_nodes/hot_threads")
    assert r.status_code == 200
    assert r.headers["content-type"].startswith("text/plain")
    assert r.text.startswith(f"::: {{{node['name']}}}{{{node['id']}}}")
    assert "Hot threads at " in r.text


# --------------------------------------------------------------------- tasks


def test_tasks_list_shape(client, node):
    r = client.get("/_tasks", params={"actions": "cluster:monitor/tasks/lists*"})
    assert r.status_code == 200
    body = r.json()
    assert set(body) == {"nodes"}
    entry = body["nodes"][node["id"]]
    assert entry["name"] == node["name"]
    actions = sorted(t["action"] for t in entry["tasks"].values())
    assert actions == ["cluster:monitor/tasks/lists", "cluster:monitor/tasks/lists[n]"]
    for tid, task in entry["tasks"].items():
        assert tid == f"{task['node']}:{task['id']}"
        assert {"node", "id", "type", "action", "start_time_in_millis",
                "running_time_in_nanos", "cancellable", "cancelled", "headers"} <= set(task)


def test_tasks_parent_child_relation(client, node):
    tasks = client.get("/_tasks", params={"actions": "*tasks/lists*"}).json()["nodes"][node["id"]]["tasks"]
    parent = next(k for k, t in tasks.items() if t["action"] == "cluster:monitor/tasks/lists")
    child = next(t for t in tasks.values() if t["action"].endswith("[n]"))
    assert child["parent_task_id"] == parent
    assert "parent_task_id" not in tasks[parent]
    assert tasks[parent]["type"] == "transport"
    assert child["type"] == "direct"


def test_tasks_detailed_has_description(client, node):
    tasks = client.get("/_tasks", params={"actions": "*tasks/lists*", "detailed": "true"}).json()[
        "nodes"][node["id"]]["tasks"]
    assert all("description" in t for t in tasks.values())
    plain = client.get("/_tasks", params={"actions": "*tasks/lists*"}).json()["nodes"][node["id"]]["tasks"]
    assert all("description" not in t for t in plain.values())


def test_tasks_group_by_none(client):
    r = client.get("/_tasks", params={"group_by": "none", "actions": "*tasks/lists*"})
    body = r.json()
    assert set(body) == {"tasks"}
    assert isinstance(body["tasks"], list)
    assert len(body["tasks"]) == 2


def test_tasks_group_by_parents(client):
    body = client.get("/_tasks", params={"group_by": "parents", "actions": "*tasks/lists*"}).json()
    assert set(body) == {"tasks"}
    (parent,) = body["tasks"].values()
    assert parent["action"] == "cluster:monitor/tasks/lists"
    assert [c["action"] for c in parent["children"]] == ["cluster:monitor/tasks/lists[n]"]


def test_tasks_actions_filter_no_match(client):
    r = client.get("/_tasks", params={"actions": "nothing:matches*"})
    assert r.status_code == 200
    assert r.json() == {"nodes": {}}


def test_tasks_group_by_invalid(client):
    r = client.get("/_tasks", params={"group_by": "bogus"})
    assert r.status_code == 400
    assert _err_type(r) == "illegal_argument_exception"
    assert _err_reason(r) == "[group_by] must be one of [nodes], [parents] or [none] but was [bogus]"


def test_tasks_malformed_id(client):
    r = client.get("/_tasks/bogus")
    assert r.status_code == 400
    assert _err_type(r) == "illegal_argument_exception"
    assert _err_reason(r) == "malformed task id bogus"


def test_tasks_unknown_task_404(client, node):
    r = client.get(f"/_tasks/{node['id']}:999999999")
    assert r.status_code == 404
    assert _err_type(r) == "resource_not_found_exception"
    assert _err_reason(r) == f"task [{node['id']}:999999999] isn't running and hasn't stored its results"


def test_tasks_unknown_node_404(client):
    r = client.get("/_tasks/nosuchnode:123")
    assert r.status_code == 404
    assert _err_type(r) == "resource_not_found_exception"
    assert _err_reason(r).startswith("task [nosuchnode:123] belongs to the node [nosuchnode]")


def test_tasks_cancel_nothing(client):
    r = client.post("/_tasks/_cancel", params={"actions": "nothing:matches*"})
    assert r.status_code == 200
    assert r.json() == {"nodes": {}}


def test_cluster_pending_tasks_shape(client):
    r = client.get("/_cluster/pending_tasks")
    assert r.status_code == 200
    assert set(r.json()) == {"tasks"}
    assert isinstance(r.json()["tasks"], list)


def test_remote_info_empty(client):
    r = client.get("/_remote/info")
    assert r.status_code == 200
    assert r.json() == {}


# ----------------------------------------------------------------------- _cat


def test_cat_root_lists_endpoints(client):
    r = client.get("/_cat")
    assert r.status_code == 200
    assert r.headers["content-type"] == "text/plain; charset=UTF-8"
    lines = r.text.splitlines()
    assert lines[0] == "=^.^="
    for ep in ("/_cat/indices", "/_cat/indices/{index}", "/_cat/health", "/_cat/count/{index}",
               "/_cat/aliases/{alias}", "/_cat/shards", "/_cat/nodes", "/_cat/templates",
               "/_cat/segments", "/_cat/allocation", "/_cat/plugins", "/_cat/cluster_manager",
               "/_cat/thread_pool/{thread_pools}", "/_cat/pending_tasks", "/_cat/tasks",
               "/_cat/recovery", "/_cat/nodeattrs", "/_cat/repositories", "/_cat/fielddata"):
        assert ep in lines
    assert "/_cat/master" not in lines


def test_cat_indices_default_columns(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"v": "true"})
    assert r.status_code == 200
    assert r.headers["content-type"] == "text/plain; charset=UTF-8"
    assert _header(r.text) == CAT_INDICES_DEFAULT
    row = _rows(r.text)[1]
    assert row[:7] == ["green", "open", env["green"], row[3], "2", "0", "3"]


def test_cat_indices_without_v_has_no_header(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"h": "index,docs.count"})
    assert r.text.split() == [env["green"], "3"]


@pytest.mark.parametrize("v", ["false", "true"])
def test_cat_v_flag_values(client, env, v):
    r = client.get(f"/_cat/indices/{env['green']}", params={"h": "index", "v": v})
    expected = f"index\n{env['green']}\n" if v == "true" else f"{env['green']}\n"
    assert r.text == expected


def test_cat_v_invalid(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"h": "index", "v": "bogus"})
    assert r.status_code == 400
    assert _err_reason(r) == "Failed to parse value [bogus] as only [true] or [false] are allowed."


def test_cat_alignment_text_left_numbers_right(client, env):
    name = env["green"]
    r = client.get(f"/_cat/indices/{name}", params={"h": "index,docs.count", "v": "true"})
    assert r.text == f"{'index'.ljust(len(name))} docs.count\n{name} {'3'.rjust(10)}\n"


def test_cat_indices_json(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"format": "json"})
    assert r.headers["content-type"] == "application/json; charset=UTF-8"
    (row,) = r.json()
    assert list(row) == CAT_INDICES_DEFAULT
    assert row["index"] == env["green"]
    assert row["pri"] == "2" and row["rep"] == "0"
    assert row["docs.count"] == "3"
    assert row["health"] == "green"


def test_cat_json_values_are_strings(client, env):
    (row,) = client.get(f"/_cat/indices/{env['green']}", params={"format": "json"}).json()
    assert all(isinstance(v, str) for v in row.values())


def test_cat_format_uppercase_json(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"format": "JSON", "h": "index"})
    assert r.json() == [{"index": env["green"]}]


def test_cat_format_yaml(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"format": "yaml", "h": "index,pri"})
    assert r.headers["content-type"] == "application/yaml"
    assert r.text == f'---\n- index: "{env["green"]}"\n  pri: "2"\n'


def test_cat_format_unknown_falls_back_to_text(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"format": "bogus", "h": "index"})
    assert r.status_code == 200
    assert r.headers["content-type"] == "text/plain; charset=UTF-8"
    assert r.text == f"{env['green']}\n"


def test_cat_json_pretty(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"format": "json", "h": "index", "pretty": "true"})
    assert r.text == f'[\n  {{\n    "index" : "{env["green"]}"\n  }}\n]\n'


def test_cat_json_filter_path(client, env):
    r = client.get(f"/_cat/indices/{env['green']}",
                   params={"format": "json", "h": "index,docs.count", "filter_path": "index"})
    assert r.json() == [{"index": env["green"]}]


@pytest.mark.parametrize("h,keys", [
    ("i,h,s", ["i", "h", "s"]),
    ("idx,dc,p,r", ["idx", "dc", "p", "r"]),
    ("index,shardsPrimary,docsCount", ["index", "shardsPrimary", "docsCount"]),
])
def test_cat_h_aliases_keep_requested_names(client, env, h, keys):
    (row,) = client.get(f"/_cat/indices/{env['green']}", params={"format": "json", "h": h}).json()
    assert list(row) == keys
    assert row[keys[0]] == env["green"]


def test_cat_h_alias_values(client, env):
    (row,) = client.get(f"/_cat/indices/{env['green']}",
                        params={"format": "json", "h": "i,h,s,p,r,dc,dd"}).json()
    assert row == {"i": env["green"], "h": "green", "s": "open", "p": "2", "r": "0", "dc": "3", "dd": "0"}


def test_cat_h_wildcard(client, env):
    (row,) = client.get(f"/_cat/indices/{env['green']}", params={"format": "json", "h": "docs.*"}).json()
    assert list(row) == ["docs.count", "docs.deleted"]


def test_cat_h_unknown_column_ignored(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"h": "index,bogus", "v": "true"})
    assert r.text == f"index\n{env['green']}\n"


def test_cat_h_only_unknown_column(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"h": "bogus"})
    assert r.status_code == 200
    assert r.text == "\n"


def test_cat_creation_date_columns(client, env):
    (row,) = client.get(f"/_cat/indices/{env['green']}",
                        params={"format": "json", "h": "creation.date,creation.date.string"}).json()
    assert row["creation.date"].isdigit()
    assert row["creation.date.string"].endswith("Z")
    assert "T" in row["creation.date.string"]


def test_cat_help_indices(client):
    r = client.get("/_cat/indices", params={"help": "true"})
    assert r.status_code == 200
    assert r.headers["content-type"] == "text/plain; charset=UTF-8"
    cols = [line.split("|")[0].strip() for line in r.text.splitlines()]
    assert cols[:12] == CAT_INDICES_DEFAULT[:8] + ["creation.date", "creation.date.string",
                                                   "store.size", "pri.store.size"]
    first = [c.strip() for c in r.text.splitlines()[2].split("|")]
    assert first == ["index", "i,idx", "index name"]


def test_cat_help_with_index_in_path_is_rejected(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"help": "true"})
    assert r.status_code == 400
    assert _err_reason(r) == f"request [/_cat/indices/{env['green']}] contains unrecognized parameter: [index]"


def test_cat_unknown_param(client):
    r = client.get("/_cat/health", params={"bogus": "1"})
    assert r.status_code == 400
    assert _err_type(r) == "illegal_argument_exception"
    assert _err_reason(r) == "request [/_cat/health] contains unrecognized parameter: [bogus]"


def test_cat_indices_sort_asc_desc(client, env):
    pat = f"{env['prefix']}-*"
    asc = client.get(f"/_cat/indices/{pat}", params={"h": "index", "s": "index"}).text.split()
    assert asc == [env["closed"], env["green"], env["yellow"]]
    desc = client.get(f"/_cat/indices/{pat}", params={"h": "index", "s": "index:desc"}).text.split()
    assert desc == [env["yellow"], env["green"], env["closed"]]
    asc2 = client.get(f"/_cat/indices/{pat}", params={"h": "index", "s": "index:asc"}).text.split()
    assert asc2 == asc


def test_cat_indices_sort_multi_key(client, env):
    pat = f"{env['prefix']}-*"
    r = client.get(f"/_cat/indices/{pat}", params={"h": "index,pri", "s": "pri:desc,index:desc"})
    assert [row[0] for row in _rows(r.text)] == [env["green"], env["yellow"], env["closed"]]


def test_cat_sort_by_alias_name(client, env):
    pat = f"{env['prefix']}-*"
    r = client.get(f"/_cat/indices/{pat}", params={"h": "i", "s": "i:desc"})
    assert r.text.split() == [env["yellow"], env["green"], env["closed"]]


def test_cat_sort_numeric_desc(client, env):
    pat = f"{env['prefix']}-*"
    r = client.get(f"/_cat/indices/{pat}",
                   params={"h": "index,docs.count", "s": "docs.count:desc,index", "expand_wildcards": "open"})
    assert [row[0] for row in _rows(r.text)] == [env["green"], env["yellow"]]


@pytest.mark.parametrize("s", ["bogus", "index:sideways"])
def test_cat_sort_unknown_key_500(client, env, s):
    r = client.get(f"/_cat/indices/{env['green']}", params={"s": s})
    assert r.status_code == 500
    assert _err_type(r) == "unsupported_operation_exception"
    assert _err_reason(r) == f"Unable to sort by unknown sort key `{s}`"


@pytest.mark.parametrize("health,expected", [("green", "green"), ("yellow", "yellow")])
def test_cat_indices_health_filter(client, env, health, expected):
    r = client.get(f"/_cat/indices/{env['prefix']}-*",
                   params={"h": "index", "health": health, "expand_wildcards": "open"})
    assert r.text.split() == [env[expected]]


def test_cat_indices_health_filter_red_empty(client, env):
    r = client.get(f"/_cat/indices/{env['prefix']}-*", params={"h": "index", "health": "red"})
    assert r.text == ""


def test_cat_indices_health_filter_invalid(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"health": "bogus"})
    assert r.status_code == 400
    assert _err_reason(r) == "unknown cluster health status [bogus]"


@pytest.mark.parametrize("ew,expected", [
    ("open", ["green", "yellow"]),
    ("closed", ["closed"]),
    ("all", ["closed", "green", "yellow"]),
])
def test_cat_indices_expand_wildcards(client, env, ew, expected):
    r = client.get(f"/_cat/indices/{env['prefix']}-*",
                   params={"h": "index", "expand_wildcards": ew, "s": "index"})
    assert r.text.split() == [env[k] for k in expected]


def test_cat_indices_closed_index_row(client, env):
    (row,) = client.get(f"/_cat/indices/{env['closed']}", params={"format": "json"}).json()
    assert row["status"] == "close"
    assert row["health"] == "green"
    assert row["pri"] == "1"
    assert row["docs.count"] is None
    assert row["store.size"] is None


def test_cat_indices_closed_index_text_blank_cells(client, env):
    r = client.get(f"/_cat/indices/{env['closed']}", params={"h": "index,status,docs.count"})
    assert r.text.split() == [env["closed"], "close"]


def test_cat_indices_missing_404(client):
    name = f"c01-missing-{uuid.uuid4().hex[:6]}"
    r = client.get(f"/_cat/indices/{name}")
    assert r.status_code == 404
    assert r.headers["content-type"].startswith("application/json")
    assert _err_type(r) == "index_not_found_exception"
    assert _err_reason(r) == f"no such index [{name}]"
    assert r.json()["error"]["resource.type"] == "index_or_alias"


def test_cat_indices_missing_with_ignore_unavailable(client, env):
    r = client.get(f"/_cat/indices/{env['green']},c01-missing-{uuid.uuid4().hex[:6]}",
                   params={"h": "index", "ignore_unavailable": "true"})
    assert r.status_code == 200
    assert r.text.split() == [env["green"]]


def test_cat_indices_partial_missing_404(client, env):
    r = client.get(f"/_cat/indices/{env['green']},c01-missing-{uuid.uuid4().hex[:6]}")
    assert r.status_code == 404


def test_cat_indices_wildcard_no_match(client):
    pat = f"c01-nomatch-{uuid.uuid4().hex[:6]}*"
    assert client.get(f"/_cat/indices/{pat}").text == ""
    assert client.get(f"/_cat/indices/{pat}", params={"format": "json"}).json() == []
    assert client.get(f"/_cat/indices/{pat}", params={"v": "true"}).text == " ".join(CAT_INDICES_DEFAULT) + "\n"


def test_cat_indices_via_alias(client, env):
    r = client.get(f"/_cat/indices/{env['alias']}", params={"h": "index"})
    assert r.text.split() == [env["green"]]


def test_cat_indices_comma_list(client, env):
    r = client.get(f"/_cat/indices/{env['green']},{env['yellow']}", params={"h": "index", "s": "index"})
    assert r.text.split() == [env["green"], env["yellow"]]


def test_cat_bytes_b_is_integer(client, env):
    (row,) = client.get(f"/_cat/indices/{env['green']}",
                        params={"format": "json", "h": "store.size,pri.store.size", "bytes": "b"}).json()
    assert row["store.size"].isdigit()
    assert row["store.size"] == row["pri.store.size"]


@pytest.mark.parametrize("unit,divisor", [("kb", 1024), ("mb", 1024 ** 2), ("gb", 1024 ** 3)])
def test_cat_bytes_units(client, env, unit, divisor):
    b = int(client.get(f"/_cat/indices/{env['green']}",
                       params={"h": "pri.store.size", "bytes": "b"}).text.strip())
    v = client.get(f"/_cat/indices/{env['green']}", params={"h": "pri.store.size", "bytes": unit}).text.strip()
    assert v.isdigit()
    assert int(v) == b // divisor


def test_cat_bytes_default_human(client, env):
    v = client.get(f"/_cat/indices/{env['green']}", params={"h": "store.size"}).text.strip()
    assert v[-2:] in ("kb", "mb") or v.endswith("b")
    assert not v.isdigit()


def test_cat_bytes_invalid_unit_ignored(client, env):
    r = client.get(f"/_cat/indices/{env['green']}", params={"h": "store.size", "bytes": "bogus"})
    assert r.status_code == 200
    assert not r.text.strip().isdigit()


# -- _cat/health


def test_cat_health_columns(client):
    r = client.get("/_cat/health", params={"v": "true"})
    assert _header(r.text) == CAT_HEALTH_DEFAULT
    row = _rows(r.text)[1]
    assert row[2] == "opensearch"
    assert row[3] in ("green", "yellow", "red")
    assert row[4] == "1" and row[5] == "1" and row[6] == "true"


def test_cat_health_ts_false(client):
    r = client.get("/_cat/health", params={"v": "true", "ts": "false"})
    assert _header(r.text) == CAT_HEALTH_DEFAULT[2:]


def test_cat_health_json(client):
    (row,) = client.get("/_cat/health", params={"format": "json"}).json()
    assert list(row) == CAT_HEALTH_DEFAULT
    assert row["epoch"].isdigit()
    assert len(row["timestamp"]) == 8 and row["timestamp"].count(":") == 2
    assert row["active_shards_percent"].endswith("%")
    assert row["node.total"] == "1"
    assert row["cluster"] == "opensearch"


def test_cat_health_h_aliases(client):
    (row,) = client.get("/_cat/health", params={"format": "json", "h": "cl,nt,nd,dcm"}).json()
    assert row == {"cl": "opensearch", "nt": "1", "nd": "1", "dcm": "true"}


def test_cat_health_help(client):
    r = client.get("/_cat/health", params={"help": "true"})
    lines = [[c.strip() for c in line.split("|")] for line in r.text.splitlines()]
    assert [line[0] for line in lines] == CAT_HEALTH_DEFAULT
    assert lines[2] == ["cluster", "cl", "cluster name"]
    assert lines[3] == ["status", "st", "health status"]


# -- _cat/count


def test_cat_count(client, env):
    r = client.get(f"/_cat/count/{env['green']}", params={"v": "true"})
    assert _header(r.text) == ["epoch", "timestamp", "count"]
    assert _rows(r.text)[1][2] == "3"


def test_cat_count_json(client, env):
    (row,) = client.get(f"/_cat/count/{env['green']}", params={"format": "json"}).json()
    assert list(row) == ["epoch", "timestamp", "count"]
    assert row["count"] == "3"


def test_cat_count_multi_index_sum(client, env):
    r = client.get(f"/_cat/count/{env['green']},{env['yellow']}", params={"h": "count"})
    assert r.text == "3\n"


def test_cat_count_filtered_alias(client, env):
    assert client.get(f"/_cat/count/{env['falias']}", params={"h": "count"}).text == "2\n"
    assert client.get(f"/_cat/count/{env['alias']}", params={"h": "count"}).text == "3\n"


def test_cat_count_empty_index(client, env):
    assert client.get(f"/_cat/count/{env['yellow']}", params={"h": "count"}).text == "0\n"


def test_cat_count_missing_404(client):
    r = client.get(f"/_cat/count/c01-missing-{uuid.uuid4().hex[:6]}")
    assert r.status_code == 404
    assert _err_type(r) == "index_not_found_exception"


def test_cat_count_closed_400(client, env):
    r = client.get(f"/_cat/count/{env['closed']}")
    assert r.status_code == 400
    assert _err_type(r) == "index_closed_exception"
    assert _err_reason(r) == "closed"


# -- _cat/aliases


def test_cat_aliases(client, env):
    r = client.get(f"/_cat/aliases/{env['prefix']}-*", params={"v": "true", "s": "alias"})
    assert _header(r.text) == CAT_ALIASES_DEFAULT
    rows = _rows(r.text)[1:]
    assert rows == [
        [env["alias"], env["green"], "-", "-", "-", "-"],
        [env["falias"], env["green"], "*", "-", "-", "-"],
        [env["ralias"], env["yellow"], "-", "r1", "r1", "true"],
    ]


def test_cat_aliases_json(client, env):
    rows = client.get(f"/_cat/aliases/{env['ralias']}", params={"format": "json"}).json()
    assert rows == [{"alias": env["ralias"], "index": env["yellow"], "filter": "-",
                     "routing.index": "r1", "routing.search": "r1", "is_write_index": "true"}]


def test_cat_aliases_no_match(client):
    name = f"c01-noalias-{uuid.uuid4().hex[:6]}"
    r = client.get(f"/_cat/aliases/{name}")
    assert r.status_code == 200
    assert r.text == ""
    assert client.get(f"/_cat/aliases/{name}", params={"format": "json"}).json() == []


def test_cat_aliases_help(client):
    r = client.get("/_cat/aliases", params={"help": "true"})
    lines = [[c.strip() for c in line.split("|")] for line in r.text.splitlines()]
    assert lines == [
        ["alias", "a", "alias name"],
        ["index", "i,idx", "index alias points to"],
        ["filter", "f,fi", "filter"],
        ["routing.index", "ri,routingIndex", "index routing"],
        ["routing.search", "rs,routingSearch", "search routing"],
        ["is_write_index", "w,isWriteIndex", "write index"],
    ]


# -- _cat/shards


def test_cat_shards(client, env, node):
    r = client.get(f"/_cat/shards/{env['green']}", params={"v": "true", "s": "shard"})
    assert _header(r.text) == CAT_SHARDS_DEFAULT
    rows = _rows(r.text)[1:]
    assert [row[:4] for row in rows] == [
        [env["green"], "0", "p", "STARTED"], [env["green"], "1", "p", "STARTED"]]
    assert sum(int(row[4]) for row in rows) == 3
    assert all(row[-1] == node["name"] for row in rows)


def test_cat_shards_unassigned_replica(client, env):
    rows = client.get(f"/_cat/shards/{env['yellow']}",
                      params={"format": "json", "h": "index,shard,prirep,state,docs,node", "s": "prirep"}).json()
    assert rows[0]["prirep"] == "p" and rows[0]["state"] == "STARTED" and rows[0]["docs"] == "0"
    assert rows[1] == {"index": env["yellow"], "shard": "0", "prirep": "r",
                       "state": "UNASSIGNED", "docs": None, "node": None}


def test_cat_shards_unassigned_reason(client, env):
    rows = client.get(f"/_cat/shards/{env['yellow']}",
                      params={"format": "json", "h": "prirep,unassigned.reason", "s": "prirep"}).json()
    assert rows == [{"prirep": "p", "unassigned.reason": None},
                    {"prirep": "r", "unassigned.reason": "INDEX_CREATED"}]


def test_cat_shards_missing_404(client):
    r = client.get(f"/_cat/shards/c01-missing-{uuid.uuid4().hex[:6]}")
    assert r.status_code == 404
    assert _err_type(r) == "index_not_found_exception"


def test_cat_shards_closed_400(client, env):
    r = client.get(f"/_cat/shards/{env['closed']}")
    assert r.status_code == 400
    assert _err_type(r) == "index_closed_exception"


def test_cat_shards_help_aliases(client):
    r = client.get("/_cat/shards", params={"help": "true"})
    lines = [[c.strip() for c in line.split("|")] for line in r.text.splitlines()]
    assert lines[0] == ["index", "i,idx", "index name"]
    assert lines[1] == ["shard", "s,sh", "shard name"]
    assert lines[2] == ["prirep", "p,pr,primaryOrReplica", "primary or replica"]


# -- _cat/nodes, master, cluster_manager


def test_cat_nodes_columns(client, node):
    r = client.get("/_cat/nodes", params={"v": "true"})
    assert _header(r.text) == CAT_NODES_DEFAULT
    row = _rows(r.text)[1]
    assert row[7] == "dimr"
    assert row[8] == "cluster_manager,data,ingest,remote_cluster_client"
    assert row[9] == "*"
    assert row[10] == node["name"]


def test_cat_nodes_json_columns(client, node):
    (row,) = client.get("/_cat/nodes", params={
        "format": "json", "h": "name,node.role,cluster_manager,master,version"}).json()
    assert row == {"name": node["name"], "node.role": "dimr", "cluster_manager": "*",
                   "master": "*", "version": "3.9.0"}


def test_cat_nodes_id_truncated_unless_full_id(client, node):
    short = client.get("/_cat/nodes", params={"format": "json", "h": "id"}).json()
    full = client.get("/_cat/nodes", params={"format": "json", "h": "id", "full_id": "true"}).json()
    assert short == [{"id": node["id"][:4]}]
    assert full == [{"id": node["id"]}]


@pytest.mark.parametrize("path", ["/_cat/cluster_manager", "/_cat/master"])
def test_cat_cluster_manager(client, node, path):
    r = client.get(path, params={"format": "json"})
    assert r.status_code == 200
    (row,) = r.json()
    assert list(row) == ["id", "host", "ip", "node"]
    assert row["id"] == node["id"]
    assert row["node"] == node["name"]


def test_cat_master_deprecation_header(client):
    # the node de-duplicates deprecation warnings, so the header may only appear once
    r = client.get("/_cat/master")
    if "warning" in r.headers:
        assert "[GET /_cat/master] is deprecated! Use [GET /_cat/cluster_manager] instead." in r.headers["warning"]
    assert "warning" not in client.get("/_cat/cluster_manager").headers


def test_cat_cluster_manager_text(client, node):
    r = client.get("/_cat/cluster_manager", params={"v": "true"})
    assert _header(r.text) == ["id", "host", "ip", "node"]
    assert _rows(r.text)[1][0] == node["id"]


def test_cat_nodeattrs(client, node):
    r = client.get("/_cat/nodeattrs", params={"v": "true"})
    assert _header(r.text) == ["node", "host", "ip", "attr", "value"]
    rows = client.get("/_cat/nodeattrs", params={"format": "json"}).json()
    (info,) = client.get(f"/_nodes/{node['id']}").json()["nodes"].values()
    assert {"node": node["name"], "host": info["host"], "ip": info["ip"],
            "attr": "shard_indexing_pressure_enabled", "value": "true"} in rows


# -- plugins, pending tasks, thread pools, misc


def test_cat_plugins_lists_bundled_plugins(client, node):
    r = client.get("/_cat/plugins", params={"v": "true"})
    assert _header(r.text) == ["name", "component", "version"]
    assert len(r.text.splitlines()) == len(FULL_DISTRIBUTION_PLUGINS) + 1
    rows = client.get("/_cat/plugins", params={"format": "json"}).json()
    assert [row["component"] for row in rows] == FULL_DISTRIBUTION_PLUGINS
    assert {(row["name"], row["version"]) for row in rows} == {(node["name"], "3.9.0.0")}


def test_cat_pending_tasks(client):
    r = client.get("/_cat/pending_tasks", params={"v": "true"})
    assert _header(r.text) == ["insertOrder", "timeInQueue", "priority", "source"]
    assert isinstance(client.get("/_cat/pending_tasks", params={"format": "json"}).json(), list)


def test_cat_thread_pool_columns(client, node):
    r = client.get("/_cat/thread_pool", params={"v": "true"})
    assert _header(r.text) == ["node_name", "name", "active", "queue", "rejected"]
    names = [row[1] for row in _rows(r.text)[1:]]
    assert names == sorted(names)
    assert {"search", "write", "get", "management", "generic"} <= set(names)


def test_cat_thread_pool_filtered(client):
    r = client.get("/_cat/thread_pool/search,write", params={"v": "true", "h": "name,type,queue_size", "s": "name"})
    rows = _rows(r.text)
    assert rows[0] == ["name", "type", "queue_size"]
    assert [row[:2] for row in rows[1:]] == [["search", "resizable"], ["write", "fixed"]]


def test_cat_thread_pool_wildcard(client):
    rows = client.get("/_cat/thread_pool/sea*", params={"format": "json", "h": "name"}).json()
    names = {r["name"] for r in rows}
    assert "search" in names
    assert all(n.startswith("sea") for n in names)


def test_cat_thread_pool_unknown_name_empty(client):
    r = client.get("/_cat/thread_pool/nonexistent_pool", params={"v": "true"})
    assert r.status_code == 200
    assert r.text == "node_name name active queue rejected\n"


def test_cat_repositories_empty(client):
    r = client.get("/_cat/repositories", params={"v": "true"})
    assert _header(r.text) == ["id", "type"]


def test_cat_fielddata_header(client):
    r = client.get("/_cat/fielddata", params={"v": "true"})
    assert _header(r.text) == ["id", "host", "ip", "node", "field", "size"]


def test_cat_allocation(client, node):
    r = client.get("/_cat/allocation", params={"v": "true"})
    assert _header(r.text) == CAT_ALLOCATION_DEFAULT
    rows = client.get("/_cat/allocation", params={"format": "json", "bytes": "b"}).json()
    mine = next(r for r in rows if r["node"] == node["name"])
    assert list(mine) == CAT_ALLOCATION_DEFAULT
    assert int(mine["shards"]) >= 3
    assert mine["disk.indices"].isdigit()
    assert mine["disk.total"].isdigit()


def test_cat_allocation_unassigned_row(client, env):
    rows = client.get("/_cat/allocation", params={"format": "json"}).json()
    un = next(r for r in rows if r["node"] == "UNASSIGNED")
    assert int(un["shards"]) >= 1
    assert un["disk.indices"] is None
    assert un["host"] is None


def test_cat_recovery(client, env):
    rows = client.get(f"/_cat/recovery/{env['green']}",
                      params={"format": "json", "h": "index,shard,type,stage", "s": "shard"}).json()
    assert rows == [
        {"index": env["green"], "shard": "0", "type": "empty_store", "stage": "done"},
        {"index": env["green"], "shard": "1", "type": "empty_store", "stage": "done"},
    ]


def test_cat_segments(client, env):
    r = client.get(f"/_cat/segments/{env['green']}", params={"v": "true"})
    assert _header(r.text) == CAT_SEGMENTS_DEFAULT
    rows = client.get(f"/_cat/segments/{env['green']}", params={"format": "json"}).json()
    assert sum(int(r["docs.count"]) for r in rows) == 3
    assert all(r["searchable"] == "true" for r in rows)
    assert all(r["segment"].startswith("_") for r in rows)
    assert all(r["version"] == "10.5.1" for r in rows)


def test_cat_segments_empty_index(client, env):
    r = client.get(f"/_cat/segments/{env['yellow']}", params={"v": "true"})
    assert r.text == " ".join(CAT_SEGMENTS_DEFAULT) + "\n"


def test_cat_tasks(client):
    r = client.get("/_cat/tasks", params={"v": "true", "actions": "*tasks/lists*"})
    assert _header(r.text) == ["action", "task_id", "parent_task_id", "type", "start_time",
                               "timestamp", "running_time", "ip", "node"]
    actions = sorted(row[0] for row in _rows(r.text)[1:])
    assert actions == ["cluster:monitor/tasks/lists", "cluster:monitor/tasks/lists[n]"]


# -- _cat/templates


@pytest.fixture
def templates(client):
    p = f"c01t-{uuid.uuid4().hex[:8]}"
    names = {"legacy": f"{p}-legacy", "comp": f"{p}-comp", "tpl1": f"{p}-tpl1",
             "tpl2": f"{p}-tpl2", "prefix": p}
    try:
        r = client.put(f"/_template/{names['legacy']}", json={"index_patterns": [f"{p}-legx-*"], "order": 2, "version": 9})
        assert r.status_code == 200, r.text
        r = client.put(f"/_component_template/{names['comp']}", json={"template": {"settings": {"number_of_replicas": 0}}})
        assert r.status_code == 200, r.text
        r = client.put(f"/_index_template/{names['tpl1']}",
                       json={"index_patterns": [f"{p}-x-*"], "priority": 7, "version": 3})
        assert r.status_code == 200, r.text
        r = client.put(f"/_index_template/{names['tpl2']}",
                       json={"index_patterns": [f"{p}-y-*", f"{p}-z-*"], "composed_of": [names["comp"]]})
        assert r.status_code == 200, r.text
        yield names
    finally:
        client.delete(f"/_index_template/{names['tpl1']}")
        client.delete(f"/_index_template/{names['tpl2']}")
        client.delete(f"/_component_template/{names['comp']}")
        client.delete(f"/_template/{names['legacy']}")


def test_cat_templates_text(client, templates):
    p = templates["prefix"]
    r = client.get(f"/_cat/templates/{p}-*", params={"v": "true", "s": "name"})
    lines = r.text.splitlines()
    assert lines[0].split() == ["name", "index_patterns", "order", "version", "composed_of"]
    assert lines[1].split() == [templates["legacy"], f"[{p}-legx-*]", "2", "9"]
    assert lines[2].split() == [templates["tpl1"], f"[{p}-x-*]", "7", "3", "[]"]
    assert lines[3].split() == [templates["tpl2"], f"[{p}-y-*,", f"{p}-z-*]", "0", f"[{templates['comp']}]"]


def test_cat_templates_json(client, templates):
    p = templates["prefix"]
    rows = client.get(f"/_cat/templates/{p}-*", params={"format": "json", "s": "name"}).json()
    assert rows == [
        {"name": templates["legacy"], "index_patterns": f"[{p}-legx-*]", "order": "2",
         "version": "9", "composed_of": ""},
        {"name": templates["tpl1"], "index_patterns": f"[{p}-x-*]", "order": "7",
         "version": "3", "composed_of": "[]"},
        {"name": templates["tpl2"], "index_patterns": f"[{p}-y-*, {p}-z-*]", "order": "0",
         "version": None, "composed_of": f"[{templates['comp']}]"},
    ]


def test_cat_templates_exact_name(client, templates):
    rows = client.get(f"/_cat/templates/{templates['tpl1']}", params={"format": "json", "h": "name"}).json()
    assert rows == [{"name": templates["tpl1"]}]


def test_cat_templates_no_match(client):
    r = client.get(f"/_cat/templates/c01t-none-{uuid.uuid4().hex[:6]}", params={"format": "json"})
    assert r.status_code == 200
    assert r.json() == []
