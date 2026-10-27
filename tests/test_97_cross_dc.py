"""Cross-datacenter behaviour: zone awareness, weighted routing, decommission,
whole-DC failure and cross-cluster search.

Opt-in. Topology (see servers/opensearch-cluster.sh): a stretched main cluster whose
nodes carry node.attr.zone (two zones, the voting majority in one of them) plus a
separate single-node cluster acting as another datacenter.

    eval "$(SS_CLUSTER_ID=2 servers/opensearch-cluster.sh env)"
    OS_PORT=9611 .venv/bin/pytest tests/test_97_cross_dc.py -q

    OS_CLUSTER_URLS  comma-separated http urls of every main-cluster node (zone tests)
    OS_REMOTE_URL    http url of the remote cluster (cross-cluster search tests)
    OS_CLUSTER_CTL   command accepting `stop-node N`, `kill-node N`, `start-node N`
                     (whole-DC failure and remote-down tests)

Node names, zones, voters and transport addresses are discovered through the APIs.
Every test that changes cluster settings or stops nodes restores them in teardown.
"""

import json
import os
import shlex
import subprocess
import time
import uuid
from collections import defaultdict

import httpx
import pytest

CLUSTER_URLS = [u.strip().rstrip("/") for u in os.getenv("OS_CLUSTER_URLS", "").split(",") if u.strip()]
REMOTE_URL = (os.getenv("OS_REMOTE_URL") or "").rstrip("/")
CTL = os.getenv("OS_CLUSTER_CTL")
TIMEOUT = float(os.getenv("OS_TIMEOUT", "60"))
RECOVERY_TIMEOUT = float(os.getenv("OS_CLUSTER_RECOVERY_TIMEOUT", "300"))

pytestmark = pytest.mark.crossdc

needs_cluster = pytest.mark.skipif(not CLUSTER_URLS, reason="OS_CLUSTER_URLS is not set")
needs_ctl = pytest.mark.skipif(
    not (CLUSTER_URLS and CTL), reason="OS_CLUSTER_URLS and OS_CLUSTER_CTL are required"
)
needs_remote = pytest.mark.skipif(
    not (CLUSTER_URLS and REMOTE_URL), reason="OS_CLUSTER_URLS and OS_REMOTE_URL are required"
)
needs_remote_ctl = pytest.mark.skipif(
    not (CLUSTER_URLS and REMOTE_URL and CTL),
    reason="OS_CLUSTER_URLS, OS_REMOTE_URL and OS_CLUSTER_CTL are required",
)

PFX = f"slopensearch-xdc-{uuid.uuid4().hex[:8]}"
ATTR = "zone"
WEIGHTS = f"/_cluster/routing/awareness/{ATTR}/weights"
DECOM = "/_cluster/decommission/awareness"
ROUTING_KEYS = [
    "cluster.routing.allocation.awareness.*",
    "cluster.routing.weighted.*",
    "cluster.routing.ignore_weighted_routing",
]
NDJSON = {"Content-Type": "application/x-ndjson"}


# --------------------------------------------------------------------------- helpers


def _ndjson(lines):
    return ("\n".join(json.dumps(line) for line in lines) + "\n").encode()


def _err(resp, status, etype, reason=None, contains=None):
    assert resp.status_code == status, resp.text
    err = resp.json()["error"]
    assert err["type"] == etype, resp.text
    assert err["root_cause"][0]["type"] == etype, resp.text
    if reason is not None:
        assert err["reason"] == reason, resp.text
    if contains is not None:
        assert contains in err["reason"], resp.text
    return err


def _ok(resp, status=200):
    assert resp.status_code == status, resp.text
    return resp.json()


def _bulk(client, index, docs):
    lines = []
    for doc_id, doc in docs:
        lines += [{"index": {"_index": index, "_id": doc_id}}, doc]
    body = _ok(client.post("/_bulk", params={"refresh": "true"}, content=_ndjson(lines), headers=NDJSON))
    assert body["errors"] is False, body
    return body


def _put_settings(client, persistent=None, transient=None):
    body = {}
    if persistent is not None:
        body["persistent"] = persistent
    if transient is not None:
        body["transient"] = transient
    return _ok(client.put("/_cluster/settings", json=body))


def _wait_index(client, index, status="green", active=None):
    params = {"wait_for_status": status, "wait_for_no_initializing_shards": "true", "timeout": "60s"}
    if active is not None:
        params["wait_for_active_shards"] = str(active)
    r = client.get(f"/_cluster/health/{index}", params=params)
    body = _ok(r)
    assert body["timed_out"] is False, body
    return body


def _create(client, name, shards=1, replicas=1, mappings=None, wait="green", **settings):
    payload = {
        "settings": {
            "number_of_shards": shards,
            "number_of_replicas": replicas,
            "index.unassigned.node_left.delayed_timeout": "0",
            **settings,
        }
    }
    if mappings:
        payload["mappings"] = mappings
    _ok(client.put(f"/{name}", json=payload))
    if wait:
        _wait_index(client, name, wait)
    return name


def _delete_indices(client, names):
    for name in names:
        try:
            client.delete(f"/{name}", params={"ignore_unavailable": "true"})
        except httpx.HTTPError:
            pass


def _layout(client, index):
    rows = _ok(
        client.get(f"/_cat/shards/{index}", params={"format": "json", "h": "shard,prirep,state,node"})
    )
    out = defaultdict(list)
    for row in rows:
        out[int(row["shard"])].append(row)
    return dict(out)


def _ctl(action, node):
    subprocess.run(f"{CTL} {action} {shlex.quote(node)}", shell=True, check=True, timeout=300, capture_output=True)


def _weights(client):
    r = client.get(WEIGHTS)
    return _ok(r)


def _put_weights(client, weights):
    version = _weights(client).get("_version", -1)
    r = client.put(WEIGHTS, json={"weights": weights, "_version": version})
    _ok(r)
    return version


def _clear_weights(client):
    r = client.get(WEIGHTS)
    if r.status_code == 200 and r.json().get("weights"):
        client.request("DELETE", WEIGHTS, json={"_version": r.json()["_version"]})


def _reset_routing(client):
    """Recommission, drop weights, clear awareness / weighted-routing settings."""
    client.request("DELETE", DECOM)
    _put_settings(client, persistent={"cluster.routing.allocation.awareness.attributes": ATTR})
    _clear_weights(client)
    nulls = {k: None for k in ROUTING_KEYS}
    _put_settings(client, persistent=nulls, transient=nulls)


def _served_by(client, index, **params):
    r = client.get(f"/{index}/_search", params={"explain": "true", "size": 200, **params})
    body = _ok(r)
    return body, {h["_node"] for h in body["hits"]["hits"]}


class Node:
    def __init__(self, url, nid, info):
        self.url = url
        self.id = nid
        self.name = info["name"]
        self.zone = info["attributes"].get(ATTR)
        self.roles = info["roles"]
        self.transport = info["transport"]["publish_address"]
        self.client = httpx.Client(base_url=url, timeout=TIMEOUT)

    def __repr__(self):
        return f"Node({self.name}, {self.zone})"


class Topology:
    def __init__(self, nodes, voters):
        self.nodes = nodes  # name -> Node
        self.by_id = {n.id: n for n in nodes.values()}
        self.zones = defaultdict(list)
        for n in nodes.values():
            self.zones[n.zone].append(n)
        votes = {z: sum(1 for n in ns if n.id in voters) for z, ns in self.zones.items()}
        ordered = sorted(self.zones, key=lambda z: (-votes[z], z))
        self.primary_zone, self.secondary_zone = ordered[0], ordered[1]
        self.created = []

    def zone_nodes(self, zone):
        return sorted(self.zones[zone], key=lambda n: n.name)

    def ids(self, zone):
        return {n.id for n in self.zones[zone]}

    def client(self, zone=None):
        zone = zone or self.primary_zone
        return self.zone_nodes(zone)[0].client

    def index(self, suffix):
        name = f"{PFX}-{suffix}"
        self.created.append(name)
        return name

    def zone_of(self, node_name):
        return self.nodes[node_name].zone


def _wait_healthy(topo, timeout=RECOVERY_TIMEOUT):
    total = len(topo.nodes)
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        healthy = True
        for node in topo.nodes.values():
            try:
                r = node.client.get(
                    "/_cluster/health",
                    params={
                        "wait_for_nodes": str(total),
                        "wait_for_status": "green",
                        "wait_for_no_relocating_shards": "true",
                        "timeout": "5s",
                    },
                    timeout=30,
                )
                last = f"{node.name}: {r.status_code} {r.text[:300]}"
                if r.status_code != 200 or r.json()["timed_out"] or r.json()["number_of_nodes"] != total:
                    healthy = False
                    break
            except httpx.HTTPError as exc:
                last = f"{node.name}: {exc!r}"
                healthy = False
                break
        if healthy:
            return
        time.sleep(1)
    raise AssertionError(f"cluster did not recover to {total} nodes / green: {last}")


def _restore_all(topo):
    if CTL:
        for name in sorted(topo.nodes):
            _ctl("start-node", name)
    _wait_healthy(topo)


@pytest.fixture(scope="module")
def topo():
    nodes = {}
    for url in CLUSTER_URLS:
        c = httpx.Client(base_url=url, timeout=TIMEOUT)
        body = _ok(c.get("/_nodes/_local"))
        c.close()
        ((nid, info),) = body["nodes"].items()
        node = Node(url, nid, info)
        nodes[node.name] = node
    first = next(iter(nodes.values()))
    state = _ok(
        first.client.get(
            "/_cluster/state/metadata",
            params={"filter_path": "metadata.cluster_coordination.last_committed_config"},
        )
    )
    voters = set(state["metadata"]["cluster_coordination"]["last_committed_config"])
    t = Topology(nodes, voters)
    assert len(t.zones) >= 2, f"need at least two zones, got {dict(t.zones)}"
    _wait_healthy(t, timeout=120)
    try:
        yield t
    finally:
        try:
            _restore_all(t)
        except Exception:  # noqa: BLE001 - best effort, report through the tests themselves
            pass
        c = t.client()
        try:
            _reset_routing(c)
        except httpx.HTTPError:
            pass
        _delete_indices(c, t.created)
        for n in nodes.values():
            n.client.close()


# =================================================================== zone awareness


