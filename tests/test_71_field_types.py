"""Field data types and mapping parameters (OpenSearch 3.x behaviour spec).

Every expectation in this file was verified empirically against a reference
OpenSearch 3.9.0 node (min distribution). Many corner cases intentionally
encode surprising-but-real behaviour.
"""

import json
import uuid

import pytest

pytestmark = pytest.mark.types

SETTINGS = {"number_of_shards": 1, "number_of_replicas": 0}


# --------------------------------------------------------------------------- helpers


def _mk(index_factory, mappings=None, settings=None):
    return index_factory(mappings=mappings, settings=dict(SETTINGS, **(settings or {})))


def _put(client, index, doc, doc_id=None, **params):
    params.setdefault("refresh", "true")
    path = f"/{index}/_doc" + (f"/{doc_id}" if doc_id is not None else "")
    return client.post(path, json=doc, params=params)


def _search(client, index, body):
    return client.post(f"/{index}/_search", json=body)


def _ok_search(client, index, body):
    r = _search(client, index, body)
    assert r.status_code == 200, r.text
    return r.json()


def _ids(client, index, query, **extra):
    body = {"query": query, "size": 100}
    body.update(extra)
    res = _ok_search(client, index, body)
    return sorted(h["_id"] for h in res["hits"]["hits"])


def _err(resp):
    """Return (top-level error type, caused_by type) of an error response."""
    e = resp.json()["error"]
    return e["type"], (e.get("caused_by") or {}).get("type")


def _cause_reason(resp):
    e = resp.json()["error"]
    return (e.get("caused_by") or {}).get("reason") or e.get("reason")


def _props(client, index):
    return client.get(f"/{index}/_mapping").json()[index]["mappings"].get("properties", {})


def _put_mapping(client, index, body):
    return client.put(f"/{index}/_mapping", json=body)


def _dv(client, index, field, doc_id=None, fmt=None):
    spec = {"field": field, "format": fmt} if fmt else field
    body = {"docvalue_fields": [spec], "size": 100}
    if doc_id is not None:
        body["query"] = {"ids": {"values": [doc_id]}}
    res = _ok_search(client, index, body)
    return {h["_id"]: h.get("fields", {}).get(field) for h in res["hits"]["hits"]}


@pytest.fixture
def try_create(client):
    """Create an index expected to (possibly) fail; always clean up."""
    names = []

    def _create(mappings=None, settings=None):
        name = f"slopensearch-t71-{uuid.uuid4().hex[:10]}"
        names.append(name)
        body = {"settings": dict(SETTINGS, **(settings or {}))}
        if mappings is not None:
            body["mappings"] = mappings
        return client.put(f"/{name}", json=body)

    yield _create
    for n in names:
        client.delete(f"/{n}", params={"ignore_unavailable": "true"})


# =========================================================================== numerics

INT_LIMITS = [
    ("byte", 127, -128),
    ("short", 32767, -32768),
    ("integer", 2147483647, -2147483648),
    ("long", 9223372036854775807, -9223372036854775808),
]


@pytest.mark.parametrize("ftype,mx,mn", INT_LIMITS)
def test_integer_types_accept_limits(client, index_factory, ftype, mx, mn):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    assert _put(client, idx, {"f": mx}, "max").status_code == 201
    assert _put(client, idx, {"f": mn}, "min").status_code == 201
    dv = _dv(client, idx, "f")
    assert dv == {"max": [mx], "min": [mn]}


@pytest.mark.parametrize("ftype,mx,mn", INT_LIMITS)
@pytest.mark.parametrize("delta", [1, -1])
def test_integer_types_reject_out_of_range(client, index_factory, ftype, mx, mn, delta):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    value = mx + 1 if delta > 0 else mn - 1
    r = _put(client, idx, {"f": value})
    assert r.status_code == 400
    top, cause = _err(r)
    assert top == "mapper_parsing_exception"
    # byte checks the value itself, bigger types fail in the JSON number coercion layer
    assert cause == ("illegal_argument_exception" if ftype == "byte" else "input_coercion_exception")


@pytest.mark.parametrize(
    "ftype,value,msg",
    [
        ("byte", "128", "Value [128] is out of range for a byte"),
        ("short", "32768", "Value [32768] is out of range for a short"),
        ("integer", "2147483648", "Value [2147483648] is out of range for an integer"),
        ("long", "9223372036854775808", "Value [9223372036854775808] is out of range for a long"),
        ("unsigned_long", "18446744073709551616", "Value [18446744073709551616] is out of range for an unsigned long"),
        ("unsigned_long", -1, "Value [-1] is out of range for an unsigned long"),
    ],
)
def test_out_of_range_string_values_message(client, index_factory, ftype, value, msg):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    r = _put(client, idx, {"f": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == msg


def test_mapper_parsing_reason_mentions_field_and_type(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "byte"}}})
    r = _put(client, idx, {"f": 128}, "doc-1")
    reason = r.json()["error"]["reason"]
    assert reason == (
        "failed to parse field [f] of type [byte] in document with id 'doc-1'. "
        "Preview of field's value: '128'"
    )


def test_unsigned_long_limits(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "unsigned_long"}}})
    assert _put(client, idx, {"f": 18446744073709551615}, "max").status_code == 201
    assert _put(client, idx, {"f": 0}, "zero").status_code == 201
    assert _put(client, idx, {"f": "18446744073709551615"}, "s").status_code == 201
    assert _dv(client, idx, "f") == {
        "max": [18446744073709551615],
        "zero": [0],
        "s": [18446744073709551615],
    }


@pytest.mark.parametrize("ftype", ["byte", "short", "integer", "long", "unsigned_long"])
@pytest.mark.parametrize(
    "value,stored",
    [("5", 5), (5.7, 5), ("5.7", 5), ("1e2", 100), (100.0, 100)],
)
def test_integer_coercion(client, index_factory, ftype, value, stored):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    assert _put(client, idx, {"f": value}, "1").status_code == 201
    assert _dv(client, idx, "f") == {"1": [stored]}
    # _source keeps the original JSON value untouched
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"f": value}


@pytest.mark.parametrize(
    "ftype,accepted",
    [("byte", True), ("short", True), ("integer", True), ("long", False), ("unsigned_long", False)],
)
def test_integer_whitespace_padded_string(client, index_factory, ftype, accepted):
    """Small int types trim whitespace, long/unsigned_long do not."""
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    r = _put(client, idx, {"f": "  7 "}, "1")
    if accepted:
        assert r.status_code == 201
        assert _dv(client, idx, "f") == {"1": [7]}
    else:
        assert r.status_code == 400
        assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")


@pytest.mark.parametrize(
    "ftype,cause",
    [
        ("byte", "number_format_exception"),
        ("integer", "number_format_exception"),
        ("long", "illegal_argument_exception"),
        ("float", "number_format_exception"),
        ("double", "number_format_exception"),
        ("unsigned_long", "illegal_argument_exception"),
    ],
)
@pytest.mark.parametrize("value", ["abc", "0x10"])
def test_numeric_rejects_garbage_strings(client, index_factory, ftype, cause, value):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    r = _put(client, idx, {"f": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", cause)
    assert _cause_reason(r) == f'For input string: "{value}"'


@pytest.mark.parametrize("ftype", ["byte", "integer", "long", "float", "double", "unsigned_long"])
def test_numeric_rejects_boolean(client, index_factory, ftype):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    r = _put(client, idx, {"f": True})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "input_coercion_exception")


def test_numeric_empty_array_is_missing(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "integer"}}})
    assert _put(client, idx, {"f": []}, "1").status_code == 201
    assert _ids(client, idx, {"exists": {"field": "f"}}) == []


@pytest.mark.parametrize("ftype", ["float", "half_float", "double"])
@pytest.mark.parametrize("value", ["NaN", "Infinity", "-Infinity", "1e400"])
def test_float_types_reject_non_finite(client, index_factory, ftype, value):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    r = _put(client, idx, {"f": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r).startswith(f"[{ftype}] supports only finite values, but got [")


@pytest.mark.parametrize(
    "ftype,value,ok",
    [
        ("float", 3.4e38, True),
        ("float", 3.5e38, False),
        ("float", 1e308, False),
        ("double", 1e308, True),
        ("half_float", 65504, True),
        ("half_float", 65520, False),
        ("half_float", 70000, False),
    ],
)
def test_float_type_magnitude_limits(client, index_factory, ftype, value, ok):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    r = _put(client, idx, {"f": value})
    assert r.status_code == (201 if ok else 400)


@pytest.mark.parametrize(
    "ftype,expected",
    [("float", 0.10000000149011612), ("half_float", 0.0999755859375), ("double", 0.1)],
)
def test_float_precision_in_doc_values(client, index_factory, ftype, expected):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    _put(client, idx, {"f": 0.1}, "1")
    _put(client, idx, {"f": "0.1"}, "2")
    assert _dv(client, idx, "f") == {"1": [expected], "2": [expected]}
    src = {h["_id"]: h["_source"]["f"] for h in _ok_search(client, idx, {})["hits"]["hits"]}
    assert src == {"1": 0.1, "2": "0.1"}


@pytest.mark.parametrize(
    "ftype,term_hits,point_range_hits",
    [("float", 1, 1), ("double", 1, 1), ("half_float", 1, 0)],
)
def test_float_precision_queries(client, index_factory, ftype, term_hits, point_range_hits):
    """term on 0.1 matches for all; an exact gte/lte 0.1 range misses on half_float."""
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype}}})
    _put(client, idx, {"f": 0.1}, "1")
    assert len(_ids(client, idx, {"term": {"f": 0.1}})) == term_hits
    assert len(_ids(client, idx, {"term": {"f": "0.1"}})) == term_hits
    assert len(_ids(client, idx, {"range": {"f": {"gte": 0.1, "lte": 0.1}}})) == point_range_hits


@pytest.mark.parametrize(
    "op,half_hits,double_hits",
    [("gt", [], []), ("gte", [], ["1"]), ("lt", ["1"], []), ("lte", ["1"], ["1"])],
)
def test_half_float_rounded_value_below_query_bound(client, index_factory, op, half_hits, double_hits):
    """0.1 is stored as half_float 0.0999755859375 (< 0.1): range bounds see the rounded value."""
    idx = _mk(index_factory, {"properties": {"f": {"type": "half_float"}, "d": {"type": "double"}}})
    _put(client, idx, {"f": 0.1, "d": 0.1}, "1")
    assert _ids(client, idx, {"range": {"f": {op: 0.1}}}) == half_hits
    assert _ids(client, idx, {"range": {"d": {op: 0.1}}}) == double_hits


@pytest.mark.parametrize(
    "value,stored",
    [(1.234, 1.23), (1.235, 1.24), (1.236, 1.24), ("2.5", 2.5), (-1.005, -1.0), (10.0, 10.0), (1.0049, 1.0)],
)
def test_scaled_float_rounding(client, index_factory, value, stored):
    idx = _mk(index_factory, {"properties": {"f": {"type": "scaled_float", "scaling_factor": 100}}})
    assert _put(client, idx, {"f": value}, "1").status_code == 201
    assert _dv(client, idx, "f") == {"1": [stored]}
    assert client.get(f"/{idx}/_doc/1").json()["_source"]["f"] == value


def test_scaled_float_queries_are_scaled_too(client, index_factory, bulk_index):
    idx = _mk(index_factory, {"properties": {"f": {"type": "scaled_float", "scaling_factor": 100}}})
    bulk_index(idx, [{"f": 1.234}, {"f": 1.235}, {"f": 1.236}, {"f": 10}])
    assert _ok_search(client, idx, {"query": {"term": {"f": 1.23}}})["hits"]["total"]["value"] == 1
    # the query value 1.234 is rounded to 1.23 as well
    assert _ok_search(client, idx, {"query": {"term": {"f": 1.234}}})["hits"]["total"]["value"] == 1
    assert _ok_search(client, idx, {"query": {"range": {"f": {"gte": 1.23, "lte": 1.24}}}})["hits"]["total"]["value"] == 3
    aggs = _ok_search(client, idx, {"size": 0, "aggs": {"s": {"sum": {"field": "f"}}, "m": {"max": {"field": "f"}}}})
    assert aggs["aggregations"]["s"]["value"] == pytest.approx(13.71)
    assert aggs["aggregations"]["m"]["value"] == 10.0


@pytest.mark.parametrize(
    "mapping,reason",
    [
        ({"type": "scaled_float"}, "Field [scaling_factor] is required"),
        ({"type": "scaled_float", "scaling_factor": 0}, "[scaling_factor] must be a positive number, got [0.0]"),
        ({"type": "scaled_float", "scaling_factor": -10}, "[scaling_factor] must be a positive number, got [-10.0]"),
    ],
)
def test_scaled_float_invalid_mapping(try_create, mapping, reason):
    r = try_create({"properties": {"f": mapping}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == reason


@pytest.mark.parametrize(
    "ftype,value,cause,reason",
    [
        ("integer", "5", "illegal_argument_exception", "Integer value passed as String"),
        ("integer", 5.7, "illegal_argument_exception", "5.7 cannot be converted to Integer without data loss"),
        ("float", "1.5", "illegal_argument_exception", "Float value passed as String"),
    ],
)
def test_coerce_false_rejects(client, index_factory, ftype, value, cause, reason):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype, "coerce": False}}})
    r = _put(client, idx, {"f": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", cause)
    assert _cause_reason(r) == reason


@pytest.mark.parametrize("ftype,value", [("integer", 5.0), ("integer", 5), ("float", 1), ("float", 1.5)])
def test_coerce_false_accepts_lossless_json_numbers(client, index_factory, ftype, value):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype, "coerce": False}}})
    assert _put(client, idx, {"f": value}).status_code == 201


def test_index_level_coerce_setting(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "integer"}, "g": {"type": "integer", "coerce": True}}},
              {"index.mapping.coerce": False})
    r = _put(client, idx, {"f": "5"})
    assert r.status_code == 400
    assert _put(client, idx, {"g": "5"}).status_code == 201


@pytest.mark.parametrize("bad", ["abc", 128000000000, True])
def test_ignore_malformed_numeric(client, index_factory, bad):
    idx = _mk(index_factory, {"properties": {"f": {"type": "integer", "ignore_malformed": True}, "k": {"type": "keyword"}}})
    assert _put(client, idx, {"f": bad, "k": "x"}, "1").status_code == 201
    doc = client.get(f"/{idx}/_doc/1").json()
    assert doc["_ignored"] == ["f"]
    assert doc["_source"] == {"f": bad, "k": "x"}
    assert _ids(client, idx, {"exists": {"field": "f"}}) == []
    assert _ids(client, idx, {"term": {"_ignored": "f"}}) == ["1"]
    assert _ids(client, idx, {"term": {"k": "x"}}) == ["1"]


def test_ignore_malformed_object_value_silently_dropped(client, index_factory):
    """An object for an ignore_malformed numeric is accepted but NOT listed in _ignored."""
    idx = _mk(index_factory, {"properties": {"f": {"type": "integer", "ignore_malformed": True}}})
    assert _put(client, idx, {"f": {"x": 1}}, "1").status_code == 201
    hit = _ok_search(client, idx, {})["hits"]["hits"][0]
    assert "_ignored" not in hit
    assert _ids(client, idx, {"exists": {"field": "_ignored"}}) == []


def test_ignore_malformed_index_setting_and_field_override(client, index_factory):
    idx = _mk(
        index_factory,
        {"properties": {"f": {"type": "integer"}, "d": {"type": "date"}, "g": {"type": "integer", "ignore_malformed": False}}},
        {"index.mapping.ignore_malformed": True},
    )
    assert _put(client, idx, {"f": "x", "d": "y"}, "1").status_code == 201
    assert client.get(f"/{idx}/_doc/1").json()["_ignored"] == ["d", "f"]
    r = _put(client, idx, {"g": "x"}, "2")
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "number_format_exception")


def test_ignored_field_has_no_fielddata(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "integer", "ignore_malformed": True}}})
    _put(client, idx, {"f": "bad"})
    r = _search(client, idx, {"size": 0, "aggs": {"i": {"terms": {"field": "_ignored"}}}})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "illegal_argument_exception")
    assert _cause_reason(r) == "Fielddata is not supported on field [_ignored] of type [_ignored]"


@pytest.mark.parametrize(
    "field,query_value",
    [("f", -1), ("k", "NULL"), ("b", True), ("d", "2000-01-01"), ("ip", "0.0.0.0")],
)
def test_null_value_indexed_for_explicit_null(client, index_factory, field, query_value):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "f": {"type": "integer", "null_value": -1},
                "k": {"type": "keyword", "null_value": "NULL"},
                "b": {"type": "boolean", "null_value": True},
                "d": {"type": "date", "null_value": "2000-01-01"},
                "ip": {"type": "ip", "null_value": "0.0.0.0"},
            }
        },
    )
    _put(client, idx, {"f": None, "k": None, "b": None, "d": None, "ip": None}, "null")
    _put(client, idx, {"f": [], "k": []}, "empty")
    _put(client, idx, {}, "missing")
    assert _ids(client, idx, {"term": {field: query_value}}) == ["null"]
    assert _ids(client, idx, {"exists": {"field": field}}) == ["null"]
    # source still holds the explicit null
    assert client.get(f"/{idx}/_doc/null").json()["_source"][field] is None


def test_null_value_inside_array(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "integer", "null_value": -1}}})
    _put(client, idx, {"f": [None, 5]}, "1")
    assert _ids(client, idx, {"term": {"f": -1}}) == ["1"]
    assert _ids(client, idx, {"term": {"f": 5}}) == ["1"]


def test_null_value_must_match_field_type(try_create):
    r = try_create({"properties": {"f": {"type": "integer", "null_value": "abc"}}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "number_format_exception")


def test_text_has_no_null_value(try_create):
    r = try_create({"properties": {"f": {"type": "text", "null_value": "x"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "unknown parameter [null_value] on mapper [f] of type [text]"


NUMERIC_SORT_CASES = [
    ("byte", [5, -3, 100]),
    ("short", [300, -2, 7]),
    ("integer", [10, -20, 3]),
    ("long", [2**40, -5, 0]),
    ("float", [1.5, -0.25, 3.75]),
    ("half_float", [1.5, -0.25, 3.75]),
    ("double", [1.5, -0.25, 3.75]),
    ("scaled_float", [1.5, -0.25, 3.75]),
    ("unsigned_long", [2**63 + 5, 1, 2**64 - 1]),
]


def _numeric_index(client, index_factory, ftype, vals):
    m = {"type": ftype}
    if ftype == "scaled_float":
        m["scaling_factor"] = 100
    idx = _mk(index_factory, {"properties": {"f": m}})
    for i, v in enumerate(vals):
        _put(client, idx, {"f": v}, str(i))
    return idx


@pytest.mark.parametrize("ftype,vals", NUMERIC_SORT_CASES)
@pytest.mark.parametrize("order", ["asc", "desc"])
def test_numeric_sort(client, index_factory, ftype, vals, order):
    idx = _numeric_index(client, index_factory, ftype, vals)
    res = _ok_search(client, idx, {"sort": [{"f": order}]})
    got = [h["sort"][0] for h in res["hits"]["hits"]]
    assert got == sorted(vals, reverse=(order == "desc"))
    if ftype not in ("float", "half_float", "double", "scaled_float"):
        assert all(isinstance(v, int) for v in got)


@pytest.mark.parametrize("ftype,vals", NUMERIC_SORT_CASES)
def test_numeric_terms_agg_and_queries(client, index_factory, ftype, vals):
    idx = _numeric_index(client, index_factory, ftype, vals)
    res = _ok_search(client, idx, {"size": 0, "aggs": {"t": {"terms": {"field": "f"}}}})
    keys = [b["key"] for b in res["aggregations"]["t"]["buckets"]]
    assert sorted(keys) == sorted(vals)
    assert _ids(client, idx, {"term": {"f": vals[0]}}) == ["0"]
    assert _ids(client, idx, {"terms": {"f": vals[:2]}}) == ["0", "1"]
    expected_nonneg = sorted(str(i) for i, v in enumerate(vals) if v >= 0)
    assert _ids(client, idx, {"range": {"f": {"gte": 0}}}) == expected_nonneg


@pytest.mark.parametrize("ftype,vals", [c for c in NUMERIC_SORT_CASES if c[0] != "unsigned_long"])
def test_numeric_stats_agg(client, index_factory, ftype, vals):
    idx = _numeric_index(client, index_factory, ftype, vals)
    res = _ok_search(client, idx, {"size": 0, "aggs": {"s": {"stats": {"field": "f"}}}})
    s = res["aggregations"]["s"]
    assert s["count"] == 3
    assert s["min"] == pytest.approx(min(vals))
    assert s["max"] == pytest.approx(max(vals))
    assert s["sum"] == pytest.approx(sum(vals))


def test_unsigned_long_range_beyond_signed_long(client, index_factory):
    idx = _numeric_index(client, index_factory, "unsigned_long", [2**63 + 5, 1, 2**64 - 1])
    assert _ids(client, idx, {"range": {"f": {"gt": 2**63 - 1}}}) == ["0", "2"]
    assert _ids(client, idx, {"range": {"f": {"gte": "18446744073709551615"}}}) == ["2"]
    assert _ids(client, idx, {"term": {"f": "9223372036854775813"}}) == ["0"]


def test_long_precision_beyond_double(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "long"}}})
    _put(client, idx, {"f": 9007199254740993}, "1")
    assert _ids(client, idx, {"term": {"f": 9007199254740993}}) == ["1"]
    assert _ids(client, idx, {"term": {"f": 9007199254740992}}) == []
    assert _dv(client, idx, "f") == {"1": [9007199254740993]}


# =========================================================================== keyword


@pytest.fixture
def ignore_above_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"k": {"type": "keyword", "ignore_above": 5}}})
    _put(client, idx, {"k": "abc"}, "1")
    _put(client, idx, {"k": "abcdef"}, "2")
    _put(client, idx, {"k": ["ab", "toolongvalue"]}, "3")
    _put(client, idx, {"k": "abcde"}, "4")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"k": "abcdef"}}, []),
        ({"term": {"k": "abc"}}, ["1"]),
        ({"term": {"k": "abcde"}}, ["4"]),
        ({"term": {"k": "toolongvalue"}}, []),
        ({"term": {"k": "ab"}}, ["3"]),
        ({"exists": {"field": "k"}}, ["1", "3", "4"]),
        ({"prefix": {"k": "abc"}}, ["1", "4"]),
        # unlike ignore_malformed, ignore_above does not populate _ignored
        ({"exists": {"field": "_ignored"}}, []),
    ],
)
def test_keyword_ignore_above_queries(client, ignore_above_idx, query, expected):
    assert _ids(client, ignore_above_idx, query) == expected


