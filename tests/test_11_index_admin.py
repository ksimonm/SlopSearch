import json
import uuid

import pytest

pytestmark = pytest.mark.admin

SINGLE = {"number_of_shards": 1, "number_of_replicas": 0}

BLOCK_RESET = {
    "index.blocks.read_only": None,
    "index.blocks.read_only_allow_delete": None,
    "index.blocks.metadata": None,
    "index.blocks.write": None,
    "index.blocks.read": None,
}


# --------------------------------------------------------------------------
# fixtures / helpers
# --------------------------------------------------------------------------


@pytest.fixture
def cluster_objs(client):
    """Registry of cluster-level objects to delete on teardown."""
    objs = {"index_template": [], "component_template": [], "template": []}
    yield objs
    for name in objs["index_template"]:
        client.delete(f"/_index_template/{name}")
    for name in objs["component_template"]:
        client.delete(f"/_component_template/{name}")
    for name in objs["template"]:
        client.delete(f"/_template/{name}")


@pytest.fixture
def pfx(client, cluster_objs):
    """Unique per-test name prefix; every index/alias/data stream created by a
    test starts with it (or with '.' + it) and is removed afterwards."""
    p = f"ia11-{uuid.uuid4().hex[:10]}"
    yield p
    client.delete(f"/_data_stream/{p}*")
    params = {"expand_wildcards": "all", "ignore_unavailable": "true", "allow_no_indices": "true"}
    for target in (f"{p}*", f".{p}*"):
        # the metadata block must be lifted on its own before anything else
        client.put(f"/{target}/_settings", json={"index.blocks.metadata": None}, params=params)
        client.put(f"/{target}/_settings", json=BLOCK_RESET, params=params)
        client.delete(f"/{target}", params=params)


def _err(resp):
    return resp.json()["error"]["type"]


def _reason(resp):
    return resp.json()["error"]["reason"]


def _mk(client, name, settings=None, mappings=None, aliases=None, params=None):
    body = {"settings": {**SINGLE, **(settings or {})}}
    if mappings is not None:
        body["mappings"] = mappings
    if aliases is not None:
        body["aliases"] = aliases
    r = client.put(f"/{name}", json=body, params=params or {})
    assert r.status_code == 200, r.text
    return name


def _docs(client, index, docs):
    """Index {id: source} docs with refresh."""
    lines = []
    for doc_id, src in docs.items():
        lines.append(json.dumps({"index": {"_index": index, "_id": str(doc_id)}}))
        lines.append(json.dumps(src))
    r = client.post(
        "/_bulk",
        content=("\n".join(lines) + "\n").encode(),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200, r.text
    assert r.json()["errors"] is False, r.text


def _count(client, target, **params):
    r = client.get(f"/{target}/_count", params=params)
    assert r.status_code == 200, r.text
    return r.json()["count"]


def _flat(client, index, **params):
    r = client.get(f"/{index}/_settings", params={"flat_settings": "true", **params})
    assert r.status_code == 200, r.text
    return r.json()[index]["settings"]


def _aliases(client, actions):
    return client.post("/_aliases", json={"actions": actions})


def _rollover(client, target, body=None, new_index=None, **params):
    path = f"/{target}/_rollover" + (f"/{new_index}" if new_index else "")
    if body is None:
        return client.post(path, params=params)
    return client.post(path, json=body, params=params)


# --------------------------------------------------------------------------
# create index: names
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "suffix,reason_part",
    [
        ("-UP", "must be lowercase"),
        ("a*b", "must not contain the following characters"),
        ("a%3Fb", "must not contain the following characters"),
        ("a%22b", "must not contain the following characters"),
        ("a%3Cb", "must not contain the following characters"),
        ("a%3Eb", "must not contain the following characters"),
        ("a%7Cb", "must not contain the following characters"),
        ("a%20b", "must not contain the following characters"),
        ("a%2Cb", "must not contain the following characters"),
        ("a%5Cb", "must not contain the following characters"),
        ("a%2Fb", "must not contain the following characters"),
        ("a%3Ab", "must not contain ':'"),
        ("a%23b", "must not contain '#'"),
    ],
)
def test_create_invalid_chars(client, pfx, suffix, reason_part):
    r = client.put(f"/{pfx}{suffix}")
    assert r.status_code == 400
    assert _err(r) == "invalid_index_name_exception"
    assert reason_part in _reason(r)
    assert r.json()["error"]["index_uuid"] == "_na_"


@pytest.mark.parametrize("lead", ["_", "-", "+"])
def test_create_invalid_leading_char(client, pfx, lead):
    r = client.put(f"/{lead}{pfx}")
    assert r.status_code == 400
    assert _err(r) == "invalid_index_name_exception"
    assert "must not start with '_', '-', or '+'" in _reason(r)


@pytest.mark.parametrize("name", ["%2E", "%2E%2E"])
def test_create_dot_and_dotdot_rejected(client, name):
    r = client.put(f"/{name}")
    assert r.status_code == 400
    assert _err(r) == "invalid_index_name_exception"
    assert "must not be '.' or '..'" in _reason(r)


def test_create_name_255_bytes_ok_256_rejected(client, pfx):
    ok = pfx + "y" * (255 - len(pfx))
    r = client.put(f"/{ok}", json={"settings": SINGLE})
    assert r.status_code == 200, r.text
    bad = pfx + "z" * (256 - len(pfx))
    r = client.put(f"/{bad}")
    assert r.status_code == 400
    assert _err(r) == "invalid_index_name_exception"
    assert "index name is too long, (256 > 255)" in _reason(r)


def test_create_name_length_counts_utf8_bytes(client, pfx):
    # 'é' is two bytes in UTF-8: pfx(15) + 120 * 2 = 255 bytes ok, 257 rejected
    ok = pfx + "é" * 120
    assert len(ok.encode()) == 255
    r = client.put(f"/{ok}", json={"settings": SINGLE})
    assert r.status_code == 200, r.text
    assert r.json()["index"] == ok
    bad = pfx + "é" * 121
    r = client.put(f"/{bad}")
    assert r.status_code == 400
    assert "(257 > 255)" in _reason(r)


def test_auto_create_uppercase_index_on_doc_put(client, pfx):
    r = client.put(f"/{pfx}-UP/_doc/1", json={"x": 1})
    assert r.status_code == 400
    assert _err(r) == "invalid_index_name_exception"


def test_create_dot_prefixed_index_allowed(client, pfx):
    r = client.put(f"/.{pfx}", json={"settings": SINGLE})
    assert r.status_code == 200
    assert r.json()["index"] == f".{pfx}"


def test_post_to_index_path_not_allowed(client, pfx):
    r = client.post(f"/{pfx}")
    assert r.status_code == 405


# --------------------------------------------------------------------------
# create index: body / settings
# --------------------------------------------------------------------------


def test_create_response_shape(client, pfx):
    r = client.put(f"/{pfx}", json={"settings": SINGLE})
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "shards_acknowledged": True, "index": pfx}


def test_create_without_body(client, pfx):
    r = client.put(f"/{pfx}")
    assert r.status_code == 200
    assert r.json()["index"] == pfx


def test_create_duplicate_is_400_resource_already_exists(client, pfx):
    _mk(client, pfx)
    r = client.put(f"/{pfx}")
    assert r.status_code == 400
    assert _err(r) == "resource_already_exists_exception"
    assert r.json()["error"]["index"] == pfx


@pytest.mark.parametrize(
    "settings",
    [
        {"number_of_shards": 1, "number_of_replicas": 0, "refresh_interval": "2s"},
        {"index.number_of_shards": 1, "index.number_of_replicas": 0, "index.refresh_interval": "2s"},
        {"index": {"number_of_shards": 1, "number_of_replicas": 0, "refresh_interval": "2s"}},
        {"index.number_of_shards": 1, "index": {"number_of_replicas": 0}, "refresh_interval": "2s"},
        {"number_of_shards": "1", "number_of_replicas": "0", "refresh_interval": "2s"},
    ],
    ids=["shorthand", "dotted", "nested", "mixed", "strings"],
)
def test_create_settings_forms_are_equivalent(client, pfx, settings):
    r = client.put(f"/{pfx}", json={"settings": settings})
    assert r.status_code == 200, r.text
    flat = _flat(client, pfx)
    assert flat["index.number_of_shards"] == "1"
    assert flat["index.number_of_replicas"] == "0"
    assert flat["index.refresh_interval"] == "2s"
    assert flat["index.provided_name"] == pfx


@pytest.mark.parametrize(
    "settings,etype,reason_part",
    [
        ({"number_of_shards": "abc"}, "illegal_argument_exception", "Failed to parse value [abc] for setting [index.number_of_shards]"),
        ({"number_of_shards": 0}, "illegal_argument_exception", "must be >= 1"),
        ({"index.number_of_shards": 2000}, "illegal_argument_exception", "must be <= 1024"),
        ({"number_of_replicas": -1}, "illegal_argument_exception", "must be >= 0"),
        ({"index": {"unknown_setting": 1}}, "settings_exception", "unknown setting [index.unknown_setting]"),
        ({"refresh_interval": "abc"}, "illegal_argument_exception", "failed to parse setting [index.refresh_interval]"),
    ],
)
def test_create_invalid_settings(client, pfx, settings, etype, reason_part):
    r = client.put(f"/{pfx}", json={"settings": settings})
    assert r.status_code == 400
    assert _err(r) == etype
    assert reason_part in _reason(r)
    assert client.head(f"/{pfx}").status_code == 404


def test_create_non_numeric_shards_caused_by_number_format(client, pfx):
    r = client.put(f"/{pfx}", json={"settings": {"number_of_shards": "abc"}})
    assert r.json()["error"]["caused_by"]["type"] == "number_format_exception"


def test_create_unknown_top_level_key(client, pfx):
    r = client.put(f"/{pfx}", json={"foo": {}})
    assert r.status_code == 400
    assert _err(r) == "parse_exception"
    assert _reason(r) == "unknown key [foo] for create index"


def test_create_mapping_nested_under_type_rejected(client, pfx):
    r = client.put(f"/{pfx}", json={"mappings": {"_doc": {"properties": {"x": {"type": "keyword"}}}}})
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "cannot be nested under a type" in _reason(r)


def test_create_unknown_field_type(client, pfx):
    r = client.put(f"/{pfx}", json={"mappings": {"properties": {"x": {"type": "bogus"}}}})
    assert r.status_code == 400
    assert _err(r) == "mapper_parsing_exception"
    assert "No handler for type [bogus] declared on field [x]" in _reason(r)


def test_create_non_json_body(client, pfx):
    r = client.put(f"/{pfx}", content=b"not json", headers={"Content-Type": "application/json"})
    assert r.status_code == 400
    assert _err(r) == "not_x_content_exception"


def test_create_with_mappings_and_aliases(client, pfx):
    _mk(
        client,
        pfx,
        mappings={"properties": {"t": {"type": "text"}}},
        aliases={f"{pfx}-al1": {}, f"{pfx}-al2": {"filter": {"term": {"t": "x"}}}},
    )
    body = client.get(f"/{pfx}").json()[pfx]
    assert body["mappings"] == {"properties": {"t": {"type": "text"}}}
    assert body["aliases"] == {
        f"{pfx}-al1": {},
        f"{pfx}-al2": {"filter": {"term": {"t": "x"}}},
    }


def test_create_alias_routing_sets_both_routings(client, pfx):
    _mk(client, pfx, aliases={f"{pfx}-al": {"is_write_index": True, "routing": "5"}})
    r = client.get(f"/{pfx}/_alias")
    assert r.json() == {
        pfx: {"aliases": {f"{pfx}-al": {"index_routing": "5", "search_routing": "5", "is_write_index": True}}}
    }


def test_create_alias_colliding_with_index_name(client, pfx):
    _mk(client, f"{pfx}-a")
    r = client.put(f"/{pfx}-b", json={"aliases": {f"{pfx}-a": {}}})
    assert r.status_code == 400
    assert _err(r) == "invalid_alias_name_exception"
    assert "an index exists with the same name as the alias" in _reason(r)
    assert client.head(f"/{pfx}-b").status_code == 404


def test_create_alias_with_invalid_name(client, pfx):
    r = client.put(f"/{pfx}", json={"aliases": {"_bad": {}}})
    assert r.status_code == 400
    assert _err(r) == "invalid_alias_name_exception"
    assert "must not start with '_', '-', or '+'" in _reason(r)


@pytest.mark.parametrize("was", ["all", "1", "0"])
def test_create_wait_for_active_shards(client, pfx, was):
    r = client.put(f"/{pfx}", json={"settings": SINGLE}, params={"wait_for_active_shards": was})
    assert r.status_code == 200
    assert r.json()["acknowledged"] is True


# --------------------------------------------------------------------------
# HEAD / GET index
# --------------------------------------------------------------------------


def test_head_index_exists_and_missing(client, pfx):
    _mk(client, pfx)
    r = client.head(f"/{pfx}")
    assert r.status_code == 200
    assert r.content == b""
    assert client.head(f"/{pfx}-missing").status_code == 404


def test_head_alias_and_unmatched_wildcard(client, pfx):
    _mk(client, pfx, aliases={f"{pfx}-al": {}})
    assert client.head(f"/{pfx}-al").status_code == 200
    # quirk: an unmatched wildcard still answers 200 (allow_no_indices default)
    assert client.head(f"/{pfx}-zz*").status_code == 200
    assert client.head(f"/{pfx}-zz*", params={"allow_no_indices": "false"}).status_code == 404


def test_get_index_shape(client, pfx):
    _mk(client, pfx, mappings={"properties": {"f": {"type": "keyword"}}}, aliases={f"{pfx}-al": {}})
    r = client.get(f"/{pfx}")
    assert r.status_code == 200
    body = r.json()
    assert list(body) == [pfx]
    idx = body[pfx]
    assert set(idx) == {"aliases", "mappings", "settings"}
    assert idx["aliases"] == {f"{pfx}-al": {}}
    assert idx["mappings"] == {"properties": {"f": {"type": "keyword"}}}
    s = idx["settings"]["index"]
    assert s["number_of_shards"] == "1"
    assert s["number_of_replicas"] == "0"
    assert s["provided_name"] == pfx
    assert {"uuid", "creation_date", "version"} <= set(s)


def test_get_index_without_mappings_has_empty_mappings(client, pfx):
    _mk(client, pfx)
    idx = client.get(f"/{pfx}").json()[pfx]
    assert idx["mappings"] == {}
    assert idx["aliases"] == {}


def test_get_index_flat_settings(client, pfx):
    _mk(client, pfx)
    s = client.get(f"/{pfx}", params={"flat_settings": "true"}).json()[pfx]["settings"]
    assert s["index.number_of_shards"] == "1"
    assert "index" not in s


