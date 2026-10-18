import json
import re
import uuid

import pytest

pytestmark = pytest.mark.ingest

UA_CHROME = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_9_2) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/33.0.1750.149 Safari/537.36"
)
UA_IPHONE = (
    "Mozilla/5.0 (iPhone; CPU iPhone OS 12_2 like Mac OS X) AppleWebKit/605.1.15 "
    "(KHTML, like Gecko) Version/12.1 Mobile/15E148 Safari/604.1"
)
TS_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$")


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def _uid(prefix="lt-ingest"):
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


def _simulate(client, processors, docs, verbose=False, **pipeline_extra):
    body = {
        "pipeline": {"processors": processors, **pipeline_extra},
        "docs": [d if "_source" in d else {"_source": d} for d in docs],
    }
    params = {"verbose": "true"} if verbose else {}
    return client.post("/_ingest/pipeline/_simulate", json=body, params=params)


def _run(client, processors, doc):
    """Simulate a single doc; return the per-doc result entry."""
    r = _simulate(client, processors, [doc])
    assert r.status_code == 200, r.text
    return r.json()["docs"][0]


def _src(client, processors, doc):
    res = _run(client, processors, doc)
    assert "doc" in res, res
    return res["doc"]["_source"]


def _doc_error(client, processors, doc):
    res = _run(client, processors, doc)
    assert "error" in res, res
    return res["error"]


def _config_error(client, processors):
    r = _simulate(client, processors, [{"a": 1}])
    assert r.status_code == 400, r.text
    return r.json()["error"]


def _ndjson(lines):
    return ("\n".join(json.dumps(x) for x in lines) + "\n").encode()


def _bulk(client, lines, **params):
    r = client.post(
        "/_bulk",
        content=_ndjson(lines),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true", **params},
    )
    assert r.status_code == 200, r.text
    return r.json()


@pytest.fixture
def pipeline_factory(client):
    created = []

    def _create(processors, name=None, **extra):
        name = name or _uid("lt-pipe")
        r = client.put(f"/_ingest/pipeline/{name}", json={"processors": processors, **extra})
        assert r.status_code == 200, r.text
        created.append(name)
        return name

    _create.created = created
    yield _create
    for name in created:
        client.delete(f"/_ingest/pipeline/{name}")


@pytest.fixture
def search_pipeline_factory(client):
    created = []

    def _create(body, name=None):
        name = name or _uid("lt-spipe")
        r = client.put(f"/_search/pipeline/{name}", json=body)
        assert r.status_code == 200, r.text
        created.append(name)
        return name

    yield _create
    for name in created:
        client.delete(f"/_search/pipeline/{name}")


@pytest.fixture
def extra_indices(client):
    names = []
    yield names
    for name in names:
        client.delete(f"/{name}", params={"ignore_unavailable": "true"})


SEARCH_MAPPING = {
    "properties": {
        "cat": {"type": "keyword"},
        "n": {"type": "integer"},
        "t": {"type": "text"},
        "tags": {"type": "keyword"},
    }
}
SEARCH_DOCS = [
    ("a", "x y"),
    ("a", "y"),
    ("b", "x"),
    ("b", "z"),
    ("c", "x"),
]


@pytest.fixture
def sidx(client, index_factory):
    name = index_factory(mappings=SEARCH_MAPPING)
    lines = []
    for i, (cat, t) in enumerate(SEARCH_DOCS):
        lines.append({"index": {"_index": name, "_id": str(i)}})
        lines.append({"cat": cat, "n": i, "t": t, "tags": "q,w,e"})
    body = _bulk(client, lines)
    assert body["errors"] is False
    return name


def _ids(resp):
    assert resp.status_code == 200, resp.text
    return [h["_id"] for h in resp.json()["hits"]["hits"]]


def _search_with(client, index, pipeline, body=None, **params):
    body = dict(body or {"query": {"match_all": {}}, "sort": ["n"]})
    if pipeline is not None:
        body["search_pipeline"] = pipeline
    return client.post(f"/{index}/_search", json=body, params=params)


# --------------------------------------------------------------------------
# ingest pipeline CRUD
# --------------------------------------------------------------------------


def test_put_get_pipeline_roundtrip(client, pipeline_factory):
    name = pipeline_factory(
        [{"set": {"field": "a", "value": 1}}], description="desc", version=7
    )
    r = client.get(f"/_ingest/pipeline/{name}")
    assert r.status_code == 200
    assert r.json() == {
        name: {
            "description": "desc",
            "version": 7,
            "processors": [{"set": {"field": "a", "value": 1}}],
        }
    }


def test_put_pipeline_meta_not_supported(client):
    name = _uid("lt-bad")
    r = client.put(f"/_ingest/pipeline/{name}", json={"processors": [], "_meta": {"o": "x"}})
    client.delete(f"/_ingest/pipeline/{name}")
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "parse_exception"
    assert r.json()["error"]["reason"] == f"pipeline [{name}] doesn't support one or more provided configuration parameters [_meta]"


def test_put_pipeline_overwrites(client, pipeline_factory):
    name = pipeline_factory([{"set": {"field": "a", "value": 1}}])
    r = client.put(f"/_ingest/pipeline/{name}", json={"processors": [{"drop": {}}]})
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    assert client.get(f"/_ingest/pipeline/{name}").json()[name] == {"processors": [{"drop": {}}]}


def test_get_missing_pipeline_is_404_with_empty_body(client):
    r = client.get(f"/_ingest/pipeline/{_uid('missing')}")
    assert r.status_code == 404
    assert r.json() == {}


def test_delete_missing_pipeline_404(client):
    name = _uid("missing")
    r = client.delete(f"/_ingest/pipeline/{name}")
    assert r.status_code == 404
    err = r.json()["error"]
    assert err["type"] == "resource_not_found_exception"
    assert err["reason"] == f"pipeline [{name}] is missing"


def test_delete_pipeline_then_get_404(client):
    name = _uid("lt-pipe")
    assert client.put(f"/_ingest/pipeline/{name}", json={"processors": []}).status_code == 200
    r = client.delete(f"/_ingest/pipeline/{name}")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    assert client.get(f"/_ingest/pipeline/{name}").status_code == 404


def test_get_multiple_pipelines_skips_missing(client, pipeline_factory):
    a = pipeline_factory([{"set": {"field": "a", "value": 1}}])
    b = pipeline_factory([{"set": {"field": "b", "value": 2}}])
    r = client.get(f"/_ingest/pipeline/{a},{b},{_uid('missing')}")
    assert r.status_code == 200
    assert set(r.json()) == {a, b}


def test_get_pipeline_wildcard_matches_own_prefix(client, pipeline_factory):
    prefix = _uid("lt-wild")
    a = pipeline_factory([], name=f"{prefix}-one")
    b = pipeline_factory([], name=f"{prefix}-two")
    r = client.get(f"/_ingest/pipeline/{prefix}-*")
    assert r.status_code == 200
    assert set(r.json()) == {a, b}


def test_get_all_pipelines_contains_created(client, pipeline_factory):
    name = pipeline_factory([{"lowercase": {"field": "x"}}])
    r = client.get("/_ingest/pipeline")
    assert r.status_code == 200
    assert r.json()[name]["processors"] == [{"lowercase": {"field": "x"}}]


def test_empty_processor_list_is_valid(client, pipeline_factory):
    name = pipeline_factory([])
    assert client.get(f"/_ingest/pipeline/{name}").json()[name] == {"processors": []}


@pytest.mark.parametrize(
    "body,reason,extra",
    [
        ({"processors": [{"nope": {}}]}, "No processor type exists with name [nope]", {"processor_type": "nope"}),
        (
            {"processors": [{"set": {"field": "a"}}]},
            "[value] required property is missing",
            {"processor_type": "set", "property_name": "value"},
        ),
        (
            {"processors": [{"set": {"value": 1}}]},
            "[field] required property is missing",
            {"processor_type": "set", "property_name": "field"},
        ),
        ({}, "[processors] required property is missing", {"property_name": "processors"}),
        (
            {"processors": [{"set": {"field": "a", "value": 1, "bogus": 1}}]},
            "processor [set] doesn't support one or more provided configuration parameters [bogus]",
            {"processor_type": "set"},
        ),
    ],
)
def test_put_invalid_pipeline_errors(client, body, reason, extra):
    name = _uid("lt-bad")
    r = client.put(f"/_ingest/pipeline/{name}", json=body)
    try:
        assert r.status_code == 400
        err = r.json()["error"]
        assert err["type"] == "parse_exception"
        assert err["reason"] == reason
        for k, v in extra.items():
            assert err[k] == v
        assert client.get(f"/_ingest/pipeline/{name}").status_code == 404
    finally:
        client.delete(f"/_ingest/pipeline/{name}")


def test_grok_processor_listing(client):
    r = client.get("/_ingest/processor/grok")
    assert r.status_code == 200
    patterns = r.json()["patterns"]
    assert patterns["WORD"] == "\\b\\w+\\b"
    for key in ("IP", "NUMBER", "COMBINEDAPACHELOG", "TIMESTAMP_ISO8601", "GREEDYDATA"):
        assert key in patterns


def test_nodes_ingest_lists_processors(client):
    r = client.get("/_nodes/ingest")
    assert r.status_code == 200
    node = next(iter(r.json()["nodes"].values()))
    types = {p["type"] for p in node["ingest"]["processors"]}
    for t in ("set", "grok", "dissect", "script", "pipeline", "foreach", "user_agent", "community_id", "copy"):
        assert t in types


# --------------------------------------------------------------------------
# simulate API
# --------------------------------------------------------------------------


def test_simulate_basic_shape(client):
    r = _simulate(client, [{"set": {"field": "a", "value": 1}}], [{"b": "x"}])
    assert r.status_code == 200
    doc = r.json()["docs"][0]["doc"]
    assert doc["_index"] == "_index"
    assert doc["_id"] == "_id"
    assert doc["_source"] == {"a": 1, "b": "x"}
    assert set(doc["_ingest"]) == {"timestamp"}
    assert TS_RE.match(doc["_ingest"]["timestamp"])


def test_simulate_preserves_metadata(client):
    r = _simulate(
        client,
        [{"set": {"field": "x", "value": "{{_routing}}|{{_version}}|{{_id}}|{{_index}}"}}],
        [{"_id": "5", "_routing": "r", "_version": 3, "_version_type": "external", "_index": "ix", "_source": {}}],
    )
    doc = r.json()["docs"][0]["doc"]
    assert doc["_index"] == "ix"
    assert doc["_id"] == "5"
    assert doc["_routing"] == "r"
    assert doc["_version"] == "3"
    assert doc["_version_type"] == "external"
    assert doc["_source"] == {"x": "r|3|5|ix"}


def test_simulate_multiple_docs_independent_errors(client):
    r = _simulate(client, [{"uppercase": {"field": "b"}}], [{"b": "x"}, {"b": 3}])
    docs = r.json()["docs"]
    assert docs[0]["doc"]["_source"] == {"b": "X"}
    assert docs[1]["error"]["type"] == "illegal_argument_exception"
    assert docs[1]["error"]["reason"] == "field [b] of type [java.lang.Integer] cannot be cast to [java.lang.String]"
    assert docs[1]["error"]["root_cause"][0]["type"] == "illegal_argument_exception"


def test_simulate_verbose_shape(client):
    procs = [
        {"set": {"field": "a", "value": 1, "tag": "t1"}},
        {"uppercase": {"field": "b"}},
        {"drop": {"if": "ctx.b == 'Y'"}},
    ]
    r = _simulate(client, procs, [{"b": "x"}, {"b": "y"}, {"b": 3}], verbose=True)
    assert r.status_code == 200
    d0, d1, d2 = [d["processor_results"] for d in r.json()["docs"]]
    assert [p["processor_type"] for p in d0] == ["set", "uppercase", "drop"]
    assert d0[0]["tag"] == "t1"
    assert "tag" not in d0[1]
    assert d0[0]["status"] == "success"
    assert d0[0]["doc"]["_source"] == {"a": 1, "b": "x"}
    assert d0[0]["doc"]["_ingest"]["pipeline"] == "_simulate_pipeline"
    assert d0[1]["doc"]["_source"] == {"a": 1, "b": "X"}
    assert d0[2] == {"processor_type": "drop", "status": "skipped", "if": {"condition": "ctx.b == 'Y'", "result": False}}
    assert d1[2] == {"processor_type": "drop", "status": "dropped", "if": {"condition": "ctx.b == 'Y'", "result": True}}
    assert len(d2) == 2
    assert d2[1]["status"] == "error"
    assert d2[1]["error"]["type"] == "illegal_argument_exception"
    assert "doc" not in d2[1]