def test_keyword_ignore_above_source_and_aggs(client, ignore_above_idx):
    idx = ignore_above_idx
    assert client.get(f"/{idx}/_doc/2").json()["_source"] == {"k": "abcdef"}
    assert "_ignored" not in client.get(f"/{idx}/_doc/2").json()
    res = _ok_search(client, idx, {"size": 0, "aggs": {"t": {"terms": {"field": "k"}}}})
    assert {b["key"] for b in res["aggregations"]["t"]["buckets"]} == {"ab", "abc", "abcde"}
    dv = _dv(client, idx, "k")
    assert dv["2"] is None
    assert dv["3"] == ["ab"]


def test_dynamic_string_keyword_subfield_ignore_above_256(client, index_factory):
    idx = _mk(index_factory)
    long_val = "x" * 257
    _put(client, idx, {"s": long_val}, "1")
    _put(client, idx, {"s": "x" * 256}, "2")
    assert _props(client, idx)["s"]["fields"]["keyword"] == {"type": "keyword", "ignore_above": 256}
    assert _ids(client, idx, {"term": {"s.keyword": long_val}}) == []
    assert _ids(client, idx, {"exists": {"field": "s.keyword"}}) == ["2"]
    assert _ids(client, idx, {"exists": {"field": "s"}}) == ["1", "2"]


@pytest.fixture
def normalizer_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {"properties": {"k": {"type": "keyword", "normalizer": "lc"}}},
        {"analysis": {"normalizer": {"lc": {"type": "custom", "filter": ["lowercase", "asciifolding"]}}}},
    )
    _put(client, idx, {"k": "Héllo World"}, "1")
    return idx


@pytest.mark.parametrize(
    "query",
    [
        {"term": {"k": "hello world"}},
        {"term": {"k": "HELLO WORLD"}},
        {"terms": {"k": ["HÉLLO WORLD"]}},
        {"match": {"k": "HÉLLO world"}},
        {"prefix": {"k": "HEL"}},
        {"wildcard": {"k": "HEL*"}},
    ],
)
def test_keyword_normalizer_applies_to_queries(client, normalizer_idx, query):
    assert _ids(client, normalizer_idx, query) == ["1"]


def test_keyword_normalizer_affects_doc_values_not_source(client, normalizer_idx):
    idx = normalizer_idx
    res = _ok_search(client, idx, {"size": 0, "aggs": {"t": {"terms": {"field": "k"}}}})
    assert res["aggregations"]["t"]["buckets"] == [{"key": "hello world", "doc_count": 1}]
    assert _dv(client, idx, "k") == {"1": ["hello world"]}
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"k": "Héllo World"}


def test_keyword_builtin_lowercase_normalizer(client, index_factory):
    idx = _mk(index_factory, {"properties": {"k": {"type": "keyword", "normalizer": "lowercase"}}})
    _put(client, idx, {"k": "ABC"}, "1")
    assert _ids(client, idx, {"term": {"k": "AbC"}}) == ["1"]


def test_keyword_unknown_normalizer(try_create):
    r = try_create({"properties": {"f": {"type": "keyword", "normalizer": "nope"}}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "mapper_parsing_exception")
    assert _cause_reason(r) == "normalizer [nope] not found for field [f]"


@pytest.fixture
def split_ws_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {"properties": {"k": {"type": "keyword", "split_queries_on_whitespace": True}, "k2": {"type": "keyword"}}},
    )
    for i, v in enumerate(["foo", "foo bar", "bar"], start=1):
        _put(client, idx, {"k": v, "k2": v}, str(i))
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"match": {"k": "foo bar"}}, ["1", "3"]),
        ({"match": {"k": {"query": "foo bar", "operator": "and"}}}, []),
        ({"term": {"k": "foo bar"}}, ["2"]),
        ({"query_string": {"query": "foo bar", "default_field": "k"}}, ["1", "3"]),
        ({"match": {"k2": "foo bar"}}, ["2"]),
        ({"match": {"k2": {"query": "foo bar", "operator": "and"}}}, ["2"]),
        ({"query_string": {"query": "foo bar", "default_field": "k2"}}, ["2"]),
        ({"match_phrase": {"k2": "foo bar"}}, ["2"]),
        # simple_query_string splits on whitespace itself, regardless of the mapping
        ({"simple_query_string": {"query": "foo bar", "fields": ["k2"]}}, ["1", "3"]),
    ],
)
def test_keyword_split_queries_on_whitespace(client, split_ws_idx, query, expected):
    assert _ids(client, split_ws_idx, query) == expected


def test_keyword_split_queries_phrase_fails(client, split_ws_idx):
    r = _search(client, split_ws_idx, {"query": {"match_phrase": {"k": "foo bar"}}})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
    assert _cause_reason(r) == "failed to create query: field:[k] was indexed without position data; cannot run PhraseQuery"


def test_keyword_eager_global_ordinals_roundtrip_and_agg(client, index_factory):
    idx = _mk(index_factory, {"properties": {"k": {"type": "keyword", "eager_global_ordinals": True}}})
    assert _props(client, idx)["k"] == {"type": "keyword", "eager_global_ordinals": True}
    _put(client, idx, {"k": "a"})
    _put(client, idx, {"k": "a"})
    res = _ok_search(client, idx, {"size": 0, "aggs": {"t": {"terms": {"field": "k"}}}})
    assert res["aggregations"]["t"]["buckets"] == [{"key": "a", "doc_count": 2}]


@pytest.mark.parametrize(
    "ftype,options,ok",
    [
        ("keyword", "docs", True),
        ("keyword", "freqs", True),
        ("keyword", "positions", False),
        ("text", "bogus", False),
    ],
)
def test_index_options_validation(try_create, ftype, options, ok):
    r = try_create({"properties": {"x": {"type": ftype, "index_options": options}}})
    if ok:
        assert r.status_code == 200
    else:
        assert r.status_code == 400
        assert _err(r) == ("mapper_parsing_exception", "mapper_parsing_exception")
        accepted = "[docs, freqs]" if ftype == "keyword" else "[positions, docs, freqs, offsets]"
        assert _cause_reason(r) == (
            f"Unknown value [{options}] for field [index_options] - accepted values are {accepted}"
        )


def test_keyword_rejects_object_value(client, index_factory):
    idx = _mk(index_factory, {"properties": {"k": {"type": "keyword"}}})
    r = _put(client, idx, {"k": {"a": 1}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_state_exception")


def test_keyword_nested_arrays_are_flattened(client, index_factory):
    idx = _mk(index_factory, {"properties": {"k": {"type": "keyword"}}})
    assert _put(client, idx, {"k": [["a", "b"], "c"]}, "1").status_code == 201
    assert sorted(_dv(client, idx, "k")["1"]) == ["a", "b", "c"]


# =========================================================================== text


@pytest.fixture
def index_options_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "docs": {"type": "text", "index_options": "docs"},
                "freqs": {"type": "text", "index_options": "freqs"},
                "positions": {"type": "text", "index_options": "positions"},
                "offsets": {"type": "text", "index_options": "offsets"},
            }
        },
    )
    text = "quick brown fox fox"
    _put(client, idx, {f: text for f in ("docs", "freqs", "positions", "offsets")}, "1")
    _put(client, idx, {f: "fox" for f in ("docs", "freqs", "positions", "offsets")}, "2")
    return idx


@pytest.mark.parametrize("field,phrase_ok", [("docs", False), ("freqs", False), ("positions", True), ("offsets", True)])
def test_text_index_options_phrase_support(client, index_options_idx, field, phrase_ok):
    r = _search(client, index_options_idx, {"query": {"match_phrase": {field: "brown fox"}}})
    if phrase_ok:
        assert r.status_code == 200
        assert [h["_id"] for h in r.json()["hits"]["hits"]] == ["1"]
    else:
        assert r.status_code == 400
        assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
        assert "was indexed without position data; cannot run PhraseQuery" in _cause_reason(r)


@pytest.mark.parametrize("field", ["docs", "freqs", "positions", "offsets"])
def test_text_index_options_match_and_highlight(client, index_options_idx, field):
    res = _ok_search(
        client,
        index_options_idx,
        {"query": {"match": {field: "fox"}}, "highlight": {"fields": {field: {}}}},
    )
    hits = res["hits"]["hits"]
    # shorter doc ranks first everywhere (norms enabled)
    assert [h["_id"] for h in hits] == ["2", "1"]
    hl = {h["_id"]: h["highlight"][field] for h in hits}
    assert hl == {"1": ["quick brown <em>fox</em> <em>fox</em>"], "2": ["<em>fox</em>"]}


def test_text_index_options_docs_ignores_term_frequency(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "d": {"type": "text", "index_options": "docs", "norms": False},
                "f": {"type": "text", "index_options": "freqs", "norms": False},
            }
        },
    )
    _put(client, idx, {"d": "fox fox fox dog", "f": "fox fox fox dog"}, "many")
    _put(client, idx, {"d": "fox cat cat dog", "f": "fox cat cat dog"}, "one")
    _put(client, idx, {"d": "zzz", "f": "zzz"}, "other")
    sd = {h["_id"]: h["_score"] for h in _ok_search(client, idx, {"query": {"match": {"d": "fox"}}})["hits"]["hits"]}
    sf = {h["_id"]: h["_score"] for h in _ok_search(client, idx, {"query": {"match": {"f": "fox"}}})["hits"]["hits"]}
    assert sd["many"] == pytest.approx(sd["one"])
    assert sf["many"] > sf["one"]


def test_text_norms_false_ignores_length(client, index_factory):
    idx = _mk(index_factory, {"properties": {"a": {"type": "text"}, "b": {"type": "text", "norms": False}}})
    _put(client, idx, {"a": "fox", "b": "fox"}, "1")
    long_text = "fox one two three four five six"
    _put(client, idx, {"a": long_text, "b": long_text}, "2")
    sa = {h["_id"]: h["_score"] for h in _ok_search(client, idx, {"query": {"match": {"a": "fox"}}})["hits"]["hits"]}
    sb = {h["_id"]: h["_score"] for h in _ok_search(client, idx, {"query": {"match": {"b": "fox"}}})["hits"]["hits"]}
    assert sa["1"] > sa["2"]
    assert sb["1"] == pytest.approx(sb["2"])


@pytest.mark.parametrize(
    "field,slop,expected",
    [("a", 0, []), ("a", 99, []), ("a", 100, ["1"]), ("b", 0, ["1"])],
)
def test_text_position_increment_gap(client, index_factory, field, slop, expected):
    idx = _mk(index_factory, {"properties": {"a": {"type": "text"}, "b": {"type": "text", "position_increment_gap": 0}}})
    _put(client, idx, {"a": ["john smith", "alice jones"], "b": ["john smith", "alice jones"]}, "1")
    q = {"match_phrase": {field: {"query": "smith alice", "slop": slop}}}
    assert _ids(client, idx, q) == expected


@pytest.mark.parametrize("op", ["agg", "sort"])
def test_text_fielddata_disabled_by_default(client, index_factory, op):
    idx = _mk(index_factory, {"properties": {"a": {"type": "text"}}})
    _put(client, idx, {"a": "x"})
    body = {"size": 0, "aggs": {"t": {"terms": {"field": "a"}}}} if op == "agg" else {"sort": ["a"]}
    r = _search(client, idx, body)
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "illegal_argument_exception")
    assert _cause_reason(r).startswith(
        "Text fields are not optimised for operations that require per-document field data"
    )
    assert "set fielddata=true on [a]" in _cause_reason(r)


def test_text_fielddata_true_aggs_on_terms(client, index_factory):
    idx = _mk(index_factory, {"properties": {"b": {"type": "text", "fielddata": True}}})
    _put(client, idx, {"b": "Zeta alpha"}, "1")
    _put(client, idx, {"b": "beta"}, "2")
    res = _ok_search(client, idx, {"size": 0, "aggs": {"t": {"terms": {"field": "b"}}}})
    assert [(b["key"], b["doc_count"]) for b in res["aggregations"]["t"]["buckets"]] == [
        ("alpha", 1),
        ("beta", 1),
        ("zeta", 1),
    ]


@pytest.mark.parametrize("order,expected", [("asc", [("1", "alpha"), ("2", "beta")]), ("desc", [("1", "zeta"), ("2", "beta")])])
def test_text_fielddata_sort_uses_min_max_term(client, index_factory, order, expected):
    idx = _mk(index_factory, {"properties": {"b": {"type": "text", "fielddata": True}}})
    _put(client, idx, {"b": "Zeta alpha"}, "1")
    _put(client, idx, {"b": "beta"}, "2")
    res = _ok_search(client, idx, {"sort": [{"b": order}]})
    assert [(h["_id"], h["sort"][0]) for h in res["hits"]["hits"]] == expected


@pytest.fixture
def prefix_phrase_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "t": {"type": "text", "index_prefixes": {"min_chars": 2, "max_chars": 4}},
                "ph": {"type": "text", "index_phrases": True},
            }
        },
    )
    _put(client, idx, {"t": "elasticsearch opensearch", "ph": "the quick brown fox"}, "1")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"prefix": {"t": "op"}}, ["1"]),
        ({"prefix": {"t": "opens"}}, ["1"]),
        ({"prefix": {"t": "o"}}, ["1"]),
        ({"match_phrase_prefix": {"t": "elasticsearch ope"}}, ["1"]),
        ({"term": {"t._index_prefix": "ope"}}, ["1"]),
        ({"term": {"t._index_prefix": "opens"}}, []),
        ({"match_phrase": {"ph": "quick brown"}}, ["1"]),
        ({"term": {"ph._index_phrase": "quick brown"}}, ["1"]),
        ({"match_phrase": {"ph": "quick fox"}}, []),
    ],
)
def test_text_index_prefixes_and_phrases(client, prefix_phrase_idx, query, expected):
    assert _ids(client, prefix_phrase_idx, query) == expected


def test_text_index_prefixes_mapping_roundtrip(client, prefix_phrase_idx):
    props = _props(client, prefix_phrase_idx)
    assert props["t"] == {"type": "text", "index_prefixes": {"min_chars": 2, "max_chars": 4}}
    assert props["ph"] == {"type": "text", "index_phrases": True}