def test_get_index_via_alias_returns_concrete_name(client, pfx):
    _mk(client, f"{pfx}-a", aliases={f"{pfx}-al": {}})
    body = client.get(f"/{pfx}-al").json()
    assert list(body) == [f"{pfx}-a"]


def test_get_multiple_and_wildcard(client, pfx):
    for s in ("a", "b", "c"):
        _mk(client, f"{pfx}-{s}")
    assert set(client.get(f"/{pfx}-a,{pfx}-c").json()) == {f"{pfx}-a", f"{pfx}-c"}
    assert set(client.get(f"/{pfx}-*").json()) == {f"{pfx}-a", f"{pfx}-b", f"{pfx}-c"}
    assert set(client.get(f"/{pfx}-*,-{pfx}-b").json()) == {f"{pfx}-a", f"{pfx}-c"}


def test_get_missing_index_404_shape(client, pfx):
    r = client.get(f"/{pfx}-missing")
    assert r.status_code == 404
    err = r.json()["error"]
    assert err["type"] == "index_not_found_exception"
    assert err["reason"] == f"no such index [{pfx}-missing]"
    assert err["index"] == f"{pfx}-missing"
    assert err["resource.type"] == "index_or_alias"
    assert err["resource.id"] == f"{pfx}-missing"
    assert err["index_uuid"] == "_na_"
    assert r.json()["status"] == 404
    assert err["root_cause"][0]["type"] == "index_not_found_exception"


def test_get_list_with_missing_and_ignore_unavailable(client, pfx):
    _mk(client, f"{pfx}-a")
    r = client.get(f"/{pfx}-a,{pfx}-missing")
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"
    r = client.get(f"/{pfx}-a,{pfx}-missing", params={"ignore_unavailable": "true"})
    assert r.status_code == 200
    assert list(r.json()) == [f"{pfx}-a"]


def test_get_unmatched_wildcard_and_allow_no_indices(client, pfx):
    r = client.get(f"/{pfx}-zz*")
    assert r.status_code == 200
    assert r.json() == {}
    r = client.get(f"/{pfx}-zz*", params={"allow_no_indices": "false"})
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"
    assert r.json()["error"]["index"] == f"{pfx}-zz*"


@pytest.fixture
def mixed_state(client, pfx):
    _mk(client, f"{pfx}-a")
    _mk(client, f"{pfx}-b")
    _mk(client, f"{pfx}-c")
    _mk(client, f"{pfx}-h", settings={"index.hidden": True})
    assert client.post(f"/{pfx}-c/_close").status_code == 200
    return pfx


@pytest.mark.parametrize(
    "ew,expected",
    [
        (None, {"a", "b"}),
        ("open", {"a", "b"}),
        ("closed", {"c"}),
        ("hidden", set()),
        ("none", set()),
        ("open,closed", {"a", "b", "c"}),
        ("open,hidden", {"a", "b", "h"}),
        ("all", {"a", "b", "c", "h"}),
    ],
)
def test_get_index_expand_wildcards(client, mixed_state, ew, expected):
    p = mixed_state
    params = {"expand_wildcards": ew} if ew else {}
    r = client.get(f"/{p}-*", params=params)
    assert r.status_code == 200
    assert set(r.json()) == {f"{p}-{s}" for s in expected}


def test_hidden_index_accessible_by_exact_name(client, mixed_state):
    p = mixed_state
    r = client.get(f"/{p}-h")
    assert r.status_code == 200
    assert r.json()[f"{p}-h"]["settings"]["index"]["hidden"] == "true"


def test_dot_prefixed_index_matched_by_wildcards(client, pfx):
    _mk(client, f"{pfx}-a")
    _mk(client, f".{pfx}-dot")
    assert set(client.get(f"/*{pfx}*").json()) == {f"{pfx}-a", f".{pfx}-dot"}
    assert set(client.get(f"/.{pfx}*").json()) == {f".{pfx}-dot"}
    assert set(client.get(f"/{pfx}*").json()) == {f"{pfx}-a"}


def test_get_all_includes_own_index(client, pfx):
    _mk(client, pfx)
    r = client.get("/_all", params={"filter_path": f"{pfx}.settings.index.provided_name"})
    assert r.json() == {pfx: {"settings": {"index": {"provided_name": pfx}}}}


# --------------------------------------------------------------------------
# delete index
# --------------------------------------------------------------------------


def test_delete_index_then_404(client, pfx):
    _mk(client, pfx)
    r = client.delete(f"/{pfx}")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    r = client.delete(f"/{pfx}")
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"
    assert client.head(f"/{pfx}").status_code == 404


def test_delete_list_with_missing(client, pfx):
    _mk(client, f"{pfx}-a")
    r = client.delete(f"/{pfx}-a,{pfx}-missing")
    assert r.status_code == 404
    assert client.head(f"/{pfx}-a").status_code == 200
    r = client.delete(f"/{pfx}-a,{pfx}-missing", params={"ignore_unavailable": "true"})
    assert r.status_code == 200
    assert client.head(f"/{pfx}-a").status_code == 404


def test_delete_wildcard_own_prefix(client, pfx):
    _mk(client, f"{pfx}-x1")
    _mk(client, f"{pfx}-x2")
    _mk(client, f"{pfx}-y1")
    r = client.delete(f"/{pfx}-x*")
    assert r.status_code == 200
    assert set(client.get(f"/{pfx}-*").json()) == {f"{pfx}-y1"}


def test_delete_unmatched_wildcard(client, pfx):
    r = client.delete(f"/{pfx}-zz*")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    r = client.delete(f"/{pfx}-zz*", params={"allow_no_indices": "false"})
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"


def test_delete_via_alias_rejected(client, pfx):
    _mk(client, f"{pfx}-a", aliases={f"{pfx}-al": {}})
    r = client.delete(f"/{pfx}-al")
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "matches an alias, specify the corresponding concrete indices instead" in _reason(r)
    assert client.head(f"/{pfx}-a").status_code == 200


def test_delete_closed_index(client, pfx):
    _mk(client, pfx)
    client.post(f"/{pfx}/_close")
    assert client.delete(f"/{pfx}").status_code == 200
    assert client.head(f"/{pfx}").status_code == 404


@pytest.mark.parametrize(
    "block,code_part",
    [("read_only", "FORBIDDEN/5/index read-only (api)"), ("metadata", "FORBIDDEN/9/index metadata (api)")],
)
def test_delete_index_blocked(client, pfx, block, code_part):
    _mk(client, pfx, settings={f"index.blocks.{block}": True})
    r = client.delete(f"/{pfx}")
    assert r.status_code == 403
    assert _err(r) == "cluster_block_exception"
    assert code_part in _reason(r)
    client.put(f"/{pfx}/_settings", json={f"index.blocks.{block}": False})
    assert client.delete(f"/{pfx}").status_code == 200


@pytest.mark.parametrize("block", ["write", "read", "read_only_allow_delete"])
def test_delete_index_allowed_under_block(client, pfx, block):
    _mk(client, pfx, settings={f"index.blocks.{block}": True})
    assert client.delete(f"/{pfx}").status_code == 200


# --------------------------------------------------------------------------
# settings
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "body",
    [{"index": {"number_of_shards": 2}}, {"number_of_shards": 2}, {"index.number_of_shards": 2}],
)
def test_update_static_number_of_shards_rejected(client, pfx, body):
    _mk(client, pfx)
    r = client.put(f"/{pfx}/_settings", json=body)
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "Can't update non dynamic settings [[index.number_of_shards]] for open indices" in _reason(r)


def test_update_static_setting_on_closed_index_is_final(client, pfx):
    _mk(client, pfx)
    client.post(f"/{pfx}/_close")
    r = client.put(f"/{pfx}/_settings", json={"index": {"number_of_shards": 3}})
    assert r.status_code == 400
    assert _err(r) == "settings_exception"
    assert "setting [index.number_of_shards], not updateable" in _reason(r)


def test_update_analysis_requires_closed_index(client, pfx):
    _mk(client, pfx)
    body = {"index": {"analysis": {"analyzer": {"my": {"type": "standard"}}}}}
    r = client.put(f"/{pfx}/_settings", json=body)
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "Can't update non dynamic settings" in _reason(r)
    client.post(f"/{pfx}/_close")
    assert client.put(f"/{pfx}/_settings", json=body).status_code == 200
    client.post(f"/{pfx}/_open")
    r = client.get(f"/{pfx}/_settings/index.analysis*")
    assert r.json() == {pfx: {"settings": {"index": {"analysis": {"analyzer": {"my": {"type": "standard"}}}}}}}


@pytest.mark.parametrize(
    "body",
    [{"index": {"number_of_replicas": 1}}, {"number_of_replicas": 1}, {"index.number_of_replicas": "1"}],
)
def test_update_number_of_replicas(client, pfx, body):
    _mk(client, pfx)
    r = client.put(f"/{pfx}/_settings", json=body)
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    assert _flat(client, pfx)["index.number_of_replicas"] == "1"


def test_max_result_window_enforced(client, pfx):
    _mk(client, pfx)
    _docs(client, pfx, {1: {"a": 1}})
    assert client.put(f"/{pfx}/_settings", json={"index.max_result_window": "5"}).status_code == 200
    assert client.get(f"/{pfx}/_search", params={"size": 5}).status_code == 200
    assert client.get(f"/{pfx}/_search", params={"size": 0, "from": 5}).status_code == 200
    r = client.get(f"/{pfx}/_search", params={"size": 3, "from": 3})
    assert r.status_code == 400
    assert _err(r) == "search_phase_execution_exception"
    rc = r.json()["error"]["root_cause"][0]
    assert rc["type"] == "illegal_argument_exception"
    assert "Result window is too large, from + size must be less than or equal to: [5] but was [6]" in rc["reason"]


def test_max_result_window_default_is_10000(client, pfx):
    _mk(client, pfx)
    assert client.get(f"/{pfx}/_search", params={"size": 10000}).status_code == 200
    r = client.get(f"/{pfx}/_search", params={"size": 10001})
    assert r.status_code == 400
    assert "[10000] but was [10001]" in r.json()["error"]["root_cause"][0]["reason"]


@pytest.mark.parametrize(
    "body,etype,reason_part",
    [
        ({"index": {"max_result_window": 0}}, "illegal_argument_exception", "must be >= 1"),
        ({"index": {"max_result_window": "x"}}, "illegal_argument_exception", "Failed to parse value [x]"),
        ({"index": {"refresh_interval": "abc"}}, "illegal_argument_exception", "unit is missing or unrecognized"),
        ({"index": {"bogus": "x"}}, "settings_exception", "unknown setting [index.bogus]"),
        ({"index": {"number_of_replicas": -1}}, "illegal_argument_exception", "must be >= 0"),
        ({}, "action_request_validation_exception", "no settings to update"),
    ],
)
def test_update_settings_invalid(client, pfx, body, etype, reason_part):
    _mk(client, pfx)
    r = client.put(f"/{pfx}/_settings", json=body)
    assert r.status_code == 400
    assert _err(r) == etype
    assert reason_part in _reason(r)


def test_refresh_interval_minus_one(client, pfx):
    _mk(client, pfx)
    assert client.put(f"/{pfx}/_settings", json={"index": {"refresh_interval": "-1"}}).status_code == 200
    assert _flat(client, pfx)["index.refresh_interval"] == "-1"


def test_reset_setting_with_null(client, pfx):
    _mk(client, pfx)
    client.put(f"/{pfx}/_settings", json={"index.max_result_window": 20})
    assert _flat(client, pfx)["index.max_result_window"] == "20"
    assert client.put(f"/{pfx}/_settings", json={"index": {"max_result_window": None}}).status_code == 200
    assert "index.max_result_window" not in _flat(client, pfx)
    r = client.get(f"/{pfx}/_settings/index.max_result_window")
    assert r.json() == {}


def test_preserve_existing(client, pfx):
    _mk(client, pfx)
    client.put(f"/{pfx}/_settings", json={"index.max_result_window": 5})
    r = client.put(
        f"/{pfx}/_settings",
        json={"index": {"max_result_window": 99, "max_inner_result_window": 7}},
        params={"preserve_existing": "true"},
    )
    assert r.status_code == 200
    flat = _flat(client, pfx)
    assert flat["index.max_result_window"] == "5"
    assert flat["index.max_inner_result_window"] == "7"


def test_get_settings_nested_and_flat(client, pfx):
    _mk(client, pfx)
    nested = client.get(f"/{pfx}/_settings").json()
    assert list(nested) == [pfx]
    assert list(nested[pfx]) == ["settings"]
    assert nested[pfx]["settings"]["index"]["number_of_shards"] == "1"
    flat = _flat(client, pfx)
    assert all(k.startswith("index.") for k in flat)
    assert flat["index.provided_name"] == pfx


def test_get_settings_filtered_by_name(client, pfx):
    _mk(client, pfx, settings={"refresh_interval": "3s"})
    r = client.get(f"/{pfx}/_settings/index.number_*")
    assert r.json() == {pfx: {"settings": {"index": {"number_of_shards": "1", "number_of_replicas": "0"}}}}
    r = client.get(
        f"/{pfx}/_settings/index.number_of_shards,index.refresh_interval",
        params={"flat_settings": "true"},
    )
    assert r.json() == {pfx: {"settings": {"index.number_of_shards": "1", "index.refresh_interval": "3s"}}}
    r = client.get(f"/{pfx}/_settings/nothing.here")
    assert r.status_code == 200
    assert r.json() == {}


def test_get_settings_include_defaults(client, pfx):
    _mk(client, pfx)
    body = client.get(
        f"/{pfx}/_settings", params={"include_defaults": "true", "flat_settings": "true"}
    ).json()[pfx]
    assert set(body) == {"settings", "defaults"}
    d = body["defaults"]
    assert d["index.max_result_window"] == "10000"
    assert d["index.hidden"] == "false"
    assert d["index.blocks.write"] == "false"
    assert d["index.mapping.total_fields.limit"] == "1000"
    assert d["index.mapping.depth.limit"] == "20"
    assert d["index.mapping.nested_fields.limit"] == "50"
    assert d["index.max_ngram_diff"] == "1"
    assert d["index.max_shingle_diff"] == "3"
    assert d["index.max_inner_result_window"] == "100"
    assert d["index.max_terms_count"] == "65536"
    assert d["index.max_regex_length"] == "1000"
    assert d["index.default_pipeline"] == "_none"
    assert d["index.final_pipeline"] == "_none"
    assert d["index.refresh_interval"] == "1s"
    assert d["index.auto_expand_replicas"] == "false"
    assert d["index.query.default_field"] == ["*"]
    # explicitly-set settings are not duplicated into defaults
    assert "index.number_of_shards" not in d


def test_include_defaults_with_name_filter(client, pfx):
    _mk(client, pfx)
    r = client.get(f"/{pfx}/_settings/index.hidden", params={"include_defaults": "true"})
    assert r.json() == {pfx: {"settings": {}, "defaults": {"index": {"hidden": "false"}}}}