def test_simulate_drop_non_verbose_returns_null(client):
    r = _simulate(client, [{"drop": {}}], [{"a": 1}, {"a": 2}])
    assert r.json() == {"docs": [None, None]}


def test_simulate_stored_pipeline_by_id(client, pipeline_factory):
    name = pipeline_factory([{"set": {"field": "p", "value": "{{_ingest.pipeline}}"}}])
    r = client.post(f"/_ingest/pipeline/{name}/_simulate", json={"docs": [{"_source": {}}]})
    assert r.status_code == 200
    assert r.json()["docs"][0]["doc"]["_source"] == {"p": name}


def test_simulate_missing_stored_pipeline(client):
    name = _uid("missing")
    r = client.post(f"/_ingest/pipeline/{name}/_simulate", json={"docs": [{"_source": {}}]})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == f"pipeline [{name}] does not exist"


@pytest.mark.parametrize(
    "body,etype,reason",
    [
        ({"docs": [{"_source": {}}]}, "parse_exception", "[pipeline] required property is missing"),
        ({"pipeline": {"processors": []}}, "parse_exception", "[docs] required property is missing"),
        (
            {"pipeline": {"processors": []}, "docs": []},
            "illegal_argument_exception",
            "must specify at least one document in [docs]",
        ),
        ({"pipeline": {"processors": []}, "docs": [{"a": 1}]}, "parse_exception", "[_source] required property is missing"),
    ],
)
def test_simulate_request_validation(client, body, etype, reason):
    r = client.post("/_ingest/pipeline/_simulate", json=body)
    assert r.status_code == 400
    assert r.json()["error"]["type"] == etype
    assert r.json()["error"]["reason"] == reason


def test_simulate_pipeline_level_on_failure(client):
    r = _simulate(
        client,
        [{"uppercase": {"field": "a", "tag": "up"}}, {"set": {"field": "never", "value": 1}}],
        [{"a": 1}],
        on_failure=[
            {
                "set": {
                    "field": "err",
                    "value": "{{_ingest.on_failure_processor_type}}/{{_ingest.on_failure_processor_tag}}",
                }
            }
        ],
    )
    doc = r.json()["docs"][0]["doc"]
    assert doc["_source"] == {"a": 1, "err": "uppercase/up"}
    assert set(doc["_ingest"]) == {"timestamp"}


def test_ingest_timestamp_template(client):
    src = _src(client, [{"set": {"field": "t", "value": "{{_ingest.timestamp}}"}}], {})
    assert TS_RE.match(src["t"])


# --------------------------------------------------------------------------
# set
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        ({"field": "a", "value": "x"}, {"a": 1}, {"a": "x"}),
        ({"field": "a", "value": "x", "override": False}, {"a": 1}, {"a": 1}),
        ({"field": "a", "value": "x", "override": False}, {"a": None}, {"a": "x"}),
        ({"field": "a", "value": "x", "override": False}, {}, {"a": "x"}),
        ({"field": "b", "value": "{{a}}-{{c.d}}"}, {"a": 1, "c": {"d": "q"}}, {"a": 1, "b": "1-q", "c": {"d": "q"}}),
        ({"field": "b", "value": "{{missing}}"}, {"a": 1}, {"a": 1, "b": ""}),
        ({"field": "b", "value": "", "ignore_empty_value": True}, {"a": 1}, {"a": 1, "b": ""}),
        ({"field": "b", "value": "{{missing}}", "ignore_empty_value": True}, {"a": 1}, {"a": 1}),
        ({"field": "{{a}}", "value": "v"}, {"a": "dyn"}, {"a": "dyn", "dyn": "v"}),
        ({"field": "x.y.z", "value": [1, {"k": 2}]}, {}, {"x": {"y": {"z": [1, {"k": 2}]}}}),
        ({"field": "b", "value": "{{a}}"}, {"a": "<&>"}, {"a": "<&>", "b": "<&>"}),
        ({"field": "b", "value": "{{{a}}}"}, {"a": "<&>"}, {"a": "<&>", "b": "<&>"}),
        ({"field": "b", "value": {"n": True, "m": None}}, {}, {"b": {"n": True, "m": None}}),
        ({"field": "a.c", "value": 2}, {"a": {"b": 1}}, {"a": {"b": 1, "c": 2}}),
    ],
)
def test_set_processor(client, cfg, doc, expected):
    assert _src(client, [{"set": cfg}], doc) == expected


def test_set_copy_from_not_supported(client):
    err = _config_error(client, [{"set": {"field": "b", "value": "x", "copy_from": "a"}}])
    assert err["reason"] == "processor [set] doesn't support one or more provided configuration parameters [copy_from]"


def test_set_through_scalar_parent_fails(client):
    err = _doc_error(client, [{"set": {"field": "a.b", "value": 1}}], {"a": "str"})
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "cannot set [b] with parent object of type [java.lang.String] as part of path [a.b]"


def test_set_index_metadata_visible_to_later_templates(client):
    res = _run(
        client,
        [{"set": {"field": "_index", "value": "other"}}, {"set": {"field": "m", "value": "{{_index}}/{{_id}}"}}],
        {},
    )
    assert res["doc"]["_index"] == "other"
    assert res["doc"]["_source"] == {"m": "other/_id"}


# --------------------------------------------------------------------------
# remove / rename / copy / remove_by_pattern
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        ({"field": "a"}, {"a": 1, "b": 2}, {"b": 2}),
        ({"field": ["a", "b"]}, {"a": 1, "b": 2, "c": 3}, {"c": 3}),
        ({"field": "zz", "ignore_missing": True}, {"a": 1}, {"a": 1}),
        ({"field": "a.b"}, {"a": {"b": 1, "c": 2}}, {"a": {"c": 2}}),
        ({"exclude_field": "a"}, {"a": 1, "b": 2}, {"a": 1}),
        ({"field": "{{f}}"}, {"f": "g", "g": 1}, {"f": "g"}),
    ],
)
def test_remove_processor(client, cfg, doc, expected):
    assert _src(client, [{"remove": cfg}], doc) == expected


@pytest.mark.parametrize("field", ["zz", ["a", "zz"]])
def test_remove_missing_field_errors(client, field):
    err = _doc_error(client, [{"remove": {"field": field}}], {"a": 1})
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "field [zz] doesn't exist"


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        ({"field": "a", "target_field": "b"}, {"a": 1}, {"b": 1}),
        ({"field": "a", "target_field": "b", "override_target": True}, {"a": 1, "b": 2}, {"b": 1}),
        ({"field": "zz", "target_field": "b", "ignore_missing": True}, {"a": 1}, {"a": 1}),
        ({"field": "a.b", "target_field": "c.d"}, {"a": {"b": 1}}, {"a": {}, "c": {"d": 1}}),
    ],
)
def test_rename_processor(client, cfg, doc, expected):
    assert _src(client, [{"rename": cfg}], doc) == expected


@pytest.mark.parametrize(
    "cfg,doc,reason",
    [
        ({"field": "a", "target_field": "b"}, {"a": 1, "b": 2}, "field [b] already exists"),
        ({"field": "zz", "target_field": "b"}, {"a": 1}, "field [zz] doesn't exist"),
    ],
)
def test_rename_errors(client, cfg, doc, reason):
    err = _doc_error(client, [{"rename": cfg}], doc)
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == reason


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        ({"source_field": "a", "target_field": "b"}, {"a": {"x": 1}}, {"a": {"x": 1}, "b": {"x": 1}}),
        ({"source_field": "a", "target_field": "b", "override_target": True}, {"a": 1, "b": 2}, {"a": 1, "b": 1}),
        ({"source_field": "a", "target_field": "b", "remove_source": True}, {"a": 1}, {"b": 1}),
        ({"source_field": "zz", "target_field": "b", "ignore_missing": True}, {"a": 1}, {"a": 1}),
        ({"source_field": "a", "target_field": "b"}, {"a": None}, {"a": None, "b": None}),
    ],
)
def test_copy_processor(client, cfg, doc, expected):
    assert _src(client, [{"copy": cfg}], doc) == expected


@pytest.mark.parametrize(
    "cfg,doc,reason",
    [
        ({"source_field": "a", "target_field": "b"}, {"a": 1, "b": 2}, "target field [b] already exists"),
        ({"source_field": "zz", "target_field": "b"}, {"a": 1}, "source field [zz] doesn't exist"),
        ({"source_field": "a", "target_field": "a"}, {"a": 1}, "source field path and target field path cannot be same"),
    ],
)
def test_copy_errors(client, cfg, doc, reason):
    err = _doc_error(client, [{"copy": cfg}], doc)
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == reason


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        (
            {"field_pattern": "tmp_*"},
            {"tmp_a": 1, "tmp_b": 2, "keep": 3, "x": {"tmp_c": 4}},
            {"keep": 3, "x": {"tmp_c": 4}},
        ),
        ({"field_pattern": ["a*", "*z"]}, {"ab": 1, "bz": 2, "c": 3}, {"c": 3}),
        ({"exclude_field_pattern": "k*"}, {"ka": 1, "b": 2}, {"ka": 1}),
        ({"field_pattern": "nomatch*"}, {"b": 2}, {"b": 2}),
    ],
)
def test_remove_by_pattern(client, cfg, doc, expected):
    assert _src(client, [{"remove_by_pattern": cfg}], doc) == expected


@pytest.mark.parametrize(
    "cfg,reason",
    [
        ({"field_pattern": "_*"}, "[field_pattern] Validation Failed: 1: field_pattern [_*] must not start with '_';"),
        ({}, "[field_pattern] either field_pattern or exclude_field_pattern must be set"),
        ({"field_pattern": "a", "exclude_field_pattern": "b"}, "[field_pattern] either field_pattern or exclude_field_pattern must be set"),
    ],
)
def test_remove_by_pattern_config_errors(client, cfg, reason):
    err = _config_error(client, [{"remove_by_pattern": cfg}])
    assert err["type"] == "parse_exception"
    assert err["reason"] == reason


# --------------------------------------------------------------------------
# append
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        ({"field": "a", "value": ["b", "c"]}, {"a": "a"}, {"a": ["a", "b", "c"]}),
        ({"field": "a", "value": "b"}, {}, {"a": ["b"]}),
        ({"field": "a", "value": ["a", "b"], "allow_duplicates": False}, {"a": ["a"]}, {"a": ["a", "b"]}),
        ({"field": "a", "value": "a", "allow_duplicates": False}, {"a": "a"}, {"a": "a"}),
        ({"field": "a", "value": "a"}, {"a": "a"}, {"a": ["a", "a"]}),
        ({"field": "a", "value": "{{b}}"}, {"a": [1], "b": "q"}, {"a": [1, "q"], "b": "q"}),
        ({"field": "a", "value": [1]}, {"a": None}, {"a": [None, 1]}),
        ({"field": "a", "value": []}, {"a": 1}, {"a": [1]}),
    ],
)
def test_append_processor(client, cfg, doc, expected):
    assert _src(client, [{"append": cfg}], doc) == expected


# --------------------------------------------------------------------------
# convert
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "typ,value,expected",
    [
        ("integer", "42", 42),
        ("integer", "0x1F", 31),
        ("integer", "-0", 0),
        ("integer", ["1", "2"], [1, 2]),
        ("long", "9999999999", 9999999999),
        ("float", "1.5", 1.5),
        ("float", "1e3", 1000.0),
        ("double", "2.25", 2.25),
        ("boolean", "true", True),
        ("boolean", "TRUE", True),
        ("boolean", "False", False),
        ("string", 12, "12"),
        ("string", True, "true"),
        ("string", 1.5, "1.5"),
        ("auto", "12", 12),
        ("auto", "1.5", 1.5),
        ("auto", "true", True),
        ("auto", "abc", "abc"),
        ("auto", "9999999999", 9999999999),
        ("auto", "1e3", 1000.0),
        ("ip", "1.2.3.4", "1.2.3.4"),
        ("double", "NaN", "NaN"),
    ],
)
def test_convert_success(client, typ, value, expected):
    assert _src(client, [{"convert": {"field": "a", "type": typ}}], {"a": value}) == {"a": expected}