@pytest.mark.parametrize(
    "prefixes,reason",
    [
        ({"min_chars": 0}, "min_chars [0] must be greater than zero"),
        ({"min_chars": 3, "max_chars": 2}, "min_chars [3] must be less than max_chars [2]"),
        ({"max_chars": 25}, "max_chars [25] must be less than 20"),
    ],
)
def test_text_index_prefixes_validation(try_create, prefixes, reason):
    r = try_create({"properties": {"x": {"type": "text", "index_prefixes": prefixes}}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == reason


def test_text_term_vector_with_positions_offsets(client, index_factory):
    idx = _mk(index_factory, {"properties": {"t": {"type": "text", "term_vector": "with_positions_offsets"}}})
    _put(client, idx, {"t": "fox fox dog"}, "1")
    tv = client.get(f"/{idx}/_termvectors/1", params={"fields": "t"}).json()["term_vectors"]["t"]
    assert tv["field_statistics"] == {"sum_doc_freq": 2, "doc_count": 1, "sum_ttf": 3}
    assert tv["terms"]["fox"] == {
        "term_freq": 2,
        "tokens": [
            {"position": 0, "start_offset": 0, "end_offset": 3},
            {"position": 1, "start_offset": 4, "end_offset": 7},
        ],
    }
    assert tv["terms"]["dog"]["tokens"] == [{"position": 2, "start_offset": 8, "end_offset": 11}]


def test_text_term_vector_invalid_value(try_create):
    r = try_create({"properties": {"x": {"type": "text", "term_vector": "bogus"}}})
    assert r.status_code == 400
    assert _cause_reason(r).startswith("Unknown value [bogus] for field [term_vector] - accepted values are [no, yes,")


def test_text_doc_values_parameter_accepted(try_create):
    assert try_create({"properties": {"t": {"type": "text", "doc_values": False}}}).status_code == 200


# =========================================================================== date


@pytest.mark.parametrize(
    "value,iso",
    [
        ("2024-01-15", "2024-01-15T00:00:00.000Z"),
        ("2024-01-15T10:20:30Z", "2024-01-15T10:20:30.000Z"),
        ("2024-01-15T10:20:30.123+02:00", "2024-01-15T08:20:30.123Z"),
        ("2024-01-15T10:20", "2024-01-15T10:20:00.000Z"),
        ("2024", "2024-01-01T00:00:00.000Z"),
        ("2024-01", "2024-01-01T00:00:00.000Z"),
        ("2024-02-29", "2024-02-29T00:00:00.000Z"),
        (1705312800000, "2024-01-15T10:00:00.000Z"),
        ("1705312800000", "2024-01-15T10:00:00.000Z"),
        (1.5e12, "2017-07-14T02:40:00.000Z"),
        ("1969-12-31", "1969-12-31T00:00:00.000Z"),
        ("1800-01-01", "1800-01-01T00:00:00.000Z"),
        ("0000-01-01", "0000-01-01T00:00:00.000Z"),
        ("-0001-01-01", "-0001-01-01T00:00:00.000Z"),
    ],
)
def test_date_default_format_accepts(client, index_factory, value, iso):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date"}}})
    assert _put(client, idx, {"d": value}, "1").status_code == 201
    assert _dv(client, idx, "d") == {"1": [iso]}


def test_date_negative_number_parsed_as_year_by_default(client, index_factory):
    """Quirk: with the default format, a negative JSON number is parsed as a year,
    not as negative epoch millis (strict_date_optional_time wins first)."""
    idx = _mk(index_factory, {"properties": {"d": {"type": "date"}}})
    _put(client, idx, {"d": -86400000}, "1")
    assert _dv(client, idx, "d") == {"1": ["-86400000-01-01T00:00:00.000Z"]}
    res = _ok_search(client, idx, {"sort": ["d"]})
    assert res["hits"]["hits"][0]["sort"] == [-2726582820019200000]


@pytest.mark.parametrize(
    "value",
    ["2024-01-15 10:20:30", "2024-13-01", "2024-02-30", "2023-02-29", "not a date", "+2024-01-01", "10000-01-01", "2024-01-15T25:00:00", True],
)
def test_date_default_format_rejects(client, index_factory, value):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date"}}})
    r = _put(client, idx, {"d": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    shown = "true" if value is True else value
    assert _cause_reason(r) == f"failed to parse date field [{shown}] with format [strict_date_optional_time||epoch_millis]"


def test_date_empty_string_rejected(client, index_factory):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date"}}})
    r = _put(client, idx, {"d": ""})
    assert r.status_code == 400
    assert _cause_reason(r) == "cannot parse empty date"


@pytest.mark.parametrize(
    "fmt,value,iso",
    [
        ("epoch_millis", -86400000, "1969-12-31T00:00:00.000Z"),
        ("epoch_millis", "-86400000", "1969-12-31T00:00:00.000Z"),
        ("epoch_millis", 1.5, "1970-01-01T00:00:00.001Z"),
        ("epoch_millis", "1705312800000.5", "2024-01-15T10:00:00.000Z"),
        ("epoch_second", 1705312800, "2024-01-15T10:00:00.000Z"),
        ("epoch_second", "1705312800", "2024-01-15T10:00:00.000Z"),
        ("epoch_second", -1, "1969-12-31T23:59:59.000Z"),
        ("epoch_second", 1705312800.5, "2024-01-15T10:00:00.500Z"),
        ("yyyy/MM/dd", "2024/01/15", "2024-01-15T00:00:00.000Z"),
        ("yyyy/MM/dd||epoch_millis", 86400000, "1970-01-02T00:00:00.000Z"),
        ("yyyy/MM/dd||epoch_millis", "2024/01/15", "2024-01-15T00:00:00.000Z"),
        ("strict_date", "2024-01-15", "2024-01-15T00:00:00.000Z"),
        ("date", "2024-1-5", "2024-01-05T00:00:00.000Z"),
        ("basic_date", "20240115", "2024-01-15T00:00:00.000Z"),
        ("dd.MM.yyyy HH:mm", "15.01.2024 13:45", "2024-01-15T13:45:00.000Z"),
    ],
)
def test_date_custom_formats_accept(client, index_factory, fmt, value, iso):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date", "format": fmt}}})
    assert _put(client, idx, {"d": value}, "1").status_code == 201
    assert _dv(client, idx, "d", fmt="strict_date_optional_time") == {"1": [iso]}


@pytest.mark.parametrize(
    "fmt,value",
    [
        ("epoch_millis", "2024-01-01"),
        ("epoch_second", "2024-01-01"),
        ("yyyy/MM/dd", "2024-01-15"),
        ("yyyy/MM/dd", "2024/1/5"),
        ("yyyy/MM/dd", 20240115),
        ("yyyy/MM/dd||epoch_millis", "2024-01-15"),
        ("strict_date", "2024-1-15"),
        ("strict_date", "2024-01-15T00:00:00Z"),
        ("basic_date", "2024-01-15"),
        ("dd.MM.yyyy HH:mm", "15.01.2024"),
    ],
)
def test_date_custom_formats_reject(client, index_factory, fmt, value):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date", "format": fmt}}})
    r = _put(client, idx, {"d": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == f"failed to parse date field [{value}] with format [{fmt}]"


def test_date_custom_format_docvalue_default_output_uses_mapping_format(client, index_factory):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date", "format": "yyyy/MM/dd"}}})
    _put(client, idx, {"d": "2024/01/15"}, "1")
    assert _dv(client, idx, "d") == {"1": ["2024/01/15"]}


def test_date_invalid_format_in_mapping(try_create):
    r = try_create({"properties": {"d": {"type": "date", "format": "bogus_fmt"}}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == "Error parsing [format] on field [d]: Invalid format: [bogus_fmt]: Unknown pattern letter: b"


@pytest.fixture
def tz_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date"}}})
    _put(client, idx, {"d": "2024-01-15T23:30:00Z"}, "a")
    _put(client, idx, {"d": "2024-01-16T00:30:00Z"}, "b")
    _put(client, idx, {"d": "2024-01-15T10:00:00+05:00"}, "c")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"range": {"d": {"gte": "2024-01-16", "lt": "2024-01-17"}}}, ["b"]),
        ({"range": {"d": {"gte": "2024-01-16", "lt": "2024-01-17", "time_zone": "+01:00"}}}, ["a", "b"]),
        ({"range": {"d": {"gte": "2024-01-16T00:00:00+01:00"}}}, ["a", "b"]),
        ({"range": {"d": {"gte": "2024-01-15T05:00:00Z", "lte": "2024-01-15T05:00:00Z"}}}, ["c"]),
        ({"range": {"d": {"gte": "2024-01-15T10:00:00+05:00", "lte": "2024-01-15T10:00:00+05:00"}}}, ["c"]),
        ({"term": {"d": "2024-01-15T05:00:00Z"}}, ["c"]),
        ({"term": {"d": "2024-01-15T10:00:00+05:00"}}, ["c"]),
        ({"range": {"d": {"gte": "2024/01/16", "format": "yyyy/MM/dd"}}}, ["b"]),
        ({"range": {"d": {"gte": 1705363200000}}}, ["b"]),
        ({"range": {"d": {"gte": "2024-01-15||+1d"}}}, ["b"]),
        ({"range": {"d": {"gte": "2024-01-15T23:30:00Z||/d", "lt": "2024-01-16||/d"}}}, ["a", "c"]),
        # day-precision upper bounds round up to the end of the day
        ({"range": {"d": {"lte": "2024-01-15"}}}, ["a", "c"]),
        ({"range": {"d": {"lt": "2024-01-15"}}}, []),
        ({"range": {"d": {"gt": "2024-01-15"}}}, ["b"]),
        ({"range": {"d": {"gt": "2024-01-15", "lte": "2024-01-15"}}}, []),
        # a term on a date with day precision matches the whole day
        ({"term": {"d": "2024-01-15"}}, ["a", "c"]),
        ({"range": {"d": {"gte": "now-100y"}}}, ["a", "b", "c"]),
    ],
)
def test_date_range_timezones_and_rounding(client, tz_idx, query, expected):
    assert _ids(client, tz_idx, query) == expected


def test_date_range_unparseable_bound(client, tz_idx):
    r = _search(client, tz_idx, {"query": {"range": {"d": {"gte": "bad"}}}})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "parse_exception")


def test_date_histogram_respects_time_zone(client, tz_idx):
    res = _ok_search(
        client,
        tz_idx,
        {
            "size": 0,
            "aggs": {
                "utc": {"date_histogram": {"field": "d", "calendar_interval": "day"}},
                "tz": {"date_histogram": {"field": "d", "calendar_interval": "day", "time_zone": "-02:00"}},
            },
        },
    )
    utc = [(b["key_as_string"], b["doc_count"]) for b in res["aggregations"]["utc"]["buckets"]]
    assert utc == [("2024-01-15T00:00:00.000Z", 2), ("2024-01-16T00:00:00.000Z", 1)]
    tz = res["aggregations"]["tz"]["buckets"]
    assert [(b["key_as_string"], b["key"], b["doc_count"]) for b in tz] == [
        ("2024-01-15T00:00:00.000-02:00", 1705284000000, 3)
    ]


def test_date_docvalue_formats(client, tz_idx):
    res = _ok_search(
        client,
        tz_idx,
        {
            "query": {"ids": {"values": ["c"]}},
            "docvalue_fields": ["d", {"field": "d", "format": "epoch_millis"}, {"field": "d", "format": "yyyy/MM/dd HH:mm"}],
        },
    )
    assert res["hits"]["hits"][0]["fields"]["d"] == ["2024-01-15T05:00:00.000Z", "1705294800000", "2024/01/15 05:00"]
    # _source keeps the original offset
    assert res["hits"]["hits"][0]["_source"]["d"] == "2024-01-15T10:00:00+05:00"


def test_date_sort_values_are_epoch_millis(client, tz_idx):
    res = _ok_search(client, tz_idx, {"sort": [{"d": "asc"}]})
    assert [(h["_id"], h["sort"][0]) for h in res["hits"]["hits"]] == [
        ("c", 1705294800000),
        ("a", 1705361400000),
        ("b", 1705365000000),
    ]


@pytest.fixture
def nanos_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date_nanos"}}})
    for i, v in enumerate(
        ["2024-01-15T10:00:00.123456789Z", "2024-01-15T10:00:00.123456788Z", "2024-01-15T10:00:00.123Z", 1705312800123]
    ):
        _put(client, idx, {"d": v}, str(i))
    return idx


def test_date_nanos_sort_keeps_nanoseconds(client, nanos_idx):
    res = _ok_search(client, nanos_idx, {"sort": [{"d": "desc"}, {"_id": "asc"}]})
    assert [(h["_id"], h["sort"][0]) for h in res["hits"]["hits"]] == [
        ("0", 1705312800123456789),
        ("1", 1705312800123456788),
        ("2", 1705312800123000000),
        ("3", 1705312800123000000),
    ]


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"d": "2024-01-15T10:00:00.123456789Z"}}, ["0"]),
        ({"range": {"d": {"gt": "2024-01-15T10:00:00.123456788Z"}}}, ["0"]),
        ({"range": {"d": {"gte": "2024-01-15T10:00:00.123Z", "lte": "2024-01-15T10:00:00.123Z"}}}, ["2", "3"]),
        ({"range": {"d": {"gte": "2024-01-15T10:00:00.123Z", "lt": "2024-01-15T10:00:00.124Z"}}}, ["0", "1", "2", "3"]),
    ],
)
def test_date_nanos_queries(client, nanos_idx, query, expected):
    assert _ids(client, nanos_idx, query) == expected


def test_date_nanos_docvalue_formats(client, nanos_idx):
    # default output format truncates to milliseconds
    assert _dv(client, nanos_idx, "d", doc_id="0") == {"0": ["2024-01-15T10:00:00.123Z"]}
    assert _dv(client, nanos_idx, "d", doc_id="0", fmt="strict_date_optional_time_nanos") == {
        "0": ["2024-01-15T10:00:00.123456789Z"]
    }


def test_date_millis_truncates_nanos(client, index_factory):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date"}}})
    _put(client, idx, {"d": "2024-01-15T10:00:00.123456789Z"}, "1")
    assert _dv(client, idx, "d", fmt="strict_date_optional_time_nanos") == {"1": ["2024-01-15T10:00:00.123Z"]}
    assert _ok_search(client, idx, {"sort": ["d"]})["hits"]["hits"][0]["sort"] == [1705312800123]


@pytest.mark.parametrize(
    "value,reason",
    [
        ("1969-12-31T00:00:00Z", "date[1969-12-31T00:00:00Z] is before the epoch in 1970 and cannot be stored in nanosecond resolution"),
        ("2262-04-12T00:00:00Z", "date[2262-04-12T00:00:00Z] is after 2262-04-11T23:47:16.854775807 and cannot be stored in nanosecond resolution"),
    ],
)
def test_date_nanos_out_of_range(client, index_factory, value, reason):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date_nanos"}}})
    r = _put(client, idx, {"d": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == reason


def test_date_nanos_max_value(client, index_factory):
    idx = _mk(index_factory, {"properties": {"d": {"type": "date_nanos"}}})
    assert _put(client, idx, {"d": "2262-04-11T23:47:16.854775807Z"}, "1").status_code == 201
    assert _ok_search(client, idx, {"sort": ["d"]})["hits"]["hits"][0]["sort"] == [9223372036854775807]


# =========================================================================== boolean


@pytest.mark.parametrize("value,stored", [(True, True), (False, False), ("true", True), ("false", False), ("", False)])
def test_boolean_accepted_values(client, index_factory, value, stored):
    idx = _mk(index_factory, {"properties": {"b": {"type": "boolean"}}})
    assert _put(client, idx, {"b": value}, "1").status_code == 201
    assert _dv(client, idx, "b") == {"1": [stored]}


@pytest.mark.parametrize("value", ["True", "FALSE", "yes", "on", "off", "1"])
def test_boolean_rejected_strings(client, index_factory, value):
    idx = _mk(index_factory, {"properties": {"b": {"type": "boolean"}}})
    r = _put(client, idx, {"b": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == f"Failed to parse value [{value}] as only [true] or [false] are allowed."


@pytest.mark.parametrize("value", [0, 1])
def test_boolean_rejects_numbers(client, index_factory, value):
    idx = _mk(index_factory, {"properties": {"b": {"type": "boolean"}}})
    r = _put(client, idx, {"b": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "input_coercion_exception")


@pytest.fixture
def bool_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"b": {"type": "boolean"}}})
    for i, v in enumerate([True, False, "true", "false", "", None, ["true", False]]):
        _put(client, idx, {"b": v}, str(i))
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"b": True}}, ["0", "2", "6"]),
        ({"term": {"b": "true"}}, ["0", "2", "6"]),
        ({"term": {"b": "false"}}, ["1", "3", "4", "6"]),
        ({"range": {"b": {"gte": True}}}, ["0", "2", "6"]),
        ({"exists": {"field": "b"}}, ["0", "1", "2", "3", "4", "6"]),
    ],
)
def test_boolean_queries(client, bool_idx, query, expected):
    assert _ids(client, bool_idx, query) == expected


@pytest.mark.parametrize("value", ["", "yes", 1])
def test_boolean_term_query_rejects_non_boolean(client, bool_idx, value):
    r = _search(client, bool_idx, {"query": {"term": {"b": value}}})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
    assert _cause_reason(r) == f"failed to create query: Can't parse boolean value [{value}], expected [true] or [false]"


def test_boolean_terms_agg_and_sort(client, bool_idx):
    res = _ok_search(client, bool_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "b"}}}})
    assert res["aggregations"]["t"]["buckets"] == [
        {"key": 0, "key_as_string": "false", "doc_count": 4},
        {"key": 1, "key_as_string": "true", "doc_count": 3},
    ]
    res = _ok_search(client, bool_idx, {"sort": [{"b": "asc"}, {"_id": "asc"}], "size": 20})
    sorts = [(h["_id"], h["sort"][0]) for h in res["hits"]["hits"]]
    assert sorts[:6] == [("1", 0), ("3", 0), ("4", 0), ("6", 0), ("0", 1), ("2", 1)]
    assert sorts[6][0] == "5"


# =========================================================================== ip


@pytest.fixture
def ip_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"ip": {"type": "ip"}}})
    vals = ["192.168.1.10", "192.168.2.1", "10.0.0.1", "::1", "2001:db8::1", "::ffff:192.168.1.11", "2001:DB8:0:0:0:0:0:2"]
    for i, v in enumerate(vals):
        _put(client, idx, {"ip": v}, str(i))
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"ip": "192.168.1.0/24"}}, ["0", "5"]),
        ({"term": {"ip": "192.168.0.0/16"}}, ["0", "1", "5"]),
        ({"term": {"ip": "2001:db8::/32"}}, ["4", "6"]),
        ({"term": {"ip": "::ffff:192.168.1.10"}}, ["0"]),
        ({"term": {"ip": "192.168.1.11"}}, ["5"]),
        ({"term": {"ip": "2001:db8::2"}}, ["6"]),
        ({"terms": {"ip": ["10.0.0.1", "::1"]}}, ["2", "3"]),
        ({"range": {"ip": {"gte": "10.0.0.0", "lte": "192.168.1.255"}}}, ["0", "2", "5"]),
        ({"range": {"ip": {"gte": "::"}}}, ["0", "1", "2", "3", "4", "5", "6"]),
    ],
)
def test_ip_queries(client, ip_idx, query, expected):
    assert _ids(client, ip_idx, query) == expected


def test_ip_sort_and_canonical_doc_values(client, ip_idx):
    res = _ok_search(client, ip_idx, {"sort": [{"ip": "asc"}], "docvalue_fields": ["ip"]})
    assert [(h["_id"], h["sort"][0]) for h in res["hits"]["hits"]] == [
        ("3", "::1"),
        ("2", "10.0.0.1"),
        ("0", "192.168.1.10"),
        ("5", "192.168.1.11"),
        ("1", "192.168.2.1"),
        ("4", "2001:db8::1"),
        ("6", "2001:db8::2"),
    ]
    assert {h["_id"]: h["fields"]["ip"][0] for h in res["hits"]["hits"]}["5"] == "192.168.1.11"


def test_ip_range_agg_and_terms(client, ip_idx):
    res = _ok_search(
        client,
        ip_idx,
        {
            "size": 0,
            "aggs": {
                "r": {"ip_range": {"field": "ip", "ranges": [{"mask": "192.168.0.0/16"}, {"to": "11.0.0.0"}]}},
                "t": {"terms": {"field": "ip", "size": 20}},
            },
        },
    )
    r = res["aggregations"]["r"]["buckets"]
    assert {b["key"]: b["doc_count"] for b in r} == {"*-11.0.0.0": 2, "192.168.0.0/16": 3}
    assert {b["key"] for b in res["aggregations"]["t"]["buckets"]} == {
        "::1", "10.0.0.1", "192.168.1.10", "192.168.1.11", "192.168.2.1", "2001:db8::1", "2001:db8::2"
    }


@pytest.mark.parametrize("value", ["256.1.1.1", "192.168.1", "abc", 3232235777, "192.168.1.1/24", " 10.0.0.2"])
def test_ip_rejects_invalid(client, index_factory, value):
    idx = _mk(index_factory, {"properties": {"ip": {"type": "ip"}}})
    r = _put(client, idx, {"ip": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == f"'{value}' is not an IP string literal."


@pytest.mark.parametrize(
    "query,reason",
    [
        ({"term": {"ip": "bad"}}, "failed to create query: 'bad' is not an IP string literal."),
        (
            {"term": {"ip": "10.0.0.0/33"}},
            "failed to create query: Illegal prefix length [33] in [10.0.0.0/33]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges",
        ),
        ({"prefix": {"ip": "192"}}, "Can only use prefix queries on keyword and text fields - not on [ip] which is of type [ip]"),
        ({"wildcard": {"ip": "192*"}}, "Can only use wildcard queries on keyword and text fields - not on [ip] which is of type [ip]"),
    ],
)
def test_ip_invalid_queries(client, ip_idx, query, reason):
    r = _search(client, ip_idx, {"query": query})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
    assert _cause_reason(r) == reason


# =========================================================================== binary


def test_binary_not_searchable(client, index_factory):
    idx = _mk(index_factory, {"properties": {"b": {"type": "binary"}}})
    _put(client, idx, {"b": "aGVsbG8="}, "1")
    r = _search(client, idx, {"query": {"term": {"b": "aGVsbG8="}}})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
    assert _cause_reason(r) == "Binary fields do not support searching"
    # no doc values by default -> exists finds nothing
    assert _ids(client, idx, {"exists": {"field": "b"}}) == []
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"b": "aGVsbG8="}


@pytest.mark.parametrize("op", ["docvalue", "sort"])
def test_binary_no_fielddata(client, index_factory, op):
    idx = _mk(index_factory, {"properties": {"b": {"type": "binary"}}})
    _put(client, idx, {"b": "aGVsbG8="})
    body = {"docvalue_fields": ["b"]} if op == "docvalue" else {"sort": ["b"]}
    r = _search(client, idx, body)
    assert r.status_code == 400
    assert _cause_reason(r) == (
        "Can't load fielddata on [b] because fielddata is unsupported on fields of type [binary]. Use doc values instead."
    )


def test_binary_without_store_or_doc_values_does_not_validate_base64(client, index_factory):
    idx = _mk(index_factory, {"properties": {"b": {"type": "binary"}, "s": {"type": "binary", "store": True}}})
    assert _put(client, idx, {"b": "not base64!!"}).status_code == 201
    r = _put(client, idx, {"s": "aGVsbG8"})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "json_parse_exception")