@needs_cluster
class TestZoneTopology:
    def test_every_node_reports_zone_attribute(self, topo):
        body = _ok(topo.client().get("/_nodes", params={"filter_path": "nodes.*.attributes,nodes.*.name"}))
        attrs = {n["name"]: n["attributes"].get(ATTR) for n in body["nodes"].values()}
        assert attrs == {name: n.zone for name, n in topo.nodes.items()}
        assert all(attrs.values())

    def test_each_zone_has_two_or_more_data_nodes(self, topo):
        for zone in (topo.primary_zone, topo.secondary_zone):
            assert len([n for n in topo.zones[zone] if "data" in n.roles]) >= 2

    def test_cat_nodeattrs_lists_zone(self, topo):
        rows = _ok(topo.client().get("/_cat/nodeattrs", params={"format": "json", "h": "node,attr,value"}))
        zones = {r["node"]: r["value"] for r in rows if r["attr"] == ATTR}
        assert zones == {name: n.zone for name, n in topo.nodes.items()}

    def test_voting_majority_lives_in_primary_zone(self, topo):
        state = _ok(
            topo.client().get(
                "/_cluster/state/metadata",
                params={"filter_path": "metadata.cluster_coordination.last_committed_config"},
            )
        )
        voters = state["metadata"]["cluster_coordination"]["last_committed_config"]
        in_primary = [v for v in voters if v in topo.ids(topo.primary_zone)]
        assert len(in_primary) * 2 > len(voters)


@pytest.fixture(scope="class")
def awareness(topo):
    c = topo.client()
    _reset_routing(c)
    _put_settings(c, persistent={"cluster.routing.allocation.awareness.attributes": ATTR})
    try:
        yield c
    finally:
        _reset_routing(c)


@needs_cluster
class TestZoneAwareness:
    @pytest.mark.parametrize("shards,replicas", [(1, 1), (2, 1), (3, 1), (6, 1), (1, 2), (3, 2)])
    def test_every_shard_spans_both_zones(self, topo, awareness, shards, replicas):
        idx = _create(awareness, topo.index(f"aw-{shards}-{replicas}"), shards, replicas)
        layout = _layout(awareness, idx)
        assert sorted(layout) == list(range(shards))
        for shard, copies in layout.items():
            assert len(copies) == replicas + 1
            assert all(c["state"] == "STARTED" for c in copies)
            zones = {topo.zone_of(c["node"]) for c in copies}
            assert {topo.primary_zone, topo.secondary_zone} <= zones, (shard, copies)
        awareness.delete(f"/{idx}")

    def test_three_replicas_put_two_copies_in_each_zone(self, topo, awareness):
        idx = _create(awareness, topo.index("aw-r3"), 2, 3)
        for copies in _layout(awareness, idx).values():
            per_zone = defaultdict(int)
            for c in copies:
                per_zone[topo.zone_of(c["node"])] += 1
            assert per_zone == {topo.primary_zone: 2, topo.secondary_zone: 2}
        awareness.delete(f"/{idx}")

    def test_more_copies_than_nodes_leaves_one_unassigned(self, topo, awareness):
        replicas = len(topo.nodes)
        idx = _create(awareness, topo.index("aw-over"), 1, replicas, wait=None)
        _wait_index(awareness, idx, "yellow", active=len(topo.nodes))
        health = _ok(awareness.get(f"/_cluster/health/{idx}"))
        assert health["status"] == "yellow"
        assert health["unassigned_shards"] == 1
        assert health["active_shards"] == len(topo.nodes)
        explain = _ok(
            awareness.post("/_cluster/allocation/explain", json={"index": idx, "shard": 0, "primary": False})
        )
        assert explain["can_allocate"] == "no"
        for decision in explain["node_allocation_decisions"]:
            deciders = {d["decider"]: d["decision"] for d in decision["deciders"]}
            assert deciders.get("same_shard") == "NO", decision
        awareness.delete(f"/{idx}")

    def test_raising_replicas_keeps_zone_spread(self, topo, awareness):
        idx = _create(awareness, topo.index("aw-grow"), 3, 0)
        _ok(awareness.put(f"/{idx}/_settings", json={"index": {"number_of_replicas": 1}}))
        _wait_index(awareness, idx)
        for copies in _layout(awareness, idx).values():
            assert {topo.zone_of(c["node"]) for c in copies} == {topo.primary_zone, topo.secondary_zone}
        awareness.delete(f"/{idx}")

    def test_awareness_setting_is_reported(self, awareness):
        body = _ok(awareness.get("/_cluster/settings", params={"flat_settings": "true"}))
        assert body["persistent"]["cluster.routing.allocation.awareness.attributes"] == ATTR


@pytest.fixture
def forced(topo):
    c = topo.client()
    _reset_routing(c)
    created = []

    def _force(values, balance=None):
        persistent = {
            "cluster.routing.allocation.awareness.attributes": ATTR,
            f"cluster.routing.allocation.awareness.force.{ATTR}.values": ",".join(values),
        }
        if balance is not None:
            persistent["cluster.routing.allocation.awareness.balance"] = balance
        _put_settings(c, persistent=persistent)
        return c

    _force.created = created
    try:
        yield _force
    finally:
        _delete_indices(c, created)
        _reset_routing(c)


def _cluster_manager(topo):
    (row,) = _ok(topo.client().get("/_cat/cluster_manager", params={"format": "json"}))
    return topo.nodes[row["node"]]


def _non_cluster_manager(topo):
    cm = _cluster_manager(topo)
    return next(n for n in sorted(topo.nodes.values(), key=lambda n: n.name) if n.name != cm.name)


def _ghost_zone(topo):
    return "dc3" if "dc3" not in topo.zones else f"{PFX}-ghost"


@needs_cluster
class TestForcedAwareness:
    def _zones(self, topo, ghost=False):
        zones = [topo.primary_zone, topo.secondary_zone]
        return zones + [_ghost_zone(topo)] if ghost else zones

    def test_two_forced_zones_one_replica_green(self, topo, forced):
        c = forced(self._zones(topo))
        idx = _create(c, topo.index("f2r1"), 3, 1)
        forced.created.append(idx)
        assert _ok(c.get(f"/_cluster/health/{idx}"))["status"] == "green"

    def test_two_forced_zones_two_replicas_all_assigned(self, topo, forced):
        c = forced(self._zones(topo))
        idx = _create(c, topo.index("f2r2"), 2, 2)
        forced.created.append(idx)
        for copies in _layout(c, idx).values():
            assert all(x["state"] == "STARTED" for x in copies)
            assert {topo.zone_of(x["node"]) for x in copies} == set(self._zones(topo))

    def test_three_forced_zones_one_replica_green(self, topo, forced):
        c = forced(self._zones(topo, ghost=True))
        idx = _create(c, topo.index("f3r1"), 2, 1)
        forced.created.append(idx)
        assert _ok(c.get(f"/_cluster/health/{idx}"))["unassigned_shards"] == 0

    def test_three_forced_zones_two_replicas_one_copy_unassigned_per_shard(self, topo, forced):
        c = forced(self._zones(topo, ghost=True))
        idx = _create(c, topo.index("f3r2"), 2, 2, wait=None)
        _wait_index(c, idx, "yellow", active=4)
        forced.created.append(idx)
        health = _ok(c.get(f"/_cluster/health/{idx}"))
        assert health["status"] == "yellow"
        assert health["unassigned_shards"] == 2
        assert health["active_shards"] == 4
        for copies in _layout(c, idx).values():
            started = [x for x in copies if x["state"] == "STARTED"]
            assert len(started) == 2
            assert {topo.zone_of(x["node"]) for x in started} == {topo.primary_zone, topo.secondary_zone}

    def test_allocation_explain_names_awareness_decider(self, topo, forced):
        c = forced(self._zones(topo, ghost=True))
        idx = _create(c, topo.index("f3exp"), 1, 2, wait=None)
        _wait_index(c, idx, "yellow", active=2)
        forced.created.append(idx)
        explain = _ok(c.post("/_cluster/allocation/explain", json={"index": idx, "shard": 0, "primary": False}))
        assert explain["current_state"] == "unassigned"
        assert explain["can_allocate"] == "no"
        assert explain["unassigned_info"]["reason"] == "INDEX_CREATED"
        assert explain["allocate_explanation"] == "cannot allocate because allocation is not permitted to any of the nodes"
        decisions = explain["node_allocation_decisions"]
        assert {d["node_name"] for d in decisions} == set(topo.nodes)
        for d in decisions:
            assert d["node_decision"] == "no"
            assert d["node_attributes"][ATTR] == topo.nodes[d["node_name"]].zone
            aw = [x for x in d["deciders"] if x["decider"] == "awareness"]
            assert aw and aw[0]["decision"] == "NO", d
            assert f"attribute [{ATTR}]" in aw[0]["explanation"]
            assert "there are [3] total configured shard copies for this shard id and [3] total attribute values" in aw[0]["explanation"]

    def test_dropping_ghost_zone_assigns_the_copy(self, topo, forced):
        c = forced(self._zones(topo, ghost=True))
        idx = _create(c, topo.index("f3lift"), 1, 2, wait=None)
        _wait_index(c, idx, "yellow", active=2)
        forced.created.append(idx)
        forced(self._zones(topo))
        _wait_index(c, idx)
        assert len([x for x in _layout(c, idx)[0] if x["state"] == "STARTED"]) == 3

    def test_balance_rejects_copies_not_multiple_of_zones(self, topo, forced):
        forced(self._zones(topo, ghost=True), balance=True)
        c = _cluster_manager(topo).client
        name = topo.index("bal3")
        _err(
            c.put(f"/{name}", json={"settings": {"number_of_shards": 1, "number_of_replicas": 1}}),
            400,
            "validation_exception",
            reason="Validation Failed: 1: expected total copies needs to be a multiple of total awareness attributes [3];",
        )
        assert c.head(f"/{name}").status_code == 404

    def test_balance_error_type_when_forwarded_to_cluster_manager(self, topo, forced):
        # ValidationException is not a registered transport exception: when the request is
        # coordinated by a node that is not the elected cluster manager it comes back as its
        # parent type, illegal_argument_exception
        forced(self._zones(topo, ghost=True), balance=True)
        c = _non_cluster_manager(topo).client
        _err(
            c.put(f"/{topo.index('balfwd')}", json={"settings": {"number_of_shards": 1, "number_of_replicas": 1}}),
            400,
            "illegal_argument_exception",
            reason="Validation Failed: 1: expected total copies needs to be a multiple of total awareness attributes [3];",
        )

    def test_balance_two_zones_rejects_three_copies(self, topo, forced):
        forced(self._zones(topo), balance=True)
        c = _cluster_manager(topo).client
        _err(
            c.put(f"/{topo.index('bal2')}", json={"settings": {"number_of_shards": 1, "number_of_replicas": 2}}),
            400,
            "validation_exception",
            reason="Validation Failed: 1: expected total copies needs to be a multiple of total awareness attributes [2];",
        )
        idx = _create(c, topo.index("bal2ok"), 1, 3)
        forced.created.append(idx)

    def test_balance_rejects_replica_update(self, topo, forced):
        forced(self._zones(topo), balance=True)
        c = _cluster_manager(topo).client
        idx = _create(c, topo.index("balupd"), 1, 1)
        forced.created.append(idx)
        _err(
            c.put(f"/{idx}/_settings", json={"index": {"number_of_replicas": 2}}),
            400,
            "validation_exception",
            reason="Validation Failed: 1: expected total copies needs to be a multiple of total awareness attributes [2];",
        )
        flat = _ok(c.get(f"/{idx}/_settings", params={"flat_settings": "true"}))[idx]["settings"]
        assert flat["index.number_of_replicas"] == "1"

    def test_balance_exempts_auto_expand_replicas(self, topo, forced):
        c = forced(self._zones(topo, ghost=True), balance=True)
        idx = _create(c, topo.index("balauto"), 1, 2, wait=None, auto_expand_replicas="0-all")
        forced.created.append(idx)
        assert c.head(f"/{idx}").status_code == 200