@pytest.mark.parametrize(
    "typ,value,reason",
    [
        ("integer", " 42 ", "unable to convert [ 42 ] to integer"),
        ("integer", "4.5", "unable to convert [4.5] to integer"),
        ("integer", "abc", "unable to convert [abc] to integer"),
        ("integer", 3.9, "unable to convert [3.9] to integer"),
        ("integer", "9999999999", "unable to convert [9999999999] to integer"),
        ("boolean", "yes", "[yes] is not a boolean value, cannot convert to boolean"),
        ("boolean", "0", "[0] is not a boolean value, cannot convert to boolean"),
        ("boolean", 1, "[1] is not a boolean value, cannot convert to boolean"),
        ("ip", "999.1.1.1", "[999.1.1.1] is not a valid ipv4/ipv6 address"),
        ("integer", None, "Field [a] is null, cannot be converted to type [integer]"),
    ],
)
def test_convert_failures(client, typ, value, reason):
    err = _doc_error(client, [{"convert": {"field": "a", "type": typ}}], {"a": value})
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == reason


def test_convert_number_format_cause(client):
    err = _doc_error(client, [{"convert": {"field": "a", "type": "integer"}}], {"a": "abc"})
    assert err["caused_by"] == {"type": "number_format_exception", "reason": 'For input string: "abc"'}


def test_convert_target_field_and_missing(client):
    assert _src(client, [{"convert": {"field": "a", "type": "integer", "target_field": "b"}}], {"a": "5"}) == {"a": "5", "b": 5}
    assert _src(client, [{"convert": {"field": "zz", "type": "integer", "ignore_missing": True}}], {"a": 1}) == {"a": 1}
    err = _doc_error(client, [{"convert": {"field": "zz", "type": "integer"}}], {"a": 1})
    assert err["reason"] == "field [zz] not present as part of path [zz]"


def test_convert_bad_type_config(client):
    err = _config_error(client, [{"convert": {"field": "a", "type": "bogus"}}])
    assert err["reason"] == "[type] type [bogus] not supported, cannot convert field."
    assert err["property_name"] == "type"


# --------------------------------------------------------------------------
# lowercase / uppercase / trim
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "proc,cfg,doc,expected",
    [
        ("lowercase", {"field": "a"}, {"a": "HeLLo ÄÖ"}, {"a": "hello äö"}),
        ("uppercase", {"field": "a"}, {"a": ["ab", "straße"]}, {"a": ["AB", "STRASSE"]}),
        ("trim", {"field": "a"}, {"a": "  x y \t\n"}, {"a": "x y"}),
        ("trim", {"field": "a", "target_field": "b"}, {"a": " x "}, {"a": " x ", "b": "x"}),
        ("lowercase", {"field": "zz", "ignore_missing": True}, {"a": 1}, {"a": 1}),
        ("lowercase", {"field": "a", "ignore_missing": True}, {"a": None}, {"a": None}),
        ("uppercase", {"field": "a.b"}, {"a": {"b": "q"}}, {"a": {"b": "Q"}}),
    ],
)
def test_case_and_trim(client, proc, cfg, doc, expected):
    assert _src(client, [{proc: cfg}], doc) == expected


@pytest.mark.parametrize(
    "doc,reason",
    [
        ({"a": 1}, "field [a] of type [java.lang.Integer] cannot be cast to [java.lang.String]"),
        ({"a": None}, "field [a] is null, cannot process it."),
        ({}, "field [a] not present as part of path [a]"),
        ({"a": ["x", 1]}, "value [1] of type [java.lang.Integer] in list field [a] cannot be cast to [java.lang.String]"),
    ],
)
def test_uppercase_errors(client, doc, reason):
    err = _doc_error(client, [{"uppercase": {"field": "a"}}], doc)
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == reason


# --------------------------------------------------------------------------
# split / join / gsub / sort
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,value,expected",
    [
        ({"separator": ","}, "a,b,,c,,", ["a", "b", "", "c"]),
        ({"separator": ",", "preserve_trailing": True}, "a,b,,c,,", ["a", "b", "", "c", "", ""]),
        ({"separator": "\\s+"}, "a  b\tc", ["a", "b", "c"]),
        ({"separator": ","}, "abc", ["abc"]),
        ({"separator": ","}, "", [""]),
        ({"separator": "|"}, "a|b", ["a", "|", "b"]),
    ],
)
def test_split_processor(client, cfg, value, expected):
    assert _src(client, [{"split": {"field": "a", **cfg}}], {"a": value}) == {"a": expected}


def test_split_on_list_fails(client):
    err = _doc_error(client, [{"split": {"field": "a", "separator": ","}}], {"a": ["x,y"]})
    assert err["reason"] == "field [a] of type [java.util.ArrayList] cannot be cast to [java.lang.String]"


@pytest.mark.parametrize(
    "value,expected",
    [(["x", 1, True], "x-1-true"), ([], ""), (["solo"], "solo")],
)
def test_join_processor(client, value, expected):
    assert _src(client, [{"join": {"field": "a", "separator": "-"}}], {"a": value}) == {"a": expected}


def test_join_errors(client):
    err = _doc_error(client, [{"join": {"field": "a", "separator": "-"}}], {"a": "str"})
    assert err["reason"] == "field [a] of type [java.lang.String] cannot be cast to [java.util.List]"
    err = _doc_error(client, [{"join": {"field": "a", "separator": "-"}}], {"a": ["x", None]})
    assert err["type"] == "null_pointer_exception"


@pytest.mark.parametrize(
    "cfg,value,expected",
    [
        ({"pattern": "\\d", "replacement": "#"}, "a1b22", "a#b##"),
        ({"pattern": "(\\w+)@(\\w+)", "replacement": "$2 at $1"}, "joe@ex", "ex at joe"),
        ({"pattern": "x", "replacement": "y"}, ["x1", "2x"], ["y1", "2y"]),
        ({"pattern": "^", "replacement": ">"}, "ab", ">ab"),
        ({"pattern": "z", "replacement": ""}, "zaz", "a"),
    ],
)
def test_gsub_processor(client, cfg, value, expected):
    assert _src(client, [{"gsub": {"field": "a", **cfg}}], {"a": value}) == {"a": expected}


def test_gsub_invalid_regex(client):
    err = _config_error(client, [{"gsub": {"field": "a", "pattern": "[", "replacement": "y"}}])
    assert err["type"] == "parse_exception"
    assert err["reason"].startswith("[pattern] Invalid regex pattern. Unclosed character class")
    assert err["processor_type"] == "gsub"


def test_gsub_non_string_fails(client):
    err = _doc_error(client, [{"gsub": {"field": "a", "pattern": "x", "replacement": "y"}}], {"a": 5})
    assert err["reason"] == "field [a] of type [java.lang.Integer] cannot be cast to [java.lang.String]"


@pytest.mark.parametrize(
    "cfg,value,expected",
    [
        ({}, [3, 1, 2], {"a": [1, 2, 3]}),
        ({}, [10, 9, 100], {"a": [9, 10, 100]}),
        ({"order": "desc"}, ["b", "C", "a"], {"a": ["b", "a", "C"]}),
        ({}, ["b", "C", "a"], {"a": ["C", "a", "b"]}),
        ({"target_field": "b"}, ["b", "a"], {"a": ["b", "a"], "b": ["a", "b"]}),
    ],
)
def test_sort_processor(client, cfg, value, expected):
    assert _src(client, [{"sort": {"field": "a", **cfg}}], {"a": value}) == expected


def test_sort_processor_errors(client):
    err = _doc_error(client, [{"sort": {"field": "a"}}], {"a": [1, "a"]})
    assert err["type"] == "class_cast_exception"
    err = _doc_error(client, [{"sort": {"field": "a"}}], {"a": "str"})
    assert err["reason"] == "field [a] of type [java.lang.String] cannot be cast to [java.util.List]"
    err = _config_error(client, [{"sort": {"field": "a", "order": "up"}}])
    assert err["reason"] == "[order] Sort direction [up] not recognized. Valid values are: [asc, desc]"


# --------------------------------------------------------------------------
# grok
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,msg,expected_extra",
    [
        (
            {"patterns": ["%{IP:ip} %{WORD:verb} %{NUMBER:n:int} %{NUMBER:f:float}"]},
            "1.2.3.4 GET 42 1.5",
            {"ip": "1.2.3.4", "verb": "GET", "n": 42, "f": 1.5},
        ),
        ({"patterns": ["%{NUMBER:n:long}"]}, "x 7 y", {"n": 7}),
        ({"patterns": ["%{NUMBER:n}"]}, "x 7 y", {"n": "7"}),
        ({"patterns": ["%{FOO:x}"], "pattern_definitions": {"FOO": "[a-c]+"}}, "zzabcz", {"x": "abc"}),
        ({"patterns": ["%{INT:i}$", "%{WORD:w}"]}, "hello", {"w": "hello"}),
        ({"patterns": ["%{WORD:a.b} %{WORD:a.c}"]}, "x y", {"a": {"b": "x", "c": "y"}}),
        ({"patterns": ["%{WORD:w} %{WORD:w}"]}, "x y", {"w": "x"}),
        ({"patterns": ["(?<named>\\d+)"]}, "ab12", {"named": "12"}),
        ({"patterns": ["%{NUMBER:n:boolean}"]}, "1", {"n": False}),
    ],
)
def test_grok_extracts(client, cfg, msg, expected_extra):
    assert _src(client, [{"grok": {"field": "m", **cfg}}], {"m": msg}) == {"m": msg, **expected_extra}


def test_grok_combined_apache_log(client):
    line = '127.0.0.1 - frank [10/Oct/2000:13:55:36 -0700] "GET /a.gif HTTP/1.0" 200 2326 "http://ex.com/" "Mozilla/4.08"'
    src = _src(client, [{"grok": {"field": "m", "patterns": ["%{COMBINEDAPACHELOG}"]}}], {"m": line})
    assert src["clientip"] == "127.0.0.1"
    assert src["auth"] == "frank"
    assert src["ident"] == "-"
    assert src["verb"] == "GET"
    assert src["request"] == "/a.gif"
    assert src["httpversion"] == "1.0"
    assert src["response"] == "200"
    assert src["bytes"] == "2326"
    assert src["referrer"] == '"http://ex.com/"'
    assert src["agent"] == '"Mozilla/4.08"'
    assert src["timestamp"] == "10/Oct/2000:13:55:36 -0700"


def test_grok_trace_match(client):
    procs = [{"grok": {"field": "m", "patterns": ["%{INT:i}$", "%{WORD:w}"], "trace_match": True}}]
    r = _simulate(client, procs, [{"m": "hello"}, {"m": "5"}])
    d0, d1 = [d["doc"] for d in r.json()["docs"]]
    assert d0["_ingest"]["_grok_match_index"] == "1"
    assert d1["_ingest"]["_grok_match_index"] == "0"
    assert d1["_source"] == {"m": "5", "i": "5"}
    assert "_grok_match_index" not in d0["_source"]


def test_grok_no_match(client):
    err = _doc_error(client, [{"grok": {"field": "m", "patterns": ["^%{NUMBER:n}$"]}}], {"m": "x 7 y"})
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "Provided Grok expressions do not match field value: [x 7 y]"


def test_grok_missing_and_non_string(client):
    err = _doc_error(client, [{"grok": {"field": "zz", "patterns": ["%{INT:i}"]}}], {"m": "5"})
    assert err["reason"] == "field [zz] not present as part of path [zz]"
    assert _src(client, [{"grok": {"field": "zz", "patterns": ["%{INT:i}"], "ignore_missing": True}}], {"m": "5"}) == {"m": "5"}
    err = _doc_error(client, [{"grok": {"field": "m", "patterns": ["%{WORD:w}"]}}], {"m": 5})
    assert err["reason"] == "field [m] of type [java.lang.Integer] cannot be cast to [java.lang.String]"


@pytest.mark.parametrize(
    "patterns,reason",
    [
        (
            ["%{UNKNOWNPAT:x}"],
            "[patterns] Invalid regex pattern found in: [%{UNKNOWNPAT:x}]. Unable to find pattern [UNKNOWNPAT] in Grok's pattern dictionary",
        ),
        ([], "[patterns] List of patterns must not be empty"),
    ],
)
def test_grok_config_errors(client, patterns, reason):
    err = _config_error(client, [{"grok": {"field": "m", "patterns": patterns}}])
    assert err["type"] == "parse_exception"
    assert err["reason"] == reason
    assert err["processor_type"] == "grok"