def test_update_settings_multiple_and_wildcard(client, pfx):
    for s in ("a", "b", "c"):
        _mk(client, f"{pfx}-{s}")
    assert client.put(f"/{pfx}-a,{pfx}-b/_settings", json={"index": {"max_result_window": 77}}).status_code == 200
    assert _flat(client, f"{pfx}-c").get("index.max_result_window") is None
    assert client.put(f"/{pfx}-*/_settings", json={"index": {"max_result_window": 78}}).status_code == 200
    r = client.get(f"/{pfx}-*/_settings/index.max_result_window")
    assert r.json() == {f"{pfx}-{s}": {"settings": {"index": {"max_result_window": "78"}}} for s in "abc"}


def test_get_settings_across_all_with_name_filter(client, pfx):
    _mk(client, pfx, settings={"index.max_result_window": 33})
    r = client.get("/_settings/index.max_result_window", params={"filter_path": pfx})
    assert r.json() == {pfx: {"settings": {"index": {"max_result_window": "33"}}}}


@pytest.mark.parametrize("method", ["GET", "PUT"])
def test_settings_missing_index(client, pfx, method):
    kw = {"json": {"index": {"number_of_replicas": 0}}} if method == "PUT" else {}
    r = client.request(method, f"/{pfx}-nope/_settings", **kw)
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"


# --------------------------------------------------------------------------
# mappings
# --------------------------------------------------------------------------

MAPPING = {
    "properties": {
        "title": {"type": "text", "fields": {"raw": {"type": "keyword"}}},
        "user": {"properties": {"name": {"type": "keyword"}, "age": {"type": "integer"}}},
        "price": {"type": "float"},
    }
}


@pytest.fixture
def mapped(client, pfx):
    return _mk(client, pfx, mappings=MAPPING)


def test_get_mapping_shape(client, mapped):
    r = client.get(f"/{mapped}/_mapping")
    assert r.status_code == 200
    assert r.json() == {mapped: {"mappings": MAPPING}}


def test_get_mappings_plural_endpoint(client, mapped):
    assert client.get(f"/{mapped}/_mappings").json() == {mapped: {"mappings": MAPPING}}


@pytest.mark.parametrize("method,path", [("PUT", "_mapping"), ("POST", "_mapping"), ("PUT", "_mappings")])
def test_put_mapping_adds_field(client, mapped, method, path):
    r = client.request(method, f"/{mapped}/{path}", json={"properties": {"q": {"type": "long"}}})
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    props = client.get(f"/{mapped}/_mapping").json()[mapped]["mappings"]["properties"]
    assert props["q"] == {"type": "long"}
    assert props["title"] == MAPPING["properties"]["title"]


def test_put_mapping_add_multifield(client, mapped):
    body = {"properties": {"title": {"type": "text", "fields": {"raw": {"type": "keyword"}, "en": {"type": "text"}}}}}
    assert client.put(f"/{mapped}/_mapping", json=body).status_code == 200
    fields = client.get(f"/{mapped}/_mapping").json()[mapped]["mappings"]["properties"]["title"]["fields"]
    assert set(fields) == {"raw", "en"}


@pytest.mark.parametrize(
    "props,etype,reason_part",
    [
        ({"title": {"type": "keyword"}}, "illegal_argument_exception", "mapper [title] cannot be changed from type [text] to [keyword]"),
        ({"price": {"type": "integer"}}, "illegal_argument_exception", "cannot be changed from type [float] to [integer]"),
        ({"user": {"type": "keyword"}}, "illegal_argument_exception", "can't merge a non object mapping [user] with an object mapping"),
        ({"x": {"type": "nosuch"}}, "mapper_parsing_exception", "No handler for type [nosuch] declared on field [x]"),
        ({"title": {"type": "text", "analyzer": "keyword"}}, "illegal_argument_exception", "Cannot update parameter [analyzer] from [default] to [keyword]"),
    ],
)
def test_put_mapping_conflicts(client, mapped, props, etype, reason_part):
    r = client.put(f"/{mapped}/_mapping", json={"properties": props})
    assert r.status_code == 400
    assert _err(r) == etype
    assert reason_part in _reason(r)


def test_put_mapping_with_type_rejected(client, mapped):
    r = client.put(f"/{mapped}/_mapping", json={"_doc": {"properties": {"q": {"type": "long"}}}})
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert _reason(r) == "Types cannot be provided in put mapping requests"


def test_put_mapping_dynamic_strict(client, mapped):
    assert client.put(f"/{mapped}/_mapping", json={"dynamic": "strict"}).status_code == 200
    assert client.get(f"/{mapped}/_mapping").json()[mapped]["mappings"]["dynamic"] == "strict"
    r = client.put(f"/{mapped}/_doc/1", json={"zzz": 1})
    assert r.status_code == 400
    assert _err(r) == "strict_dynamic_mapping_exception"
    assert "dynamic introduction of [zzz] within [_doc] is not allowed" in _reason(r)


def test_put_empty_mapping_is_noop(client, mapped):
    assert client.put(f"/{mapped}/_mapping", json={}).status_code == 200
    assert client.get(f"/{mapped}/_mapping").json() == {mapped: {"mappings": MAPPING}}


def test_put_mapping_multiple_indices(client, pfx):
    _mk(client, f"{pfx}-a")
    _mk(client, f"{pfx}-b", mappings={"properties": {"t": {"type": "text"}}})
    r = client.put(f"/{pfx}-a,{pfx}-b/_mapping", json={"properties": {"z": {"type": "keyword"}}})
    assert r.status_code == 200
    body = client.get(f"/{pfx}-a,{pfx}-b/_mapping").json()
    assert body[f"{pfx}-a"]["mappings"] == {"properties": {"z": {"type": "keyword"}}}
    assert body[f"{pfx}-b"]["mappings"]["properties"] == {"t": {"type": "text"}, "z": {"type": "keyword"}}


def test_mapping_missing_index(client, pfx):
    r = client.get(f"/{pfx}-nope/_mapping")
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"


def test_field_mapping_single(client, mapped):
    r = client.get(f"/{mapped}/_mapping/field/title")
    assert r.status_code == 200
    assert r.json() == {
        mapped: {
            "mappings": {
                "title": {
                    "full_name": "title",
                    "mapping": {"title": {"type": "text", "fields": {"raw": {"type": "keyword"}}}},
                }
            }
        }
    }


def test_field_mapping_subfield_uses_leaf_name(client, mapped):
    r = client.get(f"/{mapped}/_mapping/field/title.raw")
    assert r.json() == {
        mapped: {"mappings": {"title.raw": {"full_name": "title.raw", "mapping": {"raw": {"type": "keyword"}}}}}
    }


def test_field_mapping_wildcard_and_list(client, mapped):
    r = client.get(f"/{mapped}/_mapping/field/user.*")
    assert set(r.json()[mapped]["mappings"]) == {"user.name", "user.age"}
    assert r.json()[mapped]["mappings"]["user.age"]["mapping"] == {"age": {"type": "integer"}}
    r = client.get(f"/{mapped}/_mapping/field/title,price")
    assert set(r.json()[mapped]["mappings"]) == {"title", "price"}


def test_field_mapping_missing_field(client, mapped):
    r = client.get(f"/{mapped}/_mapping/field/nope")
    assert r.status_code == 200
    assert r.json() == {mapped: {"mappings": {}}}


def test_field_mapping_object_field_not_returned(client, mapped):
    r = client.get(f"/{mapped}/_mapping/field/user")
    assert r.json() == {mapped: {"mappings": {}}}


def test_field_mapping_include_defaults(client, mapped):
    m = client.get(
        f"/{mapped}/_mapping/field/price", params={"include_defaults": "true"}
    ).json()[mapped]["mappings"]["price"]["mapping"]["price"]
    assert m["type"] == "float"
    assert m["index"] is True
    assert m["doc_values"] is True
    assert m["store"] is False
    assert m["coerce"] is True
    assert m["ignore_malformed"] is False
    assert m["null_value"] is None


def test_field_mapping_across_all_indices(client, mapped):
    r = client.get("/_mapping/field/user.name", params={"filter_path": mapped})
    assert r.json() == {
        mapped: {"mappings": {"user.name": {"full_name": "user.name", "mapping": {"name": {"type": "keyword"}}}}}
    }


def test_get_mapping_closed_index_and_put(client, pfx):
    _mk(client, pfx, mappings={"properties": {"x": {"type": "long"}}})
    client.post(f"/{pfx}/_close")
    assert client.get(f"/{pfx}/_mapping").json() == {pfx: {"mappings": {"properties": {"x": {"type": "long"}}}}}
    assert client.put(f"/{pfx}/_mapping", json={"properties": {"y": {"type": "keyword"}}}).status_code == 200


# --------------------------------------------------------------------------
# aliases
# --------------------------------------------------------------------------


@pytest.fixture
def two(client, pfx):
    mappings = {"properties": {"k": {"type": "keyword"}}}
    a = _mk(client, f"{pfx}-a", mappings=mappings)
    b = _mk(client, f"{pfx}-b", mappings=mappings)
    _docs(client, a, {1: {"k": "x"}, 2: {"k": "y"}})
    _docs(client, b, {3: {"k": "x"}})
    return a, b


def test_aliases_add_and_get(client, pfx, two):
    a, b = two
    r = _aliases(client, [
        {"add": {"index": a, "alias": f"{pfx}-both"}},
        {"add": {"index": b, "alias": f"{pfx}-both"}},
    ])
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    body = client.get(f"/_alias/{pfx}-both").json()
    assert body == {a: {"aliases": {f"{pfx}-both": {}}}, b: {"aliases": {f"{pfx}-both": {}}}}
    assert _count(client, f"{pfx}-both") == 3


def test_alias_filtered(client, pfx, two):
    a, _ = two
    _aliases(client, [{"add": {"index": a, "alias": f"{pfx}-fx", "filter": {"term": {"k": "x"}}}}])
    assert client.get(f"/{a}/_alias/{pfx}-fx").json() == {
        a: {"aliases": {f"{pfx}-fx": {"filter": {"term": {"k": "x"}}}}}
    }
    assert _count(client, f"{pfx}-fx") == 1
    r = client.post(f"/{pfx}-fx/_search", json={"query": {"match_all": {}}})
    assert [h["_id"] for h in r.json()["hits"]["hits"]] == ["1"]


def test_alias_filter_not_applied_to_writes(client, pfx, two):
    a, _ = two
    _aliases(client, [{"add": {"index": a, "alias": f"{pfx}-fx", "filter": {"term": {"k": "x"}}}}])
    r = client.put(f"/{pfx}-fx/_doc/9", json={"k": "z"}, params={"refresh": "true"})
    assert r.status_code == 201
    assert r.json()["_index"] == a
    assert _count(client, f"{pfx}-fx") == 1
    assert _count(client, a) == 3


def test_alias_invalid_filter(client, pfx, two):
    a, _ = two
    r = _aliases(client, [{"add": {"index": a, "alias": f"{pfx}-bad", "filter": {"nosuchquery": {}}}}])
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert _reason(r) == f"failed to parse filter for alias [{pfx}-bad]"
    assert r.json()["error"]["caused_by"]["type"] == "parsing_exception"


def test_alias_routing(client, pfx, two):
    a, _ = two
    _aliases(client, [
        {"add": {"index": a, "alias": f"{pfx}-r", "routing": "1"}},
        {"add": {"index": a, "alias": f"{pfx}-sr", "search_routing": "1,2", "index_routing": "2"}},
    ])
    aliases = client.get(f"/{a}/_alias/{pfx}-*").json()[a]["aliases"]
    assert aliases[f"{pfx}-r"] == {"index_routing": "1", "search_routing": "1"}
    assert aliases[f"{pfx}-sr"] == {"index_routing": "2", "search_routing": "1,2"}
    client.put(f"/{pfx}-r/_doc/10", json={"k": "z"}, params={"refresh": "true"})
    assert client.get(f"/{a}/_doc/10").json()["_routing"] == "1"
    client.put(f"/{pfx}-sr/_doc/11", json={"k": "z"}, params={"refresh": "true"})
    assert client.get(f"/{a}/_doc/11", params={"routing": "2"}).json()["_routing"] == "2"


def test_write_to_alias_with_multiple_indices_fails(client, pfx, two):
    a, b = two
    _aliases(client, [{"add": {"indices": [a, b], "alias": f"{pfx}-both"}}])
    r = client.put(f"/{pfx}-both/_doc/9", json={"k": "z"})
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert f"no write index is defined for alias [{pfx}-both]" in _reason(r)


def test_write_index_routes_writes(client, pfx, two):
    a, b = two
    _aliases(client, [
        {"add": {"index": a, "alias": f"{pfx}-wi", "is_write_index": True}},
        {"add": {"index": b, "alias": f"{pfx}-wi"}},
    ])
    r = client.put(f"/{pfx}-wi/_doc/w1", json={"k": "w"}, params={"refresh": "true"})
    assert r.status_code == 201
    assert r.json()["_index"] == a
    assert client.get(f"/_alias/{pfx}-wi").json() == {
        a: {"aliases": {f"{pfx}-wi": {"is_write_index": True}}},
        b: {"aliases": {f"{pfx}-wi": {}}},
    }


def test_two_write_indices_is_500_illegal_state(client, pfx, two):
    a, b = two
    _aliases(client, [{"add": {"index": a, "alias": f"{pfx}-wi", "is_write_index": True}}])
    r = _aliases(client, [{"add": {"index": b, "alias": f"{pfx}-wi", "is_write_index": True}}])
    assert r.status_code == 500
    assert _err(r) == "illegal_state_exception"
    assert "has more than one write index" in _reason(r)


def test_is_write_index_false_single_index_not_writable(client, pfx, two):
    a, _ = two
    _aliases(client, [{"add": {"index": a, "alias": f"{pfx}-nw", "is_write_index": False}}])
    r = client.put(f"/{pfx}-nw/_doc/1", json={"k": "z"})
    assert r.status_code == 400
    assert "no write index is defined" in _reason(r)


def test_single_index_alias_is_implicitly_writable(client, pfx, two):
    a, _ = two
    _aliases(client, [{"add": {"index": a, "alias": f"{pfx}-one"}}])
    r = client.put(f"/{pfx}-one/_doc/9", json={"k": "z"})
    assert r.status_code == 201
    assert r.json()["_index"] == a


def test_alias_name_collides_with_index(client, pfx, two):
    a, b = two
    r = client.put(f"/{a}/_alias/{b}")
    assert r.status_code == 400
    assert _err(r) == "invalid_alias_name_exception"
    assert f"Invalid alias name [{b}], an index exists with the same name as the alias" == _reason(r)


