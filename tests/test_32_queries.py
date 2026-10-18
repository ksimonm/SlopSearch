"""Term-level, compound, joining, geo and specialized queries.

Black-box compatibility spec recorded against OpenSearch 3.9.0.
"""

import base64
import json
import math
import uuid

import pytest

pytestmark = pytest.mark.queries


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def _uname(prefix):
    return f"q32-{prefix}-{uuid.uuid4().hex[:8]}"


def _create(client, mappings, docs=None, settings=None, routing=None):
    name = _uname("idx")
    st = {"number_of_shards": 1, "number_of_replicas": 0}
    st.update(settings or {})
    r = client.put(f"/{name}", json={"settings": st, "mappings": mappings})
    assert r.status_code == 200, r.text
    if docs:
        _bulk(client, name, docs)
    return name


def _bulk(client, name, docs):
    """docs: list of (id, source) or (id, source, routing)."""
    lines = []
    for d in docs:
        meta = {"_id": str(d[0])}
        if len(d) > 2 and d[2] is not None:
            meta["routing"] = d[2]
        lines.append(json.dumps({"index": meta}))
        lines.append(json.dumps(d[1]))
    r = client.post(
        f"/{name}/_bulk",
        content=("\n".join(lines) + "\n").encode(),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200, r.text
    assert r.json()["errors"] is False, r.text


def _drop(client, name):
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def _raw(client, index, body, **params):
    return client.post(f"/{index}/_search", json=body, params=params)


def _s(client, index, query, **extra):
    body = {"query": query, "size": 100}
    body.update(extra)
    r = _raw(client, index, body)
    assert r.status_code == 200, r.text
    return r.json()


def _ids(res):
    return {h["_id"] for h in res["hits"]["hits"]}


def _order(res):
    return [h["_id"] for h in res["hits"]["hits"]]


def _scores(res):
    return {h["_id"]: h["_score"] for h in res["hits"]["hits"]}


def _q(client, index, query):
    return _ids(_s(client, index, query))


def _shard_error(r, root_type, reason_part=None):
    assert r.status_code == 400, r.text
    err = r.json()["error"]
    assert err["type"] == "search_phase_execution_exception"
    assert err["root_cause"][0]["type"] == root_type
    if reason_part is not None:
        assert reason_part in err["root_cause"][0]["reason"], err
    return err


def _parse_error(r, reason_part=None, etype="parsing_exception"):
    assert r.status_code == 400, r.text
    err = r.json()["error"]
    assert err["type"] == etype
    assert err["root_cause"][0]["type"] == etype
    if reason_part is not None:
        assert reason_part in err["reason"], err
    return err


# --------------------------------------------------------------------------
# term-level dataset
# --------------------------------------------------------------------------

TL_MAPPING = {
    "properties": {
        "kw": {"type": "keyword"},
        "num": {"type": "integer"},
        "lng": {"type": "long"},
        "dbl": {"type": "double"},
        "flag": {"type": "boolean"},
        "ip": {"type": "ip"},
        "dt": {"type": "date"},
        "tags": {"type": "keyword"},
        "txt": {"type": "text"},
        "obj": {"properties": {"a": {"type": "keyword"}, "b": {"type": "integer"}}},
        "rng": {"type": "integer_range"},
        "drng": {"type": "date_range"},
        "req": {"type": "integer"},
    }
}

TL_DOCS = [
    ("1", {"kw": "Apple", "num": 1, "lng": 10000000000, "dbl": 1.5, "flag": True,
           "ip": "192.168.0.1", "dt": "2000-01-01T00:00:00Z", "tags": ["a", "b"],
           "txt": "Quick brown fox", "obj": {"a": "x", "b": 1},
           "rng": {"gte": 1, "lte": 5},
           "drng": {"gte": "2000-01-01", "lte": "2000-01-31"}, "req": 2}),
    ("2", {"kw": "apple", "num": 2, "lng": -5, "dbl": 2.5, "flag": False,
           "ip": "192.168.0.2", "dt": "2000-01-02T12:00:00Z", "tags": ["b", "c"],
           "txt": "lazy dog", "obj": {"a": "y"}, "rng": {"gte": 10, "lte": 20},
           "drng": {"gte": "2000-02-01", "lte": "2000-02-28"}, "req": 1}),
    ("3", {"kw": "banana", "num": 3, "dbl": -1.0, "flag": True, "ip": "10.0.0.1",
           "dt": "2000-01-03", "tags": [], "txt": "the quick dog", "obj": {},
           "rng": {"gte": 3, "lte": 12}, "req": 3}),
    ("4", {"kw": "Cherry pie", "num": 10, "ip": "2001:db8::1",
           "dt": "1999-12-31T23:59:59Z", "tags": ["a"], "flag": None,
           "rng": {"gte": 5}, "req": 1}),
    ("5", {"kw": "", "num": None, "tags": None, "txt": "", "obj": None}),
    ("6", {"dt": "2100-01-01", "tags": [None], "num": [7, 20]}),
    ("7", {"kw": "a*b?c\\d", "num": 100}),
    ("8", {"kw": "x.y", "num": -3, "dbl": 0.0}),
]


@pytest.fixture(scope="module")
def tl(client):
    name = _create(client, TL_MAPPING, TL_DOCS)
    yield name
    _drop(client, name)


# --------------------------------------------------------------------------
# term
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "field,value,expected",
    [
        ("kw", "apple", {"2"}),
        ("kw", "Apple", {"1"}),
        ("kw", "APPLE", set()),
        ("kw", "Cherry pie", {"4"}),
        ("kw", "Cherry", set()),
        ("kw", "", {"5"}),
        ("num", 2, {"2"}),
        ("num", "2", {"2"}),
        ("num", 2.0, {"2"}),
        ("num", 2.5, set()),
        ("num", 7, {"6"}),
        ("num", 20, {"6"}),
        ("num", -3, {"8"}),
        ("lng", 10000000000, {"1"}),
        ("lng", "-5", {"2"}),
        ("dbl", 1.5, {"1"}),
        ("dbl", "2.5", {"2"}),
        ("dbl", 0, {"8"}),
        ("dbl", -1, {"3"}),
        ("flag", True, {"1", "3"}),
        ("flag", "true", {"1", "3"}),
        ("flag", False, {"2"}),
        ("flag", "false", {"2"}),
        ("ip", "192.168.0.1", {"1"}),
        ("ip", "192.168.0.0/16", {"1", "2"}),
        ("ip", "10.0.0.0/8", {"3"}),
        ("ip", "2001:db8::1", {"4"}),
        ("ip", "2001:0db8:0000::0001", {"4"}),
        ("ip", "2001:db8::/32", {"4"}),
        ("dt", "2000-01-01T00:00:00Z", {"1"}),
        ("dt", "2000-01-01", {"1"}),
        ("dt", 946684800000, {"1"}),
        ("dt", "2000-01-03", {"3"}),
        ("tags", "b", {"1", "2"}),
        ("tags", "a", {"1", "4"}),
        ("txt", "quick", {"1", "3"}),
        ("txt", "Quick", set()),
        ("obj.a", "x", {"1"}),
        ("missing_field", "x", set()),
        ("obj", "x", set()),
    ],
)
def test_term_values(client, tl, field, value, expected):
    assert _q(client, tl, {"term": {field: value}}) == expected


def test_term_long_form_equals_short_form(client, tl):
    a = _q(client, tl, {"term": {"kw": {"value": "apple"}}})
    b = _q(client, tl, {"term": {"kw": "apple"}})
    assert a == b == {"2"}


@pytest.mark.parametrize(
    "field,value,expected",
    [
        ("kw", "APPLE", {"1", "2"}),
        ("kw", "cherry PIE", {"4"}),
        ("txt", "QUICK", {"1", "3"}),
        ("tags", "B", {"1", "2"}),
    ],
)
def test_term_case_insensitive(client, tl, field, value, expected):
    q = {"term": {field: {"value": value, "case_insensitive": True}}}
    assert _q(client, tl, q) == expected


def test_term_case_insensitive_false_explicit(client, tl):
    q = {"term": {"kw": {"value": "APPLE", "case_insensitive": False}}}
    assert _q(client, tl, q) == set()


def test_term_boost_scales_score(client, tl):
    base = _scores(_s(client, tl, {"term": {"kw": "apple"}}))["2"]
    boosted = _scores(_s(client, tl, {"term": {"kw": {"value": "apple", "boost": 3.0}}}))["2"]
    assert boosted == pytest.approx(base * 3.0, rel=1e-5)


def test_term_boost_zero_still_matches(client, tl):
    res = _s(client, tl, {"term": {"kw": {"value": "apple", "boost": 0}}})
    assert _ids(res) == {"2"}
    assert res["hits"]["hits"][0]["_score"] == 0.0


@pytest.mark.parametrize(
    "query,root_type,reason",
    [
        ({"term": {"num": "abc"}}, "query_shard_exception", 'For input string: "abc"'),
        ({"term": {"dt": "garbage"}}, "parse_exception", "failed to parse date field [garbage]"),
        ({"term": {"flag": "yes"}}, "query_shard_exception", "Can't parse boolean value [yes]"),
        ({"term": {"ip": "nope"}}, "query_shard_exception", "'nope' is not an IP string literal."),
        ({"range": {"num": {"gte": "abc"}}}, "query_shard_exception", 'For input string: "abc"'),
    ],
)
def test_term_wrong_type_errors(client, tl, query, root_type, reason):
    _shard_error(_raw(client, tl, {"query": query}), root_type, reason)


def test_term_unknown_parameter_is_parsing_exception(client, tl):
    r = _raw(client, tl, {"query": {"term": {"kw": {"value": "a", "bogus": 1}}}})
    _parse_error(r, "[term] query does not support [bogus]")


def test_term_case_insensitive_non_boolean_rejected(client, tl):
    r = _raw(client, tl, {"query": {"term": {"kw": {"value": "a", "case_insensitive": "maybe"}}}})
    _parse_error(r, "Failed to parse value [maybe]", etype="illegal_argument_exception")


def test_unknown_query_type(client, tl):
    r = _raw(client, tl, {"query": {"bogus": {}}})
    err = _parse_error(r, "unknown query [bogus]")
    assert err["caused_by"]["type"] == "named_object_not_found_exception"


def test_pinned_query_not_available(client, tl):
    r = _raw(client, tl, {"query": {"pinned": {"ids": ["1"], "organic": {"match_all": {}}}}})
    _parse_error(r, "unknown query [pinned]")


# --------------------------------------------------------------------------
# terms
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "field,values,expected",
    [
        ("kw", ["apple", "banana"], {"2", "3"}),
        ("kw", ["apple", "nope"], {"2"}),
        ("kw", [], set()),
        ("kw", ["apple", "apple"], {"2"}),
        ("num", [1, "2", 3.0], {"1", "2", "3"}),
        ("num", [2.5, 10], {"4"}),
        ("num", [7], {"6"}),
        ("flag", [True, "false"], {"1", "2", "3"}),
        ("ip", ["10.0.0.0/8", "2001:db8::1"], {"3", "4"}),
        ("tags", ["c", "a"], {"1", "2", "4"}),
        ("dt", ["2000-01-01", "2100-01-01"], {"1", "6"}),
        ("missing_field", ["x"], set()),
    ],
)
def test_terms_values(client, tl, field, values, expected):
    assert _q(client, tl, {"terms": {field: values}}) == expected


def test_terms_mixed_types_on_keyword(client, index_factory, bulk_index):
    name = index_factory(
        mappings={"properties": {"k": {"type": "keyword"}}},
        settings={"number_of_shards": 1, "number_of_replicas": 0},
    )
    bulk_index(name, [{"k": "3"}, {"k": "true"}, {"k": "x"}])
    res = _s(client, name, {"terms": {"k": [3, True, "y"]}})
    assert sorted(h["_source"]["k"] for h in res["hits"]["hits"]) == ["3", "true"]


def test_terms_requires_array(client, tl):
    _parse_error(_raw(client, tl, {"query": {"terms": {"kw": "apple"}}}),
                 "[terms] query does not support [kw]")


def test_terms_case_insensitive_unsupported(client, tl):
    r = _raw(client, tl, {"query": {"terms": {"kw": ["a"], "case_insensitive": True}}})
    _parse_error(r, "[terms] query does not support [case_insensitive]")


def test_terms_boost_with_constant_score_semantics(client, tl):
    res = _s(client, tl, {"terms": {"kw": ["apple", "banana"], "boost": 2.5}})
    assert _scores(res) == {"2": 2.5, "3": 2.5}


def test_terms_score_is_constant_one(client, tl):
    res = _s(client, tl, {"terms": {"tags": ["a", "b"]}})
    assert set(_scores(res).values()) == {1.0}


def test_terms_max_terms_count(client):
    name = _create(client, {"properties": {"k": {"type": "keyword"}}},
                   [("1", {"k": "a"})], settings={"index.max_terms_count": 2})
    try:
        assert _q(client, name, {"terms": {"k": ["a", "b"]}}) == {"1"}
        r = _raw(client, name, {"query": {"terms": {"k": ["a", "b", "c"]}}})
        err = _shard_error(r, "query_shard_exception")
        assert "The number of terms [3] used in the Terms Query request has exceeded the allowed maximum of [2]" in err["root_cause"][0]["reason"]
    finally:
        _drop(client, name)