# --------------------------------------------------------------------------
# dissect
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,msg,expected_extra",
    [
        ({"pattern": "%{a} %{b} %{c}"}, "x y z w", {"a": "x", "b": "y", "c": "z w"}),
        ({"pattern": "%{+a} %{+a} %{b}"}, "x y z", {"a": "xy", "b": "z"}),
        ({"pattern": "%{+a} %{+a} %{b}", "append_separator": "-"}, "x y z", {"a": "x-y", "b": "z"}),
        ({"pattern": "%{+a/2} %{+a/1} %{b}"}, "x y z", {"a": "yx", "b": "z"}),
        ({"pattern": "%{a->} %{b}"}, "x     y", {"a": "x", "b": "y"}),
        ({"pattern": "%{} %{b}"}, "x y", {"b": "y"}),
        ({"pattern": "%{?skip} %{b}"}, "x y", {"b": "y"}),
        ({"pattern": "%{*k} %{&k}"}, "key val", {"key": "val"}),
        ({"pattern": "[%{ts}] %{lvl}: %{msg}"}, "[2020-01-01] INFO: hello world", {"ts": "2020-01-01", "lvl": "INFO", "msg": "hello world"}),
        ({"pattern": "%{a.b} %{c}"}, "x y", {"a": {"b": "x"}, "c": "y"}),
    ],
)
def test_dissect(client, cfg, msg, expected_extra):
    assert _src(client, [{"dissect": {"field": "m", **cfg}}], {"m": msg}) == {"m": msg, **expected_extra}


@pytest.mark.parametrize(
    "pattern,msg",
    [("%{a} - %{b}", "x y"), ("%{a}", "")],
)
def test_dissect_no_match(client, pattern, msg):
    err = _doc_error(client, [{"dissect": {"field": "m", "pattern": pattern}}], {"m": msg})
    assert err["type"] == "find_match"
    assert err["reason"] == f"Unable to find match for dissect pattern: {pattern} against source: {msg}"


@pytest.mark.parametrize(
    "pattern,reason_tail",
    [
        ("no keys", "Unable to find any keys or delimiters."),
        ("%{?k} %{&k}", "Found invalid key/reference associations: 'k' Please ensure each '*<key>' is matched with a matching '&<key>"),
    ],
)
def test_dissect_bad_pattern_is_500(client, pattern, reason_tail):
    r = _simulate(client, [{"dissect": {"field": "m", "pattern": pattern}}], [{"m": "x"}])
    assert r.status_code == 500
    err = r.json()["error"]
    assert err["type"] == "exception"
    assert err["caused_by"]["type"] == "pattern_parse"
    assert err["caused_by"]["reason"] == f"Unable to parse pattern: {pattern} Reason: {reason_tail}"


# --------------------------------------------------------------------------
# date / date_index_name
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "formats,value,extra,expected",
    [
        (["ISO8601"], "2020-01-02T03:04:05Z", {}, "2020-01-02T03:04:05.000Z"),
        (["ISO8601"], "2020-01-02T03:04:05.123+02:00", {}, "2020-01-02T01:04:05.123Z"),
        (["ISO8601"], "2020-01-02", {}, "2020-01-02T00:00:00.000Z"),
        (["UNIX"], "1600000000", {}, "2020-09-13T12:26:40.000Z"),
        (["UNIX"], "1600000000.5", {}, "2020-09-13T12:26:40.500Z"),
        (["UNIX"], 1600000000, {}, "2020-09-13T12:26:40.000Z"),
        (["UNIX_MS"], "1600000000123", {}, "2020-09-13T12:26:40.123Z"),
        (["UNIX_MS"], 1600000000123, {}, "2020-09-13T12:26:40.123Z"),
        (["TAI64N"], "4000000050d506482dbdf024", {}, "2012-12-22T01:00:46.767Z"),
        (["dd/MM/yyyy"], "05/06/2021", {}, "2021-06-05T00:00:00.000Z"),
        (["dd/MM/yyyy"], "05/06/2021", {"timezone": "Europe/Kyiv"}, "2021-06-05T00:00:00.000+03:00"),
        (["dd/MM/yyyy HH:mm"], "05/06/2021 10:00", {"timezone": "+05:00"}, "2021-06-05T10:00:00.000+05:00"),
        (["yyyy-MM-dd", "dd/MM/yyyy"], "05/06/2021", {}, "2021-06-05T00:00:00.000Z"),
        (["ISO8601"], "2020-01-02T03:04:05Z", {"output_format": "yyyy/MM/dd"}, "2020/01/02"),
        (["dd MMM yyyy"], "05 juin 2021", {"locale": "fr"}, "2021-06-05T00:00:00.000Z"),
        (["dd MMM yyyy"], "05 Jun 2021", {}, "2021-06-05T00:00:00.000Z"),
        (["ISO8601"], "2020-01-02T03:04:05", {"timezone": "America/New_York"}, "2020-01-02T03:04:05.000-05:00"),
        (["strict_date_optional_time"], "2020-01-02T03:04:05Z", {}, "2020-01-02T03:04:05.000Z"),
    ],
)
def test_date_processor(client, formats, value, extra, expected):
    src = _src(client, [{"date": {"field": "d", "formats": formats, **extra}}], {"d": value})
    assert src == {"d": value, "@timestamp": expected}


def test_date_target_field(client):
    src = _src(client, [{"date": {"field": "d", "formats": ["ISO8601"], "target_field": "ts"}}], {"d": "2020-01-02T03:04:05Z"})
    assert src == {"d": "2020-01-02T03:04:05Z", "ts": "2020-01-02T03:04:05.000Z"}


def test_date_without_year_uses_current_year(client):
    src = _src(client, [{"date": {"field": "d", "formats": ["MM/dd"]}}], {"d": "06/05"})
    assert re.match(r"^\d{4}-06-05T00:00:00\.000Z$", src["@timestamp"])


def test_date_parse_failure(client):
    err = _doc_error(client, [{"date": {"field": "d", "formats": ["yyyy-MM-dd"]}}], {"d": "nope"})
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "unable to parse date [nope]"
    assert err["caused_by"]["reason"] == "failed to parse date field [nope] with format [yyyy-MM-dd]"
    assert err["caused_by"]["caused_by"]["type"] == "date_time_parse_exception"


def test_date_unknown_timezone(client):
    err = _doc_error(client, [{"date": {"field": "d", "formats": ["ISO8601"], "timezone": "Mars/Base"}}], {"d": "2020-01-01"})
    assert err["caused_by"] == {"type": "zone_rules_exception", "reason": "Unknown time-zone ID: Mars/Base"}


def test_date_missing_formats_and_field(client):
    err = _config_error(client, [{"date": {"field": "d"}}])
    assert err["reason"] == "[formats] required property is missing"
    err = _doc_error(client, [{"date": {"field": "d", "formats": ["ISO8601"]}}], {})
    assert err["reason"] == "field [d] not present as part of path [d]"


def _index_after(client, procs, doc):
    r = _simulate(client, procs, [{"_index": "orig", "_source": doc}])
    res = r.json()["docs"][0]
    assert "doc" in res, res
    return res["doc"]["_index"]


@pytest.mark.parametrize(
    "cfg,value,expected",
    [
        ({"index_name_prefix": "logs-", "date_rounding": "M"}, "2020-03-15T10:00:00.000Z", "<logs-{2020-03-15||/M{yyyy-MM-dd|UTC}}>"),
        ({"date_rounding": "d"}, "2020-03-15T10:00:00.000Z", "<{2020-03-15||/d{yyyy-MM-dd|UTC}}>"),
        (
            {"index_name_prefix": "l-", "date_rounding": "y", "index_name_format": "yyyy"},
            "2020-03-15T10:00:00.000Z",
            "<l-{2020||/y{yyyy|UTC}}>",
        ),
        ({"index_name_prefix": "l-", "date_rounding": "w"}, "2020-03-15T10:00:00.000Z", "<l-{2020-03-15||/w{yyyy-MM-dd|UTC}}>"),
        (
            {"index_name_prefix": "l-", "date_rounding": "h", "date_formats": ["dd/MM/yyyy HH:mm"]},
            "15/03/2020 10:30",
            "<l-{2020-03-15||/h{yyyy-MM-dd|UTC}}>",
        ),
        (
            {"index_name_prefix": "l-", "date_rounding": "d", "timezone": "+05:00"},
            "2020-03-15T22:00:00.000Z",
            "<l-{2020-03-15||/d{yyyy-MM-dd|+05:00}}>",
        ),
        (
            {"index_name_prefix": "l-", "date_rounding": "d", "date_formats": ["UNIX"]},
            "1600000000",
            "<l-{2020-09-13||/d{yyyy-MM-dd|UTC}}>",
        ),
    ],
)
def test_date_index_name(client, cfg, value, expected):
    assert _index_after(client, [{"date_index_name": {"field": "d", **cfg}}], {"d": value}) == expected


def test_date_index_name_default_format_requires_millis(client):
    err = _doc_error(client, [{"date_index_name": {"field": "d", "date_rounding": "d"}}], {"d": "2020-03-15T10:00:00Z"})
    assert err["reason"] == "unable to parse date [2020-03-15T10:00:00Z]"
    assert err["caused_by"]["reason"] == "failed to parse date field [2020-03-15T10:00:00Z] with format [yyyy-MM-dd'T'HH:mm:ss.SSSXX]"


def test_date_index_name_requires_rounding(client):
    err = _config_error(client, [{"date_index_name": {"field": "d", "index_name_prefix": "l-"}}])
    assert err["reason"] == "[date_rounding] required property is missing"
    assert err["processor_type"] == "date_index_name"


# --------------------------------------------------------------------------
# json / kv / csv
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,value,expected",
    [
        ({}, '{"a":1,"b":[1,2],"c":{"d":null}}', {"j": {"a": 1, "b": [1, 2], "c": {"d": None}}}),
        ({"target_field": "t"}, '[1,"x"]', {"j": '[1,"x"]', "t": [1, "x"]}),
        ({}, '"str"', {"j": "str"}),
        ({}, "42", {"j": 42}),
        ({}, "true", {"j": True}),
        ({}, '{"a":1} trailing', {"j": {"a": 1}}),
        ({"add_to_root": True}, '{"a":1,"x":2}', {"j": '{"a":1,"x":2}', "a": 1, "x": 2}),
    ],
)
def test_json_processor(client, cfg, value, expected):
    doc = {"j": value}
    if cfg.get("add_to_root"):
        doc["x"] = 1
    assert _src(client, [{"json": {"field": "j", **cfg}}], doc) == expected


def test_json_errors(client):
    err = _doc_error(client, [{"json": {"field": "j", "add_to_root": True}}], {"j": "[1]"})
    assert err["reason"] == "cannot add non-map fields to root of document"
    err = _doc_error(client, [{"json": {"field": "j"}}], {"j": "{bad"})
    assert err["type"] == "illegal_argument_exception"
    assert err["caused_by"]["type"] == "json_parse_exception"
    err = _config_error(client, [{"json": {"field": "j", "add_to_root": True, "target_field": "t"}}])
    assert err["reason"] == "[target_field] Cannot set a target field while also setting `add_to_root` to true"


@pytest.mark.parametrize(
    "cfg,msg,expected_extra",
    [
        ({"field_split": " ", "value_split": "="}, "a=1 b=2 a=3", {"a": ["1", "3"], "b": "2"}),
        ({"field_split": "&", "value_split": "=", "target_field": "q", "include_keys": ["a"]}, "a=1&b=2", {"q": {"a": "1"}}),
        ({"field_split": "&", "value_split": "=", "exclude_keys": ["a"]}, "a=1&b=2", {"b": "2"}),
        ({"field_split": "zz", "value_split": "="}, "a=1", {"a": "1"}),
        ({"field_split": "[,;]", "value_split": "[:=]"}, "a:1;b=2,c:3", {"a": "1", "b": "2", "c": "3"}),
        ({"field_split": " ", "value_split": "=", "prefix": "p_"}, "a=1 b=2", {"p_a": "1", "p_b": "2"}),
        ({"field_split": " ", "value_split": "=", "trim_key": "<", "trim_value": ">"}, "<k=v> <j=w>", {"k": "v", "j": "w"}),
        ({"field_split": " ", "value_split": "=", "strip_brackets": True}, "k=(v) j=\"w\" l=<x>", {"k": "v", "j": "w", "l": "x"}),
    ],
)
def test_kv_processor(client, cfg, msg, expected_extra):
    assert _src(client, [{"kv": {"field": "m", **cfg}}], {"m": msg}) == {"m": msg, **expected_extra}