def test_index_name_collides_with_alias(client, pfx, two):
    a, _ = two
    client.put(f"/{a}/_alias/{pfx}-al")
    r = client.put(f"/{pfx}-al")
    assert r.status_code == 400
    assert _err(r) == "invalid_index_name_exception"
    assert "already exists as alias" in _reason(r)


def test_alias_names_allow_uppercase(client, pfx, two):
    a, _ = two
    r = _aliases(client, [{"add": {"index": a, "alias": f"{pfx}-UP"}}])
    assert r.status_code == 200
    assert _count(client, f"{pfx}-UP") == 2


def test_alias_invalid_leading_underscore(client, pfx, two):
    a, _ = two
    r = _aliases(client, [{"add": {"index": a, "alias": f"_{pfx}"}}])
    assert r.status_code == 400
    assert _err(r) == "invalid_alias_name_exception"
    assert "must not start with '_', '-', or '+'" in _reason(r)


@pytest.mark.parametrize("method,path", [("PUT", "_alias"), ("POST", "_alias"), ("PUT", "_aliases"), ("POST", "_aliases")])
def test_put_single_alias_endpoints(client, pfx, two, method, path):
    a, _ = two
    r = client.request(method, f"/{a}/{path}/{pfx}-s")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    assert client.head(f"/_alias/{pfx}-s").status_code == 200


def test_put_alias_with_body(client, pfx, two):
    a, _ = two
    r = client.put(f"/{a}/_alias/{pfx}-s", json={"filter": {"term": {"k": "y"}}, "routing": "3"})
    assert r.status_code == 200
    assert client.get(f"/{a}/_alias/{pfx}-s").json()[a]["aliases"][f"{pfx}-s"] == {
        "filter": {"term": {"k": "y"}},
        "index_routing": "3",
        "search_routing": "3",
    }


def test_delete_alias_endpoint(client, pfx, two):
    a, _ = two
    client.put(f"/{a}/_alias/{pfx}-s")
    r = client.delete(f"/{a}/_alias/{pfx}-s")
    assert r.status_code == 200
    r = client.delete(f"/{a}/_alias/{pfx}-s")
    assert r.status_code == 404
    assert _err(r) == "aliases_not_found_exception"
    assert _reason(r) == f"aliases [{pfx}-s] missing"
    assert r.json()["error"]["resource.type"] == "aliases"


def test_delete_alias_list_with_one_missing_succeeds(client, pfx, two):
    a, _ = two
    client.put(f"/{a}/_alias/{pfx}-s")
    r = client.delete(f"/{a}/_alias/{pfx}-s,{pfx}-nope")
    assert r.status_code == 200
    assert client.head(f"/_alias/{pfx}-s").status_code == 404


def test_get_alias_missing_404_plain_error(client, pfx, two):
    a, _ = two
    r = client.get(f"/_alias/{pfx}-nope")
    assert r.status_code == 404
    assert r.json() == {"error": f"alias [{pfx}-nope] missing", "status": 404}
    r = client.get(f"/{a}/_alias/{pfx}-nope")
    assert r.status_code == 404


def test_get_alias_partial_missing_returns_404_with_data(client, pfx, two):
    a, _ = two
    client.put(f"/{a}/_alias/{pfx}-s")
    r = client.get(f"/_alias/{pfx}-s,{pfx}-nope")
    assert r.status_code == 404
    body = r.json()
    assert body["error"] == f"alias [{pfx}-nope] missing"
    assert body["status"] == 404
    assert body[a] == {"aliases": {f"{pfx}-s": {}}}


def test_get_alias_wildcard_and_index_scoped(client, pfx, two):
    a, b = two
    _aliases(client, [
        {"add": {"index": a, "alias": f"{pfx}-f1"}},
        {"add": {"index": b, "alias": f"{pfx}-f2"}},
        {"add": {"index": a, "alias": f"{pfx}-g1"}},
    ])
    assert client.get(f"/{a}/_alias/{pfx}-f*").json() == {a: {"aliases": {f"{pfx}-f1": {}}}}
    assert client.get(f"/_alias/{pfx}-f*").json() == {
        a: {"aliases": {f"{pfx}-f1": {}}},
        b: {"aliases": {f"{pfx}-f2": {}}},
    }


def test_get_alias_for_index_without_aliases(client, pfx, two):
    a, _ = two
    assert client.get(f"/{a}/_alias").json() == {a: {"aliases": {}}}
    assert client.get(f"/{a}/_alias/*").json() == {}
    assert client.get("/_aliases", params={"filter_path": a}).json() == {a: {"aliases": {}}}


def test_head_alias(client, pfx, two):
    a, b = two
    client.put(f"/{a}/_alias/{pfx}-s")
    assert client.head(f"/_alias/{pfx}-s").status_code == 200
    assert client.head(f"/{a}/_alias/{pfx}-s").status_code == 200
    assert client.head(f"/{b}/_alias/{pfx}-s").status_code == 404
    assert client.head(f"/_alias/{pfx}-nope").status_code == 404


def test_aliases_remove_missing_and_must_exist(client, pfx, two):
    a, _ = two
    r = _aliases(client, [{"remove": {"index": a, "alias": f"{pfx}-nope"}}])
    assert r.status_code == 404
    assert _err(r) == "aliases_not_found_exception"
    r = _aliases(client, [{"remove": {"index": a, "alias": f"{pfx}-nope", "must_exist": False}}])
    assert r.status_code == 200


def test_aliases_swap_is_atomic(client, pfx, two):
    a, b = two
    client.put(f"/{a}/_alias/{pfx}-cur")
    r = _aliases(client, [
        {"remove": {"index": a, "alias": f"{pfx}-cur"}},
        {"add": {"index": b, "alias": f"{pfx}-cur"}},
    ])
    assert r.status_code == 200
    assert client.get(f"/_alias/{pfx}-cur").json() == {b: {"aliases": {f"{pfx}-cur": {}}}}


def test_aliases_failed_batch_applies_nothing(client, pfx, two):
    a, _ = two
    r = _aliases(client, [
        {"add": {"index": a, "alias": f"{pfx}-new"}},
        {"add": {"index": f"{pfx}-missing", "alias": f"{pfx}-x"}},
    ])
    assert r.status_code == 404
    assert client.head(f"/_alias/{pfx}-new").status_code == 404


def test_aliases_remove_missing_tolerated_when_batch_has_other_actions(client, pfx, two):
    # quirk: a missing alias in 'remove' only fails when nothing else is applied
    a, _ = two
    r = _aliases(client, [
        {"add": {"index": a, "alias": f"{pfx}-new"}},
        {"remove": {"index": a, "alias": f"{pfx}-nope"}},
    ])
    assert r.status_code == 200
    assert client.head(f"/_alias/{pfx}-new").status_code == 200


def test_aliases_multi_indices_and_aliases_arrays(client, pfx, two):
    a, b = two
    r = _aliases(client, [{"add": {"indices": [a, b], "aliases": [f"{pfx}-m1", f"{pfx}-m2"]}}])
    assert r.status_code == 200
    body = client.get(f"/_alias/{pfx}-m*").json()
    assert body == {
        a: {"aliases": {f"{pfx}-m1": {}, f"{pfx}-m2": {}}},
        b: {"aliases": {f"{pfx}-m1": {}, f"{pfx}-m2": {}}},
    }


def test_aliases_add_with_index_wildcard(client, pfx, two):
    a, b = two
    assert _aliases(client, [{"add": {"index": f"{pfx}-*", "alias": f"{pfx}-wild"}}]).status_code == 200
    assert set(client.get(f"/_alias/{pfx}-wild").json()) == {a, b}


def test_aliases_remove_index_action(client, pfx, two):
    a, b = two
    r = _aliases(client, [{"remove_index": {"index": b}}])
    assert r.status_code == 200
    assert client.head(f"/{b}").status_code == 404
    assert client.head(f"/{a}").status_code == 200


def test_aliases_remove_index_via_alias_rejected(client, pfx, two):
    a, _ = two
    client.put(f"/{a}/_alias/{pfx}-s")
    r = _aliases(client, [{"remove_index": {"index": f"{pfx}-s"}}])
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "matches an alias" in _reason(r)


@pytest.mark.parametrize(
    "actions,status,etype",
    [
        ([], 400, "illegal_argument_exception"),
        ([{"bogus": {}}], 400, "x_content_parse_exception"),
        ([{"add": {"index": "INDEX"}}], 400, "x_content_parse_exception"),
    ],
)
def test_aliases_invalid_actions(client, pfx, two, actions, status, etype):
    a, _ = two
    actions = json.loads(json.dumps(actions).replace("INDEX", a))
    r = _aliases(client, actions)
    assert r.status_code == status
    assert _err(r) == etype


def test_aliases_empty_actions_reason(client):
    r = _aliases(client, [])
    assert _reason(r) == "No action specified"


def test_aliases_add_to_missing_index(client, pfx):
    r = _aliases(client, [{"add": {"index": f"{pfx}-missing", "alias": f"{pfx}-x"}}])
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"


def test_alias_removed_when_index_deleted(client, pfx, two):
    a, b = two
    _aliases(client, [{"add": {"indices": [a, b], "alias": f"{pfx}-both"}}])
    client.delete(f"/{b}")
    assert client.get(f"/_alias/{pfx}-both").json() == {a: {"aliases": {f"{pfx}-both": {}}}}


def test_cat_aliases(client, pfx, two):
    a, _ = two
    _aliases(client, [
        {"add": {"index": a, "alias": f"{pfx}-fx", "filter": {"term": {"k": "x"}}}},
        {"add": {"index": a, "alias": f"{pfx}-r", "routing": "1", "is_write_index": True}},
    ])
    r = client.get(f"/_cat/aliases/{pfx}-*", params={"format": "json", "s": "alias"})
    rows = r.json()
    assert rows == [
        {"alias": f"{pfx}-fx", "index": a, "filter": "*", "routing.index": "-", "routing.search": "-", "is_write_index": "-"},
        {"alias": f"{pfx}-r", "index": a, "filter": "-", "routing.index": "1", "routing.search": "1", "is_write_index": "true"},
    ]


# --------------------------------------------------------------------------
# open / close
# --------------------------------------------------------------------------


@pytest.fixture
def closed(client, pfx):
    _mk(client, pfx)
    _docs(client, pfx, {1: {"x": 1}})
    r = client.post(f"/{pfx}/_close")
    assert r.status_code == 200
    return pfx


def test_close_response_shape(client, pfx):
    _mk(client, pfx)
    r = client.post(f"/{pfx}/_close")
    assert r.json() == {"acknowledged": True, "shards_acknowledged": True, "indices": {pfx: {"closed": True}}}


def test_close_twice(client, closed):
    r = client.post(f"/{closed}/_close")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "shards_acknowledged": False, "indices": {}}


@pytest.mark.parametrize(
    "method,path",
    [
        ("GET", "_search"),
        ("GET", "_count"),
        ("GET", "_doc/1"),
        ("POST", "_refresh"),
        ("POST", "_flush"),
        ("GET", "_stats/docs"),
    ],
)
def test_closed_index_operations_fail(client, closed, method, path):
    r = client.request(method, f"/{closed}/{path}")
    assert r.status_code == 400
    assert _err(r) == "index_closed_exception"
    assert _reason(r) == "closed"
    assert r.json()["error"]["index"] == closed


def test_index_into_closed_index_fails(client, closed):
    r = client.put(f"/{closed}/_doc/2", json={"x": 1})
    assert r.status_code == 400
    assert _err(r) == "index_closed_exception"


def test_closed_index_metadata_still_available(client, closed):
    assert client.head(f"/{closed}").status_code == 200
    body = client.get(f"/{closed}").json()[closed]
    assert body["settings"]["index"]["verified_before_close"] == "true"
    assert body["mappings"] == {"properties": {"x": {"type": "long"}}}


def test_wildcard_search_skips_closed(client, closed):
    r = client.get(f"/{closed}*/_search")
    assert r.status_code == 200
    assert r.json()["_shards"]["total"] == 0
    assert r.json()["hits"]["hits"] == []
    r = client.get(f"/{closed}*/_search", params={"expand_wildcards": "all"})
    assert r.status_code == 400
    assert _err(r) == "index_closed_exception"


def test_search_closed_ignore_unavailable(client, closed):
    r = client.get(f"/{closed}/_search", params={"ignore_unavailable": "true"})
    assert r.status_code == 200
    assert r.json()["hits"]["total"] == {"value": 0, "relation": "eq"}


def test_cat_indices_shows_close(client, closed):
    r = client.get(f"/_cat/indices/{closed}", params={"h": "status,index"})
    assert r.text.split() == ["close", closed]


def test_open_restores_docs(client, closed):
    r = client.post(f"/{closed}/_open")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "shards_acknowledged": True}
    assert _count(client, closed) == 1
    assert client.post(f"/{closed}/_open").json() == {"acknowledged": True, "shards_acknowledged": True}


@pytest.mark.parametrize("op", ["_open", "_close"])
def test_open_close_missing_index(client, pfx, op):
    r = client.post(f"/{pfx}-nope/{op}")
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"


def test_close_unmatched_wildcard(client, pfx):
    r = client.post(f"/{pfx}-zz*/_close")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "shards_acknowledged": False, "indices": {}}


def test_close_and_open_multiple(client, pfx):
    _mk(client, f"{pfx}-a")
    _mk(client, f"{pfx}-b")
    r = client.post(f"/{pfx}-a,{pfx}-b/_close")
    assert r.json()["indices"] == {f"{pfx}-a": {"closed": True}, f"{pfx}-b": {"closed": True}}
    assert client.get(f"/{pfx}-*").json() == {}
    assert client.post(f"/{pfx}-*/_open").status_code == 200
    assert set(client.get(f"/{pfx}-*").json()) == {f"{pfx}-a", f"{pfx}-b"}


# --------------------------------------------------------------------------
# blocks
# --------------------------------------------------------------------------


@pytest.fixture
def blk(client, pfx):
    _mk(client, pfx)
    _docs(client, pfx, {1: {"x": 1}})
    return pfx


def _set(client, index, **settings):
    r = client.put(f"/{index}/_settings", json=settings)
    assert r.status_code == 200, r.text


def test_write_block(client, blk):
    _set(client, blk, **{"index.blocks.write": True})
    for r in (
        client.put(f"/{blk}/_doc/3", json={"x": 1}),
        client.delete(f"/{blk}/_doc/1"),
        client.post(f"/{blk}/_update/1", json={"doc": {"x": 2}}),
    ):
        assert r.status_code == 403
        assert _err(r) == "cluster_block_exception"
        assert _reason(r) == f"index [{blk}] blocked by: [FORBIDDEN/8/index write (api)];"
    assert _count(client, blk) == 1