@pytest.fixture(scope="module")
def lookup(client):
    name = _create(
        client,
        {"properties": {"vals": {"type": "keyword"},
                        "nest": {"properties": {"inner": {"type": "keyword"}}},
                        "nums": {"type": "integer"}}},
        [
            ("L1", {"vals": ["apple", "banana"], "nest": {"inner": ["Cherry pie"]},
                    "nums": [1, 10]}),
            ("L2", {"vals": [], "nest": [{"inner": "apple"}, {"inner": "Apple"}]}),
            ("L3", {"vals": "banana"}, "r1"),
        ],
    )
    yield name
    _drop(client, name)


@pytest.mark.parametrize(
    "field,doc_id,path,expected",
    [
        ("kw", "L1", "vals", {"2", "3"}),
        ("kw", "L1", "nest.inner", {"4"}),
        ("num", "L1", "nums", {"1", "4"}),
        ("kw", "L2", "vals", set()),
        ("kw", "L2", "nest.inner", {"1", "2"}),
        ("kw", "L1", "nope", set()),
        ("kw", "missing-doc", "vals", set()),
    ],
)
def test_terms_lookup(client, tl, lookup, field, doc_id, path, expected):
    q = {"terms": {field: {"index": lookup, "id": doc_id, "path": path}}}
    assert _q(client, tl, q) == expected


def test_terms_lookup_with_routing(client, tl, lookup):
    q = {"terms": {"kw": {"index": lookup, "id": "L3", "path": "vals", "routing": "r1"}}}
    assert _q(client, tl, q) == {"3"}


def test_terms_lookup_missing_index_errors(client, tl):
    q = {"terms": {"kw": {"index": _uname("nope"), "id": "x", "path": "vals"}}}
    r = _raw(client, tl, {"query": q})
    assert r.status_code == 404
    assert r.json()["error"]["type"] == "index_not_found_exception"


# --------------------------------------------------------------------------
# terms_set
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "terms,expected",
    [
        (["a"], {"4"}),            # doc4 req=1 has a; doc1 req=2 needs 2
        (["a", "b"], {"1", "2", "4"}),
        (["b", "c"], {"2"}),       # doc1 needs 2 matches but only has b
        (["a", "b", "c"], {"1", "2", "4"}),
        ([], set()),
    ],
)
def test_terms_set_field(client, tl, terms, expected):
    q = {"terms_set": {"tags": {"terms": terms, "minimum_should_match_field": "req"}}}
    assert _q(client, tl, q) == expected


@pytest.mark.parametrize(
    "terms,source,expected",
    [
        (["a", "b", "c"], "params.num_terms", set()),
        (["a", "b"], "params.num_terms", {"1"}),
        (["a", "b", "c"], "Math.min(params.num_terms, 1)", {"1", "2", "4"}),
        (["b"], "1", {"1", "2"}),
    ],
)
def test_terms_set_script(client, tl, terms, source, expected):
    q = {"terms_set": {"tags": {"terms": terms,
                                "minimum_should_match_script": {"source": source}}}}
    assert _q(client, tl, q) == expected


# --------------------------------------------------------------------------
# range
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"gt": 1}, {"2", "3", "4", "6", "7"}),
        ({"gte": 1}, {"1", "2", "3", "4", "6", "7"}),
        ({"lt": 3}, {"1", "2", "8"}),
        ({"lte": 3}, {"1", "2", "3", "8"}),
        ({"gt": 1, "lt": 3}, {"2"}),
        ({"gte": 1, "lte": 3}, {"1", "2", "3"}),
        ({"gt": 3, "lt": 3}, set()),
        ({"gte": 3, "lte": 3}, {"3"}),
        ({"gt": 10, "lt": 1}, set()),
        ({"gte": 8, "lte": 9}, set()),
        ({"gte": 15, "lte": 25}, {"6"}),  # multi-valued [7, 20]
        ({"gte": None, "lte": 1}, {"1", "8"}),
        ({"gte": 50, "lte": None}, {"7"}),
        ({"gte": "2", "lte": "3"}, {"2", "3"}),
        ({"gt": 1.5, "lt": 3.5}, {"2", "3"}),
        ({"gte": 1.5}, {"2", "3", "4", "6", "7"}),
        ({"lte": 2.9}, {"1", "2", "8"}),
        ({}, {"1", "2", "3", "4", "6", "7", "8"}),
    ],
)
def test_range_integer(client, tl, spec, expected):
    assert _q(client, tl, {"range": {"num": spec}}) == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"from": 1, "to": 3}, {"1", "2", "3"}),
        ({"from": 1, "to": 3, "include_lower": False}, {"2", "3"}),
        ({"from": 1, "to": 3, "include_upper": False}, {"1", "2"}),
        ({"from": None, "to": 1}, {"1", "8"}),
    ],
)
def test_range_legacy_from_to(client, tl, spec, expected):
    assert _q(client, tl, {"range": {"num": spec}}) == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"gte": -1, "lte": 0}, {"3", "8"}),
        ({"gt": 0}, {"1", "2"}),
        ({"gte": 1.5, "lt": 2.5}, {"1"}),
    ],
)
def test_range_double(client, tl, spec, expected):
    assert _q(client, tl, {"range": {"dbl": spec}}) == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"gte": "a", "lt": "b"}, {"2", "7"}),          # lexicographic, lowercase
        ({"gte": "A", "lt": "Z"}, {"1", "4"}),          # uppercase sorts first
        ({"lt": "A"}, {"5"}),                           # empty string
        ({"gt": "apple"}, {"3", "8"}),          # "a*b?c\\d" < "apple" ("*" < "p")
        ({"gte": "apple", "lte": "apple"}, {"2"}),
        ({"gt": "x"}, {"8"}),
    ],
)
def test_range_keyword_lexicographic(client, tl, spec, expected):
    assert _q(client, tl, {"range": {"kw": spec}}) == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"gte": "192.168.0.0", "lte": "192.168.255.255"}, {"1", "2"}),
        ({"lt": "11.0.0.0"}, {"3"}),
        ({"gt": "192.168.0.1"}, {"2", "4"}),   # ipv6 sorts after ipv4
    ],
)
def test_range_ip(client, tl, spec, expected):
    assert _q(client, tl, {"range": {"ip": spec}}) == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"gte": "2000-01-01", "lt": "2000-01-03"}, {"1", "2"}),
        ({"gte": "2000-01-01", "lte": "2000-01-03"}, {"1", "2", "3"}),
        # rounding: gt rounds up to end of day, gte rounds down
        ({"gt": "2000-01-02||/d"}, {"3", "6"}),
        ({"gte": "2000-01-02||/d"}, {"2", "3", "6"}),
        # lt rounds down to start of day, lte rounds up to end of day
        ({"lt": "2000-01-02||/d"}, {"1", "4"}),
        ({"lte": "2000-01-02||/d"}, {"1", "2", "4"}),
        ({"gte": "2000-01-01||-1d", "lt": "2000-01-01"}, {"4"}),
        ({"gte": "2000-01-01||+1M/M"}, {"6"}),
        ({"gte": "2000||/y", "lte": "2000||/y"}, {"1", "2", "3"}),
        ({"gt": "now"}, {"6"}),
        ({"lt": "now-1d/d"}, {"1", "2", "3", "4"}),
        ({"gte": "now-1000y", "lte": "now+1000y"}, {"1", "2", "3", "4", "6"}),
        ({"gte": "now+1000y"}, set()),
        ({"gte": 946684800000, "lt": 946771200000}, {"1"}),
    ],
)
def test_range_dates(client, tl, spec, expected):
    assert _q(client, tl, {"range": {"dt": spec}}) == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"gte": "01/01/2000", "lte": "02/01/2000", "format": "dd/MM/yyyy"}, {"1", "2"}),
        ({"gte": "2000", "lt": "2001", "format": "yyyy"}, {"1", "2", "3"}),
        ({"gte": "01/01/2000||/M", "format": "dd/MM/yyyy", "lt": "2001-01-01"},
         None),
    ],
)
def test_range_date_format(client, tl, spec, expected):
    if expected is None:
        # explicit format applies to *both* bounds: ISO upper bound fails to parse
        r = _raw(client, tl, {"query": {"range": {"dt": spec}}})
        _shard_error(r, "parse_exception")
        return
    assert _q(client, tl, {"range": {"dt": spec}}) == expected


@pytest.mark.parametrize(
    "tz,expected",
    [
        ("+00:00", {"2"}),
        ("+12:00", set()),       # [2000-01-01T12:00Z, 2000-01-02T12:00Z)
        ("-12:00", {"2", "3"}),  # [2000-01-02T12:00Z, 2000-01-03T12:00Z)
        ("+01:00", {"2"}),       # [2000-01-01T23:00Z, 2000-01-02T23:00Z)
        ("-01:00", {"2", "3"}),  # [2000-01-02T01:00Z, 2000-01-03T01:00Z)
        ("Europe/Kiev", {"2"}),
    ],
)
def test_range_time_zone(client, tl, tz, expected):
    q = {"range": {"dt": {"gte": "2000-01-02T00:00:00", "lt": "2000-01-03T00:00:00",
                          "time_zone": tz}}}
    assert _q(client, tl, q) == expected


def test_range_time_zone_does_not_apply_to_explicit_offset(client, tl):
    q = {"range": {"dt": {"gte": "2000-01-02T00:00:00Z", "lt": "2000-01-03T00:00:00Z",
                          "time_zone": "-12:00"}}}
    assert _q(client, tl, q) == {"2"}


def test_range_time_zone_with_rounding(client, tl):
    # /d rounding happens in the given timezone: day = [2000-01-01T23:00Z, 2000-01-02T23:00Z)
    q = {"range": {"dt": {"gte": "2000-01-02||/d", "lte": "2000-01-02||/d",
                          "time_zone": "+01:00"}}}
    assert _q(client, tl, q) == {"2"}


@pytest.mark.parametrize(
    "relation,spec,expected",
    [
        ("INTERSECTS", {"gte": 4, "lte": 10}, {"1", "2", "3", "4"}),
        ("intersects", {"gte": 21, "lte": 30}, {"4"}),
        ("CONTAINS", {"gte": 11, "lte": 12}, {"2", "3", "4"}),
        ("CONTAINS", {"gte": 2, "lte": 4}, {"1"}),
        ("WITHIN", {"gte": 0, "lte": 12}, {"1", "3"}),
        ("WITHIN", {"gte": 1, "lte": 5}, {"1"}),
        ("WITHIN", {"gte": 0, "lte": 1000}, {"1", "2", "3"}),
        ("INTERSECTS", {"gt": 5, "lt": 10}, {"3", "4"}),
    ],
)
def test_range_on_range_field_relations(client, tl, relation, spec, expected):
    q = {"range": {"rng": dict(spec, relation=relation)}}
    assert _q(client, tl, q) == expected


def test_range_on_range_field_default_is_intersects(client, tl):
    assert _q(client, tl, {"range": {"rng": {"gte": 5, "lte": 5}}}) == {"1", "3", "4"}


def test_term_on_range_field_contains_value(client, tl):
    assert _q(client, tl, {"term": {"rng": 11}}) == {"2", "3", "4"}


@pytest.mark.parametrize(
    "relation,spec,expected",
    [
        ("INTERSECTS", {"gte": "2000-01-31", "lte": "2000-02-01"}, {"1", "2"}),
        ("WITHIN", {"gte": "2000-01-01", "lte": "2000-01-31||/d"}, {"1"}),
        ("CONTAINS", {"gte": "2000-02-10", "lte": "2000-02-11"}, {"2"}),
        ("WITHIN", {"gte": "2000-01||/M", "lte": "2000-02||/M"}, {"1", "2"}),
    ],
)
def test_range_on_date_range_field(client, tl, relation, spec, expected):
    q = {"range": {"drng": dict(spec, relation=relation)}}
    assert _q(client, tl, q) == expected


def test_range_unmapped_field_no_hits(client, tl):
    assert _q(client, tl, {"range": {"nope": {"gte": 1}}}) == set()


def test_range_on_text_field_uses_terms(client, tl):
    # text range is lexicographic over indexed (lowercased) terms
    assert _q(client, tl, {"range": {"txt": {"gte": "lazy", "lte": "lazy"}}}) == {"2"}


def test_range_score_is_constant(client, tl):
    res = _s(client, tl, {"range": {"num": {"gte": 1, "lte": 3, "boost": 4}}})
    assert set(_scores(res).values()) == {4.0}


# --------------------------------------------------------------------------
# exists / ids
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "field,expected",
    [
        ("kw", {"1", "2", "3", "4", "5", "7", "8"}),  # empty string counts
        ("num", {"1", "2", "3", "4", "6", "7", "8"}),  # null does not
        ("tags", {"1", "2", "4"}),                      # [], null, [null] do not
        ("flag", {"1", "2", "3"}),
        ("obj", {"1", "2"}),                            # {} and null do not
        ("obj.b", {"1"}),
        ("txt", {"1", "2", "3", "5"}),                  # "" text still exists
        ("rng", {"1", "2", "3", "4"}),
        ("nope", set()),
        ("_id", {"1", "2", "3", "4", "5", "6", "7", "8"}),
        ("o*", {"1", "2"}),
    ],
)
def test_exists(client, tl, field, expected):
    assert _q(client, tl, {"exists": {"field": field}}) == expected