def test_kv_key_collides_with_source_field(client):
    src = _src(client, [{"kv": {"field": "m", "field_split": " ", "value_split": "="}}], {"m": "m=y"})
    assert src == {"m": ["m=y", "y"]}


@pytest.mark.parametrize(
    "msg,reason",
    [
        ("a=1 b=2 c", "field [m] does not contain value_split [=]"),
        ("", "field [m] does not contain value_split [=]"),
        ("=2", "path cannot be null nor empty"),
    ],
)
def test_kv_errors(client, msg, reason):
    err = _doc_error(client, [{"kv": {"field": "m", "field_split": " ", "value_split": "="}}], {"m": msg})
    assert err["reason"] == reason


@pytest.mark.parametrize(
    "cfg,msg,expected_extra",
    [
        ({"target_fields": ["a", "b", "c"]}, '1,"x,y",3', {"a": "1", "b": "x,y", "c": "3"}),
        ({"target_fields": ["a", "b"]}, "1,2,3", {"a": "1", "b": "2"}),
        ({"target_fields": ["a", "b", "c"]}, "1,2", {"a": "1", "b": "2"}),
        ({"target_fields": ["a", "b"], "separator": ";", "quote": "'"}, "'p;q';r", {"a": "p;q", "b": "r"}),
        ({"target_fields": ["a", "b"], "trim": True}, " 1 , 2 ", {"a": "1", "b": "2 "}),
        ({"target_fields": ["a", "b"]}, " 1 , 2 ", {"a": " 1 ", "b": " 2 "}),
        ({"target_fields": ["a", "b", "c"], "empty_value": "E"}, "1,,3", {"a": "1", "b": "E", "c": "3"}),
        ({"target_fields": ["a", "b", "c"]}, "1,,3", {"a": "1", "c": "3"}),
        ({"target_fields": ["a", "b"]}, '"a""b",c', {"a": 'a"b', "b": "c"}),
    ],
)
def test_csv_processor(client, cfg, msg, expected_extra):
    assert _src(client, [{"csv": {"field": "m", **cfg}}], {"m": msg}) == {"m": msg, **expected_extra}


def test_csv_errors(client):
    err = _doc_error(client, [{"csv": {"field": "m", "target_fields": ["a", "b"]}}], {"m": '"unterminated,2'})
    assert err["reason"] == "Unmatched quote"
    err = _config_error(client, [{"csv": {"field": "m", "target_fields": ["a"], "separator": ";;"}}])
    assert err["reason"] == "[separator] separator has to be single character like , or ;"


def test_split_to_fields_processor(client):
    src = _src(client, [{"split_to_fields": {"field": "m", "separator": ",", "target_fields": ["a", "b"]}}], {"m": "x,y,z"})
    assert src == {"m": "x,y,z", "a": "x", "b": "y"}
    src = _src(client, [{"split_to_fields": {"field": "m", "separator": "\\s+", "target_fields": ["a", "b", "c"]}}], {"m": "x  y"})
    assert src == {"m": "x  y", "a": "x", "b": "y"}


# --------------------------------------------------------------------------
# urldecode / html_strip / bytes
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "value,expected",
    [("a%20b%2Bc+d%C3%A9", "a b+c dé"), (["%41", "%42"], ["A", "B"]), ("plain", "plain")],
)
def test_urldecode(client, value, expected):
    assert _src(client, [{"urldecode": {"field": "u"}}], {"u": value}) == {"u": expected}


def test_urldecode_invalid_escape(client):
    err = _doc_error(client, [{"urldecode": {"field": "u"}}], {"u": "%zz"})
    assert err["reason"].startswith("URLDecoder: Illegal hex characters in escape (%) pattern")


@pytest.mark.parametrize(
    "value,expected",
    [
        ("<p>Hi <b>there</b></p>&amp; &lt;x&gt;<script>alert(1)</script>", "\nHi there\n& <x>\n"),
        ("<br>a<br/>b", "\na\nb"),
        ("no tags", "no tags"),
    ],
)
def test_html_strip(client, value, expected):
    assert _src(client, [{"html_strip": {"field": "h"}}], {"h": value}) == {"h": expected}


@pytest.mark.parametrize(
    "value,expected",
    [
        ("1kb", 1024),
        ("1KB", 1024),
        ("1k", 1024),
        ("1 kb", 1024),
        ("2mb", 2097152),
        ("1gb", 1073741824),
        ("1tb", 1099511627776),
        ("1pb", 1125899906842624),
        ("10b", 10),
        ("0b", 0),
    ],
)
def test_bytes_processor(client, value, expected):
    assert _src(client, [{"bytes": {"field": "b"}}], {"b": value}) == {"b": expected}


@pytest.mark.parametrize(
    "value,reason_start",
    [
        ("1024", "failed to parse setting [Ingest Field] with value [1024] as a size in bytes: unit is missing or unrecognized"),
        ("abc", "failed to parse setting [Ingest Field] with value [abc] as a size in bytes: unit is missing or unrecognized"),
        ("-1kb", "failed to parse setting [Ingest Field] with value [-1kb] as a size in bytes"),
        ("1.5kb", "Failed to parse bytes value [1.5kb]. Fractional bytes values have been deprecated"),
    ],
)
def test_bytes_errors(client, value, reason_start):
    err = _doc_error(client, [{"bytes": {"field": "b"}}], {"b": value})
    assert err["type"] == "parse_exception"
    assert err["reason"].startswith(reason_start)


# --------------------------------------------------------------------------
# fingerprint / community_id
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "method,expected",
    [
        ("MD5@2.16.0", "MD5@2.16.0:dEdjh8U8mlB4+AOL9u+FQg=="),
        ("SHA-1@2.16.0", "SHA-1@2.16.0:y3GvwVv1aS25FAGBVVaDm1Ve4yI="),
        ("SHA-256@2.16.0", "SHA-256@2.16.0:+Evi39sey8YcMKdQ8b80bge/B/neFvLerpGgRzuxtoE="),
        ("SHA3-256@2.16.0", "SHA3-256@2.16.0:2do5T924cvDmILko4pKsDPzmkxbMZJ44TFkJFUmMNh8="),
    ],
)
def test_fingerprint_methods(client, method, expected):
    assert _src(client, [{"fingerprint": {"fields": ["a"], "hash_method": method}}], {"a": "x"}) == {"a": "x", "fingerprint": expected}


def test_fingerprint_defaults_and_order(client):
    h = "SHA-1@2.16.0:5Nxar4R+soro5RWTWPlbmK6azXw="
    assert _src(client, [{"fingerprint": {"fields": ["a", "b"]}}], {"a": "x", "b": 1})["fingerprint"] == h
    assert _src(client, [{"fingerprint": {"fields": ["b", "a"]}}], {"a": "x", "b": 1})["fingerprint"] == h
    # unlisted fields do not influence the hash; no fields == all fields
    one = "SHA-1@2.16.0:y3GvwVv1aS25FAGBVVaDm1Ve4yI="
    assert _src(client, [{"fingerprint": {"fields": ["a"]}}], {"a": "x", "b": "y"})["fingerprint"] == one
    assert _src(client, [{"fingerprint": {}}], {"a": "x"})["fingerprint"] == one
    assert _src(client, [{"fingerprint": {"fields": ["a"], "target_field": "fp"}}], {"a": "x"}) == {"a": "x", "fp": one}


def test_fingerprint_string_and_number_hash_identically(client):
    h1 = _src(client, [{"fingerprint": {"fields": ["a"]}}], {"a": "1"})["fingerprint"]
    h2 = _src(client, [{"fingerprint": {"fields": ["a"]}}], {"a": 1})["fingerprint"]
    assert h1 == h2 == "SHA-1@2.16.0:84ZtNs7hvwUW7v6M03Vscw65n+U="


def test_fingerprint_errors(client):
    err = _doc_error(client, [{"fingerprint": {"fields": ["zz"]}}], {"a": "x"})
    assert err["reason"] == "field [zz] doesn't exist"
    assert _src(client, [{"fingerprint": {"fields": ["zz"], "ignore_missing": True}}], {"a": "x"}) == {"a": "x"}
    err = _config_error(client, [{"fingerprint": {"fields": ["a"], "hash_method": "MD5"}}])
    assert err["reason"] == "[hash_method] hash method must be MD5@2.16.0, SHA-1@2.16.0, SHA-256@2.16.0 or SHA3-256@2.16.0"
    err = _config_error(client, [{"fingerprint": {"fields": ["a"], "exclude_fields": ["b"]}}])
    assert err["reason"] == "[fields] either fields or exclude_fields can be set"


CID_FIELDS = {
    "source_ip_field": "s",
    "source_port_field": "sp",
    "destination_ip_field": "d",
    "destination_port_field": "dp",
}


@pytest.mark.parametrize(
    "proto_cfg,doc",
    [
        ({"iana_protocol_number_field": "p"}, {"s": "66.35.250.204", "sp": 80, "d": "128.232.110.120", "dp": 34855, "p": 6}),
        ({"protocol_field": "p"}, {"s": "66.35.250.204", "sp": 80, "d": "128.232.110.120", "dp": 34855, "p": "tcp"}),
        ({"protocol_field": "p"}, {"s": "128.232.110.120", "sp": 34855, "d": "66.35.250.204", "dp": 80, "p": "TCP"}),
    ],
)
def test_community_id_is_direction_independent(client, proto_cfg, doc):
    src = _src(client, [{"community_id": {**CID_FIELDS, **proto_cfg}}], doc)
    assert src["community_id"] == "1:LQU9qZlK+B5F3KDmev6m5PMibrg="


def test_community_id_seed_target_and_icmp(client):
    doc = {"s": "128.232.110.120", "sp": 34855, "d": "66.35.250.204", "dp": 80, "p": "TCP"}
    src = _src(client, [{"community_id": {**CID_FIELDS, "protocol_field": "p", "seed": 1, "target_field": "cid"}}], doc)
    assert src["cid"] == "1:3V71V58M3Ksw/yuFALMcW0LAHvc="
    assert "community_id" not in src
    icmp = {"s": "192.168.0.89", "d": "192.168.0.1", "p": "icmp", "t": 8, "c": 0}
    cfg = {"source_ip_field": "s", "destination_ip_field": "d", "protocol_field": "p", "icmp_type_field": "t", "icmp_code_field": "c"}
    assert _src(client, [{"community_id": cfg}], icmp)["community_id"] == "1:X0snYXpgwiv9TZtqg64sgzUn6Dk="


def test_community_id_tcp_requires_ports(client):
    cfg = {"source_ip_field": "s", "destination_ip_field": "d", "protocol_field": "p"}
    err = _doc_error(client, [{"community_id": cfg}], {"s": "1.1.1.1", "d": "2.2.2.2", "p": "tcp"})
    assert err["reason"] == "both source port and destination port field path cannot be null nor empty"


# --------------------------------------------------------------------------
# dot_expander
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        ({"field": "a.b"}, {"a.b": 1, "c.d": 2}, {"a": {"b": 1}, "c.d": 2}),
        ({"field": "a.b"}, {"a.b": 1, "a": {"c": 2}}, {"a": {"b": 1, "c": 2}}),
        ({"field": "b.c", "path": "a"}, {"a": {"b.c": 1}}, {"a": {"b": {"c": 1}}}),
        ({"field": "a.zz"}, {"x": 1}, {"x": 1}),
        ({"field": "a.b"}, {"a.b": 1, "a": {"b": 2}}, {"a": {"b": [2, 1]}}),
    ],
)
def test_dot_expander(client, cfg, doc, expected):
    assert _src(client, [{"dot_expander": cfg}], doc) == expected


def test_dot_expander_errors(client):
    err = _doc_error(client, [{"dot_expander": {"field": "a.b"}}], {"a.b": 1, "a": "scalar"})
    assert err["reason"] == "cannot expend [a.b], because [a] is not an object field, but a value field"
    for field in ("nodot", "*"):
        err = _config_error(client, [{"dot_expander": {"field": field}}])
        assert err["reason"] == "[field] field does not contain a dot"