def test_binary_stored_field_and_exists(client, index_factory):
    idx = _mk(index_factory, {"properties": {"s": {"type": "binary", "store": True}, "d": {"type": "binary", "doc_values": True}}})
    _put(client, idx, {"s": "aGVsbG8=", "d": "aGVsbG8="}, "1")
    _put(client, idx, {"s": "", "d": ""}, "2")
    res = _ok_search(client, idx, {"query": {"ids": {"values": ["1"]}}, "stored_fields": ["s"]})
    assert res["hits"]["hits"][0]["fields"] == {"s": ["aGVsbG8="]}
    assert _ids(client, idx, {"exists": {"field": "d"}}) == ["1", "2"]


# =========================================================================== range types


@pytest.fixture
def range_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "ir": {"type": "integer_range"},
                "fr": {"type": "float_range"},
                "lr": {"type": "long_range"},
                "dr": {"type": "double_range"},
                "dt": {"type": "date_range", "format": "yyyy-MM-dd"},
                "ipr": {"type": "ip_range"},
            }
        },
    )
    _put(
        client,
        idx,
        {
            "ir": {"gte": 10, "lte": 20},
            "fr": {"gte": 1.5, "lt": 2.5},
            "lr": {"gt": 100, "lt": 200},
            "dr": {"gte": 0.1, "lte": 0.2},
            "dt": {"gte": "2024-01-01", "lte": "2024-01-31"},
            "ipr": "192.168.0.0/24",
        },
        "1",
    )
    _put(client, idx, {"ir": {"gte": 15}, "ipr": {"gte": "10.0.0.1", "lte": "10.0.0.5"}}, "2")
    _put(client, idx, {"ir": [{"gte": 1, "lte": 2}, {"gte": 50, "lte": 60}]}, "3")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"ir": 15}}, ["1", "2"]),
        ({"term": {"ir": 21}}, ["2"]),
        ({"term": {"ir": 55}}, ["2", "3"]),
        ({"term": {"ir": 1000}}, ["2"]),
        ({"range": {"ir": {"gte": 5, "lte": 12}}}, ["1"]),
        ({"range": {"ir": {"gte": 5, "lte": 12, "relation": "within"}}}, []),
        ({"range": {"ir": {"gte": 5, "lte": 25, "relation": "within"}}}, ["1"]),
        ({"range": {"ir": {"gte": 12, "lte": 18, "relation": "contains"}}}, ["1"]),
        ({"range": {"ir": {"gte": 12, "lte": 18, "relation": "intersects"}}}, ["1", "2"]),
        ({"range": {"ir": {"gt": 20, "lt": 50}}}, ["2"]),
        ({"range": {"ir": {"gte": 20, "lte": 50}}}, ["1", "2", "3"]),
        ({"term": {"fr": 2.5}}, []),
        ({"term": {"fr": 1.5}}, ["1"]),
        ({"term": {"lr": 100}}, []),
        ({"term": {"lr": 101}}, ["1"]),
        ({"term": {"dr": 0.15}}, ["1"]),
        ({"term": {"dt": "2024-01-15"}}, ["1"]),
        ({"range": {"dt": {"gte": "2023-12-01", "lte": "2024-02-01", "relation": "within"}}}, ["1"]),
        ({"range": {"dt": {"gte": "2024-01-31", "lte": "2024-02-01"}}}, ["1"]),
        ({"term": {"ipr": "192.168.0.77"}}, ["1"]),
        ({"term": {"ipr": "10.0.0.3"}}, ["2"]),
        ({"term": {"ipr": "10.0.0.6"}}, []),
        ({"exists": {"field": "dr"}}, ["1"]),
    ],
)
def test_range_field_queries(client, range_idx, query, expected):
    assert _ids(client, range_idx, query) == expected


def test_ip_range_term_with_cidr_rejected(client, range_idx):
    r = _search(client, range_idx, {"query": {"term": {"ipr": "192.168.0.0/25"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "failed to create query: '192.168.0.0/25' is not an IP string literal."


def test_range_field_terms_agg_unsupported(client, range_idx):
    r = _search(client, range_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "ir"}}}})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "illegal_argument_exception")
    assert _cause_reason(r) == "Field [ir] of type [integer_range] is not supported for aggregation [terms]"


@pytest.mark.parametrize(
    "doc,cause,reason",
    [
        ({"ir": {"gte": 20, "lte": 10}}, "illegal_argument_exception", "min value (20) is greater than max value (10)"),
        ({"ir": {"gte": "a"}}, "number_format_exception", 'For input string: "a"'),
        ({"ir": {"foo": 1}}, "mapper_parsing_exception", "error parsing field [ir], with unknown parameter [foo]"),
        ({"ir": 5}, "mapper_parsing_exception", "error parsing field [ir], expected an object but got ir"),
        ({"ir": {"gt": 5, "gte": 6}}, "mapper_parsing_exception", "error parsing field [ir], invalid lower bound (gt/gte)"),
        (
            {"ipr": "10.0.0.0/33"},
            "illegal_argument_exception",
            "Illegal prefix length [33] in [10.0.0.0/33]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges",
        ),
        ({"dt": {"gte": "2024/01/01"}}, "parse_exception", None),
    ],
)
def test_range_field_invalid_values(client, range_idx, doc, cause, reason):
    r = _put(client, range_idx, doc)
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", cause)
    if reason:
        assert _cause_reason(r) == reason


def test_range_field_empty_object_accepted(client, range_idx):
    assert _put(client, range_idx, {"ir": {}}, "e").status_code == 201
    # an empty range is unbounded on both sides
    assert "e" in _ids(client, range_idx, {"term": {"ir": -999999}})


# =========================================================================== object / nested / flat_object


@pytest.mark.parametrize("ftype", ["object", "nested"])
def test_object_vs_nested_cross_matching(client, index_factory, ftype):
    idx = _mk(
        index_factory,
        {"properties": {"u": {"type": ftype, "properties": {"first": {"type": "keyword"}, "last": {"type": "keyword"}}}}},
    )
    _put(client, idx, {"u": [{"first": "John", "last": "Smith"}, {"first": "Alice", "last": "White"}]}, "1")
    cross = {"bool": {"must": [{"term": {"u.first": "John"}}, {"term": {"u.last": "White"}}]}}
    same = {"bool": {"must": [{"term": {"u.first": "John"}}, {"term": {"u.last": "Smith"}}]}}
    if ftype == "object":
        assert _ids(client, idx, cross) == ["1"]
        r = _search(client, idx, {"query": {"nested": {"path": "u", "query": same}}})
        assert r.status_code == 400
        assert _cause_reason(r) == "failed to create query: [nested] nested object under path [u] is not of nested type"
    else:
        assert _ids(client, idx, cross) == []
        assert _ids(client, idx, {"nested": {"path": "u", "query": cross}}) == []
        assert _ids(client, idx, {"nested": {"path": "u", "query": same}}) == ["1"]


def test_nested_docs_counted_in_cat_indices(client, index_factory):
    idx = _mk(index_factory, {"properties": {"u": {"type": "nested"}}})
    _put(client, idx, {"u": [{"a": 1}, {"a": 2}]}, "1")
    assert client.get(f"/{idx}/_count").json()["count"] == 1
    assert client.get(f"/_cat/indices/{idx}", params={"h": "docs.count"}).text.strip() == "3"


@pytest.mark.parametrize("ftype", ["object", "nested"])
@pytest.mark.parametrize("value", ["string", 5])
def test_object_rejects_concrete_value(client, index_factory, ftype, value):
    idx = _mk(index_factory, {"properties": {"u": {"type": ftype, "properties": {"a": {"type": "keyword"}}}}})
    r = _put(client, idx, {"u": value})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "mapper_parsing_exception"
    assert r.json()["error"]["reason"] == "object mapping for [u] tried to parse field [u] as object, but found a concrete value"


@pytest.mark.parametrize("value", [{"a": "x"}, None, []])
def test_nested_accepts_single_object_null_and_empty(client, index_factory, value):
    idx = _mk(index_factory, {"properties": {"u": {"type": "nested", "properties": {"a": {"type": "keyword"}}}}})
    assert _put(client, idx, {"u": value}).status_code == 201


def test_nested_include_in_parent_and_root(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "p": {"type": "nested", "include_in_parent": True, "properties": {"a": {"type": "keyword"}, "b": {"type": "keyword"}}},
                "r": {"type": "nested", "include_in_root": True, "properties": {"a": {"type": "keyword"}}},
                "n": {"type": "nested", "properties": {"a": {"type": "keyword"}}},
            }
        },
    )
    _put(client, idx, {"p": [{"a": "x", "b": "1"}, {"a": "y", "b": "2"}], "r": {"a": "z"}, "n": {"a": "w"}}, "1")
    assert _ids(client, idx, {"bool": {"must": [{"term": {"p.a": "x"}}, {"term": {"p.b": "2"}}]}}) == ["1"]
    assert _ids(client, idx, {"term": {"r.a": "z"}}) == ["1"]
    assert _ids(client, idx, {"term": {"n.a": "w"}}) == []


@pytest.fixture
def flat_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"fo": {"type": "flat_object"}}})
    _put(client, idx, {"fo": {"a": {"b": "Hello World", "c": 5}, "d": "x"}}, "1")
    _put(client, idx, {"fo": {"a": {"b": "other"}, "e": True}}, "2")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"fo.a.b": "Hello World"}}, ["1"]),
        ({"term": {"fo": "Hello World"}}, ["1"]),
        ({"term": {"fo": "x"}}, ["1"]),
        ({"term": {"fo.a.c": "5"}}, ["1"]),
        ({"term": {"fo.a.c": 5}}, ["1"]),
        ({"match": {"fo.a.b": "Hello World"}}, ["1"]),
        ({"match": {"fo.a.b": "hello"}}, []),
        ({"exists": {"field": "fo.e"}}, ["2"]),
        ({"exists": {"field": "fo"}}, ["1", "2"]),
        ({"term": {"fo.e": "true"}}, ["2"]),
        ({"prefix": {"fo.a.b": "Hel"}}, ["1"]),
        ({"wildcard": {"fo.a.b": "*World"}}, ["1"]),
        ({"range": {"fo.a.c": {"gte": "4"}}}, ["1"]),
        ({"query_string": {"query": "fo.d:x"}}, ["1"]),
    ],
)
def test_flat_object_queries(client, flat_idx, query, expected):
    assert _ids(client, flat_idx, query) == expected


def test_flat_object_mapping_not_expanded(client, flat_idx):
    assert _props(client, flat_idx) == {"fo": {"type": "flat_object"}}


def test_flat_object_sort_on_subfield(client, flat_idx):
    res = _ok_search(client, flat_idx, {"sort": [{"fo.a.b": "asc"}]})
    assert [(h["_id"], h["sort"]) for h in res["hits"]["hits"]] == [("1", ["Hello World"]), ("2", ["other"])]


def test_flat_object_rejects_scalar_and_properties(client, flat_idx, try_create):
    assert _put(client, flat_idx, {"fo": "scalar"}).status_code == 400
    r = try_create({"properties": {"fo": {"type": "flat_object", "properties": {"a": {"type": "keyword"}}}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "Mapping definition for [fo] has unsupported parameters:  [properties : {a={type=keyword}}]"


@pytest.fixture
def disabled_obj_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"o": {"type": "object", "enabled": False}, "k": {"type": "keyword"}}})
    _put(client, idx, {"o": {"a": 1, "b": {"c": "anything"}}, "k": "x"}, "1")
    _put(client, idx, {"o": "scalar"}, "2")
    _put(client, idx, {"o": [1, "a", {"z": True}]}, "3")
    return idx


def test_object_enabled_false_accepts_anything(client, disabled_obj_idx):
    idx = disabled_obj_idx
    assert client.get(f"/{idx}/_doc/3").json()["_source"] == {"o": [1, "a", {"z": True}]}
    assert _ids(client, idx, {"term": {"o.a": 1}}) == []
    assert _ids(client, idx, {"exists": {"field": "o"}}) == []
    assert _ids(client, idx, {"exists": {"field": "o.a"}}) == []
    assert _props(client, idx) == {"k": {"type": "keyword"}, "o": {"type": "object", "enabled": False}}


def test_object_enabled_cannot_be_updated(client, index_factory):
    idx = _mk(index_factory, {"properties": {"o": {"type": "object", "enabled": False}}})
    r = _put_mapping(client, idx, {"properties": {"o": {"type": "object", "enabled": True}}})
    assert r.status_code == 500
    assert r.json()["error"]["type"] == "mapper_exception"
    assert r.json()["error"]["reason"] == "the [enabled] parameter can't be updated for the object mapping [o]"


def test_root_enabled_false(client, index_factory):
    idx = _mk(index_factory, {"enabled": False})
    assert _put(client, idx, {"a": 1, "b": "x"}, "1").status_code == 201
    assert _props(client, idx) == {}
    assert _ids(client, idx, {"match_all": {}}) == ["1"]


def test_enabled_not_allowed_on_leaf(try_create):
    r = try_create({"properties": {"k": {"type": "keyword", "enabled": False}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "unknown parameter [enabled] on mapper [k] of type [keyword]"


# =========================================================================== dynamic


def test_dynamic_strict_rejects_new_field(client, index_factory):
    idx = _mk(index_factory, {"dynamic": "strict", "properties": {"a": {"type": "keyword"}}})
    r = _put(client, idx, {"a": "x", "b": 1})
    assert r.status_code == 400
    e = r.json()["error"]
    assert e["type"] == "strict_dynamic_mapping_exception"
    assert e["reason"] == "mapping set to strict, dynamic introduction of [b] within [_doc] is not allowed"


def test_dynamic_strict_parent_with_dynamic_true_child(client, index_factory):
    idx = _mk(index_factory, {"dynamic": "strict", "properties": {"o": {"type": "object", "dynamic": True}}})
    assert _put(client, idx, {"o": {"z": 1, "y": {"w": "q"}}}).status_code == 201
    o = _props(client, idx)["o"]
    assert o["dynamic"] == "true"
    assert o["properties"]["z"] == {"type": "long"}
    assert o["properties"]["y"]["properties"]["w"]["type"] == "text"


def test_dynamic_false_parent_with_strict_child(client, index_factory):
    idx = _mk(index_factory, {"dynamic": False, "properties": {"o": {"type": "object", "dynamic": "strict"}}})
    r = _put(client, idx, {"o": {"z": 1}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "mapping set to strict, dynamic introduction of [z] within [o] is not allowed"


@pytest.fixture
def dynamic_false_idx(client, index_factory):
    idx = _mk(index_factory, {"dynamic": False, "properties": {"a": {"type": "keyword"}}})
    _put(client, idx, {"a": "x", "b": "hello", "c": {"d": 1}}, "1")
    return idx


def test_dynamic_false_keeps_source_but_not_indexed(client, dynamic_false_idx):
    idx = dynamic_false_idx
    assert _props(client, idx) == {"a": {"type": "keyword"}}
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"a": "x", "b": "hello", "c": {"d": 1}}
    assert _ids(client, idx, {"match": {"b": "hello"}}) == []
    assert _ids(client, idx, {"exists": {"field": "b"}}) == []
    assert _ids(client, idx, {"query_string": {"query": "hello"}}) == []
    assert _ids(client, idx, {"term": {"a": "x"}}) == ["1"]


def test_dynamic_false_unmapped_sort_and_agg(client, dynamic_false_idx):
    r = _search(client, dynamic_false_idx, {"sort": ["b"]})
    assert r.status_code == 400
    assert _cause_reason(r) == "No mapping found for [b] in order to sort on"
    res = _ok_search(client, dynamic_false_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "b"}}}})
    assert res["aggregations"]["t"]["buckets"] == []