# =================================================================== weighted routing


@needs_cluster
class TestWeightsNeedAwareness:
    @pytest.fixture(autouse=True)
    def _plain(self, topo):
        _reset_routing(topo.client())
        yield
        _reset_routing(topo.client())

    def test_get_without_awareness(self, topo):
        _err(
            topo.client().get(WEIGHTS),
            400,
            "action_request_validation_exception",
            reason=f"Validation Failed: 1: invalid awareness attribute {ATTR} requested for weighted routing;",
        )

    def test_put_without_awareness(self, topo):
        _err(
            _cluster_manager(topo).client.put(WEIGHTS, json={"weights": {topo.primary_zone: "1", topo.secondary_zone: "0"}, "_version": -1}),
            400,
            "action_request_validation_exception",
            reason=f"Validation Failed: 1: invalid awareness attribute {ATTR} requested for weighted routing;",
        )

    def test_put_without_awareness_forwarded_to_cluster_manager(self, topo):
        # validated on the elected cluster manager; the exception crosses the transport layer
        # as its parent type when another node coordinates the request
        _err(
            _non_cluster_manager(topo).client.put(
                WEIGHTS, json={"weights": {topo.primary_zone: "1", topo.secondary_zone: "0"}, "_version": -1}
            ),
            400,
            "illegal_argument_exception",
            reason=f"Validation Failed: 1: invalid awareness attribute {ATTR} requested for weighted routing;",
        )

    def test_decommission_without_awareness(self, topo):
        z = topo.secondary_zone
        _err(
            topo.client().put(f"{DECOM}/{ATTR}/{z}"),
            400,
            "decommissioning_failed_exception",
            reason=f"[DecommissionAttribute{{attributeName='{ATTR}', attributeValue='{z}'}}] invalid awareness attribute requested for decommissioning",
        )


@pytest.fixture(scope="class")
def weighted(topo):
    c = topo.client()
    _reset_routing(c)
    _put_settings(c, persistent={"cluster.routing.allocation.awareness.attributes": ATTR})
    # a cluster that never had weights reports no _version; one put+delete makes the
    # metadata (and its version counter) exist, so the tests behave the same on any cluster
    _put_weights(c, {topo.primary_zone: "1", topo.secondary_zone: "1"})
    _clear_weights(c)
    idx = _create(c, topo.index("wr"), 4, 1)
    _bulk(c, idx, [(str(i), {"n": i, "tag": "even" if i % 2 == 0 else "odd"}) for i in range(40)])
    try:
        yield {"client": c, "index": idx}
    finally:
        _reset_routing(c)
        _delete_indices(c, [idx])


@needs_cluster
class TestWeightedRoutingApi:
    @pytest.fixture(autouse=True)
    def _clean(self, weighted):
        c = weighted["client"]
        _clear_weights(c)
        yield
        _put_settings(c, transient={"cluster.routing.weighted.*": None, "cluster.routing.ignore_weighted_routing": None})
        _clear_weights(c)

    def zw(self, topo, w1, w2, **extra):
        return {topo.primary_zone: w1, topo.secondary_zone: w2, **extra}

    def test_put_then_get(self, topo, weighted):
        c = weighted["client"]
        version = _weights(c)["_version"]
        r = c.put(WEIGHTS, json={"weights": self.zw(topo, "1", "0"), "_version": version})
        assert _ok(r) == {"acknowledged": True}
        assert _weights(c) == {
            "weights": self.zw(topo, "1.0", "0.0"),
            "_version": version + 1,
            "discovered_cluster_manager": True,
        }

    def test_get_local_on_every_node(self, topo, weighted):
        _put_weights(weighted["client"], self.zw(topo, "1", "0"))
        expected = _weights(weighted["client"])
        for node in topo.nodes.values():
            assert _ok(node.client.get(WEIGHTS, params={"local": "true"})) == expected
            assert _ok(node.client.get(WEIGHTS)) == expected

    def test_weights_in_cluster_state(self, topo, weighted):
        _put_weights(weighted["client"], self.zw(topo, "1", "0"))
        state = _ok(
            weighted["client"].get("/_cluster/state/metadata", params={"filter_path": "metadata.weighted_shard_routing"})
        )
        wsr = state["metadata"]["weighted_shard_routing"]
        assert wsr["awareness"] == {ATTR: self.zw(topo, 1.0, 0.0)}
        assert wsr["_version"] == _weights(weighted["client"])["_version"]

    def test_weights_are_normalised_to_floats(self, topo, weighted):
        _put_weights(weighted["client"], self.zw(topo, "0.5", "2", extra="1e1"))
        assert _weights(weighted["client"])["weights"] == self.zw(topo, "0.5", "2.0", extra="10.0")

    def test_extra_unknown_zone_accepted(self, topo, weighted):
        _put_weights(weighted["client"], self.zw(topo, "1", "1", dc9="5"))
        assert _weights(weighted["client"])["weights"]["dc9"] == "5.0"

    def test_negative_weight_accepted(self, topo, weighted):
        _put_weights(weighted["client"], self.zw(topo, "-1", "1"))
        assert _weights(weighted["client"])["weights"][topo.primary_zone] == "-1.0"

    def test_stale_version_conflict(self, topo, weighted):
        c = weighted["client"]
        _put_weights(c, self.zw(topo, "1", "1"))
        current = _weights(c)["_version"]
        stale = current - 1
        _err(
            c.put(WEIGHTS, json={"weights": self.zw(topo, "1", "0"), "_version": stale}),
            409,
            "unsupported_weighted_routing_state_exception",
            reason=f"requested version is {stale} but cluster weighted routing metadata is at a different version {current} ",
        )
        assert _weights(c)["weights"] == self.zw(topo, "1.0", "1.0")

    def test_future_version_conflict(self, topo, weighted):
        c = weighted["client"]
        current = _weights(c)["_version"]
        _err(
            c.put(WEIGHTS, json={"weights": self.zw(topo, "1", "0"), "_version": current + 5}),
            409,
            "unsupported_weighted_routing_state_exception",
            reason=f"requested version is {current + 5} but cluster weighted routing metadata is at a different version {current} ",
        )

    def test_version_minus_one_only_for_fresh_metadata(self, topo, weighted):
        c = weighted["client"]
        current = _weights(c)["_version"]
        if current == -1:
            pytest.skip("weighted routing metadata has never been written on this cluster")
        _err(
            c.put(WEIGHTS, json={"weights": self.zw(topo, "1", "0"), "_version": -1}),
            409,
            "unsupported_weighted_routing_state_exception",
            contains="requested version is -1 but cluster weighted routing metadata is at a different version",
        )

    def test_version_missing(self, topo, weighted):
        _err(
            weighted["client"].put(WEIGHTS, json={"weights": self.zw(topo, "1", "0")}),
            400,
            "action_request_validation_exception",
            reason="Validation Failed: 1: Version is missing;",
        )

    @pytest.mark.parametrize("body", [{"weights": {}}, {}])
    def test_weights_missing(self, weighted, body):
        _err(
            weighted["client"].put(WEIGHTS, json={**body, "_version": _weights(weighted["client"])["_version"]}),
            400,
            "action_request_validation_exception",
            reason="Validation Failed: 1: Weights are missing;",
        )

    def test_two_zero_weights_rejected(self, topo, weighted):
        err = _err(
            weighted["client"].put(WEIGHTS, json={"weights": self.zw(topo, "0", "0"), "_version": 0}),
            400,
            "action_request_validation_exception",
        )
        assert err["reason"].startswith("Validation Failed: 1: There are too many attribute values [")
        assert err["reason"].endswith(
            "given zero weight [2]. Maximum expected number of routing weights having zero weight is [1];"
        )

    def test_non_numeric_weight(self, topo, weighted):
        _err(
            weighted["client"].put(WEIGHTS, json={"weights": self.zw(topo, "abc", "1"), "_version": 0}),
            400,
            "number_format_exception",
            reason='For input string: "abc"',
        )

    def test_json_number_weights_rejected(self, topo, weighted):
        _err(
            weighted["client"].put(WEIGHTS, json={"weights": self.zw(topo, 1, 0), "_version": 0}),
            400,
            "parse_exception",
            reason="failed to parse weighted routing request attribute [null], unknown type",
        )

    def test_string_version_rejected(self, topo, weighted):
        _err(
            weighted["client"].put(WEIGHTS, json={"weights": self.zw(topo, "1", "0"), "_version": "x"}),
            400,
            "parse_exception",
            reason=f"failed to parse weighted routing request [{ATTR}], unknown type",
        )

    def test_zone_with_nodes_must_have_weight(self, topo, weighted):
        version = _weights(weighted["client"])["_version"]
        for weights in ({topo.primary_zone: "1"}, {topo.primary_zone: "1", "dc9": "1"}):
            _err(
                weighted["client"].put(WEIGHTS, json={"weights": weights, "_version": version}),
                409,
                "unsupported_weighted_routing_state_exception",
                reason=f"weight for [{topo.secondary_zone}] is not set and it is part of forced awareness value or a routing node has this attribute.",
            )

    def test_unknown_awareness_attribute(self, topo, weighted):
        for method in ("GET", "PUT"):
            kwargs = {"json": {"weights": self.zw(topo, "1", "0"), "_version": 0}} if method == "PUT" else {}
            _err(
                _cluster_manager(topo).client.request(method, "/_cluster/routing/awareness/rack/weights", **kwargs),
                400,
                "action_request_validation_exception",
                reason="Validation Failed: 1: invalid awareness attribute rack requested for weighted routing;",
            )

    def test_delete_bumps_version_and_empties(self, topo, weighted):
        c = weighted["client"]
        _put_weights(c, self.zw(topo, "1", "0"))
        version = _weights(c)["_version"]
        assert _ok(c.request("DELETE", WEIGHTS, json={"_version": version})) == {"acknowledged": True}
        assert _weights(c) == {"weights": {}, "_version": version + 1, "discovered_cluster_manager": True}

    def test_delete_when_empty_is_404(self, weighted):
        c = weighted["client"]
        _err(
            c.request("DELETE", WEIGHTS, json={"_version": _weights(c)["_version"]}),
            404,
            "resource_not_found_exception",
            reason=f"weighted routing metadata does not have weights set for awareness attribute {ATTR}",
        )

    def test_delete_without_version_conflicts(self, topo, weighted):
        c = weighted["client"]
        _put_weights(c, self.zw(topo, "1", "0"))
        current = _weights(c)["_version"]
        _err(
            c.request("DELETE", WEIGHTS),
            409,
            "unsupported_weighted_routing_state_exception",
            reason=f"requested version is -2 but cluster weighted routing metadata is at a different version {current} ",
        )
        _err(
            c.request("DELETE", WEIGHTS, json={"_version": current - 1}),
            409,
            "unsupported_weighted_routing_state_exception",
        )
        assert _weights(c)["weights"] == self.zw(topo, "1.0", "0.0")

    def test_delete_all_attributes_endpoint(self, topo, weighted):
        c = weighted["client"]
        _put_weights(c, self.zw(topo, "1", "0"))
        version = _weights(c)["_version"]
        assert _ok(c.request("DELETE", "/_cluster/routing/awareness/weights", json={"_version": version})) == {
            "acknowledged": True
        }
        assert _weights(c)["weights"] == {}
        r = c.get("/_cluster/routing/awareness/weights")
        assert r.status_code == 405
        assert r.json() == {
            "error": "Incorrect HTTP method for uri [/_cluster/routing/awareness/weights] and method [GET], allowed: [DELETE]",
            "status": 405,
        }