def test_not_exists_via_must_not(client, tl):
    q = {"bool": {"must_not": {"exists": {"field": "tags"}}}}
    assert _q(client, tl, q) == {"3", "5", "6", "7", "8"}


@pytest.mark.parametrize(
    "values,expected",
    [
        (["1", "3"], {"1", "3"}),
        (["1", "nope"], {"1"}),
        ([], set()),
        ("2", {"2"}),
        ([1, 2], {"1", "2"}),
    ],
)
def test_ids(client, tl, values, expected):
    assert _q(client, tl, {"ids": {"values": values}}) == expected


def test_ids_score_and_boost(client, tl):
    res = _s(client, tl, {"ids": {"values": ["1", "2"], "boost": 2}})
    assert _scores(res) == {"1": 2.0, "2": 2.0}


def test_term_on_id_field(client, tl):
    assert _q(client, tl, {"terms": {"_id": ["4", "8"]}}) == {"4", "8"}


# --------------------------------------------------------------------------
# prefix / wildcard / regexp / fuzzy
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"kw": "app"}, {"2"}),
        ({"kw": "App"}, {"1"}),
        ({"kw": {"value": "APP", "case_insensitive": True}}, {"1", "2"}),
        ({"kw": ""}, {"1", "2", "3", "4", "5", "7", "8"}),
        ({"kw": "Cherry p"}, {"4"}),
        ({"txt": "qu"}, {"1", "3"}),
        ({"txt": "Qu"}, set()),
        ({"tags": "c"}, {"2"}),
        ({"nope": "a"}, set()),
        ({"kw": {"value": "ban", "boost": 2}}, {"3"}),
    ],
)
def test_prefix(client, tl, spec, expected):
    assert _q(client, tl, {"prefix": spec}) == expected


@pytest.mark.parametrize("qt", ["prefix", "wildcard", "regexp", "fuzzy"])
@pytest.mark.parametrize("field,ftype", [("num", "integer"), ("dt", "date")])
def test_multiterm_on_non_string_field_errors(client, tl, qt, field, ftype):
    value = "1*" if qt == "wildcard" else "1"
    r = _raw(client, tl, {"query": {qt: {field: value}}})
    err = _shard_error(r, "query_shard_exception")
    assert (f"Can only use {qt} queries on keyword and text fields - not on [{field}] "
            f"which is of type [{ftype}]") in err["root_cause"][0]["reason"]


def test_prefix_score_is_constant(client, tl):
    res = _s(client, tl, {"prefix": {"kw": {"value": "a", "boost": 3}}})
    assert set(_scores(res).values()) == {3.0}


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"kw": "*"}, {"1", "2", "3", "4", "5", "7", "8"}),
        ({"kw": "?"}, set()),
        ({"kw": "*pple"}, {"1", "2"}),
        ({"kw": "?pple"}, {"1", "2"}),
        ({"kw": "a?ple"}, {"2"}),
        ({"kw": "b*a"}, {"3"}),
        ({"kw": "*e*"}, {"1", "2", "4"}),
        ({"kw": {"value": "A*", "case_insensitive": True}}, {"1", "2", "7"}),
        ({"kw": {"value": "a*"}}, {"2", "7"}),
        ({"kw": "a\\*b\\?c*"}, {"7"}),
        ({"kw": "a\\*b?c\\\\d"}, {"7"}),
        ({"kw": "a\\*x*"}, set()),
        ({"kw": "x.y"}, {"8"}),
        ({"kw": "x?y"}, {"8"}),
        ({"txt": "qu*k"}, {"1", "3"}),
        ({"kw": {"wildcard": "ban*"}}, {"3"}),
        ({"nope": "*"}, set()),
    ],
)
def test_wildcard(client, tl, spec, expected):
    assert _q(client, tl, {"wildcard": spec}) == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"kw": "a.*"}, {"2", "7"}),
        ({"kw": "apple"}, {"2"}),
        ({"kw": "app"}, set()),                       # anchored
        ({"kw": "[aA]pple"}, {"1", "2"}),
        ({"kw": "x\\.y"}, {"8"}),
        ({"kw": "x.y"}, {"8"}),
        ({"kw": ".{6}"}, {"3"}),
        ({"kw": "b(an)+a"}, {"3"}),
        ({"kw": "apple|banana"}, {"2", "3"}),
        ({"kw": {"value": "APPLE", "case_insensitive": True}}, {"1", "2"}),
        ({"kw": {"value": "~(apple)", "flags": "COMPLEMENT"}},
         {"1", "3", "4", "5", "7", "8"}),
        ({"kw": {"value": "~(apple)", "flags": "NONE"}}, set()),
        ({"kw": {"value": "~(apple)", "flags": "ALL"}}, set()),   # ALL excludes complement
        ({"kw": {"value": "~(apple)"}}, set()),
        ({"kw": {"value": "a@", "flags": "ALL"}}, {"2", "7"}),     # @ = any string
        ({"kw": {"value": "a.*", "flags": "NONE"}}, {"2", "7"}),
        ({"kw": {"value": "x<1-9>", "flags": "INTERVAL"}}, set()),
        ({"kw": {"value": "b.*&.*a", "flags": "INTERSECTION"}}, {"3"}),
        ({"kw": ""}, {"5"}),
        ({"txt": "qu.*"}, {"1", "3"}),
        ({"nope": ".*"}, set()),
    ],
)
def test_regexp(client, tl, spec, expected):
    assert _q(client, tl, {"regexp": spec}) == expected


def test_regexp_interval_flag(client):
    name = _create(client, {"properties": {"k": {"type": "keyword"}}},
                   [("1", {"k": "v5"}), ("2", {"k": "v12"}), ("3", {"k": "v42"})])
    try:
        q = {"regexp": {"k": {"value": "v<1-20>", "flags": "INTERVAL"}}}
        assert _q(client, name, q) == {"1", "2"}
        q = {"regexp": {"k": {"value": "v<1-20>", "flags": "NONE"}}}
        assert _q(client, name, q) == set()
    finally:
        _drop(client, name)


def test_regexp_tilde_is_literal_without_complement_flag(client):
    name = _create(client, {"properties": {"k": {"type": "keyword"}}},
                   [("1", {"k": "apple"}), ("2", {"k": "~apple"}), ("3", {"k": "a@"})])
    try:
        assert _q(client, name, {"regexp": {"k": "~(apple)"}}) == {"2"}
        assert _q(client, name, {"regexp": {"k": {"value": "~(apple)", "flags": "ALL"}}}) == {"2"}
        q = {"regexp": {"k": {"value": "~(apple)", "flags": "COMPLEMENT"}}}
        assert _q(client, name, q) == {"2", "3"}
        assert _q(client, name, {"regexp": {"k": {"value": "a@", "flags": "NONE"}}}) == {"3"}
        assert _q(client, name, {"regexp": {"k": {"value": "a@", "flags": "ANYSTRING"}}}) == {"1", "3"}
    finally:
        _drop(client, name)


def test_regexp_invalid_syntax(client, tl):
    r = _raw(client, tl, {"query": {"regexp": {"kw": "a["}}})
    _shard_error(r, "query_shard_exception", "unexpected end-of-string")


def test_regexp_max_determinized_states(client, tl):
    q = {"regexp": {"kw": {"value": "[a-z]{1,30}[a-z]{1,30}[a-z]{1,30}",
                           "max_determinized_states": 10}}}
    err = _shard_error(_raw(client, tl, {"query": q}), "query_shard_exception")
    assert "would require more than 10 effort" in err["root_cause"][0]["reason"]


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"kw": "aple"}, {"2"}),                          # deletion, AUTO
        ({"kw": "appel"}, {"2"}),                         # transposition = 1 edit
        ({"kw": {"value": "appel", "transpositions": False}}, set()),
        ({"kw": {"value": "appel", "transpositions": False, "fuzziness": 2}}, {"2"}),
        ({"kw": {"value": "apple", "fuzziness": 0}}, {"2"}),
        ({"kw": {"value": "apple", "fuzziness": 1}}, {"1", "2"}),
        ({"kw": {"value": "bpple", "fuzziness": 1}}, {"1", "2"}),
        ({"kw": {"value": "bpple", "fuzziness": 1, "prefix_length": 1}}, set()),
        ({"kw": {"value": "appXe", "fuzziness": 1, "prefix_length": 3}}, {"2"}),
        ({"kw": {"value": "appXe", "fuzziness": 1, "prefix_length": 4}}, set()),
        ({"kw": {"value": "xy", "fuzziness": "AUTO"}}, set()),        # len<=2: exact
        ({"kw": {"value": "x.z", "fuzziness": "AUTO"}}, {"8"}),       # len 3..5: 1 edit
        ({"kw": {"value": "bnanaa", "fuzziness": "AUTO"}}, {"3"}),    # len>5: 2 edits
        ({"kw": {"value": "bnanaa", "fuzziness": "AUTO:7,10"}}, set()),
        ({"kw": {"value": "APPLE", "fuzziness": 1}}, set()),
        ({"txt": {"value": "quack"}}, {"1", "3"}),
        ({"nope": "x"}, set()),
    ],
)
def test_fuzzy(client, tl, spec, expected):
    assert _q(client, tl, {"fuzzy": spec}) == expected


def test_fuzzy_max_expansions(client):
    name = _create(client, {"properties": {"k": {"type": "keyword"}}},
                   [("1", {"k": "abcd"}), ("2", {"k": "abce"}), ("3", {"k": "abcf"})])
    try:
        assert _q(client, name, {"fuzzy": {"k": {"value": "abcz", "fuzziness": 1}}}) == {"1", "2", "3"}
        got = _q(client, name, {"fuzzy": {"k": {"value": "abcz", "fuzziness": 1,
                                                 "max_expansions": 1}}})
        assert len(got) == 1
    finally:
        _drop(client, name)


def test_fuzzy_exact_match_scores_higher(client, tl):
    res = _s(client, tl, {"fuzzy": {"kw": {"value": "apple", "fuzziness": 1}}})
    assert _order(res)[0] == "2"


# --------------------------------------------------------------------------
# compound queries dataset
# --------------------------------------------------------------------------

CQ_MAPPING = {
    "properties": {
        "color": {"type": "keyword"},
        "size": {"type": "keyword"},
        "n": {"type": "integer"},
        "pop": {"type": "integer"},
        "f": {"type": "float"},
        "title": {"type": "text"},
        "when": {"type": "date"},
        "loc": {"type": "geo_point"},
    }
}

CQ_DOCS = [
    ("1", {"color": "red", "size": "s", "n": 1, "pop": 10, "f": 0.5, "title": "red fox",
           "when": "2000-01-01", "loc": {"lat": 0.0, "lon": 0.0}}),
    ("2", {"color": "red", "size": "m", "n": 2, "pop": 0, "f": 2.0, "title": "red dog",
           "when": "2000-01-11", "loc": {"lat": 0.0, "lon": 1.0}}),
    ("3", {"color": "blue", "size": "m", "n": 3, "title": "blue fox",
           "when": "2000-01-21", "loc": {"lat": 0.0, "lon": 2.0}}),
    ("4", {"color": "blue", "size": "l", "n": 4, "pop": 100, "f": 4.0, "title": "blue whale",
           "when": "2000-01-31"}),
    ("5", {"color": "green", "n": 5, "pop": 1, "f": 8.0, "title": "green frog"}),
]


@pytest.fixture(scope="module")
def cq(client):
    name = _create(client, CQ_MAPPING, CQ_DOCS)
    yield name
    _drop(client, name)


def _cs(query, boost=1.0):
    return {"constant_score": {"filter": query, "boost": boost}}


def _t(field, value):
    return {"term": {field: value}}


ALL5 = {"1", "2", "3", "4", "5"}


# --------------------------------------------------------------------------
# bool
# --------------------------------------------------------------------------


def test_bool_empty_matches_all(client, cq):
    res = _s(client, cq, {"bool": {}})
    assert _ids(res) == ALL5
    assert set(_scores(res).values()) == {1.0}


def test_bool_must_and_should_adds_scores_should_optional(client, cq):
    q = {"bool": {"must": [_cs(_t("color", "red"), 1)],
                  "should": [_cs(_t("size", "m"), 2)]}}
    assert _scores(_s(client, cq, q)) == {"1": 1.0, "2": 3.0}


def test_bool_only_should_requires_one(client, cq):
    q = {"bool": {"should": [_cs(_t("color", "red"), 1), _cs(_t("size", "m"), 2)]}}
    assert _scores(_s(client, cq, q)) == {"1": 1.0, "2": 3.0, "3": 2.0}


def test_bool_filter_contributes_zero_score(client, cq):
    res = _s(client, cq, {"bool": {"filter": [_t("color", "red")]}})
    assert _scores(res) == {"1": 0.0, "2": 0.0}


def test_bool_filter_plus_should_makes_should_optional(client, cq):
    q = {"bool": {"filter": [_t("color", "red")], "should": [_cs(_t("size", "m"), 2)]}}
    assert _scores(_s(client, cq, q)) == {"1": 0.0, "2": 2.0}


def test_bool_filter_plus_should_with_msm_1(client, cq):
    q = {"bool": {"filter": [_t("color", "red")], "should": [_cs(_t("size", "m"), 2)],
                  "minimum_should_match": 1}}
    assert _scores(_s(client, cq, q)) == {"2": 2.0}