@pytest.mark.parametrize("value", ["runtime", "bogus", 1])
def test_dynamic_invalid_values(try_create, value):
    r = try_create({"dynamic": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == "Could not convert [dynamic.dynamic] to boolean"


@pytest.mark.parametrize("value", [True, False, "true", "false", "strict", "strict_allow_templates", "false_allow_templates"])
def test_dynamic_valid_values(try_create, value):
    assert try_create({"dynamic": value}).status_code == 200


def test_dynamic_strict_allow_templates(client, index_factory):
    idx = _mk(
        index_factory,
        {"dynamic": "strict_allow_templates", "dynamic_templates": [{"s": {"match": "s_*", "mapping": {"type": "keyword"}}}]},
    )
    assert _put(client, idx, {"s_x": "a"}).status_code == 201
    r = _put(client, idx, {"other": "a"})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "strict_dynamic_mapping_exception"
    assert r.json()["error"]["reason"] == (
        "mapping set to strict_allow_templates, dynamic introduction of [other] within [_doc] is not allowed"
    )
    assert _props(client, idx) == {"s_x": {"type": "keyword"}}


def test_dynamic_can_be_relaxed_via_put_mapping(client, index_factory):
    idx = _mk(index_factory, {"dynamic": "strict"})
    assert _put(client, idx, {"x": 1}).status_code == 400
    assert _put_mapping(client, idx, {"dynamic": True}).status_code == 200
    assert _put(client, idx, {"x": 1}).status_code == 201
    assert _props(client, idx) == {"x": {"type": "long"}}


@pytest.fixture
def templates_idx(client, index_factory):
    dt = [
        {"ints": {"match_mapping_type": "long", "mapping": {"type": "integer"}}},
        {"kw": {"match": "kw_*", "unmatch": "*_text", "mapping": {"type": "keyword"}}},
        {"path": {"path_match": "meta.*", "path_unmatch": "meta.skip", "mapping": {"type": "keyword", "index": False}}},
        {"re": {"match_pattern": "regex", "match": "^num_\\d+$", "mapping": {"type": "double"}}},
        {"strs": {"match_mapping_type": "string", "mapping": {"type": "text", "analyzer": "whitespace"}}},
        {"never": {"match": "dup_*", "mapping": {"type": "keyword"}}},
        {"objs": {"match_mapping_type": "object", "match": "obj_*", "mapping": {"type": "nested"}}},
    ]
    idx = _mk(index_factory, {"dynamic_templates": dt})
    doc = {
        "cnt": 5,
        "kw_a": "X",
        "kw_text": "Y",
        "meta": {"a": "m", "skip": "s"},
        "num_12": "3",
        "num_x": "3",
        "plain": "Hello World",
        "dup_v": "7",
        "obj_n": {"q": 1},
        "dbl": 1.5,
        "bool": True,
        "dt": "2024-01-01",
    }
    assert _put(client, idx, doc, "1").status_code == 201
    return idx


WS_TEXT = {"type": "text", "analyzer": "whitespace"}


@pytest.mark.parametrize(
    "field,expected",
    [
        ("cnt", {"type": "integer"}),
        ("kw_a", {"type": "keyword"}),
        ("kw_text", WS_TEXT),
        ("num_12", {"type": "double"}),
        ("num_x", WS_TEXT),
        ("plain", WS_TEXT),
        # first matching template wins: "strs" is listed before "never"
        ("dup_v", WS_TEXT),
        ("obj_n", {"type": "nested", "properties": {"q": {"type": "integer"}}}),
        ("dbl", {"type": "float"}),
        ("bool", {"type": "boolean"}),
        ("dt", {"type": "date"}),
        ("meta", {"properties": {"a": {"type": "keyword", "index": False}, "skip": WS_TEXT}}),
    ],
)
def test_dynamic_templates_resolution(client, templates_idx, field, expected):
    assert _props(client, templates_idx)[field] == expected


def test_dynamic_templates_roundtrip_order(client, templates_idx):
    dts = client.get(f"/{templates_idx}/_mapping").json()[templates_idx]["mappings"]["dynamic_templates"]
    assert [list(d.keys())[0] for d in dts] == ["ints", "kw", "path", "re", "strs", "never", "objs"]


def test_dynamic_template_placeholders(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "dynamic_templates": [
                {"dyn": {"match": "dyn_*", "mapping": {"type": "{dynamic_type}", "doc_values": False}}},
                {"named": {"match": "nm_*", "mapping": {"type": "keyword", "copy_to": "all_{name}"}}},
            ]
        },
    )
    doc = {"dyn_l": 5, "dyn_s": "str", "dyn_d": 1.5, "dyn_b": True, "dyn_dt": "2024-01-01", "nm_t": "zz"}
    assert _put(client, idx, doc, "1").status_code == 201
    p = _props(client, idx)
    assert p["dyn_l"] == {"type": "long", "doc_values": False}
    assert p["dyn_d"] == {"type": "float", "doc_values": False}
    assert p["dyn_b"] == {"type": "boolean", "doc_values": False}
    assert p["dyn_dt"] == {"type": "date", "doc_values": False}
    # {dynamic_type} for a string resolves to text, and text drops doc_values:false
    assert p["dyn_s"] == {"type": "text"}
    assert p["nm_t"] == {"type": "keyword", "copy_to": ["all_nm_t"]}
    assert p["all_nm_t"]["type"] == "text"
    assert _ids(client, idx, {"match": {"all_nm_t": "zz"}}) == ["1"]


def test_dynamic_template_name_placeholder_in_analyzer(client, index_factory):
    idx = _mk(index_factory, {"dynamic_templates": [{"an": {"match": "an_*", "mapping": {"type": "text", "analyzer": "{name}"}}}]})
    r = _put(client, idx, {"an_bogus": "A B"})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == "analyzer [an_bogus] has not been configured in mappings"


def test_date_detection_disabled(client, index_factory):
    idx = _mk(index_factory, {"date_detection": False})
    _put(client, idx, {"d": "2024-01-01"})
    assert _props(client, idx)["d"]["type"] == "text"


def test_numeric_detection(client, index_factory):
    idx = _mk(index_factory, {"numeric_detection": True})
    _put(client, idx, {"i": "42", "f": "4.2", "s": "4x", "neg": "-7", "e": "1e3"})
    p = _props(client, idx)
    assert {k: v["type"] for k, v in p.items()} == {"i": "long", "f": "float", "s": "text", "neg": "long", "e": "float"}


def test_numeric_detection_off_by_default(client, index_factory):
    idx = _mk(index_factory)
    _put(client, idx, {"i": "42"})
    assert _props(client, idx)["i"]["type"] == "text"


def test_dynamic_date_formats(client, index_factory):
    idx = _mk(index_factory, {"dynamic_date_formats": ["yyyy/MM/dd", "dd.MM.yyyy"]})
    _put(client, idx, {"a": "2024/01/15", "b": "2024-01-15", "c": "15.01.2024"})
    p = _props(client, idx)
    assert p["a"] == {"type": "date", "format": "yyyy/MM/dd", "print_format": "yyyy/MM/dd"}
    assert p["b"]["type"] == "text"
    assert p["c"] == {"type": "date", "format": "dd.MM.yyyy", "print_format": "dd.MM.yyyy"}


@pytest.mark.parametrize(
    "value,expected",
    [
        ("2024-01-15T10:00:00Z", {"type": "date"}),
        ("2024-01", {"type": "date"}),
        ("2024/01/15", {"type": "date", "format": "yyyy/MM/dd HH:mm:ss||yyyy/MM/dd||epoch_millis", "print_format": "yyyy/MM/dd HH:mm:ss"}),
        ("2024", None),
        ("2024-01-15 10:00:00", None),
        ("1705312800000", None),
    ],
)
def test_default_date_detection(client, index_factory, value, expected):
    idx = _mk(index_factory)
    _put(client, idx, {"f": value})
    p = _props(client, idx)["f"]
    if expected is None:
        assert p["type"] == "text"
    else:
        assert p == expected


@pytest.mark.parametrize(
    "first,second,status",
    [(5, "abc", 400), (5, "6", 201), (5, 6.5, 201), ("str", 5, 201)],
)
def test_dynamic_type_locked_by_first_document(client, index_factory, first, second, status):
    idx = _mk(index_factory)
    assert _put(client, idx, {"x": first}).status_code == 201
    assert _put(client, idx, {"x": second}).status_code == status


def test_dynamic_text_then_object_conflict(client, index_factory):
    idx = _mk(index_factory)
    _put(client, idx, {"y": "str"})
    r = _put(client, idx, {"y": {"o": 1}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_state_exception")


def test_dynamic_object_then_value_conflict(client, index_factory):
    idx = _mk(index_factory)
    _put(client, idx, {"a": {"b": 1}})
    r = _put(client, idx, {"a": 5})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "object mapping for [a] tried to parse field [a] as object, but found a concrete value"


@pytest.mark.parametrize(
    "arr,reason",
    [
        ([1, "two", 3.5], "mapper [f] cannot be changed from type [long] to [text]"),
        (["two", 1], "mapper [f] cannot be changed from type [text] to [long]"),
        ([1.5, 1], "mapper [f] cannot be changed from type [float] to [long]"),
        ([1, 1.5], "mapper [f] cannot be changed from type [long] to [float]"),
        (["1", 2], "mapper [f] cannot be changed from type [text] to [long]"),
        ([True, "x"], "mapper [f] cannot be changed from type [boolean] to [text]"),
        ([{"a": 1}, 5], "can't merge a non object mapping [f] with an object mapping"),
    ],
)
def test_dynamic_mixed_type_array_rejected(client, index_factory, arr, reason):
    idx = _mk(index_factory)
    r = _put(client, idx, {"f": arr})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == reason
    assert "f" not in _props(client, idx)


def test_dynamic_mixed_array_ok_when_field_mapped(client, index_factory):
    idx = _mk(index_factory, {"properties": {"f": {"type": "double"}}})
    assert _put(client, idx, {"f": [1, "2.5", 3.5]}, "1").status_code == 201
    assert sorted(_dv(client, idx, "f")["1"]) == [1.0, 2.5, 3.5]


@pytest.mark.parametrize("value", [None, []])
def test_dynamic_null_or_empty_array_adds_no_mapping(client, index_factory, value):
    idx = _mk(index_factory)
    assert _put(client, idx, {"n": value}).status_code == 201
    assert "n" not in _props(client, idx)


def test_dynamic_empty_object_mapped_as_object(client, index_factory):
    idx = _mk(index_factory)
    _put(client, idx, {"eo": {}})
    assert _props(client, idx)["eo"] == {"type": "object"}


def test_dynamic_big_integer_rejected(client, index_factory):
    idx = _mk(index_factory)
    r = _put(client, idx, {"big": 18446744073709551615})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "input_coercion_exception")


# =========================================================================== alias


@pytest.fixture
def alias_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "dist": {"type": "long"},
                "route": {"type": "alias", "path": "dist"},
                "t": {"type": "text"},
                "ta": {"type": "alias", "path": "t"},
            }
        },
    )
    _put(client, idx, {"dist": 5, "t": "hello world"}, "1")
    _put(client, idx, {"dist": 10, "t": "bye"}, "2")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"route": 5}}, ["1"]),
        ({"range": {"route": {"gte": 6}}}, ["2"]),
        ({"match": {"ta": "hello"}}, ["1"]),
        ({"exists": {"field": "route"}}, ["1", "2"]),
        ({"query_string": {"query": "route:10"}}, ["2"]),
    ],
)
def test_alias_queries(client, alias_idx, query, expected):
    assert _ids(client, alias_idx, query) == expected


def test_alias_sort_aggs_and_fields(client, alias_idx):
    res = _ok_search(client, alias_idx, {"sort": [{"route": "desc"}], "docvalue_fields": ["route"]})
    assert [(h["_id"], h["sort"], h["fields"]) for h in res["hits"]["hits"]] == [
        ("2", [10], {"route": [10]}),
        ("1", [5], {"route": [5]}),
    ]
    res = _ok_search(client, alias_idx, {"size": 0, "aggs": {"m": {"max": {"field": "route"}}, "t": {"terms": {"field": "route"}}}})
    assert res["aggregations"]["m"]["value"] == 10.0
    assert [b["key"] for b in res["aggregations"]["t"]["buckets"]] == [5, 10]
    res = _ok_search(client, alias_idx, {"fields": ["route"], "sort": ["_id"]})
    assert [h["fields"] for h in res["hits"]["hits"]] == [{"route": [5]}, {"route": [10]}]


def test_alias_not_in_source_filtering(client, alias_idx):
    res = _ok_search(client, alias_idx, {"_source": ["route"]})
    assert [h["_source"] for h in res["hits"]["hits"]] == [{}, {}]


def test_alias_highlight_uses_alias_name(client, alias_idx):
    res = _ok_search(client, alias_idx, {"query": {"match": {"ta": "hello"}}, "highlight": {"fields": {"ta": {}}}})
    assert res["hits"]["hits"][0]["highlight"] == {"ta": ["<em>hello</em> world"]}


def test_alias_write_rejected(client, alias_idx):
    r = _put(client, alias_idx, {"route": 7})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert r.json()["error"]["reason"] == "failed to parse"
    assert _cause_reason(r) == "Cannot write to a field alias [route]."


@pytest.mark.parametrize(
    "props,etype,reason",
    [
        (
            {"a": {"type": "alias", "path": "nope"}},
            "mapper_parsing_exception",
            "Invalid [path] value [nope] for field alias [a]: an alias must refer to an existing field in the mappings.",
        ),
        (
            {"x": {"type": "long"}, "a": {"type": "alias", "path": "b"}, "b": {"type": "alias", "path": "x"}},
            "mapper_parsing_exception",
            "Invalid [path] value [b] for field alias [a]: an alias cannot refer to another alias.",
        ),
        (
            {"x": {"properties": {"y": {"type": "long"}}}, "a": {"type": "alias", "path": "x"}},
            "mapper_parsing_exception",
            "Invalid [path] value [x] for field alias [a]: an alias must refer to an existing field in the mappings.",
        ),
        (
            {"x": {"type": "nested", "properties": {"y": {"type": "long"}}}, "a": {"type": "alias", "path": "x.y"}},
            "illegal_argument_exception",
            "Invalid [path] value [x.y] for field alias [a]: an alias must have the same nested scope as its target. "
            "The alias is not nested, but the target's nested scope is [x].",
        ),
    ],
)
def test_alias_invalid_targets(try_create, props, etype, reason):
    r = try_create({"properties": props})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == etype
    assert r.json()["error"]["reason"] == reason


@pytest.mark.parametrize(
    "mapping,reason",
    [
        ({"type": "alias"}, "The [path] property must be specified for field [a]."),
        ({"type": "alias", "path": "x", "index": False}, "Mapping definition for [a] has unsupported parameters:  [index : false]"),
    ],
)
def test_alias_invalid_definitions(try_create, mapping, reason):
    r = try_create({"properties": {"x": {"type": "long"}, "a": mapping}})
    assert r.status_code == 400
    assert _cause_reason(r) == reason


def test_alias_path_can_be_updated_but_not_type(client, index_factory):
    idx = _mk(index_factory, {"properties": {"x": {"type": "long"}, "a": {"type": "alias", "path": "x"}}})
    assert _put_mapping(client, idx, {"properties": {"y": {"type": "long"}, "a": {"type": "alias", "path": "y"}}}).status_code == 200
    assert _props(client, idx)["a"] == {"type": "alias", "path": "y"}
    r = _put_mapping(client, idx, {"properties": {"a": {"type": "keyword"}}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "Cannot merge a field alias mapping [a] with a mapping that is not for a field alias."


# =========================================================================== copy_to


@pytest.fixture
def copy_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "first": {"type": "text", "copy_to": ["full", "full_kw"]},
                "last": {"type": "text", "copy_to": "full"},
                "full": {"type": "text"},
                "full_kw": {"type": "keyword"},
                "n": {"type": "integer", "copy_to": "ns"},
                "ns": {"type": "keyword"},
            }
        },
    )
    _put(client, idx, {"first": "John", "last": "Smith", "n": 42}, "1")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"match": {"full": {"query": "john smith", "operator": "and"}}}, ["1"]),
        ({"term": {"full_kw": "John"}}, ["1"]),
        ({"term": {"ns": "42"}}, ["1"]),
        # values are copied as separate array entries -> position gap breaks the phrase
        ({"match_phrase": {"full": "john smith"}}, []),
    ],
)
def test_copy_to_queries(client, copy_idx, query, expected):
    assert _ids(client, copy_idx, query) == expected


def test_copy_to_target_not_in_source(client, copy_idx):
    assert client.get(f"/{copy_idx}/_doc/1").json()["_source"] == {"first": "John", "last": "Smith", "n": 42}
    res = _ok_search(client, copy_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "full_kw"}}}})
    assert res["aggregations"]["t"]["buckets"] == [{"key": "John", "doc_count": 1}]
    assert _dv(client, copy_idx, "ns") == {"1": ["42"]}


def test_copy_to_is_not_transitive(client, index_factory):
    idx = _mk(
        index_factory,
        {"properties": {"a": {"type": "text", "copy_to": "b"}, "b": {"type": "text", "copy_to": "c"}, "c": {"type": "text"}}},
    )
    _put(client, idx, {"a": "alpha", "b": "beta"}, "1")
    assert _ids(client, idx, {"match": {"b": "alpha"}}) == ["1"]
    assert _ids(client, idx, {"match": {"c": "beta"}}) == ["1"]
    assert _ids(client, idx, {"match": {"c": "alpha"}}) == []


def test_copy_to_creates_dynamic_target(client, index_factory):
    idx = _mk(index_factory, {"properties": {"a": {"type": "keyword", "copy_to": "dyn"}}})
    _put(client, idx, {"a": "x"}, "1")
    assert _props(client, idx)["dyn"]["type"] == "text"
    assert _ids(client, idx, {"match": {"dyn": "x"}}) == ["1"]


def test_copy_to_strict_target_missing(client, index_factory):
    idx = _mk(index_factory, {"dynamic": "strict", "properties": {"a": {"type": "keyword", "copy_to": "dyn"}}})
    r = _put(client, idx, {"a": "x"})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "strict_dynamic_mapping_exception"


def test_copy_to_target_type_validation(client, index_factory):
    idx = _mk(index_factory, {"properties": {"a": {"type": "keyword", "copy_to": "b"}, "b": {"type": "integer"}}})
    r = _put(client, idx, {"a": "x"})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "number_format_exception")
    assert _put(client, idx, {"a": "5"}, "1").status_code == 201
    assert _ids(client, idx, {"term": {"b": 5}}) == ["1"]


def test_copy_to_object_target_rejected(try_create):
    r = try_create({"properties": {"a": {"type": "text", "copy_to": "o"}, "o": {"properties": {"x": {"type": "text"}}}}})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == "Cannot copy to field [o] since it is mapped as an object"


# =========================================================================== index / doc_values / store

INDEX_FALSE_VALUES = {
    "keyword": "abc",
    "long": 5,
    "integer": 5,
    "double": 1.5,
    "date": "2024-01-01",
    "boolean": True,
    "ip": "1.2.3.4",
}


@pytest.fixture
def index_false_idx(client, index_factory):
    props = {t: {"type": t, "index": False} for t in INDEX_FALSE_VALUES}
    props["text"] = {"type": "text", "index": False}
    idx = _mk(index_factory, {"properties": props})
    doc = dict(INDEX_FALSE_VALUES, text="hello")
    _put(client, idx, doc, "1")
    return idx


@pytest.mark.parametrize("ftype", list(INDEX_FALSE_VALUES))
def test_index_false_still_searchable_via_doc_values(client, index_false_idx, ftype):
    v = INDEX_FALSE_VALUES[ftype]
    assert _ids(client, index_false_idx, {"term": {ftype: v}}) == ["1"]
    assert _ids(client, index_false_idx, {"range": {ftype: {"gte": v}}}) == ["1"]
    assert _ids(client, index_false_idx, {"exists": {"field": ftype}}) == ["1"]


@pytest.mark.parametrize(
    "query", [{"prefix": {"keyword": "ab"}}, {"wildcard": {"keyword": "a*"}}, {"regexp": {"keyword": "a.c"}}, {"fuzzy": {"keyword": "abd"}}]
)
def test_index_false_keyword_multi_term_queries(client, index_false_idx, query):
    assert _ids(client, index_false_idx, query) == ["1"]


@pytest.mark.parametrize("query", [{"term": {"text": "hello"}}, {"match": {"text": "hello"}}])
def test_index_false_text_not_searchable(client, index_false_idx, query):
    r = _search(client, index_false_idx, {"query": query})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
    assert _cause_reason(r) == "failed to create query: Cannot search on field [text] since it is not indexed."


@pytest.mark.parametrize("ftype", ["keyword", "long", "double", "date", "boolean", "ip"])
def test_index_false_and_doc_values_false(client, index_factory, ftype):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype, "index": False, "doc_values": False}}})
    _put(client, idx, {"f": INDEX_FALSE_VALUES[ftype]})
    r = _search(client, idx, {"query": {"term": {"f": INDEX_FALSE_VALUES[ftype]}}})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
    assert _cause_reason(r) == (
        "failed to create query: Cannot search on field [f] since it is both not indexed, and does not have doc_values enabled."
    )
    assert _ids(client, idx, {"exists": {"field": "f"}}) == []


@pytest.mark.parametrize("ftype", ["keyword", "long", "double", "date", "boolean", "ip"])
@pytest.mark.parametrize("op", ["sort", "agg", "docvalue"])
def test_doc_values_false_blocks_sort_agg(client, index_factory, ftype, op):
    idx = _mk(index_factory, {"properties": {"f": {"type": ftype, "doc_values": False}}})
    _put(client, idx, {"f": INDEX_FALSE_VALUES[ftype]}, "1")
    # still searchable through the inverted index
    assert _ids(client, idx, {"term": {"f": INDEX_FALSE_VALUES[ftype]}}) == ["1"]
    assert _ids(client, idx, {"exists": {"field": "f"}}) == ["1"]
    body = {
        "sort": {"sort": ["f"]},
        "agg": {"size": 0, "aggs": {"a": {"terms": {"field": "f"}}}},
        "docvalue": {"docvalue_fields": ["f"]},
    }[op]
    r = _search(client, idx, body)
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "illegal_argument_exception")
    assert _cause_reason(r) == (
        f"Can't load fielddata on [f] because fielddata is unsupported on fields of type [{ftype}]. Use doc values instead."
    )


@pytest.fixture
def stored_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "t": {"type": "text", "store": True},
                "k": {"type": "keyword", "store": True},
                "n": {"type": "integer", "store": True},
                "d": {"type": "date", "store": True},
                "b": {"type": "boolean", "store": True},
                "u": {"type": "text"},
            }
        },
    )
    _put(client, idx, {"t": "Hello", "k": ["x", "y"], "n": 5, "d": "2024-01-01", "b": True, "u": "nope"}, "1")
    return idx


def test_stored_fields_returned_without_source(client, stored_idx):
    res = _ok_search(client, stored_idx, {"stored_fields": ["t", "k", "n", "d", "b", "u"]})
    hit = res["hits"]["hits"][0]
    assert "_source" not in hit
    assert hit["fields"] == {"t": ["Hello"], "k": ["x", "y"], "n": [5], "d": ["2024-01-01T00:00:00.000Z"], "b": [True]}


def test_stored_fields_wildcard_and_none(client, stored_idx):
    res = _ok_search(client, stored_idx, {"stored_fields": ["*"]})
    assert set(res["hits"]["hits"][0]["fields"]) == {"t", "k", "n", "d", "b"}
    res = _ok_search(client, stored_idx, {"stored_fields": "_none_"})
    hit = res["hits"]["hits"][0]
    assert "_id" not in hit and "_source" not in hit and "fields" not in hit


def test_stored_fields_with_source(client, stored_idx):
    hit = _ok_search(client, stored_idx, {"stored_fields": ["t"], "_source": True})["hits"]["hits"][0]
    assert hit["_source"]["u"] == "nope"
    assert hit["fields"] == {"t": ["Hello"]}


def test_get_api_stored_fields(client, stored_idx):
    r = client.get(f"/{stored_idx}/_doc/1", params={"stored_fields": "t,n,u"}).json()
    assert r["fields"] == {"t": ["Hello"], "n": [5]}
    assert "_source" not in r


# =========================================================================== _source / _routing / _meta


@pytest.fixture
def no_source_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "_source": {"enabled": False},
            "properties": {"t": {"type": "text"}, "k": {"type": "keyword"}, "s": {"type": "keyword", "store": True}},
        },
    )
    _put(client, idx, {"t": "hello world", "k": "kv", "s": "sv"}, "1")
    return idx


def test_source_disabled_search_and_fields(client, no_source_idx):
    assert _ids(client, no_source_idx, {"match": {"t": "hello"}}) == ["1"]
    hit = _ok_search(client, no_source_idx, {"docvalue_fields": ["k"], "stored_fields": ["s"]})["hits"]["hits"][0]
    assert "_source" not in hit
    assert hit["fields"] == {"s": ["sv"], "k": ["kv"]}