@needs_cluster
class TestWeightedSearch:
    @pytest.fixture(autouse=True)
    def _weights_on(self, topo, weighted):
        c = weighted["client"]
        _clear_weights(c)
        _put_weights(c, {topo.primary_zone: "1", topo.secondary_zone: "0"})
        yield
        _put_settings(c, transient={"cluster.routing.weighted.*": None, "cluster.routing.ignore_weighted_routing": None})
        _clear_weights(c)

    def test_searches_avoid_zero_weight_zone_from_every_coordinator(self, topo, weighted):
        allowed = topo.ids(topo.primary_zone)
        for node in topo.nodes.values():
            for _ in range(3):
                body, served = _served_by(node.client, weighted["index"])
                assert body["hits"]["total"]["value"] == 40
                assert body["_shards"] == {"total": 4, "successful": 4, "skipped": 0, "failed": 0}
                assert served <= allowed, (node.name, served)

    def test_flipping_weights_moves_traffic(self, topo, weighted):
        c = weighted["client"]
        _put_weights(c, {topo.primary_zone: "0", topo.secondary_zone: "1"})
        for _ in range(3):
            _, served = _served_by(c, weighted["index"])
            assert served <= topo.ids(topo.secondary_zone)

    def test_aggregations_complete_under_weights(self, weighted):
        body = _ok(
            weighted["client"].post(
                f"/{weighted['index']}/_search",
                json={"size": 0, "aggs": {"t": {"terms": {"field": "tag.keyword"}}, "s": {"sum": {"field": "n"}}}},
            )
        )
        assert {b["key"]: b["doc_count"] for b in body["aggregations"]["t"]["buckets"]} == {"even": 20, "odd": 20}
        assert body["aggregations"]["s"]["value"] == sum(range(40))

    def test_custom_string_preference_respects_weights(self, topo, weighted):
        for pref in ("abc", "session-1", "xyz"):
            _, served = _served_by(weighted["client"], weighted["index"], preference=pref)
            assert served <= topo.ids(topo.primary_zone)

    def test_shards_preference_respects_weights(self, topo, weighted):
        for shard in range(4):
            body, served = _served_by(weighted["client"], weighted["index"], preference=f"_shards:{shard}")
            assert body["_shards"]["total"] == 1
            assert served <= topo.ids(topo.primary_zone)

    @pytest.mark.parametrize("kind", ["_only_nodes", "_prefer_nodes"])
    def test_node_preference_rejected_when_strict(self, topo, weighted, kind):
        names = ",".join(n.name for n in topo.zone_nodes(topo.secondary_zone))
        for pref in (f"{kind}:{names}", f"{kind}:{ATTR}:{topo.secondary_zone}"):
            _err(
                weighted["client"].get(f"/{weighted['index']}/_search", params={"preference": pref}),
                403,
                "preference_based_search_not_allowed_exception",
                reason="Preference type based routing not allowed with strict weighted shard routing enabled",
            )

    @pytest.mark.parametrize("spec", ["names", "attribute"])
    def test_only_nodes_allowed_when_not_strict(self, topo, weighted, spec):
        c = weighted["client"]
        _put_settings(c, transient={"cluster.routing.weighted.strict": False})
        if spec == "names":
            target = ",".join(n.name for n in topo.zone_nodes(topo.secondary_zone))
        else:
            target = f"{ATTR}:{topo.secondary_zone}"
        body, served = _served_by(c, weighted["index"], preference=f"_only_nodes:{target}")
        assert body["_shards"]["failed"] == 0
        assert served <= topo.ids(topo.secondary_zone)

    def test_only_nodes_allowed_when_weights_ignored(self, topo, weighted):
        c = weighted["client"]
        _put_settings(c, transient={"cluster.routing.ignore_weighted_routing": True})
        names = ",".join(n.name for n in topo.zone_nodes(topo.secondary_zone))
        body, served = _served_by(c, weighted["index"], preference=f"_only_nodes:{names}")
        assert body["hits"]["total"]["value"] == 40
        assert served <= topo.ids(topo.secondary_zone)

    def test_primary_and_replica_preferences_ignore_weights(self, topo, weighted):
        c = weighted["client"]
        layout = _layout(c, weighted["index"])
        ids = {n.name: n.id for n in topo.nodes.values()}
        primaries = {ids[x["node"]] for copies in layout.values() for x in copies if x["prirep"] == "p"}
        replicas = {ids[x["node"]] for copies in layout.values() for x in copies if x["prirep"] == "r"}
        assert _served_by(c, weighted["index"], preference="_primary")[1] == primaries
        assert _served_by(c, weighted["index"], preference="_replica")[1] == replicas

    def test_local_preference_on_weighed_away_node_uses_local_copies(self, topo, weighted):
        node = topo.zone_nodes(topo.secondary_zone)[0]
        layout = _layout(node.client, weighted["index"])
        local = [s for s, copies in layout.items() if any(x["node"] == node.name for x in copies)]
        assert local
        body, served = _served_by(node.client, weighted["index"], preference="_local")
        assert body["_shards"]["successful"] == 4
        assert node.id in served

    def test_only_local_on_weighed_away_node(self, topo, weighted):
        node = topo.zone_nodes(topo.secondary_zone)[0]
        layout = _layout(node.client, weighted["index"])
        local = [s for s, copies in layout.items() if any(x["node"] == node.name for x in copies)]
        body, served = _served_by(node.client, weighted["index"], preference="_only_local")
        assert served == {node.id}
        assert body["_shards"] == {"total": 4, "successful": len(local), "skipped": 0, "failed": 0}

    def test_ensure_node_weighed_in_health(self, topo, weighted):
        away = topo.zone_nodes(topo.secondary_zone)[0].client
        _err(
            away.get("/_cluster/health", params={"local": "true", "ensure_node_weighed_in": "true"}),
            421,
            "node_weighed_away_exception",
            reason="local node is weighed away",
        )
        inside = topo.zone_nodes(topo.primary_zone)[0].client
        assert _ok(inside.get("/_cluster/health", params={"local": "true", "ensure_node_weighed_in": "true"}))[
            "number_of_nodes"
        ] == len(topo.nodes)

    def test_ensure_node_weighed_in_requires_local(self, topo):
        _err(
            topo.client().get("/_cluster/health", params={"ensure_node_weighed_in": "true"}),
            400,
            "action_request_validation_exception",
            reason="Validation Failed: 1: not a local request to ensure local node commissioned or weighed in;",
        )


# =================================================================== decommission


@needs_cluster
class TestDecommissionPreconditions:
    @pytest.fixture(autouse=True)
    def _clean(self, topo):
        _reset_routing(topo.client())
        yield
        _reset_routing(topo.client())

    def prefix(self, value, attr=ATTR):
        return f"[DecommissionAttribute{{attributeName='{attr}', attributeValue='{value}'}}] "

    def aware(self, topo, force=True):
        settings = {"cluster.routing.allocation.awareness.attributes": ATTR}
        if force:
            settings[f"cluster.routing.allocation.awareness.force.{ATTR}.values"] = f"{topo.primary_zone},{topo.secondary_zone}"
        _put_settings(topo.client(), persistent=settings)

    def test_status_empty_when_nothing_decommissioned(self, topo):
        assert _ok(topo.client().get(f"{DECOM}/{ATTR}/_status")) == {}
        assert _ok(topo.client().get(f"{DECOM}/rack/_status")) == {}

    def test_recommission_without_decommission_is_acknowledged(self, topo):
        assert _ok(topo.client().request("DELETE", DECOM)) == {"acknowledged": True}

    def test_unknown_attribute(self, topo):
        self.aware(topo)
        _err(
            topo.client().put(f"{DECOM}/rack/{topo.secondary_zone}"),
            400,
            "decommissioning_failed_exception",
            reason=self.prefix(topo.secondary_zone, "rack") + "invalid awareness attribute requested for decommissioning",
        )

    def test_requires_forced_awareness(self, topo):
        self.aware(topo, force=False)
        _err(
            topo.client().put(f"{DECOM}/{ATTR}/{topo.secondary_zone}"),
            400,
            "decommissioning_failed_exception",
            reason=self.prefix(topo.secondary_zone) + "forced awareness attribute [{}] doesn't have the decommissioning attribute",
        )

    def test_requires_weights(self, topo):
        self.aware(topo)
        for value in (topo.secondary_zone, "dc9"):
            _err(
                topo.client().put(f"{DECOM}/{ATTR}/{value}"),
                400,
                "decommissioning_failed_exception",
                reason=self.prefix(value) + f"no weights are specified to attribute [{ATTR}]",
            )

    def test_requires_zero_weight(self, topo):
        self.aware(topo)
        c = topo.client()
        _put_weights(c, {topo.primary_zone: "0", topo.secondary_zone: "1"})
        _err(
            c.put(f"{DECOM}/{ATTR}/{topo.secondary_zone}"),
            400,
            "decommissioning_failed_exception",
            reason=self.prefix(topo.secondary_zone) + "weight for decommissioned attribute is expected to be [0.0] but found [1.0]",
        )
        assert _ok(c.get(f"{DECOM}/{ATTR}/_status")) == {}

    def test_no_delay_with_delay_timeout(self, topo):
        self.aware(topo)
        _put_weights(topo.client(), {topo.primary_zone: "1", topo.secondary_zone: "0"})
        _err(
            topo.client().put(f"{DECOM}/{ATTR}/{topo.secondary_zone}", params={"no_delay": "true", "delay_timeout": "5s"}),
            400,
            "action_request_validation_exception",
            contains="Invalid decommission request. no_delay is true and delay_timeout is set to",
        )

    def test_bad_delay_timeout(self, topo):
        _err(
            topo.client().put(f"{DECOM}/{ATTR}/{topo.secondary_zone}", params={"delay_timeout": "abc"}),
            400,
            "illegal_argument_exception",
            reason="failed to parse setting [delay_timeout] with value [abc] as a time value: unit is missing or unrecognized",
        )

    def test_missing_value_has_no_handler(self, topo):
        r = topo.client().put(f"{DECOM}/{ATTR}/")
        assert r.status_code == 400
        assert r.json() == {"error": f"no handler found for uri [{DECOM}/{ATTR}/] and method [PUT]"}