def test_write_block_bulk_item_error(client, blk):
    _set(client, blk, **{"index.blocks.write": True})
    r = client.post(
        f"/{blk}/_bulk",
        content=b'{"index":{"_id":"9"}}\n{"x":1}\n',
        headers={"Content-Type": "application/x-ndjson"},
    )
    assert r.status_code == 200
    assert r.json()["errors"] is True
    item = r.json()["items"][0]["index"]
    assert item["status"] == 403
    assert item["error"]["type"] == "cluster_block_exception"


def test_write_block_allows_mapping_update_and_reads(client, blk):
    _set(client, blk, **{"index.blocks.write": True})
    assert client.put(f"/{blk}/_mapping", json={"properties": {"z": {"type": "keyword"}}}).status_code == 200
    assert client.get(f"/{blk}/_doc/1").status_code == 200


def test_read_only_block(client, blk):
    _set(client, blk, **{"index.blocks.read_only": True})
    r = client.put(f"/{blk}/_doc/3", json={"x": 1})
    assert r.status_code == 403
    assert "FORBIDDEN/5/index read-only (api)" in _reason(r)
    r = client.put(f"/{blk}/_settings", json={"index.number_of_replicas": 0})
    assert r.status_code == 403
    assert _err(r) == "cluster_block_exception"
    assert _count(client, blk) == 1
    _set(client, blk, **{"index.blocks.read_only": False})
    assert client.put(f"/{blk}/_doc/3", json={"x": 1}).status_code == 201


def test_read_block(client, blk):
    _set(client, blk, **{"index.blocks.read": True})
    for r in (client.get(f"/{blk}/_search"), client.get(f"/{blk}/_doc/1")):
        assert r.status_code == 403
        assert _reason(r) == f"index [{blk}] blocked by: [FORBIDDEN/7/index read (api)];"
    assert client.put(f"/{blk}/_doc/4", json={"x": 1}).status_code == 201


def test_metadata_block(client, blk):
    _set(client, blk, **{"index.blocks.metadata": True})
    for r in (client.get(f"/{blk}/_settings"), client.get(f"/{blk}/_mapping")):
        assert r.status_code == 403
        assert "FORBIDDEN/9/index metadata (api)" in _reason(r)
    assert client.get(f"/{blk}/_doc/1").status_code == 200


def test_read_only_allow_delete_block_is_429(client, blk):
    _set(client, blk, **{"index.blocks.read_only_allow_delete": True})
    for r in (client.put(f"/{blk}/_doc/5", json={"x": 1}), client.delete(f"/{blk}/_doc/1")):
        assert r.status_code == 429
        assert _err(r) == "cluster_block_exception"
        assert "TOO_MANY_REQUESTS/12/disk usage exceeded flood-stage watermark" in _reason(r)


def test_add_block_api(client, blk):
    r = client.put(f"/{blk}/_block/write")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "shards_acknowledged": True, "indices": [{"name": blk, "blocked": True}]}
    assert _flat(client, blk)["index.blocks.write"] == "true"
    assert client.put(f"/{blk}/_doc/6", json={"x": 1}).status_code == 403


def test_add_block_api_unknown_block(client, blk):
    r = client.put(f"/{blk}/_block/bogus")
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert _reason(r) == "No block found with name bogus"


def test_block_setting_at_creation(client, pfx):
    _mk(client, pfx, settings={"index.blocks.write": True})
    r = client.put(f"/{pfx}/_doc/1", json={"x": 1})
    assert r.status_code == 403


# --------------------------------------------------------------------------
# composable index templates / component templates
# --------------------------------------------------------------------------


def _ct(client, objs, name, body):
    r = client.put(f"/_component_template/{name}", json=body)
    assert r.status_code == 200, r.text
    objs["component_template"].append(name)


def _it(client, objs, name, body):
    r = client.put(f"/_index_template/{name}", json=body)
    if r.status_code == 200:
        objs["index_template"].insert(0, name)
    return r


@pytest.fixture
def composed(client, pfx, cluster_objs):
    _ct(client, cluster_objs, f"{pfx}-ct-a", {
        "template": {"settings": {"number_of_shards": 1}, "mappings": {"properties": {"a": {"type": "keyword"}}}},
        "version": 3,
        "_meta": {"o": "x"},
    })
    _ct(client, cluster_objs, f"{pfx}-ct-b", {
        "template": {"mappings": {"properties": {"b": {"type": "long"}, "a": {"type": "text"}}}, "aliases": {f"{pfx}-tal": {}}},
    })
    r = _it(client, cluster_objs, f"{pfx}-low", {
        "index_patterns": [f"{pfx}-t*"],
        "priority": 1,
        "template": {"settings": {"number_of_replicas": 0, "index.refresh_interval": "7s"}},
    })
    assert r.status_code == 200, r.text
    r = _it(client, cluster_objs, f"{pfx}-high", {
        "index_patterns": [f"{pfx}-t*"],
        "priority": 10,
        "composed_of": [f"{pfx}-ct-a", f"{pfx}-ct-b"],
        "template": {"settings": {"number_of_replicas": 0}, "mappings": {"properties": {"c": {"type": "date"}}}},
        "version": 7,
        "_meta": {"m": 1},
    })
    assert r.status_code == 200, r.text
    return pfx


def test_component_template_get_shape(client, composed):
    p = composed
    r = client.get(f"/_component_template/{p}-ct-a")
    assert r.status_code == 200
    assert r.json() == {
        "component_templates": [{
            "name": f"{p}-ct-a",
            "component_template": {
                "template": {
                    "settings": {"index": {"number_of_shards": "1"}},
                    "mappings": {"properties": {"a": {"type": "keyword"}}},
                },
                "version": 3,
                "_meta": {"o": "x"},
            },
        }]
    }


def test_component_template_missing(client, pfx):
    r = client.get(f"/_component_template/{pfx}-nope")
    assert r.status_code == 404
    assert _err(r) == "resource_not_found_exception"
    assert _reason(r) == f"component template matching [{pfx}-nope] not found"
    assert client.head(f"/_component_template/{pfx}-nope").status_code == 404


def test_component_template_head_and_wildcard(client, composed):
    p = composed
    assert client.head(f"/_component_template/{p}-ct-a").status_code == 200
    names = [t["name"] for t in client.get(f"/_component_template/{p}-ct-*").json()["component_templates"]]
    assert sorted(names) == [f"{p}-ct-a", f"{p}-ct-b"]


def test_index_template_get_shape(client, composed):
    p = composed
    r = client.get(f"/_index_template/{p}-high")
    assert r.json() == {
        "index_templates": [{
            "name": f"{p}-high",
            "index_template": {
                "index_patterns": [f"{p}-t*"],
                "template": {
                    "settings": {"index": {"number_of_replicas": "0"}},
                    "mappings": {"properties": {"c": {"type": "date"}}},
                },
                "composed_of": [f"{p}-ct-a", f"{p}-ct-b"],
                "priority": 10,
                "version": 7,
                "_meta": {"m": 1},
            },
        }]
    }


def test_index_template_wildcard_get_and_head(client, composed):
    p = composed
    r = client.get(f"/_index_template/{p}-*", params={"filter_path": "index_templates.name"})
    assert sorted(t["name"] for t in r.json()["index_templates"]) == [f"{p}-high", f"{p}-low"]
    assert client.head(f"/_index_template/{p}-high").status_code == 200
    assert client.head(f"/_index_template/{p}-nope").status_code == 404


def test_index_template_missing(client, pfx):
    r = client.get(f"/_index_template/{pfx}-nope")
    assert r.status_code == 404
    assert _err(r) == "resource_not_found_exception"
    assert _reason(r) == f"index template matching [{pfx}-nope] not found"
    r = client.delete(f"/_index_template/{pfx}-nope")
    assert r.status_code == 404
    assert _err(r) == "index_template_missing_exception"
    assert _reason(r) == f"index_template [{pfx}-nope] missing"


def test_component_template_delete_missing(client, pfx):
    r = client.delete(f"/_component_template/{pfx}-nope")
    assert r.status_code == 404
    assert _err(r) == "index_template_missing_exception"


def test_template_applied_highest_priority_only(client, composed):
    p = composed
    assert client.put(f"/{p}-tx").status_code == 200
    body = client.get(f"/{p}-tx").json()[f"{p}-tx"]
    assert body["aliases"] == {f"{p}-tal": {}}
    # later component overrides earlier one ('a' becomes text), own template adds 'c'
    assert body["mappings"] == {"properties": {"a": {"type": "text"}, "b": {"type": "long"}, "c": {"type": "date"}}}
    s = body["settings"]["index"]
    assert s["number_of_shards"] == "1"
    assert s["number_of_replicas"] == "0"
    # the low-priority template is NOT merged in
    assert "refresh_interval" not in s


def test_create_request_overrides_template(client, composed):
    p = composed
    r = client.put(f"/{p}-ty", json={"mappings": {"properties": {"a": {"type": "integer"}}}})
    assert r.status_code == 200
    props = client.get(f"/{p}-ty/_mapping").json()[f"{p}-ty"]["mappings"]["properties"]
    assert props == {"a": {"type": "integer"}, "b": {"type": "long"}, "c": {"type": "date"}}


def test_simulate_index(client, composed):
    p = composed
    r = client.post(f"/_index_template/_simulate_index/{p}-tz")
    assert r.status_code == 200
    assert r.json() == {
        "template": {
            "settings": {"index": {"number_of_shards": "1", "number_of_replicas": "0"}},
            "mappings": {"properties": {"a": {"type": "text"}, "b": {"type": "long"}, "c": {"type": "date"}}},
            "aliases": {f"{p}-tal": {}},
        },
        "overlapping": [{"name": f"{p}-low", "index_patterns": [f"{p}-t*"]}],
    }
    # simulation does not create the index
    assert client.head(f"/{p}-tz").status_code == 404


def test_simulate_named_template(client, composed):
    p = composed
    r = client.post(f"/_index_template/_simulate/{p}-high")
    assert r.status_code == 200
    assert r.json()["template"]["aliases"] == {f"{p}-tal": {}}
    assert r.json()["overlapping"] == [{"name": f"{p}-low", "index_patterns": [f"{p}-t*"]}]


def test_simulate_template_body(client, pfx):
    r = client.post(
        "/_index_template/_simulate",
        json={"index_patterns": [f"{pfx}-sim*"], "template": {"settings": {"number_of_shards": 3}}},
    )
    assert r.status_code == 200
    assert r.json() == {"template": {"settings": {"index": {"number_of_shards": "3"}}, "aliases": {}}, "overlapping": []}


def test_simulate_index_no_match(client, pfx):
    r = client.post(f"/_index_template/_simulate_index/{pfx}-nomatch")
    assert r.status_code == 200


def test_same_priority_overlap_rejected(client, composed, cluster_objs):
    p = composed
    r = _it(client, cluster_objs, f"{p}-same", {"index_patterns": [f"{p}-t*"], "priority": 10})
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "that have the same priority [10]" in _reason(r)


def test_template_missing_component(client, pfx, cluster_objs):
    r = _it(client, cluster_objs, f"{pfx}-mc", {"index_patterns": [f"{pfx}-mc*"], "composed_of": [f"{pfx}-nope"]})
    assert r.status_code == 400
    assert _err(r) == "invalid_index_template_exception"
    assert f"specifies component templates [{pfx}-nope] that do not exist" in _reason(r)


def test_delete_component_in_use(client, composed):
    p = composed
    r = client.delete(f"/_component_template/{p}-ct-a")
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "cannot be removed as they are still in use by index templates" in _reason(r)


def test_delete_index_template(client, composed):
    p = composed
    r = client.delete(f"/_index_template/{p}-high")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    assert client.head(f"/_index_template/{p}-high").status_code == 404
    # now the low-priority template applies
    client.put(f"/{p}-tq")
    assert _flat(client, f"{p}-tq")["index.refresh_interval"] == "7s"


@pytest.mark.parametrize(
    "body,etype,reason_part",
    [
        ({"index_patterns": ["P-*"], "template": {"settings": {"index.number_of_shards": 0}}}, "invalid_index_template_exception", "must be >= 1"),
        ({"template": {}}, "illegal_argument_exception", "Required [index_patterns]"),
        ({"index_patterns": ["P-*"], "bogus": 1}, "x_content_parse_exception", "unknown field [bogus]"),
        ({"index_patterns": ["_bad*"]}, "invalid_index_template_exception", "index_pattern [_bad*] must not start with '_'"),
        ({"index_patterns": ["P-*"], "priority": -1}, "action_request_validation_exception", "index template priority must be >= 0"),
    ],
)
def test_index_template_validation(client, pfx, cluster_objs, body, etype, reason_part):
    body = json.loads(json.dumps(body).replace("P-*", f"{pfx}-v*"))
    r = _it(client, cluster_objs, f"{pfx}-v", body)
    assert r.status_code == 400
    assert _err(r) == etype
    assert reason_part in _reason(r)


def test_index_template_uppercase_name(client, pfx, cluster_objs):
    r = _it(client, cluster_objs, f"{pfx}-UP", {"index_patterns": [f"{pfx}-up*"]})
    assert r.status_code == 400
    assert _err(r) == "invalid_index_template_exception"
    assert "name must be lower cased" in _reason(r)


def test_index_template_create_flag(client, pfx, cluster_objs):
    name = f"{pfx}-c1"
    r = client.put(f"/_index_template/{name}", json={"index_patterns": [f"{pfx}-c1*"]}, params={"create": "true"})
    assert r.status_code == 200
    cluster_objs["index_template"].append(name)
    r = client.put(f"/_index_template/{name}", json={"index_patterns": [f"{pfx}-c1*"]}, params={"create": "true"})
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert _reason(r) == f"index template [{name}] already exists"
    # without create=true it is an update
    r = client.put(f"/_index_template/{name}", json={"index_patterns": [f"{pfx}-c2*"]})
    assert r.status_code == 200
    got = client.get(f"/_index_template/{name}").json()["index_templates"][0]["index_template"]
    assert got["index_patterns"] == [f"{pfx}-c2*"]


# --------------------------------------------------------------------------
# legacy templates
# --------------------------------------------------------------------------


@pytest.fixture
def legacy(client, pfx, cluster_objs):
    r = client.put(f"/_template/{pfx}-la", json={
        "index_patterns": [f"{pfx}-l*"],
        "order": 1,
        "settings": {"number_of_shards": 1, "number_of_replicas": 0},
        "mappings": {"properties": {"l": {"type": "keyword"}}},
        "aliases": {f"{pfx}-lalias": {}, "{index}-al": {}},
    })
    assert r.status_code == 200, r.text
    cluster_objs["template"].append(f"{pfx}-la")
    r = client.put(f"/_template/{pfx}-lb", json={
        "index_patterns": [f"{pfx}-l*"],
        "order": 2,
        "mappings": {"properties": {"m": {"type": "long"}}},
        "version": 5,
    })
    assert r.status_code == 200, r.text
    cluster_objs["template"].append(f"{pfx}-lb")
    return pfx