def test_source_disabled_get_and_update(client, no_source_idx):
    doc = client.get(f"/{no_source_idx}/_doc/1").json()
    assert doc["found"] is True and "_source" not in doc
    r = client.post(f"/{no_source_idx}/_update/1", json={"doc": {"k": "z"}})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "document_source_missing_exception"
    assert r.json()["error"]["reason"] == "[1]: document source missing"


def test_source_disabled_highlight_empty(client, no_source_idx):
    hit = _ok_search(client, no_source_idx, {"query": {"match": {"t": "hello"}}, "highlight": {"fields": {"t": {}}}})["hits"]["hits"][0]
    assert "highlight" not in hit


def test_source_includes_excludes_mapping(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "_source": {"includes": ["a", "o.*"], "excludes": ["o.secret"]},
            "properties": {
                "a": {"type": "keyword"},
                "b": {"type": "keyword"},
                "o": {"properties": {"x": {"type": "keyword"}, "secret": {"type": "keyword"}}},
            },
        },
    )
    _put(client, idx, {"a": "1", "b": "2", "o": {"x": "3", "secret": "4"}}, "1")
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"a": "1", "o": {"x": "3"}}
    # filtered-out fields are still indexed
    assert _ids(client, idx, {"term": {"b": "2"}}) == ["1"]
    assert _ids(client, idx, {"term": {"o.secret": "4"}}) == ["1"]


def test_source_cannot_be_disabled_later(client, index_factory):
    idx = _mk(index_factory, {"properties": {"a": {"type": "keyword"}}})
    r = _put_mapping(client, idx, {"_source": {"enabled": False}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == (
        "Mapper for [_source] conflicts with existing mapper:\n\tCannot update parameter [enabled] from [true] to [false]"
    )


def test_source_unknown_parameter(try_create):
    r = try_create({"_source": {"bogus": 1}})
    assert r.status_code == 400
    assert _cause_reason(r) == "unknown parameter [bogus] on mapper [_source] of type [null]"


@pytest.fixture
def routing_idx(client, index_factory):
    return _mk(index_factory, {"_routing": {"required": True}, "properties": {"a": {"type": "keyword"}}})


def test_routing_required_index_without_routing(client, routing_idx):
    r = _put(client, routing_idx, {"a": "x"}, "1")
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "routing_missing_exception"
    assert r.json()["error"]["reason"] == f"routing is required for [{routing_idx}]/[1]"


def test_routing_required_auto_id_without_routing(client, routing_idx):
    r = _put(client, routing_idx, {"a": "x"})
    assert r.status_code == 500
    assert r.json()["error"]["type"] == "null_pointer_exception"


def test_routing_required_get_delete_and_bulk(client, routing_idx):
    assert _put(client, routing_idx, {"a": "x"}, "1", routing="r1").status_code == 201
    r = client.get(f"/{routing_idx}/_doc/1")
    assert r.status_code == 400 and r.json()["error"]["type"] == "routing_missing_exception"
    got = client.get(f"/{routing_idx}/_doc/1", params={"routing": "r1"}).json()
    assert got["_routing"] == "r1"
    assert _ids(client, routing_idx, {"term": {"_routing": "r1"}}) == ["1"]
    r = client.delete(f"/{routing_idx}/_doc/1")
    assert r.status_code == 400 and r.json()["error"]["type"] == "routing_missing_exception"
    r = client.post(
        f"/{routing_idx}/_bulk",
        content=b'{"index":{"_id":"2"}}\n{"a":"y"}\n',
        headers={"Content-Type": "application/x-ndjson"},
    )
    assert r.json()["errors"] is True
    assert r.json()["items"][0]["index"]["error"]["type"] == "routing_missing_exception"


def test_meta_roundtrip_and_replace(client, index_factory):
    meta = {"owner": "team", "version": 3, "nested": {"a": [1, 2]}}
    idx = _mk(index_factory, {"_meta": meta, "properties": {"a": {"type": "keyword", "meta": {"unit": "ms"}}}})
    m = client.get(f"/{idx}/_mapping").json()[idx]["mappings"]
    assert m["_meta"] == meta
    assert m["properties"]["a"]["meta"] == {"unit": "ms"}
    assert _put_mapping(client, idx, {"_meta": {"owner": "other"}}).status_code == 200
    assert client.get(f"/{idx}/_mapping").json()[idx]["mappings"]["_meta"] == {"owner": "other"}


@pytest.mark.parametrize("value,shown", [(5, "Integer[5]"), ({"x": 1}, "LinkedHashMap[{x=1}]")])
def test_field_meta_values_must_be_strings(try_create, value, shown):
    r = try_create({"properties": {"a": {"type": "keyword", "meta": {"u": value}}}})
    assert r.status_code == 400
    assert _cause_reason(r) == f"[meta] values can only be strings, but got {shown} for field [a]"


# =========================================================================== multi-fields / token_count


@pytest.fixture
def multi_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "t": {
                    "type": "text",
                    "analyzer": "standard",
                    "fields": {
                        "en": {"type": "text", "analyzer": "english"},
                        "ws": {"type": "text", "analyzer": "whitespace"},
                        "raw": {"type": "keyword"},
                        "len": {"type": "token_count", "analyzer": "standard"},
                    },
                }
            }
        },
    )
    _put(client, idx, {"t": "The Running Foxes"}, "1")
    _put(client, idx, {"t": "run fox"}, "2")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"match": {"t": "running"}}, ["1"]),
        ({"match": {"t": "run"}}, ["2"]),
        ({"match": {"t.en": "run"}}, ["1", "2"]),
        ({"match": {"t.en": "foxes"}}, ["1", "2"]),
        ({"term": {"t.ws": "Running"}}, ["1"]),
        ({"term": {"t.ws": "running"}}, []),
        ({"term": {"t.raw": "run fox"}}, ["2"]),
        ({"term": {"t.len": 3}}, ["1"]),
        ({"range": {"t.len": {"gte": 2}}}, ["1", "2"]),
        ({"multi_match": {"query": "run", "fields": ["t", "t.en"]}}, ["1", "2"]),
    ],
)
def test_multi_fields_queries(client, multi_idx, query, expected):
    assert _ids(client, multi_idx, query) == expected


def test_multi_fields_not_in_source_and_sortable(client, multi_idx):
    assert client.get(f"/{multi_idx}/_doc/1").json()["_source"] == {"t": "The Running Foxes"}
    res = _ok_search(client, multi_idx, {"sort": [{"t.len": "desc"}], "docvalue_fields": ["t.len"]})
    assert [(h["_id"], h["sort"], h["fields"]) for h in res["hits"]["hits"]] == [
        ("1", [3], {"t.len": [3]}),
        ("2", [2], {"t.len": [2]}),
    ]


@pytest.mark.parametrize(
    "fields,reason",
    [
        ({"x": {"type": "object"}}, "Type [object] cannot be used in multi field"),
        ({"x": {"type": "nested"}}, "Type [nested] cannot be used in multi field"),
        ({"x.y": {"type": "keyword"}}, "Field name [x.y] which is a multi field of [t] cannot contain '.'"),
    ],
)
def test_multi_fields_invalid(try_create, fields, reason):
    r = try_create({"properties": {"t": {"type": "text", "fields": fields}}})
    assert r.status_code == 400
    assert _cause_reason(r) == reason


def test_multi_fields_nested_multi_field_accepted(try_create):
    r = try_create({"properties": {"t": {"type": "text", "fields": {"x": {"type": "text", "fields": {"y": {"type": "keyword"}}}}}}})
    assert r.status_code == 200


def test_token_count_requires_analyzer(try_create):
    r = try_create({"properties": {"t": {"type": "token_count"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "Analyzer must be set for field [t] but wasn't."


@pytest.mark.parametrize(
    "analyzer,epi,expected",
    [("standard", True, 3), ("stop", True, 3), ("stop", False, 1)],
)
def test_token_count_position_increments(client, index_factory, analyzer, epi, expected):
    idx = _mk(
        index_factory,
        {"properties": {"tc": {"type": "token_count", "analyzer": analyzer, "enable_position_increments": epi}}},
    )
    _put(client, idx, {"tc": "the a quick"}, "1")
    assert _dv(client, idx, "tc") == {"1": [expected]}


def test_token_count_array_and_numeric_input(client, index_factory):
    idx = _mk(index_factory, {"properties": {"tc": {"type": "token_count", "analyzer": "standard"}}})
    _put(client, idx, {"tc": ["a b", "c"]}, "1")
    assert sorted(_dv(client, idx, "tc")["1"]) == [1, 2]
    assert _put(client, idx, {"tc": 5}).status_code == 201


# =========================================================================== search_as_you_type


@pytest.fixture
def sayt_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {"properties": {"s": {"type": "search_as_you_type"}, "s2": {"type": "search_as_you_type", "max_shingle_size": 2}}},
    )
    _put(client, idx, {"s": "quick brown fox jumps", "s2": "quick brown fox"}, "1")
    _put(client, idx, {"s": "brown quick", "s2": "x"}, "2")
    return idx


def test_search_as_you_type_mapping_defaults(client, sayt_idx):
    p = _props(client, sayt_idx)
    assert p["s"] == {"type": "search_as_you_type", "doc_values": False, "max_shingle_size": 3}
    assert p["s2"] == {"type": "search_as_you_type", "doc_values": False, "max_shingle_size": 2}


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"multi_match": {"query": "quick br", "type": "bool_prefix", "fields": ["s", "s._2gram", "s._3gram"]}}, ["1", "2"]),
        (
            {"multi_match": {"query": "quick br", "type": "bool_prefix", "operator": "and", "fields": ["s", "s._2gram", "s._3gram"]}},
            ["1", "2"],
        ),
        ({"multi_match": {"query": "fox ju", "type": "bool_prefix", "fields": ["s", "s._2gram", "s._3gram"]}}, ["1"]),
        ({"match_phrase_prefix": {"s": "brown fo"}}, ["1"]),
        ({"term": {"s._2gram": "quick brown"}}, ["1"]),
        ({"term": {"s._3gram": "quick brown fox"}}, ["1"]),
        ({"term": {"s._index_prefix": "qui"}}, ["1", "2"]),
        ({"term": {"s._index_prefix": "quick b"}}, ["1"]),
        ({"match": {"s": "jumps"}}, ["1"]),
        ({"match_bool_prefix": {"s": "quick bro"}}, ["1", "2"]),
        ({"term": {"s2._2gram": "quick brown"}}, ["1"]),
        # sub-fields beyond max_shingle_size simply don't exist
        ({"term": {"s2._3gram": "quick brown fox"}}, []),
        ({"term": {"s._4gram": "quick brown fox jumps"}}, []),
    ],
)
def test_search_as_you_type_queries(client, sayt_idx, query, expected):
    assert _ids(client, sayt_idx, query) == expected


@pytest.mark.parametrize("size,ok", [(1, False), (2, True), (4, True), (5, False)])
def test_search_as_you_type_max_shingle_size(try_create, size, ok):
    r = try_create({"properties": {"s": {"type": "search_as_you_type", "max_shingle_size": size}}})
    if ok:
        assert r.status_code == 200
    else:
        assert r.status_code == 400
        assert _cause_reason(r) == f"[max_shingle_size] must be at least [2] and at most [4], got [{size}]"


def test_search_as_you_type_no_fielddata(client, sayt_idx):
    r = _search(client, sayt_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "s"}}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "Fielddata is not supported on field [s] of type [search_as_you_type]"


# =========================================================================== rank_feature(s)


@pytest.fixture
def rank_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "pr": {"type": "rank_feature"},
                "neg": {"type": "rank_feature", "positive_score_impact": False},
                "topics": {"type": "rank_features"},
                "t": {"type": "text"},
            }
        },
    )
    _put(client, idx, {"pr": 10, "neg": 1, "topics": {"sports": 50, "politics": 5}, "t": "a"}, "1")
    _put(client, idx, {"pr": 1, "neg": 100, "topics": {"sports": 5}, "t": "a"}, "2")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"rank_feature": {"field": "pr", "saturation": {"pivot": 5}}}, [("1", 10 / 15), ("2", 1 / 6)]),
        ({"rank_feature": {"field": "pr", "log": {"scaling_factor": 1}}}, [("1", 2.3978953), ("2", 0.6931472)]),
        ({"rank_feature": {"field": "pr", "sigmoid": {"pivot": 5, "exponent": 1}}}, [("1", 10 / 15), ("2", 1 / 6)]),
        ({"rank_feature": {"field": "pr", "linear": {}}}, [("1", 10.0), ("2", 1.0)]),
        ({"rank_feature": {"field": "topics.politics"}}, [("1", 0.5)]),
    ],
)
def test_rank_feature_functions(client, rank_idx, query, expected):
    hits = _ok_search(client, rank_idx, {"query": query})["hits"]["hits"]
    assert [h["_id"] for h in hits] == [e[0] for e in expected]
    for h, (_, score) in zip(hits, expected):
        assert h["_score"] == pytest.approx(score, rel=1e-4)


@pytest.mark.parametrize("field,first", [("pr", "1"), ("neg", "1"), ("topics.sports", "1")])
def test_rank_feature_default_ordering(client, rank_idx, field, first):
    hits = _ok_search(client, rank_idx, {"query": {"rank_feature": {"field": field}}})["hits"]["hits"]
    assert hits[0]["_id"] == first
    assert len(hits) == 2


def test_rank_feature_exists(client, rank_idx):
    assert _ids(client, rank_idx, {"exists": {"field": "pr"}}) == ["1", "2"]


@pytest.mark.parametrize(
    "body,reason",
    [
        ({"query": {"term": {"pr": 10}}}, "failed to create query: Queries on [rank_feature] fields are not supported"),
        (
            {"query": {"rank_feature": {"field": "t"}}},
            "failed to create query: [rank_feature] query only works on [rank_feature] fields and features of [rank_features] fields, not [text]",
        ),
        ({"sort": ["pr"]}, "[rank_feature] fields do not support sorting, scripting or aggregating"),
    ],
)
def test_rank_feature_unsupported_operations(client, rank_idx, body, reason):
    r = _search(client, rank_idx, body)
    assert r.status_code == 400
    assert _cause_reason(r) == reason


@pytest.mark.parametrize(
    "doc,fragment",
    [
        ({"pr": 0}, "featureValue must be a positive normal float, got: 0.0 for feature pr"),
        ({"pr": -1}, "featureValue must be a positive normal float, got: -1.0 for feature pr"),
        ({"pr": 1e-50}, "featureValue must be a positive normal float, got: 0.0 for feature pr"),
        ({"pr": [1, 2]}, "[rank_feature] fields do not support indexing multiple values for the same field [pr] in the same document"),
        ({"topics": {"x": -1}}, "featureValue must be a positive normal float, got: -1.0 for feature x on field topics"),
        ({"topics": 5}, "[rank_features] fields must be json objects, expected a START_OBJECT but got: VALUE_NUMBER"),
    ],
)
def test_rank_feature_invalid_values(client, rank_idx, doc, fragment):
    r = _put(client, rank_idx, doc)
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert fragment in _cause_reason(r)


def test_rank_feature_string_number_accepted(client, rank_idx):
    assert _put(client, rank_idx, {"pr": "5"}).status_code == 201


# =========================================================================== completion


@pytest.fixture
def completion_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "sug": {"type": "completion"},
                "sug2": {"type": "completion", "analyzer": "simple", "preserve_separators": False, "max_input_length": 5},
            }
        },
    )
    _put(client, idx, {"sug": {"input": ["Nirvana", "Nevermind"], "weight": 10}}, "1")
    _put(client, idx, {"sug": ["Nickelback"]}, "2")
    _put(client, idx, {"sug": "Nine Inch Nails"}, "3")
    _put(client, idx, {"sug": [{"input": "Nirvana Live", "weight": 20}]}, "4")
    _put(client, idx, {"sug2": "Foo Fighters"}, "5")
    return idx


def _suggest(client, idx, prefix, field="sug", **opts):
    body = {"suggest": {"x": {"prefix": prefix, "completion": dict({"field": field}, **opts)}}}
    res = _ok_search(client, idx, body)
    return [(o["text"], o["_id"], o["_score"]) for o in res["suggest"]["x"][0]["options"]]


@pytest.mark.parametrize(
    "prefix,opts,expected",
    [
        ("ni", {}, [("Nirvana Live", "4", 20.0), ("Nirvana", "1", 10.0), ("Nickelback", "2", 1.0), ("Nine Inch Nails", "3", 1.0)]),
        ("Ni", {}, [("Nirvana Live", "4", 20.0), ("Nirvana", "1", 10.0), ("Nickelback", "2", 1.0), ("Nine Inch Nails", "3", 1.0)]),
        ("nir", {}, [("Nirvana Live", "4", 20.0), ("Nirvana", "1", 10.0)]),
        ("nev", {}, [("Nevermind", "1", 10.0)]),
        ("ni", {"size": 1}, [("Nirvana Live", "4", 20.0)]),
        ("nirv", {"fuzzy": {"fuzziness": 1}}, [("Nirvana Live", "4", 60.0), ("Nirvana", "1", 30.0)]),
        ("xyz", {}, []),
    ],
)
def test_completion_suggestions(client, completion_idx, prefix, opts, expected):
    assert _suggest(client, completion_idx, prefix, **opts) == expected


@pytest.mark.parametrize("prefix,expected", [("foof", [("Foo F", "5", 1.0)]), ("foo f", [("Foo F", "5", 1.0)]), ("foo fi", []), ("foofig", [])])
def test_completion_preserve_separators_and_max_input_length(client, completion_idx, prefix, expected):
    assert _suggest(client, completion_idx, prefix, field="sug2") == expected


def test_completion_not_term_queryable(client, completion_idx):
    assert _ids(client, completion_idx, {"term": {"sug": "Nirvana"}}) == []