def _wait_decommission(client, zone, timeout=180):
    seen = []
    deadline = time.time() + timeout
    while time.time() < deadline:
        status = _ok(client.get(f"{DECOM}/{ATTR}/_status")).get(zone)
        if status and (not seen or seen[-1] != status):
            seen.append(status)
        if status in ("SUCCESSFUL", "FAILED"):
            return seen
        time.sleep(0.2)
    raise AssertionError(f"decommission of {zone} did not finish: {seen}")


@pytest.fixture(scope="class")
def decommissioned(topo):
    c = topo.client()
    _reset_routing(c)
    z1, z2 = topo.primary_zone, topo.secondary_zone
    _put_settings(
        c,
        persistent={
            "cluster.routing.allocation.awareness.attributes": ATTR,
            f"cluster.routing.allocation.awareness.force.{ATTR}.values": f"{z1},{z2}",
        },
    )
    idx = _create(c, topo.index("decom"), 3, 1)
    _bulk(c, idx, [(str(i), {"n": i}) for i in range(30)])
    _put_weights(c, {z1: "1", z2: "0"})
    state = {"client": c, "index": idx}
    try:
        state["put"] = c.put(f"{DECOM}/{ATTR}/{z2}", params={"no_delay": "true"})
        state["statuses"] = _wait_decommission(c, z2)
        # the decommissioned nodes notice they were removed a few seconds after SUCCESSFUL
        deadline = time.time() + 120
        for node in topo.zone_nodes(z2):
            while time.time() < deadline:
                local = _ok(node.client.get("/_cluster/health", params={"local": "true"}))
                if not local["discovered_cluster_manager"]:
                    break
                time.sleep(0.5)
        yield state
    finally:
        c.request("DELETE", DECOM)
        if CTL:
            for n in topo.zone_nodes(z2):
                _ctl("stop-node", n.name)
                _ctl("start-node", n.name)
        _wait_healthy(topo)
        _reset_routing(c)
        _delete_indices(c, [idx])


@needs_cluster
class TestDecommissionZone:
    def test_put_acknowledged(self, decommissioned):
        assert _ok(decommissioned["put"]) == {"acknowledged": True}

    def test_status_transitions_end_successful(self, decommissioned):
        seen = decommissioned["statuses"]
        assert seen[-1] == "SUCCESSFUL", seen
        order = ["INIT", "DRAINING", "IN_PROGRESS", "SUCCESSFUL"]
        assert all(s in order for s in seen), seen
        assert [order.index(s) for s in seen] == sorted(order.index(s) for s in seen)

    def test_status_shape(self, topo, decommissioned):
        assert _ok(decommissioned["client"].get(f"{DECOM}/{ATTR}/_status")) == {topo.secondary_zone: "SUCCESSFUL"}

    def test_metadata_records_decommission(self, topo, decommissioned):
        state = _ok(
            decommissioned["client"].get("/_cluster/state/metadata", params={"filter_path": "metadata.decommissionedAttribute"})
        )
        attr = state["metadata"]["decommissionedAttribute"]
        assert attr["awareness"] == {ATTR: topo.secondary_zone}
        assert attr["status"] == "successful"
        assert isinstance(attr["requestID"], str) and attr["requestID"]

    def test_zone_nodes_left_the_cluster(self, topo, decommissioned):
        rows = _ok(decommissioned["client"].get("/_cat/nodes", params={"format": "json", "h": "name"}))
        assert {r["name"] for r in rows} == {n.name for n in topo.zones[topo.primary_zone]}

    def test_cluster_manager_in_remaining_zone(self, topo, decommissioned):
        (row,) = _ok(decommissioned["client"].get("/_cat/cluster_manager", params={"format": "json"}))
        assert topo.zone_of(row["node"]) == topo.primary_zone

    def test_voters_only_in_remaining_zone(self, topo, decommissioned):
        state = _ok(
            decommissioned["client"].get(
                "/_cluster/state/metadata", params={"filter_path": "metadata.cluster_coordination.last_committed_config"}
            )
        )
        voters = set(state["metadata"]["cluster_coordination"]["last_committed_config"])
        assert voters and voters <= topo.ids(topo.primary_zone)

    def test_health_yellow_replicas_unassigned(self, decommissioned):
        c, idx = decommissioned["client"], decommissioned["index"]
        health = _wait_index(c, idx, "yellow")
        assert health["status"] == "yellow"
        assert health["active_primary_shards"] == 3
        assert health["unassigned_shards"] == 3

    def test_data_still_available(self, decommissioned):
        c, idx = decommissioned["client"], decommissioned["index"]
        assert _ok(c.get(f"/{idx}/_count"))["count"] >= 30
        docs = _ok(c.post(f"/{idx}/_mget", json={"ids": [str(i) for i in range(30)]}))["docs"]
        assert all(d["found"] for d in docs)

    def test_writes_succeed(self, decommissioned):
        c, idx = decommissioned["client"], decommissioned["index"]
        body = _ok(c.put(f"/{idx}/_doc/during", params={"refresh": "true"}, json={"n": -1}), 201)
        assert body["_shards"] == {"total": 2, "successful": 1, "failed": 0}

    def test_decommissioned_nodes_are_running_but_detached(self, topo, decommissioned):
        for node in topo.zone_nodes(topo.secondary_zone):
            assert _ok(node.client.get("/"))["name"] == node.name
            _err(
                node.client.get("/_cluster/health", params={"timeout": "1s"}),
                503,
                "cluster_manager_not_discovered_exception",
            )
            local = _ok(node.client.get("/_cluster/health", params={"local": "true"}))
            assert local["discovered_cluster_manager"] is False

    def test_same_request_again_rejected(self, topo, decommissioned):
        z = topo.secondary_zone
        _err(
            decommissioned["client"].put(f"{DECOM}/{ATTR}/{z}"),
            400,
            "decommissioning_failed_exception",
            reason=f"[DecommissionAttribute{{attributeName='{ATTR}', attributeValue='{z}'}}] same request is already in status [SUCCESSFUL]",
        )

    def test_other_zone_rejected_until_recommission(self, topo, decommissioned):
        z1, z2 = topo.primary_zone, topo.secondary_zone
        _err(
            decommissioned["client"].put(f"{DECOM}/{ATTR}/{z1}"),
            400,
            "decommissioning_failed_exception",
            reason=(
                f"[DecommissionAttribute{{attributeName='{ATTR}', attributeValue='{z1}'}}] one awareness attribute "
                f"[DecommissionAttribute{{attributeName='{ATTR}', attributeValue='{z2}'}}] already successfully "
                "decommissioned, recommission before triggering another decommission"
            ),
        )

    def test_weights_cannot_bring_zone_back(self, topo, decommissioned):
        c = decommissioned["client"]
        version = _weights(c)["_version"]
        _err(
            c.put(WEIGHTS, json={"weights": {topo.primary_zone: "1", topo.secondary_zone: "1"}, "_version": version}),
            409,
            "unsupported_weighted_routing_state_exception",
            reason=f"weight for [{topo.secondary_zone}] must be set to [0.0] as it is under decommission action",
        )
        assert _weights(c)["weights"][topo.secondary_zone] == "0.0"

    def test_zz_recommission_restores_cluster(self, topo, decommissioned):
        c, idx = decommissioned["client"], decommissioned["index"]
        assert _ok(c.request("DELETE", DECOM)) == {"acknowledged": True}
        assert _ok(c.get(f"{DECOM}/{ATTR}/_status")) == {}
        state = _ok(c.get("/_cluster/state/metadata", params={"filter_path": "metadata.decommissionedAttribute"}))
        assert state == {}
        # recommissioned nodes retry discovery only every
        # discovery.find_peers_interval_during_decommission (120s); restart them when possible
        if CTL:
            for n in topo.zone_nodes(topo.secondary_zone):
                _ctl("stop-node", n.name)
                _ctl("start-node", n.name)
        _wait_healthy(topo)
        for copies in _layout(c, idx).values():
            assert {topo.zone_of(x["node"]) for x in copies} == {topo.primary_zone, topo.secondary_zone}
        assert _ok(c.get(f"/{idx}/_count"))["count"] == 31


# =================================================================== whole-DC failure


@pytest.fixture(scope="class")
def secondary_outage(topo):
    c = topo.client()
    _reset_routing(c)
    z1, z2 = topo.primary_zone, topo.secondary_zone
    _put_settings(
        c,
        persistent={
            "cluster.routing.allocation.awareness.attributes": ATTR,
            f"cluster.routing.allocation.awareness.force.{ATTR}.values": f"{z1},{z2}",
        },
    )
    idx = _create(c, topo.index("dc2down"), 3, 1)
    _bulk(c, idx, [(str(i), {"n": i}) for i in range(30)])
    try:
        for n in topo.zone_nodes(z2):
            _ctl("kill-node", n.name)
        r = c.get("/_cluster/health", params={"wait_for_nodes": str(len(topo.zones[z1])), "timeout": "60s"})
        _ok(r)
        _wait_index(c, idx, "yellow")
        yield {"client": c, "index": idx}
    finally:
        _restore_all(topo)
        _reset_routing(c)
        _delete_indices(c, [idx])


