"""Multi-node cluster behaviour (black-box, measured against OpenSearch 3.9).

Opt-in: OS_CLUSTER_URLS must list the http URLs of the main cluster nodes, in
order a1,a2,a3,a4 (a1,a2 zone dc1; a3,a4 zone dc2; a1..a3 cluster_manager
eligible, a4 data-only). Node-failure tests additionally need OS_CLUSTER_CTL, a
command prefix accepting `stop-node N`, `kill-node N`, `start-node N`:

    eval "$(SS_CLUSTER_ID=1 servers/opensearch-cluster.sh env)"
    OS_PORT=9511 .venv/bin/pytest tests/test_96_multinode.py -q
"""

import json
import os
import re
import subprocess
import threading
import time
import uuid

import httpx
import pytest

URLS = [u.strip().rstrip("/") for u in os.getenv("OS_CLUSTER_URLS", "").split(",") if u.strip()]
CTL = os.getenv("OS_CLUSTER_CTL")
TIMEOUT = float(os.getenv("OS_TIMEOUT", "60"))

pytestmark = [
    pytest.mark.multinode,
    pytest.mark.skipif(not URLS, reason="OS_CLUSTER_URLS is not set"),
]

PFX = f"ss-mn-{uuid.uuid4().hex[:8]}"
# expected topology by URL position
EXPECT_ELIGIBLE = [True, True, True, False]
EXPECT_ZONE = ["dc1", "dc1", "dc2", "dc2"]
FAST = {"index.unassigned.node_left.delayed_timeout": "0"}
TOUCHED_SETTINGS = [
    "cluster.routing.allocation.enable",
    "search.max_buckets",
    "action.auto_create_index",
    "indices.recovery.max_bytes_per_sec",
]


# ---------------------------------------------------------------- helpers


class Node:
    def __init__(self, pos, url):
        self.pos = pos
        self.url = url
        self.c = httpx.Client(base_url=url, timeout=TIMEOUT)
        r = self.c.get("/_nodes/_local")
        assert r.status_code == 200, r.text
        ((self.id, info),) = r.json()["nodes"].items()
        self.name = info["name"]
        self.roles = info["roles"]
        self.attrs = info["attributes"]

    def alive(self):
        try:
            return self.c.get("/", timeout=10).status_code == 200
        except httpx.HTTPError:
            return False

    def __repr__(self):
        return self.name


class Cluster:
    def __init__(self, urls):
        self.nodes = [Node(i, u) for i, u in enumerate(urls)]
        self.by_name = {n.name: n for n in self.nodes}
        self.by_id = {n.id: n for n in self.nodes}

    def live(self):
        return [n for n in self.nodes if n.alive()]

    def any(self, exclude=()):
        for n in self.live():
            if n.name not in exclude:
                return n
        raise AssertionError("no live node")

    def manager(self, via=None):
        via = via or self.any()
        r = via.c.get("/_cat/cluster_manager", params={"format": "json"})
        assert r.status_code == 200, r.text
        return self.by_id[r.json()[0]["id"]]

    def eligible(self):
        return [n for n in self.nodes if "cluster_manager" in n.roles]

    def data_only(self):
        return [n for n in self.nodes if "cluster_manager" not in n.roles]


_STOPPED = set()


def ctl(cmd, name):
    r = subprocess.run(f"{CTL} {cmd} {name}", shell=True, capture_output=True, text=True, timeout=300)
    assert r.returncode == 0, f"{cmd} {name}: {r.stdout} {r.stderr}"


def stop_node(node, mode):
    """mode: 'kill' (SIGKILL) or 'stop' (SIGTERM)."""
    _STOPPED.add(node.name)
    ctl(f"{mode}-node", node.name)


def start_node(node):
    # starting an already-starting node breaks the control script's pidfile
    # bookkeeping, so each stopped node is started exactly once and then awaited
    ctl("start-node", node.name)
    _STOPPED.discard(node.name)
    deadline = time.time() + 240
    while not node.alive():
        assert time.time() < deadline, f"{node.name} did not come back"
        time.sleep(1)


def health(c, index=None, **params):
    params.setdefault("timeout", "60s")
    path = f"/_cluster/health/{index}" if index else "/_cluster/health"
    r = c.get(path, params=params)
    assert r.status_code in (200, 408), r.text
    return r.json()


def wait_green(c, index=None, nodes=None, timeout="60s"):
    params = {
        "wait_for_status": "green",
        "wait_for_no_relocating_shards": "true",
        "wait_for_no_initializing_shards": "true",
        "timeout": timeout,
    }
    if nodes:
        params["wait_for_nodes"] = str(nodes)
    h = health(c, index, **params)
    assert not h["timed_out"], h
    return h


def settle(c, index, status="yellow", active=None):
    extra = {"wait_for_active_shards": str(active)} if active else {}
    h = health(
        c,
        index,
        wait_for_status=status,
        wait_for_no_initializing_shards="true",
        wait_for_no_relocating_shards="true",
        **extra,
    )
    assert not h["timed_out"], h
    return h


def voting(c):
    r = c.get("/_cluster/state/metadata", params={"filter_path": "metadata.cluster_coordination"})
    assert r.status_code == 200, r.text
    return r.json()["metadata"]["cluster_coordination"]


def restore_voting(cl):
    c = cl.any().c
    c.delete("/_cluster/voting_config_exclusions", params={"wait_for_removal": "false"})
    want = {n.id for n in cl.eligible()}
    deadline = time.time() + 60
    while time.time() < deadline:
        # any cluster-state update lets the reconfigurator grow the config back
        tmp = f"{PFX}-vote-{uuid.uuid4().hex[:6]}"
        c.put(f"/{tmp}", json={"settings": {"number_of_shards": 1, "number_of_replicas": 0}})
        c.delete(f"/{tmp}")
        v = voting(c)
        if set(v["last_committed_config"]) == want and not v["voting_config_exclusions"]:
            return
        time.sleep(1)
    raise AssertionError(f"voting config did not recover: {voting(c)}")


def ensure_healthy(cl):
    for name in list(_STOPPED):
        start_node(cl.by_name[name])
    deadline = time.time() + 240
    last = None
    while time.time() < deadline:
        try:
            c = cl.any().c
            last = health(
                c,
                wait_for_nodes=str(len(cl.nodes)),
                wait_for_status="green",
                wait_for_no_relocating_shards="true",
                wait_for_no_initializing_shards="true",
                timeout="20s",
            )
            if not last["timed_out"]:
                return
        except (httpx.HTTPError, AssertionError) as exc:
            last = str(exc)
        time.sleep(1)
    raise AssertionError(f"cluster did not become healthy: {last}")


def shards(c, index):
    r = c.get(
        f"/_cat/shards/{index}",
        params={"format": "json", "h": "index,shard,prirep,state,node,unassigned.reason"},
    )
    assert r.status_code == 200, r.text
    return r.json()


def primary_node(c, index, shard="0"):
    (p,) = [s for s in shards(c, index) if s["shard"] == shard and s["prirep"] == "p"]
    return p["node"]


def copy_nodes(c, index, shard="0"):
    return sorted(s["node"] for s in shards(c, index) if s["shard"] == shard and s["state"] == "STARTED")


def primary_terms(c, index):
    r = c.get(f"/_cluster/state/metadata/{index}")
    assert r.status_code == 200, r.text
    return r.json()["metadata"]["indices"][index]["primary_terms"]


def ndjson(lines):
    return ("\n".join(json.dumps(l) for l in lines) + "\n").encode()


def bulk(c, index, docs, refresh="true"):
    lines = []
    for doc_id, doc in docs.items():
        lines += [{"index": {"_index": index, "_id": doc_id}}, doc]
    r = c.post(
        "/_bulk",
        content=ndjson(lines),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": refresh},
    )
    assert r.status_code == 200 and not r.json()["errors"], r.text
    return r.json()


def explain(c, index, shard=0, primary=False):
    r = c.post("/_cluster/allocation/explain", json={"index": index, "shard": shard, "primary": primary})
    assert r.status_code == 200, r.text
    return r.json()


def deciders(expl):
    return {d["node_name"]: {x["decider"]: x["decision"] for x in d.get("deciders", [])} for d in expl["node_allocation_decisions"]}


def resp(r):
    try:
        body = r.json()
    except ValueError:
        body = r.text
    return r.status_code, body


def err_type(body):
    return body["error"]["type"]


def hit_nodes(c, index, **params):
    r = c.post(f"/{index}/_search", params=params, json={"size": 100, "explain": True, "_source": False})
    assert r.status_code == 200, r.text
    return {(h["_shard"], h["_node"]) for h in r.json()["hits"]["hits"]}


# ---------------------------------------------------------------- fixtures


@pytest.fixture(scope="session", autouse=True)
def wait_for_cluster():
    """Overrides conftest's fixture: wait on the first cluster URL, not OS_PORT."""
    deadline = time.time() + 60
    last = "no response"
    while time.time() < deadline:
        try:
            r = httpx.get(f"{URLS[0]}/_cluster/health", params={"wait_for_status": "yellow", "timeout": "5s"}, timeout=10)
            if r.status_code == 200:
                return r.json()
            last = r.text
        except httpx.HTTPError as exc:
            last = str(exc)
        time.sleep(2)
    pytest.fail(f"cluster not reachable at {URLS[0]}: {last}")


@pytest.fixture(scope="module")
def cl():
    deadline = time.time() + 180
    while True:
        try:
            cluster = Cluster(URLS)
            break
        except (httpx.HTTPError, AssertionError):
            if time.time() > deadline:
                raise
            time.sleep(2)
    ensure_healthy(cluster)
    yield cluster
    try:
        ensure_healthy(cluster)
    finally:
        c = cluster.any().c
        c.delete(f"/{PFX}-*", params={"expand_wildcards": "all"})
        c.put(
            "/_cluster/settings",
            json={"persistent": {k: None for k in TOUCHED_SETTINGS}, "transient": {k: None for k in TOUCHED_SETTINGS}},
        )
        restore_voting(cluster)
        for n in cluster.nodes:
            n.c.close()


@pytest.fixture
def mk(cl):
    """Create indices with a unique name; deleted after the test."""
    created = []

    def _mk(suffix, settings=None, mappings=None, params=None, expect=(200,)):
        name = f"{PFX}-{suffix}-{uuid.uuid4().hex[:4]}"
        body = {"settings": {**FAST, **(settings or {})}}
        if mappings:
            body["mappings"] = mappings
        r = cl.nodes[0].c.put(f"/{name}", json=body, params=params or {})
        assert r.status_code in expect, r.text
        created.append(name)
        return name

    yield _mk
    c = cl.any().c
    for name in created:
        c.delete(f"/{name}", params={"ignore_unavailable": "true"})


@pytest.fixture
def cluster_settings(cl):
    """Reset every touched cluster setting after the test, through a live node."""
    yield
    c = cl.any().c
    r = c.put(
        "/_cluster/settings",
        json={"persistent": {k: None for k in TOUCHED_SETTINGS}, "transient": {k: None for k in TOUCHED_SETTINGS}},
    )
    assert r.status_code == 200, r.text


NODE_POS = [pytest.param(i, id=f"via-n{i + 1}") for i in range(4)]


# ================================================================= topology


