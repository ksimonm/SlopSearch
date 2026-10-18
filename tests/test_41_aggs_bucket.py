"""Bucket aggregations: exhaustive black-box spec against OpenSearch 3.x behaviour.

Every expectation below is hand-computed from the inline datasets and was
verified against a real OpenSearch node.
"""

import json
import uuid
from datetime import datetime, timezone

import pytest

pytestmark = pytest.mark.aggs

SETTINGS = {"number_of_shards": 1, "number_of_replicas": 0}

# ---------------------------------------------------------------------------
# Main dataset
#
# id | color  | tags  | n  | price | flag | ip            | ts                   | comments
#  1 | red    | a,b   |  1 |  1.5  | T    | 10.0.0.1      | 2024-01-01T00:00:00  | alice5 bob3
#  2 | red    | a     |  2 |  2.5  | F    | 10.0.0.2      | 2024-01-15T12:30:00  | alice4
#  3 | blue   | b,c   |  3 | 10.0  | T    | 10.0.1.5      | 2024-02-29T23:59:59  | carol2
#  4 | blue   | c     |  4 | -3.0  | T    | 192.168.1.1   | 2024-03-31T10:00:00  | []
#  5 | green  | []    |  5 |  7.25 | F    | 192.168.1.200 | 2024-04-01T00:00:00  | -
#  6 | red    | d     |  6 |  0.0  | -    | 2001:db8::1   | 2024-12-31T23:00:00  | -
#  7 | yellow | -     |  7 | 15.0  | T    | -             | 2025-01-01T00:00:00  | -
#  8 | -      | a     |  8 | 100.0 | F    | -             | 2023-06-15T08:00:00  | -
#  9 | blue   | a,c   | -2 | -10.5 | -    | 10.0.0.1      | -                    | -
# 10 | Red    | -     | 10 | -     | -    | -             | -                    | -
# ---------------------------------------------------------------------------

MAIN_MAPPING = {
    "properties": {
        "color": {"type": "keyword"},
        "tags": {"type": "keyword"},
        "n": {"type": "integer"},
        "price": {"type": "double"},
        "flag": {"type": "boolean"},
        "ip": {"type": "ip"},
        "ts": {"type": "date"},
        "body": {"type": "text"},
        "comments": {
            "type": "nested",
            "properties": {"author": {"type": "keyword"}, "stars": {"type": "integer"}},
        },
    }
}

MAIN_DOCS = [
    {"color": "red", "tags": ["a", "b"], "n": 1, "price": 1.5, "flag": True, "ip": "10.0.0.1",
     "ts": "2024-01-01T00:00:00Z", "body": "quick brown fox",
     "comments": [{"author": "alice", "stars": 5}, {"author": "bob", "stars": 3}]},
    {"color": "red", "tags": ["a"], "n": 2, "price": 2.5, "flag": False, "ip": "10.0.0.2",
     "ts": "2024-01-15T12:30:00Z", "body": "lazy brown dog",
     "comments": [{"author": "alice", "stars": 4}]},
    {"color": "blue", "tags": ["b", "c"], "n": 3, "price": 10.0, "flag": True, "ip": "10.0.1.5",
     "ts": "2024-02-29T23:59:59Z", "body": "quick red fox",
     "comments": [{"author": "carol", "stars": 2}]},
    {"color": "blue", "tags": ["c"], "n": 4, "price": -3.0, "flag": True, "ip": "192.168.1.1",
     "ts": "2024-03-31T10:00:00Z", "body": "slow green turtle", "comments": []},
    {"color": "green", "tags": [], "n": 5, "price": 7.25, "flag": False, "ip": "192.168.1.200",
     "ts": "2024-04-01T00:00:00Z", "body": "quick quick fox"},
    {"color": "red", "tags": ["d"], "n": 6, "price": 0.0, "ip": "2001:db8::1",
     "ts": "2024-12-31T23:00:00Z", "body": "brown bear"},
    {"color": "yellow", "n": 7, "price": 15.0, "flag": True, "ts": "2025-01-01T00:00:00Z"},
    {"tags": ["a"], "n": 8, "price": 100.0, "flag": False, "ts": "2023-06-15T08:00:00Z"},
    {"color": "blue", "tags": ["a", "c"], "n": -2, "price": -10.5, "ip": "10.0.0.1"},
    {"color": "Red", "n": 10},
]


# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------


def _bulk(client, index, docs, ids=True, routing=None):
    lines = []
    for i, doc in enumerate(docs):
        meta = {}
        if isinstance(doc, tuple):
            meta, doc = doc
        elif ids:
            meta = {"_id": str(i + 1)}
        lines.append(json.dumps({"index": meta}))
        lines.append(json.dumps(doc, ensure_ascii=False))
    r = client.post(
        f"/{index}/_bulk",
        content=("\n".join(lines) + "\n").encode(),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200, r.text
    assert r.json()["errors"] is False, r.text


def _make_index(client, mappings, docs, prefix="slopsearch-aggb"):
    name = f"{prefix}-{uuid.uuid4().hex[:10]}"
    r = client.put(f"/{name}", json={"settings": SETTINGS, "mappings": mappings})
    assert r.status_code == 200, r.text
    if docs:
        _bulk(client, name, docs)
    return name


@pytest.fixture(scope="module")
def main(client):
    name = _make_index(client, MAIN_MAPPING, MAIN_DOCS)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def _search(client, index, body, params=None):
    return client.post(f"/{index}/_search", json=body, params=params or {})


def _aggs(client, index, aggs, query=None, **extra):
    body = {"size": 0, "aggs": aggs}
    if query is not None:
        body["query"] = query
    body.update(extra)
    r = _search(client, index, body)
    assert r.status_code == 200, r.text
    return r.json()["aggregations"]


def _agg(client, index, agg, query=None, **extra):
    """Run a single aggregation named 'x' and return its result."""
    return _aggs(client, index, {"x": agg}, query=query, **extra)["x"]


def _kc(buckets):
    return [(b["key"], b["doc_count"]) for b in buckets]


def _ksc(buckets):
    return [(b["key_as_string"], b["doc_count"]) for b in buckets]


def _err(client, index, aggs, query=None, params=None):
    body = {"size": 0, "aggs": aggs}
    if query is not None:
        body["query"] = query
    return _search(client, index, body, params)


def _root(r):
    return r.json()["error"]["root_cause"][0]


def ms(s):
    """ISO-8601 instant -> epoch millis."""
    return int(datetime.fromisoformat(s.replace("Z", "+00:00")).astimezone(timezone.utc).timestamp() * 1000)


TERMS_HEADER = {"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0}


# ===========================================================================
# terms
# ===========================================================================


def test_terms_keyword_default_order_count_desc_then_key_asc(client, main):
    res = _agg(client, main, {"terms": {"field": "color"}})
    assert res == {
        **TERMS_HEADER,
        "buckets": [
            {"key": "blue", "doc_count": 3},
            {"key": "red", "doc_count": 3},
            {"key": "Red", "doc_count": 1},
            {"key": "green", "doc_count": 1},
            {"key": "yellow", "doc_count": 1},
        ],
    }


def test_terms_keyword_is_case_sensitive_and_byte_ordered(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "order": {"_key": "asc"}}})
    assert [b["key"] for b in res["buckets"]] == ["Red", "blue", "green", "red", "yellow"]


@pytest.mark.parametrize(
    "size,expected,other",
    [
        (1, [("blue", 3)], 6),
        (2, [("blue", 3), ("red", 3)], 3),
        (3, [("blue", 3), ("red", 3), ("Red", 1)], 2),
        (5, [("blue", 3), ("red", 3), ("Red", 1), ("green", 1), ("yellow", 1)], 0),
        (100, [("blue", 3), ("red", 3), ("Red", 1), ("green", 1), ("yellow", 1)], 0),
    ],
)
def test_terms_size_and_sum_other_doc_count(client, main, size, expected, other):
    res = _agg(client, main, {"terms": {"field": "color", "size": size}})
    assert _kc(res["buckets"]) == expected
    assert res["sum_other_doc_count"] == other
    assert res["doc_count_error_upper_bound"] == 0


def test_terms_multivalued_field_counts_docs_per_value(client, main):
    res = _agg(client, main, {"terms": {"field": "tags"}})
    assert _kc(res["buckets"]) == [("a", 4), ("c", 3), ("b", 2), ("d", 1)]


@pytest.mark.parametrize(
    "order,expected",
    [
        ({"_key": "asc"}, ["a", "b", "c", "d"]),
        ({"_key": "desc"}, ["d", "c", "b", "a"]),
        ({"_count": "asc"}, ["d", "b", "c", "a"]),
        ({"_count": "desc"}, ["a", "c", "b", "d"]),
        ([{"_count": "desc"}], ["a", "c", "b", "d"]),
    ],
)
def test_terms_order_count_and_key(client, main, order, expected):
    res = _agg(client, main, {"terms": {"field": "tags", "order": order}})
    assert [b["key"] for b in res["buckets"]] == expected


def test_terms_count_asc_ties_broken_by_key_asc(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "order": {"_count": "asc"}}})
    assert _kc(res["buckets"]) == [("Red", 1), ("green", 1), ("yellow", 1), ("blue", 3), ("red", 3)]


def test_terms_order_by_single_value_metric_desc(client, main):
    res = _agg(
        client, main,
        {"terms": {"field": "color", "order": {"mx": "desc"}}, "aggs": {"mx": {"max": {"field": "n"}}}},
    )
    assert [(b["key"], b["mx"]["value"]) for b in res["buckets"]] == [
        ("Red", 10.0), ("yellow", 7.0), ("red", 6.0), ("green", 5.0), ("blue", 4.0),
    ]


def test_terms_order_by_single_value_metric_asc(client, main):
    res = _agg(
        client, main,
        {"terms": {"field": "color", "order": {"mn": "asc"}}, "aggs": {"mn": {"min": {"field": "n"}}}},
    )
    assert [b["key"] for b in res["buckets"]] == ["blue", "red", "green", "yellow", "Red"]


@pytest.mark.parametrize("path", ["st.avg", "st[avg]"])
def test_terms_order_by_multi_value_metric_path(client, main, path):
    res = _agg(
        client, main,
        {"terms": {"field": "color", "order": {path: "desc"}}, "aggs": {"st": {"stats": {"field": "price"}}}},
    )
    assert [b["key"] for b in res["buckets"]] == ["yellow", "green", "red", "blue", "Red"]
    red_bucket = res["buckets"][-1]
    assert red_bucket["st"] == {"count": 0, "min": None, "max": None, "avg": None, "sum": 0.0}


def test_terms_order_multiple_criteria(client, main):
    res = _agg(
        client, main,
        {
            "terms": {"field": "color", "order": [{"_count": "desc"}, {"mx": "asc"}]},
            "aggs": {"mx": {"max": {"field": "n"}}},
        },
    )
    assert [b["key"] for b in res["buckets"]] == ["blue", "red", "green", "yellow", "Red"]


def test_terms_order_multiple_criteria_key_desc_tiebreak(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "order": [{"_count": "desc"}, {"_key": "desc"}]}})
    assert [b["key"] for b in res["buckets"]] == ["red", "blue", "yellow", "green", "Red"]


def test_terms_order_by_single_bucket_subagg_doc_count(client, main):
    res = _agg(
        client, main,
        {
            "terms": {"field": "color", "order": [{"f": "desc"}, {"_key": "asc"}]},
            "aggs": {"f": {"filter": {"term": {"flag": True}}}},
        },
    )
    assert [(b["key"], b["f"]["doc_count"]) for b in res["buckets"]] == [
        ("blue", 2), ("red", 1), ("yellow", 1), ("Red", 0), ("green", 0),
    ]


def test_terms_order_by_path_through_single_bucket_agg_nulls_last(client, main):
    res = _agg(
        client, main,
        {
            "terms": {"field": "color", "order": [{"f>mx": "desc"}, {"_key": "asc"}]},
            "aggs": {"f": {"filter": {"term": {"flag": True}}, "aggs": {"mx": {"max": {"field": "n"}}}}},
        },
    )
    assert [(b["key"], b["f"]["mx"]["value"]) for b in res["buckets"]] == [
        ("yellow", 7.0), ("blue", 4.0), ("red", 1.0), ("Red", None), ("green", None),
    ]


def test_terms_order_by_count_path_inside_single_bucket_is_error(client, main):
    r = _err(
        client, main,
        {"x": {"terms": {"field": "color", "order": {"f>_count": "desc"}}, "aggs": {"f": {"filter": {"match_all": {}}}}}},
    )
    assert r.status_code == 500
    assert _root(r)["reason"] == (
        "Invalid aggregation order path [f>_count]. The provided aggregation [_count] either does not exist, "
        "or is a pipeline aggregation and cannot be used to sort the buckets."
    )


def test_terms_order_by_multi_bucket_subagg_is_error(client, main):
    r = _err(
        client, main,
        {"x": {"terms": {"field": "color", "order": {"t": "desc"}}, "aggs": {"t": {"terms": {"field": "tags"}}}}},
    )
    assert r.status_code == 500
    assert _root(r)["type"] == "aggregation_execution_exception"
    assert _root(r)["reason"] == (
        "Invalid aggregation order path [t]. Buckets can only be sorted on a sub-aggregator path that is built out "
        "of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics "
        "aggregation at the path end."
    )


def test_terms_order_by_nonexistent_subagg_is_error(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color", "order": {"nonexistent": "asc"}}}})
    assert r.status_code == 500
    assert _root(r)["type"] == "aggregation_execution_exception"
    assert _root(r)["reason"] == (
        "Invalid aggregation order path [nonexistent]. The provided aggregation [nonexistent] either "
        "does not exist, or is a pipeline aggregation and cannot be used to sort the buckets."
    )


def test_terms_order_by_multi_value_metric_without_key_is_error(client, main):
    r = _err(
        client, main,
        {"x": {"terms": {"field": "color", "order": {"st": "asc"}}, "aggs": {"st": {"stats": {"field": "price"}}}}},
    )
    assert r.status_code == 500
    assert _root(r)["reason"] == (
        "Invalid aggregation order path [st]. When ordering on a multi-value metrics aggregation "
        "a metric name must be specified."
    )


def test_terms_order_bad_direction_is_error(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color", "order": {"_count": "sideways"}}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "parsing_exception"
    assert _root(r)["reason"] == "Unknown order direction [sideways]"


def test_terms_size_zero_is_rejected(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color", "size": 0}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == "[size] must be greater than 0. Found [0] in [x]"


def test_terms_min_doc_count_filters_buckets(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "min_doc_count": 2}})
    assert _kc(res["buckets"]) == [("blue", 3), ("red", 3)]
    assert res["sum_other_doc_count"] == 0


def test_terms_min_doc_count_zero_includes_terms_from_non_matching_docs(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "min_doc_count": 0}}, query={"term": {"color": "blue"}})
    assert _kc(res["buckets"]) == [("blue", 3), ("Red", 0), ("green", 0), ("red", 0), ("yellow", 0)]


def test_terms_min_doc_count_zero_numeric(client, main):
    res = _agg(
        client, main, {"terms": {"field": "n", "min_doc_count": 0, "size": 4}},
        query={"range": {"n": {"gte": 7}}},
    )
    assert _kc(res["buckets"]) == [(7, 1), (8, 1), (10, 1), (-2, 0)]


@pytest.mark.parametrize(
    "include,exclude,expected",
    [
        ("r.*", None, [("red", 3)]),
        (".*e.*", None, [("blue", 3), ("red", 3), ("Red", 1), ("green", 1), ("yellow", 1)]),
        ("[Rr]ed", None, [("red", 3), ("Red", 1)]),
        (None, "r.*|y.*", [("blue", 3), ("Red", 1), ("green", 1)]),
        (".*", "blue", [("red", 3), ("Red", 1), ("green", 1), ("yellow", 1)]),
        ("b.*|g.*", "g.*", [("blue", 3)]),
        (["red", "Red", "zzz"], None, [("red", 3), ("Red", 1)]),
        (None, ["red", "blue"], [("Red", 1), ("green", 1), ("yellow", 1)]),
        (["red", "Red"], ["red"], [("Red", 1)]),
        ("re", None, []),
    ],
)
def test_terms_include_exclude(client, main, include, exclude, expected):
    body = {"field": "color"}
    if include is not None:
        body["include"] = include
    if exclude is not None:
        body["exclude"] = exclude
    res = _agg(client, main, {"terms": body})
    assert _kc(res["buckets"]) == expected


def test_terms_include_exclude_numeric_arrays(client, main):
    inc = _agg(client, main, {"terms": {"field": "n", "include": [1, 2, 10]}})
    exc = _agg(client, main, {"terms": {"field": "n", "exclude": [1, 2, 10]}})
    assert [b["key"] for b in inc["buckets"]] == [1, 2, 10]
    assert [b["key"] for b in exc["buckets"]] == [-2, 3, 4, 5, 6, 7, 8]