@needs_ctl
class TestSecondaryDatacenterOutage:
    def test_cluster_keeps_quorum(self, topo, secondary_outage):
        c = secondary_outage["client"]
        rows = _ok(c.get("/_cat/nodes", params={"format": "json", "h": "name"}))
        assert {r["name"] for r in rows} == {n.name for n in topo.zones[topo.primary_zone]}
        (row,) = _ok(c.get("/_cat/cluster_manager", params={"format": "json"}))
        assert topo.zone_of(row["node"]) == topo.primary_zone

    def test_health_yellow_with_replicas_unassigned(self, topo, secondary_outage):
        c, idx = secondary_outage["client"], secondary_outage["index"]
        health = _ok(c.get(f"/_cluster/health/{idx}"))
        assert health["status"] == "yellow"
        assert health["number_of_nodes"] == len(topo.zones[topo.primary_zone])
        assert health["active_primary_shards"] == 3
        assert health["unassigned_shards"] == 3

    def test_replica_unassigned_because_of_node_left_and_awareness(self, secondary_outage):
        c, idx = secondary_outage["client"], secondary_outage["index"]
        explain = _ok(c.post("/_cluster/allocation/explain", json={"index": idx, "shard": 0, "primary": False}))
        assert explain["unassigned_info"]["reason"] == "NODE_LEFT"
        assert explain["can_allocate"] == "no"
        for d in explain["node_allocation_decisions"]:
            assert any(x["decider"] == "awareness" and x["decision"] == "NO" for x in d["deciders"]), d

    def test_all_data_readable_from_every_survivor(self, topo, secondary_outage):
        idx = secondary_outage["index"]
        for node in topo.zone_nodes(topo.primary_zone):
            body = _ok(node.client.get(f"/{idx}/_search", params={"size": 0}))
            assert body["hits"]["total"]["value"] >= 30
            assert body["_shards"]["failed"] == 0
            docs = _ok(node.client.post(f"/{idx}/_mget", json={"ids": [str(i) for i in range(30)]}))["docs"]
            assert all(d["found"] for d in docs)

    def test_writes_succeed_during_outage(self, secondary_outage):
        c, idx = secondary_outage["client"], secondary_outage["index"]
        body = _bulk(c, idx, [(f"out-{i}", {"n": 100 + i}) for i in range(10)])
        for item in body["items"]:
            assert item["index"]["_shards"] == {"total": 2, "successful": 1, "failed": 0}
        assert _ok(c.get(f"/{idx}/_count"))["count"] == 40

    def test_wait_for_all_active_shards_times_out(self, secondary_outage):
        c, idx = secondary_outage["client"], secondary_outage["index"]
        err = _err(
            c.put(f"/{idx}/_doc/strict", params={"wait_for_active_shards": "all", "timeout": "1s"}, json={"n": 0}),
            503,
            "unavailable_shards_exception",
        )
        assert "Not enough active copies to meet shard count of [ALL] (have 1, needed 2)" in err["reason"]
        assert c.get(f"/{idx}/_doc/strict").json()["found"] is False

    def test_zz_datacenter_returns_and_catches_up(self, topo, secondary_outage):
        c, idx = secondary_outage["client"], secondary_outage["index"]
        for n in topo.zone_nodes(topo.secondary_zone):
            _ctl("start-node", n.name)
        _wait_healthy(topo)
        dc2 = ",".join(n.name for n in topo.zone_nodes(topo.secondary_zone))
        for copies in _layout(c, idx).values():
            assert {topo.zone_of(x["node"]) for x in copies} == {topo.primary_zone, topo.secondary_zone}
        body = _ok(c.get(f"/{idx}/_search", params={"size": 0, "preference": f"_only_nodes:{dc2}"}))
        assert body["hits"]["total"]["value"] == 40
        got = _ok(c.get(f"/{idx}/_doc/out-9", params={"preference": f"_only_nodes:{dc2}", "realtime": "false"}))
        assert got["_source"] == {"n": 109}
        stats = _ok(c.get(f"/{idx}/_stats", params={"level": "shards"}))["indices"][idx]["shards"]
        for copies in stats.values():
            assert len(copies) == 2
            assert copies[0]["seq_no"]["max_seq_no"] == copies[1]["seq_no"]["max_seq_no"]
            assert copies[0]["docs"]["count"] == copies[1]["docs"]["count"]


@pytest.fixture(scope="class")
def primary_outage(topo):
    c = topo.client()
    _reset_routing(c)
    _put_settings(c, persistent={"cluster.routing.allocation.awareness.attributes": ATTR})
    idx = _create(c, topo.index("dc1down"), 3, 1)
    _bulk(c, idx, [(str(i), {"n": i}) for i in range(20)])
    survivors = topo.zone_nodes(topo.secondary_zone)
    try:
        for n in topo.zone_nodes(topo.primary_zone):
            _ctl("kill-node", n.name)
        deadline = time.time() + 60
        while time.time() < deadline:
            local = [_ok(s.client.get("/_cluster/health", params={"local": "true"})) for s in survivors]
            if all(not h["discovered_cluster_manager"] for h in local):
                break
            time.sleep(0.5)
        yield {"index": idx, "survivors": survivors}
    finally:
        _restore_all(topo)
        _reset_routing(c)
        _delete_indices(c, [idx])


@needs_ctl
class TestPrimaryDatacenterOutage:
    def test_health_is_503_without_cluster_manager(self, primary_outage):
        for node in primary_outage["survivors"]:
            r = node.client.get("/_cluster/health", params={"timeout": "2s"})
            assert r.status_code == 503, r.text
            assert r.json() == {
                "error": {
                    "root_cause": [{"type": "cluster_manager_not_discovered_exception", "reason": None}],
                    "type": "cluster_manager_not_discovered_exception",
                    "reason": None,
                },
                "status": 503,
            }

    def test_local_health_reports_missing_cluster_manager(self, primary_outage):
        for node in primary_outage["survivors"]:
            h = _ok(node.client.get("/_cluster/health", params={"local": "true"}))
            assert h["discovered_cluster_manager"] is False
            assert h["discovered_master"] is False
            assert h["status"] == "red"

    def test_reads_still_served(self, primary_outage):
        idx = primary_outage["index"]
        for node in primary_outage["survivors"]:
            body = _ok(node.client.get(f"/{idx}/_search", params={"size": 0}))
            assert body["hits"]["total"]["value"] >= 20
            assert body["_shards"] == {"total": 3, "successful": 3, "skipped": 0, "failed": 0}
            assert _ok(node.client.get(f"/{idx}/_doc/7"))["_source"] == {"n": 7}

    def test_cluster_manager_actions_fail(self, primary_outage):
        node = primary_outage["survivors"][0]
        params = {"cluster_manager_timeout": "2s", "timeout": "2s"}
        _err(node.client.put(f"/{PFX}-never", params=params), 503, "cluster_manager_not_discovered_exception")
        _err(
            node.client.put("/_cluster/settings", params=params, json={"transient": {"cluster.routing.allocation.enable": "all"}}),
            503,
            "cluster_manager_not_discovered_exception",
        )
        _err(node.client.get("/_cat/nodes", params={"cluster_manager_timeout": "2s"}), 503, "cluster_manager_not_discovered_exception")

    def test_writes_do_not_succeed(self, primary_outage):
        node = primary_outage["survivors"][0]
        try:
            r = node.client.put(
                f"/{primary_outage['index']}/_doc/lost", params={"timeout": "1s"}, json={"n": -1}, timeout=8
            )
        except httpx.TimeoutException:
            return
        assert r.status_code >= 500, r.text

    def test_zz_datacenter_returns(self, topo, primary_outage):
        for n in topo.zone_nodes(topo.primary_zone):
            _ctl("start-node", n.name)
        _wait_healthy(topo)
        c = topo.client()
        idx = primary_outage["index"]
        docs = _ok(c.post(f"/{idx}/_mget", json={"ids": [str(i) for i in range(20)]}))["docs"]
        assert all(d["found"] for d in docs)
        assert _ok(c.get("/_cluster/health"))["discovered_cluster_manager"] is True


# =================================================================== cross-cluster search

ALIAS = "xdc"
PROXY = "xdcp"
LOCAL_DOCS = [("l1", {"t": "a", "v": 10}), ("l2", {"t": "c", "v": 20}), ("l3", {"t": "a", "v": 30}), ("l4", {"t": "d", "v": 40})]
REMOTE_DOCS = [("r1", {"t": "a", "v": 1}), ("r2", {"t": "b", "v": 2}), ("r3", {"t": "a", "v": 3})]
MAPPINGS = {"properties": {"t": {"type": "keyword"}, "v": {"type": "long"}}}


def _remote_info(client, alias, connected=True, timeout=30):
    deadline = time.time() + timeout
    while True:
        info = _ok(client.get("/_remote/info")).get(alias)
        if info is not None and info.get("connected") == connected:
            return info
        if time.time() > deadline:
            return info
        time.sleep(0.3)


def _wait_remote_green(remote):
    deadline = time.time() + RECOVERY_TIMEOUT
    while time.time() < deadline:
        try:
            r = remote.get("/_cluster/health", params={"wait_for_status": "green", "timeout": "5s"})
            if r.status_code == 200 and not r.json()["timed_out"]:
                return
        except httpx.HTTPError:
            pass
        time.sleep(1)
    raise AssertionError("remote cluster did not come back green")


@pytest.fixture(scope="class")
def ccs(topo):
    main = topo.client()
    remote = httpx.Client(base_url=REMOTE_URL, timeout=TIMEOUT)
    nulls = {"cluster.remote.*": None}
    for c in (main, remote):
        _put_settings(c, persistent=nulls, transient=nulls)
    ((_, rnode),) = _ok(remote.get("/_nodes/_local/transport")).get("nodes").items()
    raddr = rnode["transport"]["publish_address"]
    idx = f"{PFX}-ccs"
    extra = f"{PFX}-ccs-extra"
    local_only = f"{PFX}-ccs-local"
    topo.created += [idx, local_only]
    try:
        _create(main, idx, 1, 1, MAPPINGS)
        _create(main, local_only, 1, 0, MAPPINGS)
        _create(remote, idx, 1, 0, MAPPINGS)
        _create(remote, extra, 1, 0, MAPPINGS)
        _bulk(main, idx, LOCAL_DOCS)
        _bulk(main, local_only, [("x1", {"t": "z", "v": 1000})])
        _bulk(remote, idx, REMOTE_DOCS)
        _bulk(remote, extra, [("e1", {"t": "b", "v": 100})])
        _put_settings(
            main,
            persistent={
                f"cluster.remote.{ALIAS}.seeds": [raddr],
                f"cluster.remote.{PROXY}.mode": "proxy",
                f"cluster.remote.{PROXY}.proxy_address": raddr,
            },
        )
        _remote_info(main, ALIAS)
        _remote_info(main, PROXY)
        yield {
            "main": main,
            "remote": remote,
            "raddr": raddr,
            "rname": rnode["name"],
            "idx": idx,
            "extra": extra,
            "local_only": local_only,
        }
    finally:
        for c in (main, remote):
            try:
                _put_settings(c, persistent=nulls, transient=nulls)
            except (httpx.HTTPError, AssertionError):
                pass
        _delete_indices(remote, [idx, extra])
        _delete_indices(main, [idx, local_only])
        remote.close()