def test_legacy_template_get_shape(client, legacy):
    p = legacy
    r = client.get(f"/_template/{p}-la")
    assert r.json() == {
        f"{p}-la": {
            "order": 1,
            "index_patterns": [f"{p}-l*"],
            "settings": {"index": {"number_of_shards": "1", "number_of_replicas": "0"}},
            "mappings": {"properties": {"l": {"type": "keyword"}}},
            "aliases": {f"{p}-lalias": {}, "{index}-al": {}},
        }
    }
    assert client.get(f"/_template/{p}-lb").json()[f"{p}-lb"]["version"] == 5


def test_legacy_templates_merge_by_order(client, legacy):
    p = legacy
    client.put(f"/{p}-lx")
    body = client.get(f"/{p}-lx").json()[f"{p}-lx"]
    assert body["mappings"] == {"properties": {"l": {"type": "keyword"}, "m": {"type": "long"}}}
    # {index} placeholder resolved in alias names
    assert body["aliases"] == {f"{p}-lalias": {}, f"{p}-lx-al": {}}


def test_legacy_template_missing(client, pfx):
    r = client.get(f"/_template/{pfx}-nope")
    assert r.status_code == 404
    assert r.json() == {}
    assert client.head(f"/_template/{pfx}-nope").status_code == 404
    r = client.delete(f"/_template/{pfx}-nope")
    assert r.status_code == 404
    assert _err(r) == "index_template_missing_exception"


def test_legacy_template_head_and_wildcard(client, legacy):
    p = legacy
    assert client.head(f"/_template/{p}-la").status_code == 200
    assert set(client.get(f"/_template/{p}-l*").json()) == {f"{p}-la", f"{p}-lb"}


def test_composable_template_wins_over_legacy(client, legacy, cluster_objs):
    p = legacy
    r = _it(client, cluster_objs, f"{p}-v2", {
        "index_patterns": [f"{p}-l*"],
        "template": {"mappings": {"properties": {"v2": {"type": "keyword"}}}},
    })
    assert r.status_code == 200
    client.put(f"/{p}-ly")
    assert client.get(f"/{p}-ly/_mapping").json() == {f"{p}-ly": {"mappings": {"properties": {"v2": {"type": "keyword"}}}}}
    sim = client.post(f"/_index_template/_simulate_index/{p}-lz").json()
    assert sorted(o["name"] for o in sim["overlapping"]) == [f"{p}-la", f"{p}-lb"]


def test_legacy_template_requires_patterns(client, pfx):
    r = client.put(f"/_template/{pfx}-lc", json={"settings": {}})
    assert r.status_code == 400
    assert _err(r) == "action_request_validation_exception"
    assert "index patterns are missing" in _reason(r)


def test_legacy_template_string_pattern_and_create(client, pfx, cluster_objs):
    name = f"{pfx}-lc"
    r = client.put(f"/_template/{name}", json={"index_patterns": f"{pfx}-lc*"}, params={"create": "true"})
    assert r.status_code == 200
    cluster_objs["template"].append(name)
    assert client.get(f"/_template/{name}").json() == {
        name: {"order": 0, "index_patterns": [f"{pfx}-lc*"], "settings": {}, "mappings": {}, "aliases": {}}
    }
    r = client.put(f"/_template/{name}", json={"index_patterns": f"{pfx}-lc*"}, params={"create": "true"})
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert _reason(r) == f"index_template [{name}] already exists"


# --------------------------------------------------------------------------
# rollover
# --------------------------------------------------------------------------


@pytest.fixture
def ro(client, pfx):
    _mk(client, f"{pfx}-000001", aliases={f"{pfx}-w": {"is_write_index": True}})
    r = client.post(f"/{pfx}-w/_doc", json={"x": 1}, params={"refresh": "true"})
    assert r.status_code == 201
    return pfx


def test_rollover_condition_not_met(client, ro):
    r = _rollover(client, f"{ro}-w", {"conditions": {"max_docs": 5}})
    assert r.status_code == 200
    assert r.json() == {
        "acknowledged": False,
        "shards_acknowledged": False,
        "old_index": f"{ro}-000001",
        "new_index": f"{ro}-000002",
        "rolled_over": False,
        "dry_run": False,
        "conditions": {"[max_docs: 5]": False},
    }
    assert client.head(f"/{ro}-000002").status_code == 404


def test_rollover_dry_run(client, ro):
    r = _rollover(client, f"{ro}-w", {"conditions": {"max_docs": 1}}, dry_run="true")
    assert r.json()["dry_run"] is True
    assert r.json()["rolled_over"] is False
    assert r.json()["conditions"] == {"[max_docs: 1]": True}
    assert client.head(f"/{ro}-000002").status_code == 404


def test_rollover_condition_met(client, ro):
    r = _rollover(client, f"{ro}-w", {"conditions": {"max_docs": 1, "max_age": "1d"}})
    body = r.json()
    assert body["rolled_over"] is True
    assert body["acknowledged"] is True
    assert body["new_index"] == f"{ro}-000002"
    assert body["conditions"] == {"[max_docs: 1]": True, "[max_age: 1d]": False}
    assert client.get(f"/_alias/{ro}-w").json() == {
        f"{ro}-000001": {"aliases": {f"{ro}-w": {"is_write_index": False}}},
        f"{ro}-000002": {"aliases": {f"{ro}-w": {"is_write_index": True}}},
    }
    r = client.post(f"/{ro}-w/_doc", json={"x": 2})
    assert r.json()["_index"] == f"{ro}-000002"


def test_rollover_unconditional_and_custom_name(client, ro):
    r = _rollover(client, f"{ro}-w")
    assert r.json()["rolled_over"] is True
    assert r.json()["conditions"] == {}
    assert r.json()["new_index"] == f"{ro}-000002"
    r = _rollover(client, f"{ro}-w", new_index=f"{ro}-custom")
    assert r.json()["old_index"] == f"{ro}-000002"
    assert r.json()["new_index"] == f"{ro}-custom"
    # next auto-named rollover fails since name has no numeric suffix
    r = _rollover(client, f"{ro}-w")
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert f"index name [{ro}-custom] does not match pattern" in _reason(r)


def test_rollover_counter_increments_digits(client, pfx):
    _mk(client, f"{pfx}-000009", aliases={f"{pfx}-w": {}})
    r = _rollover(client, f"{pfx}-w")
    assert r.json()["new_index"] == f"{pfx}-000010"


def test_rollover_alias_without_write_flag_moves_alias(client, pfx):
    _mk(client, f"{pfx}-000001", aliases={f"{pfx}-w": {}})
    _rollover(client, f"{pfx}-w")
    assert client.get(f"/_alias/{pfx}-w").json() == {f"{pfx}-000002": {"aliases": {f"{pfx}-w": {}}}}


def test_rollover_non_numeric_name_needs_explicit_target(client, pfx):
    _mk(client, f"{pfx}-a", aliases={f"{pfx}-w": {}})
    r = _rollover(client, f"{pfx}-w")
    assert r.status_code == 400
    assert "does not match pattern '^.*-\\d+$'" in _reason(r)
    r = _rollover(client, f"{pfx}-w", new_index=f"{pfx}-b")
    assert r.status_code == 200
    assert r.json()["new_index"] == f"{pfx}-b"


def test_rollover_errors(client, pfx):
    _mk(client, f"{pfx}-000001")
    r = _rollover(client, f"{pfx}-000001")
    assert r.status_code == 400
    assert _reason(r) == "rollover target is a [concrete index] but one of [alias,data_stream] was expected"
    r = _rollover(client, f"{pfx}-nope")
    assert r.status_code == 400
    assert _reason(r) == f"rollover target [{pfx}-nope] does not exist"


def test_rollover_alias_multiple_indices_no_write_index(client, pfx):
    _mk(client, f"{pfx}-1", aliases={f"{pfx}-w": {}})
    _mk(client, f"{pfx}-2", aliases={f"{pfx}-w": {}})
    r = _rollover(client, f"{pfx}-w")
    assert r.status_code == 400
    assert _reason(r) == f"rollover target [{pfx}-w] does not point to a write index"


def test_rollover_unknown_condition(client, ro):
    r = _rollover(client, f"{ro}-w", {"conditions": {"bogus": 1}})
    assert r.status_code == 400
    assert _err(r) == "x_content_parse_exception"


# --------------------------------------------------------------------------
# data streams
# --------------------------------------------------------------------------


@pytest.fixture
def ds_tpl(client, pfx, cluster_objs):
    r = _it(client, cluster_objs, f"{pfx}-dst", {
        "index_patterns": [f"{pfx}-ds*"],
        "data_stream": {},
        "priority": 50,
        "template": {"settings": SINGLE},
    })
    assert r.status_code == 200, r.text
    return pfx


def test_data_stream_create_and_get(client, ds_tpl):
    p = ds_tpl
    r = client.put(f"/_data_stream/{p}-ds")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    ds = client.get(f"/_data_stream/{p}-ds").json()["data_streams"]
    assert len(ds) == 1
    d = ds[0]
    assert d["name"] == f"{p}-ds"
    assert d["timestamp_field"] == {"name": "@timestamp"}
    assert [i["index_name"] for i in d["indices"]] == [f".ds-{p}-ds-000001"]
    assert d["generation"] == 1
    assert d["status"] == "GREEN"
    assert d["template"] == f"{p}-dst"