@pytest.mark.parametrize(
    "value,cause,reason",
    [
        ({"input": "x", "weight": -1}, "illegal_argument_exception", "weight must be in the interval [0..2147483647], but was [-1]"),
        ({"input": "x", "weight": "abc"}, "illegal_argument_exception", "weight must be an integer, but was [abc]"),
        ({"input": "x", "bogus": 1}, "illegal_argument_exception", "unknown field name [bogus], must be one of [input, weight, contexts]"),
        (5, "parsing_exception", "failed to parse [sug]: expected text or object, but got VALUE_NUMBER"),
        ({"input": "a\u001fb"}, "illegal_argument_exception", "Illegal input [a\u001fb] UTF-16 codepoint [0x1f] at position 1 is a reserved character"),
    ],
)
def test_completion_invalid_inputs(client, completion_idx, value, cause, reason):
    r = _put(client, completion_idx, {"sug": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", cause)
    assert _cause_reason(r) == reason


# =========================================================================== geo_point

GEO_INPUTS = {
    "obj": {"lat": 41.12, "lon": -71.34},
    "objstr": {"lat": "41.12", "lon": "-71.34"},
    "str": "41.12,-71.34",
    "strsp": " 41.12 , -71.34 ",
    "arr": [-71.34, 41.12],
    "wkt": "POINT (-71.34 41.12)",
    "geojson": {"type": "Point", "coordinates": [-71.34, 41.12]},
    "hash": "drm3btev3e86",
}


@pytest.mark.parametrize("name", list(GEO_INPUTS))
def test_geo_point_input_formats(client, index_factory, name):
    idx = _mk(index_factory, {"properties": {"p": {"type": "geo_point"}}})
    assert _put(client, idx, {"p": GEO_INPUTS[name]}, "1").status_code == 201
    assert _ids(client, idx, {"geo_distance": {"distance": "1km", "p": {"lat": 41.12, "lon": -71.34}}}) == ["1"]
    lat, lon = (float(x) for x in _dv(client, idx, "p")["1"][0].split(","))
    assert lat == pytest.approx(41.12, abs=1e-6)
    assert lon == pytest.approx(-71.34, abs=1e-6)


def test_geo_point_queries_and_distance_sort(client, index_factory):
    idx = _mk(index_factory, {"properties": {"p": {"type": "geo_point"}}})
    _put(client, idx, {"p": {"lat": 41.12, "lon": -71.34}}, "near")
    _put(client, idx, {"p": {"lat": 52.52, "lon": 13.40}}, "far")
    box = {"geo_bounding_box": {"p": {"top_left": {"lat": 42, "lon": -72}, "bottom_right": {"lat": 40, "lon": -70}}}}
    assert _ids(client, idx, box) == ["near"]
    res = _ok_search(client, idx, {"sort": [{"_geo_distance": {"p": {"lat": 41, "lon": -71}, "order": "asc", "unit": "km"}}]})
    assert [h["_id"] for h in res["hits"]["hits"]] == ["near", "far"]
    assert res["hits"]["hits"][0]["sort"][0] == pytest.approx(31.475, abs=0.01)


@pytest.mark.parametrize(
    "value,cause,reason",
    [
        ({"lat": 91, "lon": 0}, "illegal_argument_exception", "illegal latitude value [91.0] for p"),
        ({"lat": 0, "lon": 181}, "illegal_argument_exception", "illegal longitude value [181.0] for p"),
        ([200, 1], "illegal_argument_exception", "illegal longitude value [200.0] for p"),
        ("abc", "parse_exception", "unsupported symbol [a] in geohash [abc]"),
        ({"lat": 1}, "parse_exception", "field [lon] missing"),
        ({"lat": 1, "lon": 2, "z": 3}, "parse_exception", "field must be either [lon|lat], [type|coordinates], or [geohash]"),
    ],
)
def test_geo_point_invalid(client, index_factory, value, cause, reason):
    idx = _mk(index_factory, {"properties": {"p": {"type": "geo_point"}}})
    r = _put(client, idx, {"p": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", cause)
    assert r.json()["error"]["reason"] == "failed to parse field [p] of type [geo_point]"
    assert _cause_reason(r) == reason


@pytest.mark.parametrize("value", [[1, 2, 3], "1,2,3", "POINT (1 2 3)"])
def test_geo_point_z_value_ignored_by_default(client, index_factory, value):
    idx = _mk(index_factory, {"properties": {"p": {"type": "geo_point"}}})
    assert _put(client, idx, {"p": value}).status_code == 201


@pytest.mark.parametrize(
    "value,reason",
    [
        ([1, 2, 3], "Exception parsing coordinates: found Z value [3.0] but [ignore_z_value] parameter is [false]"),
        ("1,2,3", "Exception parsing coordinates: found Z value [3.0] but [ignore_z_value] parameter is [false]"),
        ("POINT (1 2 3)", "Invalid WKT format"),
    ],
)
def test_geo_point_ignore_z_value_false(client, index_factory, value, reason):
    idx = _mk(index_factory, {"properties": {"p": {"type": "geo_point", "ignore_z_value": False}}})
    r = _put(client, idx, {"p": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "parse_exception")
    assert _cause_reason(r) == reason


def test_geo_point_ignore_malformed_normalizes_out_of_range(client, index_factory):
    """With ignore_malformed, out-of-range coordinates are normalized (kept),
    only unparseable values are ignored."""
    idx = _mk(index_factory, {"properties": {"p": {"type": "geo_point", "ignore_malformed": True}}})
    assert _put(client, idx, {"p": {"lat": 91, "lon": 0}}, "lat").status_code == 201
    assert _put(client, idx, {"p": [200, 1]}, "lon").status_code == 201
    assert _put(client, idx, {"p": "abc"}, "bad").status_code == 201
    assert _ids(client, idx, {"exists": {"field": "p"}}) == ["lat", "lon"]
    assert _ids(client, idx, {"exists": {"field": "_ignored"}}) == ["bad"]
    lat, lon = (float(x) for x in _dv(client, idx, "p", doc_id="lon")["lon"][0].split(","))
    assert lon == pytest.approx(-160.0, abs=1e-6)
    assert lat == pytest.approx(1.0, abs=1e-6)
    lat, lon = (float(x) for x in _dv(client, idx, "p", doc_id="lat")["lat"][0].split(","))
    assert lat == pytest.approx(89.0, abs=1e-6)
    assert lon == pytest.approx(180.0, abs=1e-6)


def test_geo_point_centroid_agg(client, index_factory):
    idx = _mk(index_factory, {"properties": {"p": {"type": "geo_point"}}})
    _put(client, idx, {"p": [0, 0]})
    _put(client, idx, {"p": [10, 20]})
    res = _ok_search(client, idx, {"size": 0, "aggs": {"c": {"geo_centroid": {"field": "p"}}}})
    c = res["aggregations"]["c"]
    assert c["count"] == 2
    assert c["location"]["lat"] == pytest.approx(10, abs=1e-5)
    assert c["location"]["lon"] == pytest.approx(5, abs=1e-5)


# =========================================================================== geo_shape


@pytest.fixture
def shape_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"g": {"type": "geo_shape"}}})
    docs = {
        "pt": {"type": "Point", "coordinates": [13.4, 52.5]},
        "wktpt": "POINT (2.35 48.85)",
        "poly": {"type": "Polygon", "coordinates": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]]},
        "wktpoly": "POLYGON ((20 20, 30 20, 30 30, 20 30, 20 20))",
        "line": {"type": "LineString", "coordinates": [[-10, -10], [-5, -5]]},
        "env": {"type": "envelope", "coordinates": [[100, 10], [110, 0]]},
        "wktenv": "BBOX (40, 50, 10, 0)",
        "multi": "MULTIPOINT (60 60, 61 61)",
        "coll": {"type": "GeometryCollection", "geometries": [{"type": "Point", "coordinates": [70, 5]}]},
    }
    for k, v in docs.items():
        assert _put(client, idx, {"g": v}, k).status_code == 201
    return idx


ALL_SHAPES = ["coll", "env", "line", "multi", "poly", "pt", "wktenv", "wktpoly", "wktpt"]


@pytest.mark.parametrize(
    "shape,relation,expected",
    [
        ({"type": "envelope", "coordinates": [[-1, 11], [11, -1]]}, "intersects", ["poly"]),
        ({"type": "envelope", "coordinates": [[-1, 11], [11, -1]]}, "within", ["poly"]),
        ({"type": "envelope", "coordinates": [[-1, 11], [11, -1]]}, "disjoint", [s for s in ALL_SHAPES if s != "poly"]),
        ({"type": "point", "coordinates": [5, 5]}, "contains", ["poly"]),
        ({"type": "envelope", "coordinates": [[0, 60], [20, 40]]}, "intersects", ["pt", "wktpt"]),
        ("POLYGON ((25 25, 26 25, 26 26, 25 26, 25 25))", "intersects", ["wktpoly"]),
        ({"type": "envelope", "coordinates": [[39, 11], [51, -1]]}, "intersects", ["wktenv"]),
        ({"type": "envelope", "coordinates": [[59, 62], [62, 59]]}, "intersects", ["multi"]),
        ({"type": "point", "coordinates": [70, 5]}, "intersects", ["coll"]),
        ({"type": "linestring", "coordinates": [[-10, -5], [-5, -10]]}, "intersects", ["line"]),
    ],
)
def test_geo_shape_relations(client, shape_idx, shape, relation, expected):
    q = {"geo_shape": {"g": {"shape": shape, "relation": relation}}}
    assert _ids(client, shape_idx, q) == expected


def test_geo_shape_with_point_queries(client, shape_idx):
    box = {"geo_bounding_box": {"g": {"top_left": {"lat": 60, "lon": 0}, "bottom_right": {"lat": 40, "lon": 20}}}}
    assert _ids(client, shape_idx, box) == ["pt", "wktpt"]
    assert _ids(client, shape_idx, {"geo_distance": {"distance": "100km", "g": {"lat": 52.5, "lon": 13.4}}}) == ["pt"]


@pytest.mark.parametrize(
    "value,cause",
    [
        ({"type": "Polygon", "coordinates": [[[0, 0], [10, 0], [10, 10]]]}, "x_content_parse_exception"),
        ({"type": "Polygon", "coordinates": [[[0, 0], [10, 0], [10, 10], [0, 10]]]}, "x_content_parse_exception"),
        ({"type": "Foo", "coordinates": [1, 2]}, "x_content_parse_exception"),
        ("POINT (1)", "parse_exception"),
        ("bogus", "illegal_argument_exception"),
        ({"type": "circle", "coordinates": [0, 0], "radius": "1km"}, "unsupported_operation_exception"),
    ],
)
def test_geo_shape_invalid(client, shape_idx, value, cause):
    r = _put(client, shape_idx, {"g": value})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", cause)
    assert r.json()["error"]["reason"] == "failed to parse field [g] of type [geo_shape]"


def test_geo_shape_not_sortable(client, shape_idx):
    r = _search(client, shape_idx, {"sort": ["g"]})
    assert r.status_code == 400
    assert _cause_reason(r) == "can't sort on geo_shape field without using specific sorting feature, like geo_distance"


# =========================================================================== percolator


@pytest.fixture
def perc_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"q": {"type": "percolator"}, "msg": {"type": "text"}, "n": {"type": "integer"}}})
    _put(client, idx, {"q": {"match": {"msg": "bonsai tree"}}}, "1")
    _put(client, idx, {"q": {"range": {"n": {"gte": 10}}}}, "2")
    _put(client, idx, {"q": {"bool": {"must": [{"match": {"msg": "tree"}}, {"term": {"n": 5}}]}}}, "3")
    return idx


@pytest.mark.parametrize(
    "doc,expected",
    [
        ({"msg": "A new bonsai tree in the office"}, ["1"]),
        ({"n": 12}, ["2"]),
        ({"msg": "tree", "n": 5}, ["1", "3"]),
        ({"msg": "nothing"}, []),
    ],
)
def test_percolate_single_document(client, perc_idx, doc, expected):
    assert _ids(client, perc_idx, {"percolate": {"field": "q", "document": doc}}) == expected


def test_percolate_multiple_documents_slots(client, perc_idx):
    res = _ok_search(
        client,
        perc_idx,
        {"query": {"percolate": {"field": "q", "documents": [{"msg": "bonsai"}, {"n": 50}, {"msg": "tree", "n": 5}]}}},
    )
    slots = {h["_id"]: h["fields"]["_percolator_document_slot"] for h in res["hits"]["hits"]}
    assert slots == {"1": [0, 2], "2": [1], "3": [2]}


@pytest.mark.parametrize(
    "query,cause,reason",
    [
        ({"match": {"unknown_field": "x"}}, "query_shard_exception", "No field mapping can be found for the field with name [unknown_field]"),
        ("not a query", "parsing_exception", "[_na] query malformed, must start with start_object"),
        ({"bogus_query": {}}, "parsing_exception", "unknown query [bogus_query]"),
    ],
)
def test_percolator_invalid_stored_queries(client, perc_idx, query, cause, reason):
    r = _put(client, perc_idx, {"q": query})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", cause)
    assert _cause_reason(r) == reason


def test_percolate_on_non_percolator_field(client, perc_idx):
    r = _search(client, perc_idx, {"query": {"percolate": {"field": "msg", "document": {"msg": "x"}}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "expected field [msg] to be of type [percolator], but is of type [text]"


# =========================================================================== join


@pytest.fixture
def join_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {"properties": {"jf": {"type": "join", "relations": {"question": ["answer", "comment"]}}, "t": {"type": "text"}}},
    )
    assert _put(client, idx, {"jf": "question", "t": "q1"}, "q1").status_code == 201
    assert _put(client, idx, {"jf": {"name": "question"}, "t": "q2"}, "q2").status_code == 201
    assert _put(client, idx, {"jf": {"name": "answer", "parent": "q1"}, "t": "a1"}, "a1", routing="q1").status_code == 201
    assert _put(client, idx, {"jf": {"name": "comment", "parent": "q2"}, "t": "c1"}, "c1", routing="q2").status_code == 201
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"has_child": {"type": "answer", "query": {"match_all": {}}}}, ["q1"]),
        ({"has_child": {"type": "comment", "query": {"match_all": {}}}}, ["q2"]),
        ({"has_parent": {"parent_type": "question", "query": {"term": {"_id": "q2"}}}}, ["c1"]),
        ({"parent_id": {"type": "answer", "id": "q1"}}, ["a1"]),
        ({"term": {"jf": "question"}}, ["q1", "q2"]),
        ({"term": {"jf#question": "q1"}}, ["a1", "q1"]),
    ],
)
def test_join_queries(client, join_idx, query, expected):
    assert _ids(client, join_idx, query) == expected


def test_join_doc_values_and_agg(client, join_idx):
    res = _ok_search(client, join_idx, {"query": {"ids": {"values": ["a1"]}}, "docvalue_fields": ["jf", "jf#question"]})
    assert res["hits"]["hits"][0]["fields"] == {"jf": ["answer"], "jf#question": ["q1"]}
    res = _ok_search(client, join_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "jf"}}}})
    assert {b["key"]: b["doc_count"] for b in res["aggregations"]["t"]["buckets"]} == {"question": 2, "answer": 1, "comment": 1}


def test_join_mapping_has_eager_global_ordinals(client, join_idx):
    assert _props(client, join_idx)["jf"] == {
        "type": "join",
        "eager_global_ordinals": True,
        "relations": {"question": ["answer", "comment"]},
    }


@pytest.mark.parametrize(
    "doc,params,reason",
    [
        ({"jf": {"name": "answer", "parent": "q1"}}, {}, "[routing] is missing for join field [jf]"),
        ({"jf": "unknown"}, {}, "unknown join name [unknown] for field [jf]"),
        ({"jf": {"name": "answer"}}, {"routing": "q1"}, "[parent] is missing for join field [jf]"),
    ],
)
def test_join_invalid_documents(client, join_idx, doc, params, reason):
    r = _put(client, join_idx, doc, "x", **params)
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == reason


@pytest.mark.parametrize(
    "props,reason",
    [
        (
            {"j1": {"type": "join", "relations": {"a": "b"}}, "j2": {"type": "join", "relations": {"c": "d"}}},
            "Field [_parent_join] is defined more than once",
        ),
        (
            {"o": {"properties": {"j1": {"type": "join", "relations": {"a": "b"}}}}},
            "join field [o.j1] cannot be added inside an object or in a multi-field",
        ),
    ],
)
def test_join_invalid_mappings(try_create, props, reason):
    r = try_create({"properties": props})
    assert r.status_code == 400
    assert _cause_reason(r) == reason


# =========================================================================== wildcard


@pytest.fixture
def wildcard_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "properties": {
                "w": {"type": "wildcard"},
                "wi": {"type": "wildcard", "ignore_above": 5},
                "wn": {"type": "wildcard", "normalizer": "lowercase"},
            }
        },
    )
    _put(client, idx, {"w": "Error: disk FULL on /dev/sda1", "wi": "abc", "wn": "MiXeD"}, "1")
    _put(client, idx, {"w": "warning: cpu hot", "wi": "abcdefg", "wn": "other"}, "2")
    _put(client, idx, {"w": ["x1", "y2"]}, "3")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"wildcard": {"w": "*disk*"}}, ["1"]),
        ({"wildcard": {"w": "*DISK*"}}, []),
        ({"wildcard": {"w": {"value": "*DISK*", "case_insensitive": True}}}, ["1"]),
        ({"regexp": {"w": ".*sda[0-9]"}}, ["1"]),
        ({"term": {"w": "warning: cpu hot"}}, ["2"]),
        ({"term": {"w": "warning"}}, []),
        ({"match": {"w": "warning: cpu hot"}}, ["2"]),
        ({"match": {"w": "warning"}}, []),
        ({"match_phrase": {"w": "cpu hot"}}, []),
        ({"prefix": {"w": "Err"}}, ["1"]),
        ({"wildcard": {"w": "?1"}}, ["3"]),
        ({"terms": {"w": ["x1", "nope"]}}, ["3"]),
        ({"query_string": {"query": "w:*cpu*"}}, ["2"]),
        ({"exists": {"field": "wi"}}, ["1"]),
        ({"wildcard": {"wi": "abc*"}}, ["1"]),
        ({"term": {"wn": "mixed"}}, ["1"]),
        ({"wildcard": {"wn": "MIX*"}}, ["1"]),
    ],
)
def test_wildcard_field_queries(client, wildcard_idx, query, expected):
    assert _ids(client, wildcard_idx, query) == expected


def test_wildcard_field_sort_and_mapping(client, wildcard_idx):
    res = _ok_search(client, wildcard_idx, {"sort": [{"w": "asc"}]})
    assert [(h["_id"], h["sort"]) for h in res["hits"]["hits"]] == [
        ("1", ["Error: disk FULL on /dev/sda1"]),
        ("2", ["warning: cpu hot"]),
        ("3", ["x1"]),
    ]
    assert _props(client, wildcard_idx)["w"] == {"type": "wildcard", "doc_values": True}


@pytest.mark.parametrize(
    "query,reason",
    [
        (
            {"fuzzy": {"w": "x2"}},
            "failed to create query: Can only use fuzzy queries on keyword and text fields - not on [w] which is of type [wildcard]",
        ),
        ({"range": {"w": {"gte": "w"}}}, "failed to create query: TODO"),
    ],
)
def test_wildcard_field_unsupported_queries(client, wildcard_idx, query, reason):
    r = _search(client, wildcard_idx, {"query": query})
    assert r.status_code == 400
    assert _err(r) == ("search_phase_execution_exception", "query_shard_exception")
    assert _cause_reason(r) == reason


# =========================================================================== constant_keyword / match_only_text / derived


@pytest.fixture
def ck_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"ck": {"type": "constant_keyword", "value": "prod"}, "t": {"type": "keyword"}}})
    _put(client, idx, {"t": "a"}, "1")
    _put(client, idx, {"t": "b", "ck": "prod"}, "2")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"ck": "prod"}}, ["1", "2"]),
        ({"term": {"ck": "dev"}}, []),
        ({"terms": {"ck": ["x", "prod"]}}, ["1", "2"]),
        ({"prefix": {"ck": "pr"}}, ["1", "2"]),
        ({"wildcard": {"ck": "p*d"}}, ["1", "2"]),
        ({"regexp": {"ck": "p.*"}}, ["1", "2"]),
        ({"match": {"ck": "prod"}}, ["1", "2"]),
        ({"range": {"ck": {"gte": "a"}}}, ["1", "2"]),
        ({"exists": {"field": "ck"}}, ["1", "2"]),
    ],
)
def test_constant_keyword_queries(client, ck_idx, query, expected):
    assert _ids(client, ck_idx, query) == expected


def test_constant_keyword_rejects_other_value(client, ck_idx):
    r = _put(client, ck_idx, {"ck": "dev"})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == "constant keyword field [ck] must have a value of [prod]"


def test_constant_keyword_value_required_and_immutable(client, ck_idx, try_create):
    r = try_create({"properties": {"f": {"type": "constant_keyword"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "Field [f] is missing required parameter [value]"
    r = _put_mapping(client, ck_idx, {"properties": {"ck": {"type": "constant_keyword", "value": "other"}}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == (
        "Mapper for [ck] conflicts with existing mapper:\n\tCannot update parameter [value] from [prod] to [other]"
    )


def test_constant_keyword_agg_counts_all_docs(client, ck_idx):
    res = _ok_search(client, ck_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "ck"}}}})
    buckets = res["aggregations"]["t"]["buckets"]
    assert len(buckets) == 1 and buckets[0]["doc_count"] == 2
    assert client.get(f"/{ck_idx}/_doc/1").json()["_source"] == {"t": "a"}


@pytest.fixture
def mot_idx(client, index_factory):
    idx = _mk(index_factory, {"properties": {"m": {"type": "match_only_text"}}})
    _put(client, idx, {"m": "the quick brown fox"}, "1")
    _put(client, idx, {"m": "quick quick quick"}, "2")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"match": {"m": "quick"}}, ["1", "2"]),
        ({"match_phrase": {"m": "quick brown"}}, ["1"]),
        ({"match_phrase": {"m": "brown quick"}}, []),
        ({"match_phrase": {"m": {"query": "quick fox", "slop": 1}}}, ["1"]),
        ({"prefix": {"m": "bro"}}, ["1"]),
        ({"match_phrase_prefix": {"m": "quick br"}}, ["1"]),
    ],
)
def test_match_only_text_queries(client, mot_idx, query, expected):
    assert _ids(client, mot_idx, query) == expected