def test_bool_must_not_only_matches_rest_with_zero_score(client, cq):
    res = _s(client, cq, {"bool": {"must_not": [_t("color", "red")]}})
    assert _ids(res) == {"3", "4", "5"}
    assert set(_scores(res).values()) == {0.0}


def test_bool_must_not_with_filter(client, cq):
    q = {"bool": {"filter": {"range": {"n": {"gte": 2}}}, "must_not": {"term": {"color": "blue"}}}}
    assert _q(client, cq, q) == {"2", "5"}


def test_bool_adjust_pure_negative_false_matches_nothing(client, cq):
    q = {"bool": {"must_not": [_t("color", "red")], "adjust_pure_negative": False}}
    assert _q(client, cq, q) == set()


def test_bool_must_not_missing_field_matches_all(client, cq):
    assert _q(client, cq, {"bool": {"must_not": {"term": {"nope": "x"}}}}) == ALL5


def test_bool_must_not_wins_over_must(client, cq):
    q = {"bool": {"must": _t("color", "red"), "must_not": _t("color", "red")}}
    assert _q(client, cq, q) == set()


def test_bool_clause_object_equals_array(client, cq):
    a = _q(client, cq, {"bool": {"must": _t("color", "red")}})
    b = _q(client, cq, {"bool": {"must": [_t("color", "red")]}})
    assert a == b == {"1", "2"}


def test_bool_empty_clause_arrays_match_all(client, cq):
    q = {"bool": {"must": [], "should": [], "filter": [], "must_not": []}}
    assert _q(client, cq, q) == ALL5


# should-clause match counts per doc: 1 -> 2, 2 -> 3, 3 -> 3, 4 -> 3, 5 -> 1
MSM_SHOULD = [
    {"bool": {"should": [_t("color", "red"), _t("color", "blue")]}},
    {"bool": {"should": [_t("size", "m"), {"range": {"pop": {"gte": 100}}}]}},
    {"bool": {"should": [{"range": {"n": {"lte": 2}}}, {"range": {"n": {"gte": 3}}}]}},
]


@pytest.mark.parametrize(
    "msm,expected",
    [
        (1, ALL5),
        (2, {"1", "2", "3", "4"}),
        (3, {"2", "3", "4"}),
        (4, set()),
        (0, ALL5),
        ("2", {"1", "2", "3", "4"}),
        (-1, {"1", "2", "3", "4"}),       # 3 - 1 = 2
        (-2, ALL5),                       # 3 - 2 = 1
        ("66%", ALL5),                    # floor(1.98) = 1
        ("67%", {"1", "2", "3", "4"}),    # floor(2.01) = 2
        ("100%", {"2", "3", "4"}),
        ("-34%", {"1", "2", "3", "4"}),   # 3 - floor(1.02) = 2
        ("-100%", ALL5),
        ("2<-1", {"1", "2", "3", "4"}),   # 3 clauses > 2 -> 3 - 1 = 2
        ("3<-1", {"2", "3", "4"}),        # 3 clauses not > 3 -> all required
        ("1<-1 2<50%", ALL5),             # > 2 -> floor(1.5) = 1
    ],
)
def test_bool_minimum_should_match(client, cq, msm, expected):
    q = {"bool": {"should": MSM_SHOULD, "minimum_should_match": msm}}
    assert _q(client, cq, q) == expected


def test_bool_msm_with_must_makes_should_required(client, cq):
    q = {"bool": {"must": _t("color", "red"), "should": [_t("size", "m"), _t("size", "xl")],
                  "minimum_should_match": 1}}
    assert _q(client, cq, q) == {"2"}


def test_bool_nested_bools(client, cq):
    q = {"bool": {
        "must": [{"bool": {"should": [_t("color", "red"), _t("color", "green")]}}],
        "must_not": [{"bool": {"filter": [_t("size", "s")]}}],
    }}
    assert _q(client, cq, q) == {"2", "5"}


def test_bool_boost_multiplies(client, cq):
    q = {"bool": {"must": [_cs(_t("color", "red"), 2)], "boost": 3}}
    assert _scores(_s(client, cq, q)) == {"1": 6.0, "2": 6.0}


def test_bool_unknown_clause_errors(client, cq):
    r = _raw(client, cq, {"query": {"bool": {"musts": [_t("color", "red")]}}})
    _parse_error(r, "[bool] unknown field [musts] did you mean any of [must, mustNot]?",
                 etype="x_content_parse_exception")


# --------------------------------------------------------------------------
# constant_score / boosting / dis_max
# --------------------------------------------------------------------------


@pytest.mark.parametrize("boost,expected", [(None, 1.0), (2.5, 2.5), (0, 0.0)])
def test_constant_score(client, cq, boost, expected):
    q = {"constant_score": {"filter": {"match": {"title": "fox"}}}}
    if boost is not None:
        q["constant_score"]["boost"] = boost
    res = _s(client, cq, q)
    assert _ids(res) == {"1", "3"}
    assert set(_scores(res).values()) == {expected}


def test_constant_score_requires_filter(client, cq):
    r = _raw(client, cq, {"query": {"constant_score": {"boost": 1}}})
    _parse_error(r, "[constant_score] requires a 'filter' element")


@pytest.mark.parametrize("match_q", [{"match_all": {}}, {"match_all": {"boost": 1.5}}])
def test_match_all_boost(client, cq, match_q):
    res = _s(client, cq, match_q)
    expected = match_q["match_all"].get("boost", 1.0)
    assert _ids(res) == ALL5
    assert set(_scores(res).values()) == {expected}


def test_match_none(client, cq):
    res = _s(client, cq, {"match_none": {"boost": 3}})
    assert res["hits"]["total"]["value"] == 0
    assert res["hits"]["max_score"] is None


def test_boosting_reorders(client, cq):
    q = {"boosting": {
        "positive": _cs({"match": {"title": "fox"}}, 2.0),
        "negative": _t("color", "red"),
        "negative_boost": 0.25,
    }}
    res = _s(client, cq, q)
    assert _order(res) == ["3", "1"]
    assert _scores(res) == {"3": 2.0, "1": 0.5}


def test_boosting_requires_negative_boost(client, cq):
    q = {"boosting": {"positive": {"match_all": {}}, "negative": _t("color", "red")}}
    r = _raw(client, cq, {"query": q})
    _parse_error(r, "[boosting] query requires 'negative_boost' to be set to be a positive value")


@pytest.mark.parametrize(
    "tie,expected",
    [
        (None, {"1": 2.0, "2": 2.0, "3": 1.0}),
        (0.0, {"1": 2.0, "2": 2.0, "3": 1.0}),
        (0.5, {"1": 2.0, "2": 2.5, "3": 1.0}),
        (1.0, {"1": 2.0, "2": 3.0, "3": 1.0}),
    ],
)
def test_dis_max_tie_breaker(client, cq, tie, expected):
    q = {"dis_max": {"queries": [_cs(_t("color", "red"), 2), _cs(_t("size", "m"), 1)]}}
    if tie is not None:
        q["dis_max"]["tie_breaker"] = tie
    assert _scores(_s(client, cq, q)) == expected


def test_dis_max_empty_queries_rejected(client, cq):
    r = _raw(client, cq, {"query": {"dis_max": {"queries": []}}})
    _parse_error(r, "query malformed, must start with start_object")


# --------------------------------------------------------------------------
# function_score
# --------------------------------------------------------------------------


def _fs(functions, query=None, **kw):
    body = {"query": query or _cs(_t("color", "red"), 2.0), "functions": functions}
    body.update(kw)
    return {"function_score": body}


W3 = {"weight": 3}
W4 = {"weight": 4}


@pytest.mark.parametrize(
    "score_mode,expected",
    [
        ("multiply", 12.0),
        ("sum", 7.0),
        ("max", 4.0),
        ("min", 3.0),
        ("first", 3.0),
        ("avg", 1.0),  # weighted avg of raw function values: (3*1 + 4*1) / (3 + 4)
    ],
)
def test_function_score_score_mode(client, cq, score_mode, expected):
    q = _fs([W3, W4], score_mode=score_mode, boost_mode="replace")
    s = _scores(_s(client, cq, q))
    assert s.keys() == {"1", "2"}
    assert s["1"] == pytest.approx(expected, rel=1e-6)


@pytest.mark.parametrize(
    "boost_mode,expected",
    [
        ("multiply", 6.0),
        ("replace", 3.0),
        ("sum", 5.0),
        ("avg", 2.5),
        ("max", 3.0),
        ("min", 2.0),
    ],
)
def test_function_score_boost_mode(client, cq, boost_mode, expected):
    q = _fs([W3], boost_mode=boost_mode)
    assert _scores(_s(client, cq, q)) == {"1": expected, "2": expected}


def test_function_score_filtered_functions(client, cq):
    q = _fs([{"filter": _t("size", "m"), "weight": 10}, {"weight": 2}],
            query={"match_all": {}}, score_mode="first", boost_mode="replace")
    assert _scores(_s(client, cq, q)) == {"1": 2.0, "2": 10.0, "3": 10.0, "4": 2.0, "5": 2.0}


def test_function_score_no_function_matches_uses_query_score(client, cq):
    q = _fs([{"filter": _t("size", "xl"), "weight": 10}], boost_mode="replace")
    # no function matched: function score defaults to 1, replace -> 1
    assert _scores(_s(client, cq, q)) == {"1": 1.0, "2": 1.0}


def test_function_score_max_boost(client, cq):
    q = _fs([{"weight": 10}], max_boost=4, boost_mode="replace")
    assert _scores(_s(client, cq, q)) == {"1": 4.0, "2": 4.0}


def test_function_score_min_score(client, cq):
    q = _fs([{"field_value_factor": {"field": "n"}}], query={"match_all": {}},
            boost_mode="replace", min_score=3)
    assert _scores(_s(client, cq, q)) == {"3": 3.0, "4": 4.0, "5": 5.0}


def test_function_score_top_level_weight_without_functions(client, cq):
    q = {"function_score": {"query": _cs(_t("color", "blue"), 1), "weight": 5,
                            "boost_mode": "replace"}}
    assert _scores(_s(client, cq, q)) == {"3": 5.0, "4": 5.0}


@pytest.mark.parametrize(
    "boost_mode,expected",
    [("replace", 3.0), ("multiply", 12.0), ("sum", 7.0)],
)
def test_function_score_outer_boost_applies_to_query_score(client, cq, boost_mode, expected):
    # boost multiplies the query score (2 -> 4) before combining with the function
    q = _fs([W3], boost_mode=boost_mode, boost=2)
    assert _scores(_s(client, cq, q)) == {"1": expected, "2": expected}


def test_function_score_avg_weights_are_pure_weights(client, cq):
    q = _fs([{"field_value_factor": {"field": "n"}, "weight": 2}, {"weight": 4}],
            score_mode="avg", boost_mode="replace")
    # doc1: (2*1 + 4*1) / 6 = 1 ; doc2: (2*2 + 4*1) / 6 = 4/3
    got = _scores(_s(client, cq, q))
    assert got["1"] == pytest.approx(1.0)
    assert got["2"] == pytest.approx(4 / 3, rel=1e-6)


@pytest.mark.parametrize(
    "fvf,expected",
    [
        ({"field": "n"}, {"1": 1.0, "2": 2.0, "3": 3.0, "4": 4.0, "5": 5.0}),
        ({"field": "n", "factor": 2}, {"1": 2.0, "2": 4.0, "3": 6.0, "4": 8.0, "5": 10.0}),
        ({"field": "n", "modifier": "square"},
         {"1": 1.0, "2": 4.0, "3": 9.0, "4": 16.0, "5": 25.0}),
        ({"field": "n", "modifier": "reciprocal"},
         {"1": 1.0, "2": 0.5, "3": 1 / 3, "4": 0.25, "5": 0.2}),
        ({"field": "n", "modifier": "sqrt"},
         {k: math.sqrt(v) for k, v in {"1": 1, "2": 2, "3": 3, "4": 4, "5": 5}.items()}),
        ({"field": "n", "modifier": "log1p"},
         {k: math.log10(1 + v) for k, v in {"1": 1, "2": 2, "3": 3, "4": 4, "5": 5}.items()}),
        ({"field": "n", "modifier": "log2p"},
         {k: math.log10(2 + v) for k, v in {"1": 1, "2": 2, "3": 3, "4": 4, "5": 5}.items()}),
        ({"field": "n", "modifier": "ln1p"},
         {k: math.log(1 + v) for k, v in {"1": 1, "2": 2, "3": 3, "4": 4, "5": 5}.items()}),
        ({"field": "n", "modifier": "ln2p"},
         {k: math.log(2 + v) for k, v in {"1": 1, "2": 2, "3": 3, "4": 4, "5": 5}.items()}),
        ({"field": "pop", "missing": 7},
         {"1": 10.0, "2": 0.0, "3": 7.0, "4": 100.0, "5": 1.0}),
        ({"field": "f", "factor": 0.5, "missing": 1},
         {"1": 0.25, "2": 1.0, "3": 0.5, "4": 2.0, "5": 4.0}),
    ],
)
def test_function_score_field_value_factor(client, cq, fvf, expected):
    q = _fs([{"field_value_factor": fvf}], query={"match_all": {}}, boost_mode="replace")
    got = _scores(_s(client, cq, q))
    assert got.keys() == expected.keys()
    for k, v in expected.items():
        assert got[k] == pytest.approx(v, rel=1e-5), k