def test_data_stream_create_twice(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    r = client.put(f"/_data_stream/{p}-ds")
    assert r.status_code == 400
    assert _err(r) == "resource_already_exists_exception"
    assert _reason(r) == f"data_stream [{p}-ds] already exists"


def test_data_stream_requires_template(client, pfx):
    r = client.put(f"/_data_stream/{pfx}-nods")
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert _reason(r) == f"no matching index template found for data stream [{pfx}-nods]"


def test_data_stream_uppercase_name(client, ds_tpl):
    r = client.put(f"/_data_stream/{ds_tpl}-dsUP")
    assert r.status_code == 400
    assert _reason(r) == f"data_stream [{ds_tpl}-dsUP] must be lowercase"


def test_data_stream_backing_index(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    body = client.get(f"/{p}-ds").json()
    backing = f".ds-{p}-ds-000001"
    assert list(body) == [backing]
    idx = body[backing]
    assert idx["data_stream"] == f"{p}-ds"
    assert idx["settings"]["index"]["hidden"] == "true"
    assert idx["mappings"] == {"_data_stream_timestamp": {"enabled": True}, "properties": {"@timestamp": {"type": "date"}}}


def test_data_stream_indexing_rules(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    r = client.post(f"/{p}-ds/_doc", json={"@timestamp": "2024-01-01T00:00:00Z", "m": 1}, params={"refresh": "true"})
    assert r.status_code == 201
    assert r.json()["_index"] == f".ds-{p}-ds-000001"
    r = client.post(f"/{p}-ds/_doc", json={"m": 1})
    assert r.status_code == 400
    assert _err(r) == "mapper_parsing_exception"
    assert "documents must contain a single-valued timestamp field '@timestamp' of date type" in r.json()["error"]["caused_by"]["reason"]
    r = client.put(f"/{p}-ds/_doc/1", json={"@timestamp": "2024-01-01T00:00:00Z"})
    assert r.status_code == 400
    assert _reason(r) == "only write ops with an op_type of create are allowed in data streams"
    r = client.put(f"/{p}-ds/_create/1", json={"@timestamp": "2024-01-01T00:00:00Z"}, params={"refresh": "true"})
    assert r.status_code == 201
    assert _count(client, f"{p}-ds") == 2


def test_data_stream_rollover(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    r = _rollover(client, f"{p}-ds")
    assert r.json()["old_index"] == f".ds-{p}-ds-000001"
    assert r.json()["new_index"] == f".ds-{p}-ds-000002"
    d = client.get(f"/_data_stream/{p}-ds").json()["data_streams"][0]
    assert d["generation"] == 2
    assert [i["index_name"] for i in d["indices"]] == [f".ds-{p}-ds-000001", f".ds-{p}-ds-000002"]
    r = client.delete(f"/.ds-{p}-ds-000002")
    assert r.status_code == 400
    assert "is the write index for data stream" in _reason(r)


def test_data_stream_auto_created_on_index(client, ds_tpl):
    p = ds_tpl
    r = client.post(f"/{p}-dsauto/_doc", json={"@timestamp": "2024-01-01"})
    assert r.status_code == 201
    assert r.json()["_index"] == f".ds-{p}-dsauto-000001"
    assert client.get(f"/_data_stream/{p}-dsauto").json()["data_streams"][0]["name"] == f"{p}-dsauto"


def test_data_stream_template_blocks_regular_index(client, ds_tpl):
    p = ds_tpl
    r = client.put(f"/{p}-dsx")
    assert r.status_code == 400
    assert "matches with template" in _reason(r)
    assert "use create data stream api instead" in _reason(r)


def test_data_stream_get_missing_and_wildcard(client, ds_tpl):
    p = ds_tpl
    r = client.get(f"/_data_stream/{p}-dsnope")
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"
    r = client.get(f"/_data_stream/{p}-dszz*")
    assert r.status_code == 200
    assert r.json() == {"data_streams": []}


def test_data_stream_delete(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    # deleting via index API does not find the data stream
    r = client.delete(f"/{p}-ds")
    assert r.status_code == 404
    r = client.delete(f"/_data_stream/{p}-ds")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    assert client.head(f"/.ds-{p}-ds-000001").status_code == 404
    assert client.get(f"/_data_stream/{p}-ds").status_code == 404


def test_template_in_use_by_data_stream_cannot_be_deleted(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    r = client.delete(f"/_index_template/{p}-dst")
    assert r.status_code == 400
    assert "as they are in use by a data streams" in _reason(r)


def test_data_stream_custom_timestamp_field(client, pfx, cluster_objs):
    r = _it(client, cluster_objs, f"{pfx}-dst", {
        "index_patterns": [f"{pfx}-ds*"],
        "data_stream": {"timestamp_field": {"name": "ts"}},
        "priority": 50,
        "template": {"settings": SINGLE},
    })
    assert r.status_code == 200
    client.put(f"/_data_stream/{pfx}-ds")
    assert client.get(f"/_data_stream/{pfx}-ds").json()["data_streams"][0]["timestamp_field"] == {"name": "ts"}
    r = client.post(f"/{pfx}-ds/_doc", json={"@timestamp": "2024-01-01T00:00:00Z"})
    assert r.status_code == 400
    r = client.post(f"/{pfx}-ds/_doc", json={"ts": "2024-01-01T00:00:00Z"})
    assert r.status_code == 201


def test_data_stream_stats(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    client.post(f"/{p}-ds/_doc", json={"@timestamp": "2024-01-01T00:00:00Z"}, params={"refresh": "true"})
    body = client.get(f"/_data_stream/{p}-ds/_stats").json()
    assert body["data_stream_count"] == 1
    assert body["backing_indices"] == 1
    assert body["data_streams"][0]["data_stream"] == f"{p}-ds"
    assert body["data_streams"][0]["maximum_timestamp"] == 1704067200000


# --------------------------------------------------------------------------
# refresh / flush / forcemerge / cache
# --------------------------------------------------------------------------

SHARDS_1 = {"total": 1, "successful": 1, "failed": 0}


@pytest.mark.parametrize(
    "method,path,params",
    [
        ("POST", "_refresh", {}),
        ("GET", "_refresh", {}),
        ("POST", "_flush", {}),
        ("POST", "_flush", {"wait_if_ongoing": "true", "force": "true"}),
        ("POST", "_forcemerge", {"max_num_segments": "1"}),
        ("POST", "_forcemerge", {"only_expunge_deletes": "true"}),
        ("POST", "_cache/clear", {}),
        ("POST", "_cache/clear", {"query": "true", "fielddata": "true", "request": "true"}),
        ("POST", "_cache/clear", {"fields": "n"}),
    ],
)
def test_maintenance_ops_shape(client, pfx, method, path, params):
    _mk(client, pfx)
    _docs(client, pfx, {1: {"n": 1}})
    r = client.request(method, f"/{pfx}/{path}", params=params)
    assert r.status_code == 200
    assert r.json() == {"_shards": SHARDS_1}


@pytest.mark.parametrize(
    "method,path",
    [
        ("POST", "_refresh"),
        ("POST", "_flush"),
        ("POST", "_forcemerge"),
        ("POST", "_cache/clear"),
        ("GET", "_stats"),
        ("GET", "_segments"),
        ("GET", "_recovery"),
    ],
)
def test_maintenance_ops_missing_index(client, pfx, method, path):
    r = client.request(method, f"/{pfx}-nope/{path}")
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"


def test_refresh_unmatched_wildcard(client, pfx):
    r = client.post(f"/{pfx}-zz*/_refresh")
    assert r.status_code == 200
    assert r.json() == {"_shards": {"total": 0, "successful": 0, "failed": 0}}


def test_refresh_makes_docs_visible(client, pfx):
    _mk(client, pfx, settings={"refresh_interval": "-1"})
    client.put(f"/{pfx}/_doc/1", json={"x": 1})
    assert _count(client, pfx) == 0
    client.post(f"/{pfx}/_refresh")
    assert _count(client, pfx) == 1


def test_refresh_multi_shard_counts(client, pfx):
    _mk(client, pfx, settings={"number_of_shards": 3})
    r = client.post(f"/{pfx}/_refresh")
    assert r.json() == {"_shards": {"total": 3, "successful": 3, "failed": 0}}


# --------------------------------------------------------------------------
# stats / segments / recovery
# --------------------------------------------------------------------------


@pytest.fixture
def statted(client, pfx):
    _mk(client, pfx)
    _docs(client, pfx, {i: {"n": i} for i in range(5)})
    assert client.delete(f"/{pfx}/_doc/0", params={"refresh": "true"}).status_code == 200
    return pfx


def test_stats_shape(client, statted):
    body = client.get(f"/{statted}/_stats").json()
    assert set(body) == {"_shards", "_all", "indices"}
    assert body["_shards"] == SHARDS_1
    assert set(body["_all"]) == {"primaries", "total"}
    idx = body["indices"][statted]
    assert set(idx) == {"uuid", "primaries", "total"}
    for metric in ("docs", "store", "indexing", "get", "search", "merges", "refresh", "flush",
                   "warmer", "query_cache", "fielddata", "completion", "segments", "translog",
                   "request_cache", "recovery"):
        assert metric in idx["primaries"], metric
    assert idx["primaries"]["docs"]["count"] == 4
    assert idx["primaries"]["indexing"]["index_total"] == 5
    assert idx["primaries"]["indexing"]["delete_total"] == 1


def test_stats_metric_filter(client, statted):
    body = client.get(f"/{statted}/_stats/docs").json()
    assert set(body["_all"]["primaries"]) == {"docs"}
    assert body["_all"]["primaries"]["docs"]["count"] == 4
    assert set(body["indices"][statted]["total"]) == {"docs"}
    body = client.get(f"/{statted}/_stats/docs,store").json()
    assert set(body["_all"]["primaries"]) == {"docs", "store"}
    assert "size_in_bytes" in body["_all"]["primaries"]["store"]


def test_stats_all_metric(client, statted):
    body = client.get(f"/{statted}/_stats/_all").json()
    assert "indexing" in body["_all"]["primaries"]


def test_stats_unknown_metric(client, statted):
    r = client.get(f"/{statted}/_stats/bogus")
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert "contains unrecognized metric: [bogus]" in _reason(r)


@pytest.mark.parametrize("level,has_indices,has_shards", [("cluster", False, False), ("indices", True, False), ("shards", True, True)])
def test_stats_level(client, statted, level, has_indices, has_shards):
    body = client.get(f"/{statted}/_stats/docs", params={"level": level}).json()
    assert ("indices" in body) is has_indices
    if has_indices:
        assert ("shards" in body["indices"][statted]) is has_shards
    if has_shards:
        shard = body["indices"][statted]["shards"]["0"]
        assert shard[0]["routing"]["primary"] is True


def test_stats_invalid_level(client, statted):
    r = client.get(f"/{statted}/_stats", params={"level": "bogus"})
    assert r.status_code == 400
    assert "level parameter must be one of [cluster] or [indices] or [shards] but was [bogus]" in _reason(r)


def test_stats_multiple_indices_sum(client, pfx):
    _mk(client, f"{pfx}-a")
    _mk(client, f"{pfx}-b")
    _docs(client, f"{pfx}-a", {1: {}, 2: {}})
    _docs(client, f"{pfx}-b", {1: {}})
    body = client.get(f"/{pfx}-*/_stats/docs").json()
    assert body["_all"]["primaries"]["docs"]["count"] == 3
    assert set(body["indices"]) == {f"{pfx}-a", f"{pfx}-b"}
    assert body["_shards"]["total"] == 2


def test_stats_hidden_index_needs_expand_wildcards(client, pfx):
    _mk(client, pfx, settings={"index.hidden": True})
    body = client.get(f"/{pfx}*/_stats/docs").json()
    assert body == {
        "_shards": {"total": 0, "successful": 0, "failed": 0},
        "_all": {"primaries": {}, "total": {}},
        "indices": {},
    }
    fp = f"indices.{pfx}.primaries.docs.count"
    body = client.get(f"/{pfx}*/_stats/docs", params={"filter_path": fp, "expand_wildcards": "all"}).json()
    assert body == {"indices": {pfx: {"primaries": {"docs": {"count": 0}}}}}
    assert client.get(f"/{pfx}/_stats/docs").json()["_shards"]["total"] == 1


def test_segments_shape_and_doc_sum(client, statted):
    r = client.get(f"/{statted}/_segments")
    assert r.status_code == 200
    body = r.json()
    assert body["_shards"] == SHARDS_1
    shard = body["indices"][statted]["shards"]["0"][0]
    assert shard["routing"]["state"] == "STARTED"
    assert shard["routing"]["primary"] is True
    assert {"num_committed_segments", "num_search_segments", "segments"} <= set(shard)
    segs = shard["segments"]
    assert segs
    for seg in segs.values():
        assert {"generation", "num_docs", "deleted_docs", "size_in_bytes", "committed", "search", "compound"} <= set(seg)
    assert sum(s["num_docs"] for s in segs.values() if s["search"]) == 4


def test_forcemerge_single_segment(client, statted):
    client.post(f"/{statted}/_forcemerge", params={"max_num_segments": "1"})
    client.post(f"/{statted}/_refresh")
    shard = client.get(f"/{statted}/_segments").json()["indices"][statted]["shards"]["0"][0]
    searchable = [s for s in shard["segments"].values() if s["search"]]
    assert len(searchable) == 1
    assert searchable[0]["num_docs"] == 4


def test_recovery_shape(client, statted):
    body = client.get(f"/{statted}/_recovery").json()
    shards = body[statted]["shards"]
    assert len(shards) == 1
    s = shards[0]
    assert s["id"] == 0
    assert s["type"] == "EMPTY_STORE"
    assert s["stage"] == "DONE"
    assert s["primary"] is True
    assert {"index", "translog", "verify_index", "target", "source"} <= set(s)


def test_recovery_active_only(client, statted):
    r = client.get(f"/{statted}/_recovery", params={"active_only": "true"})
    assert r.status_code == 200
    assert r.json() == {}


# --------------------------------------------------------------------------
# clone / shrink / split
# --------------------------------------------------------------------------


@pytest.fixture
def rsrc(client, pfx):
    src = _mk(client, f"{pfx}-src", settings={"number_of_shards": 2}, mappings={"properties": {"n": {"type": "integer"}}})
    _docs(client, src, {i: {"n": i} for i in range(6)})
    return src


@pytest.mark.parametrize(
    "op,body",
    [("_clone", None), ("_shrink", None), ("_split", {"settings": {"index.number_of_shards": 4}})],
)
def test_resize_requires_write_block(client, pfx, rsrc, op, body):
    r = client.post(f"/{rsrc}/{op}/{pfx}-dst", json=body) if body else client.post(f"/{rsrc}/{op}/{pfx}-dst")
    assert r.status_code == 500
    assert _err(r) == "illegal_state_exception"
    assert f"index {rsrc} must block write operations to resize index" in _reason(r)
    assert client.head(f"/{pfx}-dst").status_code == 404


def test_clone(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(f"/{rsrc}/_clone/{pfx}-clone")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True, "shards_acknowledged": True, "index": f"{pfx}-clone"}
    assert _count(client, f"{pfx}-clone") == 6
    flat = _flat(client, f"{pfx}-clone")
    assert flat["index.number_of_shards"] == "2"
    assert flat["index.resize.source.name"] == rsrc
    # write block is copied to the target
    assert flat["index.blocks.write"] == "true"
    assert client.put(f"/{pfx}-clone/_doc/x", json={"n": 1}).status_code == 403
    assert client.get(f"/{pfx}-clone/_mapping").json()[f"{pfx}-clone"]["mappings"] == {"properties": {"n": {"type": "integer"}}}


def test_clone_can_unset_write_block(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(f"/{rsrc}/_clone/{pfx}-clone", json={"settings": {"index.blocks.write": None}})
    assert r.status_code == 200
    assert client.put(f"/{pfx}-clone/_doc/x", json={"n": 1}).status_code == 201


def test_clone_shard_count_must_match(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(f"/{rsrc}/_clone/{pfx}-c2", json={"settings": {"index.number_of_shards": 3}})
    assert r.status_code == 400
    assert _reason(r) == "the number of target shards (3) must be the same as the number of source shards (2)"


def test_clone_target_exists(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    _mk(client, f"{pfx}-exists")
    r = client.post(f"/{rsrc}/_clone/{pfx}-exists")
    assert r.status_code == 400
    assert _err(r) == "resource_already_exists_exception"


def test_clone_invalid_target_name(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(f"/{rsrc}/_clone/{pfx}-UP")
    assert r.status_code == 400
    assert _err(r) == "invalid_index_name_exception"


def test_shrink(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(
        f"/{rsrc}/_shrink/{pfx}-shrink",
        json={"settings": {"index.number_of_shards": 1, "index.number_of_replicas": 0}, "aliases": {f"{pfx}-sal": {}}},
    )
    assert r.status_code == 200
    assert _count(client, f"{pfx}-shrink") == 6
    assert _count(client, f"{pfx}-sal") == 6
    assert _flat(client, f"{pfx}-shrink")["index.number_of_shards"] == "1"


def test_shrink_default_target_is_one_shard(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    assert client.post(f"/{rsrc}/_shrink/{pfx}-shrink").status_code == 200
    assert _flat(client, f"{pfx}-shrink")["index.number_of_shards"] == "1"
    assert _count(client, f"{pfx}-shrink") == 6


def test_shrink_errors(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(f"/{rsrc}/_shrink/{pfx}-s3", json={"settings": {"index.number_of_shards": 3}})
    assert r.status_code == 400
    assert _reason(r) == "the number of target shards [3] must be less that the number of source shards [2]"
    one = _mk(client, f"{pfx}-one", settings={"index.blocks.write": True})
    r = client.post(f"/{one}/_shrink/{pfx}-s1")
    assert r.status_code == 400
    assert _reason(r) == "can't shrink an index with only one shard"


def test_split(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(f"/{rsrc}/_split/{pfx}-split", json={"settings": {"index.number_of_shards": 4, "index.number_of_replicas": 0}})
    assert r.status_code == 200
    assert _count(client, f"{pfx}-split") == 6
    assert _flat(client, f"{pfx}-split")["index.number_of_shards"] == "4"
    r = client.get(f"/{pfx}-split/_doc/3")
    assert r.json()["_source"] == {"n": 3}


def test_split_errors(client, pfx, rsrc):
    _set(client, rsrc, **{"index.blocks.write": True})
    r = client.post(f"/{rsrc}/_split/{pfx}-s3", json={"settings": {"index.number_of_shards": 3}})
    assert r.status_code == 400
    assert _reason(r) == "the number of source shards [2] must be a factor of [3]"
    r = client.post(f"/{rsrc}/_split/{pfx}-s0")
    assert r.status_code == 400
    assert _err(r) == "action_request_validation_exception"
    assert "index.number_of_shards is required for split operations" in _reason(r)


# --------------------------------------------------------------------------
# reindex
# --------------------------------------------------------------------------


@pytest.fixture
def rx(client, pfx):
    src = _mk(client, f"{pfx}-src", mappings={"properties": {"n": {"type": "integer"}, "k": {"type": "keyword"}}})
    _docs(client, src, {i: {"n": i, "k": "even" if i % 2 == 0 else "odd", "extra": f"e{i}"} for i in range(6)})
    return src, f"{pfx}-dst"


def _reindex(client, body, **params):
    return client.post("/_reindex", json=body, params={"refresh": "true", **params})


def test_reindex_basic(client, rx):
    src, dst = rx
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst}})
    assert r.status_code == 200
    body = r.json()
    assert body["total"] == 6
    assert body["created"] == 6
    assert body["updated"] == 0
    assert body["deleted"] == 0
    assert body["batches"] == 1
    assert body["version_conflicts"] == 0
    assert body["noops"] == 0
    assert body["failures"] == []
    assert body["timed_out"] is False
    assert body["retries"] == {"bulk": 0, "search": 0}
    assert body["requests_per_second"] == -1.0
    assert _count(client, dst) == 6
    assert client.get(f"/{dst}/_doc/3").json()["_source"] == {"n": 3, "k": "odd", "extra": "e3"}


def test_reindex_dest_uses_dynamic_mapping(client, rx):
    src, dst = rx
    _reindex(client, {"source": {"index": src}, "dest": {"index": dst}})
    props = client.get(f"/{dst}/_mapping").json()[dst]["mappings"]["properties"]
    assert props["n"] == {"type": "long"}
    assert props["k"]["type"] == "text"


def test_reindex_second_run_updates(client, rx):
    src, dst = rx
    _reindex(client, {"source": {"index": src}, "dest": {"index": dst}})
    body = _reindex(client, {"source": {"index": src}, "dest": {"index": dst}}).json()
    assert body["updated"] == 6
    assert body["created"] == 0


def test_reindex_op_type_create_conflicts(client, rx):
    src, dst = rx
    _reindex(client, {"source": {"index": src}, "dest": {"index": dst}})
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst, "op_type": "create"}})
    assert r.status_code == 409
    body = r.json()
    assert body["version_conflicts"] == 6
    assert body["created"] == 0
    f = body["failures"][0]
    assert f["index"] == dst
    assert f["status"] == 409
    assert f["cause"]["type"] == "version_conflict_engine_exception"
    assert "document already exists" in f["cause"]["reason"]


def test_reindex_conflicts_proceed(client, rx):
    src, dst = rx
    _reindex(client, {"source": {"index": src}, "dest": {"index": dst}})
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst, "op_type": "create"}, "conflicts": "proceed"})
    assert r.status_code == 200
    assert r.json()["version_conflicts"] == 6
    assert r.json()["failures"] == []


def test_reindex_version_type_external_conflict(client, rx):
    src, dst = rx
    _reindex(client, {"source": {"index": src}, "dest": {"index": dst}})
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst, "version_type": "external"}})
    assert r.status_code == 409
    assert "is higher or equal to the one provided" in r.json()["failures"][0]["cause"]["reason"]


def test_reindex_query_and_source_filter(client, rx):
    src, dst = rx
    r = _reindex(client, {"source": {"index": src, "query": {"term": {"k": "even"}}, "_source": ["n"]}, "dest": {"index": dst}})
    assert r.json()["total"] == 3
    assert _count(client, dst) == 3
    assert client.get(f"/{dst}/_doc/2").json()["_source"] == {"n": 2}
    assert client.get(f"/{dst}/_doc/1").status_code == 404


def test_reindex_max_docs(client, rx):
    src, dst = rx
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst}, "max_docs": 2})
    assert r.json()["total"] == 2
    assert r.json()["created"] == 2
    assert _count(client, dst) == 2