def _ids(body):
    return [h["_id"] for h in body["hits"]["hits"]]


def _search(client, target, body=None, **params):
    return _ok(client.post(f"/{target}/_search", params=params, json=body or {}))


@needs_remote
class TestCrossClusterSearch:
    def test_remote_info_sniff(self, ccs):
        info = _remote_info(ccs["main"], ALIAS)
        assert info == {
            "connected": True,
            "mode": "sniff",
            "seeds": [ccs["raddr"]],
            "num_nodes_connected": 1,
            "max_connections_per_cluster": 3,
            "initial_connect_timeout": "30s",
            "skip_unavailable": False,
        }

    def test_remote_info_proxy(self, ccs):
        info = _remote_info(ccs["main"], PROXY)
        assert info == {
            "connected": True,
            "mode": "proxy",
            "proxy_address": ccs["raddr"],
            "server_name": "",
            "num_proxy_sockets_connected": 18,
            "max_proxy_socket_connections": 18,
            "initial_connect_timeout": "30s",
            "skip_unavailable": False,
        }

    def test_remote_info_on_every_node(self, topo, ccs):
        for node in topo.nodes.values():
            info = _remote_info(node.client, ALIAS)
            assert info["connected"] is True and info["mode"] == "sniff", node.name
            assert set(_ok(node.client.get("/_remote/info"))) == {ALIAS, PROXY}

    def test_remote_only_search(self, ccs):
        body = _search(ccs["main"], f"{ALIAS}:{ccs['idx']}", {"sort": [{"v": "asc"}]})
        assert _ids(body) == ["r1", "r2", "r3"]
        assert {h["_index"] for h in body["hits"]["hits"]} == {f"{ALIAS}:{ccs['idx']}"}
        assert body["_clusters"] == {"total": 1, "successful": 1, "skipped": 0}
        assert body["hits"]["total"] == {"value": 3, "relation": "eq"}

    def test_proxy_alias_search(self, ccs):
        body = _search(ccs["main"], f"{PROXY}:{ccs['idx']}", {"sort": [{"v": "asc"}]})
        assert _ids(body) == ["r1", "r2", "r3"]
        assert {h["_index"] for h in body["hits"]["hits"]} == {f"{PROXY}:{ccs['idx']}"}

    def test_local_plus_remote(self, ccs):
        idx = ccs["idx"]
        body = _search(ccs["main"], f"{idx},{ALIAS}:{idx}", {"sort": [{"v": "desc"}], "size": 10})
        assert _ids(body) == ["l4", "l3", "l2", "l1", "r3", "r2", "r1"]
        assert [h["_index"] for h in body["hits"]["hits"]] == [idx] * 4 + [f"{ALIAS}:{idx}"] * 3
        assert body["_clusters"] == {"total": 2, "successful": 2, "skipped": 0}
        assert body["_shards"]["total"] == 2

    def test_three_clusters_same_remote_twice(self, ccs):
        idx = ccs["idx"]
        body = _search(ccs["main"], f"{idx},{ALIAS}:{idx},{PROXY}:{idx}", {"size": 0})
        assert body["hits"]["total"]["value"] == 10
        assert body["_clusters"] == {"total": 3, "successful": 3, "skipped": 0}

    def test_star_cluster_matches_remotes_only(self, ccs):
        idx = ccs["idx"]
        body = _search(ccs["main"], f"*:{idx}", {"size": 20, "sort": [{"v": "asc"}]})
        assert body["_clusters"] == {"total": 2, "successful": 2, "skipped": 0}
        assert sorted({h["_index"] for h in body["hits"]["hits"]}) == sorted([f"{ALIAS}:{idx}", f"{PROXY}:{idx}"])
        assert sorted(_ids(body)) == ["r1", "r1", "r2", "r2", "r3", "r3"]

    def test_alias_wildcard(self, ccs):
        body = _search(ccs["main"], "xd*:" + ccs["idx"], {"size": 0})
        assert body["_clusters"]["total"] == 2
        assert body["hits"]["total"]["value"] == 6

    def test_remote_index_wildcard(self, ccs):
        body = _search(ccs["main"], f"{ALIAS}:{PFX}-ccs*", {"sort": [{"v": "asc"}]})
        assert _ids(body) == ["r1", "r2", "r3", "e1"]
        assert body["hits"]["hits"][-1]["_index"] == f"{ALIAS}:{ccs['extra']}"

    def test_local_wildcard_does_not_reach_remote(self, ccs):
        body = _search(ccs["main"], f"{PFX}-ccs*", {"size": 0})
        assert body["hits"]["total"]["value"] == 5
        assert "_clusters" not in body

    @pytest.mark.parametrize("query", [
        {"sort": [{"v": "desc"}], "size": 10},
        {"query": {"term": {"t": "a"}}, "sort": [{"v": "asc"}]},
        {"query": {"range": {"v": {"gte": 2, "lte": 30}}}, "sort": [{"v": "asc"}], "from": 1, "size": 3},
    ])
    def test_minimize_roundtrips_same_hits(self, ccs, query):
        target = f"{ccs['idx']},{ALIAS}:{ccs['idx']}"
        a = _search(ccs["main"], target, query, ccs_minimize_roundtrips="true")
        b = _search(ccs["main"], target, query, ccs_minimize_roundtrips="false")
        key = lambda body: [(h["_index"], h["_id"], h.get("sort")) for h in body["hits"]["hits"]]  # noqa: E731
        assert key(a) == key(b)
        assert a["hits"]["total"] == b["hits"]["total"]
        assert a["_clusters"] == b["_clusters"] == {"total": 2, "successful": 2, "skipped": 0}

    def test_num_reduce_phases_only_with_minimize_roundtrips(self, ccs):
        target = f"{ccs['idx']},{ALIAS}:{ccs['idx']}"
        assert _search(ccs["main"], target, ccs_minimize_roundtrips="true")["num_reduce_phases"] == 3
        assert "num_reduce_phases" not in _search(ccs["main"], target, ccs_minimize_roundtrips="false")

    def test_term_query_across_clusters(self, ccs):
        body = _search(ccs["main"], f"{ccs['idx']},{ALIAS}:{ccs['idx']}", {"query": {"term": {"t": "a"}}, "sort": [{"v": "asc"}]})
        assert _ids(body) == ["r1", "r3", "l1", "l3"]

    def test_sort_from_size_across_clusters(self, ccs):
        body = _search(ccs["main"], f"{ccs['idx']},{ALIAS}:{ccs['idx']}", {"sort": [{"v": "asc"}], "from": 2, "size": 3})
        assert _ids(body) == ["r3", "l1", "l2"]
        assert [h["sort"] for h in body["hits"]["hits"]] == [[3], [10], [20]]
        assert body["hits"]["total"]["value"] == 7

    @pytest.mark.parametrize("mr", ["true", "false"])
    def test_aggregations_merged(self, ccs, mr):
        body = _search(
            ccs["main"],
            f"{ccs['idx']},{ALIAS}:{ccs['idx']}",
            {
                "size": 0,
                "aggs": {
                    "t": {"terms": {"field": "t"}},
                    "s": {"sum": {"field": "v"}},
                    "mx": {"max": {"field": "v"}},
                    "idx": {"terms": {"field": "_index"}},
                },
            },
            ccs_minimize_roundtrips=mr,
        )
        aggs = body["aggregations"]
        assert [(b["key"], b["doc_count"]) for b in aggs["t"]["buckets"]] == [("a", 4), ("b", 1), ("c", 1), ("d", 1)]
        assert aggs["s"]["value"] == 106.0
        assert aggs["mx"]["value"] == 40.0
        assert {b["key"]: b["doc_count"] for b in aggs["idx"]["buckets"]} == {ccs["idx"]: 4, f"{ALIAS}:{ccs['idx']}": 3}

    def test_count_across_clusters(self, ccs):
        body = _ok(ccs["main"].get(f"/{ccs['idx']},{ALIAS}:{ccs['idx']}/_count"))
        assert body["count"] == 7
        assert body["_shards"]["total"] == 2
        assert "_clusters" not in body
        assert _ok(ccs["main"].get(f"/{ALIAS}:{ccs['idx']}/_count", params={"q": "t:a"}))["count"] == 2

    def test_msearch_with_remote(self, ccs):
        lines = [
            {"index": f"{ALIAS}:{ccs['idx']}"},
            {"sort": [{"v": "asc"}]},
            {"index": f"{ccs['idx']},{ALIAS}:{ccs['idx']}"},
            {"size": 0},
            {"index": f"{ALIAS}:nope-{PFX}"},
            {},
        ]
        body = _ok(ccs["main"].post("/_msearch", content=_ndjson(lines), headers=NDJSON))
        first, second, third = body["responses"]
        assert first["status"] == 200 and _ids(first) == ["r1", "r2", "r3"]
        assert first["_clusters"] == {"total": 1, "successful": 1, "skipped": 0}
        assert second["hits"]["total"]["value"] == 7
        assert second["_clusters"]["total"] == 2
        assert third["status"] == 404
        assert third["error"]["type"] == "index_not_found_exception"
        assert third["error"]["reason"] == f"no such index [nope-{PFX}]"

    def test_field_caps_across_clusters(self, ccs):
        body = _ok(ccs["main"].get(f"/{ALIAS}:{ccs['idx']},{ccs['idx']}/_field_caps", params={"fields": "t,v"}))
        assert sorted(body["indices"]) == sorted([f"{ALIAS}:{ccs['idx']}", ccs["idx"]])
        assert body["fields"] == {
            "t": {"keyword": {"type": "keyword", "searchable": True, "aggregatable": True}},
            "v": {"long": {"type": "long", "searchable": True, "aggregatable": True}},
        }

    def test_resolve_index_across_clusters(self, ccs):
        body = _ok(ccs["main"].get(f"/_resolve/index/{ALIAS}:{PFX}-ccs*,{ccs['idx']}"))
        names = sorted(i["name"] for i in body["indices"])
        assert names == sorted([ccs["idx"], f"{ALIAS}:{ccs['idx']}", f"{ALIAS}:{ccs['extra']}"])
        assert all(i["attributes"] == ["open"] for i in body["indices"])
        assert body["aliases"] == [] and body["data_streams"] == []

    @pytest.mark.parametrize("mr", ["true", "false"])
    def test_missing_remote_index(self, ccs, mr):
        missing = f"nope-{PFX}"
        for target in (f"{ALIAS}:{missing}", f"{ccs['idx']},{ALIAS}:{missing}"):
            err = _err(
                ccs["main"].get(f"/{target}/_search", params={"ccs_minimize_roundtrips": mr}),
                404,
                "index_not_found_exception",
                reason=f"no such index [{missing}]",
            )
            assert err["index"] == missing

    def test_missing_remote_wildcard_matches_nothing(self, ccs):
        body = _search(ccs["main"], f"{ccs['idx']},{ALIAS}:nope-{PFX}*", {"size": 0})
        assert body["hits"]["total"]["value"] == 4
        assert body["_clusters"] == {"total": 2, "successful": 2, "skipped": 0}

    def test_missing_remote_index_ignore_unavailable(self, ccs):
        body = _search(ccs["main"], f"{ALIAS}:nope-{PFX}", ignore_unavailable="true")
        assert body["hits"]["total"]["value"] == 0
        assert body["_shards"] == {"total": 0, "successful": 0, "skipped": 0, "failed": 0}
        assert body["_clusters"] == {"total": 1, "successful": 1, "skipped": 0}

    def test_unknown_alias_is_a_local_index_name(self, ccs):
        target = f"nosuchcluster:{ccs['idx']}"
        _err(
            ccs["main"].get(f"/{target}/_search"),
            404,
            "index_not_found_exception",
            reason=f"no such index [{target}]",
        )

    def test_document_get_does_not_cross_clusters(self, ccs):
        err = _err(
            ccs["main"].get(f"/{ALIAS}:{ccs['idx']}/_doc/r1"),
            404,
            "index_not_found_exception",
            reason=f"no such index [{ALIAS}:{ccs['idx']}]",
        )
        assert err["resource.type"] == "index_expression"

    def test_reverse_direction(self, topo, ccs):
        remote, idx = ccs["remote"], ccs["idx"]
        seed = topo.zone_nodes(topo.primary_zone)[0].transport
        try:
            _put_settings(remote, persistent={"cluster.remote.main.seeds": [seed]})
            info = _remote_info(remote, "main")
            assert info["connected"] is True
            assert info["seeds"] == [seed]
            assert info["num_nodes_connected"] == min(3, len(topo.nodes))
            body = _search(remote, f"{idx},main:{idx}", {"sort": [{"v": "desc"}], "size": 10})
            assert _ids(body) == ["l4", "l3", "l2", "l1", "r3", "r2", "r1"]
            assert [h["_index"] for h in body["hits"]["hits"]] == [f"main:{idx}"] * 4 + [idx] * 3
            assert body["_clusters"] == {"total": 2, "successful": 2, "skipped": 0}
        finally:
            _put_settings(remote, persistent={"cluster.remote.main.seeds": None})
        assert _ok(remote.get("/_remote/info")) == {}