def test_function_score_field_value_factor_missing_value_errors(client, cq):
    q = _fs([{"field_value_factor": {"field": "pop"}}], query={"match_all": {}})
    r = _raw(client, cq, {"query": q})
    # OpenSearch surfaces this as a generic 500 "exception"
    assert r.status_code == 500
    err = r.json()["error"]
    assert err["type"] == "search_phase_execution_exception"
    assert err["root_cause"][0] == {"type": "exception",
                                    "reason": "Missing value for field [pop]"}


def test_function_score_field_value_factor_log_of_zero_errors(client, cq):
    q = _fs([{"field_value_factor": {"field": "pop", "modifier": "log", "missing": 1}}],
            query={"match_all": {}})
    r = _raw(client, cq, {"query": q})
    err = _shard_error(r, "illegal_argument_exception")
    assert "consider using log1p or log2p" in err["root_cause"][0]["reason"]


def test_function_score_field_value_factor_unmapped_field_errors(client, cq):
    q = _fs([{"field_value_factor": {"field": "nope"}}], query={"match_all": {}})
    r = _raw(client, cq, {"query": q})
    err = _shard_error(r, "exception")
    assert err["root_cause"][0]["reason"] == (
        "Unable to find a field mapper for field [nope]. No 'missing' value defined.")


def test_random_score_seed_field_deterministic(client, cq):
    q = _fs([{"random_score": {"seed": 42, "field": "_seq_no"}}], query={"match_all": {}},
            boost_mode="replace")
    a = _s(client, cq, q)
    b = _s(client, cq, q)
    assert _scores(a) == _scores(b)
    assert all(0.0 <= v < 1.0 for v in _scores(a).values())
    q2 = _fs([{"random_score": {"seed": 43, "field": "_seq_no"}}], query={"match_all": {}},
             boost_mode="replace")
    assert _scores(_s(client, cq, q2)) != _scores(a)


def test_random_score_with_weight_bounded(client, cq):
    q = _fs([{"random_score": {"seed": 1, "field": "_seq_no"}, "weight": 10}],
            query={"match_all": {}}, boost_mode="replace")
    assert all(0.0 <= v < 10.0 for v in _scores(_s(client, cq, q)).values())


@pytest.mark.parametrize(
    "fn,spec,expected",
    [
        # numeric decay over n: origin 1, scale 2, decay 0.5
        ("gauss", {"n": {"origin": 1, "scale": 2}},
         {"1": 1.0, "3": 0.5, "5": 0.0625}),
        ("exp", {"n": {"origin": 1, "scale": 2}},
         {"1": 1.0, "3": 0.5, "5": 0.25}),
        ("linear", {"n": {"origin": 1, "scale": 2}},
         {"1": 1.0, "3": 0.5, "5": 0.0}),
        ("linear", {"n": {"origin": 1, "scale": 2, "offset": 1}},
         {"1": 1.0, "2": 1.0, "4": 0.5}),
        ("exp", {"n": {"origin": 3, "scale": 1, "decay": 0.1}},
         {"3": 1.0, "2": 0.1, "4": 0.1, "1": 0.01}),
        # date decay: origin 2000-01-01, scale 10d
        ("gauss", {"when": {"origin": "2000-01-01", "scale": "10d"}},
         {"1": 1.0, "2": 0.5, "3": 0.0625}),
        ("exp", {"when": {"origin": "2000-01-21", "scale": "10d", "decay": 0.25}},
         {"3": 1.0, "2": 0.25, "4": 0.25, "1": 0.0625}),
    ],
)
def test_function_score_decay(client, cq, fn, spec, expected):
    q = _fs([{fn: spec}], query={"match_all": {}}, boost_mode="replace")
    got = _scores(_s(client, cq, q))
    for k, v in expected.items():
        assert got[k] == pytest.approx(v, rel=1e-4, abs=1e-6), (k, got)


def test_function_score_decay_missing_field_scores_one(client, cq):
    q = _fs([{"gauss": {"pop": {"origin": 0, "scale": 1}}}], query={"match_all": {}},
            boost_mode="replace")
    assert _scores(_s(client, cq, q))["3"] == 1.0


def test_function_score_geo_decay(client, cq):
    q = _fs([{"gauss": {"loc": {"origin": {"lat": 0, "lon": 0}, "scale": "111km"}}}],
            query={"exists": {"field": "loc"}}, boost_mode="replace")
    got = _scores(_s(client, cq, q))
    assert got["1"] == pytest.approx(1.0)
    # 1 degree of longitude at the equator is ~111.19km
    assert got["2"] == pytest.approx(0.5, abs=0.01)
    assert got["1"] > got["2"] > got["3"]


def test_function_score_script_score(client, cq):
    q = _fs([{"script_score": {"script": {"source": "doc['n'].value * params.k + _score",
                                           "params": {"k": 10}}}}],
            boost_mode="replace")
    assert _scores(_s(client, cq, q)) == {"1": 12.0, "2": 22.0}


def test_function_score_unknown_score_mode_errors(client, cq):
    r = _raw(client, cq, {"query": _fs([W3], score_mode="bogus")})
    _parse_error(r, "No enum constant", etype="illegal_argument_exception")


def test_function_score_negative_weight_rejected(client, cq):
    r = _raw(client, cq, {"query": _fs([{"weight": -1}], boost_mode="replace")})
    _parse_error(r, "[weight] cannot be negative for a filtering function",
                 etype="illegal_argument_exception")


# --------------------------------------------------------------------------
# script_score / script queries
# --------------------------------------------------------------------------


def test_script_score_query(client, cq):
    q = {"script_score": {"query": {"term": {"color": "blue"}},
                          "script": {"source": "doc['n'].value * 1.5"}}}
    res = _s(client, cq, q)
    assert _order(res) == ["4", "3"]
    assert _scores(res) == {"4": 6.0, "3": 4.5}


def test_script_score_min_score(client, cq):
    q = {"script_score": {"query": {"match_all": {}}, "min_score": 3,
                          "script": {"source": "doc['n'].value"}}}
    assert _q(client, cq, q) == {"3", "4", "5"}


def test_script_score_uses_subquery_score(client, cq):
    q = {"script_score": {"query": _cs(_t("color", "red"), 4),
                          "script": {"source": "_score * 2"}}}
    assert _scores(_s(client, cq, q)) == {"1": 8.0, "2": 8.0}


def test_script_score_negative_score_errors(client, cq):
    q = {"script_score": {"query": {"match_all": {}}, "script": {"source": "-1"}}}
    r = _raw(client, cq, {"query": q})
    err = _shard_error(r, "illegal_argument_exception")
    assert "script_score script returned an invalid score [-1.0]" in err["root_cause"][0]["reason"]


@pytest.mark.parametrize(
    "source,params,expected",
    [
        ("doc['n'].value > 2", {}, {"3", "4", "5"}),
        ("doc['n'].value % 2 == 0", {}, {"2", "4"}),
        ("doc['pop'].size() == 0", {}, {"3"}),
        ("doc['color'].value == params.c", {"c": "red"}, {"1", "2"}),
        ("false", {}, set()),
    ],
)
def test_script_query(client, cq, source, params, expected):
    q = {"script": {"script": {"source": source, "params": params}}}
    assert _q(client, cq, q) == expected


def test_script_query_scores_constant(client, cq):
    q = {"script": {"script": "doc['n'].value > 3", "boost": 2}}
    assert _scores(_s(client, cq, q)) == {"4": 2.0, "5": 2.0}


def test_script_query_compile_error(client, cq):
    r = _raw(client, cq, {"query": {"script": {"script": "doc['n'].value >"}}})
    assert r.status_code == 400
    assert r.json()["error"]["root_cause"][0]["type"] == "script_exception"


def test_script_query_missing_value_runtime_error(client, cq):
    r = _raw(client, cq, {"query": {"script": {"script": "doc['pop'].value > 1"}}})
    assert r.status_code == 400
    assert "script_exception" in json.dumps(r.json()["error"])


# --------------------------------------------------------------------------
# nested
# --------------------------------------------------------------------------

NESTED_MAPPING = {
    "properties": {
        "title": {"type": "keyword"},
        "comments": {
            "type": "nested",
            "properties": {
                "author": {"type": "keyword"},
                "stars": {"type": "integer"},
                "replies": {"type": "nested", "properties": {"who": {"type": "keyword"}}},
            },
        },
        "flat": {"properties": {"author": {"type": "keyword"}, "stars": {"type": "integer"}}},
    }
}


def _nested_doc(title, comments):
    return {"title": title, "comments": comments,
            "flat": [{"author": c["author"], "stars": c["stars"]} for c in comments]}


NESTED_DOCS = [
    ("P1", _nested_doc("one", [{"author": "alice", "stars": 5, "replies": [{"who": "bob"}]},
                               {"author": "bob", "stars": 1}])),
    ("P2", _nested_doc("two", [{"author": "alice", "stars": 2},
                               {"author": "carol", "stars": 5, "replies": [{"who": "alice"},
                                                                           {"who": "dave"}]}])),
    ("P3", _nested_doc("three", [{"author": "dave", "stars": 3}])),
    ("P4", {"title": "four"}),
    ("P5", {"title": "five", "comments": []}),
]


@pytest.fixture(scope="module")
def nd(client):
    name = _create(client, NESTED_MAPPING, NESTED_DOCS)
    yield name
    _drop(client, name)


def _stars_score_query(inner=None):
    return {"function_score": {"query": inner or {"match_all": {}},
                               "functions": [{"field_value_factor": {"field": "comments.stars"}}],
                               "boost_mode": "replace"}}


@pytest.mark.parametrize(
    "score_mode,expected",
    [
        (None, {"P1": 3.0, "P2": 3.5, "P3": 3.0}),
        ("avg", {"P1": 3.0, "P2": 3.5, "P3": 3.0}),
        ("max", {"P1": 5.0, "P2": 5.0, "P3": 3.0}),
        ("min", {"P1": 1.0, "P2": 2.0, "P3": 3.0}),
        ("sum", {"P1": 6.0, "P2": 7.0, "P3": 3.0}),
        ("none", {"P1": 0.0, "P2": 0.0, "P3": 0.0}),
    ],
)
def test_nested_score_mode(client, nd, score_mode, expected):
    q = {"nested": {"path": "comments", "query": _stars_score_query()}}
    if score_mode:
        q["nested"]["score_mode"] = score_mode
    assert _scores(_s(client, nd, q)) == expected


def test_nested_score_mode_only_counts_matching_children(client, nd):
    q = {"nested": {"path": "comments", "score_mode": "sum",
                    "query": _stars_score_query({"term": {"comments.author": "alice"}})}}
    assert _scores(_s(client, nd, q)) == {"P1": 5.0, "P2": 2.0}


@pytest.mark.parametrize(
    "author,stars,obj_expected,nested_expected",
    [
        ("alice", 1, {"P1"}, set()),
        ("alice", 5, {"P1", "P2"}, {"P1"}),
        ("bob", 5, {"P1"}, set()),
        ("dave", 3, {"P3"}, {"P3"}),
    ],
)
def test_nested_vs_object_cross_matching(client, nd, author, stars, obj_expected, nested_expected):
    obj_q = {"bool": {"must": [{"term": {"flat.author": author}},
                               {"term": {"flat.stars": stars}}]}}
    nested_q = {"nested": {"path": "comments", "query": {"bool": {"must": [
        {"term": {"comments.author": author}}, {"term": {"comments.stars": stars}}]}}}}
    assert _q(client, nd, obj_q) == obj_expected
    assert _q(client, nd, nested_q) == nested_expected


def test_nested_field_not_queryable_directly(client, nd):
    assert _q(client, nd, {"term": {"comments.author": "alice"}}) == set()


def test_nested_inner_hits_shape(client, nd):
    q = {"nested": {"path": "comments", "query": {"term": {"comments.author": "carol"}},
                    "inner_hits": {}}}
    res = _s(client, nd, q)
    assert _ids(res) == {"P2"}
    ih = res["hits"]["hits"][0]["inner_hits"]["comments"]["hits"]
    assert ih["total"] == {"value": 1, "relation": "eq"}
    hit = ih["hits"][0]
    assert hit["_id"] == "P2"
    assert hit["_nested"] == {"field": "comments", "offset": 1}
    assert hit["_source"] == {"author": "carol", "stars": 5,
                              "replies": [{"who": "alice"}, {"who": "dave"}]}


def test_nested_inner_hits_size_sort_source_name(client, nd):
    q = {"nested": {"path": "comments", "query": {"match_all": {}},
                    "inner_hits": {"name": "top", "size": 1,
                                   "sort": [{"comments.stars": "asc"}],
                                   "_source": ["comments.author"]}}}
    res = _s(client, nd, q, sort=[{"title": "asc"}])
    assert _order(res) == ["P1", "P3", "P2"]
    by_id = {h["_id"]: h["inner_hits"]["top"]["hits"] for h in res["hits"]["hits"]}
    assert by_id["P1"]["total"]["value"] == 2
    assert len(by_id["P1"]["hits"]) == 1
    assert by_id["P1"]["hits"][0]["_source"] == {"author": "bob"}
    assert by_id["P1"]["hits"][0]["_nested"]["offset"] == 1
    assert by_id["P2"]["hits"][0]["_source"] == {"author": "alice"}
    assert by_id["P1"]["hits"][0]["sort"] == [1]