def test_terms_include_invalid_regex_is_400(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color", "include": "[["}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "illegal_argument_exception"
    assert _root(r)["reason"] == "expected ']' at position 2"


@pytest.mark.parametrize(
    "partition,expected",
    [(0, ["red", "Red"]), (1, ["yellow"]), (2, ["blue", "green"])],
)
def test_terms_include_partitions_keyword(client, main, partition, expected):
    res = _agg(client, main, {"terms": {"field": "color", "include": {"partition": partition, "num_partitions": 3}}})
    assert [b["key"] for b in res["buckets"]] == expected


def test_terms_partitions_are_disjoint_and_complete(client, main):
    seen = []
    for p in range(4):
        res = _agg(client, main, {"terms": {"field": "n", "include": {"partition": p, "num_partitions": 4}}})
        seen.extend(b["key"] for b in res["buckets"])
    assert sorted(seen) == [-2, 1, 2, 3, 4, 5, 6, 7, 8, 10]
    assert len(seen) == len(set(seen))


@pytest.mark.parametrize("partition,expected", [(0, [-2, 3, 4, 5, 6, 7, 10]), (1, [1, 2, 8])])
def test_terms_include_partitions_numeric(client, main, partition, expected):
    res = _agg(client, main, {"terms": {"field": "n", "include": {"partition": partition, "num_partitions": 2}}})
    assert [b["key"] for b in res["buckets"]] == expected


def test_terms_missing_keyword(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "missing": "none"}})
    assert _kc(res["buckets"]) == [
        ("blue", 3), ("red", 3), ("Red", 1), ("green", 1), ("none", 1), ("yellow", 1),
    ]


def test_terms_missing_value_merges_with_existing_term(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "missing": "red"}})
    assert _kc(res["buckets"])[0] == ("red", 4)


def test_terms_missing_on_multivalued_counts_docs_without_values(client, main):
    res = _agg(client, main, {"terms": {"field": "tags", "missing": "NONE"}})
    assert _kc(res["buckets"]) == [("a", 4), ("NONE", 3), ("c", 3), ("b", 2), ("d", 1)]


def test_terms_missing_numeric(client, main):
    res = _agg(client, main, {"terms": {"field": "price", "missing": -1, "size": 3, "order": {"_key": "asc"}}})
    assert _kc(res["buckets"]) == [(-10.5, 1), (-3.0, 1), (-1.0, 1)]


def test_terms_unmapped_field_empty_buckets(client, main):
    assert _agg(client, main, {"terms": {"field": "nope"}}) == {**TERMS_HEADER, "buckets": []}


def test_terms_unmapped_field_with_value_type(client, main):
    assert _agg(client, main, {"terms": {"field": "nope", "value_type": "long"}}) == {**TERMS_HEADER, "buckets": []}


def test_terms_unmapped_with_missing_puts_all_docs_in_one_bucket(client, main):
    res = _agg(client, main, {"terms": {"field": "nope", "missing": "x"}})
    assert res["buckets"] == [{"key": "x", "doc_count": 10}]


def test_terms_unmapped_with_missing_and_value_type_long(client, main):
    res = _agg(client, main, {"terms": {"field": "nope", "missing": 5, "value_type": "long"}})
    assert res["buckets"] == [{"key": 5, "doc_count": 10}]


def test_terms_integer_keys_are_json_numbers(client, main):
    res = _agg(client, main, {"terms": {"field": "n"}})
    assert _kc(res["buckets"]) == [(-2, 1), (1, 1), (2, 1), (3, 1), (4, 1), (5, 1), (6, 1), (7, 1), (8, 1), (10, 1)]
    assert all(isinstance(b["key"], int) for b in res["buckets"])
    assert all("key_as_string" not in b for b in res["buckets"])


def test_terms_double_keys(client, main):
    res = _agg(client, main, {"terms": {"field": "price", "size": 3}})
    assert res["buckets"] == [
        {"key": -10.5, "doc_count": 1}, {"key": -3.0, "doc_count": 1}, {"key": 0.0, "doc_count": 1},
    ]
    assert res["sum_other_doc_count"] == 6


def test_terms_boolean_key_and_key_as_string(client, main):
    res = _agg(client, main, {"terms": {"field": "flag"}})
    assert res["buckets"] == [
        {"key": 1, "key_as_string": "true", "doc_count": 4},
        {"key": 0, "key_as_string": "false", "doc_count": 3},
    ]


def test_terms_ip_keys_are_strings_sorted_by_binary(client, main):
    res = _agg(client, main, {"terms": {"field": "ip", "order": {"_key": "asc"}}})
    assert _kc(res["buckets"]) == [
        ("10.0.0.1", 2), ("10.0.0.2", 1), ("10.0.1.5", 1),
        ("192.168.1.1", 1), ("192.168.1.200", 1), ("2001:db8::1", 1),
    ]


def test_terms_date_key_and_key_as_string(client, main):
    res = _agg(client, main, {"terms": {"field": "ts", "size": 2}})
    assert res["buckets"] == [
        {"key": 1686816000000, "key_as_string": "2023-06-15T08:00:00.000Z", "doc_count": 1},
        {"key": 1704067200000, "key_as_string": "2024-01-01T00:00:00.000Z", "doc_count": 1},
    ]
    assert res["sum_other_doc_count"] == 6


def test_terms_date_with_format(client, main):
    res = _agg(client, main, {"terms": {"field": "ts", "size": 2, "format": "yyyy-MM-dd"}})
    assert [b["key_as_string"] for b in res["buckets"]] == ["2023-06-15", "2024-01-01"]


def test_terms_script_keys_are_strings(client, main):
    res = _agg(client, main, {"terms": {"script": {"source": "doc['n'].value % 3", "lang": "painless"}}})
    assert _kc(res["buckets"]) == [("1", 4), ("2", 3), ("0", 2), ("-2", 1)]


def test_terms_script_string_values(client, main):
    src = "doc['color'].size()==0 ? 'none' : doc['color'].value.toUpperCase()"
    res = _agg(client, main, {"terms": {"script": {"source": src}}})
    assert _kc(res["buckets"]) == [("RED", 4), ("BLUE", 3), ("GREEN", 1), ("YELLOW", 1), ("none", 1)]


def test_terms_value_script_on_field(client, main):
    res = _agg(client, main, {"terms": {"field": "n", "script": {"source": "_value * 2"}, "size": 3}})
    assert res["buckets"] == [{"key": -4.0, "doc_count": 1}, {"key": 2.0, "doc_count": 1}, {"key": 4.0, "doc_count": 1}]


@pytest.mark.parametrize("hint", ["map", "global_ordinals"])
def test_terms_execution_hint_does_not_change_result(client, main, hint):
    res = _agg(client, main, {"terms": {"field": "color", "execution_hint": hint, "size": 2}})
    assert _kc(res["buckets"]) == [("blue", 3), ("red", 3)]
    assert res["sum_other_doc_count"] == 3


def test_terms_execution_hint_invalid(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color", "execution_hint": "bogus"}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Unknown `execution_hint`: [bogus], expected any of [map, global_ordinals]"


@pytest.mark.parametrize("mode", ["breadth_first", "depth_first"])
def test_terms_collect_mode(client, main, mode):
    res = _agg(
        client, main,
        {"terms": {"field": "color", "collect_mode": mode, "size": 2}, "aggs": {"t": {"terms": {"field": "tags"}}}},
    )
    assert [(b["key"], _kc(b["t"]["buckets"])) for b in res["buckets"]] == [
        ("blue", [("c", 3), ("a", 1), ("b", 1)]),
        ("red", [("a", 2), ("b", 1), ("d", 1)]),
    ]


def test_terms_collect_mode_invalid(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color", "collect_mode": "bogus"}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "no [collect_mode] found for value [bogus]"


def test_terms_show_term_doc_count_error(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "size": 2, "show_term_doc_count_error": True}})
    assert res["buckets"] == [
        {"key": "blue", "doc_count": 3, "doc_count_error_upper_bound": 0},
        {"key": "red", "doc_count": 3, "doc_count_error_upper_bound": 0},
    ]


@pytest.mark.parametrize("shard_size,size,expected_len,other", [(1, 1, 1, 6), (1, 3, 3, 2), (10, 2, 2, 3)])
def test_terms_shard_size(client, main, shard_size, size, expected_len, other):
    res = _agg(client, main, {"terms": {"field": "color", "shard_size": shard_size, "size": size}})
    assert len(res["buckets"]) == expected_len
    assert res["sum_other_doc_count"] == other
    assert res["doc_count_error_upper_bound"] == 0


def test_terms_on_text_field_without_fielddata_is_400(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "body"}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "illegal_argument_exception"
    assert _root(r)["reason"].startswith("Text fields are not optimised for operations")


def test_terms_respects_query(client, main):
    res = _agg(client, main, {"terms": {"field": "tags"}}, query={"term": {"flag": True}})
    assert _kc(res["buckets"]) == [("b", 2), ("c", 2), ("a", 1)]


def test_terms_unknown_parameter_is_400(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "n", "bogus": 1}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "x_content_parse_exception"
    assert _root(r)["reason"].endswith("[terms] unknown field [bogus]")


# ===========================================================================
# multi_terms
# ===========================================================================


def test_multi_terms_keyword_boolean(client, main):
    res = _agg(client, main, {"multi_terms": {"terms": [{"field": "color"}, {"field": "flag"}]}})
    assert res == {
        **TERMS_HEADER,
        "buckets": [
            {"key": ["blue", True], "key_as_string": "blue|true", "doc_count": 2},
            {"key": ["green", False], "key_as_string": "green|false", "doc_count": 1},
            {"key": ["red", False], "key_as_string": "red|false", "doc_count": 1},
            {"key": ["red", True], "key_as_string": "red|true", "doc_count": 1},
            {"key": ["yellow", True], "key_as_string": "yellow|true", "doc_count": 1},
        ],
    }


def test_multi_terms_multivalued_cartesian(client, main):
    res = _agg(client, main, {"multi_terms": {"terms": [{"field": "color"}, {"field": "tags"}]}})
    assert [(b["key_as_string"], b["doc_count"]) for b in res["buckets"]] == [
        ("blue|c", 3), ("red|a", 2), ("blue|a", 1), ("blue|b", 1), ("red|b", 1), ("red|d", 1),
    ]


def test_multi_terms_size_order_key_desc(client, main):
    res = _agg(
        client, main,
        {"multi_terms": {"terms": [{"field": "color"}, {"field": "n"}], "size": 2, "order": {"_key": "desc"}}},
    )
    assert res["buckets"] == [
        {"key": ["yellow", 7], "key_as_string": "yellow|7", "doc_count": 1},
        {"key": ["red", 6], "key_as_string": "red|6", "doc_count": 1},
    ]
    assert res["sum_other_doc_count"] == 7


def test_multi_terms_missing(client, main):
    res = _agg(
        client, main,
        {"multi_terms": {"terms": [{"field": "color"}, {"field": "tags", "missing": "zz"}], "size": 3}},
    )
    assert [(b["key"], b["doc_count"]) for b in res["buckets"]] == [
        (["blue", "c"], 3), (["red", "a"], 2), (["Red", "zz"], 1),
    ]
    assert res["sum_other_doc_count"] == 6


def test_multi_terms_ip_and_date_keys_are_formatted(client, main):
    res = _agg(client, main, {"multi_terms": {"terms": [{"field": "ip"}, {"field": "ts"}], "size": 2}})
    assert res["buckets"][0] == {
        "key": ["10.0.0.1", "2024-01-01T00:00:00.000Z"],
        "key_as_string": "10.0.0.1|2024-01-01T00:00:00.000Z",
        "doc_count": 1,
    }
    assert res["sum_other_doc_count"] == 4


def test_multi_terms_order_by_sub_agg(client, main):
    res = _agg(
        client, main,
        {
            "multi_terms": {"terms": [{"field": "color"}, {"field": "flag"}], "order": {"s": "desc"}},
            "aggs": {"s": {"sum": {"field": "price"}}},
        },
    )
    assert [(b["key_as_string"], b["s"]["value"]) for b in res["buckets"]] == [
        ("yellow|true", 15.0), ("green|false", 7.25), ("blue|true", 7.0), ("red|false", 2.5), ("red|true", 1.5),
    ]


def test_multi_terms_requires_two_terms(client, main):
    r = _err(client, main, {"x": {"multi_terms": {"terms": [{"field": "color"}]}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == (
        "multi term aggregation must has at least 2 terms. Found [1] in [x] Use terms aggregation for single term aggregation"
    )


# ===========================================================================
# rare_terms
# ===========================================================================


def test_rare_terms_default_max_doc_count_one(client, main):
    res = _agg(client, main, {"rare_terms": {"field": "color"}})
    assert res == {"buckets": [
        {"key": "Red", "doc_count": 1}, {"key": "green", "doc_count": 1}, {"key": "yellow", "doc_count": 1},
    ]}


def test_rare_terms_max_doc_count_orders_by_count_asc(client, main):
    res = _agg(client, main, {"rare_terms": {"field": "tags", "max_doc_count": 2}})
    assert _kc(res["buckets"]) == [("d", 1), ("b", 2)]


def test_rare_terms_numeric_with_include(client, main):
    res = _agg(client, main, {"rare_terms": {"field": "n", "include": [1, 2]}})
    assert res["buckets"] == [{"key": 1, "doc_count": 1}, {"key": 2, "doc_count": 1}]


def test_rare_terms_with_query(client, main):
    res = _agg(client, main, {"rare_terms": {"field": "tags"}}, query={"term": {"color": "red"}})
    assert _kc(res["buckets"]) == [("b", 1), ("d", 1)]


def test_rare_terms_max_doc_count_limit(client, main):
    r = _err(client, main, {"x": {"rare_terms": {"field": "color", "max_doc_count": 101}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == "[max_doc_count] must be smallerthan 100in [x]"


# ===========================================================================
# significant_terms / significant_text
# ===========================================================================

SIG_DOCS = [
    {"cat": "crime", "kw": ["bike", "theft"], "txt": "stolen bike reported downtown"},
    {"cat": "crime", "kw": ["bike", "theft"], "txt": "bike stolen near station"},
    {"cat": "crime", "kw": ["bike"], "txt": "another bike theft today"},
    {"cat": "crime", "kw": ["car"], "txt": "car window smashed downtown"},
    {"cat": "news", "kw": ["car"], "txt": "new car model released today"},
    {"cat": "news", "kw": ["car"], "txt": "car prices rising today"},
    {"cat": "news", "kw": ["weather"], "txt": "sunny weather today"},
    {"cat": "news", "kw": ["car", "weather"], "txt": "rain slows car traffic"},
    {"cat": "news", "kw": ["sport"], "txt": "local team wins today"},
    {"cat": "news", "kw": ["sport", "bike"], "txt": "bike race today"},
]


@pytest.fixture(scope="module")
def sig(client):
    name = _make_index(
        client,
        {"properties": {"cat": {"type": "keyword"}, "kw": {"type": "keyword"}, "txt": {"type": "text"}}},
        SIG_DOCS,
    )
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


CRIME = {"term": {"cat": "crime"}}


def test_significant_terms_default_min_doc_count_three(client, sig):
    res = _agg(client, sig, {"significant_terms": {"field": "kw"}}, query=CRIME)
    assert res == {
        "doc_count": 4,
        "bg_count": 10,
        "buckets": [{"key": "bike", "doc_count": 3, "score": 0.65625, "bg_count": 4}],
    }


def test_significant_terms_jlh_scores(client, sig):
    res = _agg(client, sig, {"significant_terms": {"field": "kw", "min_doc_count": 1}}, query=CRIME)
    assert res["buckets"] == [
        {"key": "theft", "doc_count": 2, "score": 0.75, "bg_count": 2},
        {"key": "bike", "doc_count": 3, "score": 0.65625, "bg_count": 4},
    ]


def test_significant_terms_background_filter(client, sig):
    res = _agg(
        client, sig,
        {"significant_terms": {"field": "kw", "min_doc_count": 1, "background_filter": {"term": {"cat": "news"}}}},
        query=CRIME,
    )
    assert res["doc_count"] == 4
    assert res["bg_count"] == 6
    assert res["buckets"] == [
        {"key": "bike", "doc_count": 3, "score": 2.625, "bg_count": 1},
        {"key": "theft", "doc_count": 2, "score": 1.0, "bg_count": 0},
    ]


def test_significant_terms_percentage(client, sig):
    res = _agg(client, sig, {"significant_terms": {"field": "kw", "min_doc_count": 1, "percentage": {}}}, query=CRIME)
    assert [(b["key"], b["score"]) for b in res["buckets"]] == [("theft", 1.0), ("bike", 0.75), ("car", 0.25)]


def test_significant_terms_chi_square(client, sig):
    res = _agg(client, sig, {"significant_terms": {"field": "kw", "min_doc_count": 1, "chi_square": {}}}, query=CRIME)
    assert [b["key"] for b in res["buckets"]] == ["theft", "bike"]
    assert res["buckets"][0]["score"] == pytest.approx(3.75)
    assert res["buckets"][1]["score"] == pytest.approx(3.4027777777777777)


def test_significant_terms_mutual_information(client, sig):
    res = _agg(
        client, sig, {"significant_terms": {"field": "kw", "min_doc_count": 1, "mutual_information": {}}}, query=CRIME,
    )
    assert [b["key"] for b in res["buckets"]] == ["theft", "bike"]
    assert res["buckets"][0]["score"] == pytest.approx(0.32192809488736235)


def test_significant_terms_gnd(client, sig):
    res = _agg(client, sig, {"significant_terms": {"field": "kw", "min_doc_count": 1, "gnd": {}}}, query=CRIME)
    assert [b["key"] for b in res["buckets"]] == ["bike", "theft", "car"]
    assert res["buckets"][0]["score"] == pytest.approx(0.7305455114033804)


def test_significant_terms_as_sub_agg_of_terms(client, sig):
    res = _agg(
        client, sig,
        {"terms": {"field": "cat"}, "aggs": {"s": {"significant_terms": {"field": "kw", "min_doc_count": 1}}}},
    )
    by_cat = {b["key"]: b["s"] for b in res["buckets"]}
    assert by_cat["crime"]["doc_count"] == 4 and by_cat["crime"]["bg_count"] == 10
    assert [b["key"] for b in by_cat["crime"]["buckets"]] == ["theft", "bike"]
    assert by_cat["news"]["doc_count"] == 6
    assert {b["key"] for b in by_cat["news"]["buckets"][:2]} == {"weather", "sport"}
    assert by_cat["news"]["buckets"][2]["key"] == "car"
    assert by_cat["news"]["buckets"][2]["bg_count"] == 4


def test_significant_terms_no_matching_docs(client, sig):
    res = _agg(client, sig, {"significant_terms": {"field": "kw"}}, query={"term": {"cat": "nothing"}})
    assert res == {"doc_count": 0, "bg_count": 10, "buckets": []}


def test_significant_text(client, sig):
    res = _agg(client, sig, {"significant_text": {"field": "txt", "min_doc_count": 2}}, query=CRIME)
    assert res["doc_count"] == 4 and res["bg_count"] == 10
    assert {b["key"] for b in res["buckets"][:2]} == {"downtown", "stolen"}
    assert res["buckets"][2] == {"key": "bike", "doc_count": 3, "score": 0.65625, "bg_count": 4}
    assert len(res["buckets"]) == 3


def test_significant_text_filter_duplicate_text(client, sig):
    res = _agg(
        client, sig,
        {"significant_text": {"field": "txt", "min_doc_count": 1, "filter_duplicate_text": True, "size": 3}},
        query=CRIME,
    )
    assert {b["key"] for b in res["buckets"][:2]} == {"downtown", "stolen"}
    assert res["buckets"][2]["key"] == "bike"


# ===========================================================================
# histogram
# ===========================================================================
# price values: -10.5 -3 0 1.5 2.5 7.25 10 15 100 (doc 10 has none)
# n values:     -2 1 2 3 4 5 6 7 8 10


def test_histogram_fills_gaps_by_default(client, main):
    res = _agg(client, main, {"histogram": {"field": "price", "interval": 5}})
    keys = [b["key"] for b in res["buckets"]]
    assert keys == [float(k) for k in range(-15, 105, 5)]
    counts = {b["key"]: b["doc_count"] for b in res["buckets"]}
    assert counts[-15.0] == 1 and counts[-10.0] == 0 and counts[-5.0] == 1
    assert counts[0.0] == 3 and counts[5.0] == 1 and counts[10.0] == 1 and counts[15.0] == 1
    assert counts[100.0] == 1
    assert sum(counts.values()) == 9


def test_histogram_min_doc_count_one(client, main):
    res = _agg(client, main, {"histogram": {"field": "price", "interval": 5, "min_doc_count": 1}})
    assert res == {"buckets": [
        {"key": -15.0, "doc_count": 1}, {"key": -5.0, "doc_count": 1}, {"key": 0.0, "doc_count": 3},
        {"key": 5.0, "doc_count": 1}, {"key": 10.0, "doc_count": 1}, {"key": 15.0, "doc_count": 1},
        {"key": 100.0, "doc_count": 1},
    ]}


def test_histogram_float_interval(client, main):
    res = _agg(client, main, {"histogram": {"field": "price", "interval": 2.5, "min_doc_count": 1}})
    assert _kc(res["buckets"]) == [
        (-12.5, 1), (-5.0, 1), (0.0, 2), (2.5, 1), (5.0, 1), (10.0, 1), (15.0, 1), (100.0, 1),
    ]


def test_histogram_offset(client, main):
    res = _agg(client, main, {"histogram": {"field": "price", "interval": 5, "offset": 2, "min_doc_count": 1}})
    assert _kc(res["buckets"]) == [(-13.0, 1), (-3.0, 3), (2.0, 1), (7.0, 2), (12.0, 1), (97.0, 1)]


def test_histogram_negative_values_floor_not_truncate(client, main):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 4, "min_doc_count": 1}})
    assert _kc(res["buckets"]) == [(-4.0, 1), (0.0, 3), (4.0, 4), (8.0, 2)]


def test_histogram_integer_field_keys_are_doubles(client, main):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 4}})
    assert all(isinstance(b["key"], float) for b in res["buckets"])