@needs_remote
class TestRemoteClusterSettings:
    @pytest.fixture(autouse=True)
    def _clean_ghost(self, ccs):
        yield
        _put_settings(ccs["main"], persistent={"cluster.remote.ghost.*": None, "cluster.remote.bad.*": None})

    def test_invalid_seed(self, ccs):
        _err(
            ccs["main"].put("/_cluster/settings", json={"persistent": {"cluster.remote.bad.seeds": ["notaport"]}}),
            400,
            "illegal_argument_exception",
            reason="remote hosts need to be configured as [host:port], found [notaport] instead",
        )

    def test_seeds_rejected_in_proxy_mode(self, ccs):
        _err(
            ccs["main"].put(
                "/_cluster/settings",
                json={"persistent": {"cluster.remote.bad.mode": "proxy", "cluster.remote.bad.seeds": [ccs["raddr"]]}},
            ),
            400,
            "illegal_argument_exception",
            reason='Setting "cluster.remote.bad.seeds" cannot be used with the configured "cluster.remote.bad.mode" [required=SNIFF, configured=PROXY]',
        )

    def test_unknown_mode(self, ccs):
        _err(
            ccs["main"].put("/_cluster/settings", json={"persistent": {"cluster.remote.bad.mode": "bogus"}}),
            400,
            "illegal_argument_exception",
            reason="No enum constant org.opensearch.transport.RemoteConnectionStrategy.ConnectionStrategy.BOGUS",
        )

    def test_skip_unavailable_needs_configured_remote(self, ccs):
        _err(
            ccs["main"].put("/_cluster/settings", json={"persistent": {"cluster.remote.bad.skip_unavailable": True}}),
            400,
            "illegal_argument_exception",
            reason="Cannot configure setting [cluster.remote.bad.skip_unavailable] if remote cluster is not enabled.",
        )

    def test_bogus_seeds_info(self, ccs):
        _put_settings(ccs["main"], persistent={"cluster.remote.ghost.seeds": ["127.0.0.1:1"]})
        info = _ok(ccs["main"].get("/_remote/info"))["ghost"]
        assert info == {
            "connected": False,
            "mode": "sniff",
            "seeds": ["127.0.0.1:1"],
            "num_nodes_connected": 0,
            "max_connections_per_cluster": 3,
            "initial_connect_timeout": "30s",
            "skip_unavailable": False,
        }

    @pytest.mark.parametrize("mr", ["true", "false"])
    def test_bogus_seeds_fail_search_without_skip(self, ccs, mr):
        _put_settings(ccs["main"], persistent={"cluster.remote.ghost.seeds": ["127.0.0.1:1"]})
        err = _err(
            ccs["main"].get(f"/{ccs['idx']},ghost:{ccs['idx']}/_search", params={"ccs_minimize_roundtrips": mr}),
            500,
            "connect_transport_exception",
            reason="[][127.0.0.1:1] connect_exception",
        )
        assert err["caused_by"]["reason"] == "Connection refused (connect failed): /127.0.0.1:1"

    @pytest.mark.parametrize("mr", ["true", "false"])
    def test_bogus_seeds_skipped_when_skip_unavailable(self, ccs, mr):
        main, idx = ccs["main"], ccs["idx"]
        _put_settings(main, persistent={"cluster.remote.ghost.seeds": ["127.0.0.1:1"], "cluster.remote.ghost.skip_unavailable": True})
        assert _ok(main.get("/_remote/info"))["ghost"]["skip_unavailable"] is True
        body = _search(main, f"{idx},ghost:{idx}", {"size": 0}, ccs_minimize_roundtrips=mr)
        assert body["_clusters"] == {"total": 2, "successful": 1, "skipped": 1}
        assert body["hits"]["total"]["value"] == 4
        body = _search(main, f"ghost:{idx}", ccs_minimize_roundtrips=mr)
        assert body["_clusters"] == {"total": 1, "successful": 0, "skipped": 1}
        assert body["_shards"] == {"total": 0, "successful": 0, "skipped": 0, "failed": 0}
        assert body["hits"]["total"]["value"] == 0

    def test_bogus_seeds_count_and_msearch(self, ccs):
        main, idx = ccs["main"], ccs["idx"]
        _put_settings(main, persistent={"cluster.remote.ghost.seeds": ["127.0.0.1:1"]})
        _err(main.get(f"/{idx},ghost:{idx}/_count"), 500, "connect_transport_exception")
        body = _ok(main.post("/_msearch", content=_ndjson([{"index": f"ghost:{idx}"}, {}]), headers=NDJSON))
        (item,) = body["responses"]
        assert item["status"] == 500
        assert item["error"]["type"] == "connect_transport_exception"
        _put_settings(main, persistent={"cluster.remote.ghost.skip_unavailable": True})
        assert _ok(main.get(f"/{idx},ghost:{idx}/_count"))["count"] == 4

    def test_field_caps_and_resolve_ignore_unreachable_remote(self, ccs):
        main, idx = ccs["main"], ccs["idx"]
        _put_settings(main, persistent={"cluster.remote.ghost.seeds": ["127.0.0.1:1"]})
        body = _ok(main.get(f"/ghost:{idx},{idx}/_field_caps", params={"fields": "t"}))
        assert body["indices"] == [idx]
        assert _ok(main.get("/_resolve/index/ghost:*")) == {"indices": [], "aliases": [], "data_streams": []}

    def test_removing_remote_with_null_seeds(self, ccs):
        main, idx = ccs["main"], ccs["idx"]
        _put_settings(main, persistent={"cluster.remote.ghost.seeds": [ccs["raddr"]]})
        assert _remote_info(main, "ghost")["connected"] is True
        assert _search(main, f"ghost:{idx}")["hits"]["total"]["value"] == 3
        _put_settings(main, persistent={"cluster.remote.ghost.seeds": None})
        assert "ghost" not in _ok(main.get("/_remote/info"))
        _err(main.get(f"/ghost:{idx}/_search"), 404, "index_not_found_exception", reason=f"no such index [ghost:{idx}]")

    def test_transient_remote_settings(self, ccs):
        main, idx = ccs["main"], ccs["idx"]
        try:
            _put_settings(main, transient={"cluster.remote.ghost.seeds": [ccs["raddr"]]})
            assert _remote_info(main, "ghost")["mode"] == "sniff"
            assert _search(main, f"ghost:{idx}")["hits"]["total"]["value"] == 3
        finally:
            _put_settings(main, transient={"cluster.remote.ghost.seeds": None})
        assert "ghost" not in _ok(main.get("/_remote/info"))


@needs_remote_ctl
class TestRemoteDatacenterDown:
    def test_remote_node_down_then_back(self, ccs):
        main, idx, rname = ccs["main"], ccs["idx"], ccs["rname"]
        target = f"{idx},{ALIAS}:{idx}"
        _ctl("stop-node", rname)
        try:
            info = _remote_info(main, ALIAS, connected=False)
            assert info["connected"] is False
            assert info["num_nodes_connected"] == 0
            for mr in ("true", "false"):
                _err(
                    main.get(f"/{target}/_search", params={"ccs_minimize_roundtrips": mr}),
                    500,
                    "connect_transport_exception",
                    reason=f"[][{ccs['raddr']}] connect_exception",
                )
            _put_settings(main, persistent={f"cluster.remote.{ALIAS}.skip_unavailable": True})
            for mr in ("true", "false"):
                body = _search(main, target, {"size": 0}, ccs_minimize_roundtrips=mr)
                assert body["_clusters"] == {"total": 2, "successful": 1, "skipped": 1}
                assert body["hits"]["total"]["value"] == 4
        finally:
            _put_settings(main, persistent={f"cluster.remote.{ALIAS}.skip_unavailable": None})
            _ctl("start-node", rname)
            _wait_remote_green(ccs["remote"])
        # the connection is re-established on demand
        deadline = time.time() + 60
        while True:
            r = main.get(f"/{target}/_search", params={"size": 0})
            if r.status_code == 200 or time.time() > deadline:
                break
            time.sleep(1)
        body = _ok(r)
        assert body["_clusters"] == {"total": 2, "successful": 2, "skipped": 0}
        assert body["hits"]["total"]["value"] == 7
        assert _remote_info(main, ALIAS)["connected"] is True