def test_nested_inner_hits_from(client, nd):
    q = {"nested": {"path": "comments", "query": {"match_all": {}},
                    "inner_hits": {"from": 1, "size": 5}}}
    res = _s(client, nd, q)
    by_id = {h["_id"]: h["inner_hits"]["comments"]["hits"]["hits"] for h in res["hits"]["hits"]}
    assert [h["_nested"]["offset"] for h in by_id["P1"]] == [1]
    assert by_id["P3"] == []


def test_nested_in_nested(client, nd):
    q = {"nested": {"path": "comments", "query": {"nested": {
        "path": "comments.replies", "query": {"term": {"comments.replies.who": "alice"}}}}}}
    assert _q(client, nd, q) == {"P2"}


def test_nested_in_nested_full_path_directly(client, nd):
    q = {"nested": {"path": "comments.replies",
                    "query": {"term": {"comments.replies.who": "bob"}}}}
    assert _q(client, nd, q) == {"P1"}


def test_nested_in_nested_inner_hits_only_inner_is_dropped(client, nd):
    q = {"nested": {"path": "comments", "query": {"nested": {
        "path": "comments.replies", "query": {"term": {"comments.replies.who": "dave"}},
        "inner_hits": {}}}}}
    res = _s(client, nd, q)
    assert _ids(res) == {"P2"}
    assert "inner_hits" not in res["hits"]["hits"][0]


def test_nested_in_nested_inner_hits(client, nd):
    q = {"nested": {"path": "comments", "inner_hits": {}, "query": {"nested": {
        "path": "comments.replies", "query": {"term": {"comments.replies.who": "dave"}},
        "inner_hits": {}}}}}
    res = _s(client, nd, q)
    outer = res["hits"]["hits"][0]["inner_hits"]["comments"]["hits"]["hits"][0]
    assert outer["_nested"] == {"field": "comments", "offset": 1}
    ih = outer["inner_hits"]["comments.replies"]["hits"]["hits"][0]
    assert ih["_nested"] == {"field": "comments", "offset": 1,
                             "_nested": {"field": "replies", "offset": 1}}
    assert ih["_source"] == {"who": "dave"}


def test_nested_must_not_semantics(client, nd):
    # docs that have no comment by alice (incl. those without comments)
    q = {"bool": {"must_not": {"nested": {"path": "comments",
                                          "query": {"term": {"comments.author": "alice"}}}}}}
    assert _q(client, nd, q) == {"P3", "P4", "P5"}
    # docs having at least one comment not by alice
    q2 = {"nested": {"path": "comments", "query": {"bool": {
        "must_not": {"term": {"comments.author": "alice"}}}}}}
    assert _q(client, nd, q2) == {"P1", "P2", "P3"}


def test_nested_exists(client, nd):
    q = {"nested": {"path": "comments", "query": {"exists": {"field": "comments"}}}}
    assert _q(client, nd, q) == {"P1", "P2", "P3"}


def test_nested_ignore_unmapped(client, nd):
    q = {"nested": {"path": "nope", "query": {"match_all": {}}, "ignore_unmapped": True}}
    assert _q(client, nd, q) == set()


def test_nested_unmapped_path_errors(client, nd):
    r = _raw(client, nd, {"query": {"nested": {"path": "nope", "query": {"match_all": {}}}}})
    _shard_error(r, "query_shard_exception", "[nested] failed to find nested object under path [nope]")


def test_nested_on_object_path_errors(client, nd):
    r = _raw(client, nd, {"query": {"nested": {"path": "flat", "query": {"match_all": {}}}}})
    _shard_error(r, "query_shard_exception", "[nested] nested object under path [flat] is not of nested type")


def test_nested_hits_total_counts_parents(client, nd):
    res = _s(client, nd, {"match_all": {}})
    assert res["hits"]["total"]["value"] == 5


# --------------------------------------------------------------------------
# parent / child join
# --------------------------------------------------------------------------

JOIN_MAPPING = {
    "properties": {
        "rel": {"type": "join", "relations": {"question": "answer"}},
        "body": {"type": "keyword"},
        "votes": {"type": "integer"},
    }
}

JOIN_DOCS = [
    ("Q1", {"rel": "question", "body": "q one"}),
    ("Q2", {"rel": {"name": "question"}, "body": "q two"}),
    ("Q3", {"rel": "question", "body": "q three"}),
    ("A1", {"rel": {"name": "answer", "parent": "Q1"}, "votes": 1, "body": "a"}, "Q1"),
    ("A2", {"rel": {"name": "answer", "parent": "Q1"}, "votes": 3, "body": "b"}, "Q1"),
    ("A3", {"rel": {"name": "answer", "parent": "Q2"}, "votes": 5, "body": "a"}, "Q2"),
]


@pytest.fixture(scope="module")
def jn(client):
    name = _create(client, JOIN_MAPPING, JOIN_DOCS)
    yield name
    _drop(client, name)


def _votes_q(inner=None):
    return {"function_score": {"query": inner or {"match_all": {}},
                               "functions": [{"field_value_factor": {"field": "votes"}}],
                               "boost_mode": "replace"}}


@pytest.mark.parametrize(
    "extra,expected",
    [
        ({}, {"Q1", "Q2"}),
        ({"min_children": 2}, {"Q1"}),
        ({"max_children": 1}, {"Q2"}),
        ({"min_children": 1, "max_children": 2}, {"Q1", "Q2"}),
        ({"min_children": 3}, set()),
    ],
)
def test_has_child_min_max_children(client, jn, extra, expected):
    q = {"has_child": dict({"type": "answer", "query": {"match_all": {}}}, **extra)}
    assert _q(client, jn, q) == expected


@pytest.mark.parametrize(
    "score_mode,expected",
    [
        ("max", {"Q1": 3.0, "Q2": 5.0}),
        ("min", {"Q1": 1.0, "Q2": 5.0}),
        ("sum", {"Q1": 4.0, "Q2": 5.0}),
        ("avg", {"Q1": 2.0, "Q2": 5.0}),
        ("none", {"Q1": 1.0, "Q2": 1.0}),
        (None, {"Q1": 1.0, "Q2": 1.0}),
    ],
)
def test_has_child_score_mode(client, jn, score_mode, expected):
    q = {"has_child": {"type": "answer", "query": _votes_q()}}
    if score_mode:
        q["has_child"]["score_mode"] = score_mode
    assert _scores(_s(client, jn, q)) == expected


def test_has_child_filters_children(client, jn):
    q = {"has_child": {"type": "answer", "query": {"term": {"body": "a"}}}}
    assert _q(client, jn, q) == {"Q1", "Q2"}
    q = {"has_child": {"type": "answer", "query": {"range": {"votes": {"gte": 2}}}}}
    assert _q(client, jn, q) == {"Q1", "Q2"}
    q = {"has_child": {"type": "answer", "query": {"term": {"body": "b"}}}}
    assert _q(client, jn, q) == {"Q1"}


def test_has_child_inner_hits(client, jn):
    q = {"has_child": {"type": "answer", "query": {"match_all": {}},
                       "inner_hits": {"sort": [{"votes": "desc"}], "size": 5}}}
    res = _s(client, jn, q)
    by_id = {h["_id"]: h["inner_hits"]["answer"]["hits"] for h in res["hits"]["hits"]}
    assert [h["_id"] for h in by_id["Q1"]["hits"]] == ["A2", "A1"]
    assert by_id["Q1"]["hits"][0]["_routing"] == "Q1"
    assert by_id["Q2"]["total"]["value"] == 1


@pytest.mark.parametrize(
    "parent_query,expected",
    [
        ({"ids": {"values": ["Q1"]}}, {"A1", "A2"}),
        ({"term": {"body": "q two"}}, {"A3"}),
        ({"term": {"body": "q three"}}, set()),
        ({"match_all": {}}, {"A1", "A2", "A3"}),
    ],
)
def test_has_parent(client, jn, parent_query, expected):
    q = {"has_parent": {"parent_type": "question", "query": parent_query}}
    assert _q(client, jn, q) == expected


@pytest.mark.parametrize("score,expected", [(True, 7.0), (False, 1.0)])
def test_has_parent_score(client, jn, score, expected):
    q = {"has_parent": {"parent_type": "question", "score": score,
                        "query": _cs({"ids": {"values": ["Q1"]}}, 7.0)}}
    assert _scores(_s(client, jn, q)) == {"A1": expected, "A2": expected}


def test_has_parent_inner_hits(client, jn):
    q = {"has_parent": {"parent_type": "question", "query": {"match_all": {}},
                        "inner_hits": {}}}
    res = _s(client, jn, q)
    for h in res["hits"]["hits"]:
        parent = h["inner_hits"]["question"]["hits"]["hits"]
        assert len(parent) == 1
        assert parent[0]["_id"] == h["_routing"]


@pytest.mark.parametrize("pid,expected", [("Q1", {"A1", "A2"}), ("Q2", {"A3"}),
                                          ("Q3", set()), ("nope", set())])
def test_parent_id(client, jn, pid, expected):
    assert _q(client, jn, {"parent_id": {"type": "answer", "id": pid}}) == expected


def test_term_on_join_field(client, jn):
    assert _q(client, jn, {"term": {"rel": "answer"}}) == {"A1", "A2", "A3"}
    assert _q(client, jn, {"term": {"rel": "question"}}) == {"Q1", "Q2", "Q3"}
    assert _q(client, jn, {"term": {"rel#question": "Q1"}}) == {"Q1", "A1", "A2"}


def test_join_child_routing_returned(client, jn):
    res = _s(client, jn, {"ids": {"values": ["A3"]}})
    assert res["hits"]["hits"][0]["_routing"] == "Q2"