def test_match_only_text_constant_scores(client, mot_idx):
    hits = _ok_search(client, mot_idx, {"query": {"match": {"m": "quick"}}})["hits"]["hits"]
    assert [h["_score"] for h in hits] == [1.0, 1.0]


@pytest.mark.parametrize(
    "query,reason",
    [
        ({"intervals": {"m": {"match": {"query": "quick fox"}}}}, "failed to create query: Cannot create intervals over field [m] with no positions indexed"),
    ],
)
def test_match_only_text_positional_queries_fail(client, mot_idx, query, reason):
    r = _search(client, mot_idx, {"query": query})
    assert r.status_code == 400
    assert _cause_reason(r) == reason


def test_match_only_text_span_query_is_server_error(client, mot_idx):
    r = _search(client, mot_idx, {"query": {"span_term": {"m": "quick"}}})
    assert r.status_code == 500
    assert _err(r) == ("search_phase_execution_exception", "illegal_state_exception")
    assert _cause_reason(r) == 'field "m" was indexed without position data; cannot run SpanTermQuery (term=quick)'


@pytest.mark.parametrize(
    "mapping,reason",
    [
        ({"type": "match_only_text", "index_options": "positions"}, "Unknown value [positions] for field [index_options] - accepted values are [docs]"),
        ({"type": "match_only_text", "norms": True}, "Norms cannot be enabled on for match_only_text field"),
    ],
)
def test_match_only_text_invalid_params(try_create, mapping, reason):
    r = try_create({"properties": {"m": mapping}})
    assert r.status_code == 400
    assert _cause_reason(r) == reason


@pytest.fixture
def derived_idx(client, index_factory):
    idx = _mk(
        index_factory,
        {
            "derived": {
                "up": {"type": "keyword", "script": {"source": "emit(params._source['name'].toUpperCase())"}},
                "dbl": {"type": "long", "script": {"source": "emit(doc['n'].value * 2)"}},
            },
            "properties": {"name": {"type": "keyword"}, "n": {"type": "long"}},
        },
    )
    _put(client, idx, {"name": "alice", "n": 3}, "1")
    _put(client, idx, {"name": "bob", "n": 10}, "2")
    return idx


@pytest.mark.parametrize(
    "query,expected",
    [
        ({"term": {"up": "ALICE"}}, ["1"]),
        ({"prefix": {"up": "B"}}, ["2"]),
        ({"range": {"dbl": {"gte": 10}}}, ["2"]),
        ({"term": {"dbl": 6}}, ["1"]),
    ],
)
def test_derived_field_queries(client, derived_idx, query, expected):
    assert _ids(client, derived_idx, query) == expected


def test_derived_field_fields_and_aggs(client, derived_idx):
    res = _ok_search(client, derived_idx, {"fields": ["up", "dbl"], "sort": [{"n": "asc"}]})
    assert [h["fields"] for h in res["hits"]["hits"]] == [{"up": ["ALICE"], "dbl": [6]}, {"up": ["BOB"], "dbl": [20]}]
    res = _ok_search(client, derived_idx, {"size": 0, "aggs": {"t": {"terms": {"field": "up"}}, "m": {"max": {"field": "dbl"}}}})
    assert [b["key"] for b in res["aggregations"]["t"]["buckets"]] == ["ALICE", "BOB"]
    assert res["aggregations"]["m"]["value"] == 20.0


def test_derived_field_mapping_and_exists_unsupported(client, derived_idx):
    m = client.get(f"/{derived_idx}/_mapping").json()[derived_idx]["mappings"]
    assert m["derived"]["dbl"] == {"type": "long", "script": {"source": "emit(doc['n'].value * 2)", "lang": "painless"}}
    assert set(m["properties"]) == {"name", "n"}
    r = _search(client, derived_idx, {"query": {"exists": {"field": "up"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "failed to create query: Field [up] of type [derived] does not support exist queries"


def test_derived_field_defined_in_search_request(client, derived_idx):
    body = {
        "derived": {"x": {"type": "long", "script": "emit(doc['n'].value + 100)"}},
        "query": {"term": {"x": 103}},
        "fields": ["x"],
    }
    res = _ok_search(client, derived_idx, body)
    assert [(h["_id"], h["fields"]) for h in res["hits"]["hits"]] == [("1", {"x": [103]})]


@pytest.mark.parametrize("ftype", ["flattened", "dense_vector", "histogram"])
def test_types_not_available(try_create, ftype):
    r = try_create({"properties": {"f": {"type": ftype}}})
    assert r.status_code == 400
    assert _cause_reason(r) == f"No handler for type [{ftype}] declared on field [f]"


def test_xy_point_type_available(try_create):
    assert try_create({"properties": {"f": {"type": "xy_point"}}}).status_code == 200


def test_knn_vector_requires_dimension(try_create):
    r = try_create({"properties": {"f": {"type": "knn_vector"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "Dimension value missing for vector: f"


def test_knn_vector_mapping_round_trip(client, try_create):
    r = try_create({"properties": {"f": {"type": "knn_vector", "dimension": 3}}})
    assert r.status_code == 200
    name = r.json()["index"]
    props = client.get(f"/{name}/_mapping").json()[name]["mappings"]["properties"]
    assert props["f"] == {"type": "knn_vector", "doc_values": True, "dimension": 3}


def test_runtime_section_not_supported(try_create):
    r = try_create({"runtime": {"r": {"type": "keyword"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "Root mapping definition has unsupported parameters:  [runtime : {r={type=keyword}}]"


# =========================================================================== mapping updates


@pytest.fixture
def upd_idx(client, index_factory):
    return _mk(
        index_factory,
        {
            "properties": {
                "t": {"type": "text", "analyzer": "standard"},
                "plain": {"type": "text"},
                "k": {"type": "keyword"},
                "n": {"type": "integer"},
                "o": {"properties": {"x": {"type": "keyword"}}},
                "ne": {"type": "nested"},
            }
        },
    )


@pytest.mark.parametrize(
    "props,reason",
    [
        ({"k": {"type": "long"}}, "mapper [k] cannot be changed from type [keyword] to [long]"),
        ({"n": {"type": "long"}}, "mapper [n] cannot be changed from type [integer] to [long]"),
        (
            {"t": {"type": "text", "analyzer": "whitespace"}},
            "Mapper for [t] conflicts with existing mapper:\n\tCannot update parameter [analyzer] from [standard] to [whitespace]",
        ),
        # omitting an explicitly set analyzer counts as changing it back to default
        (
            {"t": {"type": "text", "fields": {"raw": {"type": "keyword"}}}},
            "Mapper for [t] conflicts with existing mapper:\n\tCannot update parameter [analyzer] from [standard] to [default]",
        ),
        (
            {"k": {"type": "keyword", "index": False}},
            "Mapper for [k] conflicts with existing mapper:\n\tCannot update parameter [index] from [true] to [false]",
        ),
        (
            {"k": {"type": "keyword", "doc_values": False}},
            "Mapper for [k] conflicts with existing mapper:\n\tCannot update parameter [doc_values] from [true] to [false]",
        ),
        (
            {"k": {"type": "keyword", "normalizer": "lowercase"}},
            "Mapper for [k] conflicts with existing mapper:\n\tCannot update parameter [normalizer] from [default] to [lowercase]",
        ),
        (
            {"n": {"type": "integer", "null_value": 0}},
            "Mapper for [n] conflicts with existing mapper:\n\tCannot update parameter [null_value] from [null] to [0]",
        ),
        (
            {"plain": {"type": "text", "index_options": "docs"}},
            "Mapper for [plain] conflicts with existing mapper:\n\tCannot update parameter [index_options] from [positions] to [docs]",
        ),
        ({"o": {"type": "nested"}}, "cannot change object mapping from non-nested to nested"),
        ({"ne": {"type": "object"}}, "cannot change object mapping from nested to non-nested"),
        ({"o": {"type": "keyword"}}, "can't merge a non object mapping [o] with an object mapping"),
        ({"k": {"properties": {"x": {"type": "keyword"}}}}, "can't merge a non object mapping [k] with an object mapping"),
    ],
)
def test_mapping_update_conflicts(client, upd_idx, props, reason):
    r = _put_mapping(client, upd_idx, {"properties": props})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == reason


@pytest.mark.parametrize(
    "props",
    [
        {"new": {"type": "keyword"}},
        {"plain": {"type": "text", "fields": {"raw": {"type": "keyword"}}}},
        {"t": {"type": "text", "analyzer": "standard", "fields": {"raw": {"type": "keyword"}}}},
        {"k": {"type": "keyword", "ignore_above": 10}},
        {"n": {"type": "integer", "ignore_malformed": True}},
        {"n": {"type": "integer", "coerce": False}},
        {"plain": {"type": "text", "fielddata": True}},
        {"o": {"properties": {"y": {"type": "long"}}}},
    ],
)
def test_mapping_update_allowed_changes(client, upd_idx, props):
    r = _put_mapping(client, upd_idx, {"properties": props})
    assert r.status_code == 200, r.text
    assert r.json() == {"acknowledged": True}
    name, spec = next(iter(props.items()))
    got = _props(client, upd_idx)[name]
    if "properties" in spec:
        assert got["properties"]["y"] == {"type": "long"}
        assert got["properties"]["x"] == {"type": "keyword"}
    else:
        for key, val in spec.items():
            assert got[key] == val


def test_mapping_update_multi_field_type_change_rejected(client, index_factory):
    idx = _mk(index_factory, {"properties": {"t": {"type": "text", "fields": {"raw": {"type": "keyword"}}}}})
    r = _put_mapping(client, idx, {"properties": {"t": {"type": "text", "fields": {"raw": {"type": "long"}}}}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "mapper [t.raw] cannot be changed from type [keyword] to [long]"


def test_mapping_update_omitting_multi_fields_keeps_them(client, index_factory):
    idx = _mk(index_factory, {"properties": {"t": {"type": "text", "fields": {"raw": {"type": "keyword"}}}}})
    assert _put_mapping(client, idx, {"properties": {"t": {"type": "text"}}}).status_code == 200
    assert _props(client, idx)["t"] == {"type": "text", "fields": {"raw": {"type": "keyword"}}}


def test_mapping_update_new_multi_field_only_indexes_new_docs(client, index_factory):
    idx = _mk(index_factory, {"properties": {"t": {"type": "text"}}})
    _put(client, idx, {"t": "Old Doc"}, "old")
    _put_mapping(client, idx, {"properties": {"t": {"type": "text", "fields": {"raw": {"type": "keyword"}}}}})
    _put(client, idx, {"t": "New Doc"}, "new")
    assert _ids(client, idx, {"exists": {"field": "t.raw"}}) == ["new"]
    assert _ids(client, idx, {"term": {"t.raw": "Old Doc"}}) == []


@pytest.mark.parametrize(
    "props,reason",
    [
        ({"bad": {"type": "nope"}}, "No handler for type [nope] declared on field [bad]"),
        ({"k": {"type": "keyword", "foo": 1}}, "unknown parameter [foo] on mapper [k] of type [keyword]"),
    ],
)
def test_mapping_update_parse_errors(client, upd_idx, props, reason):
    r = _put_mapping(client, upd_idx, {"properties": props})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "mapper_parsing_exception"
    assert r.json()["error"]["reason"] == reason


def test_routing_required_cannot_be_changed(client, upd_idx):
    r = _put_mapping(client, upd_idx, {"_routing": {"required": True}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == (
        "Mapper for [_routing] conflicts with existing mapper:\n\tCannot update parameter [required] from [false] to [true]"
    )


# =========================================================================== mapping limits


def test_total_fields_limit_dynamic(client, index_factory):
    idx = _mk(index_factory, {"properties": {"a": {"type": "keyword"}}}, {"index.mapping.total_fields.limit": 3})
    # b + b.keyword -> exactly 3 fields
    assert _put(client, idx, {"b": "x"}).status_code == 201
    r = _put(client, idx, {"c": 1})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == "Limit of total fields [3] has been exceeded"


def test_total_fields_limit_put_mapping_and_create(client, index_factory, try_create):
    idx = _mk(index_factory, {"properties": {"a": {"type": "keyword"}}}, {"index.mapping.total_fields.limit": 2})
    r = _put_mapping(client, idx, {"properties": {"z": {"type": "keyword"}, "y": {"type": "keyword"}}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "Limit of total fields [2] has been exceeded"
    r = try_create({"properties": {"a": {"type": "keyword"}, "b": {"type": "keyword"}}}, {"index.mapping.total_fields.limit": 1})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == "Limit of total fields [1] has been exceeded"


def test_total_fields_limit_counts_objects(client, index_factory):
    idx = _mk(index_factory, {}, {"index.mapping.total_fields.limit": 1})
    r = _put(client, idx, {"q": {"r": 1}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "Limit of total fields [1] has been exceeded"


def test_depth_limit(client, index_factory):
    idx = _mk(index_factory, {}, {"index.mapping.depth.limit": 2})
    assert _put(client, idx, {"a": {"b": 1}}).status_code == 201
    r = _put(client, idx, {"a": {"c": {"d": 1}}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "parse_exception")
    assert _cause_reason(r) == (
        "The depth of the field has exceeded the allowed limit of [2]. "
        "This limit can be set by changing the [index.mapping.depth.limit] index level setting."
    )


def test_nested_fields_limit(client, index_factory):
    idx = _mk(index_factory, {}, {"index.mapping.nested_fields.limit": 1})
    assert _put_mapping(client, idx, {"properties": {"n1": {"type": "nested"}}}).status_code == 200
    r = _put_mapping(client, idx, {"properties": {"n2": {"type": "nested"}}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "Limit of nested fields [1] has been exceeded"


def test_nested_objects_limit(client, index_factory):
    idx = _mk(index_factory, {"properties": {"n1": {"type": "nested"}}}, {"index.mapping.nested_objects.limit": 2})
    assert _put(client, idx, {"n1": [{"a": 1}, {"a": 2}]}).status_code == 201
    r = _put(client, idx, {"n1": [{"a": 1}, {"a": 2}, {"a": 3}]})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "mapper_parsing_exception"
    assert r.json()["error"]["reason"] == (
        "The number of nested documents has exceeded the allowed limit of [2]. "
        "This limit can be set by changing the [index.mapping.nested_objects.limit] index level setting."
    )


def test_field_name_length_limit_applies_to_metadata_fields(try_create):
    r = try_create({}, {"index.mapping.field_name_length.limit": 5})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == "Field name [_ignored] is longer than the limit of [5] characters"


def test_field_name_length_limit(client, index_factory, try_create):
    # the longest built-in metadata field is _data_stream_timestamp (22 chars)
    r = try_create({}, {"index.mapping.field_name_length.limit": 21})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "Field name [_data_stream_timestamp] is longer than the limit of [21] characters"
    idx = _mk(index_factory, {}, {"index.mapping.field_name_length.limit": 22})
    assert _put(client, idx, {"a" * 22: 1}).status_code == 201
    r = _put(client, idx, {"b" * 23: 1})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == f"Field name [{'b' * 23}] is longer than the limit of [22] characters"
    # the limit applies to each path segment, not to the full dotted path
    assert _put(client, idx, {"o" * 12: {"p" * 12: 1}}).status_code == 201


# =========================================================================== field names


def test_dotted_field_names_expand_to_objects(client, index_factory):
    idx = _mk(index_factory)
    _put(client, idx, {"a.b.c": "x", "a.d": 1}, "1")
    _put(client, idx, {"a": {"b.c": "y"}}, "2")
    p = _props(client, idx)
    assert p["a"]["properties"]["d"] == {"type": "long"}
    assert p["a"]["properties"]["b"]["properties"]["c"]["type"] == "text"
    assert _ids(client, idx, {"term": {"a.b.c.keyword": "x"}}) == ["1"]
    assert _ids(client, idx, {"term": {"a.b.c.keyword": "y"}}) == ["2"]
    # _source keeps the dotted keys verbatim
    assert client.get(f"/{idx}/_doc/1").json()["_source"] == {"a.b.c": "x", "a.d": 1}


def test_dotted_names_in_explicit_mapping(client, index_factory):
    idx = _mk(index_factory, {"properties": {"user.name": {"type": "keyword"}, "user.age": {"type": "integer"}}})
    assert _props(client, idx) == {"user": {"properties": {"age": {"type": "integer"}, "name": {"type": "keyword"}}}}
    _put(client, idx, {"user": {"name": "a", "age": 3}}, "1")
    assert _ids(client, idx, {"term": {"user.name": "a"}}) == ["1"]


def test_dotted_name_conflicts_with_leaf(client, index_factory, try_create):
    r = try_create({"properties": {"user.name": {"type": "keyword"}, "user": {"type": "keyword"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "can't merge a non object mapping [user] with an object mapping"
    idx = _mk(index_factory)
    r = _put(client, idx, {"x": 1, "x.y": 2})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "can't merge a non object mapping [x] with an object mapping"


@pytest.mark.parametrize(
    "name,reason",
    [
        ("", "field name cannot be an empty string"),
        (".", "field name cannot contain only the character [.]"),
        (".a", "object field starting or ending with a [.] makes object resolution ambiguous: [.a]"),
        ("a..b", "object field starting or ending with a [.] makes object resolution ambiguous: [a..b]"),
    ],
)
def test_invalid_field_names_in_documents(client, index_factory, name, reason):
    idx = _mk(index_factory)
    r = _put(client, idx, {name: 1})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == reason


@pytest.mark.parametrize("name", [" ", "a b", "a*b", "a#b", "_foo", "їжак"])
def test_unusual_but_valid_field_names(client, index_factory, name):
    idx = _mk(index_factory)
    assert _put(client, idx, {name: 1}, "1").status_code == 201
    assert _props(client, idx)[name] == {"type": "long"}
    assert _ids(client, idx, {"term": {name: 1}}) == ["1"]


def test_trailing_dot_field_name_is_stripped(client, index_factory):
    idx = _mk(index_factory)
    assert _put(client, idx, {"a.": 1}, "1").status_code == 201
    assert _props(client, idx) == {"a": {"type": "long"}}


@pytest.mark.parametrize("name", ["", "a..b"])
def test_empty_field_name_in_mapping(try_create, name):
    r = try_create({"properties": {name: {"type": "keyword"}}})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "illegal_argument_exception")
    assert _cause_reason(r) == "name cannot be empty string"


@pytest.mark.parametrize("meta", ["_id", "_source", "_index"])
def test_metadata_field_inside_document(client, index_factory, meta):
    idx = _mk(index_factory)
    r = _put(client, idx, {meta: "x"})
    assert r.status_code == 400
    assert _err(r) == ("mapper_parsing_exception", "mapper_parsing_exception")
    assert _cause_reason(r) == (
        f"Field [{meta}] is a metadata field and cannot be added inside a document. Use the index API request parameters."
    )


def test_metadata_field_in_mapping_properties(try_create):
    r = try_create({"properties": {"_id": {"type": "keyword"}}})
    assert r.status_code == 400
    assert _cause_reason(r) == "Field [_id] is defined more than once"


@pytest.mark.parametrize(
    "props,reason",
    [
        ({"f": {"type": "nope"}}, "No handler for type [nope] declared on field [f]"),
        ({"f": {"type": "keyword", "bogus": True}}, "unknown parameter [bogus] on mapper [f] of type [keyword]"),
        ({"f": {"type": "keyword", "ignore_above": "x"}}, None),
    ],
)
def test_invalid_mapping_definitions(try_create, props, reason):
    r = try_create({"properties": props})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "mapper_parsing_exception"
    if reason:
        assert _cause_reason(r) == reason


def test_unicode_values_roundtrip_in_keyword_and_text(client, index_factory):
    idx = _mk(index_factory, {"properties": {"k": {"type": "keyword"}, "t": {"type": "text"}}})
    _put(client, idx, {"k": "Привіт 🌍", "t": "Привіт світ"}, "1")
    assert _ids(client, idx, {"term": {"k": "Привіт 🌍"}}) == ["1"]
    assert _ids(client, idx, {"match": {"t": "привіт"}}) == ["1"]
    assert json.dumps(client.get(f"/{idx}/_doc/1").json()["_source"], ensure_ascii=False) == '{"k": "Привіт 🌍", "t": "Привіт світ"}'