class TestTopology:
    def test_urls_are_distinct_nodes(self, cl):
        assert len(cl.nodes) == 4
        assert len({n.id for n in cl.nodes}) == 4
        assert len({n.name for n in cl.nodes}) == 4

    def test_same_cluster_name_and_uuid_everywhere(self, cl):
        roots = [n.c.get("/").json() for n in cl.nodes]
        assert len({r["cluster_name"] for r in roots}) == 1
        assert len({r["cluster_uuid"] for r in roots}) == 1
        assert roots[0]["cluster_uuid"] != "_na_"
        assert [r["name"] for r in roots] == [n.name for n in cl.nodes]

    @pytest.mark.parametrize("pos", NODE_POS)
    def test_every_node_sees_all_nodes(self, cl, pos):
        r = cl.nodes[pos].c.get("/_cat/nodes", params={"format": "json", "h": "name,id", "full_id": "true"})
        assert r.status_code == 200
        assert {(x["name"], x["id"]) for x in r.json()} == {(n.name, n.id) for n in cl.nodes}

    @pytest.mark.parametrize("pos", NODE_POS)
    def test_every_node_agrees_on_elected_manager(self, cl, pos):
        expected = cl.manager(cl.nodes[0])
        r = cl.nodes[pos].c.get("/_cat/cluster_manager", params={"format": "json"})
        assert r.status_code == 200
        (row,) = r.json()
        assert row["id"] == expected.id and row["node"] == expected.name
        st = cl.nodes[pos].c.get("/_cluster/state/cluster_manager_node,master_node").json()
        assert st["cluster_manager_node"] == expected.id
        assert st["master_node"] == expected.id

    def test_cat_master_alias_matches_cluster_manager(self, cl):
        c = cl.nodes[0].c
        a = c.get("/_cat/master", params={"format": "json"}).json()
        b = c.get("/_cat/cluster_manager", params={"format": "json"}).json()
        assert a == b

    def test_cat_nodes_cluster_manager_column(self, cl):
        r = cl.nodes[1].c.get("/_cat/nodes", params={"format": "json", "h": "name,cluster_manager,node.role"})
        rows = {x["name"]: x for x in r.json()}
        stars = [n for n, x in rows.items() if x["cluster_manager"] == "*"]
        assert stars == [cl.manager().name]
        assert all(x["cluster_manager"] in ("*", "-") for x in rows.values())

    def test_cat_nodes_role_letters(self, cl):
        r = cl.nodes[0].c.get("/_cat/nodes", params={"format": "json", "h": "name,node.role"})
        rows = {x["name"]: x["node.role"] for x in r.json()}
        for n in cl.nodes:
            assert "d" in rows[n.name] and "i" in rows[n.name]
            assert ("m" in rows[n.name]) == EXPECT_ELIGIBLE[n.pos]

    def test_nodes_api_roles(self, cl):
        for n in cl.nodes:
            assert "data" in n.roles and "ingest" in n.roles
            assert ("cluster_manager" in n.roles) == EXPECT_ELIGIBLE[n.pos]
            assert "master" not in n.roles

    def test_zone_attributes(self, cl):
        assert [n.attrs.get("zone") for n in cl.nodes] == EXPECT_ZONE
        r = cl.nodes[3].c.get("/_cat/nodeattrs", params={"format": "json", "h": "node,attr,value"})
        zones = {x["node"]: x["value"] for x in r.json() if x["attr"] == "zone"}
        assert zones == {n.name: EXPECT_ZONE[n.pos] for n in cl.nodes}

    def test_data_only_node_never_elected(self, cl):
        (d,) = cl.data_only()
        assert cl.manager().name != d.name
        for n in cl.nodes:
            assert cl.manager(n).id != d.id

    def test_elected_manager_is_eligible(self, cl):
        assert "cluster_manager" in cl.manager().roles

    @pytest.mark.parametrize("pos", NODE_POS)
    def test_health_from_every_node(self, cl, pos):
        h = health(cl.nodes[pos].c)
        assert h["number_of_nodes"] == 4
        assert h["number_of_data_nodes"] == 4
        assert h["discovered_cluster_manager"] is True
        assert h["discovered_master"] is True
        assert h["status"] == "green"

    def test_voting_config_is_exactly_the_eligible_nodes(self, cl):
        v = voting(cl.nodes[3].c)
        assert set(v["last_committed_config"]) == {n.id for n in cl.eligible()}
        assert v["last_accepted_config"] == v["last_committed_config"]
        assert v["voting_config_exclusions"] == []
        assert v["term"] >= 1

    @pytest.mark.parametrize(
        "spec,expected",
        [
            ("cluster_manager:true", {0, 1, 2}),
            # role:false only removes from the set built so far
            ("cluster_manager:false", set()),
            ("_all,cluster_manager:false", {3}),
            ("data:true,zone:dc1", {0, 1, 2, 3}),
            ("data:true", {0, 1, 2, 3}),
            ("zone:dc1", {0, 1}),
            ("zone:dc2", {2, 3}),
            ("_all", {0, 1, 2, 3}),
        ],
    )
    def test_node_selectors(self, cl, spec, expected):
        r = cl.nodes[2].c.get(f"/_nodes/{spec}/none")
        assert r.status_code == 200, r.text
        assert set(r.json()["nodes"]) == {cl.nodes[i].id for i in expected}
        assert r.json()["_nodes"]["total"] == len(expected)

    def test_node_selector_elected_manager_and_local(self, cl):
        r = cl.nodes[3].c.get("/_nodes/_cluster_manager/none")
        assert set(r.json()["nodes"]) == {cl.manager().id}
        for n in cl.nodes:
            assert set(n.c.get("/_nodes/_local/none").json()["nodes"]) == {n.id}

    def test_cat_health_node_counts(self, cl):
        (row,) = cl.nodes[3].c.get("/_cat/health", params={"format": "json"}).json()
        assert row["node.total"] == "4" and row["node.data"] == "4"
        assert row["cluster"] == cl.nodes[0].c.get("/").json()["cluster_name"]


# ================================================================= coordinating