def test_join_child_without_routing_rejected(client, jn):
    r = client.put(f"/{jn}/_doc/AX", json={"rel": {"name": "answer", "parent": "Q1"}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "mapper_parsing_exception"
    assert err["reason"] == "failed to parse"
    assert err["caused_by"] == {"type": "illegal_argument_exception",
                                "reason": "[routing] is missing for join field [rel]"}


def test_has_child_unknown_type_errors(client, jn):
    r = _raw(client, jn, {"query": {"has_child": {"type": "nope", "query": {"match_all": {}}}}})
    _shard_error(r, "query_shard_exception", "[has_child] join field [rel] doesn't hold [nope] as a child")


@pytest.mark.parametrize(
    "query",
    [
        {"has_child": {"type": "answer", "query": {"match_all": {}}, "ignore_unmapped": True}},
        {"has_parent": {"parent_type": "question", "query": {"match_all": {}},
                        "ignore_unmapped": True}},
        {"parent_id": {"type": "answer", "id": "Q1", "ignore_unmapped": True}},
    ],
)
def test_join_queries_ignore_unmapped_on_index_without_join(client, cq, query):
    assert _q(client, cq, query) == set()


def test_has_child_without_join_field_errors(client, cq):
    r = _raw(client, cq, {"query": {"has_child": {"type": "x", "query": {"match_all": {}}}}})
    _shard_error(r, "query_shard_exception", "[has_child] no join field has been configured")


# --------------------------------------------------------------------------
# geo
# --------------------------------------------------------------------------

GEO_MAPPING = {
    "properties": {
        "name": {"type": "keyword"},
        "loc": {"type": "geo_point"},
        "shape": {"type": "geo_shape"},
    }
}

PARIS = {"lat": 48.8566, "lon": 2.3522}

GEO_DOCS = [
    ("paris", {"name": "paris", "loc": PARIS,
               "shape": {"type": "point", "coordinates": [2.3522, 48.8566]}}),
    ("london", {"name": "london", "loc": [-0.1278, 51.5074],
                "shape": {"type": "envelope", "coordinates": [[-1, 52], [1, 51]]}}),
    ("ny", {"name": "ny", "loc": "40.7128,-74.0060",
            "shape": "POLYGON ((-75 40, -73 40, -73 41, -75 41, -75 40))"}),
    ("fiji", {"name": "fiji", "loc": "POINT (179.5 0)",
              "shape": {"type": "linestring", "coordinates": [[0, 0], [10, 10]]}}),
    ("samoa", {"name": "samoa", "loc": {"lat": 0, "lon": -179.5},
               "shape": {"type": "polygon",
                         "coordinates": [[[-20, 40], [20, 40], [20, 60], [-20, 60], [-20, 40]]]}}),
    ("berlin", {"name": "berlin", "loc": "u33dc0cpke7v"}),
    ("noloc", {"name": "noloc"}),
]


@pytest.fixture(scope="module")
def geo(client):
    name = _create(client, GEO_MAPPING, GEO_DOCS)
    yield name
    _drop(client, name)


@pytest.mark.parametrize(
    "distance,expected",
    [
        ("100km", {"paris"}),
        ("400km", {"paris", "london"}),
        ("1000km", {"paris", "london", "berlin"}),
        ("250mi", {"paris", "london"}),
        ("340km", {"paris"}),        # Paris-London ~343.5km
        ("350km", {"paris", "london"}),
        ("400000m", {"paris", "london"}),
        ("400000", {"paris", "london"}),
        (400000, {"paris", "london"}),
        ("0.4mm", {"paris"}),        # distance 0 always matches the origin itself
        ("6000km", {"paris", "london", "berlin", "ny"}),
        ("216nmi", {"paris", "london"}),
    ],
)
def test_geo_distance_units(client, geo, distance, expected):
    q = {"geo_distance": {"distance": distance, "loc": PARIS}}
    assert _q(client, geo, q) == expected


@pytest.mark.parametrize(
    "origin",
    [
        {"lat": 48.8566, "lon": 2.3522},
        [2.3522, 48.8566],
        "48.8566,2.3522",
        "u09tvw0f6szy",
        "POINT (2.3522 48.8566)",
    ],
    ids=["object", "array-lon-lat", "string-lat-lon", "geohash", "wkt"],
)
def test_geo_distance_point_formats(client, geo, origin):
    q = {"geo_distance": {"distance": "400km", "loc": origin}}
    assert _q(client, geo, q) == {"paris", "london"}


def test_geo_distance_array_is_lon_lat_not_lat_lon(client, geo):
    q = {"geo_distance": {"distance": "400km", "loc": [48.8566, 2.3522]}}
    assert _q(client, geo, q) == set()


@pytest.mark.parametrize("dtype", ["arc", "plane"])
def test_geo_distance_type(client, geo, dtype):
    q = {"geo_distance": {"distance": "400km", "distance_type": dtype, "loc": PARIS}}
    assert _q(client, geo, q) == {"paris", "london"}


def test_geo_distance_across_dateline(client, geo):
    q = {"geo_distance": {"distance": "200km", "loc": {"lat": 0, "lon": 180}}}
    assert _q(client, geo, q) == {"fiji", "samoa"}


def test_geo_distance_score_constant(client, geo):
    res = _s(client, geo, {"geo_distance": {"distance": "400km", "loc": PARIS, "boost": 2}})
    assert set(_scores(res).values()) == {2.0}


def test_geo_distance_sort_values(client, geo):
    res = _s(client, geo, {"geo_distance": {"distance": "1000km", "loc": PARIS}},
             sort=[{"_geo_distance": {"loc": PARIS, "order": "asc", "unit": "km"}}])
    assert _order(res) == ["paris", "london", "berlin"]
    assert res["hits"]["hits"][0]["sort"][0] == pytest.approx(0.0, abs=1e-6)
    assert res["hits"]["hits"][1]["sort"][0] == pytest.approx(343.5, abs=1.0)


def test_geo_distance_unmapped_field_errors(client, geo):
    r = _raw(client, geo, {"query": {"geo_distance": {"distance": "1km", "nope": [0, 0]}}})
    _shard_error(r, "query_shard_exception", "failed to find geo field [nope]")


def test_geo_distance_ignore_unmapped(client, geo):
    q = {"geo_distance": {"distance": "1km", "nope": [0, 0], "ignore_unmapped": True}}
    assert _q(client, geo, q) == set()


def test_geo_distance_on_non_geo_field_errors(client, geo):
    r = _raw(client, geo, {"query": {"geo_distance": {"distance": "1km", "name": [0, 0]}}})
    _shard_error(r, "query_shard_exception",
                 "for field [name] is not supported for [geo_distance] queries. "
                 "Must be one of [geo_point] or [geo_shape]")


def test_geo_distance_invalid_lat_errors(client, geo):
    q = {"geo_distance": {"distance": "1km", "loc": {"lat": 100, "lon": 0}}}
    r = _raw(client, geo, {"query": q})
    err = _shard_error(r, "query_shard_exception", "couldn't validate latitude/ longitude values")
    assert "[geo_distance] center point latitude is invalid: 100.0" in json.dumps(err)


def test_geo_distance_invalid_lat_ignore_malformed_still_fails(client, geo):
    q = {"geo_distance": {"distance": "1km", "loc": {"lat": 100, "lon": 0},
                          "validation_method": "IGNORE_MALFORMED"}}
    r = _raw(client, geo, {"query": q})
    _shard_error(r, "query_shard_exception",
                 "invalid latitude 100.0; must be between -90.0 and 90.0")


@pytest.mark.parametrize(
    "box,expected",
    [
        ({"top_left": {"lat": 55, "lon": -5}, "bottom_right": {"lat": 45, "lon": 15}},
         {"paris", "london", "berlin"}),
        ({"top_left": [-5, 55], "bottom_right": [15, 45]}, {"paris", "london", "berlin"}),
        ({"top_left": "55,-5", "bottom_right": "45,15"}, {"paris", "london", "berlin"}),
        ({"top": 55, "left": -5, "bottom": 45, "right": 15}, {"paris", "london", "berlin"}),
        ({"wkt": "BBOX (-5, 15, 55, 45)"}, {"paris", "london", "berlin"}),
        ({"top_left": {"lat": 50, "lon": 0}, "bottom_right": {"lat": 45, "lon": 5}}, {"paris"}),
        # crossing the dateline: left > right
        ({"top_left": {"lat": 1, "lon": 179}, "bottom_right": {"lat": -1, "lon": -179}},
         {"fiji", "samoa"}),
        ({"top_left": {"lat": 1, "lon": -179}, "bottom_right": {"lat": -1, "lon": 179}},
         set()),
        ({"top_right": {"lat": 55, "lon": 15}, "bottom_left": {"lat": 45, "lon": -5}},
         {"paris", "london", "berlin"}),
    ],
)
def test_geo_bounding_box(client, geo, box, expected):
    assert _q(client, geo, {"geo_bounding_box": {"loc": box}}) == expected


def test_geo_bounding_box_top_below_bottom_errors(client, geo):
    q = {"geo_bounding_box": {"loc": {"top_left": {"lat": 40, "lon": -5},
                                      "bottom_right": {"lat": 50, "lon": 15}}}}
    r = _raw(client, geo, {"query": q})
    assert r.status_code == 400
    assert "top is below bottom corner" in json.dumps(r.json())


def test_geo_bounding_box_ignore_unmapped(client, geo):
    box = {"top_left": [0, 1], "bottom_right": [1, 0]}
    assert _q(client, geo, {"geo_bounding_box": {"nope": box, "ignore_unmapped": True}}) == set()
    r = _raw(client, geo, {"query": {"geo_bounding_box": {"nope": box}}})
    _shard_error(r, "query_shard_exception", "failed to find geo field [nope]")


@pytest.mark.parametrize(
    "points,expected",
    [
        ([{"lat": 55, "lon": -5}, {"lat": 55, "lon": 15}, {"lat": 45, "lon": 15},
          {"lat": 45, "lon": -5}], {"paris", "london", "berlin"}),
        ([[-5, 55], [15, 55], [15, 45], [-5, 45]], {"paris", "london", "berlin"}),
        (["55,-5", "55,1", "45,1"], {"london"}),
    ],
)
def test_geo_polygon_still_supported(client, geo, points, expected):
    q = {"geo_polygon": {"loc": {"points": points}}}
    assert _q(client, geo, q) == expected


@pytest.mark.parametrize(
    "shape,relation,expected",
    [
        ({"type": "envelope", "coordinates": [[-2, 53], [3, 48]]}, "intersects",
         {"paris", "london", "samoa"}),
        ({"type": "envelope", "coordinates": [[-2, 53], [3, 48]]}, "within",
         {"paris", "london"}),
        ({"type": "envelope", "coordinates": [[-2, 53], [3, 48]]}, "disjoint",
         {"ny", "fiji"}),
        ({"type": "point", "coordinates": [-0.5, 51.5]}, "contains", {"london", "samoa"}),
        ({"type": "envelope", "coordinates": [[4, 6], [6, 4]]}, "intersects", {"fiji"}),
        ({"type": "envelope", "coordinates": [[4, 6], [6, 4]]}, "within", set()),
        ({"type": "polygon",
          "coordinates": [[[-80, 35], [-70, 35], [-70, 45], [-80, 45], [-80, 35]]]},
         "within", {"ny"}),
        ("POINT (-74 40.5)", "intersects", {"ny"}),
        ("BBOX (-2, 3, 53, 48)", "within", {"paris", "london"}),
    ],
)
def test_geo_shape_relations(client, geo, shape, relation, expected):
    q = {"geo_shape": {"shape": {"shape": shape, "relation": relation}}}
    assert _q(client, geo, q) == expected


def test_geo_shape_default_relation_is_intersects(client, geo):
    q = {"geo_shape": {"shape": {"shape": {"type": "envelope", "coordinates": [[4, 6], [6, 4]]}}}}
    assert _q(client, geo, q) == {"fiji"}


def test_geo_shape_query_on_geo_point_field(client, geo):
    q = {"geo_shape": {"loc": {"shape": {"type": "envelope", "coordinates": [[-5, 55], [15, 45]]},
                               "relation": "intersects"}}}
    assert _q(client, geo, q) == {"paris", "london", "berlin"}


def test_geo_shape_indexed_shape(client, geo):
    q = {"geo_shape": {"shape": {"indexed_shape": {"index": geo, "id": "london",
                                                   "path": "shape"},
                                 "relation": "intersects"}}}
    assert _q(client, geo, q) == {"london", "samoa"}


def test_geo_shape_ignore_unmapped(client, geo):
    shape = {"type": "point", "coordinates": [0, 0]}
    assert _q(client, geo, {"geo_shape": {"nope": {"shape": shape}, "ignore_unmapped": True}}) == set()
    r = _raw(client, geo, {"query": {"geo_shape": {"nope": {"shape": shape}}}})
    _shard_error(r, "query_shard_exception", "failed to find type for field [nope]")


# --------------------------------------------------------------------------
# percolate
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def perc(client):
    name = _create(
        client,
        {"properties": {"query": {"type": "percolator"}, "msg": {"type": "text"},
                        "n": {"type": "integer"}, "tag": {"type": "keyword"}}},
        [
            ("fox", {"query": {"match": {"msg": "fox"}}, "tag": "animal"}),
            ("big", {"query": {"range": {"n": {"gte": 10}}}, "tag": "num"}),
            ("quickdog", {"query": {"bool": {"must": [{"match": {"msg": "quick"}},
                                                      {"match": {"msg": "dog"}}]}},
                          "tag": "animal"}),
            ("all", {"query": {"match_all": {}}, "tag": "any"}),
            ("doc1", {"msg": "lazy fox jumps", "n": 50}),
        ],
    )
    yield name
    _drop(client, name)


def _perc_q(**kw):
    return {"percolate": dict({"field": "query"}, **kw)}


@pytest.mark.parametrize(
    "doc,expected",
    [
        ({"msg": "the quick brown fox"}, {"fox", "all"}),
        ({"msg": "quick dog", "n": 10}, {"big", "quickdog", "all"}),
        ({"msg": "nothing", "n": 9}, {"all"}),
        ({}, {"all"}),
    ],
)
def test_percolate_single_document(client, perc, doc, expected):
    assert _q(client, perc, _perc_q(document=doc)) == expected


def test_percolate_multiple_documents_slots(client, perc):
    res = _s(client, perc, _perc_q(documents=[{"msg": "fox"}, {"msg": "dog", "n": 11},
                                              {"msg": "a fox", "n": 12}]))
    slots = {h["_id"]: h["fields"]["_percolator_document_slot"] for h in res["hits"]["hits"]}
    assert slots == {"fox": [0, 2], "big": [1, 2], "all": [0, 1, 2]}


def test_percolate_single_document_slot_field(client, perc):
    res = _s(client, perc, _perc_q(document={"msg": "fox"}))
    for h in res["hits"]["hits"]:
        assert h["fields"]["_percolator_document_slot"] == [0]


def test_percolate_stored_document(client, perc):
    assert _q(client, perc, _perc_q(index=perc, id="doc1")) == {"fox", "big", "all"}


def test_percolate_stored_document_missing(client, perc):
    r = _raw(client, perc, {"query": _perc_q(index=perc, id="nope")})
    assert r.status_code == 404
    assert r.json()["error"]["type"] == "resource_not_found_exception"


def test_percolate_combined_with_filter(client, perc):
    q = {"bool": {"must": _perc_q(document={"msg": "quick fox dog", "n": 99}),
                  "filter": {"term": {"tag": "animal"}}}}
    assert _q(client, perc, q) == {"fox", "quickdog"}


def test_percolate_name_does_not_rename_single_slot_field(client, perc):
    q = _perc_q(document={"msg": "fox"}, name="mine")
    res = _s(client, perc, q)
    assert {h["_id"]: h["fields"] for h in res["hits"]["hits"]} == {
        "fox": {"_percolator_document_slot": [0]},
        "all": {"_percolator_document_slot": [0]},
    }


def test_percolate_highlighting(client, perc):
    q = _perc_q(document={"msg": "the quick brown fox"})
    res = _s(client, perc, q, highlight={"fields": {"msg": {}}})
    by_id = {h["_id"]: h.get("highlight") for h in res["hits"]["hits"]}
    assert by_id["fox"] == {"msg": ["the quick brown <em>fox</em>"]}


def test_percolate_highlighting_multiple_documents(client, perc):
    q = _perc_q(documents=[{"msg": "fox one"}, {"msg": "two fox"}])
    res = _s(client, perc, q, highlight={"fields": {"msg": {}}})
    by_id = {h["_id"]: h.get("highlight") for h in res["hits"]["hits"]}
    assert by_id["fox"] == {"0_msg": ["<em>fox</em> one"], "1_msg": ["two <em>fox</em>"]}


def test_percolate_on_non_percolator_field_errors(client, perc):
    r = _raw(client, perc, {"query": {"percolate": {"field": "msg", "document": {}}}})
    _shard_error(r, "query_shard_exception", "expected field [msg] to be of type [percolator]")


def test_percolator_rejects_query_on_unmapped_field(client, perc):
    r = client.put(f"/{perc}/_doc/bad", json={"query": {"term": {"nope": "x"}}})
    assert r.status_code == 400
    assert "No field mapping can be found for the field with name [nope]" in r.text


# --------------------------------------------------------------------------
# more_like_this
# --------------------------------------------------------------------------

MLT_DOCS = [
    ("1", {"body": "apple banana cherry", "cat": "f"}),
    ("2", {"body": "apple banana", "cat": "f"}),
    ("3", {"body": "banana cherry durian", "cat": "f"}),
    ("4", {"body": "zebra lion tiger", "cat": "a"}),
    ("5", {"body": "lion tiger apple", "cat": "a"}),
    ("6", {"body": "elephant", "cat": "a"}),
]


@pytest.fixture(scope="module")
def mlt(client):
    name = _create(client, {"properties": {"body": {"type": "text"}, "cat": {"type": "keyword"}}},
                   MLT_DOCS)
    yield name
    _drop(client, name)


def _mlt(**kw):
    body = {"fields": ["body"], "min_term_freq": 1, "min_doc_freq": 1}
    body.update(kw)
    return {"more_like_this": body}


def test_mlt_default_thresholds_match_nothing_on_small_index(client, mlt):
    q = {"more_like_this": {"fields": ["body"], "like": "apple banana"}}
    assert _q(client, mlt, q) == set()


@pytest.mark.parametrize(
    "like,expected",
    [
        ("apple", {"1", "2", "5"}),
        ("tiger zebra", {"4", "5"}),
        ("unknownword", set()),
        (["apple", "elephant"], {"1", "2", "5", "6"}),
    ],
)
def test_mlt_like_text(client, mlt, like, expected):
    assert _q(client, mlt, _mlt(like=like)) == expected


def test_mlt_like_doc_excludes_itself(client, mlt):
    q = _mlt(like=[{"_index": mlt, "_id": "4"}])
    assert _q(client, mlt, q) == {"5"}


def test_mlt_like_doc_include(client, mlt):
    q = _mlt(like=[{"_index": mlt, "_id": "4"}], include=True)
    assert _q(client, mlt, q) == {"4", "5"}


def test_mlt_like_artificial_doc(client, mlt):
    q = _mlt(like=[{"doc": {"body": "durian"}}])
    assert _q(client, mlt, q) == {"3"}


def test_mlt_unlike(client, mlt):
    q = _mlt(like="apple banana", unlike="banana")
    assert _q(client, mlt, q) == {"1", "2", "5"}


def test_mlt_min_doc_freq_filters_rare_terms(client, mlt):
    # 'durian' appears in 1 doc only, 'banana' in 3
    q = _mlt(like="durian banana", min_doc_freq=2)
    assert _q(client, mlt, q) == {"1", "2", "3"}


def test_mlt_max_doc_freq(client, mlt):
    q = _mlt(like="banana lion", max_doc_freq=2)
    assert _q(client, mlt, q) == {"4", "5"}


def test_mlt_min_term_freq_on_like_text(client, mlt):
    assert _q(client, mlt, _mlt(like="apple lion", min_term_freq=2)) == set()
    assert _q(client, mlt, _mlt(like="apple apple lion", min_term_freq=2)) == {"1", "2", "5"}


def test_mlt_minimum_should_match(client, mlt):
    q = _mlt(like="apple banana cherry", minimum_should_match="100%")
    assert _q(client, mlt, q) == {"1"}


def test_mlt_stop_words(client, mlt):
    assert _q(client, mlt, _mlt(like="apple lion", stop_words=["apple"])) == {"4", "5"}


def test_mlt_requires_like(client, mlt):
    r = _raw(client, mlt, {"query": {"more_like_this": {"fields": ["body"]}}})
    assert r.status_code == 400
    assert "requires 'like' to be specified" in r.text


# --------------------------------------------------------------------------
# distance_feature / rank_feature
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "boost,expected",
    [
        (1.0, {"1": 1.0, "2": 0.5, "3": 1 / 3, "4": 0.25}),
        (4.0, {"1": 4.0, "2": 2.0, "3": 4 / 3, "4": 1.0}),
    ],
)
def test_distance_feature_date(client, cq, boost, expected):
    # score = boost * pivot / (pivot + distance)
    q = {"distance_feature": {"field": "when", "origin": 946684800000, "pivot": "10d",
                              "boost": boost}}
    got = _scores(_s(client, cq, q))
    assert got.keys() == expected.keys()
    for k, v in expected.items():
        assert got[k] == pytest.approx(v, rel=1e-5)


def test_distance_feature_date_string_origin_rounds_up(client, cq):
    # a date-string origin is parsed with round-up semantics:
    # "2000-01-01" behaves like 2000-01-01T23:59:59.999
    q = {"distance_feature": {"field": "when", "origin": "2000-01-01", "pivot": "10d"}}
    got = _scores(_s(client, cq, q))
    expected = {"1": 10 / 11, "2": 10 / 19, "3": 10 / 29, "4": 10 / 39}
    for k, v in expected.items():
        assert got[k] == pytest.approx(v, rel=1e-5)


def test_distance_feature_date_symmetric(client, cq):
    q = {"distance_feature": {"field": "when", "origin": 948412800000, "pivot": "10d"}}
    res = _s(client, cq, q)
    s = _scores(res)
    assert _order(res)[0] == "3"
    assert s["3"] == pytest.approx(1.0)
    assert s["2"] == pytest.approx(0.5) and s["4"] == pytest.approx(0.5)


def test_distance_feature_geo(client, cq):
    q = {"distance_feature": {"field": "loc", "origin": [0, 0], "pivot": "1000km"}}
    res = _s(client, cq, q)
    assert _order(res) == ["1", "2", "3"]
    assert _scores(res)["1"] == pytest.approx(1.0)


def test_distance_feature_bad_field_type(client, cq):
    q = {"distance_feature": {"field": "n", "origin": 1, "pivot": "1"}}
    r = _raw(client, cq, {"query": q})
    _shard_error(r, "query_shard_exception",
                 "[distance_feature] query can only be run on a date, date_nanos or geo_point field type!")


RF_DOCS = [
    ("1", {"pr": 1.0, "neg": 1.0}),
    ("2", {"pr": 2.0, "neg": 2.0}),
    ("3", {"pr": 4.0, "neg": 4.0}),
    ("4", {"pr": 8.0, "neg": 8.0}),
    ("5", {"other": 1}),
]


@pytest.fixture(scope="module")
def rf(client):
    name = _create(client, {"properties": {
        "pr": {"type": "rank_feature"},
        "neg": {"type": "rank_feature", "positive_score_impact": False},
        "tags": {"type": "rank_features"},
        "other": {"type": "integer"},
    }}, RF_DOCS + [("6", {"tags": {"a": 3.0, "b": 1.0}})])
    yield name
    _drop(client, name)


@pytest.mark.parametrize(
    "fn,formula",
    [
        ({"saturation": {"pivot": 4}}, lambda s: s / (s + 4)),
        ({"log": {"scaling_factor": 1}}, lambda s: math.log(1 + s)),
        ({"sigmoid": {"pivot": 4, "exponent": 2}}, lambda s: s ** 2 / (s ** 2 + 16)),
        ({"linear": {}}, lambda s: s),
    ],
    ids=["saturation", "log", "sigmoid", "linear"],
)
def test_rank_feature_functions(client, rf, fn, formula):
    q = {"rank_feature": dict({"field": "pr"}, **fn)}
    res = _s(client, rf, q)
    got = _scores(res)
    assert got.keys() == {"1", "2", "3", "4"}
    assert _order(res) == ["4", "3", "2", "1"]
    for k, v in {"1": 1.0, "2": 2.0, "3": 4.0, "4": 8.0}.items():
        assert got[k] == pytest.approx(formula(v), rel=1e-3)


def test_rank_feature_default_saturation(client, rf):
    res = _s(client, rf, {"rank_feature": {"field": "pr"}})
    assert _order(res) == ["4", "3", "2", "1"]
    assert all(0 < v < 1 for v in _scores(res).values())


def test_rank_feature_negative_impact_reverses(client, rf):
    res = _s(client, rf, {"rank_feature": {"field": "neg", "saturation": {"pivot": 4}}})
    assert _order(res) == ["1", "2", "3", "4"]
    # negative impact indexes 1/S: score = (1/S) / (1/S + pivot)
    for k, v in {"1": 1.0, "2": 2.0, "3": 4.0, "4": 8.0}.items():
        assert _scores(res)[k] == pytest.approx((1 / v) / (1 / v + 4), rel=1e-3)


def test_rank_feature_boost(client, rf):
    q = {"rank_feature": {"field": "pr", "linear": {}, "boost": 2}}
    assert _scores(_s(client, rf, q))["4"] == pytest.approx(16.0, rel=1e-3)


def test_rank_features_subfield(client, rf):
    q = {"rank_feature": {"field": "tags.a", "linear": {}}}
    res = _s(client, rf, q)
    assert _ids(res) == {"6"}
    assert _scores(res)["6"] == pytest.approx(3.0, rel=1e-3)


def test_rank_feature_on_non_feature_field_errors(client, rf):
    r = _raw(client, rf, {"query": {"rank_feature": {"field": "other"}}})
    _shard_error(r, "query_shard_exception",
                 "[rank_feature] query only works on [rank_feature] fields and features of "
                 "[rank_features] fields, not [integer]")


def test_rank_feature_multiple_functions_rejected(client, rf):
    q = {"rank_feature": {"field": "pr", "linear": {}, "log": {"scaling_factor": 1}}}
    r = _raw(client, rf, {"query": q})
    assert r.status_code == 400


def test_rank_feature_rejects_negative_value(client, rf):
    r = client.put(f"/{rf}/_doc/x", json={"pr": -1})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "mapper_parsing_exception"


# --------------------------------------------------------------------------
# wrapper / named queries / unmapped
# --------------------------------------------------------------------------


def _b64(obj):
    return base64.b64encode(json.dumps(obj).encode()).decode()


def test_wrapper_query(client, cq):
    q = {"wrapper": {"query": _b64({"term": {"color": "red"}})}}
    assert _q(client, cq, q) == {"1", "2"}


def test_wrapper_inside_bool(client, cq):
    q = {"bool": {"filter": [{"wrapper": {"query": _b64({"range": {"n": {"gte": 4}}})}}]}}
    assert _q(client, cq, q) == {"4", "5"}


def test_wrapper_invalid_base64(client, cq):
    r = _raw(client, cq, {"query": {"wrapper": {"query": "!!!notbase64"}}})
    assert r.status_code == 400


def test_named_queries_matched_queries(client, cq):
    q = {"bool": {"should": [
        {"term": {"color": {"value": "red", "_name": "is_red"}}},
        {"range": {"n": {"gte": 2, "_name": "n_ge_2"}}},
        {"match": {"title": {"query": "fox", "_name": "foxy"}}},
    ]}}
    res = _s(client, cq, q)
    mq = {h["_id"]: sorted(h["matched_queries"]) for h in res["hits"]["hits"]}
    assert mq == {
        "1": ["foxy", "is_red"],
        "2": ["is_red", "n_ge_2"],
        "3": ["foxy", "n_ge_2"],
        "4": ["n_ge_2"],
        "5": ["n_ge_2"],
    }


def test_named_queries_on_compound_and_filter(client, cq):
    q = {"bool": {"_name": "outer", "filter": [{"terms": {"color": ["blue"], "_name": "blue"}}]}}
    res = _s(client, cq, q)
    for h in res["hits"]["hits"]:
        assert sorted(h["matched_queries"]) == ["blue", "outer"]


def test_named_queries_absent_when_none_named(client, cq):
    res = _s(client, cq, {"term": {"color": "red"}})
    assert all("matched_queries" not in h for h in res["hits"]["hits"])


def test_named_queries_with_scores(client, cq):
    q = {"bool": {"should": [
        _cs({"term": {"color": "red"}}, 2),
        {"constant_score": {"filter": {"term": {"size": "m"}}, "boost": 3, "_name": "m"}},
        {"constant_score": {"filter": {"term": {"color": "red"}}, "boost": 2, "_name": "r"}},
    ]}}
    r = _raw(client, cq, {"query": q}, include_named_queries_score="true")
    assert r.status_code == 200
    mq = {h["_id"]: h["matched_queries"] for h in r.json()["hits"]["hits"]}
    assert mq == {"1": {"r": 2.0}, "2": {"m": 3.0, "r": 2.0}, "3": {"m": 3.0}}


@pytest.mark.parametrize(
    "query",
    [
        {"term": {"nope": "x"}},
        {"terms": {"nope": ["x"]}},
        {"range": {"nope": {"gte": 1}}},
        {"prefix": {"nope": "x"}},
        {"wildcard": {"nope": "*"}},
        {"regexp": {"nope": ".*"}},
        {"fuzzy": {"nope": "x"}},
        {"exists": {"field": "nope"}},
        {"match": {"nope": "x"}},
        {"terms_set": {"nope": {"terms": ["x"], "minimum_should_match_script": {"source": "1"}}}},
    ],
)
def test_unmapped_field_queries_return_zero_hits(client, cq, query):
    res = _s(client, cq, query)
    assert res["hits"]["total"]["value"] == 0
    assert res["_shards"]["failed"] == 0