# --------------------------------------------------------------------------
# foreach
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "inner,doc,expected",
    [
        ({"uppercase": {"field": "_ingest._value"}}, {"a": ["x", "y"]}, {"a": ["X", "Y"]}),
        (
            {"set": {"field": "_ingest._value.n", "value": "{{_ingest._value.v}}!"}},
            {"a": [{"v": 1}, {"v": 2}]},
            {"a": [{"v": 1, "n": "1!"}, {"v": 2, "n": "2!"}]},
        ),
        ({"uppercase": {"field": "_ingest._value"}}, {"a": []}, {"a": []}),
        ({"append": {"field": "out", "value": "{{_ingest._value}}"}}, {"a": ["x", "y"]}, {"a": ["x", "y"], "out": ["x", "y"]}),
    ],
)
def test_foreach(client, inner, doc, expected):
    assert _src(client, [{"foreach": {"field": "a", "processor": inner}}], doc) == expected


def test_foreach_ignore_missing(client):
    procs = [{"foreach": {"field": "zz", "ignore_missing": True, "processor": {"uppercase": {"field": "_ingest._value"}}}}]
    assert _src(client, procs, {}) == {}


def test_foreach_inner_error_is_per_doc(client):
    err = _doc_error(client, [{"foreach": {"field": "a", "processor": {"uppercase": {"field": "_ingest._value"}}}}], {"a": ["x", 1]})
    assert err["reason"] == "field [_ingest._value] of type [java.lang.Integer] cannot be cast to [java.lang.String]"


@pytest.mark.parametrize(
    "doc,reason",
    [
        ({}, "field [zz] not present as part of path [zz]"),
        ({"zz": "x"}, "field [zz] of type [java.lang.String] cannot be cast to [java.util.List]"),
        ({"zz": {"k": "x"}}, "field [zz] of type [java.util.HashMap] cannot be cast to [java.util.List]"),
    ],
)
def test_foreach_bad_field_fails_whole_simulate_request(client, doc, reason):
    r = _simulate(client, [{"foreach": {"field": "zz", "processor": {"uppercase": {"field": "_ingest._value"}}}}], [doc])
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == reason


# --------------------------------------------------------------------------
# fail / drop / script / pipeline
# --------------------------------------------------------------------------


def test_fail_processor_message_template(client):
    err = _doc_error(client, [{"fail": {"message": "boom {{b}}"}}], {"b": "x"})
    assert err == {"root_cause": [{"type": "fail_processor_exception", "reason": "boom x"}], "type": "fail_processor_exception", "reason": "boom x"}


def test_fail_requires_message(client):
    err = _config_error(client, [{"fail": {}}])
    assert err["reason"] == "[message] required property is missing"


def test_drop_conditional(client):
    r = _simulate(client, [{"drop": {"if": "ctx.k == 'x'"}}], [{"k": "x"}, {"k": "y"}])
    docs = r.json()["docs"]
    assert docs[0] is None
    assert docs[1]["doc"]["_source"] == {"k": "y"}


@pytest.mark.parametrize(
    "cfg,doc,expected",
    [
        ({"source": "ctx.c = ctx.a + ctx.b"}, {"a": 1, "b": 2}, {"a": 1, "b": 2, "c": 3}),
        ({"source": "ctx.c = params.x * 2", "params": {"x": 21}}, {}, {"c": 42}),
        ({"lang": "painless", "source": "ctx.remove('a')"}, {"a": 1, "b": 2}, {"b": 2}),
        ({"source": "ctx.t = ctx._ingest == null"}, {"a": 1}, {"a": 1, "t": True}),
        ({"source": "ctx.l = ctx.s.splitOnToken(',').length"}, {"s": "a,b"}, {"s": "a,b", "l": 2}),
        ({"source": "ctx.s = ctx.s.toUpperCase()"}, {"s": "ab"}, {"s": "AB"}),
        ({"source": "if (ctx.n > 1) { ctx.big = true }"}, {"n": 5}, {"n": 5, "big": True}),
    ],
)
def test_script_processor(client, cfg, doc, expected):
    assert _src(client, [{"script": cfg}], doc) == expected


def test_script_changes_index(client):
    r = _simulate(client, [{"script": {"source": "ctx._index = 'new-' + ctx.a"}}], [{"a": "x"}])
    doc = r.json()["docs"][0]["doc"]
    assert doc["_index"] == "new-x"
    assert doc["_source"] == {"a": "x"}


def test_script_runtime_error(client):
    err = _doc_error(client, [{"script": {"source": "throw new IllegalArgumentException('custom')"}}], {})
    assert err["type"] == "script_exception"
    assert err["reason"] == "runtime error"
    assert err["lang"] == "painless"
    assert err["caused_by"] == {"type": "illegal_argument_exception", "reason": "custom"}


def test_script_null_pointer(client):
    err = _doc_error(client, [{"script": {"source": "ctx.c = ctx.zz.length()"}}], {})
    assert err["type"] == "script_exception"
    assert err["caused_by"]["type"] == "null_pointer_exception"


def test_script_compile_error(client):
    err = _config_error(client, [{"script": {"source": "ctx.c = "}}])
    assert err["type"] == "script_exception"
    assert err["reason"] == "compile error"
    assert err["processor_type"] == "script"
    assert err["script_stack"] == ["ctx.c = ", "        ^---- HERE"]


def test_script_missing_stored_script(client):
    sid = _uid("nosuch")
    r = _simulate(client, [{"script": {"id": sid}}], [{}])
    assert r.status_code == 404
    err = r.json()["error"]
    assert err["type"] == "resource_not_found_exception"
    assert err["reason"] == f"unable to find script [{sid}] in cluster state"


def test_script_mustache_lang_rejected(client):
    r = _simulate(client, [{"script": {"lang": "mustache", "source": "x"}}], [{}])
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "mustache engine does not know how to handle context [ingest]"


def test_script_stored_by_id(client):
    sid = _uid("lt-script")
    r = client.put(f"/_scripts/{sid}", json={"script": {"lang": "painless", "source": "ctx.v = params.v"}})
    assert r.status_code == 200
    try:
        src = _src(client, [{"script": {"id": sid, "params": {"v": 9}}}], {})
        assert src == {"v": 9}
    finally:
        client.delete(f"/_scripts/{sid}")


def test_pipeline_processor_nested(client, pipeline_factory):
    inner = pipeline_factory([{"set": {"field": "inner", "value": "{{_ingest.pipeline}}"}}])
    outer = pipeline_factory(
        [
            {"set": {"field": "o", "value": "{{_ingest.pipeline}}"}},
            {"pipeline": {"name": inner}},
            {"set": {"field": "o2", "value": "{{_ingest.pipeline}}"}},
        ]
    )
    r = client.post(f"/_ingest/pipeline/{outer}/_simulate", json={"docs": [{"_source": {}}]})
    assert r.json()["docs"][0]["doc"]["_source"] == {"o": outer, "inner": inner, "o2": outer}


def test_pipeline_processor_verbose(client, pipeline_factory):
    inner = pipeline_factory([{"set": {"field": "i", "value": 1}}])
    outer = pipeline_factory([{"pipeline": {"name": inner}}, {"set": {"field": "o", "value": 2}}])
    r = client.post(f"/_ingest/pipeline/{outer}/_simulate", json={"docs": [{"_source": {}}]}, params={"verbose": "true"})
    results = r.json()["docs"][0]["processor_results"]
    assert [p["processor_type"] for p in results] == ["pipeline", "set", "set"]
    assert results[0] == {"processor_type": "pipeline", "status": "success"}
    assert results[1]["doc"]["_ingest"]["pipeline"] == inner
    assert results[2]["doc"]["_ingest"]["pipeline"] == outer
    assert results[2]["doc"]["_source"] == {"i": 1, "o": 2}


def test_pipeline_processor_missing_pipeline(client):
    name = _uid("missing")
    err = _doc_error(client, [{"pipeline": {"name": name}}], {})
    assert err["type"] == "illegal_state_exception"
    assert err["reason"] == f"Pipeline processor configured for non-existent pipeline [{name}]"


def test_pipeline_processor_cycle(client, pipeline_factory):
    name = _uid("lt-loop")
    pipeline_factory([{"pipeline": {"name": name}}], name=name)
    r = client.post(f"/_ingest/pipeline/{name}/_simulate", json={"docs": [{"_source": {}}]})
    err = r.json()["docs"][0]["error"]
    assert err["type"] == "illegal_state_exception"
    assert err["reason"] == f"Cycle detected for pipeline: {name}"


def test_pipeline_processor_name_template(client, pipeline_factory):
    inner = pipeline_factory([{"set": {"field": "hit", "value": True}}])
    src = _src(client, [{"pipeline": {"name": "{{target}}"}}], {"target": inner})
    assert src == {"target": inner, "hit": True}


# --------------------------------------------------------------------------
# user_agent
# --------------------------------------------------------------------------


def test_user_agent_chrome(client):
    src = _src(client, [{"user_agent": {"field": "ua"}}], {"ua": UA_CHROME})
    assert src["user_agent"] == {
        "name": "Chrome",
        "original": UA_CHROME,
        "os": {"name": "Mac OS X", "version": "10.9.2", "full": "Mac OS X 10.9.2"},
        "device": {"name": "Mac"},
        "version": "33.0.1750.149",
    }


def test_user_agent_iphone(client):
    src = _src(client, [{"user_agent": {"field": "ua"}}], {"ua": UA_IPHONE})
    ua = src["user_agent"]
    assert ua["name"] == "Mobile Safari"
    assert ua["os"] == {"name": "iOS", "version": "12.2", "full": "iOS 12.2"}
    assert ua["device"] == {"name": "iPhone"}
    assert ua["version"] == "12.1"


@pytest.mark.parametrize(
    "value,expected",
    [
        ("garbage", {"name": "Other", "device": {"name": "Other"}, "original": "garbage"}),
        ("curl/7.68.0", {"name": "curl", "original": "curl/7.68.0", "device": {"name": "Other"}, "version": "7.68.0"}),
    ],
)
def test_user_agent_other(client, value, expected):
    assert _src(client, [{"user_agent": {"field": "ua"}}], {"ua": value})["user_agent"] == expected


def test_user_agent_properties_and_target(client):
    src = _src(client, [{"user_agent": {"field": "ua", "target_field": "x", "properties": ["name", "version"]}}], {"ua": UA_CHROME})
    assert src == {"ua": UA_CHROME, "x": {"name": "Chrome", "version": "33.0.1750.149"}}


def test_user_agent_errors(client):
    err = _config_error(client, [{"user_agent": {"field": "ua", "properties": ["bogus"]}}])
    assert err["reason"].startswith("[properties] illegal property value [bogus]. valid values are [NAME, MAJOR")
    err = _doc_error(client, [{"user_agent": {"field": "zz"}}], {"ua": UA_CHROME})
    assert err["reason"] == "field [zz] not present as part of path [zz]"


# --------------------------------------------------------------------------
# failure handling, conditions, tags
# --------------------------------------------------------------------------


def test_on_failure_metadata(client):
    procs = [
        {
            "uppercase": {
                "field": "a",
                "tag": "T",
                "on_failure": [
                    {
                        "set": {
                            "field": "err",
                            "value": "{{_ingest.on_failure_message}}|{{_ingest.on_failure_processor_type}}|{{_ingest.on_failure_processor_tag}}",
                        }
                    }
                ],
            }
        }
    ]
    src = _src(client, procs, {"a": 1})
    assert src == {"a": 1, "err": "field [a] of type [java.lang.Integer] cannot be cast to [java.lang.String]|uppercase|T"}


def test_on_failure_continues_pipeline(client):
    procs = [
        {"uppercase": {"field": "a", "on_failure": [{"set": {"field": "f", "value": 1}}]}},
        {"set": {"field": "after", "value": True}},
    ]
    assert _src(client, procs, {"a": 1}) == {"a": 1, "f": 1, "after": True}


def test_on_failure_handler_failure_propagates(client):
    err = _doc_error(client, [{"uppercase": {"field": "a", "on_failure": [{"fail": {"message": "inner"}}]}}], {"a": 1})
    assert err["type"] == "fail_processor_exception"
    assert err["reason"] == "inner"


def test_nested_on_failure(client):
    procs = [
        {
            "uppercase": {
                "field": "a",
                "on_failure": [
                    {"fail": {"message": "inner", "on_failure": [{"set": {"field": "x", "value": "{{_ingest.on_failure_message}}"}}]}}
                ],
            }
        }
    ]
    assert _src(client, procs, {"a": 1}) == {"a": 1, "x": "inner"}


def test_ignore_failure(client):
    procs = [{"uppercase": {"field": "a", "ignore_failure": True}}, {"set": {"field": "b", "value": 1}}]
    assert _src(client, procs, {"a": 1}) == {"a": 1, "b": 1}