class TestCoordinating:
    @pytest.mark.parametrize("pos", NODE_POS)
    def test_create_via_any_node_visible_everywhere_immediately(self, cl, mk, pos):
        name = f"{PFX}-cr{pos}-{uuid.uuid4().hex[:4]}"
        c = cl.nodes[pos].c
        try:
            r = c.put(
                f"/{name}",
                json={
                    "settings": {"number_of_shards": 2, "number_of_replicas": 1, **FAST},
                    "mappings": {"properties": {"title": {"type": "text"}, "tag": {"type": "keyword"}}},
                },
            )
            assert r.status_code == 200, r.text
            assert r.json() == {"acknowledged": True, "shards_acknowledged": True, "index": name}
            for n in cl.nodes:
                m = n.c.get(f"/{name}/_mapping")
                assert m.status_code == 200, (n, m.text)
                assert m.json()[name]["mappings"]["properties"]["tag"] == {"type": "keyword"}
                s = n.c.get(f"/{name}/_settings", params={"flat_settings": "true"}).json()[name]["settings"]
                assert s["index.number_of_shards"] == "2"
                assert s["index.number_of_replicas"] == "1"
        finally:
            c.delete(f"/{name}")

    def test_put_mapping_via_data_only_node_acked_everywhere(self, cl, mk):
        name = mk("pm", {"number_of_shards": 1, "number_of_replicas": 1})
        r = cl.nodes[3].c.put(f"/{name}/_mapping", json={"properties": {"price": {"type": "integer"}}})
        assert r.status_code == 200 and r.json() == {"acknowledged": True}
        for n in cl.nodes:
            props = n.c.get(f"/{name}/_mapping/field/price").json()[name]["mappings"]
            assert props["price"]["mapping"] == {"price": {"type": "integer"}}

    def test_settings_update_via_one_node_visible_everywhere(self, cl, mk):
        name = mk("su", {"number_of_shards": 1, "number_of_replicas": 0})
        r = cl.nodes[2].c.put(f"/{name}/_settings", json={"index": {"refresh_interval": "7s", "max_result_window": 777}})
        assert r.status_code == 200 and r.json() == {"acknowledged": True}
        for n in cl.nodes:
            s = n.c.get(f"/{name}/_settings", params={"flat_settings": "true"}).json()[name]["settings"]
            assert s["index.refresh_interval"] == "7s"
            assert s["index.max_result_window"] == "777"

    def test_alias_via_one_node_visible_everywhere(self, cl, mk):
        name = mk("al", {"number_of_shards": 1, "number_of_replicas": 0})
        alias = f"{name}-alias"
        r = cl.nodes[1].c.post("/_aliases", json={"actions": [{"add": {"index": name, "alias": alias}}]})
        assert r.status_code == 200
        for n in cl.nodes:
            assert n.c.get(f"/_alias/{alias}").json() == {name: {"aliases": {alias: {}}}}

    def test_delete_index_via_one_node_gone_everywhere(self, cl):
        name = f"{PFX}-del-{uuid.uuid4().hex[:4]}"
        c = cl.nodes[0].c
        assert c.put(f"/{name}", json={"settings": {"number_of_shards": 2, "number_of_replicas": 1}}).status_code == 200
        r = cl.nodes[3].c.delete(f"/{name}")
        assert r.status_code == 200 and r.json() == {"acknowledged": True}
        for n in cl.nodes:
            r = n.c.get(f"/{name}")
            assert r.status_code == 404
            assert err_type(r.json()) == "index_not_found_exception"

    @pytest.mark.parametrize("pos", NODE_POS)
    def test_realtime_get_from_every_node_without_refresh(self, cl, mk, pos):
        name = mk("rt", {"number_of_shards": 2, "number_of_replicas": 1, "refresh_interval": "-1"})
        wait_green(cl.nodes[0].c, name)
        r = cl.nodes[0].c.put(f"/{name}/_doc/k1", json={"v": 1})
        assert r.status_code == 201
        assert r.json()["_shards"] == {"total": 2, "successful": 2, "failed": 0}
        c = cl.nodes[pos].c
        # not refreshed: a search finds nothing, a realtime GET finds the doc
        assert c.get(f"/{name}/_count").json()["count"] == 0
        g = c.get(f"/{name}/_doc/k1")
        assert g.status_code == 200
        assert g.json()["found"] is True and g.json()["_source"] == {"v": 1}
        assert g.json()["_version"] == 1 and g.json()["_seq_no"] == 0 and g.json()["_primary_term"] == 1

    def test_realtime_false_get_not_visible_before_refresh(self, cl, mk):
        name = mk("rtf", {"number_of_shards": 1, "number_of_replicas": 1, "refresh_interval": "-1"})
        wait_green(cl.nodes[0].c, name)
        assert cl.nodes[0].c.put(f"/{name}/_doc/k1", json={"v": 1}).status_code == 201
        for n in cl.nodes:
            g = n.c.get(f"/{name}/_doc/k1", params={"realtime": "false"})
            assert g.status_code == 404
            assert g.json() == {"_index": name, "_id": "k1", "found": False}
        assert cl.nodes[3].c.post(f"/{name}/_refresh").json()["_shards"] == {"total": 2, "successful": 2, "failed": 0}
        for n in cl.nodes:
            assert n.c.get(f"/{name}/_doc/k1", params={"realtime": "false"}).json()["found"] is True

    def test_refresh_wait_for_then_search_on_other_node(self, cl, mk):
        name = mk("wf", {"number_of_shards": 3, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        r = cl.nodes[0].c.put(f"/{name}/_doc/a", json={"t": "x"}, params={"refresh": "wait_for"})
        assert r.status_code == 201
        assert "forced_refresh" not in r.json()
        for n in cl.nodes:
            assert n.c.get(f"/{name}/_count").json()["count"] == 1

    def test_refresh_true_reports_forced_refresh(self, cl, mk):
        name = mk("fr", {"number_of_shards": 1, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        r = cl.nodes[3].c.put(f"/{name}/_doc/a", json={"t": "x"}, params={"refresh": "true"})
        assert r.json()["forced_refresh"] is True
        for n in cl.nodes:
            assert n.c.get(f"/{name}/_count").json()["count"] == 1

    def test_search_results_identical_from_every_node(self, cl, mk):
        name = mk(
            "srch",
            {"number_of_shards": 4, "number_of_replicas": 1},
            {"properties": {"tag": {"type": "keyword"}, "n": {"type": "integer"}, "t": {"type": "text"}}},
        )
        wait_green(cl.nodes[0].c, name)
        docs = {str(i): {"tag": f"t{i % 5}", "n": i, "t": f"word{i % 3} common"} for i in range(60)}
        bulk(cl.nodes[1].c, name, docs)
        body = {
            "size": 100,
            "query": {"bool": {"must": [{"match": {"t": "common"}}], "filter": [{"range": {"n": {"gte": 5}}}]}},
            "sort": [{"n": "desc"}],
            "aggs": {"tags": {"terms": {"field": "tag"}, "aggs": {"s": {"sum": {"field": "n"}}}}, "st": {"stats": {"field": "n"}}},
        }
        results = []
        for n in cl.nodes:
            r = n.c.post(f"/{name}/_search", json=body)
            assert r.status_code == 200
            b = r.json()
            assert b["_shards"] == {"total": 4, "successful": 4, "skipped": 0, "failed": 0}
            results.append((b["hits"]["total"], [(h["_id"], h["sort"]) for h in b["hits"]["hits"]], b["aggregations"]))
        assert all(x == results[0] for x in results)
        assert results[0][0] == {"value": 55, "relation": "eq"}
        assert results[0][2]["st"]["count"] == 55

    def test_count_identical_from_every_node(self, cl, mk):
        name = mk("cnt", {"number_of_shards": 3, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        bulk(cl.nodes[2].c, name, {str(i): {"n": i} for i in range(37)})
        for n in cl.nodes:
            b = n.c.get(f"/{name}/_count").json()
            assert b == {"count": 37, "_shards": {"total": 3, "successful": 3, "skipped": 0, "failed": 0}}
            q = n.c.post(f"/{name}/_count", json={"query": {"range": {"n": {"lt": 10}}}}).json()
            assert q["count"] == 10

    def test_bulk_through_data_only_node_lands_on_other_nodes(self, cl, mk):
        name = mk("bulk", {"number_of_shards": 4, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        d = cl.data_only()[0]
        res = bulk(d.c, name, {str(i): {"n": i} for i in range(40)})
        assert len(res["items"]) == 40
        for it in res["items"]:
            assert it["index"]["status"] == 201
            assert it["index"]["_shards"] == {"total": 2, "successful": 2, "failed": 0}
        per_shard = {}
        for s in range(4):
            per_shard[s] = d.c.get(f"/{name}/_count", params={"preference": f"_shards:{s}"}).json()["count"]
        assert sum(per_shard.values()) == 40 and all(v > 0 for v in per_shard.values())
        prim_nodes = {s["node"] for s in shards(d.c, name) if s["prirep"] == "p"}
        assert len(prim_nodes) >= 2
        for n in cl.nodes:
            got = n.c.post(f"/{name}/_mget", json={"ids": [str(i) for i in range(40)]}).json()["docs"]
            assert [g["_source"]["n"] for g in got] == list(range(40))

    def test_update_delete_chain_across_nodes(self, cl, mk):
        name = mk("chain", {"number_of_shards": 2, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        a, b, c3, d = (n.c for n in cl.nodes)
        assert a.put(f"/{name}/_doc/x", json={"v": 1}).json()["_version"] == 1
        r = b.post(f"/{name}/_update/x", json={"doc": {"w": 2}})
        assert r.status_code == 200 and r.json()["_version"] == 2 and r.json()["result"] == "updated"
        g = c3.get(f"/{name}/_doc/x").json()
        assert g["_version"] == 2 and g["_source"] == {"v": 1, "w": 2}
        r = d.delete(f"/{name}/_doc/x")
        assert r.json()["result"] == "deleted" and r.json()["_version"] == 3
        for n in cl.nodes:
            g = n.c.get(f"/{name}/_doc/x")
            assert g.status_code == 404 and g.json()["found"] is False

    def test_write_response_shards_total_counts_unassigned_replicas(self, cl, mk):
        name = mk("wtot", {"number_of_shards": 1, "number_of_replicas": 4})
        settle(cl.nodes[0].c, name)
        r = cl.nodes[2].c.put(f"/{name}/_doc/1", json={"a": 1})
        assert r.status_code == 201
        sh = r.json()["_shards"]
        assert sh["total"] == 5 and sh["failed"] == 0
        assert 1 <= sh["successful"] <= 4


# ================================================================= allocation


class TestAllocation:
    def test_shards_with_replica_green_and_never_colocated(self, cl, mk):
        name = mk("place", {"number_of_shards": 4, "number_of_replicas": 1})
        h = wait_green(cl.nodes[0].c, name)
        assert h["active_primary_shards"] == 4 and h["active_shards"] == 8 and h["unassigned_shards"] == 0
        rows = shards(cl.nodes[0].c, name)
        assert len(rows) == 8
        for s in range(4):
            copies = [x for x in rows if x["shard"] == str(s)]
            assert sorted(x["prirep"] for x in copies) == ["p", "r"]
            assert copies[0]["node"] != copies[1]["node"]
        assert {x["node"] for x in rows} == {n.name for n in cl.nodes}

    def test_cat_shards_identical_from_every_node(self, cl, mk):
        name = mk("catsh", {"number_of_shards": 3, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        views = [sorted((x["shard"], x["prirep"], x["node"]) for x in shards(n.c, name)) for n in cl.nodes]
        assert all(v == views[0] for v in views)

    def test_search_shards_matches_cat_shards(self, cl, mk):
        name = mk("ss", {"number_of_shards": 3, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        cat = sorted((int(x["shard"]), x["prirep"] == "p", x["node"]) for x in shards(cl.nodes[0].c, name))
        for n in cl.nodes:
            b = n.c.get(f"/{name}/_search_shards").json()
            assert set(b["nodes"]) == {x.id for x in cl.nodes}
            assert b["indices"] == {name: {}}
            assert len(b["shards"]) == 3
            got = sorted((s["shard"], s["primary"], cl.by_id[s["node"]].name) for g in b["shards"] for s in g)
            assert got == cat
            for g in b["shards"]:
                assert len({s["shard"] for s in g}) == 1
                assert all(s["state"] == "STARTED" and s["index"] == name for s in g)

    def test_routing_table_identical_in_every_nodes_state(self, cl, mk):
        name = mk("rtab", {"number_of_shards": 2, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        tabs = [
            n.c.get(f"/_cluster/state/routing_table/{name}").json()["routing_table"]["indices"][name] for n in cl.nodes
        ]
        assert all(t == tabs[0] for t in tabs)

    def test_custom_routing_same_shard_everywhere(self, cl, mk):
        name = mk("route", {"number_of_shards": 4, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        r = cl.nodes[0].c.put(f"/{name}/_doc/d1", json={"u": "user7"}, params={"routing": "user7", "refresh": "true"})
        assert r.status_code == 201
        shard_groups = []
        for n in cl.nodes:
            b = n.c.get(f"/{name}/_search_shards", params={"routing": "user7"}).json()
            assert len(b["shards"]) == 1
            shard_groups.append(b["shards"][0][0]["shard"])
            g = n.c.get(f"/{name}/_doc/d1", params={"routing": "user7"}).json()
            assert g["found"] is True and g["_routing"] == "user7"
            s = n.c.post(f"/{name}/_search", params={"routing": "user7"}, json={"explain": True}).json()
            assert s["_shards"]["total"] == 1
            assert [h["_id"] for h in s["hits"]["hits"]] == ["d1"]
            assert s["hits"]["hits"][0]["_shard"] == f"[{name}][{shard_groups[-1]}]"
        assert len(set(shard_groups)) == 1

    def test_routing_required_mapping_rejected_without_routing_from_any_node(self, cl, mk):
        name = mk("rreq", {"number_of_shards": 2, "number_of_replicas": 1}, {"_routing": {"required": True}})
        for n in cl.nodes:
            r = n.c.put(f"/{name}/_doc/x", json={"a": 1})
            assert r.status_code == 400
            assert err_type(r.json()) == "routing_missing_exception"

    def test_preference_only_nodes_by_id_and_name(self, cl, mk):
        name = mk("pon", {"number_of_shards": 2, "auto_expand_replicas": "0-all"})
        wait_green(cl.nodes[0].c, name)
        bulk(cl.nodes[0].c, name, {str(i): {"n": i} for i in range(20)})
        for target in cl.nodes:
            for spec in (target.id, target.name):
                for coord in (cl.nodes[0], cl.nodes[3]):
                    got = hit_nodes(coord.c, name, preference=f"_only_nodes:{spec}")
                    assert {nid for _, nid in got} == {target.id}
                    assert len({s for s, _ in got}) == 2

    def test_preference_only_nodes_by_attribute(self, cl, mk):
        name = mk("poa", {"number_of_shards": 3, "auto_expand_replicas": "0-all"})
        wait_green(cl.nodes[0].c, name)
        bulk(cl.nodes[0].c, name, {str(i): {"n": i} for i in range(30)})
        dc2 = {n.id for n in cl.nodes if n.attrs["zone"] == "dc2"}
        for _ in range(5):
            got = hit_nodes(cl.nodes[0].c, name, preference="_only_nodes:zone:dc2")
            assert {nid for _, nid in got} <= dc2

    def test_preference_only_nodes_missing_copy_is_error(self, cl, mk):
        name = mk("pom", {"number_of_shards": 2, "number_of_replicas": 0, "index.routing.allocation.require._name": cl.nodes[0].name})
        wait_green(cl.nodes[0].c, name)
        other = cl.nodes[3]
        for n in cl.nodes:
            r = n.c.post(f"/{name}/_search", params={"preference": f"_only_nodes:{other.name}"}, json={})
            assert r.status_code == 400
            assert err_type(r.json()) == "illegal_argument_exception"
            assert re.fullmatch(
                rf"no data nodes with criteria \[{other.name}\] found for shard: \[{name}\]\[[01]\]",
                r.json()["error"]["reason"],
            )
        r = cl.nodes[0].c.post(f"/{name}/_search", params={"preference": "_only_nodes:nosuch"}, json={})
        assert r.status_code == 400
        assert "no data nodes with criteria [nosuch] found for shard" in r.json()["error"]["reason"]

    def test_preference_prefer_nodes(self, cl, mk):
        name = mk("ppn", {"number_of_shards": 2, "auto_expand_replicas": "0-all"})
        wait_green(cl.nodes[0].c, name)
        bulk(cl.nodes[0].c, name, {str(i): {"n": i} for i in range(20)})
        for target in cl.nodes:
            for coord in (cl.nodes[1], cl.nodes[3]):
                got = hit_nodes(coord.c, name, preference=f"_prefer_nodes:{target.id}")
                assert {nid for _, nid in got} == {target.id}
            # unlike _only_nodes, _prefer_nodes takes node ids only; a name is not an error
            r = cl.nodes[0].c.post(f"/{name}/_search", params={"preference": f"_prefer_nodes:{target.name}"}, json={})
            assert r.status_code == 200 and r.json()["hits"]["total"]["value"] == 20

    @pytest.mark.parametrize("pos", NODE_POS)
    def test_preference_local_uses_local_copies(self, cl, mk, pos):
        name = mk("plo", {"number_of_shards": 4, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        bulk(cl.nodes[0].c, name, {str(i): {"n": i} for i in range(60)})
        me = cl.nodes[pos]
        local_shards = {f"[{name}][{x['shard']}]" for x in shards(me.c, name) if x["node"] == me.name}
        assert local_shards
        for _ in range(3):
            got = hit_nodes(me.c, name, preference="_local")
            assert len({s for s, _ in got}) == 4
            for s, nid in got:
                if s in local_shards:
                    assert nid == me.id

    def test_search_shards_only_local(self, cl, mk):
        name = mk("pol", {"number_of_shards": 4, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        for n in cl.nodes:
            b = n.c.get(f"/{name}/_search_shards", params={"preference": "_only_local"}).json()
            assert len(b["shards"]) == 4
            local = {int(x["shard"]) for x in shards(n.c, name) if x["node"] == n.name}
            for g in b["shards"]:
                assert all(s["node"] == n.id for s in g)
            assert {g[0]["shard"] for g in b["shards"] if g} == local

    def test_search_shards_primary_and_replica_preferences(self, cl, mk):
        name = mk("ppr", {"number_of_shards": 3, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        c = cl.nodes[2].c
        p = c.get(f"/{name}/_search_shards", params={"preference": "_primary"}).json()["shards"]
        r = c.get(f"/{name}/_search_shards", params={"preference": "_replica"}).json()["shards"]
        assert [[s["primary"] for s in g] for g in p] == [[True]] * 3
        assert [[s["primary"] for s in g] for g in r] == [[False]] * 3
        s = c.get(f"/{name}/_search_shards", params={"preference": "_shards:0,2"}).json()["shards"]
        assert sorted(g[0]["shard"] for g in s) == [0, 2]

    def test_custom_preference_string_stable_across_coordinators(self, cl, mk):
        name = mk("pcs", {"number_of_shards": 4, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        bulk(cl.nodes[0].c, name, {str(i): {"n": i} for i in range(60)})
        for pref in ("session-abc", "user-42", "xyz"):
            seen = [hit_nodes(n.c, name, preference=pref) for n in cl.nodes for _ in range(2)]
            assert all(s == seen[0] for s in seen), pref
            assert len({s for s, _ in seen[0]}) == 4
            ss = [
                [(g[0]["shard"], g[0]["node"]) for g in n.c.get(f"/{name}/_search_shards", params={"preference": pref}).json()["shards"]]
                for n in cl.nodes
            ]
            assert all(x == ss[0] for x in ss)
            assert {(f"[{name}][{s}]", nid) for s, nid in ss[0]} == seen[0]

    def test_too_many_replicas_yellow_with_same_shard_decider(self, cl, mk):
        name = mk("yel", {"number_of_shards": 1, "number_of_replicas": 4})
        h = settle(cl.nodes[0].c, name, active=4)
        assert h["status"] == "yellow"
        assert h["active_primary_shards"] == 1 and h["active_shards"] == 4 and h["unassigned_shards"] == 1
        e = explain(cl.nodes[3].c, name)
        assert e["current_state"] == "unassigned"
        assert e["unassigned_info"]["reason"] == "INDEX_CREATED"
        assert e["can_allocate"] == "no"
        assert e["allocate_explanation"] == "cannot allocate because allocation is not permitted to any of the nodes"
        dec = deciders(e)
        assert set(dec) == {n.name for n in cl.nodes}
        assert all(d == {"same_shard": "NO"} for d in dec.values())
        rows = [x for x in shards(cl.nodes[0].c, name) if x["state"] == "UNASSIGNED"]
        assert [(x["prirep"], x["unassigned.reason"]) for x in rows] == [("r", "INDEX_CREATED")]

    def test_cluster_health_yellow_level_indices(self, cl, mk):
        name = mk("yel2", {"number_of_shards": 2, "number_of_replicas": 4})
        settle(cl.nodes[0].c, name, active=8)
        h = health(cl.nodes[1].c, name, level="shards")
        idx = h["indices"][name]
        assert idx["status"] == "yellow"
        assert idx["number_of_replicas"] == 4
        assert idx["unassigned_shards"] == 2
        assert {k: v["status"] for k, v in idx["shards"].items()} == {"0": "yellow", "1": "yellow"}

    def test_health_wait_for_green_times_out_408(self, cl, mk):
        name = mk("h408", {"number_of_shards": 1, "number_of_replicas": 4})
        settle(cl.nodes[0].c, name, active=4)
        r = cl.nodes[2].c.get(f"/_cluster/health/{name}", params={"wait_for_status": "green", "timeout": "500ms"})
        assert r.status_code == 408
        assert r.json()["timed_out"] is True and r.json()["status"] == "yellow"

    @pytest.mark.parametrize("spec,replicas", [("0-all", 3), ("0-1", 1), ("1-2", 2), ("0-5", 3)])
    def test_auto_expand_replicas(self, cl, mk, spec, replicas):
        name = mk("ae", {"number_of_shards": 1, "auto_expand_replicas": spec})
        wait_green(cl.nodes[0].c, name)
        s = cl.nodes[3].c.get(f"/{name}/_settings", params={"flat_settings": "true"}).json()[name]["settings"]
        assert s["index.number_of_replicas"] == str(replicas)
        assert s["index.auto_expand_replicas"] == spec
        assert len(copy_nodes(cl.nodes[0].c, name)) == replicas + 1

    def test_require_name(self, cl, mk):
        target = cl.nodes[1]
        name = mk("reqn", {"number_of_shards": 3, "number_of_replicas": 0, "index.routing.allocation.require._name": target.name})
        wait_green(cl.nodes[0].c, name)
        assert {x["node"] for x in shards(cl.nodes[0].c, name)} == {target.name}

    def test_include_name_with_replica(self, cl, mk):
        a, b = cl.nodes[0], cl.nodes[3]
        name = mk("incn", {"number_of_shards": 2, "number_of_replicas": 1, "index.routing.allocation.include._name": f"{a.name},{b.name}"})
        wait_green(cl.nodes[0].c, name)
        for s in ("0", "1"):
            assert copy_nodes(cl.nodes[0].c, name, s) == sorted([a.name, b.name])

    def test_include_single_node_with_replica_is_yellow(self, cl, mk):
        a = cl.nodes[2]
        name = mk("inc1", {"number_of_shards": 1, "number_of_replicas": 1, "index.routing.allocation.include._name": a.name})
        h = settle(cl.nodes[0].c, name)
        assert h["status"] == "yellow"
        dec = deciders(explain(cl.nodes[0].c, name))
        assert dec[a.name] == {"same_shard": "NO"}
        for n in cl.nodes:
            if n is not a:
                assert dec[n.name] == {"filter": "NO"}

    def test_exclude_name(self, cl, mk):
        ex = cl.nodes[0]
        name = mk("exn", {"number_of_shards": 4, "number_of_replicas": 1, "index.routing.allocation.exclude._name": ex.name})
        wait_green(cl.nodes[0].c, name)
        nodes = {x["node"] for x in shards(cl.nodes[0].c, name)}
        assert ex.name not in nodes and len(nodes) == 3

    @pytest.mark.parametrize("zone", ["dc1", "dc2"])
    def test_require_zone_attribute(self, cl, mk, zone):
        name = mk("reqz", {"number_of_shards": 2, "number_of_replicas": 1, "index.routing.allocation.require.zone": zone})
        wait_green(cl.nodes[0].c, name)
        assert {x["node"] for x in shards(cl.nodes[0].c, name)} == {n.name for n in cl.nodes if n.attrs["zone"] == zone}

    def test_exclude_zone_attribute(self, cl, mk):
        name = mk("exz", {"number_of_shards": 3, "number_of_replicas": 0, "index.routing.allocation.exclude.zone": "dc1"})
        wait_green(cl.nodes[0].c, name)
        assert {x["node"] for x in shards(cl.nodes[0].c, name)} <= {n.name for n in cl.nodes if n.attrs["zone"] == "dc2"}

    def test_dynamic_exclude_relocates_shards_away(self, cl, mk):
        name = mk("dyn", {"number_of_shards": 4, "number_of_replicas": 0})
        c = cl.nodes[0].c
        wait_green(c, name)
        bulk(c, name, {str(i): {"n": i} for i in range(30)})
        victim = shards(c, name)[0]["node"]
        r = cl.nodes[2].c.put(f"/{name}/_settings", json={"index.routing.allocation.exclude._name": victim})
        assert r.status_code == 200
        wait_green(c, name)
        assert victim not in {x["node"] for x in shards(c, name)}
        for n in cl.nodes:
            assert n.c.get(f"/{name}/_count").json()["count"] == 30

    def test_unsatisfiable_require_is_red(self, cl, mk):
        name = mk("red", {"number_of_shards": 1, "number_of_replicas": 0, "index.routing.allocation.require._name": "nosuch-node"}, params={"timeout": "1s"})
        h = health(cl.nodes[0].c, name, timeout="1s")
        assert h["status"] == "red"
        assert h["active_primary_shards"] == 0 and h["unassigned_shards"] == 1
        e = explain(cl.nodes[1].c, name, primary=True)
        assert e["can_allocate"] == "no"
        assert e["unassigned_info"]["reason"] == "INDEX_CREATED"
        for d in e["node_allocation_decisions"]:
            (dd,) = d["deciders"]
            assert dd["decider"] == "filter" and dd["decision"] == "NO"
            assert dd["explanation"] == 'node does not match index setting [index.routing.allocation.require] filters [_name:"nosuch-node"]'

    def test_create_on_unsatisfiable_index_not_shards_acknowledged(self, cl, mk):
        name = mk("redack", {"number_of_shards": 1, "number_of_replicas": 0, "index.routing.allocation.require._name": "nosuch-node"}, params={"timeout": "1s"})
        # mk asserted 200; recreate a sibling to inspect the body
        sib = f"{name}-b"
        try:
            r = cl.nodes[0].c.put(
                f"/{sib}",
                json={"settings": {"number_of_shards": 1, "number_of_replicas": 0, "index.routing.allocation.require._name": "nosuch-node"}},
                params={"timeout": "1s"},
            )
            assert r.status_code == 200
            assert r.json() == {"acknowledged": True, "shards_acknowledged": False, "index": sib}
        finally:
            cl.nodes[0].c.delete(f"/{sib}")

    def test_total_shards_per_node_leaves_replicas_unassigned(self, cl, mk):
        name = mk("tspn", {"number_of_shards": 4, "number_of_replicas": 1, "index.routing.allocation.total_shards_per_node": 1})
        h = settle(cl.nodes[0].c, name, active=4)
        assert h["status"] == "yellow"
        assert h["active_primary_shards"] == 4 and h["unassigned_shards"] == 4
        rows = shards(cl.nodes[0].c, name)
        assert sorted(x["node"] for x in rows if x["prirep"] == "p") == sorted(n.name for n in cl.nodes)
        assert all(x["state"] == "UNASSIGNED" for x in rows if x["prirep"] == "r")
        dec = deciders(explain(cl.nodes[0].c, name))
        assert all(d.get("shards_limit") == "NO" for d in dec.values())

    def test_total_shards_per_node_two_is_green(self, cl, mk):
        name = mk("tspn2", {"number_of_shards": 4, "number_of_replicas": 1, "index.routing.allocation.total_shards_per_node": 2})
        wait_green(cl.nodes[0].c, name)
        per = {}
        for x in shards(cl.nodes[0].c, name):
            per[x["node"]] = per.get(x["node"], 0) + 1
        assert per == {n.name: 2 for n in cl.nodes}

    def test_allocation_enable_none_then_primaries_then_all(self, cl, mk, cluster_settings):
        c = cl.nodes[0].c
        r = cl.nodes[1].c.put("/_cluster/settings", json={"persistent": {"cluster.routing.allocation.enable": "none"}})
        assert r.status_code == 200
        assert r.json() == {
            "acknowledged": True,
            "persistent": {"cluster": {"routing": {"allocation": {"enable": "none"}}}},
            "transient": {},
        }
        name = mk("en", {"number_of_shards": 2, "number_of_replicas": 1}, params={"timeout": "1s"})
        h = health(c, name, timeout="1s")
        assert h["status"] == "red" and h["unassigned_shards"] == 4 and h["active_shards"] == 0
        e = explain(c, name, primary=True)
        for d in e["node_allocation_decisions"]:
            assert d["deciders"] == [
                {
                    "decider": "enable",
                    "decision": "NO",
                    "explanation": "no allocations are allowed due to cluster setting [cluster.routing.allocation.enable=none]",
                }
            ]
        assert cl.nodes[2].c.put("/_cluster/settings", json={"persistent": {"cluster.routing.allocation.enable": "primaries"}}).status_code == 200
        h = health(c, name, wait_for_active_shards="2", wait_for_no_initializing_shards="true")
        assert h["status"] == "yellow" and h["active_primary_shards"] == 2 and h["unassigned_shards"] == 2
        rows = shards(c, name)
        assert {(x["prirep"], x["state"]) for x in rows} == {("p", "STARTED"), ("r", "UNASSIGNED")}
        assert cl.nodes[3].c.put("/_cluster/settings", json={"persistent": {"cluster.routing.allocation.enable": None}}).status_code == 200
        wait_green(c, name)

    def test_reroute_move(self, cl, mk):
        c = cl.nodes[3].c
        name = mk("mv", {"number_of_shards": 1, "number_of_replicas": 0})
        wait_green(c, name)
        bulk(c, name, {str(i): {"n": i} for i in range(10)})
        src = primary_node(c, name)
        dst = next(n.name for n in cl.nodes if n.name != src)
        cmd = {"commands": [{"move": {"index": name, "shard": 0, "from_node": src, "to_node": dst}}]}
        dry = c.post("/_cluster/reroute", params={"dry_run": "true", "explain": "true", "metric": "none"}, json=cmd)
        assert dry.status_code == 200
        (ex,) = dry.json()["explanations"]
        assert ex["command"] == "move"
        assert ex["parameters"] == {"index": name, "shard": 0, "from_node": src, "to_node": dst}
        decs = {d["decider"]: d["decision"] for d in ex["decisions"]}
        assert decs["same_shard"] == "YES" and decs["enable"] == "YES"
        assert primary_node(c, name) == src
        r = c.post("/_cluster/reroute", json=cmd)
        assert r.status_code == 200 and r.json()["acknowledged"] is True
        (copy,) = r.json()["state"]["routing_table"]["indices"][name]["shards"]["0"]
        assert copy["state"] == "RELOCATING"
        assert copy["node"] == cl.by_name[src].id and copy["relocating_node"] == cl.by_name[dst].id
        wait_green(c, name)
        assert primary_node(c, name) == dst
        for n in cl.nodes:
            assert n.c.get(f"/{name}/_count").json()["count"] == 10

    def test_reroute_metric_none_body(self, cl, mk):
        c = cl.nodes[0].c
        name = mk("mvn", {"number_of_shards": 1, "number_of_replicas": 0})
        wait_green(c, name)
        r = c.post("/_cluster/reroute", params={"metric": "none"}, json={"commands": []})
        uuid_ = c.get("/").json()["cluster_uuid"]
        assert r.status_code == 200 and r.json() == {"acknowledged": True, "state": {"cluster_uuid": uuid_}}

    def test_reroute_move_errors(self, cl, mk):
        c = cl.nodes[1].c
        name = mk("mve", {"number_of_shards": 1, "number_of_replicas": 0})
        wait_green(c, name)
        src = primary_node(c, name)
        other = next(n.name for n in cl.nodes if n.name != src)
        third = next(n.name for n in cl.nodes if n.name not in (src, other))
        r = c.post("/_cluster/reroute", json={"commands": [{"move": {"index": name, "shard": 0, "from_node": other, "to_node": third}}]})
        assert r.status_code == 400
        assert err_type(r.json()) == "illegal_argument_exception"
        assert r.json()["error"]["reason"].startswith(f"[move_allocation] can't move 0, failed to find it on node {{{other}}}")
        r = c.post("/_cluster/reroute", json={"commands": [{"move": {"index": name, "shard": 0, "from_node": src, "to_node": "nope"}}]})
        assert r.status_code == 400
        assert r.json()["error"]["reason"] == "failed to resolve [nope], no matching nodes"

    def test_reroute_cancel_started_primary_needs_allow_primary(self, cl, mk):
        c = cl.nodes[0].c
        name = mk("cp", {"number_of_shards": 1, "number_of_replicas": 0})
        wait_green(c, name)
        src = primary_node(c, name)
        r = c.post("/_cluster/reroute", json={"commands": [{"cancel": {"index": name, "shard": 0, "node": src}}]})
        assert r.status_code == 400
        reason = r.json()["error"]["reason"]
        assert reason.startswith(f"[cancel_allocation] can't cancel 0 on node {{{src}}}")
        assert reason.endswith("shard is primary and started")

    def test_reroute_cancel_replica_reason(self, cl, mk, cluster_settings):
        c = cl.nodes[0].c
        name = mk("cr", {"number_of_shards": 1, "number_of_replicas": 1})
        wait_green(c, name)
        assert c.put("/_cluster/settings", json={"persistent": {"cluster.routing.allocation.enable": "primaries"}}).status_code == 200
        rn = [x["node"] for x in shards(c, name) if x["prirep"] == "r"][0]
        r = cl.nodes[2].c.post("/_cluster/reroute", params={"metric": "none"}, json={"commands": [{"cancel": {"index": name, "shard": 0, "node": rn}}]})
        assert r.status_code == 200
        rows = shards(c, name)
        assert [(x["prirep"], x["state"], x["unassigned.reason"]) for x in rows if x["prirep"] == "r"] == [("r", "UNASSIGNED", "REROUTE_CANCELLED")]
        assert c.put("/_cluster/settings", json={"persistent": {"cluster.routing.allocation.enable": None}}).status_code == 200
        wait_green(c, name)

    def test_reroute_allocate_replica(self, cl, mk, cluster_settings):
        c = cl.nodes[0].c
        name = mk("ar", {"number_of_shards": 1, "number_of_replicas": 0})
        wait_green(c, name)
        assert c.put("/_cluster/settings", json={"persistent": {"cluster.routing.allocation.enable": "primaries"}}).status_code == 200
        assert c.put(f"/{name}/_settings", json={"number_of_replicas": 1}).status_code == 200
        rows = shards(c, name)
        assert [(x["prirep"], x["unassigned.reason"]) for x in rows if x["state"] == "UNASSIGNED"] == [("r", "REPLICA_ADDED")]
        pn = primary_node(c, name)
        tn = next(n.name for n in cl.nodes if n.name != pn)
        r = c.post("/_cluster/reroute", json={"commands": [{"allocate_replica": {"index": name, "shard": 0, "node": pn}}]})
        assert r.status_code == 400
        assert r.json()["error"]["reason"].startswith(f"[allocate_replica] allocation of [{name}][0] on node {{{pn}}}")
        assert "is not allowed" in r.json()["error"]["reason"]
        r = cl.nodes[3].c.post("/_cluster/reroute", params={"metric": "none"}, json={"commands": [{"allocate_replica": {"index": name, "shard": 0, "node": tn}}]})
        assert r.status_code == 200
        wait_green(c, name)
        assert copy_nodes(c, name) == sorted([pn, tn])

    def test_wait_for_active_shards_all_on_create_times_out_softly(self, cl, mk):
        name = f"{PFX}-wfa-{uuid.uuid4().hex[:4]}"
        c = cl.nodes[1].c
        try:
            r = c.put(
                f"/{name}",
                json={"settings": {"number_of_shards": 1, "number_of_replicas": 4}},
                params={"wait_for_active_shards": "all", "timeout": "1s"},
            )
            assert r.status_code == 200
            assert r.json() == {"acknowledged": True, "shards_acknowledged": False, "index": name}
        finally:
            c.delete(f"/{name}")

    def test_wait_for_active_shards_all_write_fails_on_unsatisfiable(self, cl, mk):
        name = mk("wfw", {"number_of_shards": 1, "number_of_replicas": 4})
        settle(cl.nodes[0].c, name, active=4)
        for n in (cl.nodes[0], cl.nodes[3]):
            r = n.c.put(f"/{name}/_doc/1", json={"a": 1}, params={"wait_for_active_shards": "all", "timeout": "1s"})
            assert r.status_code == 503
            b = r.json()
            assert err_type(b) == "unavailable_shards_exception"
            assert b["error"]["reason"] == f"[{name}][0] Not enough active copies to meet shard count of [ALL] (have 4, needed 5). Timeout: [1s]"
            assert b["status"] == 503
        r = cl.nodes[2].c.put(f"/{name}/_doc/1", json={"a": 1}, params={"wait_for_active_shards": "6", "timeout": "1s"})
        assert r.status_code == 503
        assert "shard count of [6] (have 4, needed 6)" in r.json()["error"]["reason"]
        r = cl.nodes[2].c.put(f"/{name}/_doc/1", json={"a": 1}, params={"wait_for_active_shards": "4", "timeout": "1s"})
        assert r.status_code == 201

    def test_index_level_write_wait_for_active_shards(self, cl, mk):
        name = mk("wfs", {"number_of_shards": 1, "number_of_replicas": 4, "index.write.wait_for_active_shards": "all"})
        settle(cl.nodes[0].c, name, active=4)
        r = cl.nodes[1].c.put(f"/{name}/_doc/1", json={"a": 1}, params={"timeout": "1s"})
        assert r.status_code == 503
        assert "Not enough active copies to meet shard count of [ALL] (have 4, needed 5)" in r.json()["error"]["reason"]

    def test_wait_for_active_shards_all_success_when_green(self, cl, mk):
        name = mk("wfg", {"number_of_shards": 1, "auto_expand_replicas": "0-all"})
        wait_green(cl.nodes[0].c, name)
        r = cl.nodes[3].c.put(f"/{name}/_doc/1", json={"a": 1}, params={"wait_for_active_shards": "all"})
        assert r.status_code == 201
        assert r.json()["_shards"] == {"total": 4, "successful": 4, "failed": 0}


# ================================================================= voting config


class TestVotingExclusions:
    @pytest.fixture(autouse=True)
    def _restore(self, cl):
        yield
        restore_voting(cl)

    def test_exclude_non_elected_eligible_node(self, cl):
        mgr = cl.manager()
        ex = next(n for n in cl.eligible() if n.id != mgr.id)
        c = cl.nodes[3].c
        r = c.post("/_cluster/voting_config_exclusions", params={"node_names": ex.name})
        assert r.status_code == 200 and r.text == ""
        for n in cl.nodes:
            v = voting(n.c)
            assert v["voting_config_exclusions"] == [{"node_id": ex.id, "node_name": ex.name}]
            assert ex.id not in v["last_committed_config"]
        # still a cluster member, manager unchanged
        assert health(c)["number_of_nodes"] == 4
        assert cl.manager().id == mgr.id

    def test_clear_exclusions_waits_for_removal(self, cl):
        mgr = cl.manager()
        ex = next(n for n in cl.eligible() if n.id != mgr.id)
        c = cl.nodes[0].c
        assert c.post("/_cluster/voting_config_exclusions", params={"node_names": ex.name}).status_code == 200
        r = c.delete("/_cluster/voting_config_exclusions", params={"timeout": "1s"})
        assert r.status_code == 400
        assert r.json()["error"]["reason"] == "request [/_cluster/voting_config_exclusions] contains unrecognized parameter: [timeout]"
        # the default wait is 30s
        r = c.delete("/_cluster/voting_config_exclusions")
        assert r.status_code == 504
        assert err_type(r.json()) == "timeout_exception"
        assert r.json()["error"]["reason"].startswith("timed out waiting for removal of nodes; if nodes should not be removed, set waitForRemoval to false.")
        assert voting(c)["voting_config_exclusions"] != []
        r = c.delete("/_cluster/voting_config_exclusions", params={"wait_for_removal": "false"})
        assert r.status_code == 200
        assert voting(c)["voting_config_exclusions"] == []

    def test_exclude_data_only_node_is_noop(self, cl):
        (d,) = cl.data_only()
        c = cl.nodes[1].c
        r = c.post("/_cluster/voting_config_exclusions", params={"node_names": d.name})
        assert r.status_code == 200
        v = voting(c)
        assert v["voting_config_exclusions"] == []
        assert set(v["last_committed_config"]) == {n.id for n in cl.eligible()}

    def test_exclude_unknown_node_records_absent(self, cl):
        c = cl.nodes[0].c
        r = c.post("/_cluster/voting_config_exclusions", params={"node_names": "ghost-node", "timeout": "2s"})
        assert r.status_code == 200
        assert voting(c)["voting_config_exclusions"] == [{"node_id": "_absent_", "node_name": "ghost-node"}]

    def test_exclude_by_node_id(self, cl):
        mgr = cl.manager()
        ex = next(n for n in cl.eligible() if n.id != mgr.id)
        c = cl.nodes[2].c
        assert c.post("/_cluster/voting_config_exclusions", params={"node_ids": ex.id}).status_code == 200
        assert voting(c)["voting_config_exclusions"] == [{"node_id": ex.id, "node_name": ex.name}]

    @pytest.mark.parametrize(
        "params",
        [{}, {"node_names": "a1", "node_ids": "x"}],
        ids=["none", "both"],
    )
    def test_exclusion_identifier_validation(self, cl, params):
        r = cl.nodes[0].c.post("/_cluster/voting_config_exclusions", params=params)
        assert r.status_code == 400
        assert err_type(r.json()) == "illegal_argument_exception"
        assert r.json()["error"]["reason"] == (
            "Please set node identifiers correctly. One and only one of [node_name], [node_names] and [node_ids] has to be set"
        )

    def test_excluding_elected_manager_triggers_reelection(self, cl):
        mgr = cl.manager()
        term = voting(mgr.c)["term"]
        other = next(n for n in cl.nodes if n.id != mgr.id)
        r = other.c.post("/_cluster/voting_config_exclusions", params={"node_names": mgr.name})
        assert r.status_code == 200
        deadline = time.time() + 30
        while cl.manager(other).id == mgr.id and time.time() < deadline:
            time.sleep(0.5)
        new = cl.manager(other)
        assert new.id != mgr.id
        assert "cluster_manager" in new.roles
        for n in cl.nodes:
            assert cl.manager(n).id == new.id
        v = voting(other.c)
        assert v["term"] > term
        assert mgr.id not in v["last_committed_config"]
        assert health(other.c)["number_of_nodes"] == 4


# ================================================================= cluster settings


class TestClusterSettings:
    def test_persistent_and_transient_visible_from_all_nodes(self, cl, cluster_settings):
        r = cl.nodes[2].c.put(
            "/_cluster/settings",
            json={"persistent": {"search.max_buckets": 12345}, "transient": {"indices.recovery.max_bytes_per_sec": "77mb"}},
        )
        assert r.status_code == 200
        assert r.json()["persistent"] == {"search": {"max_buckets": "12345"}}
        assert r.json()["transient"] == {"indices": {"recovery": {"max_bytes_per_sec": "77mb"}}}
        for n in cl.nodes:
            s = n.c.get("/_cluster/settings", params={"flat_settings": "true"}).json()
            assert s["persistent"].get("search.max_buckets") == "12345"
            assert s["transient"].get("indices.recovery.max_bytes_per_sec") == "77mb"
        r = cl.nodes[3].c.put(
            "/_cluster/settings",
            json={"persistent": {"search.max_buckets": None}, "transient": {"indices.recovery.max_bytes_per_sec": None}},
        )
        assert r.json() == {"acknowledged": True, "persistent": {}, "transient": {}}
        for n in cl.nodes:
            s = n.c.get("/_cluster/settings", params={"flat_settings": "true"}).json()
            assert "search.max_buckets" not in s["persistent"]
            assert "indices.recovery.max_bytes_per_sec" not in s["transient"]

    def test_max_buckets_set_via_one_node_enforced_on_another(self, cl, mk, cluster_settings):
        name = mk("mb", {"number_of_shards": 2, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        bulk(cl.nodes[0].c, name, {str(i): {"k": f"v{i}"} for i in range(20)})
        body = {"size": 0, "aggs": {"t": {"terms": {"field": "k.keyword", "size": 20}}}}
        assert cl.nodes[3].c.post(f"/{name}/_search", json=body).status_code == 200
        assert cl.nodes[0].c.put("/_cluster/settings", json={"persistent": {"search.max_buckets": 5}}).status_code == 200
        for n in cl.nodes[1:]:
            r = n.c.post(f"/{name}/_search", json=body)
            assert r.status_code >= 400
            assert "too_many_buckets_exception" in r.text

    def test_auto_create_index_disabled_via_one_node(self, cl, cluster_settings):
        c = cl.nodes[1].c
        assert c.put("/_cluster/settings", json={"persistent": {"action.auto_create_index": "false"}}).status_code == 200
        name = f"{PFX}-auto-{uuid.uuid4().hex[:4]}"
        for n in cl.nodes:
            r = n.c.put(f"/{name}/_doc/1", json={"a": 1})
            assert r.status_code == 404
            assert err_type(r.json()) == "index_not_found_exception"
            assert r.json()["error"]["reason"] == f"no such index [{name}]"
            assert r.json()["error"]["index"] == name

    def test_unknown_setting_rejected(self, cl):
        for n in (cl.nodes[0], cl.nodes[3]):
            r = n.c.put("/_cluster/settings", json={"persistent": {"cluster.no_such_thing": 1}})
            assert r.status_code == 400
            assert err_type(r.json()) == "settings_exception"
            assert r.json()["error"]["reason"] == "persistent setting [cluster.no_such_thing], not recognized"


# ================================================================= concurrency


def _parallel(fns):
    out = [None] * len(fns)

    def run(i, f):
        out[i] = f()

    ts = [threading.Thread(target=run, args=(i, f)) for i, f in enumerate(fns)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    return out


class TestConcurrency:
    def test_concurrent_scripted_increments_via_all_nodes(self, cl, mk):
        name = mk("inc", {"number_of_shards": 2, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        assert cl.nodes[0].c.put(f"/{name}/_doc/c", json={"n": 0}).status_code == 201

        def worker(node):
            def f():
                codes = []
                with httpx.Client(base_url=node.url, timeout=TIMEOUT) as c:
                    for _ in range(10):
                        r = c.post(
                            f"/{name}/_update/c",
                            params={"retry_on_conflict": "50"},
                            json={"script": {"source": "ctx._source.n += 1", "lang": "painless"}},
                        )
                        codes.append(r.status_code)
                return codes

            return f

        res = _parallel([worker(n) for n in cl.nodes])
        assert all(code == 200 for codes in res for code in codes)
        for n in cl.nodes:
            g = n.c.get(f"/{name}/_doc/c").json()
            assert g["_source"]["n"] == 40 and g["_version"] == 41

    def test_concurrent_index_same_id_unique_versions(self, cl, mk):
        name = mk("ver", {"number_of_shards": 1, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)

        def worker(node):
            def f():
                with httpx.Client(base_url=node.url, timeout=TIMEOUT) as c:
                    return [c.put(f"/{name}/_doc/d", json={"by": node.name, "i": i}).json() for i in range(5)]

            return f

        res = [x for xs in _parallel([worker(n) for n in cl.nodes]) for x in xs]
        assert sorted(x["_version"] for x in res) == list(range(1, 21))
        assert sorted(x["_seq_no"] for x in res) == list(range(20))
        assert sum(x["result"] == "created" for x in res) == 1
        finals = [n.c.get(f"/{name}/_doc/d").json() for n in cl.nodes]
        assert all(f["_version"] == 20 and f["_seq_no"] == 19 for f in finals)
        assert len({json.dumps(f["_source"], sort_keys=True) for f in finals}) == 1

    def test_versions_monotonic_when_read_from_other_nodes(self, cl, mk):
        name = mk("mono", {"number_of_shards": 1, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        last = 0
        for i in range(12):
            w = cl.nodes[i % 4].c.put(f"/{name}/_doc/m", json={"i": i}).json()
            g = cl.nodes[(i + 1) % 4].c.get(f"/{name}/_doc/m").json()
            assert w["_version"] == g["_version"] == last + 1
            assert g["_source"] == {"i": i}
            last = g["_version"]

    def test_concurrent_create_same_id_exactly_one_wins(self, cl, mk):
        name = mk("cre", {"number_of_shards": 2, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)

        def worker(node):
            def f():
                with httpx.Client(base_url=node.url, timeout=TIMEOUT) as c:
                    return resp(c.put(f"/{name}/_create/one", json={"by": node.name}))

            return f

        res = _parallel([worker(n) for n in cl.nodes])
        codes = sorted(s for s, _ in res)
        assert codes == [201, 409, 409, 409]
        for s, b in res:
            if s == 409:
                assert err_type(b) == "version_conflict_engine_exception"
                assert b["error"]["reason"] == "[one]: version conflict, document already exists (current version [1])"

    def test_if_seq_no_conflict_detected_across_nodes(self, cl, mk):
        name = mk("cas", {"number_of_shards": 1, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        w = cl.nodes[0].c.put(f"/{name}/_doc/x", json={"v": 0}).json()
        seq, term = w["_seq_no"], w["_primary_term"]
        g = cl.nodes[3].c.get(f"/{name}/_doc/x").json()
        assert (g["_seq_no"], g["_primary_term"]) == (seq, term)
        r = cl.nodes[1].c.put(f"/{name}/_doc/x", params={"if_seq_no": seq, "if_primary_term": term}, json={"v": 1})
        assert r.status_code == 200 and r.json()["_seq_no"] == seq + 1
        r = cl.nodes[2].c.put(f"/{name}/_doc/x", params={"if_seq_no": seq, "if_primary_term": term}, json={"v": 2})
        assert r.status_code == 409
        assert err_type(r.json()) == "version_conflict_engine_exception"
        assert r.json()["error"]["reason"] == (
            f"[x]: version conflict, required seqNo [{seq}], primary term [{term}]. "
            f"current document has seqNo [{seq + 1}] and primary term [{term}]"
        )
        r = cl.nodes[3].c.delete(f"/{name}/_doc/x", params={"if_seq_no": seq, "if_primary_term": term})
        assert r.status_code == 409

    def test_concurrent_cas_exactly_one_wins(self, cl, mk):
        name = mk("cas2", {"number_of_shards": 1, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        w = cl.nodes[0].c.put(f"/{name}/_doc/x", json={"v": 0}).json()

        def worker(node):
            def f():
                with httpx.Client(base_url=node.url, timeout=TIMEOUT) as c:
                    return c.put(
                        f"/{name}/_doc/x",
                        params={"if_seq_no": w["_seq_no"], "if_primary_term": w["_primary_term"]},
                        json={"by": node.name},
                    ).status_code

            return f

        assert sorted(_parallel([worker(n) for n in cl.nodes])) == [200, 409, 409, 409]

    def test_external_version_conflict_across_nodes(self, cl, mk):
        name = mk("ext", {"number_of_shards": 1, "number_of_replicas": 1})
        wait_green(cl.nodes[0].c, name)
        ext = {"version_type": "external"}
        assert cl.nodes[0].c.put(f"/{name}/_doc/e", params={**ext, "version": 5}, json={"v": 5}).status_code == 201
        r = cl.nodes[1].c.put(f"/{name}/_doc/e", params={**ext, "version": 3}, json={"v": 3})
        assert r.status_code == 409
        assert r.json()["error"]["reason"] == "[e]: version conflict, current version [5] is higher or equal to the one provided [3]"
        r = cl.nodes[2].c.put(f"/{name}/_doc/e", params={**ext, "version": 10}, json={"v": 10})
        assert r.status_code == 200 and r.json()["_version"] == 10
        assert cl.nodes[3].c.get(f"/{name}/_doc/e").json()["_version"] == 10


# ================================================================= node failures


def need_ctl():
    if not CTL:
        pytest.skip("OS_CLUSTER_CTL is not set")


def wait_nodes(c, count):
    h = health(c, wait_for_nodes=str(count), timeout="90s")
    assert not h["timed_out"], h
    return h


def create_pinned_replicated(c, name, primary_on, replica_on, shards_=1, extra=None):
    """Index whose primaries sit on `primary_on` and replicas on `replica_on`."""
    r = c.put(
        f"/{name}",
        json={
            "settings": {
                "number_of_shards": shards_,
                "number_of_replicas": 1,
                "index.routing.allocation.require._name": primary_on,
                **FAST,
                **(extra or {}),
            }
        },
    )
    assert r.status_code == 200, r.text
    r = c.put(
        f"/{name}/_settings",
        json={"index.routing.allocation.require._name": None, "index.routing.allocation.include._name": f"{primary_on},{replica_on}"},
    )
    assert r.status_code == 200, r.text
    wait_green(c, name)
    for s in range(shards_):
        assert primary_node(c, name, str(s)) == primary_on


@pytest.fixture(scope="module")
def failover(cl):
    """Kill a data node holding primaries; observe the cluster; bring it back."""
    need_ctl()
    ensure_healthy(cl)
    mgr = cl.manager()
    others = [n for n in cl.nodes if n.id != mgr.id]
    victim, peer = others[0], others[1]
    survivors = [n for n in cl.nodes if n is not victim]
    c = mgr.c
    R, Z, G = f"{PFX}-fo-rep", f"{PFX}-fo-red", f"{PFX}-fo-green"
    obs = {"victim": victim, "peer": peer, "mgr": mgr, "survivors": survivors, "R": R, "Z": Z, "G": G}
    try:
        create_pinned_replicated(c, R, victim.name, peer.name)
        for name, node, n_sh in ((Z, victim, 2), (G, peer, 1)):
            r = c.put(
                f"/{name}",
                json={"settings": {"number_of_shards": n_sh, "number_of_replicas": 0, "index.routing.allocation.require._name": node.name, **FAST}},
            )
            assert r.status_code == 200, r.text
        wait_green(c, f"{R},{Z},{G}")
        bulk(cl.nodes[0].c, R, {f"r{i}": {"n": i} for i in range(20)})
        bulk(cl.nodes[1].c, Z, {f"z{i}": {"n": i} for i in range(10)})
        bulk(cl.nodes[2].c, G, {f"g{i}": {"n": i} for i in range(5)})
        obs["term_before"] = primary_terms(c, R)["0"]
        obs["doc_before"] = c.get(f"/{R}/_doc/r1").json()

        stop_node(victim, "kill")
        try:
            obs["health_nodes"] = wait_nodes(c, 3)
            obs["health_R"] = settle(c, R)
            obs["health_Z"] = health(c, Z, timeout="1s")
            obs["health_G"] = health(c, G, timeout="1s")
            obs["health_all"] = health(c, timeout="1s")
            obs["shards"] = shards(c, f"{R},{Z},{G}")
            obs["explain_R"] = explain(c, R)
            obs["explain_Z"] = explain(c, Z, primary=True)
            obs["term_after"] = primary_terms(c, R)["0"]
            obs["cat_nodes"] = {n.name: sorted(x["name"] for x in n.c.get("/_cat/nodes", params={"format": "json", "h": "name"}).json()) for n in survivors}
            obs["counts_R"] = {n.name: n.c.get(f"/{R}/_count").json()["count"] for n in survivors}
            obs["mget_R"] = {
                n.name: [d["found"] for d in n.c.post(f"/{R}/_mget", json={"ids": [f"r{i}" for i in range(20)]}).json()["docs"]]
                for n in survivors
            }
            obs["get_old"] = survivors[-1].c.get(f"/{R}/_doc/r1").json()
            obs["writes"] = [
                resp(survivors[i % len(survivors)].c.put(f"/{R}/_doc/w{i}", json={"w": i})) for i in range(9)
            ]
            obs["update"] = resp(survivors[0].c.post(f"/{R}/_update/r0", json={"doc": {"upd": True}}))
            obs["delete"] = resp(survivors[1].c.delete(f"/{R}/_doc/r19"))
            c.post(f"/{R}/_refresh")
            obs["count_R_after_writes"] = c.get(f"/{R}/_count").json()["count"]
            obs["search_Z"] = resp(c.post(f"/{Z}/_search", json={}))
            obs["search_Z_strict"] = resp(c.post(f"/{Z}/_search", params={"allow_partial_search_results": "false"}, json={}))
            obs["search_mixed"] = resp(c.post(f"/{Z},{G}/_search", json={"size": 0}))
            obs["search_mixed_strict"] = resp(c.post(f"/{Z},{G}/_search", params={"allow_partial_search_results": "false"}, json={"size": 0}))
            obs["count_mixed"] = resp(c.get(f"/{Z},{G}/_count"))
            obs["get_Z"] = resp(c.get(f"/{Z}/_doc/z1"))
            obs["index_Z"] = resp(c.put(f"/{Z}/_doc/znew", json={"a": 1}, params={"timeout": "1s"}))
            obs["bulk_Z"] = resp(
                c.post(
                    "/_bulk",
                    params={"timeout": "1s"},
                    content=ndjson([{"index": {"_index": Z, "_id": "zb"}}, {"a": 1}, {"index": {"_index": G, "_id": "gb"}}, {"a": 1}]),
                    headers={"Content-Type": "application/x-ndjson"},
                )
            )
        finally:
            start_node(victim)
            ensure_healthy(cl)

        obs["post_shards"] = shards(c, f"{R},{Z}")
        obs["post_copies"] = {}
        for node_name in copy_nodes(c, R):
            n = cl.by_name[node_name]
            obs["post_copies"][node_name] = {
                "count": c.get(f"/{R}/_count", params={"preference": f"_only_nodes:{n.id}"}).json()["count"],
                "docs": [
                    (d["_id"], d.get("_seq_no"), d.get("_version"), d.get("_primary_term"))
                    for d in c.post(
                        f"/{R}/_mget",
                        params={"preference": f"_only_nodes:{n.id}", "realtime": "false"},
                        json={"ids": [f"w{i}" for i in range(9)] + ["r0", "r19"]},
                    ).json()["docs"]
                ],
            }
        obs["post_count_Z"] = c.get(f"/{Z}/_count").json()["count"]
        obs["post_search_Z"] = resp(c.post(f"/{Z}/_search", json={"size": 0}))
        yield obs
    finally:
        ensure_healthy(cl)
        cl.any().c.delete(f"/{R},{Z},{G}", params={"ignore_unavailable": "true"})


class TestFailover:
    def test_survivors_see_three_nodes(self, failover):
        assert failover["health_nodes"]["number_of_nodes"] == 3
        assert failover["health_nodes"]["number_of_data_nodes"] == 3
        names = sorted(n.name for n in failover["survivors"])
        assert all(v == names for v in failover["cat_nodes"].values())

    def test_cluster_red_because_of_unreplicated_index(self, failover):
        assert failover["health_all"]["status"] == "red"
        assert failover["health_Z"]["status"] == "red"
        assert failover["health_Z"]["active_primary_shards"] == 0
        assert failover["health_Z"]["unassigned_shards"] == 2
        assert failover["health_G"]["status"] == "green"

    def test_replica_promoted_and_index_yellow(self, failover):
        h = failover["health_R"]
        assert h["status"] == "yellow" and h["active_primary_shards"] == 1 and h["unassigned_shards"] == 1
        rows = [x for x in failover["shards"] if x["index"] == failover["R"]]
        assert {(x["prirep"], x["state"], x["node"]) for x in rows} == {
            ("p", "STARTED", failover["peer"].name),
            ("r", "UNASSIGNED", None),
        }

    def test_primary_term_increases(self, failover):
        assert failover["term_before"] == 1
        assert failover["term_after"] == failover["term_before"] + 1

    def test_unassigned_reason_node_left(self, failover):
        for x in failover["shards"]:
            if x["state"] == "UNASSIGNED":
                assert x["unassigned.reason"] == "NODE_LEFT"
        info = failover["explain_R"]["unassigned_info"]
        assert info["reason"] == "NODE_LEFT"
        assert info["details"] == f"node_left [{failover['victim'].id}]"
        dec = deciders(failover["explain_R"])
        assert dec[failover["peer"].name] == {"same_shard": "NO"}
        assert failover["victim"].name not in dec

    def test_red_primary_explain(self, failover):
        e = failover["explain_Z"]
        assert e["current_state"] == "unassigned"
        assert e["unassigned_info"]["reason"] == "NODE_LEFT"
        assert e["can_allocate"] in ("no", "no_valid_shard_copy")

    def test_data_fully_readable_during_outage(self, failover):
        assert set(failover["counts_R"].values()) == {20}
        for v in failover["mget_R"].values():
            assert v == [True] * 20
        assert failover["get_old"]["_source"] == failover["doc_before"]["_source"]
        assert failover["get_old"]["_primary_term"] == 1

    def test_writes_during_outage(self, failover):
        for status, body in failover["writes"]:
            assert status == 201
            assert body["_shards"] == {"total": 2, "successful": 1, "failed": 0}
            assert body["_primary_term"] == 2
        seqs = [b["_seq_no"] for _, b in failover["writes"]]
        assert seqs == sorted(seqs) and seqs[0] == 20
        s, b = failover["update"]
        assert s == 200 and b["_version"] == 2 and b["_primary_term"] == 2
        s, b = failover["delete"]
        assert s == 200 and b["result"] == "deleted"
        assert failover["count_R_after_writes"] == 20 + 9 - 1

    def test_search_red_index_partial_allowed(self, failover):
        s, b = failover["search_Z"]
        assert s == 503
        assert b["error"]["type"] == "search_phase_execution_exception"
        assert b["error"]["reason"] == "all shards failed"
        assert b["error"]["phase"] == "query"
        assert b["error"]["failed_shards"] == [] and b["error"]["root_cause"] == []

    def test_search_red_index_partial_disallowed(self, failover):
        s, b = failover["search_Z_strict"]
        Z = failover["Z"]
        assert s == 503
        assert b["error"]["type"] == "search_phase_execution_exception"
        assert b["error"]["caused_by"]["reason"] == (
            f"Search rejected due to missing shards [[{Z}][0], [{Z}][1]]. "
            "Consider using `allow_partial_search_results` setting to bypass this error."
        )

    def test_search_mixed_partial_results(self, failover):
        s, b = failover["search_mixed"]
        assert s == 200
        # unassigned shards count in total but neither as successful nor failed
        assert b["_shards"] == {"total": 3, "successful": 1, "skipped": 0, "failed": 0}
        assert b["hits"]["total"] == {"value": 5, "relation": "eq"}

    def test_search_mixed_partial_disallowed(self, failover):
        s, b = failover["search_mixed_strict"]
        assert s == 503
        assert "Search rejected due to missing shards" in b["error"]["caused_by"]["reason"]

    def test_count_mixed(self, failover):
        s, b = failover["count_mixed"]
        assert s == 200
        assert b["count"] == 5
        assert b["_shards"]["total"] == 3 and b["_shards"]["successful"] == 1

    def test_get_and_write_on_red_index(self, failover):
        Z = failover["Z"]
        s, b = failover["get_Z"]
        assert s == 503
        assert b["error"]["type"] == "no_shard_available_action_exception"
        assert b["error"]["reason"].startswith(f"No shard available for [get [{Z}]")
        s, b = failover["index_Z"]
        assert s == 503
        assert b["error"]["type"] == "unavailable_shards_exception"
        assert b["error"]["reason"].endswith("primary shard is not active Timeout: [1s]")

    def test_bulk_mixed_red_and_green(self, failover):
        s, b = failover["bulk_Z"]
        assert s == 200 and b["errors"] is True
        z, g = b["items"]
        assert z["index"]["status"] == 503
        assert z["index"]["error"]["type"] == "unavailable_shards_exception"
        assert g["index"]["status"] == 201

    def test_node_return_restores_green_without_failback(self, failover):
        R = failover["R"]
        rows = [x for x in failover["post_shards"] if x["index"] == R]
        assert all(x["state"] == "STARTED" for x in rows)
        # the promoted copy stays primary; the returning node gets the replica
        assert {(x["prirep"], x["node"]) for x in rows} == {
            ("p", failover["peer"].name),
            ("r", failover["victim"].name),
        }

    def test_writes_made_during_outage_on_all_copies(self, failover):
        copies = failover["post_copies"]
        assert len(copies) == 2
        views = list(copies.values())
        assert all(v["count"] == 28 for v in views)
        assert views[0]["docs"] == views[1]["docs"]
        docs = {d[0]: d for d in views[0]["docs"]}
        assert docs["r19"][1] is None  # deleted during the outage
        assert all(docs[f"w{i}"][3] == 2 for i in range(9))

    def test_red_index_recovers_with_data(self, failover):
        assert failover["post_count_Z"] == 10
        s, b = failover["post_search_Z"]
        assert s == 200 and b["_shards"]["failed"] == 0 and b["_shards"]["successful"] == 2


@pytest.fixture(scope="module")
def manager_kill(cl):
    """Kill the elected cluster manager and observe the election."""
    need_ctl()
    ensure_healthy(cl)
    old = cl.manager()
    survivors = [n for n in cl.nodes if n.id != old.id]
    obs = {"old": old, "survivors": survivors, "term_before": voting(old.c)["term"]}
    name = f"{PFX}-mk"
    try:
        stop_node(old, "kill")
        try:
            deadline = time.time() + 90
            while time.time() < deadline:
                try:
                    ids = {cl.manager(n).id for n in survivors}
                    if len(ids) == 1 and old.id not in ids:
                        break
                except (httpx.HTTPError, AssertionError, IndexError, KeyError):
                    pass
                time.sleep(0.5)
            obs["managers"] = {n.name: cl.manager(n).id for n in survivors}
            c = survivors[0].c
            obs["health"] = wait_nodes(c, 3)
            obs["term_after"] = voting(c)["term"]
            obs["state_mgr"] = {n.name: n.c.get("/_cluster/state/cluster_manager_node").json()["cluster_manager_node"] for n in survivors}
            obs["create"] = resp(survivors[-1].c.put(f"/{name}", json={"settings": {"number_of_shards": 2, "number_of_replicas": 1, **FAST}}))
            obs["mapping"] = resp(survivors[1].c.put(f"/{name}/_mapping", json={"properties": {"k": {"type": "keyword"}}}))
            obs["doc"] = resp(survivors[0].c.put(f"/{name}/_doc/1", json={"k": "v"}, params={"refresh": "true"}))
            obs["seen"] = {n.name: n.c.get(f"/{name}/_count").json()["count"] for n in survivors}
            obs["setting"] = resp(survivors[2].c.put("/_cluster/settings", json={"transient": {"search.max_buckets": 4321}}))
            obs["setting_seen"] = {
                n.name: n.c.get("/_cluster/settings", params={"flat_settings": "true"}).json()["transient"].get("search.max_buckets")
                for n in survivors
            }
            survivors[0].c.put("/_cluster/settings", json={"transient": {"search.max_buckets": None}})
            obs["cat_nodes"] = survivors[0].c.get("/_cat/nodes", params={"format": "json", "h": "name,cluster_manager"}).json()
        finally:
            start_node(old)
            ensure_healthy(cl)
        obs["after_rejoin"] = {n.name: cl.manager(n).id for n in cl.nodes}
        obs["rejoined_mapping"] = resp(old.c.get(f"/{name}/_mapping"))
        yield obs
    finally:
        ensure_healthy(cl)
        c = cl.any().c
        c.delete(f"/{name}", params={"ignore_unavailable": "true"})
        c.put("/_cluster/settings", json={"transient": {"search.max_buckets": None}})


class TestManagerFailure:
    def test_new_manager_elected_and_agreed(self, manager_kill):
        ids = set(manager_kill["managers"].values())
        assert len(ids) == 1
        (new,) = ids
        assert new != manager_kill["old"].id
        assert set(manager_kill["state_mgr"].values()) == {new}

    def test_new_manager_is_eligible_not_data_only(self, cl, manager_kill):
        (new,) = set(manager_kill["managers"].values())
        assert "cluster_manager" in cl.by_id[new].roles
        assert new not in {n.id for n in cl.data_only()}

    def test_term_increases(self, manager_kill):
        assert manager_kill["term_after"] > manager_kill["term_before"]

    def test_cluster_has_three_nodes(self, manager_kill):
        h = manager_kill["health"]
        assert h["number_of_nodes"] == 3 and h["discovered_cluster_manager"] is True
        rows = manager_kill["cat_nodes"]
        assert sorted(x["name"] for x in rows) == sorted(n.name for n in manager_kill["survivors"])
        assert sum(x["cluster_manager"] == "*" for x in rows) == 1

    def test_cluster_operations_work_without_old_manager(self, manager_kill):
        assert manager_kill["create"][0] == 200 and manager_kill["create"][1]["acknowledged"] is True
        assert manager_kill["mapping"] == (200, {"acknowledged": True})
        assert manager_kill["doc"][0] == 201
        assert set(manager_kill["seen"].values()) == {1}
        assert manager_kill["setting"][0] == 200
        assert set(manager_kill["setting_seen"].values()) == {"4321"}

    def test_old_manager_rejoins_as_follower(self, manager_kill):
        (new,) = set(manager_kill["managers"].values())
        assert set(manager_kill["after_rejoin"].values()) == {new}

    def test_rejoined_node_has_metadata_created_while_away(self, manager_kill):
        s, b = manager_kill["rejoined_mapping"]
        assert s == 200
        ((_, m),) = b.items()
        assert m["mappings"]["properties"]["k"] == {"type": "keyword"}


@pytest.fixture(scope="module")
def data_only_stop(cl):
    """Gracefully stop the data-only node."""
    need_ctl()
    ensure_healthy(cl)
    (d,) = cl.data_only()
    mgr = cl.manager()
    peer = next(n for n in cl.nodes if n.id not in (d.id, mgr.id))
    c = mgr.c
    AE, P = f"{PFX}-dos-ae", f"{PFX}-dos-p"
    obs = {"node": d, "peer": peer, "mgr": mgr, "AE": AE, "P": P}
    try:
        r = c.put(f"/{AE}", json={"settings": {"number_of_shards": 1, "auto_expand_replicas": "0-all", **FAST}})
        assert r.status_code == 200, r.text
        create_pinned_replicated(c, P, d.name, peer.name)
        wait_green(c, f"{AE},{P}")
        bulk(c, AE, {str(i): {"n": i} for i in range(7)})
        bulk(c, P, {str(i): {"n": i} for i in range(7)})
        obs["replicas_before"] = c.get(f"/{AE}/_settings", params={"flat_settings": "true"}).json()[AE]["settings"]["index.number_of_replicas"]
        stop_node(d, "stop")
        try:
            obs["health"] = wait_nodes(c, 3)
            deadline = time.time() + 60
            while time.time() < deadline:
                rep = c.get(f"/{AE}/_settings", params={"flat_settings": "true"}).json()[AE]["settings"]["index.number_of_replicas"]
                if rep == "2":
                    break
                time.sleep(0.5)
            obs["replicas_during"] = rep
            obs["health_AE"] = settle(c, AE, "green")
            obs["health_P"] = settle(c, P)
            obs["shards_P"] = shards(c, P)
            obs["explain_P"] = explain(c, P)
            obs["mgr_during"] = cl.manager(mgr).id
            obs["counts"] = {n.name: (n.c.get(f"/{AE}/_count").json()["count"], n.c.get(f"/{P}/_count").json()["count"]) for n in cl.nodes if n is not d}
        finally:
            start_node(d)
            ensure_healthy(cl)
        deadline = time.time() + 60
        while time.time() < deadline:
            rep = c.get(f"/{AE}/_settings", params={"flat_settings": "true"}).json()[AE]["settings"]["index.number_of_replicas"]
            if rep == "3":
                break
            time.sleep(0.5)
        ensure_healthy(cl)
        obs["replicas_after"] = rep
        obs["copies_after"] = copy_nodes(c, AE)
        yield obs
    finally:
        ensure_healthy(cl)
        cl.any().c.delete(f"/{AE},{P}", params={"ignore_unavailable": "true"})


class TestDataOnlyNodeStop:
    def test_cluster_shrinks_to_three_data_nodes(self, data_only_stop):
        h = data_only_stop["health"]
        assert h["number_of_nodes"] == 3 and h["number_of_data_nodes"] == 3

    def test_manager_unchanged(self, data_only_stop):
        assert data_only_stop["mgr_during"] == data_only_stop["mgr"].id

    def test_auto_expand_replicas_follow_node_count(self, data_only_stop):
        assert data_only_stop["replicas_before"] == "3"
        assert data_only_stop["replicas_during"] == "2"
        assert data_only_stop["health_AE"]["status"] == "green"
        assert data_only_stop["replicas_after"] == "3"
        assert len(data_only_stop["copies_after"]) == 4

    def test_graceful_stop_promotes_replica_with_node_left(self, data_only_stop):
        assert data_only_stop["health_P"]["status"] == "yellow"
        rows = data_only_stop["shards_P"]
        assert {(x["prirep"], x["state"], x["node"]) for x in rows} == {
            ("p", "STARTED", data_only_stop["peer"].name),
            ("r", "UNASSIGNED", None),
        }
        info = data_only_stop["explain_P"]["unassigned_info"]
        assert info["reason"] == "NODE_LEFT"
        assert info["details"] == f"node_left [{data_only_stop['node'].id}]"

    def test_data_readable_on_survivors(self, data_only_stop):
        assert set(data_only_stop["counts"].values()) == {(7, 7)}