@pytest.mark.parametrize(
    "max_docs,reason_part",
    [(0, "[max_docs] should be >= [slices]"), (-1, "[max_docs] parameter cannot be negative, found [-1]")],
)
def test_reindex_invalid_max_docs(client, rx, max_docs, reason_part):
    src, dst = rx
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst}, "max_docs": max_docs})
    assert r.status_code == 400
    assert _err(r) == "x_content_parse_exception"
    assert r.json()["error"]["caused_by"]["reason"] == reason_part


def test_reindex_small_batches(client, rx):
    src, dst = rx
    r = _reindex(client, {"source": {"index": src, "size": 2}, "dest": {"index": dst}})
    assert r.json()["batches"] == 3
    assert r.json()["created"] == 6


def test_reindex_script_modify_and_noop(client, rx):
    src, dst = rx
    script = "ctx._source.n = ctx._source.n * 10; if (ctx._source.k == 'odd') { ctx.op = 'noop' }"
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst}, "script": {"source": script}})
    assert r.json()["created"] == 3
    assert r.json()["noops"] == 3
    assert client.get(f"/{dst}/_doc/4").json()["_source"]["n"] == 40
    assert client.get(f"/{dst}/_doc/3").status_code == 404


def test_reindex_script_change_id_and_delete(client, rx):
    src, dst = rx
    script = "if (ctx._source.n == 1) { ctx.op = 'delete' } ctx._id = 'x' + ctx._id"
    r = _reindex(client, {"source": {"index": src}, "dest": {"index": dst}, "script": {"source": script}})
    assert r.json()["created"] == 5
    assert r.json()["deleted"] == 1
    assert client.get(f"/{dst}/_doc/x3").json()["found"] is True
    assert client.get(f"/{dst}/_doc/3").status_code == 404


def test_reindex_remote_not_allowlisted(client, rx):
    src, dst = rx
    r = _reindex(client, {"source": {"remote": {"host": "http://localhost:9200"}, "index": src}, "dest": {"index": dst}})
    assert r.status_code == 400
    assert _err(r) == "illegal_argument_exception"
    assert _reason(r) == "[localhost:9200] not allowlisted in reindex.remote.allowlist"


@pytest.mark.parametrize(
    "body,status,etype,reason_part",
    [
        ({"source": {"index": "SRC"}, "dest": {"index": "SRC"}}, 400, "action_request_validation_exception", "reindex cannot write into an index its reading from"),
        ({"source": {"index": "SRC"}}, 400, "action_request_validation_exception", "index must be specified"),
        ({"dest": {"index": "DST"}}, 400, "action_request_validation_exception", "use _all if you really want to copy from all existing indexes"),
        ({"source": {"index": "SRC"}, "dest": {"index": "DST"}, "bogus": 1}, 400, "x_content_parse_exception", "unknown field [bogus]"),
        ({"source": {"index": "MISSING"}, "dest": {"index": "DST"}}, 404, "index_not_found_exception", "no such index"),
    ],
)
def test_reindex_errors(client, rx, body, status, etype, reason_part):
    src, dst = rx
    raw = json.dumps(body).replace("SRC", src).replace("DST", dst).replace("MISSING", dst + "-missing")
    r = client.post("/_reindex", content=raw.encode(), headers={"Content-Type": "application/json"})
    assert r.status_code == status
    assert _err(r) == etype
    assert reason_part in _reason(r)


def test_reindex_invalid_dest_name_reports_per_doc_failures(client, rx, pfx):
    src, _ = rx
    r = client.post("/_reindex", json={"source": {"index": src}, "dest": {"index": f"{pfx}-UP"}})
    assert r.status_code == 400
    body = r.json()
    assert body["created"] == 0
    assert body["failures"][0]["cause"]["type"] == "invalid_index_name_exception"
    assert body["failures"][0]["status"] == 400


def test_reindex_from_multiple_sources(client, rx, pfx):
    src, dst = rx
    other = _mk(client, f"{pfx}-other")
    _docs(client, other, {"o1": {"n": 100}})
    r = _reindex(client, {"source": {"index": [src, other]}, "dest": {"index": dst}})
    assert r.json()["created"] == 7
    assert _count(client, dst) == 7


# --------------------------------------------------------------------------
# resolve index
# --------------------------------------------------------------------------


@pytest.fixture
def resolvable(client, pfx):
    _mk(client, f"{pfx}-a", aliases={f"{pfx}-al": {}})
    _mk(client, f"{pfx}-h", settings={"index.hidden": True}, aliases={f"{pfx}-hal": {"is_hidden": True}})
    _mk(client, f".{pfx}-dot")
    _mk(client, f"{pfx}-c")
    client.post(f"/{pfx}-c/_close")
    return pfx


def test_resolve_wildcard(client, resolvable):
    p = resolvable
    r = client.get(f"/_resolve/index/{p}-*")
    assert r.status_code == 200
    assert r.json() == {
        "indices": [
            {"name": f"{p}-a", "aliases": [f"{p}-al"], "attributes": ["open"]},
        ],
        "aliases": [{"name": f"{p}-al", "indices": [f"{p}-a"]}],
        "data_streams": [],
    }


def test_resolve_expand_all(client, resolvable):
    p = resolvable
    body = client.get(f"/_resolve/index/{p}-*", params={"expand_wildcards": "all"}).json()
    assert body["indices"] == [
        {"name": f"{p}-a", "aliases": [f"{p}-al"], "attributes": ["open"]},
        {"name": f"{p}-c", "attributes": ["closed"]},
        {"name": f"{p}-h", "aliases": [f"{p}-hal"], "attributes": ["hidden", "open"]},
    ]
    assert body["aliases"] == [
        {"name": f"{p}-al", "indices": [f"{p}-a"]},
        {"name": f"{p}-hal", "indices": [f"{p}-h"]},
    ]


def test_resolve_concrete_names(client, resolvable):
    p = resolvable
    assert client.get(f"/_resolve/index/{p}-a").json() == {
        "indices": [{"name": f"{p}-a", "aliases": [f"{p}-al"], "attributes": ["open"]}],
        "aliases": [],
        "data_streams": [],
    }
    assert client.get(f"/_resolve/index/{p}-al").json() == {
        "indices": [],
        "aliases": [{"name": f"{p}-al", "indices": [f"{p}-a"]}],
        "data_streams": [],
    }


def test_resolve_missing_returns_empty(client, resolvable):
    p = resolvable
    empty = {"indices": [], "aliases": [], "data_streams": []}
    assert client.get(f"/_resolve/index/{p}-nope").json() == empty
    assert client.get(f"/_resolve/index/{p}-zz*").json() == empty


def test_resolve_dot_index(client, resolvable):
    p = resolvable
    assert client.get(f"/_resolve/index/.{p}*").json()["indices"] == [{"name": f".{p}-dot", "attributes": ["open"]}]
    names = [i["name"] for i in client.get(f"/_resolve/index/*{p}*").json()["indices"]]
    assert names == [f".{p}-dot", f"{p}-a"]


def test_resolve_data_stream(client, ds_tpl):
    p = ds_tpl
    client.put(f"/_data_stream/{p}-ds")
    _rollover(client, f"{p}-ds")
    body = client.get(f"/_resolve/index/{p}-ds*").json()
    assert body == {
        "indices": [],
        "aliases": [],
        "data_streams": [{
            "name": f"{p}-ds",
            "backing_indices": [f".ds-{p}-ds-000001", f".ds-{p}-ds-000002"],
            "timestamp_field": "@timestamp",
        }],
    }


# --------------------------------------------------------------------------
# hidden indices / dot indices / _count across indices
# --------------------------------------------------------------------------


@pytest.fixture
def hidden_set(client, pfx):
    _mk(client, f"{pfx}-a")
    _mk(client, f"{pfx}-h", settings={"index.hidden": True}, aliases={f"{pfx}-hal": {"is_hidden": True}})
    _mk(client, f".{pfx}-dot")
    for name in (f"{pfx}-a", f"{pfx}-h", f".{pfx}-dot"):
        _docs(client, name, {1: {"x": 1}})
    return pfx


def test_count_wildcard_excludes_hidden(client, hidden_set):
    p = hidden_set
    r = client.get(f"/{p}-*/_count")
    assert r.json()["count"] == 1
    assert r.json()["_shards"]["total"] == 1
    assert _count(client, f"{p}-*", expand_wildcards="open,hidden") == 2


def test_hidden_index_by_name_and_hidden_alias(client, hidden_set):
    p = hidden_set
    assert _count(client, f"{p}-h") == 1
    assert _count(client, f"{p}-hal") == 1


def test_dot_index_wildcards_count(client, hidden_set):
    p = hidden_set
    assert _count(client, f"*{p}*") == 2
    assert _count(client, f".{p}*") == 1


def test_unhide_index(client, hidden_set):
    p = hidden_set
    _set(client, f"{p}-h", **{"index.hidden": False})
    assert _count(client, f"{p}-*") == 2


def test_hidden_alias_mismatch_rejected(client, hidden_set):
    p = hidden_set
    r = _aliases(client, [{"add": {"index": f"{p}-a", "alias": f"{p}-hal"}}])
    assert r.status_code == 500
    assert _err(r) == "illegal_state_exception"
    assert "alias must have the same is_hidden setting on all indices" in _reason(r)


def test_get_alias_shows_is_hidden(client, hidden_set):
    p = hidden_set
    assert client.get(f"/_alias/{p}-hal").json() == {f"{p}-h": {"aliases": {f"{p}-hal": {"is_hidden": True}}}}


def test_cat_indices_hidden(client, hidden_set):
    p = hidden_set
    r = client.get(f"/_cat/indices/{p}-*", params={"h": "index", "s": "index"})
    assert r.text.split() == [f"{p}-a"]
    r = client.get(f"/_cat/indices/{p}-*", params={"h": "index", "s": "index", "expand_wildcards": "all"})
    assert r.text.split() == [f"{p}-a", f"{p}-h"]


def test_count_across_named_indices(client, pfx):
    _mk(client, f"{pfx}-a")
    _mk(client, f"{pfx}-b")
    _docs(client, f"{pfx}-a", {1: {"k": 1}, 2: {"k": 2}})
    _docs(client, f"{pfx}-b", {1: {"k": 1}})
    r = client.get(f"/{pfx}-a,{pfx}-b/_count")
    assert r.json() == {"count": 3, "_shards": {"total": 2, "successful": 2, "skipped": 0, "failed": 0}}
    r = client.post(f"/{pfx}-a,{pfx}-b/_count", json={"query": {"term": {"k": 1}}})
    assert r.json()["count"] == 2


def test_count_missing_and_ignore_unavailable(client, pfx):
    _mk(client, f"{pfx}-a")
    _docs(client, f"{pfx}-a", {1: {}})
    r = client.get(f"/{pfx}-a,{pfx}-nope/_count")
    assert r.status_code == 404
    assert _err(r) == "index_not_found_exception"
    assert _count(client, f"{pfx}-a,{pfx}-nope", ignore_unavailable="true") == 1


def test_count_query_string_param(client, pfx):
    _mk(client, pfx)
    _docs(client, pfx, {1: {"x": 1}, 2: {"x": 2}})
    assert _count(client, pfx, q="x:1") == 1


def test_count_rejects_size(client, pfx):
    _mk(client, pfx)
    r = client.post(f"/{pfx}/_count", json={"size": 1})
    assert r.status_code == 400
    assert _err(r) == "parsing_exception"
    assert _reason(r) == "request does not support [size]"


def test_count_unknown_query(client, pfx):
    _mk(client, pfx)
    r = client.post(f"/{pfx}/_count", json={"query": {"bogus": {}}})
    assert r.status_code == 400
    assert _err(r) == "parsing_exception"
    assert _reason(r) == "unknown query [bogus]"


def test_count_terminate_after_adds_flag(client, pfx):
    _mk(client, pfx)
    _docs(client, pfx, {1: {}, 2: {}, 3: {}})
    body = client.get(f"/{pfx}/_count", params={"terminate_after": "1"}).json()
    # quirk: match_all count is answered from segment stats, so the full count
    # is reported even though terminated_early is set
    assert body["terminated_early"] is True
    assert body["count"] == 3
    body = client.post(f"/{pfx}/_count", json={"query": {"term": {"x": 1}}}, params={"terminate_after": "1"}).json()
    assert body["terminated_early"] is False
    assert body["count"] == 0