def test_histogram_extended_bounds(client, main):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 3, "extended_bounds": {"min": -10, "max": 20}}})
    assert _kc(res["buckets"]) == [
        (-12.0, 0), (-9.0, 0), (-6.0, 0), (-3.0, 1), (0.0, 2), (3.0, 3), (6.0, 3), (9.0, 1),
        (12.0, 0), (15.0, 0), (18.0, 0),
    ]


def test_histogram_extended_bounds_ignored_with_min_doc_count_one(client, main):
    res = _agg(
        client, main,
        {"histogram": {"field": "n", "interval": 3, "min_doc_count": 1, "extended_bounds": {"min": -10, "max": 20}}},
    )
    assert _kc(res["buckets"]) == [(-3.0, 1), (0.0, 2), (3.0, 3), (6.0, 3), (9.0, 1)]


def test_histogram_extended_bounds_min_greater_than_max_is_400(client, main):
    r = _err(client, main, {"x": {"histogram": {"field": "n", "interval": 3, "extended_bounds": {"min": 5, "max": 1}}}})
    assert r.status_code == 400
    assert "max bound [1.0] must be greater than min bound [5.0]" in r.text


def test_histogram_hard_bounds(client, main):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 3, "hard_bounds": {"min": 0, "max": 6}}})
    assert _kc(res["buckets"]) == [(0.0, 2), (3.0, 3), (6.0, 3)]


def test_histogram_keyed(client, main):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 4, "keyed": True}})
    assert res == {"buckets": {
        "-4.0": {"key": -4.0, "doc_count": 1},
        "0.0": {"key": 0.0, "doc_count": 3},
        "4.0": {"key": 4.0, "doc_count": 4},
        "8.0": {"key": 8.0, "doc_count": 2},
    }}


def test_histogram_keyed_preserves_key_order(client, main):
    r = _search(client, main, {"size": 0, "aggs": {"x": {"histogram": {"field": "price", "interval": 0.5, "keyed": True, "min_doc_count": 1}}}})
    keys = list(r.json()["aggregations"]["x"]["buckets"].keys())
    assert keys == ["-10.5", "-3.0", "0.0", "1.5", "2.5", "7.0", "10.0", "15.0", "100.0"]


def test_histogram_missing(client, main):
    res = _agg(client, main, {"histogram": {"field": "price", "interval": 50, "missing": 60}})
    assert _kc(res["buckets"]) == [(-50.0, 2), (0.0, 6), (50.0, 1), (100.0, 1)]


@pytest.mark.parametrize(
    "order,expected",
    [
        ({"_count": "desc"}, [4.0, 0.0, 8.0, -4.0]),
        ({"_count": "asc"}, [-4.0, 8.0, 0.0, 4.0]),
        ({"_key": "desc"}, [8.0, 4.0, 0.0, -4.0]),
        ({"_key": "asc"}, [-4.0, 0.0, 4.0, 8.0]),
    ],
)
def test_histogram_order(client, main, order, expected):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 4, "order": order}})
    assert [b["key"] for b in res["buckets"]] == expected


def test_histogram_order_by_sub_agg(client, main):
    res = _agg(
        client, main,
        {"histogram": {"field": "n", "interval": 4, "order": {"s": "desc"}}, "aggs": {"s": {"sum": {"field": "n"}}}},
    )
    assert [(b["key"], b["s"]["value"]) for b in res["buckets"]] == [
        (4.0, 22.0), (8.0, 18.0), (0.0, 6.0), (-4.0, -2.0),
    ]