def test_failure_stops_pipeline(client):
    err = _doc_error(client, [{"uppercase": {"field": "a"}}, {"set": {"field": "b", "value": 1}}], {"a": 1})
    assert err["type"] == "illegal_argument_exception"


def test_on_failure_must_not_be_empty(client):
    err = _config_error(client, [{"set": {"field": "a", "value": 1, "on_failure": []}}])
    assert err["reason"] == "[on_failure] processors list cannot be empty"


@pytest.mark.parametrize(
    "cond,doc,expected",
    [
        ("ctx.a > 5", {"a": 10}, {"a": 10, "b": 1}),
        ("ctx.a > 5", {"a": 1}, {"a": 1}),
        ("ctx.zz?.y == 1", {"a": 1}, {"a": 1}),
        ("ctx.containsKey('a')", {"a": 1}, {"a": 1, "b": 1}),
        ({"source": "ctx.a == params.v", "params": {"v": 3}}, {"a": 3}, {"a": 3, "b": 1}),
    ],
)
def test_if_condition(client, cond, doc, expected):
    assert _src(client, [{"set": {"field": "b", "value": 1, "if": cond}}], doc) == expected


def test_if_condition_runtime_errors(client):
    err = _doc_error(client, [{"set": {"field": "b", "value": 1, "if": "ctx.a"}}], {"a": 1})
    assert err["type"] == "script_exception"
    assert err["caused_by"] == {"type": "class_cast_exception", "reason": "cannot cast def [int] to boolean"}
    err = _doc_error(client, [{"set": {"field": "b", "value": 1, "if": "ctx.zz.y == 1"}}], {"a": 1})
    assert err["caused_by"]["type"] == "null_pointer_exception"


def test_if_condition_compile_error(client):
    err = _config_error(client, [{"set": {"field": "b", "value": 1, "if": "ctx.a = "}}])
    assert err["type"] == "script_exception"
    assert err["reason"] == "compile error"
    assert err["processor_type"] == "set"


def test_description_and_tag_are_accepted(client):
    src = _src(client, [{"set": {"field": "b", "value": 1, "description": "d", "tag": "t"}}], {})
    assert src == {"b": 1}


# --------------------------------------------------------------------------
# pipelines applied at index time
# --------------------------------------------------------------------------


def test_index_with_pipeline_param(client, index_factory, pipeline_factory):
    idx = index_factory()
    p = pipeline_factory([{"set": {"field": "via", "value": "{{_ingest.pipeline}}"}}, {"lowercase": {"field": "n"}}])
    r = client.put(f"/{idx}/_doc/1", json={"n": "ABC"}, params={"pipeline": p, "refresh": "true"})
    assert r.status_code == 201
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"n": "abc", "via": p}


def test_index_ingest_timestamp_stored(client, index_factory, pipeline_factory):
    idx = index_factory()
    p = pipeline_factory([{"set": {"field": "ts", "value": "{{_ingest.timestamp}}"}}])
    client.put(f"/{idx}/_doc/1", json={}, params={"pipeline": p, "refresh": "true"})
    assert TS_RE.match(client.get(f"/{idx}/_doc/1").json()["_source"]["ts"])


def test_index_with_missing_pipeline_400(client, index_factory):
    idx = index_factory()
    name = _uid("missing")
    r = client.put(f"/{idx}/_doc/1", json={"a": 1}, params={"pipeline": name})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == f"pipeline with id [{name}] does not exist"


def test_index_with_failing_pipeline_500(client, index_factory, pipeline_factory):
    idx = index_factory()
    p = pipeline_factory([{"fail": {"message": "nope {{a}}"}}])
    r = client.put(f"/{idx}/_doc/1", json={"a": "x"}, params={"pipeline": p})
    assert r.status_code == 500
    assert r.json()["error"]["type"] == "fail_processor_exception"
    assert r.json()["error"]["reason"] == "nope x"
    assert client.get(f"/{idx}/_doc/1").status_code == 404


def test_index_dropped_doc_is_noop(client, index_factory, pipeline_factory):
    idx = index_factory()
    p = pipeline_factory([{"drop": {}}])
    r = client.put(f"/{idx}/_doc/1", json={"a": 1}, params={"pipeline": p, "refresh": "true"})
    assert r.status_code == 200
    body = r.json()
    assert body["result"] == "noop"
    assert body["_version"] == -3
    assert body["_shards"] == {"total": 0, "successful": 0, "failed": 0}
    assert client.get(f"/{idx}/_doc/1").status_code == 404


def test_default_and_final_pipeline(client, index_factory, pipeline_factory):
    p1 = pipeline_factory([{"set": {"field": "p1", "value": True}}])
    p2 = pipeline_factory([{"set": {"field": "p2", "value": "{{_index}}"}}])
    other = pipeline_factory([{"set": {"field": "other", "value": True}}])
    idx = index_factory(settings={"index.default_pipeline": p1, "index.final_pipeline": p2})
    client.put(f"/{idx}/_doc/1", json={"a": 1}, params={"refresh": "true"})
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"a": 1, "p1": True, "p2": idx}
    client.put(f"/{idx}/_doc/2", json={"a": 1}, params={"refresh": "true", "pipeline": "_none"})
    assert client.get(f"/{idx}/_doc/2").json()["_source"] == {"a": 1, "p2": idx}
    client.put(f"/{idx}/_doc/3", json={"a": 1}, params={"refresh": "true", "pipeline": other})
    assert client.get(f"/{idx}/_doc/3").json()["_source"] == {"a": 1, "other": True, "p2": idx}


def test_default_pipeline_missing_fails_indexing(client, index_factory):
    name = _uid("missing")
    idx = index_factory(settings={"index.default_pipeline": name})
    r = client.put(f"/{idx}/_doc/1", json={"a": 1})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == f"pipeline with id [{name}] does not exist"


def test_default_pipeline_setting_visible(client, index_factory, pipeline_factory):
    p = pipeline_factory([])
    idx = index_factory(settings={"index.default_pipeline": p})
    s = client.get(f"/{idx}/_settings").json()[idx]["settings"]["index"]
    assert s["default_pipeline"] == p


def test_pipeline_in_use_can_be_deleted(client, index_factory):
    name = _uid("lt-pipe")
    assert client.put(f"/_ingest/pipeline/{name}", json={"processors": []}).status_code == 200
    idx = index_factory(settings={"index.default_pipeline": name})
    assert client.delete(f"/_ingest/pipeline/{name}").status_code == 200
    r = client.put(f"/{idx}/_doc/1", json={"a": 1})
    assert r.status_code == 400


def test_default_pipeline_applies_to_update_api(client, index_factory, pipeline_factory):
    p = pipeline_factory([{"script": {"source": "ctx.runs = (ctx.runs == null ? 0 : ctx.runs) + 1"}}])
    idx = index_factory(settings={"index.default_pipeline": p})
    client.put(f"/{idx}/_doc/1", json={"a": 1}, params={"refresh": "true"})
    assert client.get(f"/{idx}/_doc/1").json()["_source"]["runs"] == 1
    r = client.post(f"/{idx}/_update/1", json={"doc": {"b": 1}}, params={"refresh": "true"})
    assert r.status_code == 200
    src = client.get(f"/{idx}/_doc/1").json()["_source"]
    assert src["b"] == 1
    assert src["runs"] == 2  # default pipeline re-runs on partial update


def test_pipeline_reroutes_index(client, index_factory, pipeline_factory, extra_indices):
    target = _uid("lt-target")
    extra_indices.append(target)
    source = index_factory()
    p = pipeline_factory([{"set": {"field": "_index", "value": target}}])
    r = client.put(f"/{source}/_doc/1", json={"a": 1}, params={"pipeline": p, "refresh": "true"})
    assert r.status_code == 201
    assert r.json()["_index"] == target
    assert client.get(f"/{target}/_doc/1").json()["_source"] == {"a": 1}
    assert client.get(f"/{source}/_doc/1").status_code == 404


def test_default_pipeline_may_reroute_index(client, index_factory, pipeline_factory, extra_indices):
    target = _uid("lt-target")
    extra_indices.append(target)
    p = pipeline_factory([{"set": {"field": "_index", "value": target}}])
    idx = index_factory(settings={"index.default_pipeline": p})
    r = client.put(f"/{idx}/_doc/1", json={"a": 1}, params={"refresh": "true"})
    assert r.status_code == 201
    assert r.json()["_index"] == target


def test_final_pipeline_cannot_change_index(client, index_factory, pipeline_factory, extra_indices):
    target = _uid("lt-target")
    extra_indices.append(target)
    p = pipeline_factory([{"set": {"field": "_index", "value": target}}])
    idx = index_factory(settings={"index.final_pipeline": p})
    r = client.put(f"/{idx}/_doc/1", json={"a": 1})
    assert r.status_code == 500
    assert r.json()["error"]["type"] == "illegal_state_exception"
    assert r.json()["error"]["reason"] == f"FINAL pipeline [{p}] can't change the target index"


def test_pipeline_sets_id_and_routing(client, index_factory, pipeline_factory):
    idx = index_factory()
    p = pipeline_factory([{"set": {"field": "_id", "value": "id-{{k}}"}}, {"set": {"field": "_routing", "value": "r1"}}])
    r = client.post(f"/{idx}/_doc", json={"k": "z"}, params={"pipeline": p, "refresh": "true"})
    assert r.status_code == 201
    assert r.json()["_id"] == "id-z"
    got = client.get(f"/{idx}/_doc/id-z", params={"routing": "r1"}).json()
    assert got["_routing"] == "r1"
    assert got["_source"] == {"k": "z"}


def test_bulk_per_action_pipeline_overrides_request_pipeline(client, index_factory, pipeline_factory):
    idx = index_factory()
    p1 = pipeline_factory([{"set": {"field": "p1", "value": True}}])
    p2 = pipeline_factory([{"set": {"field": "p2", "value": True}}])
    body = _bulk(
        client,
        [
            {"index": {"_index": idx, "_id": "b1", "pipeline": p1}},
            {"a": 1},
            {"index": {"_index": idx, "_id": "b2"}},
            {"a": 2},
            {"index": {"_index": idx, "_id": "b3", "pipeline": "_none"}},
            {"a": 3},
        ],
        pipeline=p2,
    )
    assert body["errors"] is False
    assert "ingest_took" in body
    assert client.get(f"/{idx}/_doc/b1").json()["_source"] == {"a": 1, "p1": True}
    assert client.get(f"/{idx}/_doc/b2").json()["_source"] == {"a": 2, "p2": True}
    assert client.get(f"/{idx}/_doc/b3").json()["_source"] == {"a": 3}


def test_bulk_pipeline_errors_and_drops(client, index_factory, pipeline_factory):
    idx = index_factory()
    drop = pipeline_factory([{"drop": {}}])
    fail = pipeline_factory([{"fail": {"message": "nope {{a}}"}}])
    missing = _uid("missing")
    body = _bulk(
        client,
        [
            {"index": {"_index": idx, "_id": "b1"}},
            {"a": 1},
            {"index": {"_index": idx, "_id": "b3", "pipeline": drop}},
            {"a": 3},
            {"index": {"_index": idx, "_id": "b4", "pipeline": fail}},
            {"a": 4},
            {"create": {"_index": idx, "_id": "b5", "pipeline": missing}},
            {"a": 5},
        ],
    )
    assert body["errors"] is True
    items = {next(iter(i.values()))["_id"]: i for i in body["items"]}
    assert items["b1"]["index"]["status"] == 201
    assert items["b3"]["index"]["status"] == 200
    assert items["b3"]["index"]["result"] == "noop"
    assert items["b4"]["index"]["status"] == 500
    assert items["b4"]["index"]["error"] == {"type": "fail_processor_exception", "reason": "nope 4"}
    assert items["b5"]["create"]["status"] == 400
    assert items["b5"]["create"]["error"] == {
        "type": "illegal_argument_exception",
        "reason": f"pipeline with id [{missing}] does not exist",
    }
    r = client.post(f"/{idx}/_search", json={"query": {"match_all": {}}})
    assert r.json()["hits"]["total"]["value"] == 1


def test_bulk_dropped_items_keep_order(client, index_factory, pipeline_factory):
    idx = index_factory()
    drop = pipeline_factory([{"drop": {"if": "ctx.d == true"}}])
    body = _bulk(
        client,
        [
            {"index": {"_index": idx, "_id": "1"}},
            {"d": True},
            {"index": {"_index": idx, "_id": "2"}},
            {"d": False},
        ],
        pipeline=drop,
    )
    assert [i["index"]["_id"] for i in body["items"]] == ["1", "2"]
    assert body["items"][0]["index"]["result"] == "noop"
    assert body["items"][0]["index"]["status"] == 200
    assert body["items"][1]["index"]["status"] == 201


def test_reindex_with_pipeline(client, index_factory, bulk_index, pipeline_factory):
    src = index_factory()
    dst = index_factory()
    bulk_index(src, [{"n": "a"}, {"n": "b"}])
    p = pipeline_factory([{"uppercase": {"field": "n"}}])
    r = client.post(
        "/_reindex",
        json={"source": {"index": src}, "dest": {"index": dst, "pipeline": p}},
        params={"refresh": "true"},
    )
    assert r.status_code == 200
    assert r.json()["created"] == 2
    hits = client.post(f"/{dst}/_search", json={"sort": [{"n.keyword": "asc"}]}).json()["hits"]["hits"]
    assert [h["_source"]["n"] for h in hits] == ["A", "B"]


def test_update_by_query_with_pipeline(client, index_factory, bulk_index, pipeline_factory):
    idx = index_factory()
    bulk_index(idx, [{"n": "a"}, {"n": "b"}])
    p = pipeline_factory([{"set": {"field": "touched", "value": True}}])
    r = client.post(f"/{idx}/_update_by_query", params={"pipeline": p, "refresh": "true"})
    assert r.status_code == 200
    assert r.json()["updated"] == 2
    r = client.post(f"/{idx}/_count", json={"query": {"term": {"touched": True}}})
    assert r.json()["count"] == 2


# --------------------------------------------------------------------------
# search pipelines
# --------------------------------------------------------------------------


def test_search_pipeline_crud(client, search_pipeline_factory):
    body = {
        "description": "d",
        "request_processors": [{"filter_query": {"query": {"term": {"cat": "a"}}}}],
        "response_processors": [{"rename_field": {"field": "t", "target_field": "text"}}],
    }
    name = search_pipeline_factory(body)
    r = client.get(f"/_search/pipeline/{name}")
    assert r.status_code == 200
    assert r.json() == {name: body}


def test_search_pipeline_missing(client):
    name = _uid("missing")
    r = client.get(f"/_search/pipeline/{name}")
    assert r.status_code == 404
    assert r.json() == {}
    r = client.delete(f"/_search/pipeline/{name}")
    assert r.status_code == 404
    assert r.json()["error"]["type"] == "resource_not_found_exception"
    assert r.json()["error"]["reason"] == f"pipeline [{name}] is missing"


def test_search_pipeline_delete(client):
    name = _uid("lt-spipe")
    assert client.put(f"/_search/pipeline/{name}", json={}).status_code == 200
    r = client.delete(f"/_search/pipeline/{name}")
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    assert client.get(f"/_search/pipeline/{name}").status_code == 404


def test_search_pipeline_get_multiple(client, search_pipeline_factory):
    a = search_pipeline_factory({})
    b = search_pipeline_factory({"response_processors": [{"truncate_hits": {"target_size": 1}}]})
    r = client.get(f"/_search/pipeline/{a},{b}")
    assert r.status_code == 200
    assert set(r.json()) == {a, b}


@pytest.mark.parametrize(
    "body,reason",
    [
        ({"request_processors": [{"bogus": {}}]}, "Invalid processor type bogus"),
        ({"request_processors": [{"rename_field": {"field": "a", "target_field": "b"}}]}, "Invalid processor type rename_field"),
        ({"response_processors": [{"filter_query": {"query": {"match_all": {}}}}]}, "Invalid processor type filter_query"),
        ({"request_processors": [{"filter_query": {}}]}, "Did not specify the query property in processor of type filter_query"),
    ],
)
def test_search_pipeline_invalid(client, body, reason):
    name = _uid("lt-sbad")
    r = client.put(f"/_search/pipeline/{name}", json=body)
    try:
        assert r.status_code == 400
        assert r.json()["error"]["type"] == "illegal_argument_exception"
        assert r.json()["error"]["reason"] == reason
    finally:
        client.delete(f"/_search/pipeline/{name}")


@pytest.mark.parametrize(
    "proc,reason",
    [
        ({"oversample": {"sample_factor": 0.5}}, "[sample_factor] Value must be >= 1.0"),
        ({"oversample": {}}, "[sample_factor] required property is missing"),
    ],
)
def test_oversample_config_errors(client, sidx, proc, reason):
    r = _search_with(client, sidx, {"request_processors": [proc]})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "parse_exception"
    assert r.json()["error"]["reason"] == reason


def test_search_with_named_pipeline(client, sidx, search_pipeline_factory):
    name = search_pipeline_factory(
        {
            "request_processors": [{"filter_query": {"query": {"term": {"cat": "a"}}}}],
            "response_processors": [{"rename_field": {"field": "t", "target_field": "text"}}],
        }
    )
    r = client.post(f"/{sidx}/_search", json={"query": {"match": {"t": "x"}}, "sort": ["n"]}, params={"search_pipeline": name})
    hits = r.json()["hits"]
    assert hits["total"]["value"] == 1
    assert hits["hits"][0]["_id"] == "0"
    assert hits["hits"][0]["_source"] == {"cat": "a", "n": 0, "text": "x y", "tags": "q,w,e"}


def test_search_with_undefined_pipeline(client, sidx):
    name = _uid("missing")
    r = client.post(f"/{sidx}/_search", json={"query": {"match_all": {}}}, params={"search_pipeline": name})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == f"Pipeline {name} is not defined"


def test_search_with_inline_pipeline(client, sidx):
    r = _search_with(client, sidx, {"request_processors": [{"filter_query": {"query": {"term": {"cat": "b"}}}}]})
    assert _ids(r) == ["2", "3"]


def test_filter_query_keeps_scoring_query(client, sidx):
    r = _search_with(
        client,
        sidx,
        {"request_processors": [{"filter_query": {"query": {"terms": {"cat": ["a", "c"]}}}}]},
        {"query": {"match": {"t": "x"}}, "sort": ["n"]},
    )
    assert _ids(r) == ["0", "4"]


def test_rename_field_missing_errors(client, sidx):
    r = _search_with(client, sidx, {"response_processors": [{"rename_field": {"field": "zz", "target_field": "q"}}]})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "Document with id 0 is missing field zz"


def test_rename_field_ignore_missing(client, sidx):
    r = _search_with(client, sidx, {"response_processors": [{"rename_field": {"field": "zz", "target_field": "q", "ignore_missing": True}}]})
    assert _ids(r) == ["0", "1", "2", "3", "4"]


def test_rename_field_overwrites_existing(client, sidx):
    r = _search_with(
        client,
        sidx,
        {"response_processors": [{"rename_field": {"field": "cat", "target_field": "n"}}]},
        {"query": {"match_all": {}}, "sort": ["n"], "size": 1},
    )
    assert r.json()["hits"]["hits"][0]["_source"] == {"t": "x y", "n": "a", "tags": "q,w,e"}


@pytest.mark.parametrize(
    "source,expected",
    [
        ("ctx._source.size = 2", ["0", "1"]),
        ("ctx._source['from'] = 3", ["3", "4"]),
        ("ctx._source.size = 1; ctx._source['from'] = 1", ["1"]),
    ],
)
def test_script_request_processor(client, sidx, source, expected):
    r = _search_with(client, sidx, {"request_processors": [{"script": {"source": source}}]})
    assert _ids(r) == expected


def test_oversample_then_truncate(client, sidx):
    body = {"query": {"match_all": {}}, "sort": ["n"], "size": 2}
    r = _search_with(client, sidx, {"request_processors": [{"oversample": {"sample_factor": 2}}]}, body)
    assert _ids(r) == ["0", "1", "2", "3"]
    r = _search_with(
        client,
        sidx,
        {"request_processors": [{"oversample": {"sample_factor": 2}}], "response_processors": [{"truncate_hits": {}}]},
        body,
    )
    assert _ids(r) == ["0", "1"]
    assert r.json()["hits"]["total"]["value"] == 5


def test_truncate_hits_target_size(client, sidx):
    r = _search_with(
        client,
        sidx,
        {"response_processors": [{"truncate_hits": {"target_size": 1}}]},
        {"query": {"match_all": {}}, "sort": ["n"], "size": 3},
    )
    assert _ids(r) == ["0"]


def test_truncate_hits_without_target_size_errors(client, sidx):
    r = _search_with(client, sidx, {"response_processors": [{"truncate_hits": {}}]})
    assert r.status_code == 500
    assert r.json()["error"]["type"] == "illegal_state_exception"
    assert r.json()["error"]["reason"] == "Must specify target_size unless an earlier processor set original_size"


def test_collapse_response_processor(client, sidx):
    r = _search_with(client, sidx, {"response_processors": [{"collapse": {"field": "cat"}}]})
    assert _ids(r) == ["0", "2", "4"]
    assert r.json()["hits"]["total"]["value"] == 5


def test_split_and_sort_response_processors(client, sidx):
    body = {"query": {"match_all": {}}, "sort": ["n"], "size": 1}
    r = _search_with(client, sidx, {"response_processors": [{"split": {"field": "tags", "separator": ","}}]}, body)
    assert r.json()["hits"]["hits"][0]["_source"]["tags"] == ["q", "w", "e"]
    r = _search_with(
        client,
        sidx,
        {
            "response_processors": [
                {"split": {"field": "tags", "separator": ",", "target_field": "tl"}},
                {"sort": {"field": "tl", "order": "desc"}},
            ]
        },
        body,
    )
    src = r.json()["hits"]["hits"][0]["_source"]
    assert src["tags"] == "q,w,e"
    assert src["tl"] == ["w", "q", "e"]


def test_sort_response_processor_on_non_list_is_noop(client, sidx):
    r = _search_with(client, sidx, {"response_processors": [{"sort": {"field": "tags"}}]}, {"query": {"match_all": {}}, "sort": ["n"], "size": 1})
    assert r.json()["hits"]["hits"][0]["_source"]["tags"] == "q,w,e"


def test_index_default_search_pipeline(client, sidx, search_pipeline_factory):
    name = search_pipeline_factory({"request_processors": [{"filter_query": {"query": {"term": {"cat": "b"}}}}]})
    r = client.put(f"/{sidx}/_settings", json={"index.search.default_pipeline": name})
    assert r.status_code == 200
    assert _ids(_search_with(client, sidx, None)) == ["2", "3"]
    assert _ids(_search_with(client, sidx, None, search_pipeline="_none")) == ["0", "1", "2", "3", "4"]
    # the default search pipeline also applies to aggregations and _count
    body = {"size": 0, "aggs": {"c": {"terms": {"field": "cat"}}}}
    aggs = client.post(f"/{sidx}/_search", json=body).json()["aggregations"]["c"]["buckets"]
    assert aggs == [{"key": "b", "doc_count": 2}]
    assert client.post(f"/{sidx}/_count", json={"query": {"match_all": {}}}).json()["count"] == 2
    client.put(f"/{sidx}/_settings", json={"index.search.default_pipeline": None})
    assert len(_ids(_search_with(client, sidx, None))) == 5


def test_index_default_search_pipeline_missing(client, sidx):
    name = _uid("missing")
    client.put(f"/{sidx}/_settings", json={"index.search.default_pipeline": name})
    r = client.post(f"/{sidx}/_search", json={"query": {"match_all": {}}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == f"Pipeline {name} is not defined"


def test_search_pipeline_verbose(client, sidx, search_pipeline_factory):
    name = search_pipeline_factory({"request_processors": [{"filter_query": {"query": {"term": {"cat": "b"}}}}]})
    r = client.post(
        f"/{sidx}/_search",
        json={"query": {"match_all": {}}, "sort": ["n"]},
        params={"search_pipeline": name, "verbose_pipeline": "true"},
    )
    body = r.json()
    assert [h["_id"] for h in body["hits"]["hits"]] == ["2", "3"]
    (res,) = body["processor_results"]
    assert res["processor_name"] == "filter_query"
    assert res["status"] == "success"
    assert "duration_millis" in res
    q = res["output_data"]["query"]["bool"]
    assert q["filter"] == [{"term": {"cat": {"boost": 1.0, "value": "b"}}}]
    assert q["must"] == [{"match_all": {"boost": 1.0}}]