@pytest.mark.parametrize("interval", [0, -1])
def test_histogram_non_positive_interval_is_400(client, main, interval):
    r = _err(client, main, {"x": {"histogram": {"field": "n", "interval": interval}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == "[interval] must be >0 for histogram aggregation [x]"


def test_histogram_missing_interval_is_400(client, main):
    r = _err(client, main, {"x": {"histogram": {"field": "n"}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "interval must be positive, got: 0.0"


def test_histogram_on_keyword_is_400(client, main):
    r = _err(client, main, {"x": {"histogram": {"field": "color", "interval": 1}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Field [color] of type [keyword] is not supported for aggregation [histogram]"


def test_histogram_unmapped_field(client, main):
    assert _agg(client, main, {"histogram": {"field": "nope", "interval": 1}}) == {"buckets": []}


def test_histogram_with_query_restricts_range_of_buckets(client, main):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 1}}, query={"terms": {"n": [3, 6]}})
    assert _kc(res["buckets"]) == [(3.0, 1), (4.0, 0), (5.0, 0), (6.0, 1)]


# ===========================================================================
# date_histogram
# ===========================================================================

DATE_DOCS = [
    "2023-12-31T23:59:59.999Z",
    "2024-01-01T00:00:00Z",
    "2024-01-01T00:00:30Z",
    "2024-01-01T00:01:00Z",
    "2024-01-01T01:59:00Z",
    "2024-01-31T23:59:59Z",
    "2024-02-01T00:00:00Z",
    "2024-03-31T23:59:59Z",
    "2024-04-01T00:00:00Z",
    "2024-06-30T12:00:00Z",
    "2024-07-01T00:00:00Z",
    "2024-12-31T23:59:59Z",
    "2025-01-01T00:00:00Z",
]


@pytest.fixture(scope="module")
def dates(client):
    name = _make_index(
        client,
        {"properties": {"t": {"type": "date"}, "v": {"type": "integer"}}},
        [{"t": t, "v": i} for i, t in enumerate(DATE_DOCS)],
    )
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def _dh(client, index, query=None, **params):
    params.setdefault("field", "t")
    return _agg(client, index, {"date_histogram": params}, query=query)["buckets"]


def test_date_histogram_bucket_shape(client, dates):
    b = _dh(client, dates, calendar_interval="year")
    assert b == [
        {"key_as_string": "2023-01-01T00:00:00.000Z", "key": 1672531200000, "doc_count": 1},
        {"key_as_string": "2024-01-01T00:00:00.000Z", "key": 1704067200000, "doc_count": 11},
        {"key_as_string": "2025-01-01T00:00:00.000Z", "key": 1735689600000, "doc_count": 1},
    ]


def test_date_histogram_quarter_fills_gaps(client, dates):
    b = _dh(client, dates, calendar_interval="quarter")
    assert _ksc(b) == [
        ("2023-10-01T00:00:00.000Z", 1),
        ("2024-01-01T00:00:00.000Z", 7),
        ("2024-04-01T00:00:00.000Z", 2),
        ("2024-07-01T00:00:00.000Z", 1),
        ("2024-10-01T00:00:00.000Z", 1),
        ("2025-01-01T00:00:00.000Z", 1),
    ]


def test_date_histogram_month_min_doc_count_one(client, dates):
    b = _dh(client, dates, calendar_interval="month", min_doc_count=1, format="yyyy-MM")
    assert _ksc(b) == [
        ("2023-12", 1), ("2024-01", 5), ("2024-02", 1), ("2024-03", 1), ("2024-04", 1),
        ("2024-06", 1), ("2024-07", 1), ("2024-12", 1), ("2025-01", 1),
    ]


def test_date_histogram_month_gap_filling(client, dates):
    b = _dh(client, dates, calendar_interval="month", format="yyyy-MM")
    assert len(b) == 14
    assert _ksc(b)[5:10] == [("2024-05", 0), ("2024-06", 1), ("2024-07", 1), ("2024-08", 0), ("2024-09", 0)]
    assert sum(x["doc_count"] for x in b) == 13


def test_date_histogram_month_keys_are_calendar_month_starts(client, dates):
    b = _dh(client, dates, calendar_interval="month", format="yyyy-MM")
    assert [x["key"] for x in b[:4]] == [
        ms("2023-12-01T00:00:00Z"), ms("2024-01-01T00:00:00Z"), ms("2024-02-01T00:00:00Z"), ms("2024-03-01T00:00:00Z"),
    ]


def test_date_histogram_week_starts_monday(client, dates):
    b = _dh(client, dates, calendar_interval="week", min_doc_count=1, format="yyyy-MM-dd")
    assert _ksc(b) == [
        ("2023-12-25", 1), ("2024-01-01", 4), ("2024-01-29", 2), ("2024-03-25", 1),
        ("2024-04-01", 1), ("2024-06-24", 1), ("2024-07-01", 1), ("2024-12-30", 2),
    ]


def test_date_histogram_day(client, dates):
    b = _dh(client, dates, calendar_interval="day", min_doc_count=1, format="yyyy-MM-dd")
    assert _ksc(b) == [
        ("2023-12-31", 1), ("2024-01-01", 4), ("2024-01-31", 1), ("2024-02-01", 1), ("2024-03-31", 1),
        ("2024-04-01", 1), ("2024-06-30", 1), ("2024-07-01", 1), ("2024-12-31", 1), ("2025-01-01", 1),
    ]


def test_date_histogram_hour(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-01-02"}}}
    b = _dh(client, dates, q, calendar_interval="hour")
    assert _ksc(b) == [("2024-01-01T00:00:00.000Z", 3), ("2024-01-01T01:00:00.000Z", 1)]


def test_date_histogram_minute(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-01-02"}}}
    b = _dh(client, dates, q, calendar_interval="minute", format="HH:mm")
    assert len(b) == 120
    assert _ksc(b)[:3] == [("00:00", 2), ("00:01", 1), ("00:02", 0)]
    assert _ksc(b)[-1] == ("01:59", 1)


@pytest.mark.parametrize(
    "word,unit",
    [("minute", "1m"), ("hour", "1h"), ("day", "1d"), ("week", "1w"), ("month", "1M"), ("quarter", "1q"), ("year", "1y")],
)
def test_date_histogram_calendar_interval_aliases(client, dates, word, unit):
    a = _dh(client, dates, calendar_interval=word, min_doc_count=1)
    b = _dh(client, dates, calendar_interval=unit, min_doc_count=1)
    assert a == b
    assert sum(x["doc_count"] for x in a) == 13


@pytest.mark.parametrize("bad", ["2d", "2M", "1.5h", "bogus", "3w"])
def test_date_histogram_invalid_calendar_interval(client, dates, bad):
    r = _err(client, dates, {"x": {"date_histogram": {"field": "t", "calendar_interval": bad}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == f"The supplied interval [{bad}] could not be parsed as a calendar interval."


@pytest.mark.parametrize("bad", ["1M", "1y", "1q", "abc"])
def test_date_histogram_invalid_fixed_interval(client, dates, bad):
    r = _err(client, dates, {"x": {"date_histogram": {"field": "t", "fixed_interval": bad}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == (
        f"failed to parse setting [date_histogram.fixedInterval] with value [{bad}] as a time value: unit is missing or unrecognized"
    )


def test_date_histogram_fixed_and_calendar_together_is_400(client, dates):
    r = _err(client, dates, {"x": {"date_histogram": {"field": "t", "calendar_interval": "month", "fixed_interval": "1d"}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == "Cannot use [fixed_interval] with [calendar_interval] configuration option."


def test_date_histogram_without_interval_is_400(client, dates):
    r = _err(client, dates, {"x": {"date_histogram": {"field": "t"}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Invalid interval specified, must be non-null and non-empty"


def test_date_histogram_fixed_interval_30s(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01T00:00:00Z", "lt": "2024-01-01T00:05:00Z"}}}
    b = _dh(client, dates, q, fixed_interval="30s", format="HH:mm:ss")
    assert _ksc(b) == [("00:00:00", 1), ("00:00:30", 1), ("00:01:00", 1)]


def test_date_histogram_fixed_interval_aligned_to_epoch(client, dates):
    b = _dh(client, dates, fixed_interval="30d", min_doc_count=1)
    # buckets are multiples of 30 days since 1970-01-01
    assert all(x["key"] % (30 * 86400000) == 0 for x in b)
    assert _ksc(b)[0] == ("2023-12-19T00:00:00.000Z", 5)


def test_date_histogram_fixed_interval_90m(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-01-02"}}}
    b = _dh(client, dates, q, fixed_interval="90m")
    assert _ksc(b) == [("2024-01-01T00:00:00.000Z", 3), ("2024-01-01T01:30:00.000Z", 1)]


def test_date_histogram_deprecated_interval_param_still_works(client, dates):
    a = _dh(client, dates, interval="month", min_doc_count=1)
    b = _dh(client, dates, calendar_interval="month", min_doc_count=1)
    assert a == b


def test_date_histogram_deprecated_interval_param_fixed(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-01-02"}}}
    b = _dh(client, dates, q, interval="90m")
    assert _ksc(b) == [("2024-01-01T00:00:00.000Z", 3), ("2024-01-01T01:30:00.000Z", 1)]


def test_date_histogram_offset(client, dates):
    b = _dh(client, dates, calendar_interval="day", offset="+6h", min_doc_count=1, format="yyyy-MM-dd'T'HH")
    # 2024-01-01 00:00..01:59 still belong to the bucket that started 2023-12-31 06:00
    assert _ksc(b)[:2] == [("2023-12-31T06", 5), ("2024-01-31T06", 2)]


def test_date_histogram_month_offset(client, main):
    q = {"range": {"ts": {"gte": "2024-01-01", "lt": "2024-05-01"}}}
    b = _agg(client, main, {"date_histogram": {"field": "ts", "calendar_interval": "month", "offset": "+1d", "min_doc_count": 1, "format": "yyyy-MM-dd"}}, query=q)["buckets"]
    assert _ksc(b) == [("2023-12-02", 1), ("2024-01-02", 1), ("2024-02-02", 1), ("2024-03-02", 2)]


def test_date_histogram_leap_day(client, main):
    q = {"range": {"ts": {"gte": "2024-02-01", "lt": "2024-03-01"}}}
    b = _agg(client, main, {"date_histogram": {"field": "ts", "calendar_interval": "day", "format": "yyyy-MM-dd"}}, query=q)["buckets"]
    assert _ksc(b) == [("2024-02-29", 1)]


def test_date_histogram_keyed(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-04-01"}}}
    res = _agg(client, dates, {"date_histogram": {"field": "t", "calendar_interval": "month", "keyed": True, "format": "yyyy-MM"}}, query=q)
    assert res == {"buckets": {
        "2024-01": {"key_as_string": "2024-01", "key": 1704067200000, "doc_count": 5},
        "2024-02": {"key_as_string": "2024-02", "key": 1706745600000, "doc_count": 1},
        "2024-03": {"key_as_string": "2024-03", "key": 1709251200000, "doc_count": 1},
    }}


def test_date_histogram_extended_bounds_with_date_strings(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-02-01"}}}
    b = _dh(client, dates, q, calendar_interval="month", format="yyyy-MM", extended_bounds={"min": "2023-11", "max": "2024-03"})
    assert _ksc(b) == [("2023-11", 0), ("2023-12", 0), ("2024-01", 5), ("2024-02", 0), ("2024-03", 0)]


def test_date_histogram_extended_bounds_with_epoch_millis(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-02-01"}}}
    b = _dh(client, dates, q, calendar_interval="month", extended_bounds={"min": ms("2024-01-01T00:00:00Z"), "max": ms("2024-03-15T00:00:00Z")})
    assert [x["doc_count"] for x in b] == [5, 0, 0]


def test_date_histogram_extended_bounds_on_no_hits_creates_empty_buckets(client, dates):
    b = _dh(client, dates, {"term": {"v": 999}}, calendar_interval="year", format="yyyy", extended_bounds={"min": "2020", "max": "2022"})
    assert _ksc(b) == [("2020", 0), ("2021", 0), ("2022", 0)]


def test_date_histogram_hard_bounds_max_is_exclusive(client, dates):
    b = _dh(client, dates, calendar_interval="month", format="yyyy-MM", hard_bounds={"min": "2024-02", "max": "2024-04"})
    assert _ksc(b) == [("2024-02", 1), ("2024-03", 1)]


def test_date_histogram_hard_bounds_are_parsed_with_format(client, dates):
    r = _err(client, dates, {"x": {"date_histogram": {"field": "t", "calendar_interval": "month", "format": "yyyy-MM", "hard_bounds": {"min": "2024-02-15", "max": "2024-04-15"}}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "parse_exception"


def test_histogram_hard_bounds_numeric_filters_by_bucket_key(client, main):
    res = _agg(client, main, {"histogram": {"field": "n", "interval": 3, "hard_bounds": {"min": 1, "max": 5}}})
    assert _kc(res["buckets"]) == [(3.0, 3)]


def test_date_histogram_missing_uses_format(client, main):
    b = _agg(client, main, {"date_histogram": {"field": "ts", "calendar_interval": "year", "format": "yyyy", "missing": "2000", "min_doc_count": 1}})["buckets"]
    assert _ksc(b) == [("2000", 2), ("2023", 1), ("2024", 6), ("2025", 1)]


def test_date_histogram_missing_must_match_format(client, main):
    r = _err(client, main, {"x": {"date_histogram": {"field": "ts", "calendar_interval": "year", "format": "yyyy", "missing": "2000-06-01"}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "parse_exception"


@pytest.mark.parametrize(
    "order,expected",
    [
        ({"_count": "asc"}, ["2023", "2025", "2024"]),
        ({"_count": "desc"}, ["2024", "2023", "2025"]),
        ({"_key": "desc"}, ["2025", "2024", "2023"]),
    ],
)
def test_date_histogram_order(client, dates, order, expected):
    b = _dh(client, dates, calendar_interval="year", format="yyyy", order=order)
    assert [x["key_as_string"] for x in b] == expected


@pytest.mark.parametrize(
    "fmt,expected",
    [
        ("yyyy-MM-dd", "2024-01-01"),
        ("epoch_millis", "1704067200000"),
        ("epoch_second", "1704067200"),
        ("dd/MM/yyyy HH:mm", "01/01/2024 00:00"),
        ("strict_date", "2024-01-01"),
        ("basic_date", "20240101"),
    ],
)
def test_date_histogram_format(client, dates, fmt, expected):
    q = {"range": {"t": {"gte": "2024-01-01", "lt": "2024-01-02"}}}
    b = _dh(client, dates, q, calendar_interval="day", format=fmt)
    assert b == [{"key_as_string": expected, "key": 1704067200000, "doc_count": 4}]


def test_date_histogram_on_numeric_field_has_no_key_as_string(client, main):
    b = _agg(client, main, {"date_histogram": {"field": "n", "calendar_interval": "year"}})["buckets"]
    assert b == [{"key": -31536000000, "doc_count": 1}, {"key": 0, "doc_count": 9}]


def test_date_histogram_on_keyword_is_400(client, main):
    r = _err(client, main, {"x": {"date_histogram": {"field": "color", "calendar_interval": "year"}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Field [color] of type [keyword] is not supported for aggregation [date_histogram]"


def test_date_histogram_unmapped(client, dates):
    assert _dh(client, dates, field="nope", calendar_interval="day") == []


def test_date_histogram_invalid_time_zone(client, dates):
    r = _err(client, dates, {"x": {"date_histogram": {"field": "t", "calendar_interval": "day", "time_zone": "Mars/Olympus"}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["type"] == "zone_rules_exception"
    assert r.json()["error"]["caused_by"]["reason"] == "Unknown time-zone ID: Mars/Olympus"


# ---- time zones / DST ------------------------------------------------------

TZ_DOCS = [
    # America/New_York spring forward 2024-03-10 02:00 EST -> 03:00 EDT (07:00Z)
    "2024-03-09T12:00:00Z",  # Mar 9 07:00 EST
    "2024-03-10T06:59:00Z",  # Mar 10 01:59 EST
    "2024-03-10T07:00:00Z",  # Mar 10 03:00 EDT
    "2024-03-11T03:30:00Z",  # Mar 10 23:30 EDT
    "2024-03-11T04:30:00Z",  # Mar 11 00:30 EDT
    # America/New_York fall back 2024-11-03 02:00 EDT -> 01:00 EST (06:00Z)
    "2024-11-03T05:30:00Z",  # 01:30 EDT
    "2024-11-03T06:30:00Z",  # 01:30 EST
    "2024-11-04T04:30:00Z",  # Nov 3 23:30 EST
    "2024-11-04T05:30:00Z",  # Nov 4 00:30 EST
    # Europe/Kyiv spring forward 2024-03-31 03:00 EET -> 04:00 EEST (01:00Z)
    "2024-03-30T21:30:00Z",  # Mar 30 23:30 EET
    "2024-03-30T22:30:00Z",  # Mar 31 00:30 EET
    "2024-03-31T00:30:00Z",  # Mar 31 02:30 EET
    "2024-03-31T01:30:00Z",  # Mar 31 04:30 EEST
    "2024-03-31T21:30:00Z",  # Apr 1 00:30 EEST
    # Europe/Kyiv fall back 2024-10-27 04:00 EEST -> 03:00 EET (01:00Z)
    "2024-10-26T21:30:00Z",  # Oct 27 00:30 EEST
    "2024-10-27T00:30:00Z",  # Oct 27 03:30 EEST
    "2024-10-27T01:30:00Z",  # Oct 27 03:30 EET
    "2024-10-27T21:30:00Z",  # Oct 27 23:30 EET
    "2024-10-27T22:30:00Z",  # Oct 28 00:30 EET
]

NY_SPRING = {"range": {"t": {"gte": "2024-03-09", "lt": "2024-03-12"}}}
NY_FALL = {"range": {"t": {"gte": "2024-11-01", "lt": "2024-11-30"}}}
KY_SPRING = {"range": {"t": {"gte": "2024-03-30T12:00:00Z", "lt": "2024-04-05"}}}
KY_FALL = {"range": {"t": {"gte": "2024-10-20", "lt": "2024-10-30"}}}


@pytest.fixture(scope="module")
def tzidx(client):
    name = _make_index(client, {"properties": {"t": {"type": "date"}}}, [{"t": t} for t in TZ_DOCS])
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def test_tz_new_york_day_buckets_spring_forward(client, tzidx):
    b = _dh(client, tzidx, NY_SPRING, calendar_interval="day", time_zone="America/New_York")
    assert b == [
        {"key_as_string": "2024-03-09T00:00:00.000-05:00", "key": ms("2024-03-09T05:00:00Z"), "doc_count": 1},
        {"key_as_string": "2024-03-10T00:00:00.000-05:00", "key": ms("2024-03-10T05:00:00Z"), "doc_count": 3},
        {"key_as_string": "2024-03-11T00:00:00.000-04:00", "key": ms("2024-03-11T04:00:00Z"), "doc_count": 1},
    ]


def test_tz_new_york_spring_forward_day_is_23h(client, tzidx):
    b = _dh(client, tzidx, NY_SPRING, calendar_interval="day", time_zone="America/New_York")
    assert b[2]["key"] - b[1]["key"] == 23 * 3600 * 1000


def test_tz_new_york_hour_spring_forward_skips_2am(client, tzidx):
    q = {"range": {"t": {"gte": "2024-03-10T06:00:00Z", "lt": "2024-03-10T08:00:00Z"}}}
    b = _dh(client, tzidx, q, calendar_interval="hour", time_zone="America/New_York")
    assert _ksc(b) == [("2024-03-10T01:00:00.000-05:00", 1), ("2024-03-10T03:00:00.000-04:00", 1)]
    assert b[1]["key"] - b[0]["key"] == 3600 * 1000


def test_tz_new_york_hour_fall_back_has_two_1am_buckets(client, tzidx):
    b = _dh(client, tzidx, NY_FALL, calendar_interval="hour", time_zone="America/New_York", min_doc_count=1)
    assert _ksc(b) == [
        ("2024-11-03T01:00:00.000-04:00", 1),
        ("2024-11-03T01:00:00.000-05:00", 1),
        ("2024-11-03T23:00:00.000-05:00", 1),
        ("2024-11-04T00:00:00.000-05:00", 1),
    ]
    assert b[1]["key"] - b[0]["key"] == 3600 * 1000


def test_tz_new_york_day_fall_back_is_25h(client, tzidx):
    b = _dh(client, tzidx, NY_FALL, calendar_interval="day", time_zone="America/New_York")
    assert _ksc(b) == [("2024-11-03T00:00:00.000-04:00", 3), ("2024-11-04T00:00:00.000-05:00", 1)]
    assert b[1]["key"] - b[0]["key"] == 25 * 3600 * 1000


def test_tz_utc_day_buckets_differ_from_new_york(client, tzidx):
    b = _dh(client, tzidx, NY_FALL, calendar_interval="day", format="yyyy-MM-dd")
    assert _ksc(b) == [("2024-11-03", 2), ("2024-11-04", 2)]


def test_tz_kyiv_day_spring_forward(client, tzidx):
    b = _dh(client, tzidx, KY_SPRING, calendar_interval="day", time_zone="Europe/Kyiv")
    assert b == [
        {"key_as_string": "2024-03-30T00:00:00.000+02:00", "key": ms("2024-03-29T22:00:00Z"), "doc_count": 1},
        {"key_as_string": "2024-03-31T00:00:00.000+02:00", "key": ms("2024-03-30T22:00:00Z"), "doc_count": 3},
        {"key_as_string": "2024-04-01T00:00:00.000+03:00", "key": ms("2024-03-31T21:00:00Z"), "doc_count": 1},
    ]


def test_tz_kyiv_hour_spring_forward_skips_3am(client, tzidx):
    q = {"range": {"t": {"gte": "2024-03-31T00:00:00Z", "lt": "2024-03-31T02:00:00Z"}}}
    b = _dh(client, tzidx, q, calendar_interval="hour", time_zone="Europe/Kyiv")
    assert _ksc(b) == [("2024-03-31T02:00:00.000+02:00", 1), ("2024-03-31T04:00:00.000+03:00", 1)]


def test_tz_kyiv_day_fall_back(client, tzidx):
    b = _dh(client, tzidx, KY_FALL, calendar_interval="day", time_zone="Europe/Kyiv")
    assert _ksc(b) == [("2024-10-27T00:00:00.000+03:00", 4), ("2024-10-28T00:00:00.000+02:00", 1)]
    assert b[1]["key"] - b[0]["key"] == 25 * 3600 * 1000


def test_tz_kyiv_hour_fall_back_repeats_3am(client, tzidx):
    q = {"range": {"t": {"gte": "2024-10-27T00:00:00Z", "lt": "2024-10-27T02:00:00Z"}}}
    b = _dh(client, tzidx, q, calendar_interval="hour", time_zone="Europe/Kyiv")
    assert _ksc(b) == [("2024-10-27T03:00:00.000+03:00", 1), ("2024-10-27T03:00:00.000+02:00", 1)]


def test_tz_kyiv_month_moves_doc_into_april(client, tzidx):
    b = _dh(client, tzidx, KY_SPRING, calendar_interval="month", time_zone="Europe/Kyiv")
    assert _ksc(b) == [("2024-03-01T00:00:00.000+02:00", 4), ("2024-04-01T00:00:00.000+03:00", 1)]
    b_utc = _dh(client, tzidx, KY_SPRING, calendar_interval="month")
    assert _ksc(b_utc) == [("2024-03-01T00:00:00.000Z", 5)]


def test_tz_fixed_offset(client, tzidx):
    b = _dh(client, tzidx, KY_SPRING, calendar_interval="day", time_zone="+03:00")
    assert _ksc(b) == [
        ("2024-03-31T00:00:00.000+03:00", 4),
        ("2024-04-01T00:00:00.000+03:00", 1),
    ]


def test_tz_with_custom_format(client, tzidx):
    b = _dh(client, tzidx, KY_FALL, calendar_interval="day", time_zone="Europe/Kyiv", format="yyyy-MM-dd")
    assert _ksc(b) == [("2024-10-27", 4), ("2024-10-28", 1)]


def test_tz_offset_combined(client, tzidx):
    b = _dh(client, tzidx, NY_FALL, calendar_interval="day", time_zone="America/New_York", offset="+2h", min_doc_count=1)
    # the offset is added as a fixed duration to the local-midnight rounding (00:00 EDT + 2h = 01:00 EST)
    assert _ksc(b) == [("2024-11-02T02:00:00.000-04:00", 1), ("2024-11-03T01:00:00.000-05:00", 3)]


def test_tz_fixed_interval_day(client, tzidx):
    # a 1d fixed interval with a DST zone still rounds to local midnight
    b = _dh(client, tzidx, KY_FALL, fixed_interval="1d", time_zone="Europe/Kyiv")
    assert _ksc(b) == [("2024-10-27T00:00:00.000+03:00", 4), ("2024-10-28T00:00:00.000+02:00", 1)]
    b = _dh(client, tzidx, KY_SPRING, fixed_interval="1d", time_zone="Europe/Kyiv")
    assert _ksc(b) == [
        ("2024-03-30T00:00:00.000+02:00", 1), ("2024-03-31T00:00:00.000+02:00", 3), ("2024-04-01T00:00:00.000+03:00", 1),
    ]


# ===========================================================================
# auto_date_histogram / variable_width_histogram
# ===========================================================================


def test_auto_date_histogram_picks_year_interval(client, main):
    res = _agg(client, main, {"auto_date_histogram": {"field": "ts", "buckets": 3}})
    assert res == {
        "buckets": [
            {"key_as_string": "2023-01-01T00:00:00.000Z", "key": 1672531200000, "doc_count": 1},
            {"key_as_string": "2024-01-01T00:00:00.000Z", "key": 1704067200000, "doc_count": 6},
            {"key_as_string": "2025-01-01T00:00:00.000Z", "key": 1735689600000, "doc_count": 1},
        ],
        "interval": "1y",
    }


@pytest.mark.parametrize("n", [10, None])
def test_auto_date_histogram_three_month_interval(client, main, n):
    body = {"field": "ts"}
    if n is not None:
        body["buckets"] = n
    res = _agg(client, main, {"auto_date_histogram": body})
    assert res["interval"] == "3M"
    assert _ksc(res["buckets"]) == [
        ("2023-06-01T00:00:00.000Z", 1), ("2023-09-01T00:00:00.000Z", 0), ("2023-12-01T00:00:00.000Z", 3),
        ("2024-03-01T00:00:00.000Z", 2), ("2024-06-01T00:00:00.000Z", 0), ("2024-09-01T00:00:00.000Z", 0),
        ("2024-12-01T00:00:00.000Z", 2),
    ]


def test_auto_date_histogram_format(client, main):
    res = _agg(client, main, {"auto_date_histogram": {"field": "ts", "buckets": 3, "format": "yyyy-MM"}})
    assert [b["key_as_string"] for b in res["buckets"]] == ["2023-01", "2024-01", "2025-01"]


def test_auto_date_histogram_small_range_uses_fine_interval(client, dates):
    q = {"range": {"t": {"gte": "2024-01-01T00:00:00Z", "lt": "2024-01-01T00:02:00Z"}}}
    res = _agg(client, dates, {"auto_date_histogram": {"field": "t", "buckets": 5}}, query=q)
    assert res["interval"] == "30s"
    assert _ksc(res["buckets"]) == [
        ("2024-01-01T00:00:00.000Z", 1), ("2024-01-01T00:00:30.000Z", 1), ("2024-01-01T00:01:00.000Z", 1),
    ]


def test_auto_date_histogram_bucket_count_never_exceeds_target(client, dates):
    for n in (1, 2, 3, 5, 8, 20):
        res = _agg(client, dates, {"auto_date_histogram": {"field": "t", "buckets": n}})
        assert 1 <= len(res["buckets"]) <= n
        assert sum(b["doc_count"] for b in res["buckets"]) == 13


def test_variable_width_histogram_integers(client, main):
    res = _agg(client, main, {"variable_width_histogram": {"field": "n", "buckets": 3}})
    assert res == {"buckets": [
        {"min": -2.0, "key": -2.0, "max": -2.0, "doc_count": 1},
        {"min": 1.0, "key": 2.5, "max": 4.0, "doc_count": 4},
        {"min": 5.0, "key": 7.2, "max": 10.0, "doc_count": 5},
    ]}


def test_variable_width_histogram_isolates_outlier(client, main):
    res = _agg(client, main, {"variable_width_histogram": {"field": "price", "buckets": 2}})
    assert res == {"buckets": [
        {"min": -10.5, "key": 2.84375, "max": 15.0, "doc_count": 8},
        {"min": 100.0, "key": 100.0, "max": 100.0, "doc_count": 1},
    ]}


def test_variable_width_histogram_more_buckets_than_values(client, main):
    res = _agg(client, main, {"variable_width_histogram": {"field": "n", "buckets": 50}})
    assert [(b["min"], b["key"], b["max"], b["doc_count"]) for b in res["buckets"]] == [
        (float(v), float(v), float(v), 1) for v in [-2, 1, 2, 3, 4, 5, 6, 7, 8, 10]
    ]


# ===========================================================================
# range / date_range / ip_range
# ===========================================================================


def test_range_from_inclusive_to_exclusive(client, main):
    res = _agg(client, main, {"range": {"field": "n", "ranges": [{"to": 2}, {"from": 2, "to": 5}, {"from": 5}]}})
    assert res == {"buckets": [
        {"key": "*-2.0", "to": 2.0, "doc_count": 2},
        {"key": "2.0-5.0", "from": 2.0, "to": 5.0, "doc_count": 3},
        {"key": "5.0-*", "from": 5.0, "doc_count": 5},
    ]}


def test_range_buckets_sorted_by_from_then_to(client, main):
    res = _agg(client, main, {"range": {"field": "n", "ranges": [{"from": 5}, {"to": 2}, {"from": 2, "to": 5}, {"from": 1, "to": 5}]}})
    assert _kc(res["buckets"]) == [("*-2.0", 2), ("1.0-5.0", 4), ("2.0-5.0", 3), ("5.0-*", 5)]


def test_range_overlapping_ranges_count_doc_in_each(client, main):
    res = _agg(client, main, {"range": {"field": "price", "ranges": [{"from": -3, "to": 2.5}, {"from": 0, "to": 10.0}]}})
    assert _kc(res["buckets"]) == [("-3.0-2.5", 3), ("0.0-10.0", 4)]


def test_range_keyed_with_custom_keys(client, main):
    res = _agg(
        client, main,
        {"range": {"field": "price", "keyed": True, "ranges": [
            {"key": "cheap", "to": 0}, {"key": "mid", "from": 0, "to": 10}, {"key": "big", "from": 10},
        ]}},
    )
    assert res == {"buckets": {
        "cheap": {"to": 0.0, "doc_count": 2},
        "mid": {"from": 0.0, "to": 10.0, "doc_count": 4},
        "big": {"from": 10.0, "doc_count": 3},
    }}


def test_range_custom_keys_not_keyed(client, main):
    res = _agg(client, main, {"range": {"field": "price", "ranges": [{"key": "neg", "to": 0}, {"from": 0}]}})
    assert res["buckets"] == [{"key": "neg", "to": 0.0, "doc_count": 2}, {"key": "0.0-*", "from": 0.0, "doc_count": 7}]


def test_range_inverted_range_is_empty(client, main):
    res = _agg(client, main, {"range": {"field": "price", "ranges": [{"from": 10, "to": 1}]}})
    assert res["buckets"] == [{"key": "10.0-1.0", "from": 10.0, "to": 1.0, "doc_count": 0}]


def test_range_on_doubles_boundary(client, main):
    res = _agg(client, main, {"range": {"field": "price", "ranges": [{"from": 7.25, "to": 7.26}, {"from": 7.24, "to": 7.25}]}})
    assert _kc(res["buckets"]) == [("7.24-7.25", 0), ("7.25-7.26", 1)]


def test_range_missing(client, main):
    res = _agg(client, main, {"range": {"field": "price", "missing": 0, "ranges": [{"to": 2}]}})
    assert res["buckets"] == [{"key": "*-2.0", "to": 2.0, "doc_count": 5}]


def test_range_format_adds_as_string(client, main):
    res = _agg(client, main, {"range": {"field": "price", "format": "#.0", "ranges": [{"from": 1, "to": 2}]}})
    assert res["buckets"] == [{"key": "1.0-2.0", "from": 1.0, "from_as_string": "1.0", "to": 2.0, "to_as_string": "2.0", "doc_count": 1}]


def test_range_unmapped_field(client, main):
    res = _agg(client, main, {"range": {"field": "nope", "ranges": [{"to": 2}, {"from": 2}]}})
    assert res == {"buckets": [{"key": "*-2.0", "to": 2.0, "doc_count": 0}, {"key": "2.0-*", "from": 2.0, "doc_count": 0}]}


@pytest.mark.parametrize("ranges", [[], None])
def test_range_without_ranges_is_400(client, main, ranges):
    body = {"field": "n"}
    if ranges is not None:
        body["ranges"] = ranges
    r = _err(client, main, {"x": {"range": body}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "No [ranges] specified for the [x] aggregation"


def test_range_with_sub_agg(client, main):
    res = _agg(
        client, main,
        {"range": {"field": "n", "ranges": [{"to": 5}, {"from": 5}]}, "aggs": {"s": {"sum": {"field": "price"}}}},
    )
    assert [(b["key"], b["doc_count"], b["s"]["value"]) for b in res["buckets"]] == [
        ("*-5.0", 5, 0.5), ("5.0-*", 5, 122.25),
    ]


def test_date_range_shape(client, main):
    res = _agg(client, main, {"date_range": {"field": "ts", "ranges": [{"to": "2024-01-01"}, {"from": "2024-01-01", "to": "2024-04-01"}, {"from": "2024-04-01"}]}})
    assert res == {"buckets": [
        {"key": "*-2024-01-01T00:00:00.000Z", "to": 1704067200000.0, "to_as_string": "2024-01-01T00:00:00.000Z", "doc_count": 1},
        {"key": "2024-01-01T00:00:00.000Z-2024-04-01T00:00:00.000Z", "from": 1704067200000.0, "from_as_string": "2024-01-01T00:00:00.000Z",
         "to": 1711929600000.0, "to_as_string": "2024-04-01T00:00:00.000Z", "doc_count": 4},
        {"key": "2024-04-01T00:00:00.000Z-*", "from": 1711929600000.0, "from_as_string": "2024-04-01T00:00:00.000Z", "doc_count": 3},
    ]}


def test_date_range_format(client, main):
    res = _agg(client, main, {"date_range": {"field": "ts", "format": "yyyy-MM", "ranges": [{"to": "2024-02"}, {"from": "2024-02"}]}})
    assert res["buckets"] == [
        {"key": "*-2024-02", "to": 1706745600000.0, "to_as_string": "2024-02", "doc_count": 3},
        {"key": "2024-02-*", "from": 1706745600000.0, "from_as_string": "2024-02", "doc_count": 5},
    ]


def test_date_range_date_math(client, main):
    res = _agg(client, main, {"date_range": {"field": "ts", "ranges": [{"from": "2024-01-01||+1M/M", "to": "2025-01-01||-1d"}]}})
    assert res["buckets"] == [{
        "key": "2024-02-01T00:00:00.000Z-2024-12-31T00:00:00.000Z",
        "from": 1706745600000.0, "from_as_string": "2024-02-01T00:00:00.000Z",
        "to": 1735603200000.0, "to_as_string": "2024-12-31T00:00:00.000Z",
        "doc_count": 3,
    }]


def test_date_range_time_zone(client, main):
    res = _agg(client, main, {"date_range": {"field": "ts", "time_zone": "+02:00", "ranges": [{"from": "2024-01-01T01:00:00", "to": "2024-04-01T02:00:00"}]}})
    assert res["buckets"] == [{
        "key": "2024-01-01T01:00:00.000+02:00-2024-04-01T02:00:00.000+02:00",
        "from": 1704063600000.0, "from_as_string": "2024-01-01T01:00:00.000+02:00",
        "to": 1711929600000.0, "to_as_string": "2024-04-01T02:00:00.000+02:00",
        "doc_count": 4,
    }]


def test_date_range_keyed(client, main):
    res = _agg(client, main, {"date_range": {"field": "ts", "keyed": True, "ranges": [{"key": "old", "to": "2024-01-01"}, {"key": "new", "from": "2024-01-01"}]}})
    assert res == {"buckets": {
        "old": {"to": 1704067200000.0, "to_as_string": "2024-01-01T00:00:00.000Z", "doc_count": 1},
        "new": {"from": 1704067200000.0, "from_as_string": "2024-01-01T00:00:00.000Z", "doc_count": 7},
    }}


def test_date_range_epoch_millis_bounds(client, main):
    res = _agg(client, main, {"date_range": {"field": "ts", "ranges": [{"from": 1704067200000, "to": 1709251200000}]}})
    assert _kc(res["buckets"]) == [("2024-01-01T00:00:00.000Z-2024-03-01T00:00:00.000Z", 3)]


def test_date_range_now_far_from_data(client, main):
    res = _agg(client, main, {"date_range": {"field": "ts", "ranges": [{"from": "now-1000y", "to": "now+1000y"}]}})
    assert res["buckets"][0]["doc_count"] == 8


def test_ip_range_from_to(client, main):
    res = _agg(client, main, {"ip_range": {"field": "ip", "ranges": [{"to": "10.0.0.2"}, {"from": "10.0.0.2", "to": "192.168.1.1"}, {"from": "192.168.1.1"}]}})
    assert res == {"buckets": [
        {"key": "*-10.0.0.2", "to": "10.0.0.2", "doc_count": 2},
        {"key": "10.0.0.2-192.168.1.1", "from": "10.0.0.2", "to": "192.168.1.1", "doc_count": 2},
        {"key": "192.168.1.1-*", "from": "192.168.1.1", "doc_count": 3},
    ]}


def test_ip_range_cidr_masks(client, main):
    res = _agg(client, main, {"ip_range": {"field": "ip", "ranges": [
        {"mask": "10.0.0.0/24"}, {"mask": "10.0.0.0/8"}, {"mask": "192.168.1.128/25"}, {"mask": "2001:db8::/32"},
    ]}})
    assert res["buckets"] == [
        {"key": "10.0.0.0/24", "from": "10.0.0.0", "to": "10.0.1.0", "doc_count": 3},
        {"key": "10.0.0.0/8", "from": "10.0.0.0", "to": "11.0.0.0", "doc_count": 4},
        {"key": "192.168.1.128/25", "from": "192.168.1.128", "to": "192.168.2.0", "doc_count": 1},
        {"key": "2001:db8::/32", "from": "2001:db8::", "to": "2001:db9::", "doc_count": 1},
    ]


def test_ip_range_mask_with_host_bits_is_normalised(client, main):
    res = _agg(client, main, {"ip_range": {"field": "ip", "ranges": [{"mask": "10.0.0.5/24"}]}})
    assert res["buckets"] == [{"key": "10.0.0.5/24", "from": "10.0.0.0", "to": "10.0.1.0", "doc_count": 3}]


def test_ip_range_keyed(client, main):
    res = _agg(client, main, {"ip_range": {"field": "ip", "keyed": True, "ranges": [{"key": "lan", "mask": "10.0.0.0/8"}, {"mask": "192.168.0.0/16"}]}})
    assert res == {"buckets": {
        "lan": {"from": "10.0.0.0", "to": "11.0.0.0", "doc_count": 4},
        "192.168.0.0/16": {"from": "192.168.0.0", "to": "192.169.0.0", "doc_count": 2},
    }}


def test_ip_range_sorted(client, main):
    res = _agg(client, main, {"ip_range": {"field": "ip", "ranges": [{"from": "192.168.1.1"}, {"to": "10.0.0.2"}]}})
    assert _kc(res["buckets"]) == [("*-10.0.0.2", 2), ("192.168.1.1-*", 3)]


def test_ip_range_invalid_prefix(client, main):
    r = _err(client, main, {"x": {"ip_range": {"field": "ip", "ranges": [{"mask": "10.0.0.0/33"}]}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == (
        "Illegal prefix length [33] in [10.0.0.0/33]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges"
    )


# ===========================================================================
# filter / filters / missing / global
# ===========================================================================


def test_filter_with_sub_agg(client, main):
    res = _agg(client, main, {"filter": {"term": {"color": "red"}}, "aggs": {"s": {"sum": {"field": "n"}}}})
    assert res == {"doc_count": 3, "s": {"value": 9.0}}


def test_filter_is_intersected_with_query(client, main):
    res = _agg(client, main, {"filter": {"term": {"tags": "a"}}}, query={"term": {"flag": True}})
    assert res == {"doc_count": 1}


def test_filter_match_none(client, main):
    assert _agg(client, main, {"filter": {"match_none": {}}}) == {"doc_count": 0}


def test_filters_named_buckets_sorted_by_name(client, main):
    r = _search(client, main, {"size": 0, "aggs": {"x": {"filters": {"filters": {
        "reds": {"term": {"color": "red"}}, "blues": {"term": {"color": "blue"}}, "aa": {"term": {"tags": "a"}},
    }}}}})
    buckets = r.json()["aggregations"]["x"]["buckets"]
    assert buckets == {"aa": {"doc_count": 4}, "blues": {"doc_count": 3}, "reds": {"doc_count": 3}}
    assert list(buckets) == ["aa", "blues", "reds"]


def test_filters_anonymous_buckets_keep_order(client, main):
    res = _agg(client, main, {"filters": {"filters": [{"term": {"tags": "a"}}, {"term": {"color": "red"}}, {"match_none": {}}]}})
    assert res == {"buckets": [{"doc_count": 4}, {"doc_count": 3}, {"doc_count": 0}]}


def test_filters_other_bucket_default_key(client, main):
    res = _agg(client, main, {"filters": {"filters": {"reds": {"term": {"color": "red"}}}, "other_bucket": True}})
    assert res == {"buckets": {"reds": {"doc_count": 3}, "_other_": {"doc_count": 7}}}


def test_filters_other_bucket_key_implies_other_bucket(client, main):
    res = _agg(client, main, {"filters": {"filters": {"reds": {"term": {"color": "red"}}}, "other_bucket_key": "rest"}})
    assert res == {"buckets": {"reds": {"doc_count": 3}, "rest": {"doc_count": 7}}}


def test_filters_other_bucket_anonymous_is_last(client, main):
    res = _agg(client, main, {"filters": {"filters": [{"term": {"color": "red"}}, {"term": {"tags": "a"}}], "other_bucket": True}})
    # doc 8 & docs 1,2 match tags:a / red; other = docs matching neither
    assert res == {"buckets": [{"doc_count": 3}, {"doc_count": 4}, {"doc_count": 5}]}


def test_filters_doc_in_multiple_filters_counted_in_each(client, main):
    res = _agg(client, main, {"filters": {"filters": {"a": {"term": {"tags": "a"}}, "c": {"term": {"tags": "c"}}}}})
    assert res["buckets"] == {"a": {"doc_count": 4}, "c": {"doc_count": 3}}


def test_filters_with_sub_aggs(client, main):
    res = _agg(
        client, main,
        {"filters": {"filters": {"t": {"term": {"flag": True}}, "f": {"term": {"flag": False}}}}, "aggs": {"m": {"max": {"field": "n"}}}},
    )
    assert res["buckets"] == {"f": {"doc_count": 3, "m": {"value": 8.0}}, "t": {"doc_count": 4, "m": {"value": 7.0}}}


def test_filters_empty_is_400(client, main):
    r = _err(client, main, {"x": {"filters": {"filters": {}}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "[filters] cannot be empty."


def test_filters_keyed_false_is_not_supported(client, main):
    r = _err(client, main, {"x": {"filters": {"filters": {"a": {"match_all": {}}}, "keyed": False}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Unknown key for a VALUE_BOOLEAN in [x]: [keyed]."


@pytest.mark.parametrize(
    "field,expected",
    [("color", 1), ("ip", 3), ("tags", 3), ("flag", 3), ("ts", 2), ("price", 1), ("n", 0), ("nope", 10), ("comments.author", 10)],
)
def test_missing_agg(client, main, field, expected):
    assert _agg(client, main, {"missing": {"field": field}}) == {"doc_count": expected}


def test_missing_agg_with_sub_agg_and_query(client, main):
    res = _agg(client, main, {"missing": {"field": "ip"}, "aggs": {"c": {"terms": {"field": "color"}}}}, query={"range": {"n": {"gte": 7}}})
    assert res["doc_count"] == 3
    assert _kc(res["c"]["buckets"]) == [("Red", 1), ("yellow", 1)]


def test_global_ignores_query(client, main):
    res = _aggs(
        client, main,
        {"g": {"global": {}, "aggs": {"c": {"value_count": {"field": "n"}}}}, "c": {"value_count": {"field": "n"}}},
        query={"term": {"color": "red"}},
    )
    assert res == {"c": {"value": 3}, "g": {"doc_count": 10, "c": {"value": 10}}}


def test_global_with_filter_inside(client, main):
    res = _agg(
        client, main,
        {"global": {}, "aggs": {"f": {"filter": {"term": {"color": "blue"}}}}},
        query={"term": {"color": "red"}},
    )
    assert res == {"doc_count": 10, "f": {"doc_count": 3}}


def test_global_does_not_count_nested_docs(client, main):
    res = _agg(client, main, {"global": {}}, query={"match_none": {}})
    assert res == {"doc_count": 10}


def test_global_with_body_is_400(client, main):
    r = _err(client, main, {"x": {"global": {"x": 1}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "parsing_exception"


def test_global_as_sub_agg_is_error(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color"}, "aggs": {"g": {"global": {}}}}})
    assert r.status_code == 500
    assert _root(r)["type"] == "aggregation_execution_exception"
    assert _root(r)["reason"] == (
        "Aggregation [x] cannot have a global sub-aggregation [g]. Global aggregations can only be defined as top level aggregations"
    )


# ===========================================================================
# nested / reverse_nested
# ===========================================================================


def test_nested_doc_count_counts_nested_objects(client, main):
    assert _agg(client, main, {"nested": {"path": "comments"}}) == {"doc_count": 4}


def test_nested_respects_root_query(client, main):
    assert _agg(client, main, {"nested": {"path": "comments"}}, query={"term": {"color": "red"}}) == {"doc_count": 3}


def test_nested_terms_and_reverse_nested_to_root(client, main):
    res = _agg(
        client, main,
        {"nested": {"path": "comments"}, "aggs": {"au": {"terms": {"field": "comments.author"}, "aggs": {
            "back": {"reverse_nested": {}, "aggs": {"col": {"terms": {"field": "color"}}}},
        }}}},
    )
    assert res["doc_count"] == 4
    assert [(b["key"], b["doc_count"], b["back"]["doc_count"], _kc(b["back"]["col"]["buckets"])) for b in res["au"]["buckets"]] == [
        ("alice", 2, 2, [("red", 2)]),
        ("bob", 1, 1, [("red", 1)]),
        ("carol", 1, 1, [("blue", 1)]),
    ]


def test_nested_histogram_on_nested_field(client, main):
    res = _agg(client, main, {"nested": {"path": "comments"}, "aggs": {"st": {"histogram": {"field": "comments.stars", "interval": 2}}}})
    assert _kc(res["st"]["buckets"]) == [(2.0, 2), (4.0, 2)]


def test_nested_filter_then_reverse_nested_dedupes_parents(client, main):
    res = _agg(
        client, main,
        {"nested": {"path": "comments"}, "aggs": {"f": {"filter": {"range": {"comments.stars": {"gte": 3}}}, "aggs": {"r": {"reverse_nested": {}}}}}},
    )
    # stars 5,3 (doc1) and 4 (doc2) -> 3 nested docs, 2 root docs
    assert res == {"doc_count": 4, "f": {"doc_count": 3, "r": {"doc_count": 2}}}


def test_nested_field_not_visible_from_root(client, main):
    res = _agg(client, main, {"terms": {"field": "comments.author"}})
    assert res["buckets"] == []


def test_nested_unknown_path_is_zero(client, main):
    assert _agg(client, main, {"nested": {"path": "nope"}}) == {"doc_count": 0}


def test_nested_path_on_non_nested_field_is_zero(client, main):
    assert _agg(client, main, {"nested": {"path": "color"}}) == {"doc_count": 0}


def test_reverse_nested_outside_nested_is_400(client, main):
    r = _err(client, main, {"x": {"reverse_nested": {}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Reverse nested aggregation [x] can only be used inside a [nested] aggregation"


def test_reverse_nested_unknown_path_is_zero(client, main):
    res = _agg(
        client, main,
        {"nested": {"path": "comments"}, "aggs": {"au": {"terms": {"field": "comments.author"}, "aggs": {"back": {"reverse_nested": {"path": "nope"}}}}}},
    )
    assert [b["back"] for b in res["au"]["buckets"]] == [{"doc_count": 0}] * 3


LIB_MAPPING = {
    "properties": {
        "shelf": {"type": "keyword"},
        "authors": {
            "type": "nested",
            "properties": {
                "name": {"type": "keyword"},
                "books": {"type": "nested", "properties": {"genre": {"type": "keyword"}}},
            },
        },
    }
}

LIB_DOCS = [
    {"shelf": "s1", "authors": [
        {"name": "ann", "books": [{"genre": "scifi"}, {"genre": "poetry"}]},
        {"name": "bob", "books": [{"genre": "scifi"}]},
    ]},
    {"shelf": "s2", "authors": [{"name": "ann", "books": [{"genre": "drama"}, {"genre": "scifi"}]}]},
    {"shelf": "s3", "authors": [{"name": "cid", "books": []}]},
]


@pytest.fixture(scope="module")
def lib(client):
    name = _make_index(client, LIB_MAPPING, LIB_DOCS)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def test_multi_level_nested_counts(client, lib):
    res = _agg(client, lib, {"nested": {"path": "authors"}, "aggs": {"b": {"nested": {"path": "authors.books"}}}})
    assert res == {"doc_count": 4, "b": {"doc_count": 5}}


def test_multi_level_nested_direct_path(client, lib):
    assert _agg(client, lib, {"nested": {"path": "authors.books"}}) == {"doc_count": 5}


def test_reverse_nested_to_intermediate_path_and_root(client, lib):
    res = _agg(
        client, lib,
        {"nested": {"path": "authors"}, "aggs": {"books": {"nested": {"path": "authors.books"}, "aggs": {
            "g": {"terms": {"field": "authors.books.genre"}, "aggs": {
                "to_author": {"reverse_nested": {"path": "authors"}, "aggs": {"n": {"terms": {"field": "authors.name"}}}},
                "to_root": {"reverse_nested": {}, "aggs": {"s": {"terms": {"field": "shelf"}}}},
            }},
        }}}},
    )
    out = [
        (b["key"], b["doc_count"], _kc(b["to_author"]["n"]["buckets"]), b["to_root"]["doc_count"], _kc(b["to_root"]["s"]["buckets"]))
        for b in res["books"]["g"]["buckets"]
    ]
    assert out == [
        ("scifi", 3, [("ann", 2), ("bob", 1)], 2, [("s1", 1), ("s2", 1)]),
        ("drama", 1, [("ann", 1)], 1, [("s2", 1)]),
        ("poetry", 1, [("ann", 1)], 1, [("s1", 1)]),
    ]


def test_reverse_nested_author_count_dedupes_per_author_object(client, lib):
    res = _agg(
        client, lib,
        {"nested": {"path": "authors.books"}, "aggs": {"f": {"filter": {"term": {"authors.books.genre": "scifi"}}, "aggs": {
            "a": {"reverse_nested": {"path": "authors"}},
        }}}},
    )
    assert res == {"doc_count": 5, "f": {"doc_count": 3, "a": {"doc_count": 3}}}


# ===========================================================================
# children / parent (join field)
# ===========================================================================


@pytest.fixture(scope="module")
def qa(client):
    name = _make_index(
        client,
        {"properties": {"rel": {"type": "join", "relations": {"question": "answer"}}, "tag": {"type": "keyword"}, "owner": {"type": "keyword"}}},
        [],
    )
    docs = [
        ({"_id": "q1"}, {"rel": "question", "tag": "java"}),
        ({"_id": "q2"}, {"rel": "question", "tag": "python"}),
        ({"_id": "q3"}, {"rel": "question", "tag": "java"}),
        ({"_id": "a1", "routing": "q1"}, {"rel": {"name": "answer", "parent": "q1"}, "owner": "sam"}),
        ({"_id": "a2", "routing": "q1"}, {"rel": {"name": "answer", "parent": "q1"}, "owner": "kim"}),
        ({"_id": "a3", "routing": "q2"}, {"rel": {"name": "answer", "parent": "q2"}, "owner": "sam"}),
    ]
    _bulk(client, name, docs)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def test_children_agg(client, qa):
    res = _agg(client, qa, {"terms": {"field": "tag"}, "aggs": {"ans": {"children": {"type": "answer"}, "aggs": {"o": {"terms": {"field": "owner"}}}}}})
    assert [(b["key"], b["doc_count"], b["ans"]["doc_count"], _kc(b["ans"]["o"]["buckets"])) for b in res["buckets"]] == [
        ("java", 2, 2, [("kim", 1), ("sam", 1)]),
        ("python", 1, 1, [("sam", 1)]),
    ]


def test_parent_agg(client, qa):
    res = _agg(client, qa, {"terms": {"field": "owner"}, "aggs": {"q": {"parent": {"type": "answer"}, "aggs": {"t": {"terms": {"field": "tag"}}}}}})
    assert [(b["key"], b["doc_count"], b["q"]["doc_count"], _kc(b["q"]["t"]["buckets"])) for b in res["buckets"]] == [
        ("sam", 2, 2, [("java", 1), ("python", 1)]),
        ("kim", 1, 1, [("java", 1)]),
    ]


def test_children_top_level_counts_all_children_of_matching_parents(client, qa):
    assert _agg(client, qa, {"children": {"type": "answer"}}) == {"doc_count": 3}
    assert _agg(client, qa, {"children": {"type": "answer"}}, query={"term": {"tag": "python"}}) == {"doc_count": 1}


def test_parent_dedupes_parent_docs(client, qa):
    assert _agg(client, qa, {"parent": {"type": "answer"}}) == {"doc_count": 2}


@pytest.mark.parametrize("agg", [{"children": {"type": "question"}}, {"children": {"type": "nope"}}, {"parent": {"type": "question"}}])
def test_join_aggs_with_wrong_type_are_empty(client, qa, agg):
    assert _agg(client, qa, agg) == {"doc_count": 0}


def test_terms_on_join_field(client, qa):
    res = _agg(client, qa, {"terms": {"field": "rel"}})
    assert _kc(res["buckets"]) == [("answer", 3), ("question", 3)]
    res = _agg(client, qa, {"terms": {"field": "rel#question"}})
    assert _kc(res["buckets"]) == [("q1", 3), ("q2", 2), ("q3", 1)]


# ===========================================================================
# composite
# ===========================================================================

COLOR_TAGS_ALL = [
    ({"c": None, "t": "a"}, 1),
    ({"c": "Red", "t": None}, 1),
    ({"c": "blue", "t": "a"}, 1),
    ({"c": "blue", "t": "b"}, 1),
    ({"c": "blue", "t": "c"}, 3),
    ({"c": "green", "t": None}, 1),
    ({"c": "red", "t": "a"}, 2),
    ({"c": "red", "t": "b"}, 1),
    ({"c": "red", "t": "d"}, 1),
    ({"c": "yellow", "t": None}, 1),
]

CT_SOURCES = [
    {"c": {"terms": {"field": "color", "missing_bucket": True}}},
    {"t": {"terms": {"field": "tags", "missing_bucket": True}}},
]


def test_composite_single_terms_source(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"col": {"terms": {"field": "color"}}}]}})
    assert res == {
        "after_key": {"col": "yellow"},
        "buckets": [
            {"key": {"col": "Red"}, "doc_count": 1},
            {"key": {"col": "blue"}, "doc_count": 3},
            {"key": {"col": "green"}, "doc_count": 1},
            {"key": {"col": "red"}, "doc_count": 3},
            {"key": {"col": "yellow"}, "doc_count": 1},
        ],
    }


def test_composite_size_and_after_key(client, main):
    src = [{"col": {"terms": {"field": "color"}}}]
    p1 = _agg(client, main, {"composite": {"size": 2, "sources": src}})
    assert p1["after_key"] == {"col": "blue"}
    assert [b["key"]["col"] for b in p1["buckets"]] == ["Red", "blue"]
    p2 = _agg(client, main, {"composite": {"size": 2, "sources": src, "after": p1["after_key"]}})
    assert p2 == {"after_key": {"col": "red"}, "buckets": [{"key": {"col": "green"}, "doc_count": 1}, {"key": {"col": "red"}, "doc_count": 3}]}
    p3 = _agg(client, main, {"composite": {"size": 2, "sources": src, "after": p2["after_key"]}})
    assert p3 == {"after_key": {"col": "yellow"}, "buckets": [{"key": {"col": "yellow"}, "doc_count": 1}]}
    p4 = _agg(client, main, {"composite": {"size": 2, "sources": src, "after": p3["after_key"]}})
    assert p4 == {"buckets": []}


def test_composite_after_arbitrary_value_not_in_index(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"col": {"terms": {"field": "color"}}}], "after": {"col": "c"}}})
    assert [b["key"]["col"] for b in res["buckets"]] == ["green", "red", "yellow"]


def test_composite_two_sources_full(client, main):
    res = _agg(client, main, {"composite": {"size": 100, "sources": CT_SOURCES}})
    assert [(b["key"], b["doc_count"]) for b in res["buckets"]] == COLOR_TAGS_ALL
    assert res["after_key"] == {"c": "yellow", "t": None}


@pytest.mark.parametrize("size", [1, 2, 3, 4, 7, 9, 10, 11])
def test_composite_pagination_walks_all_buckets(client, main, size):
    seen, after, pages = [], None, 0
    while True:
        body = {"size": size, "sources": CT_SOURCES}
        if after is not None:
            body["after"] = after
        res = _agg(client, main, {"composite": body})
        pages += 1
        if not res["buckets"]:
            assert "after_key" not in res
            break
        assert len(res["buckets"]) <= size
        seen.extend((b["key"], b["doc_count"]) for b in res["buckets"])
        assert res["after_key"] == res["buckets"][-1]["key"]
        after = res["after_key"]
        assert pages < 50
    assert seen == COLOR_TAGS_ALL


def test_composite_missing_bucket_null_sorts_first_asc(client, main):
    res = _agg(client, main, {"composite": {"size": 3, "sources": [{"col": {"terms": {"field": "color", "missing_bucket": True}}}]}})
    assert [(b["key"]["col"], b["doc_count"]) for b in res["buckets"]] == [(None, 1), ("Red", 1), ("blue", 3)]


def test_composite_missing_bucket_null_sorts_last_desc(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"col": {"terms": {"field": "color", "order": "desc", "missing_bucket": True}}}]}})
    assert [b["key"]["col"] for b in res["buckets"]] == ["yellow", "red", "green", "blue", "Red", None]


def test_composite_after_null(client, main):
    res = _agg(client, main, {"composite": {"size": 2, "sources": [{"col": {"terms": {"field": "color", "missing_bucket": True}}}], "after": {"col": None}}})
    assert [b["key"]["col"] for b in res["buckets"]] == ["Red", "blue"]


def test_composite_without_missing_bucket_skips_docs(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"col": {"terms": {"field": "color"}}}, {"t": {"terms": {"field": "tags"}}}]}})
    assert sum(b["doc_count"] for b in res["buckets"]) == 9
    assert all(None not in b["key"].values() for b in res["buckets"])


def test_composite_mixed_order_sources(client, main):
    res = _agg(client, main, {"composite": {"sources": [
        {"c": {"terms": {"field": "color", "order": "desc"}}},
        {"t": {"terms": {"field": "tags", "order": "asc"}}},
    ]}})
    assert [(b["key"]["c"], b["key"]["t"]) for b in res["buckets"]] == [
        ("red", "a"), ("red", "b"), ("red", "d"), ("blue", "a"), ("blue", "b"), ("blue", "c"),
    ]


def test_composite_source_order_determines_key_priority(client, main):
    res = _agg(client, main, {"composite": {"sources": [
        {"t": {"terms": {"field": "tags"}}}, {"c": {"terms": {"field": "color"}}},
    ]}})
    assert [(b["key"]["t"], b["key"]["c"], b["doc_count"]) for b in res["buckets"]] == [
        ("a", "blue", 1), ("a", "red", 2), ("b", "blue", 1), ("b", "red", 1), ("c", "blue", 3), ("d", "red", 1),
    ]
    assert list(res["buckets"][0]["key"].keys()) == ["t", "c"]


def test_composite_boolean_terms_source(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"col": {"terms": {"field": "color"}}}, {"f": {"terms": {"field": "flag"}}}]}})
    assert [(b["key"], b["doc_count"]) for b in res["buckets"]] == [
        ({"col": "blue", "f": True}, 2), ({"col": "green", "f": False}, 1), ({"col": "red", "f": False}, 1),
        ({"col": "red", "f": True}, 1), ({"col": "yellow", "f": True}, 1),
    ]


def test_composite_numeric_terms_source(client, main):
    res = _agg(client, main, {"composite": {"size": 3, "sources": [{"n": {"terms": {"field": "n", "order": "desc"}}}]}})
    assert res == {"after_key": {"n": 7}, "buckets": [
        {"key": {"n": 10}, "doc_count": 1}, {"key": {"n": 8}, "doc_count": 1}, {"key": {"n": 7}, "doc_count": 1},
    ]}


def test_composite_histogram_source(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"h": {"histogram": {"field": "price", "interval": 10}}}]}})
    assert [(b["key"]["h"], b["doc_count"]) for b in res["buckets"]] == [
        (-20.0, 1), (-10.0, 1), (0.0, 4), (10.0, 2), (100.0, 1),
    ]
    assert res["after_key"] == {"h": 100.0}


def test_composite_histogram_missing_bucket(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"h": {"histogram": {"field": "price", "interval": 50, "missing_bucket": True}}}]}})
    assert [(b["key"]["h"], b["doc_count"]) for b in res["buckets"]] == [(None, 1), (-50.0, 2), (0.0, 6), (100.0, 1)]


def test_composite_date_histogram_source_raw_keys(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"d": {"date_histogram": {"field": "ts", "calendar_interval": "year"}}}]}})
    assert res == {"after_key": {"d": 1735689600000}, "buckets": [
        {"key": {"d": 1672531200000}, "doc_count": 1},
        {"key": {"d": 1704067200000}, "doc_count": 6},
        {"key": {"d": 1735689600000}, "doc_count": 1},
    ]}


def test_composite_date_histogram_format(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"d": {"date_histogram": {"field": "ts", "calendar_interval": "year", "format": "yyyy"}}}]}})
    assert res["after_key"] == {"d": "2025"}
    assert [(b["key"]["d"], b["doc_count"]) for b in res["buckets"]] == [("2023", 1), ("2024", 6), ("2025", 1)]


def test_composite_date_histogram_after_with_formatted_key(client, main):
    res = _agg(client, main, {"composite": {
        "sources": [{"d": {"date_histogram": {"field": "ts", "calendar_interval": "year", "format": "yyyy"}}}],
        "after": {"d": "2023"},
    }})
    assert [b["key"]["d"] for b in res["buckets"]] == ["2024", "2025"]


def test_composite_date_histogram_time_zone(client, tzidx):
    res = _agg(
        client, tzidx,
        {"composite": {"sources": [{"d": {"date_histogram": {"field": "t", "calendar_interval": "day", "time_zone": "America/New_York", "format": "yyyy-MM-dd"}}}]}},
        query=NY_FALL,
    )
    assert [(b["key"]["d"], b["doc_count"]) for b in res["buckets"]] == [("2024-11-03", 3), ("2024-11-04", 1)]


def test_composite_with_sub_aggs(client, main):
    res = _agg(
        client, main,
        {"composite": {"sources": [{"col": {"terms": {"field": "color"}}}]}, "aggs": {"x": {"terms": {"field": "tags"}}}},
        query={"term": {"color": "blue"}},
    )
    assert res["after_key"] == {"col": "blue"}
    assert _kc(res["buckets"][0]["x"]["buckets"]) == [("c", 3), ("a", 1), ("b", 1)]


def test_composite_unmapped_source_without_missing_bucket(client, main):
    assert _agg(client, main, {"composite": {"sources": [{"t": {"terms": {"field": "nope"}}}]}}) == {"buckets": []}


def test_composite_unmapped_source_with_missing_bucket(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"t": {"terms": {"field": "nope", "missing_bucket": True}}}]}})
    assert res["buckets"] == [{"key": {"t": None}, "doc_count": 10}]


def test_composite_no_hits(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"col": {"terms": {"field": "color"}}}]}}, query={"term": {"color": "nothing"}})
    assert res == {"buckets": []}


def test_composite_empty_sources_is_400(client, main):
    r = _err(client, main, {"x": {"composite": {"sources": []}}})
    assert r.status_code == 400
    assert "Composite [sources] cannot be null or empty" in r.text


def test_composite_after_with_wrong_key_is_400(client, main):
    r = _err(client, main, {"x": {"composite": {"sources": [{"col": {"terms": {"field": "color"}}}], "after": {"zzz": "blue"}}}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Missing value for [after.col]"


def test_composite_script_source(client, main):
    res = _agg(client, main, {"composite": {"sources": [{"m": {"terms": {"script": {"source": "doc['n'].value % 2"}}}}]}})
    # script values default to string keys
    assert [(b["key"]["m"], b["doc_count"]) for b in res["buckets"]] == [("0", 6), ("1", 4)]


# ===========================================================================
# sampler / diversified_sampler
# ===========================================================================

BOOSTED = {"bool": {"should": [
    {"constant_score": {"filter": {"term": {"color": "red"}}, "boost": 3}},
    {"constant_score": {"filter": {"term": {"color": "blue"}}, "boost": 2}},
]}}


def test_sampler_takes_top_scoring_docs(client, main):
    res = _agg(client, main, {"sampler": {"shard_size": 3}, "aggs": {"c": {"terms": {"field": "color"}}}}, query=BOOSTED)
    assert res["doc_count"] == 3
    assert _kc(res["c"]["buckets"]) == [("red", 3)]


def test_sampler_shard_size_larger(client, main):
    res = _agg(client, main, {"sampler": {"shard_size": 5}, "aggs": {"c": {"terms": {"field": "color"}}}}, query=BOOSTED)
    assert res["doc_count"] == 5
    assert _kc(res["c"]["buckets"]) == [("red", 3), ("blue", 2)]


def test_sampler_default_shard_size_covers_small_index(client, main):
    res = _agg(client, main, {"sampler": {}, "aggs": {"m": {"max": {"field": "n"}}}})
    assert res == {"doc_count": 10, "m": {"value": 10.0}}


def test_diversified_sampler_max_docs_per_value(client, main):
    q = {"bool": {"filter": {"terms": {"color": ["red", "blue", "Red"]}}}}
    one = _agg(client, main, {"diversified_sampler": {"shard_size": 10, "field": "color"}, "aggs": {"c": {"terms": {"field": "color"}}}}, query=q)
    assert one["doc_count"] == 3
    assert _kc(one["c"]["buckets"]) == [("Red", 1), ("blue", 1), ("red", 1)]
    two = _agg(client, main, {"diversified_sampler": {"shard_size": 10, "field": "color", "max_docs_per_value": 2}, "aggs": {"c": {"terms": {"field": "color"}}}}, query=q)
    assert two["doc_count"] == 5
    assert _kc(two["c"]["buckets"]) == [("blue", 2), ("red", 2), ("Red", 1)]


@pytest.mark.parametrize("agg", [{"sampler": {}}, {"diversified_sampler": {"field": "color"}}])
def test_sampler_without_sub_aggs_is_error(client, main, agg):
    r = _err(client, main, {"x": agg})
    assert r.status_code == 500
    assert _root(r)["reason"] == "Sampler aggregation must be used with child aggregations."


# ===========================================================================
# adjacency_matrix
# ===========================================================================


def test_adjacency_matrix(client, main):
    res = _agg(client, main, {"adjacency_matrix": {"filters": {
        "red": {"term": {"color": "red"}}, "ta": {"term": {"tags": "a"}}, "tc": {"term": {"tags": "c"}},
    }}})
    # red&tc has no docs and is omitted
    assert res == {"buckets": [
        {"key": "red", "doc_count": 3}, {"key": "red&ta", "doc_count": 2}, {"key": "ta", "doc_count": 4},
        {"key": "ta&tc", "doc_count": 1}, {"key": "tc", "doc_count": 3},
    ]}


def test_adjacency_matrix_custom_separator(client, main):
    res = _agg(client, main, {"adjacency_matrix": {"separator": "+", "filters": {"red": {"term": {"color": "red"}}, "ta": {"term": {"tags": "a"}}}}})
    assert _kc(res["buckets"]) == [("red", 3), ("red+ta", 2), ("ta", 4)]


def test_adjacency_matrix_keys_sorted_and_pairs_ordered_by_name(client, main):
    res = _agg(client, main, {"adjacency_matrix": {"filters": {"z": {"term": {"tags": "a"}}, "b": {"term": {"tags": "c"}}}}})
    assert _kc(res["buckets"]) == [("b", 3), ("b&z", 1), ("z", 4)]


def test_adjacency_matrix_with_sub_agg(client, main):
    res = _agg(client, main, {"adjacency_matrix": {"filters": {"blue": {"term": {"color": "blue"}}, "tc": {"term": {"tags": "c"}}}}, "aggs": {"s": {"sum": {"field": "n"}}}})
    assert [(b["key"], b["doc_count"], b["s"]["value"]) for b in res["buckets"]] == [
        ("blue", 3, 5.0), ("blue&tc", 3, 5.0), ("tc", 3, 5.0),
    ]


def test_adjacency_matrix_empty_filters_is_error(client, main):
    r = _err(client, main, {"x": {"adjacency_matrix": {"filters": {}}}})
    assert r.status_code == 500
    assert _root(r)["reason"] == "[x] is missing : filters parameter"


# ===========================================================================
# geo bucket aggregations
# ===========================================================================

GEO_DOCS = [
    {"city": "ams", "p": {"lat": 52.37, "lon": 4.89}},
    {"city": "ams", "p": {"lat": 52.36, "lon": 4.90}},
    {"city": "ams", "p": {"lat": 52.38, "lon": 4.88}},
    {"city": "ber", "p": {"lat": 52.52, "lon": 13.40}},
    {"city": "ber", "p": {"lat": 52.51, "lon": 13.39}},
    {"city": "kyiv", "p": {"lat": 50.45, "lon": 30.52}},
    {"city": "ny", "p": {"lat": 40.71, "lon": -74.0}},
    {"city": "none"},
]


@pytest.fixture(scope="module")
def geo(client):
    name = _make_index(client, {"properties": {"city": {"type": "keyword"}, "p": {"type": "geo_point"}}}, GEO_DOCS)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


AMS = {"lat": 52.37, "lon": 4.89}


def test_geohash_grid_precision_1(client, geo):
    assert _agg(client, geo, {"geohash_grid": {"field": "p", "precision": 1}}) == {"buckets": [
        {"key": "u", "doc_count": 6}, {"key": "d", "doc_count": 1},
    ]}


def test_geohash_grid_precision_3(client, geo):
    res = _agg(client, geo, {"geohash_grid": {"field": "p", "precision": 3}})
    assert _kc(res["buckets"]) == [("u17", 3), ("u33", 2), ("u8v", 1), ("dr5", 1)]


def test_geohash_grid_size(client, geo):
    res = _agg(client, geo, {"geohash_grid": {"field": "p", "precision": 3, "size": 2}})
    assert _kc(res["buckets"]) == [("u17", 3), ("u33", 2)]


def test_geohash_grid_default_precision_5(client, geo):
    res = _agg(client, geo, {"geohash_grid": {"field": "p"}})
    assert all(len(b["key"]) == 5 for b in res["buckets"])
    assert sum(b["doc_count"] for b in res["buckets"]) == 7


@pytest.mark.parametrize("precision", [0, 13])
def test_geohash_grid_invalid_precision(client, geo, precision):
    r = _err(client, geo, {"x": {"geohash_grid": {"field": "p", "precision": precision}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == f"Invalid geohash aggregation precision of {precision}. Must be between 1 and 12."


def test_geohash_grid_with_sub_agg(client, geo):
    res = _agg(client, geo, {"geohash_grid": {"field": "p", "precision": 1}, "aggs": {"c": {"terms": {"field": "city"}}}})
    assert [(b["key"], _kc(b["c"]["buckets"])) for b in res["buckets"]] == [
        ("u", [("ams", 3), ("ber", 2), ("kyiv", 1)]), ("d", [("ny", 1)]),
    ]


def test_geotile_grid_precision_0(client, geo):
    assert _agg(client, geo, {"geotile_grid": {"field": "p", "precision": 0}}) == {"buckets": [{"key": "0/0/0", "doc_count": 7}]}


def test_geotile_grid_precision_1(client, geo):
    res = _agg(client, geo, {"geotile_grid": {"field": "p", "precision": 1}})
    assert _kc(res["buckets"]) == [("1/1/0", 6), ("1/0/0", 1)]


def test_geotile_grid_precision_2(client, geo):
    res = _agg(client, geo, {"geotile_grid": {"field": "p", "precision": 2}})
    assert _kc(res["buckets"]) == [("2/2/1", 6), ("2/1/1", 1)]


def test_geotile_grid_invalid_precision(client, geo):
    r = _err(client, geo, {"x": {"geotile_grid": {"field": "p", "precision": 30}}})
    assert r.status_code == 400
    assert r.json()["error"]["caused_by"]["reason"] == "Invalid geotile_grid precision of 30. Must be between 0 and 29."


def test_geo_distance_km(client, geo):
    res = _agg(client, geo, {"geo_distance": {"field": "p", "origin": AMS, "unit": "km", "ranges": [
        {"to": 100}, {"from": 100, "to": 1000}, {"from": 1000},
    ]}})
    assert res == {"buckets": [
        {"key": "*-100.0", "from": 0.0, "to": 100.0, "doc_count": 3},
        {"key": "100.0-1000.0", "from": 100.0, "to": 1000.0, "doc_count": 2},
        {"key": "1000.0-*", "from": 1000.0, "doc_count": 2},
    ]}


def test_geo_distance_default_unit_is_meters(client, geo):
    res = _agg(client, geo, {"geo_distance": {"field": "p", "origin": "52.37,4.89", "ranges": [{"to": 100000}, {"from": 100000}]}})
    assert _kc(res["buckets"]) == [("*-100000.0", 3), ("100000.0-*", 4)]


def test_geo_distance_keyed_custom_keys(client, geo):
    res = _agg(client, geo, {"geo_distance": {"field": "p", "origin": AMS, "unit": "km", "keyed": True, "ranges": [
        {"key": "near", "to": 700}, {"key": "far", "from": 700},
    ]}})
    assert res == {"buckets": {
        "near": {"from": 0.0, "to": 700.0, "doc_count": 5},
        "far": {"from": 700.0, "doc_count": 2},
    }}


def test_geo_distance_miles_and_plane(client, geo):
    res = _agg(client, geo, {"geo_distance": {"field": "p", "origin": [4.89, 52.37], "unit": "mi", "distance_type": "plane", "ranges": [
        {"to": 1000}, {"from": 1000, "to": 2000}, {"from": 2000},
    ]}})
    assert _kc(res["buckets"]) == [("*-1000.0", 5), ("1000.0-2000.0", 1), ("2000.0-*", 1)]


def test_geo_distance_with_sub_agg(client, geo):
    res = _agg(client, geo, {"geo_distance": {"field": "p", "origin": AMS, "unit": "km", "ranges": [{"from": 100, "to": 2000}]}, "aggs": {"c": {"terms": {"field": "city"}}}})
    assert _kc(res["buckets"][0]["c"]["buckets"]) == [("ber", 2), ("kyiv", 1)]


# ===========================================================================
# deep nesting of bucket aggregations
# ===========================================================================


def test_three_level_bucket_nesting(client, main):
    res = _agg(
        client, main,
        {"terms": {"field": "color", "size": 2}, "aggs": {
            "t": {"terms": {"field": "tags"}, "aggs": {
                "r": {"range": {"field": "n", "ranges": [{"to": 3}, {"from": 3}]}, "aggs": {
                    "f": {"terms": {"field": "flag"}},
                }},
            }},
        }},
    )
    flat = []
    for c in res["buckets"]:
        for t in c["t"]["buckets"]:
            for r in t["r"]["buckets"]:
                flat.append((c["key"], t["key"], r["key"], r["doc_count"], [x["key_as_string"] for x in r["f"]["buckets"]]))
    assert flat == [
        ("blue", "c", "*-3.0", 1, []),
        ("blue", "c", "3.0-*", 2, ["true"]),
        ("blue", "a", "*-3.0", 1, []),
        ("blue", "a", "3.0-*", 0, []),
        ("blue", "b", "*-3.0", 0, []),
        ("blue", "b", "3.0-*", 1, ["true"]),
        ("red", "a", "*-3.0", 2, ["false", "true"]),
        ("red", "a", "3.0-*", 0, []),
        ("red", "b", "*-3.0", 1, ["true"]),
        ("red", "b", "3.0-*", 0, []),
        ("red", "d", "*-3.0", 0, []),
        ("red", "d", "3.0-*", 1, []),
    ]


def test_deep_nesting_date_histogram_filters_terms(client, main):
    res = _agg(
        client, main,
        {"date_histogram": {"field": "ts", "calendar_interval": "year", "format": "yyyy"}, "aggs": {
            "fs": {"filters": {"filters": {"cheap": {"range": {"price": {"lt": 5}}}, "pricey": {"range": {"price": {"gte": 5}}}}}, "aggs": {
                "c": {"terms": {"field": "color", "missing": "?"}},
            }},
        }},
    )
    out = {
        (b["key_as_string"], name): _kc(fb["c"]["buckets"])
        for b in res["buckets"] for name, fb in b["fs"]["buckets"].items()
    }
    assert out == {
        ("2023", "cheap"): [], ("2023", "pricey"): [("?", 1)],
        ("2024", "cheap"): [("red", 3), ("blue", 1)], ("2024", "pricey"): [("blue", 1), ("green", 1)],
        ("2025", "cheap"): [], ("2025", "pricey"): [("yellow", 1)],
    }


def test_sibling_bucket_aggs_are_independent(client, main):
    res = _aggs(client, main, {
        "a": {"terms": {"field": "color", "size": 1}},
        "b": {"histogram": {"field": "n", "interval": 100}},
        "c": {"filter": {"term": {"tags": "d"}}},
    })
    assert res == {
        "a": {"doc_count_error_upper_bound": 0, "sum_other_doc_count": 6, "buckets": [{"key": "blue", "doc_count": 3}]},
        "b": {"buckets": [{"key": -100.0, "doc_count": 1}, {"key": 0.0, "doc_count": 9}]},
        "c": {"doc_count": 1},
    }


# ===========================================================================
# empty index / no hits / unmapped
# ===========================================================================

SHAPE_AGGS = {
    "kw": {"terms": {"field": "color"}},
    "lt": {"terms": {"field": "n"}},
    "mt": {"multi_terms": {"terms": [{"field": "color"}, {"field": "n"}]}},
    "rt": {"rare_terms": {"field": "color"}},
    "h": {"histogram": {"field": "n", "interval": 1}},
    "he": {"histogram": {"field": "n", "interval": 5, "extended_bounds": {"min": 0, "max": 10}}},
    "dh": {"date_histogram": {"field": "ts", "calendar_interval": "year"}},
    "dhe": {"date_histogram": {"field": "ts", "calendar_interval": "year", "format": "yyyy", "extended_bounds": {"min": "2020", "max": "2022"}}},
    "adh": {"auto_date_histogram": {"field": "ts"}},
    "vwh": {"variable_width_histogram": {"field": "n"}},
    "r": {"range": {"field": "n", "ranges": [{"to": 1}]}},
    "dr": {"date_range": {"field": "ts", "ranges": [{"to": "2020-01-01"}]}},
    "ipr": {"ip_range": {"field": "ip", "ranges": [{"to": "10.0.0.5"}]}},
    "f": {"filter": {"match_all": {}}},
    "fs": {"filters": {"filters": {"a": {"match_all": {}}}, "other_bucket": True}},
    "m": {"missing": {"field": "color"}},
    "g": {"global": {}},
    "ne": {"nested": {"path": "comments"}},
    "co": {"composite": {"sources": [{"a": {"terms": {"field": "color"}}}]}},
    "am": {"adjacency_matrix": {"filters": {"a": {"match_all": {}}}}},
    "gh": {"geohash_grid": {"field": "loc"}},
    "gd": {"geo_distance": {"field": "loc", "origin": "52.37,4.89", "ranges": [{"to": 1}]}},
}

EMPTY_SHAPES = {
    "kw": {**TERMS_HEADER, "buckets": []},
    "lt": {**TERMS_HEADER, "buckets": []},
    "mt": {**TERMS_HEADER, "buckets": []},
    "rt": {"buckets": []},
    "h": {"buckets": []},
    "he": {"buckets": [{"key": 0.0, "doc_count": 0}, {"key": 5.0, "doc_count": 0}, {"key": 10.0, "doc_count": 0}]},
    "dh": {"buckets": []},
    "dhe": {"buckets": [
        {"key_as_string": "2020", "key": 1577836800000, "doc_count": 0},
        {"key_as_string": "2021", "key": 1609459200000, "doc_count": 0},
        {"key_as_string": "2022", "key": 1640995200000, "doc_count": 0},
    ]},
    "adh": {"buckets": [], "interval": "1s"},
    "vwh": {"buckets": []},
    "r": {"buckets": [{"key": "*-1.0", "to": 1.0, "doc_count": 0}]},
    "dr": {"buckets": [{"key": "*-2020-01-01T00:00:00.000Z", "to": 1577836800000.0, "to_as_string": "2020-01-01T00:00:00.000Z", "doc_count": 0}]},
    "ipr": {"buckets": [{"key": "*-10.0.0.5", "to": "10.0.0.5", "doc_count": 0}]},
    "f": {"doc_count": 0},
    "fs": {"buckets": {"a": {"doc_count": 0}, "_other_": {"doc_count": 0}}},
    "m": {"doc_count": 0},
    "g": {"doc_count": 0},
    "ne": {"doc_count": 0},
    "co": {"buckets": []},
    "am": {"buckets": []},
    "gh": {"buckets": []},
    "gd": {"buckets": [{"key": "*-1.0", "from": 0.0, "to": 1.0, "doc_count": 0}]},
}

GEO_MAPPING = {"properties": {**MAIN_MAPPING["properties"], "loc": {"type": "geo_point"}}}


@pytest.mark.parametrize("name", sorted(SHAPE_AGGS))
def test_empty_index_agg_shapes(client, index_factory, name):
    idx = index_factory(mappings=GEO_MAPPING, settings=SETTINGS)
    res = _aggs(client, idx, {name: SHAPE_AGGS[name]})
    assert res == {name: EMPTY_SHAPES[name]}


@pytest.mark.parametrize("name", sorted(SHAPE_AGGS))
def test_index_without_mapping_agg_shapes(client, index_factory, name):
    idx = index_factory(settings=SETTINGS)
    res = _aggs(client, idx, {name: SHAPE_AGGS[name]})
    assert res == {name: EMPTY_SHAPES[name]}


def test_all_shapes_in_one_request_on_empty_index(client, index_factory):
    idx = index_factory(mappings=GEO_MAPPING, settings=SETTINGS)
    assert _aggs(client, idx, SHAPE_AGGS) == EMPTY_SHAPES


def test_significant_terms_shape_mapped_vs_unmapped(client, index_factory):
    mapped = index_factory(mappings=MAIN_MAPPING, settings=SETTINGS)
    unmapped = index_factory(settings=SETTINGS)
    agg = {"significant_terms": {"field": "color"}}
    assert _agg(client, mapped, agg) == {"doc_count": 0, "bg_count": 0, "buckets": []}
    assert _agg(client, unmapped, agg) == {"buckets": []}


@pytest.mark.parametrize(
    "agg,expected",
    [
        ({"terms": {"field": "color"}}, {**TERMS_HEADER, "buckets": []}),
        ({"histogram": {"field": "n", "interval": 1}}, {"buckets": []}),
        ({"date_histogram": {"field": "ts", "calendar_interval": "day"}}, {"buckets": []}),
        ({"range": {"field": "n", "ranges": [{"from": 0}]}}, {"buckets": [{"key": "0.0-*", "from": 0.0, "doc_count": 0}]}),
        ({"filters": {"filters": [{"match_all": {}}]}}, {"buckets": [{"doc_count": 0}]}),
        ({"missing": {"field": "color"}}, {"doc_count": 0}),
        ({"nested": {"path": "comments"}}, {"doc_count": 0}),
        ({"rare_terms": {"field": "tags"}}, {"buckets": []}),
    ],
)
def test_aggs_on_no_matching_docs(client, main, agg, expected):
    assert _agg(client, main, agg, query={"term": {"color": "nothing"}}) == expected


def test_terms_min_doc_count_zero_on_no_matching_docs(client, main):
    res = _agg(client, main, {"terms": {"field": "tags", "min_doc_count": 0}}, query={"match_none": {}})
    assert _kc(res["buckets"]) == [("a", 0), ("b", 0), ("c", 0), ("d", 0)]


def test_aggs_over_two_indices_one_unmapped(client, main, index_factory):
    other = index_factory(settings=SETTINGS)
    client.put(f"/{other}/_doc/1", json={"something": "else"}, params={"refresh": "true"})
    r = client.post(f"/{main},{other}/_search", json={"size": 0, "aggs": {"x": {"terms": {"field": "color", "size": 2}}}})
    assert r.status_code == 200
    assert r.json()["hits"]["total"]["value"] == 11
    assert _kc(r.json()["aggregations"]["x"]["buckets"]) == [("blue", 3), ("red", 3)]


# ===========================================================================
# request-level behaviour
# ===========================================================================


def test_size_zero_returns_no_hits_but_total(client, main):
    r = _search(client, main, {"size": 0, "aggs": {"x": {"terms": {"field": "color", "size": 1}}}})
    body = r.json()
    assert body["hits"]["hits"] == []
    assert body["hits"]["total"] == {"value": 10, "relation": "eq"}
    assert body["hits"]["max_score"] is None
    assert body["aggregations"]["x"]["buckets"] == [{"key": "blue", "doc_count": 3}]


def test_hits_and_aggs_together(client, main):
    r = _search(client, main, {"size": 2, "query": {"term": {"color": "red"}}, "sort": [{"n": "asc"}], "aggs": {"x": {"terms": {"field": "tags"}}}})
    body = r.json()
    assert [h["_id"] for h in body["hits"]["hits"]] == ["1", "2"]
    assert _kc(body["aggregations"]["x"]["buckets"]) == [("a", 2), ("b", 1), ("d", 1)]


def test_no_aggregations_key_without_aggs(client, main):
    r = _search(client, main, {"size": 0})
    assert "aggregations" not in r.json()


def test_aggregations_long_form_key(client, main):
    r = _search(client, main, {"size": 0, "aggregations": {"x": {"filter": {"term": {"color": "red"}}}}})
    assert r.status_code == 200
    assert r.json()["aggregations"] == {"x": {"doc_count": 3}}


def test_aggregations_long_form_in_sub_aggs(client, main):
    r = _search(client, main, {"size": 0, "aggs": {"x": {"filter": {"term": {"color": "red"}}, "aggregations": {"y": {"terms": {"field": "tags", "size": 1}}}}}})
    assert r.status_code == 200
    assert r.json()["aggregations"]["x"]["y"]["buckets"] == [{"key": "a", "doc_count": 2}]


def test_aggs_and_aggregations_together_last_one_wins(client, main):
    r = _search(client, main, {
        "size": 0,
        "aggs": {"x": {"filter": {"term": {"color": "red"}}}},
        "aggregations": {"y": {"filter": {"term": {"color": "blue"}}}},
    })
    assert r.status_code == 200
    assert r.json()["aggregations"] == {"y": {"doc_count": 3}}


def test_aggs_via_get_with_body_in_query_string_source(client, main):
    src = json.dumps({"size": 0, "aggs": {"x": {"missing": {"field": "color"}}}})
    r = client.get(f"/{main}/_search", params={"source": src, "source_content_type": "application/json"})
    assert r.status_code == 200
    assert r.json()["aggregations"] == {"x": {"doc_count": 1}}


def test_unknown_agg_type_is_400(client, main):
    r = _err(client, main, {"x": {"bogus_agg": {"field": "n"}}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "parsing_exception"
    assert err["reason"] == "Unknown aggregation type [bogus_agg]"
    assert err["caused_by"]["type"] == "named_object_not_found_exception"


def test_unknown_agg_type_in_sub_aggs_is_400(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "color"}, "aggs": {"y": {"nope": {}}}}})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == "Unknown aggregation type [nope]"


def test_two_agg_types_in_one_definition_is_400(client, main):
    r = _err(client, main, {"x": {"terms": {"field": "n"}, "histogram": {"field": "n", "interval": 1}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "parsing_exception"
    assert _root(r)["reason"] == "Found two aggregation type definitions in [x]: [terms] and [histogram]"


def test_empty_agg_definition_is_400(client, main):
    r = _err(client, main, {"x": {}})
    assert r.status_code == 400
    assert _root(r)["reason"] == "Missing definition for aggregation [x]"


def test_duplicate_agg_name_is_400(client, main):
    raw = '{"size":0,"aggs":{"x":{"terms":{"field":"n"}},"x":{"terms":{"field":"color"}}}}'
    r = client.post(f"/{main}/_search", content=raw.encode(), headers={"Content-Type": "application/json"})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "json_parse_exception"
    assert 'Duplicate Object property "x"' in r.json()["error"]["reason"]


def test_same_agg_name_at_different_levels_is_allowed(client, main):
    res = _aggs(client, main, {"x": {"filter": {"term": {"color": "red"}}, "aggs": {"x": {"terms": {"field": "tags", "size": 1}}}}})
    assert res == {"x": {"doc_count": 3, "x": {**TERMS_HEADER, "sum_other_doc_count": 2, "buckets": [{"key": "a", "doc_count": 2}]}}}


@pytest.mark.parametrize("name", ["a[b", "a]b", "a>b", "[x]", ">"])
def test_agg_name_with_illegal_characters(client, main, name):
    r = _err(client, main, {name: {"terms": {"field": "n"}}})
    assert r.status_code == 400
    assert _root(r)["type"] == "parsing_exception"
    assert _root(r)["reason"] == (
        f"Invalid aggregation name [{name}]. Aggregation names can contain any character except '[', ']', and '>'"
    )


@pytest.mark.parametrize("name", ["a.b", "with space", "ünï-cødé", "a#b", "_x", "1"])
def test_agg_name_with_allowed_characters(client, main, name):
    res = _aggs(client, main, {name: {"missing": {"field": "color"}}})
    assert res == {name: {"doc_count": 1}}


def test_meta_echoed_back(client, main):
    meta = {"color": "blue", "n": [1, 2], "nested": {"k": True}}
    res = _agg(client, main, {"terms": {"field": "color", "size": 1}, "meta": meta})
    assert res == {"meta": meta, **TERMS_HEADER, "sum_other_doc_count": 6, "buckets": [{"key": "blue", "doc_count": 3}]}


@pytest.mark.parametrize(
    "agg",
    [
        {"filter": {"match_all": {}}},
        {"histogram": {"field": "n", "interval": 100}},
        {"range": {"field": "n", "ranges": [{"to": 0}]}},
        {"global": {}},
        {"nested": {"path": "comments"}},
        {"composite": {"sources": [{"a": {"terms": {"field": "flag"}}}]}},
    ],
)
def test_meta_echoed_back_for_bucket_aggs(client, main, agg):
    res = _agg(client, main, {**agg, "meta": {"id": 7}})
    assert res["meta"] == {"id": 7}


def test_meta_on_sub_agg_inside_buckets(client, main):
    res = _agg(client, main, {"terms": {"field": "color", "size": 1}, "aggs": {"y": {"filter": {"match_all": {}}, "meta": {"s": 1}}}})
    assert res["buckets"][0]["y"] == {"meta": {"s": 1}, "doc_count": 3}


def test_meta_absent_by_default(client, main):
    res = _agg(client, main, {"filter": {"match_all": {}}})
    assert "meta" not in res


TYPED = [
    ({"terms": {"field": "color"}}, "sterms"),
    ({"terms": {"field": "n"}}, "lterms"),
    ({"terms": {"field": "flag"}}, "lterms"),
    ({"terms": {"field": "ts"}}, "lterms"),
    ({"terms": {"field": "price"}}, "dterms"),
    ({"terms": {"field": "ip"}}, "sterms"),
    ({"terms": {"field": "nope"}}, "sterms"),
    ({"multi_terms": {"terms": [{"field": "color"}, {"field": "n"}]}}, "multi_terms"),
    ({"rare_terms": {"field": "color"}}, "srareterms"),
    ({"rare_terms": {"field": "n"}}, "lrareterms"),
    ({"significant_terms": {"field": "color"}}, "sigsterms"),
    ({"significant_text": {"field": "body"}}, "sigsterms"),
    ({"histogram": {"field": "n", "interval": 1}}, "histogram"),
    ({"date_histogram": {"field": "ts", "calendar_interval": "year"}}, "date_histogram"),
    ({"auto_date_histogram": {"field": "ts"}}, "auto_date_histogram"),
    ({"variable_width_histogram": {"field": "n"}}, "variable_width_histogram"),
    ({"range": {"field": "n", "ranges": [{"to": 1}]}}, "range"),
    ({"date_range": {"field": "ts", "ranges": [{"to": "2024-01-01"}]}}, "date_range"),
    ({"ip_range": {"field": "ip", "ranges": [{"to": "10.0.0.5"}]}}, "ip_range"),
    ({"filter": {"match_all": {}}}, "filter"),
    ({"filters": {"filters": {"a": {"match_all": {}}}}}, "filters"),
    ({"missing": {"field": "color"}}, "missing"),
    ({"global": {}}, "global"),
    ({"nested": {"path": "comments"}}, "nested"),
    ({"composite": {"sources": [{"a": {"terms": {"field": "color"}}}]}}, "composite"),
    ({"sampler": {}, "aggs": {"m": {"max": {"field": "n"}}}}, "sampler"),
    ({"diversified_sampler": {"field": "color"}, "aggs": {"m": {"max": {"field": "n"}}}}, "sampler"),
    ({"adjacency_matrix": {"filters": {"a": {"match_all": {}}}}}, "adjacency_matrix"),
]


@pytest.mark.parametrize("agg,prefix", TYPED, ids=[f"{p}-{i}" for i, (_, p) in enumerate(TYPED)])
def test_typed_keys_prefix(client, main, agg, prefix):
    r = _search(client, main, {"size": 0, "aggs": {"x": agg}}, params={"typed_keys": "true"})
    assert r.status_code == 200
    assert list(r.json()["aggregations"]) == [f"{prefix}#x"]


def test_typed_keys_applies_to_sub_aggs(client, main):
    r = _search(
        client, main,
        {"size": 0, "aggs": {"x": {"terms": {"field": "color", "size": 1}, "aggs": {
            "m": {"max": {"field": "n"}}, "f": {"filter": {"term": {"flag": True}}},
            "ne": {"nested": {"path": "comments"}, "aggs": {"rn": {"reverse_nested": {}}}},
        }}}},
        params={"typed_keys": "true"},
    )
    b = r.json()["aggregations"]["sterms#x"]["buckets"][0]
    assert b == {
        "key": "blue", "doc_count": 3,
        "max#m": {"value": 4.0},
        "filter#f": {"doc_count": 2},
        "nested#ne": {"doc_count": 1, "reverse_nested#rn": {"doc_count": 1}},
    }


def test_typed_keys_false_by_default(client, main):
    r = _search(client, main, {"size": 0, "aggs": {"x": {"terms": {"field": "color"}}}}, params={"typed_keys": "false"})
    assert list(r.json()["aggregations"]) == ["x"]
