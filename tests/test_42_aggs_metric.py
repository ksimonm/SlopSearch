"""Metric and pipeline aggregations: exact OpenSearch 3.x behaviour.

All expectations below were hand-computed from the inline datasets and
verified against a reference OpenSearch node.
"""
import json
import math
import uuid

import pytest

pytestmark = pytest.mark.aggs

SETTINGS = {"number_of_shards": 1, "number_of_replicas": 0}

# ---------------------------------------------------------------------------
# Datasets
# ---------------------------------------------------------------------------

METRIC_MAPPING = {
    "properties": {
        "cat": {"type": "keyword"},
        "price": {"type": "integer"},
        "score": {"type": "double"},
        "fl": {"type": "float"},
        "vals": {"type": "integer"},
        "ts": {"type": "date"},
        "w": {"type": "integer"},
        "loc": {"type": "geo_point"},
        "name": {"type": "keyword"},
        "body": {"type": "text"},
        "x": {"type": "double"},
        "y": {"type": "double"},
    }
}

# price: 10,20,30,40,50   score: 1.5,2.5,3.5,4.5 (doc5 missing)
# fl: 0.5,1.25,2.0 (docs 4,5 missing)   vals: [1,2],[3],[4,5,6],[],-
# w: 1,2,3,4,- ; loc on docs 1-4 ; x: 1..5 ; y: 2,4,7,8,11
METRIC_DOCS = [
    {"cat": "a", "price": 10, "score": 1.5, "fl": 0.5, "vals": [1, 2], "ts": "2024-01-01T00:00:00Z", "w": 1,
     "loc": {"lat": 10, "lon": 20}, "name": "alpha", "body": "red apple", "x": 1, "y": 2},
    {"cat": "a", "price": 20, "score": 2.5, "fl": 1.25, "vals": [3], "ts": "2024-01-15T00:00:00Z", "w": 2,
     "loc": {"lat": 20, "lon": 40}, "name": "beta", "body": "green apple", "x": 2, "y": 4},
    {"cat": "b", "price": 30, "score": 3.5, "fl": 2.0, "vals": [4, 5, 6], "ts": "2024-02-10T00:00:00Z", "w": 3,
     "loc": {"lat": -10, "lon": -20}, "name": "gamma", "body": "yellow banana", "x": 3, "y": 7},
    {"cat": "b", "price": 40, "score": 4.5, "vals": [], "ts": "2024-03-05T00:00:00Z", "w": 4,
     "loc": {"lat": 0, "lon": 0}, "name": "delta", "body": "blue berry", "x": 4, "y": 8},
    {"cat": "c", "price": 50, "name": "epsilon", "ts": "2024-03-20T00:00:00Z", "body": "red cherry", "x": 5, "y": 11},
]

SERIES_MAPPING = {"properties": {"ts": {"type": "date"}, "v": {"type": "integer"}, "k": {"type": "keyword"}}}

# Monthly sums: Jan 30 (2 docs), Feb 0 (empty), Mar 60 (1), Apr 90 (3), May 40 (1), Jun 100 (1)
SERIES_DOCS = [
    {"ts": "2024-01-05", "v": 10, "k": "x"},
    {"ts": "2024-01-20", "v": 20, "k": "y"},
    {"ts": "2024-03-10", "v": 60, "k": "x"},
    {"ts": "2024-04-01", "v": 10, "k": "z"},
    {"ts": "2024-04-02", "v": 30, "k": "x"},
    {"ts": "2024-04-03", "v": 50, "k": "y"},
    {"ts": "2024-05-15", "v": 40, "k": "w"},
    {"ts": "2024-06-30", "v": 100, "k": "w"},
]
MONTHS = ["2024-01", "2024-02", "2024-03", "2024-04", "2024-05", "2024-06"]
MONTH_SUMS = [30.0, 0.0, 60.0, 90.0, 40.0, 100.0]
MONTH_COUNTS = [2, 0, 1, 3, 1, 1]


def _make_index(client, mapping, docs):
    name = f"slopensearch-test-aggm-{uuid.uuid4().hex[:10]}"
    r = client.put(f"/{name}", json={"settings": SETTINGS, "mappings": mapping})
    assert r.status_code in (200, 201), r.text
    lines = []
    for d in docs:
        lines.append('{"index":{}}')
        lines.append(json.dumps(d))
    r = client.post(
        f"/{name}/_bulk",
        content=("\n".join(lines) + "\n").encode(),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200 and not r.json()["errors"], r.text
    return name


@pytest.fixture(scope="module")
def midx(client):
    name = _make_index(client, METRIC_MAPPING, METRIC_DOCS)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


@pytest.fixture(scope="module")
def sidx(client):
    name = _make_index(client, SERIES_MAPPING, SERIES_DOCS)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


@pytest.fixture
def small_index(client, index_factory, bulk_index):
    def _mk(mapping, docs):
        name = index_factory(mappings=mapping, settings=SETTINGS)
        if docs:
            bulk_index(name, docs)
        return name

    return _mk


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _search(client, index, aggs, query=None):
    body = {"size": 0, "aggs": aggs}
    if query is not None:
        body["query"] = query
    return client.post(f"/{index}/_search", json=body)


def _aggs(client, index, aggs, query=None):
    r = _search(client, index, aggs, query)
    assert r.status_code == 200, r.text
    return r.json()["aggregations"]


def _one(client, index, agg, query=None):
    return _aggs(client, index, {"a": agg}, query)["a"]


def _err(client, index, aggs, status=400):
    r = _search(client, index, aggs)
    assert r.status_code == status, r.text
    return r.json()["error"]


def _root(err):
    return err["root_cause"][0]


HIST = {"date_histogram": {"field": "ts", "calendar_interval": "month", "format": "yyyy-MM"}}


def _series(client, sidx, sub, siblings=None):
    h = dict(HIST)
    h["aggs"] = sub
    aggs = {"h": h}
    if siblings:
        aggs.update(siblings)
    res = _aggs(client, sidx, aggs)
    return res["h"]["buckets"], res


def _col(buckets, name, field="value"):
    """Per-bucket value of pipeline `name`; MISSING if the bucket lacks it."""
    return [b[name][field] if name in b else MISSING for b in buckets]


MISSING = "<missing>"
NONE_MATCH = {"term": {"cat": "no-such-category"}}


# ===========================================================================
# Single-value metrics
# ===========================================================================

@pytest.mark.parametrize(
    "agg,field,expected",
    [
        ("avg", "price", 30.0),
        ("sum", "price", 150.0),
        ("min", "price", 10.0),
        ("max", "price", 50.0),
        ("avg", "score", 3.0),
        ("sum", "score", 12.0),
        ("min", "score", 1.5),
        ("max", "score", 4.5),
        ("sum", "fl", 3.75),
        ("avg", "fl", 1.25),
        ("min", "fl", 0.5),
        ("max", "fl", 2.0),
        ("avg", "vals", 3.5),
        ("sum", "vals", 21.0),
        ("min", "vals", 1.0),
        ("max", "vals", 6.0),
        ("avg", "x", 3.0),
        ("sum", "y", 32.0),
    ],
)
def test_single_value_metric(client, midx, agg, field, expected):
    res = _one(client, midx, {agg: {"field": field}})
    assert res == {"value": pytest.approx(expected)}
    assert isinstance(res["value"], float)


@pytest.mark.parametrize(
    "field,expected",
    [("price", 5), ("score", 4), ("fl", 3), ("vals", 6), ("cat", 5), ("name", 5), ("ts", 5), ("loc", 4), ("w", 4)],
)
def test_value_count_counts_every_value(client, midx, field, expected):
    res = _one(client, midx, {"value_count": {"field": field}})
    assert res == {"value": expected}
    assert isinstance(res["value"], int)


def test_sum_of_integers_serialised_as_float(client, midx):
    r = _search(client, midx, {"s": {"sum": {"field": "price"}}})
    assert '"s":{"value":150.0}' in r.text.replace(" ", "")


def test_value_count_serialised_as_integer(client, midx):
    r = _search(client, midx, {"c": {"value_count": {"field": "price"}}})
    assert '"c":{"value":5}' in r.text.replace(" ", "")


def test_metrics_per_terms_bucket(client, midx):
    res = _aggs(
        client,
        midx,
        {
            "t": {
                "terms": {"field": "cat", "order": {"_key": "asc"}},
                "aggs": {
                    "avg": {"avg": {"field": "price"}},
                    "sum": {"sum": {"field": "vals"}},
                    "max": {"max": {"field": "score"}},
                },
            }
        },
    )
    got = {b["key"]: (b["avg"]["value"], b["sum"]["value"], b["max"]["value"]) for b in res["t"]["buckets"]}
    assert got == {"a": (15.0, 6.0, 2.5), "b": (35.0, 15.0, 4.5), "c": (50.0, 0.0, None)}


def test_metrics_respect_query(client, midx):
    res = _aggs(
        client, midx, {"s": {"sum": {"field": "price"}}, "c": {"value_count": {"field": "vals"}}},
        query={"terms": {"cat": ["a", "c"]}},
    )
    assert res["s"]["value"] == 80.0
    assert res["c"]["value"] == 3


# ===========================================================================
# stats / extended_stats
# ===========================================================================

def test_stats_integer_field(client, midx):
    res = _one(client, midx, {"stats": {"field": "price"}})
    assert res == {"count": 5, "min": 10.0, "max": 50.0, "avg": 30.0, "sum": 150.0}


def test_stats_multi_valued_field(client, midx):
    res = _one(client, midx, {"stats": {"field": "vals"}})
    assert res == {"count": 6, "min": 1.0, "max": 6.0, "avg": 3.5, "sum": 21.0}


def test_stats_double_field_with_missing_docs(client, midx):
    res = _one(client, midx, {"stats": {"field": "score"}})
    assert res == {"count": 4, "min": 1.5, "max": 4.5, "avg": 3.0, "sum": 12.0}


def test_stats_key_order(client, midx):
    res = _one(client, midx, {"stats": {"field": "price"}})
    assert list(res) == ["count", "min", "max", "avg", "sum"]


def test_extended_stats_price(client, midx):
    res = _one(client, midx, {"extended_stats": {"field": "price"}})
    sd = math.sqrt(200.0)
    sds = math.sqrt(250.0)
    assert res["count"] == 5
    assert res["sum"] == 150.0
    assert res["sum_of_squares"] == 5500.0
    assert res["variance"] == pytest.approx(200.0)
    assert res["variance_population"] == pytest.approx(200.0)
    assert res["variance_sampling"] == pytest.approx(250.0)
    assert res["std_deviation"] == pytest.approx(sd)
    assert res["std_deviation_population"] == pytest.approx(sd)
    assert res["std_deviation_sampling"] == pytest.approx(sds)
    assert res["std_deviation_bounds"] == {
        "upper": pytest.approx(30 + 2 * sd),
        "lower": pytest.approx(30 - 2 * sd),
        "upper_population": pytest.approx(30 + 2 * sd),
        "lower_population": pytest.approx(30 - 2 * sd),
        "upper_sampling": pytest.approx(30 + 2 * sds),
        "lower_sampling": pytest.approx(30 - 2 * sds),
    }


def test_extended_stats_key_order(client, midx):
    res = _one(client, midx, {"extended_stats": {"field": "price"}})
    assert list(res) == [
        "count", "min", "max", "avg", "sum", "sum_of_squares", "variance", "variance_population",
        "variance_sampling", "std_deviation", "std_deviation_population", "std_deviation_sampling",
        "std_deviation_bounds",
    ]


def test_extended_stats_multi_valued(client, midx):
    res = _one(client, midx, {"extended_stats": {"field": "vals"}})
    assert res["count"] == 6
    assert res["sum_of_squares"] == 91.0
    assert res["variance"] == pytest.approx(35 / 12)
    assert res["variance_sampling"] == pytest.approx(3.5)
    assert res["std_deviation_sampling"] == pytest.approx(math.sqrt(3.5))


@pytest.mark.parametrize("sigma", [0, 1, 1.5, 3])
def test_extended_stats_sigma(client, midx, sigma):
    res = _one(client, midx, {"extended_stats": {"field": "price", "sigma": sigma}})
    b = res["std_deviation_bounds"]
    sd, sds = math.sqrt(200.0), math.sqrt(250.0)
    assert b["upper"] == pytest.approx(30 + sigma * sd)
    assert b["lower"] == pytest.approx(30 - sigma * sd)
    assert b["upper_sampling"] == pytest.approx(30 + sigma * sds)
    assert b["lower_sampling"] == pytest.approx(30 - sigma * sds)


def test_extended_stats_negative_sigma_rejected(client, midx):
    err = _err(client, midx, {"a": {"extended_stats": {"field": "price", "sigma": -1}}})
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"]["type"] == "illegal_argument_exception"
    assert err["caused_by"]["reason"] == "[sigma] must be greater than or equal to 0. Found [-1.0] in [a]"


# ===========================================================================
# Empty result shapes
# ===========================================================================

EMPTY_SHAPES = [
    ({"avg": {"field": "price"}}, {"value": None}),
    ({"sum": {"field": "price"}}, {"value": 0.0}),
    ({"min": {"field": "price"}}, {"value": None}),
    ({"max": {"field": "price"}}, {"value": None}),
    ({"value_count": {"field": "price"}}, {"value": 0}),
    ({"cardinality": {"field": "price"}}, {"value": 0}),
    ({"median_absolute_deviation": {"field": "price"}}, {"value": None}),
    ({"weighted_avg": {"value": {"field": "price"}, "weight": {"field": "w"}}}, {"value": None}),
    ({"stats": {"field": "price"}}, {"count": 0, "min": None, "max": None, "avg": None, "sum": 0.0}),
    ({"percentile_ranks": {"field": "price", "values": [10]}}, {"values": {"10.0": None}}),
    ({"geo_bounds": {"field": "loc"}}, {}),
    ({"geo_centroid": {"field": "loc"}}, {"count": 0}),
    ({"min": {"field": "ts"}}, {"value": None}),
    ({"stats": {"field": "ts"}}, {"count": 0, "min": None, "max": None, "avg": None, "sum": 0.0}),
    ({"top_hits": {}}, {"hits": {"total": {"value": 0, "relation": "eq"}, "max_score": None, "hits": []}}),
]


@pytest.mark.parametrize("agg,expected", EMPTY_SHAPES, ids=[json.dumps(a)[:40] for a, _ in EMPTY_SHAPES])
def test_empty_result_shapes(client, midx, agg, expected):
    assert _one(client, midx, agg, query=NONE_MATCH) == expected


def test_empty_percentiles_all_null(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "price"}}, query=NONE_MATCH)
    assert res == {"values": {k: None for k in ["1.0", "5.0", "25.0", "50.0", "75.0", "95.0", "99.0"]}}


def test_empty_extended_stats_shape(client, midx):
    res = _one(client, midx, {"extended_stats": {"field": "price"}}, query=NONE_MATCH)
    assert res["count"] == 0
    assert res["sum"] == 0.0
    for k in ["min", "max", "avg", "sum_of_squares", "variance", "variance_population", "variance_sampling",
              "std_deviation", "std_deviation_population", "std_deviation_sampling"]:
        assert res[k] is None, k
    assert set(res["std_deviation_bounds"].values()) == {None}
    assert len(res["std_deviation_bounds"]) == 6


def test_empty_date_min_has_no_value_as_string(client, midx):
    res = _one(client, midx, {"min": {"field": "ts"}}, query=NONE_MATCH)
    assert "value_as_string" not in res


def test_empty_matrix_stats_inside_filter(client, midx):
    res = _aggs(client, midx, {"f": {"filter": NONE_MATCH, "aggs": {"m": {"matrix_stats": {"fields": ["x"]}}}}})
    assert res["f"]["m"] == {
        "doc_count": 0,
        "fields": [{"name": "x", "count": 0, "mean": 0.0, "variance": 0.0, "skewness": 0.0, "kurtosis": 0.0,
                    "covariance": {"x": 0.0}, "correlation": {"x": 1.0}}],
    }


def test_empty_metrics_in_empty_histogram_bucket(client, sidx):
    buckets, _ = _series(client, sidx, {"a": {"avg": {"field": "v"}}, "s": {"sum": {"field": "v"}},
                                         "m": {"min": {"field": "v"}}, "c": {"value_count": {"field": "v"}}})
    feb = buckets[1]
    assert feb["doc_count"] == 0
    assert feb["a"] == {"value": None}
    assert feb["s"] == {"value": 0.0}
    assert feb["m"] == {"value": None}
    assert feb["c"] == {"value": 0}


# ===========================================================================
# Unmapped fields and `missing`
# ===========================================================================

UNMAPPED = [
    ({"avg": {"field": "nope"}}, {"value": None}),
    ({"sum": {"field": "nope"}}, {"value": 0.0}),
    ({"min": {"field": "nope"}}, {"value": None}),
    ({"max": {"field": "nope"}}, {"value": None}),
    ({"value_count": {"field": "nope"}}, {"value": 0}),
    ({"cardinality": {"field": "nope"}}, {"value": 0}),
    ({"stats": {"field": "nope"}}, {"count": 0, "min": None, "max": None, "avg": None, "sum": 0.0}),
    ({"median_absolute_deviation": {"field": "nope"}}, {"value": None}),
]


@pytest.mark.parametrize("agg,expected", UNMAPPED, ids=[next(iter(a)) for a, _ in UNMAPPED])
def test_unmapped_field(client, midx, agg, expected):
    assert _one(client, midx, agg) == expected


def test_unmapped_field_percentiles(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "nope", "percents": [50]}})
    assert res == {"values": {"50.0": None}}


@pytest.mark.parametrize(
    "agg,expected",
    [
        ({"avg": {"field": "nope", "missing": 7}}, 7.0),
        ({"sum": {"field": "nope", "missing": 7}}, 35.0),
        ({"avg": {"field": "score", "missing": 0}}, 2.4),
        ({"sum": {"field": "score", "missing": 10}}, 22.0),
        ({"min": {"field": "score", "missing": 0}}, 0.0),
        ({"max": {"field": "score", "missing": 9}}, 9.0),
        ({"max": {"field": "vals", "missing": 100}}, 100.0),
        ({"min": {"field": "fl", "missing": -1}}, -1.0),
        ({"sum": {"field": "vals", "missing": 1}}, 23.0),
    ],
)
def test_missing_parameter(client, midx, agg, expected):
    assert _one(client, midx, agg)["value"] == pytest.approx(expected)


def test_value_count_with_missing_counts_every_doc(client, midx):
    assert _one(client, midx, {"value_count": {"field": "score", "missing": 0}}) == {"value": 5}


def test_stats_with_missing(client, midx):
    res = _one(client, midx, {"stats": {"field": "score", "missing": 0}})
    assert res == {"count": 5, "min": 0.0, "max": 4.5, "avg": pytest.approx(2.4), "sum": 12.0}


def test_cardinality_with_missing_value(client, midx):
    assert _one(client, midx, {"cardinality": {"field": "score", "missing": 99}}) == {"value": 5}
    assert _one(client, midx, {"cardinality": {"field": "score", "missing": 1.5}}) == {"value": 4}


# ===========================================================================
# Dates, formats
# ===========================================================================

@pytest.mark.parametrize(
    "agg,value,as_string",
    [
        ("min", 1704067200000.0, "2024-01-01T00:00:00.000Z"),
        ("max", 1710892800000.0, "2024-03-20T00:00:00.000Z"),
        ("avg", 1707471360000.0, "2024-02-09T09:36:00.000Z"),
        ("sum", 8537356800000.0, "2240-07-16T00:00:00.000Z"),
    ],
)
def test_date_metric_value_as_string(client, midx, agg, value, as_string):
    assert _one(client, midx, {agg: {"field": "ts"}}) == {"value": value, "value_as_string": as_string}


def test_date_metric_custom_format(client, midx):
    res = _one(client, midx, {"max": {"field": "ts", "format": "yyyy-MM-dd"}})
    assert res == {"value": 1710892800000.0, "value_as_string": "2024-03-20"}


def test_date_stats_as_strings(client, midx):
    res = _one(client, midx, {"stats": {"field": "ts"}})
    assert res == {
        "count": 5,
        "min": 1704067200000.0,
        "max": 1710892800000.0,
        "avg": 1707471360000.0,
        "sum": 8537356800000.0,
        "min_as_string": "2024-01-01T00:00:00.000Z",
        "max_as_string": "2024-03-20T00:00:00.000Z",
        "avg_as_string": "2024-02-09T09:36:00.000Z",
        "sum_as_string": "2240-07-16T00:00:00.000Z",
    }


def test_date_value_count_has_no_string(client, midx):
    assert _one(client, midx, {"value_count": {"field": "ts"}}) == {"value": 5}


def test_date_percentiles_as_string(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "ts", "percents": [50]}})
    assert res == {"values": {"50.0": 1707523200000.0, "50.0_as_string": "2024-02-10T00:00:00.000Z"}}


def test_date_weighted_avg_has_no_string(client, midx):
    res = _one(client, midx, {"weighted_avg": {"value": {"field": "ts"}, "weight": {"field": "w"}}})
    assert res == {"value": 1707557760000.0}


def test_numeric_field_without_format_has_no_string(client, midx):
    assert "value_as_string" not in _one(client, midx, {"min": {"field": "price"}})


@pytest.mark.parametrize(
    "agg,fmt,expected",
    [
        ("min", "0000.0", "0010.0"),
        ("avg", "#.00", "30.00"),
        ("max", "#", "50"),
        ("sum", "0.000", "150.000"),
    ],
)
def test_numeric_format(client, midx, agg, fmt, expected):
    res = _one(client, midx, {agg: {"field": "price", "format": fmt}})
    assert res["value_as_string"] == expected


def test_stats_numeric_format(client, midx):
    res = _one(client, midx, {"stats": {"field": "price", "format": "0.0"}})
    assert res["min_as_string"] == "10.0"
    assert res["max_as_string"] == "50.0"
    assert res["avg_as_string"] == "30.0"
    assert res["sum_as_string"] == "150.0"


# ===========================================================================
# Errors on unsupported field types / parse errors
# ===========================================================================

@pytest.mark.parametrize(
    "agg",
    ["avg", "sum", "min", "max", "stats", "extended_stats", "percentiles", "median_absolute_deviation"],
)
def test_numeric_metric_on_keyword_is_error(client, midx, agg):
    err = _err(client, midx, {"a": {agg: {"field": "cat"}}})
    assert err["type"] == "search_phase_execution_exception"
    assert _root(err)["type"] == "illegal_argument_exception"
    assert _root(err)["reason"] == f"Field [cat] of type [keyword] is not supported for aggregation [{agg}]"


def test_percentile_ranks_on_keyword_is_error(client, midx):
    err = _err(client, midx, {"a": {"percentile_ranks": {"field": "cat", "values": [1]}}})
    assert _root(err)["reason"] == "Field [cat] of type [keyword] is not supported for aggregation [percentile_ranks]"


def test_avg_on_geo_point_is_error(client, midx):
    err = _err(client, midx, {"a": {"avg": {"field": "loc"}}})
    assert _root(err)["reason"] == "Field [loc] of type [geo_point] is not supported for aggregation [avg]"


def test_sum_on_text_field_requires_fielddata(client, midx):
    err = _err(client, midx, {"a": {"sum": {"field": "body"}}})
    assert _root(err)["type"] == "illegal_argument_exception"
    assert _root(err)["reason"].startswith("Text fields are not optimised for operations")
    assert "set fielddata=true on [body]" in _root(err)["reason"]


def test_weighted_avg_on_keyword_value_is_error(client, midx):
    err = _err(client, midx, {"a": {"weighted_avg": {"value": {"field": "cat"}, "weight": {"field": "w"}}}})
    assert _root(err)["reason"] == "Expected numeric type on field [cat], but got [keyword]"


def test_avg_without_field_or_script(client, midx):
    err = _err(client, midx, {"a": {"avg": {}}})
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "Required one of fields [field, script], but none were specified. "


@pytest.mark.parametrize("agg", ["avg", "sum", "max", "value_count", "cardinality"])
def test_metric_unknown_parameter(client, midx, agg):
    err = _err(client, midx, {"a": {agg: {"field": "price", "bogus": 1}}})
    assert err["type"] == "x_content_parse_exception"
    assert f"[{agg}] unknown field [bogus]" in err["reason"]


def test_unknown_aggregation_type_suggests(client, midx):
    err = _err(client, midx, {"a": {"avgx": {"field": "price"}}})
    assert err["type"] == "parsing_exception"
    assert err["reason"] == "Unknown aggregation type [avgx] did you mean [avg]?"
    assert err["caused_by"]["type"] == "named_object_not_found_exception"


@pytest.mark.parametrize(
    "agg,body",
    [
        ("top_metrics", {"metrics": {"field": "price"}, "sort": {"score": "desc"}}),
        ("string_stats", {"field": "name"}),
        ("boxplot", {"field": "price"}),
        ("t_test", {"a": {"field": "x"}, "b": {"field": "y"}, "type": "paired"}),
        ("rate", {"field": "price", "unit": "month"}),
    ],
)
def test_xpack_only_metric_aggs_absent(client, midx, agg, body):
    err = _err(client, midx, {"a": {agg: body}})
    assert err["type"] == "parsing_exception"
    assert err["reason"].startswith(f"Unknown aggregation type [{agg}]")


@pytest.mark.parametrize("agg", ["cumulative_cardinality", "normalize"])
def test_xpack_only_pipeline_aggs_absent(client, sidx, agg):
    h = dict(HIST)
    h["aggs"] = {"s": {"sum": {"field": "v"}}, "p": {agg: {"buckets_path": "s"}}}
    err = _err(client, sidx, {"h": h})
    assert err["type"] == "parsing_exception"
    assert err["reason"] == f"Unknown aggregation type [{agg}]"


# ===========================================================================
# Scripts
# ===========================================================================

def test_sum_script_with_params(client, midx):
    res = _one(client, midx, {"sum": {"script": {"source": "doc.price.value * params.f", "params": {"f": 2}}}})
    assert res == {"value": 300.0}


def test_avg_value_script(client, midx):
    assert _one(client, midx, {"avg": {"field": "price", "script": {"source": "_value + 1"}}}) == {"value": 31.0}


def test_max_script_string_shorthand(client, midx):
    assert _one(client, midx, {"max": {"script": "doc['price'].value - 5"}}) == {"value": 45.0}


def test_value_count_script(client, midx):
    assert _one(client, midx, {"value_count": {"script": "doc.price.value"}}) == {"value": 5}


def test_stats_value_script_on_multi_valued(client, midx):
    res = _one(client, midx, {"stats": {"field": "vals", "script": "_value * 10"}})
    assert res == {"count": 6, "min": 10.0, "max": 60.0, "avg": 35.0, "sum": 210.0}


def test_min_script_with_params(client, midx):
    res = _one(client, midx, {"min": {"script": {"source": "doc.price.value + params.d", "params": {"d": -100}}}})
    assert res == {"value": -90.0}


# ===========================================================================
# Percentiles / percentile_ranks / MAD
# ===========================================================================

def test_percentiles_default(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "price"}})
    assert res == {"values": {"1.0": 10.0, "5.0": 10.0, "25.0": 20.0, "50.0": 30.0, "75.0": 40.0,
                              "95.0": 50.0, "99.0": 50.0}}
    assert list(res["values"]) == ["1.0", "5.0", "25.0", "50.0", "75.0", "95.0", "99.0"]


def test_percentiles_multi_valued(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "vals"}})
    assert res == {"values": {"1.0": 1.0, "5.0": 1.0, "25.0": 2.0, "50.0": 4.0, "75.0": 5.0,
                              "95.0": 6.0, "99.0": 6.0}}


def test_percentiles_keyed_false_sorted(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "price", "percents": [0, 50, 100, 33.3], "keyed": False}})
    assert res == {"values": [
        {"key": 0.0, "value": 10.0},
        {"key": 33.3, "value": 20.0},
        {"key": 50.0, "value": 30.0},
        {"key": 100.0, "value": 50.0},
    ]}


def test_percentiles_custom_percents_dedup_and_sorted(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "price", "percents": [50, 50, 99.9, 0.1]}})
    assert res == {"values": {"0.1": 10.0, "50.0": 30.0, "99.9": 50.0}}
    assert list(res["values"]) == ["0.1", "50.0", "99.9"]


def test_percentiles_tdigest_compression(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "price", "percents": [50], "tdigest": {"compression": 200}}})
    assert res == {"values": {"50.0": 30.0}}


def test_percentiles_hdr(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "price", "hdr": {"number_of_significant_value_digits": 3}}})
    assert res == {"values": {"1.0": 10.0, "5.0": 10.0, "25.0": 20.0078125, "50.0": 30.0078125,
                              "75.0": 40.0234375, "95.0": 50.0234375, "99.0": 50.0234375}}


def test_percentiles_hdr_on_doubles(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "score", "percents": [50],
                                               "hdr": {"number_of_significant_value_digits": 3}}})
    assert res == {"values": {"50.0": 2.5009765625}}


def test_percentiles_format(client, midx):
    res = _one(client, midx, {"percentiles": {"field": "price", "percents": [50], "format": "0.00"}})
    assert res == {"values": {"50.0": 30.0, "50.0_as_string": "30.00"}}


def test_percentile_ranks(client, midx):
    res = _one(client, midx, {"percentile_ranks": {"field": "price", "values": [10, 25, 50, 5, 60]}})
    assert res == {"values": {"5.0": 0.0, "10.0": 10.0, "25.0": 40.0, "50.0": 90.0, "60.0": 100.0}}
    assert list(res["values"]) == ["5.0", "10.0", "25.0", "50.0", "60.0"]


def test_percentile_ranks_keyed_false(client, midx):
    res = _one(client, midx, {"percentile_ranks": {"field": "price", "values": [30], "keyed": False}})
    assert res == {"values": [{"key": 30.0, "value": 50.0}]}


def test_percentile_ranks_hdr(client, midx):
    res = _one(client, midx, {"percentile_ranks": {"field": "price", "values": [10, 25, 50],
                                                    "hdr": {"number_of_significant_value_digits": 3}}})
    assert res == {"values": {"10.0": 20.0, "25.0": 40.0, "50.0": 100.0}}


@pytest.mark.parametrize(
    "body,reason",
    [
        ({"percents": []}, "[percents] must not be empty: [a]"),
        ({"percents": [101]}, "percent must be in [0,100], got [101.0]: [a]"),
        ({"percents": [-1]}, "percent must be in [0,100], got [-1.0]: [a]"),
        ({"hdr": {}, "tdigest": {}}, "Only one percentiles method should be declared."),
    ],
)
def test_percentiles_invalid_params(client, midx, body, reason):
    err = _err(client, midx, {"a": {"percentiles": dict(body, field="price")}})
    assert err["type"] == "x_content_parse_exception"
    assert err["reason"] == "Failed to build [percentiles] after last required field arrived"
    assert err["caused_by"] == {"type": "illegal_argument_exception", "reason": reason}


@pytest.mark.parametrize(
    "body,reason",
    [
        ({}, "[values] must not be null: [a]"),
        ({"values": []}, "[values] must not be an empty array: [a]"),
    ],
)
def test_percentile_ranks_invalid_values(client, midx, body, reason):
    err = _err(client, midx, {"a": {"percentile_ranks": dict(body, field="price")}})
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"] == {"type": "illegal_argument_exception", "reason": reason}


def test_percentiles_negative_compression(client, midx):
    err = _err(client, midx, {"a": {"percentiles": {"field": "price", "tdigest": {"compression": -5}}}})
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"]["caused_by"]["reason"] == "[compression] must be greater than or equal to 0. Found [-5.0]"


def test_percentiles_hdr_digits_out_of_range(client, midx):
    err = _err(client, midx, {"a": {"percentiles": {"field": "price",
                                                     "hdr": {"number_of_significant_value_digits": 6}}}})
    assert err["caused_by"]["caused_by"]["reason"] == "[numberOfSignificantValueDigits] must be between 0 and 5"


def test_percentiles_method_field_unknown(client, midx):
    err = _err(client, midx, {"a": {"percentiles": {"field": "price", "method": "hdr"}}})
    assert err["type"] == "x_content_parse_exception"
    assert "[percentiles] unknown field [method]" in err["reason"]


@pytest.mark.parametrize("field,expected", [("price", 10.0), ("vals", 2.0), ("x", 1.0)])
def test_median_absolute_deviation(client, midx, field, expected):
    assert _one(client, midx, {"median_absolute_deviation": {"field": field}}) == {"value": expected}


def test_mad_constant_values_is_zero(client, small_index):
    idx = small_index({"properties": {"n": {"type": "integer"}}}, [{"n": 7}] * 4)
    assert _one(client, idx, {"median_absolute_deviation": {"field": "n"}}) == {"value": 0.0}


# ===========================================================================
# Cardinality
# ===========================================================================

@pytest.mark.parametrize(
    "field,expected", [("cat", 3), ("name", 5), ("vals", 6), ("price", 5), ("score", 4), ("loc", 4), ("ts", 5)]
)
def test_cardinality_small_sets_exact(client, midx, field, expected):
    assert _one(client, midx, {"cardinality": {"field": field}}) == {"value": expected}


def test_cardinality_precision_threshold_small(client, midx):
    assert _one(client, midx, {"cardinality": {"field": "cat", "precision_threshold": 1}}) == {"value": 3}


def test_cardinality_negative_precision(client, midx):
    err = _err(client, midx, {"a": {"cardinality": {"field": "price", "precision_threshold": -1}}})
    assert err["caused_by"]["reason"] == "[precisionThreshold] must be greater than or equal to 0. Found [-1] in [a]"


def test_cardinality_script(client, midx):
    assert _one(client, midx, {"cardinality": {"script": "doc.price.value % 20"}}) == {"value": 2}


def test_cardinality_per_bucket(client, midx):
    res = _aggs(client, midx, {"t": {"terms": {"field": "cat", "order": {"_key": "asc"}},
                                     "aggs": {"c": {"cardinality": {"field": "vals"}}}}})
    assert [b["c"]["value"] for b in res["t"]["buckets"]] == [3, 3, 0]


@pytest.fixture(scope="module")
def big_idx(client):
    docs = [{"n": i % 2000, "s": f"term-{i % 1500}"} for i in range(2500)]
    name = _make_index(client, {"properties": {"n": {"type": "long"}, "s": {"type": "keyword"}}}, docs)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def test_cardinality_exact_below_default_threshold(client, big_idx):
    res = _aggs(client, big_idx, {"n": {"cardinality": {"field": "n"}}, "s": {"cardinality": {"field": "s"}}})
    assert res == {"n": {"value": 2000}, "s": {"value": 1500}}


def test_cardinality_high_precision_threshold_exact(client, big_idx):
    assert _one(client, big_idx, {"cardinality": {"field": "s", "precision_threshold": 40000}}) == {"value": 1500}


def test_cardinality_low_precision_threshold_is_approximate(client, big_idx):
    v = _one(client, big_idx, {"cardinality": {"field": "n", "precision_threshold": 100}})["value"]
    assert v != 2000
    assert abs(v - 2000) < 100


# ===========================================================================
# weighted_avg
# ===========================================================================

def test_weighted_avg_basic(client, midx):
    # (10*1 + 20*2 + 30*3 + 40*4) / 10 ; doc5 has no weight -> skipped
    assert _one(client, midx, {"weighted_avg": {"value": {"field": "price"}, "weight": {"field": "w"}}}) == {
        "value": 30.0}


def test_weighted_avg_weight_missing(client, midx):
    res = _one(client, midx, {"weighted_avg": {"value": {"field": "price"}, "weight": {"field": "w", "missing": 10}}})
    assert res == {"value": 40.0}


def test_weighted_avg_value_missing_irrelevant_without_weight(client, midx):
    res = _one(client, midx, {"weighted_avg": {"value": {"field": "score", "missing": 100},
                                                "weight": {"field": "w"}}})
    assert res == {"value": 3.5}


def test_weighted_avg_value_missing_used(client, midx):
    # doc5: score missing->100, w missing->1 : (35 + 100) / 11
    res = _one(client, midx, {"weighted_avg": {"value": {"field": "score", "missing": 100},
                                                "weight": {"field": "w", "missing": 1}}})
    assert res["value"] == pytest.approx(135 / 11)


def test_weighted_avg_script_weight(client, midx):
    res = _one(client, midx, {"weighted_avg": {"value": {"field": "price"}, "weight": {"script": "2"}}})
    assert res == {"value": 30.0}


def test_weighted_avg_multi_valued_value(client, midx):
    # each value gets the doc weight: (1+2)*1 + 3*2 + (4+5+6)*3 = 54 ; weights 2+2+9 = 13
    res = _one(client, midx, {"weighted_avg": {"value": {"field": "vals"}, "weight": {"field": "w"}}})
    assert res["value"] == pytest.approx(54 / 13)


def test_weighted_avg_format(client, midx):
    res = _one(client, midx, {"weighted_avg": {"value": {"field": "price"}, "weight": {"field": "w"},
                                                "format": "0.000"}})
    assert res == {"value": 30.0, "value_as_string": "30.000"}


def test_weighted_avg_multi_valued_weight_is_error(client, midx):
    err = _err(client, midx, {"a": {"weighted_avg": {"value": {"field": "price"}, "weight": {"field": "vals"}}}},
               status=500)
    assert _root(err)["type"] == "aggregation_execution_exception"
    assert _root(err)["reason"].startswith("Encountered more than one weight for a single document.")


def test_weighted_avg_without_weight(client, midx):
    err = _err(client, midx, {"a": {"weighted_avg": {"value": {"field": "price"}}}})
    assert _root(err)["reason"] == "Could not find field name [weight] in multiValuesSource"


# ===========================================================================
# top_hits
# ===========================================================================

def _ids_by_name(client, midx):
    r = client.post(f"/{midx}/_search", json={"size": 10, "_source": ["name"]})
    return {h["_source"]["name"]: h["_id"] for h in r.json()["hits"]["hits"]}


def test_top_hits_default_size_is_three(client, midx):
    res = _one(client, midx, {"top_hits": {"_source": False}})
    assert res["hits"]["total"] == {"value": 5, "relation": "eq"}
    assert len(res["hits"]["hits"]) == 3
    assert res["hits"]["max_score"] == 1.0


def test_top_hits_per_bucket_sort_and_source(client, midx):
    res = _aggs(client, midx, {"t": {"terms": {"field": "cat", "order": {"_key": "asc"}}, "aggs": {
        "th": {"top_hits": {"size": 1, "sort": [{"price": "desc"}], "_source": {"includes": ["name", "price"]}}}}}})
    got = []
    for b in res["t"]["buckets"]:
        hits = b["th"]["hits"]
        assert hits["total"] == {"value": b["doc_count"], "relation": "eq"}
        assert hits["max_score"] is None
        h = hits["hits"][0]
        assert h["_score"] is None
        got.append((b["key"], h["_source"], h["sort"]))
    assert got == [
        ("a", {"price": 20, "name": "beta"}, [20]),
        ("b", {"price": 40, "name": "delta"}, [40]),
        ("c", {"price": 50, "name": "epsilon"}, [50]),
    ]


def test_top_hits_from_and_source_false(client, midx):
    res = _one(client, midx, {"top_hits": {"from": 1, "size": 2, "sort": [{"price": {"order": "asc"}}],
                                            "_source": False}})
    hits = res["hits"]["hits"]
    assert [h["sort"] for h in hits] == [[20], [30]]
    assert all("_source" not in h for h in hits)
    ids = _ids_by_name(client, midx)
    assert [h["_id"] for h in hits] == [ids["beta"], ids["gamma"]]


def test_top_hits_source_excludes_and_extras(client, midx):
    res = _one(client, midx, {"top_hits": {
        "size": 2, "sort": [{"price": "desc"}],
        "_source": {"excludes": ["body", "loc", "ts", "vals"]},
        "version": True, "seq_no_primary_term": True,
        "docvalue_fields": ["cat"],
        "script_fields": {"dbl": {"script": "doc.price.value*2"}},
    }})
    h0, h1 = res["hits"]["hits"]
    assert h0["_source"] == {"cat": "c", "price": 50, "name": "epsilon", "x": 5, "y": 11}
    assert h1["_source"] == {"cat": "b", "price": 40, "score": 4.5, "w": 4, "name": "delta", "x": 4, "y": 8}
    assert h0["fields"] == {"cat": ["c"], "dbl": [100]}
    assert h1["fields"] == {"cat": ["b"], "dbl": [80]}
    assert h0["_version"] == 1
    assert h0["_seq_no"] == 4 and h1["_seq_no"] == 3
    assert h0["_primary_term"] == 1


def test_top_hits_source_string(client, midx):
    res = _one(client, midx, {"top_hits": {"size": 1, "sort": [{"ts": "desc"}], "_source": "name"}})
    h = res["hits"]["hits"][0]
    assert h["_source"] == {"name": "epsilon"}
    assert h["sort"] == [1710892800000]


def test_top_hits_stored_fields_none_drops_id(client, midx):
    res = _one(client, midx, {"top_hits": {"size": 1, "sort": [{"price": "desc"}], "stored_fields": ["_none_"]}})
    h = res["hits"]["hits"][0]
    assert "_id" not in h
    assert "_source" not in h
    assert h["sort"] == [50]


def test_top_hits_track_scores_with_sort(client, midx):
    res = _one(client, midx, {"top_hits": {"size": 1, "sort": [{"price": "asc"}], "track_scores": True,
                                            "_source": False}})
    assert res["hits"]["max_score"] == 1.0
    assert res["hits"]["hits"][0]["_score"] == 1.0
    assert res["hits"]["hits"][0]["sort"] == [10]


def test_top_hits_highlight(client, midx):
    r = client.post(f"/{midx}/_search", json={
        "size": 0, "query": {"match": {"body": "red"}},
        "aggs": {"t": {"terms": {"field": "cat", "order": {"_key": "asc"}},
                       "aggs": {"th": {"top_hits": {"size": 1, "_source": ["name"],
                                                    "highlight": {"fields": {"body": {}}}}}}}},
    })
    assert r.status_code == 200, r.text
    got = {b["key"]: (b["th"]["hits"]["hits"][0]["_source"]["name"], b["th"]["hits"]["hits"][0]["highlight"])
           for b in r.json()["aggregations"]["t"]["buckets"]}
    assert got == {"a": ("alpha", {"body": ["<em>red</em> apple"]}),
                   "c": ("epsilon", {"body": ["<em>red</em> cherry"]})}


def test_top_hits_scores_without_sort(client, midx):
    r = client.post(f"/{midx}/_search", json={
        "size": 0, "query": {"match": {"body": "apple"}},
        "aggs": {"th": {"top_hits": {"size": 5, "_source": False}}}})
    hits = r.json()["aggregations"]["th"]["hits"]
    assert hits["total"]["value"] == 2
    assert hits["max_score"] == hits["hits"][0]["_score"]
    assert all(h["_score"] > 0 for h in hits["hits"])
    assert all("sort" not in h for h in hits["hits"])


@pytest.mark.parametrize("body,total", [({"size": 101}, 101), ({"from": 100, "size": 1}, 101)])
def test_top_hits_window_limit(client, midx, body, total):
    err = _err(client, midx, {"a": {"top_hits": body}})
    assert _root(err)["type"] == "illegal_argument_exception"
    assert _root(err)["reason"] == (
        "Top hits result window is too large, the top hits aggregator [a]'s from + size must be less than or "
        f"equal to: [100] but was [{total}]. This limit can be set by changing the "
        "[index.max_inner_result_window] index level setting."
    )


def test_top_hits_rejects_sub_aggregations(client, midx):
    err = _err(client, midx, {"a": {"top_hits": {"size": 1}, "aggs": {"x": {"avg": {"field": "price"}}}}},
               status=500)
    assert err["type"] == "aggregation_initialization_exception"
    assert err["reason"] == "Aggregator [a] of type [top_hits] cannot accept sub-aggregations"


# ===========================================================================
# scripted_metric
# ===========================================================================

SM_COUNT = {
    "init_script": "state.c = 0",
    "map_script": "state.c += 1",
    "combine_script": "return state.c",
    "reduce_script": "int t = 0; for (s in states) { t += s } return t",
}


def test_scripted_metric_count(client, midx):
    assert _one(client, midx, {"scripted_metric": SM_COUNT}) == {"value": 5}


def test_scripted_metric_sum_double(client, midx):
    res = _one(client, midx, {"scripted_metric": {
        "init_script": "state.t = []",
        "map_script": "state.t.add(doc.price.value)",
        "combine_script": "double s = 0; for (t in state.t) { s += t } return s",
        "reduce_script": "double s = 0; for (a in states) { s += a } return s",
    }})
    assert res == {"value": 150.0}


def test_scripted_metric_states_size_single_shard(client, midx):
    sm = dict(SM_COUNT, reduce_script="return states.size()")
    assert _one(client, midx, {"scripted_metric": sm}) == {"value": 1}


def test_scripted_metric_returns_map_with_params(client, midx):
    res = _one(client, midx, {"scripted_metric": {
        "init_script": "state.l = []",
        "map_script": "state.l.add(doc.price.value * params.m)",
        "combine_script": "return state.l",
        "reduce_script": "def r = []; for (s in states) { r.addAll(s) } Collections.sort(r); "
                         "return ['vals': r, 'n': r.size()]",
        "params": {"m": 3},
    }})
    assert res == {"value": {"vals": [30, 60, 90, 120, 150], "n": 5}}


def test_scripted_metric_returns_states_list(client, midx):
    res = _one(client, midx, {"scripted_metric": {
        "map_script": "state.x = 1", "combine_script": "return state", "reduce_script": "return states"}})
    assert res == {"value": [{"x": 1}]}


def test_scripted_metric_per_bucket(client, midx):
    res = _aggs(client, midx, {"t": {"terms": {"field": "cat", "order": {"_key": "asc"}}, "aggs": {"a": {
        "scripted_metric": {
            "init_script": "state.s = 0",
            "map_script": "state.s += doc.price.value",
            "combine_script": "return state.s",
            "reduce_script": "long t = 0; for (s in states) { t += s } return t",
        }}}}})
    assert [(b["key"], b["a"]["value"]) for b in res["t"]["buckets"]] == [("a", 30), ("b", 70), ("c", 50)]


def test_scripted_metric_no_matching_docs(client, midx):
    res = _aggs(client, midx, {"f": {"filter": NONE_MATCH, "aggs": {"a": {"scripted_metric": SM_COUNT}}}})
    assert res["f"]["a"] == {"value": 0}


@pytest.mark.parametrize(
    "drop,reason",
    [("reduce_script", "[reduceScript] must not be null: [a]"),
     ("combine_script", "[combineScript] must not be null: [a]")],
)
def test_scripted_metric_requires_combine_and_reduce(client, midx, drop, reason):
    sm = {k: v for k, v in SM_COUNT.items() if k != drop}
    err = _err(client, midx, {"a": {"scripted_metric": sm}})
    assert _root(err)["type"] == "illegal_argument_exception"
    assert _root(err)["reason"] == reason


def test_scripted_metric_requires_map_script(client, midx):
    sm = {k: v for k, v in SM_COUNT.items() if k != "map_script"}
    err = _err(client, midx, {"a": {"scripted_metric": sm}})
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "Required [map_script]"


# ===========================================================================
# geo_bounds / geo_centroid / matrix_stats
# ===========================================================================

def test_geo_bounds(client, midx):
    res = _one(client, midx, {"geo_bounds": {"field": "loc"}})
    b = res["bounds"]
    assert b["top_left"] == {"lat": pytest.approx(20, abs=1e-6), "lon": pytest.approx(-20, abs=1e-6)}
    assert b["bottom_right"] == {"lat": pytest.approx(-10, abs=1e-6), "lon": pytest.approx(40, abs=1e-6)}


def test_geo_centroid(client, midx):
    res = _one(client, midx, {"geo_centroid": {"field": "loc"}})
    assert res["count"] == 4
    assert res["location"] == {"lat": pytest.approx(5, abs=1e-6), "lon": pytest.approx(10, abs=1e-6)}


GEO_DOCS = [
    {"loc": {"lat": 10, "lon": 170}, "g": "a"},
    {"loc": "20,-170", "g": "a"},
    {"loc": [-175, -5], "g": "b"},
    {"loc": "POINT (175 15)", "g": "b"},
]


@pytest.fixture
def geo_idx(small_index):
    return small_index({"properties": {"loc": {"type": "geo_point"}, "g": {"type": "keyword"}}}, GEO_DOCS)


@pytest.mark.parametrize(
    "wrap,left,right", [(None, 170, -170), (True, 170, -170), (False, -175, 175)]
)
def test_geo_bounds_dateline(client, geo_idx, wrap, left, right):
    body = {"field": "loc"}
    if wrap is not None:
        body["wrap_longitude"] = wrap
    b = _one(client, geo_idx, {"geo_bounds": body})["bounds"]
    assert b["top_left"]["lat"] == pytest.approx(20, abs=1e-6)
    assert b["bottom_right"]["lat"] == pytest.approx(-5, abs=1e-6)
    assert b["top_left"]["lon"] == pytest.approx(left, abs=1e-6)
    assert b["bottom_right"]["lon"] == pytest.approx(right, abs=1e-6)


def test_geo_centroid_per_bucket(client, geo_idx):
    res = _aggs(client, geo_idx, {"g": {"terms": {"field": "g", "order": {"_key": "asc"}},
                                        "aggs": {"c": {"geo_centroid": {"field": "loc"}}}}})
    got = [(b["key"], b["c"]["count"], round(b["c"]["location"]["lat"], 5), round(b["c"]["location"]["lon"], 5))
           for b in res["g"]["buckets"]]
    assert got == [("a", 2, 15.0, 0.0), ("b", 2, 5.0, 0.0)]


def test_matrix_stats(client, midx):
    res = _one(client, midx, {"matrix_stats": {"fields": ["x", "y"]}})
    assert res["doc_count"] == 5
    fx, fy = res["fields"]
    assert fx["name"] == "x" and fy["name"] == "y"
    assert fx["count"] == 5 and fy["count"] == 5
    assert fx["mean"] == pytest.approx(3.0)
    assert fy["mean"] == pytest.approx(6.4)
    assert fx["variance"] == pytest.approx(2.5)
    assert fy["variance"] == pytest.approx(12.3)
    assert fx["skewness"] == pytest.approx(0.0, abs=1e-12)
    assert fx["kurtosis"] == pytest.approx(1.7)
    assert fx["covariance"] == {"x": pytest.approx(2.5), "y": pytest.approx(5.5)}
    assert fy["covariance"] == {"x": pytest.approx(5.5), "y": pytest.approx(12.3)}
    corr = 5.5 / math.sqrt(2.5 * 12.3)
    assert fx["correlation"] == {"x": pytest.approx(1.0), "y": pytest.approx(corr)}
    assert fy["correlation"]["x"] == pytest.approx(corr)


def test_matrix_stats_skips_docs_missing_any_field(client, midx):
    # score is missing on doc5 -> only docs 1-4 counted for both fields
    res = _one(client, midx, {"matrix_stats": {"fields": ["x", "score"]}})
    assert res["doc_count"] == 4
    by = {f["name"]: f for f in res["fields"]}
    assert by["x"]["count"] == 4
    assert by["x"]["mean"] == pytest.approx(2.5)
    assert by["score"]["mean"] == pytest.approx(3.0)
    assert by["x"]["correlation"]["score"] == pytest.approx(1.0)


# ===========================================================================
# Parent pipelines: derivative
# ===========================================================================

def test_histogram_bucket_layout(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}})
    assert [b["key_as_string"] for b in buckets] == MONTHS
    assert [b["doc_count"] for b in buckets] == MONTH_COUNTS
    assert _col(buckets, "s") == MONTH_SUMS


def test_derivative_skip_gap_policy(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "d": {"derivative": {"buckets_path": "s"}}})
    # first bucket has no derivative at all; gap (Feb) and bucket after gap are null
    assert _col(buckets, "d") == [MISSING, None, None, 30.0, -50.0, 60.0]


def test_derivative_insert_zeros(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "d": {"derivative": {"buckets_path": "s", "gap_policy": "insert_zeros"}}})
    assert _col(buckets, "d") == [MISSING, -30.0, 60.0, 30.0, -50.0, 60.0]


def test_derivative_second_order(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "d": {"derivative": {"buckets_path": "s"}},
                                        "d2": {"derivative": {"buckets_path": "d"}}})
    assert _col(buckets, "d2") == [MISSING, MISSING, None, None, -80.0, 110.0]


def test_derivative_second_order_insert_zeros(client, sidx):
    gp = {"gap_policy": "insert_zeros"}
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "d": {"derivative": dict(buckets_path="s", **gp)},
                                        "d2": {"derivative": dict(buckets_path="d", **gp)}})
    # Feb's own first derivative (-30) is ignored: the empty bucket resolves to 0 under insert_zeros
    assert _col(buckets, "d") == [MISSING, -30.0, 60.0, 30.0, -50.0, 60.0]
    assert _col(buckets, "d2") == [MISSING, MISSING, 60.0, -30.0, -80.0, 110.0]


def test_derivative_second_order_skip_over_insert_zeros(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "d": {"derivative": {"buckets_path": "s", "gap_policy": "insert_zeros"}},
                                        "d2": {"derivative": {"buckets_path": "d"}}})
    assert _col(buckets, "d2") == [MISSING, MISSING, None, -30.0, -80.0, 110.0]


def test_derivative_of_count_ignores_gap(client, sidx):
    buckets, _ = _series(client, sidx, {"d": {"derivative": {"buckets_path": "_count"}}})
    assert _col(buckets, "d") == [MISSING, -2.0, 1.0, 2.0, -2.0, 0.0]


def test_derivative_unit_day(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "d": {"derivative": {"buckets_path": "s", "unit": "day"}}})
    assert _col(buckets, "d") == [MISSING, None, None, 30.0, -50.0, 60.0]
    norm = _col(buckets, "d", "normalized_value")
    assert norm[:3] == [MISSING, None, None]
    # Mar->Apr 31 days, Apr->May 30 days, May->Jun 31 days
    assert norm[3:] == [pytest.approx(30 / 31), pytest.approx(-50 / 30), pytest.approx(60 / 31)]


def test_derivative_unit_on_numeric_histogram(client, midx):
    res = _aggs(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "s": {"sum": {"field": "price"}}, "d": {"derivative": {"buckets_path": "s", "unit": "1d"}}}}})
    b = res["h"]["buckets"]
    assert [x["key"] for x in b] == [0.0, 20.0, 40.0]
    assert "d" not in b[0]
    assert b[1]["d"] == {"value": 40.0, "normalized_value": 172800000.0}
    assert b[2]["d"] == {"value": 40.0, "normalized_value": 172800000.0}


def test_derivative_without_unit_has_no_normalized_value(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "d": {"derivative": {"buckets_path": "s"}}})
    assert "normalized_value" not in buckets[3]["d"]


def test_derivative_bad_unit(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "d": {"derivative": {"buckets_path": "_count", "unit": "bogus"}}}}})
    assert err["caused_by"]["type"] == "illegal_argument_exception"
    assert err["caused_by"]["reason"] == "failed to parse [bogus]"


def test_derivative_format(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "d": {"derivative": {"buckets_path": "s", "format": "0.0"}}})
    assert buckets[3]["d"] == {"value": 30.0, "value_as_string": "30.0"}


# ===========================================================================
# cumulative_sum
# ===========================================================================

def test_cumulative_sum(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "c": {"cumulative_sum": {"buckets_path": "s"}}})
    assert _col(buckets, "c") == [30.0, 30.0, 90.0, 180.0, 220.0, 320.0]


def test_cumulative_sum_of_count(client, sidx):
    buckets, _ = _series(client, sidx, {"c": {"cumulative_sum": {"buckets_path": "_count"}}})
    assert _col(buckets, "c") == [2.0, 2.0, 3.0, 6.0, 7.0, 8.0]


def test_cumulative_sum_of_avg_skips_null(client, sidx):
    buckets, _ = _series(client, sidx, {"a": {"avg": {"field": "v"}},
                                        "c": {"cumulative_sum": {"buckets_path": "a"}}})
    # avgs: 15, null, 60, 30, 40, 100
    assert _col(buckets, "c") == [15.0, 15.0, 75.0, 105.0, 145.0, 245.0]


def test_cumulative_sum_rejects_gap_policy(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "m": {"cumulative_sum": {"buckets_path": "_count", "gap_policy": "insert_zeros"}}}}})
    assert err["type"] == "x_content_parse_exception"
    assert "[cumulative_sum] unknown field [gap_policy]" in err["reason"]


def test_cumulative_sum_on_numeric_histogram(client, midx):
    res = _aggs(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "c": {"cumulative_sum": {"buckets_path": "_count"}}}}})
    assert [b["c"]["value"] for b in res["h"]["buckets"]] == [1.0, 3.0, 5.0]


# ===========================================================================
# moving_fn / moving_avg
# ===========================================================================

@pytest.mark.parametrize(
    "script,window,expected",
    [
        ("MovingFunctions.unweightedAvg(values)", 2, [None, MISSING, 30.0, 45.0, 75.0, 65.0]),
        ("MovingFunctions.max(values)", 3, [None, MISSING, 30.0, 60.0, 90.0, 90.0]),
        ("MovingFunctions.min(values)", 3, [None, MISSING, 30.0, 30.0, 30.0, 40.0]),
        ("MovingFunctions.sum(values)", 3, [0.0, MISSING, 30.0, 90.0, 180.0, 190.0]),
        ("MovingFunctions.linearWeightedAvg(values)", 3, [None, MISSING, 15.0, 37.5, 60.0, 360 / 7]),
        ("MovingFunctions.ewma(values, 0.5)", 3, [None, MISSING, 30.0, 45.0, 67.5, 57.5]),
        ("MovingFunctions.stdDev(values, MovingFunctions.unweightedAvg(values))", 3,
         [None, MISSING, 0.0, 15.0, math.sqrt(600), math.sqrt(422 + 2 / 9)]),
        ("values.length", 3, [0.0, MISSING, 1.0, 2.0, 3.0, 3.0]),
    ],
)
def test_moving_fn(client, sidx, script, window, expected):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "m": {"moving_fn": {"buckets_path": "s", "window": window,
                                                            "script": script}}})
    got = _col(buckets, "m")
    assert got == [e if e in (None, MISSING) else pytest.approx(e) for e in expected]


def test_moving_fn_shift_includes_current(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "m": {"moving_fn": {
        "buckets_path": "s", "window": 2, "shift": 1, "script": "MovingFunctions.sum(values)"}}})
    assert _col(buckets, "m") == [30.0, MISSING, 90.0, 150.0, 130.0, 140.0]


def test_moving_fn_insert_zeros(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "m": {"moving_fn": {
        "buckets_path": "s", "window": 2, "script": "MovingFunctions.sum(values)", "gap_policy": "insert_zeros"}}})
    assert _col(buckets, "m") == [0.0, 30.0, 30.0, 60.0, 150.0, 130.0]


def test_moving_fn_large_shift_yields_null(client, midx):
    res = _aggs(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {"m": {"moving_fn": {
        "buckets_path": "_count", "window": 2, "shift": 5, "script": "MovingFunctions.unweightedAvg(values)"}}}}})
    assert [b["m"]["value"] for b in res["h"]["buckets"]] == [None, None, None]


def test_moving_fn_script_params(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "m": {"moving_fn": {
        "buckets_path": "s", "window": 1,
        "script": {"source": "MovingFunctions.sum(values) * params.k", "params": {"k": 10}}}}})
    assert _col(buckets, "m") == [0.0, MISSING, 300.0, 600.0, 900.0, 400.0]


@pytest.mark.parametrize(
    "body,etype,reason",
    [
        ({"window": 2}, "illegal_argument_exception", "Required [script]"),
        ({"script": "1"}, "illegal_argument_exception", "Required [window]"),
    ],
)
def test_moving_fn_required_params(client, midx, body, etype, reason):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "m": {"moving_fn": dict(body, buckets_path="_count")}}}})
    assert err["type"] == etype
    assert err["reason"] == reason


def test_moving_fn_zero_window(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "m": {"moving_fn": {"buckets_path": "_count", "window": 0, "script": "1"}}}}})
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"]["caused_by"]["reason"] == "[window] must be a positive, non-zero integer."


def test_moving_avg_still_supported_simple_default(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "m": {"moving_avg": {"buckets_path": "s"}}})
    assert _col(buckets, "m") == [MISSING, MISSING, 30.0, 45.0, 60.0, 55.0]


@pytest.mark.parametrize(
    "extra,expected",
    [
        ({"model": "linear", "window": 3}, [MISSING, MISSING, 15.0, 37.5, 60.0, 360 / 7]),
        ({"model": "ewma", "window": 3, "settings": {"alpha": 0.5}}, [MISSING, MISSING, 30.0, 45.0, 67.5, 57.5]),
        ({"model": "simple", "window": 2, "gap_policy": "insert_zeros"},
         [MISSING, 30.0, 15.0, 30.0, 75.0, 65.0]),
    ],
)
def test_moving_avg_models(client, sidx, extra, expected):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "m": {"moving_avg": dict(extra, buckets_path="s")}})
    assert _col(buckets, "m") == [e if e is MISSING else pytest.approx(e) for e in expected]


# ===========================================================================
# serial_diff
# ===========================================================================

@pytest.mark.parametrize(
    "extra,expected",
    [
        ({}, [MISSING, MISSING, MISSING, 30.0, -50.0, 60.0]),
        ({"lag": 2}, [MISSING, MISSING, 30.0, MISSING, -20.0, 10.0]),
        ({"lag": 1, "gap_policy": "insert_zeros"}, [MISSING, -30.0, 60.0, 30.0, -50.0, 60.0]),
        ({"lag": 3, "gap_policy": "insert_zeros"}, [MISSING, MISSING, MISSING, 60.0, 40.0, 40.0]),
    ],
)
def test_serial_diff(client, sidx, extra, expected):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}},
                                        "sd": {"serial_diff": dict(extra, buckets_path="s")}})
    assert _col(buckets, "sd") == expected


def test_serial_diff_zero_lag(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "m": {"serial_diff": {"buckets_path": "_count", "lag": 0}}}}})
    assert err["type"] == "parsing_exception"
    assert err["reason"] == "Lag must be a positive, non-zero integer.  Value supplied was0 in [m]: [lag]."


# ===========================================================================
# bucket_script / bucket_selector / bucket_sort
# ===========================================================================

def test_bucket_script_multiple_vars(client, sidx):
    buckets, _ = _series(client, sidx, {
        "s": {"sum": {"field": "v"}},
        "bs": {"bucket_script": {"buckets_path": {"a": "s", "b": "_count"}, "script": "params.a / params.b"}},
    })
    assert _col(buckets, "bs") == [15.0, MISSING, 60.0, 30.0, 40.0, 100.0]


def test_bucket_script_params(client, sidx):
    buckets, _ = _series(client, sidx, {
        "s": {"sum": {"field": "v"}},
        "bs": {"bucket_script": {"buckets_path": {"a": "s"},
                                 "script": {"source": "params.a * params.f", "params": {"f": 2}}}},
    })
    assert _col(buckets, "bs") == [60.0, MISSING, 120.0, 180.0, 80.0, 200.0]


def test_bucket_script_insert_zeros(client, sidx):
    buckets, _ = _series(client, sidx, {
        "s": {"sum": {"field": "v"}},
        "bs": {"bucket_script": {"buckets_path": {"a": "s"}, "script": "params.a + 1", "gap_policy": "insert_zeros"}},
    })
    assert _col(buckets, "bs") == [31.0, 1.0, 61.0, 91.0, 41.0, 101.0]


def test_bucket_script_multi_value_paths(client, sidx):
    buckets, _ = _series(client, sidx, {
        "p": {"percentiles": {"field": "v", "percents": [50]}},
        "st": {"stats": {"field": "v"}},
        "bs": {"bucket_script": {"buckets_path": {"a": "p[50.0]", "b": "st.max", "c": "st.count"},
                                 "script": "params.a + params.b * 1000 + params.c * 1000000"}},
    })
    assert _col(buckets, "bs") == [2020020.0, MISSING, 1060060.0, 3050030.0, 1040040.0, 1100100.0]


def test_bucket_script_null_result_omits_value(client, midx):
    res = _aggs(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "b": {"bucket_script": {"buckets_path": {"c": "_count"}, "script": "params.c > 1 ? null : params.c"}}}}})
    b = res["h"]["buckets"]
    assert b[0]["b"] == {"value": 1.0}
    assert "b" not in b[1] and "b" not in b[2]


def test_bucket_script_format(client, midx):
    res = _aggs(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "b": {"bucket_script": {"buckets_path": {"c": "_count"}, "script": "params.c", "format": "0.00"}}}}})
    assert [x["b"] for x in res["h"]["buckets"]] == [
        {"value": 1.0, "value_as_string": "1.00"},
        {"value": 2.0, "value_as_string": "2.00"},
        {"value": 2.0, "value_as_string": "2.00"},
    ]


def test_bucket_script_in_terms(client, midx):
    res = _aggs(client, midx, {"t": {"terms": {"field": "cat", "order": {"_key": "asc"}}, "aggs": {
        "mx": {"max": {"field": "price"}}, "mn": {"min": {"field": "price"}},
        "r": {"bucket_script": {"buckets_path": {"hi": "mx", "lo": "mn"}, "script": "params.hi - params.lo"}}}}})
    assert [(b["key"], b["r"]["value"]) for b in res["t"]["buckets"]] == [("a", 10.0), ("b", 10.0), ("c", 0.0)]


def test_bucket_script_non_numeric_result(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "b": {"bucket_script": {"buckets_path": {"c": "_count"}, "script": "'str'"}}}}})
    assert err["caused_by"]["type"] == "script_exception"
    assert err["caused_by"]["caused_by"]["reason"] == "Cannot cast from [java.lang.String] to [java.lang.Number]."


def test_bucket_script_top_level_rejected(client, sidx):
    err = _err(client, sidx, {"h": HIST, "bc": {"bucket_script": {"buckets_path": {"n": "h._count"},
                                                                  "script": "params.n"}}})
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == "Validation Failed: 1: bucket_script aggregation [bc] must be declared inside of another aggregation;"


def test_bucket_selector_skip_drops_gap_bucket(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "sel": {"bucket_selector": {
        "buckets_path": {"a": "s"}, "script": "params.a < 50"}}})
    assert [b["key_as_string"] for b in buckets] == ["2024-01", "2024-05"]
    assert all("sel" not in b for b in buckets)


def test_bucket_selector_insert_zeros_keeps_gap_bucket(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "sel": {"bucket_selector": {
        "buckets_path": {"a": "s"}, "script": "params.a < 50", "gap_policy": "insert_zeros"}}})
    assert [b["key_as_string"] for b in buckets] == ["2024-01", "2024-02", "2024-05"]


def test_bucket_selector_on_count(client, sidx):
    buckets, _ = _series(client, sidx, {"sel": {"bucket_selector": {
        "buckets_path": {"c": "_count"}, "script": "params.c < 2"}}})
    assert [b["key_as_string"] for b in buckets] == ["2024-02", "2024-03", "2024-05", "2024-06"]


def test_bucket_selector_in_terms(client, midx):
    res = _aggs(client, midx, {"t": {"terms": {"field": "cat", "order": {"_key": "asc"}}, "aggs": {
        "s": {"sum": {"field": "price"}},
        "sel": {"bucket_selector": {"buckets_path": {"s": "s"}, "script": "params.s >= 50"}}}}})
    assert [(b["key"], b["s"]["value"]) for b in res["t"]["buckets"]] == [("b", 70.0), ("c", 50.0)]
    # sum_other_doc_count is not adjusted by the selector
    assert res["t"]["sum_other_doc_count"] == 0


def test_bucket_selector_bucket_count_path(client, sidx):
    res = _aggs(client, sidx, {"t": {"terms": {"field": "k", "order": {"_key": "asc"}}, "aggs": {
        "h": {"date_histogram": {"field": "ts", "calendar_interval": "month", "min_doc_count": 1}},
        "sel": {"bucket_selector": {"buckets_path": {"n": "h._bucket_count"}, "script": "params.n > 1"}}}}})
    assert [(b["key"], len(b["h"]["buckets"])) for b in res["t"]["buckets"]] == [("w", 2), ("x", 3), ("y", 2)]


def test_bucket_selector_non_boolean_script(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "b": {"bucket_selector": {"buckets_path": {"c": "_count"}, "script": "1"}}}}})
    assert err["caused_by"]["type"] == "script_exception"
    assert err["caused_by"]["caused_by"]["reason"] == "Cannot cast from [int] to [boolean]."


def test_bucket_sort_by_metric_desc_with_size(client, sidx):
    buckets, _ = _series(client, sidx, {"s": {"sum": {"field": "v"}}, "so": {"bucket_sort": {
        "sort": [{"s": {"order": "desc"}}], "size": 3}}})
    assert [b["key_as_string"] for b in buckets] == ["2024-06", "2024-04", "2024-03"]
    assert all("so" not in b for b in buckets)


def test_bucket_sort_truncate_only(client, sidx):
    buckets, _ = _series(client, sidx, {"so": {"bucket_sort": {"from": 2, "size": 2}}})
    assert [b["key_as_string"] for b in buckets] == ["2024-03", "2024-04"]


def test_bucket_sort_from_only(client, sidx):
    buckets, _ = _series(client, sidx, {"so": {"bucket_sort": {"from": 4}}})
    assert [b["key_as_string"] for b in buckets] == ["2024-05", "2024-06"]


def test_bucket_sort_count_then_key(client, sidx):
    buckets, _ = _series(client, sidx, {"so": {"bucket_sort": {"sort": [{"_count": "desc"}, {"_key": "desc"}]}}})
    assert [b["key_as_string"] for b in buckets] == ["2024-04", "2024-01", "2024-06", "2024-05", "2024-03", "2024-02"]


def test_bucket_sort_skip_gap_puts_missing_last(client, sidx):
    buckets, _ = _series(client, sidx, {"a": {"avg": {"field": "v"}}, "so": {"bucket_sort": {
        "sort": [{"a": "asc"}]}}})
    # avgs: Jan 15, Feb null, Mar 60, Apr 30, May 40, Jun 100
    assert [b["key_as_string"] for b in buckets][:5] == ["2024-01", "2024-04", "2024-05", "2024-03", "2024-06"]


def test_bucket_sort_in_terms(client, midx):
    res = _aggs(client, midx, {"t": {"terms": {"field": "cat"}, "aggs": {
        "s": {"sum": {"field": "price"}},
        "so": {"bucket_sort": {"sort": [{"s": "desc"}], "size": 2}}}}})
    assert [b["key"] for b in res["t"]["buckets"]] == ["b", "c"]


def test_bucket_sort_multi_value_without_key(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "st": {"stats": {"field": "price"}}, "b": {"bucket_sort": {"sort": ["st"]}}}}}, status=500)
    assert err["caused_by"]["type"] == "aggregation_execution_exception"
    assert err["caused_by"]["reason"] == (
        "buckets_path must reference either a number value or a single value numeric metric aggregation, "
        "but [st] contains multiple values. Please specify which to use.")


def test_bucket_sort_unknown_path(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "b": {"bucket_sort": {"sort": [{"nope": "asc"}]}}}}})
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == "Validation Failed: 1: No aggregation found for path [nope];"


def test_bucket_sort_noop_config(client, midx):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "b": {"bucket_sort": {}}}}})
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == ("Validation Failed: 1: [b] is configured to perform nothing. "
                             "Please set either of [sort, size, from] to use bucket_sort;")


@pytest.mark.parametrize(
    "body,reason",
    [({"size": -1}, "[size] must be a positive integer: [-1]"),
     ({"from": -1}, "[from] must be a non-negative integer: [-1]")],
)
def test_bucket_sort_negative_params(client, midx, body, reason):
    err = _err(client, midx, {"h": {"histogram": {"field": "price", "interval": 20}, "aggs": {
        "b": {"bucket_sort": body}}}})
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"]["caused_by"]["reason"] == reason


# ===========================================================================
# Sibling pipelines
# ===========================================================================

SIBLING_BASIC = [
    ("avg_bucket", {"value": 64.0}),
    ("sum_bucket", {"value": 320.0}),
    ("min_bucket", {"value": 30.0, "keys": ["2024-01"]}),
    ("max_bucket", {"value": 100.0, "keys": ["2024-06"]}),
    ("stats_bucket", {"count": 5, "min": 30.0, "max": 100.0, "avg": 64.0, "sum": 320.0}),
]


@pytest.mark.parametrize("agg,expected", SIBLING_BASIC, ids=[a for a, _ in SIBLING_BASIC])
def test_sibling_pipeline_skip_gap(client, sidx, agg, expected):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}}, {"p": {agg: {"buckets_path": "h>s"}}})
    assert res["p"] == expected


SIBLING_ZEROS = [
    ("avg_bucket", {"value": pytest.approx(320 / 6)}),
    ("sum_bucket", {"value": 320.0}),
    ("min_bucket", {"value": 0.0, "keys": ["2024-02"]}),
    ("max_bucket", {"value": 100.0, "keys": ["2024-06"]}),
    ("stats_bucket", {"count": 6, "min": 0.0, "max": 100.0, "avg": pytest.approx(320 / 6), "sum": 320.0}),
]


@pytest.mark.parametrize("agg,expected", SIBLING_ZEROS, ids=[a for a, _ in SIBLING_ZEROS])
def test_sibling_pipeline_insert_zeros(client, sidx, agg, expected):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}},
                     {"p": {agg: {"buckets_path": "h>s", "gap_policy": "insert_zeros"}}})
    assert res["p"] == expected


def test_sibling_avg_of_avg_skips_null_buckets(client, sidx):
    _, res = _series(client, sidx, {"a": {"avg": {"field": "v"}}}, {"p": {"avg_bucket": {"buckets_path": "h>a"}}})
    assert res["p"]["value"] == pytest.approx((15 + 60 + 30 + 40 + 100) / 5)


def test_extended_stats_bucket(client, sidx):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}},
                     {"p": {"extended_stats_bucket": {"buckets_path": "h>s"}}})
    p = res["p"]
    sd = math.sqrt(744.0)
    sds = math.sqrt(930.0)
    assert p["count"] == 5
    assert p["sum_of_squares"] == 24200.0
    assert p["variance"] == pytest.approx(744.0)
    assert p["variance_sampling"] == pytest.approx(930.0)
    assert p["std_deviation"] == pytest.approx(sd)
    assert p["std_deviation_sampling"] == pytest.approx(sds)
    assert p["std_deviation_bounds"]["upper"] == pytest.approx(64 + 2 * sd)
    assert p["std_deviation_bounds"]["lower_sampling"] == pytest.approx(64 - 2 * sds)


def test_extended_stats_bucket_sigma(client, sidx):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}},
                     {"p": {"extended_stats_bucket": {"buckets_path": "h>s", "sigma": 1}}})
    b = res["p"]["std_deviation_bounds"]
    assert b["upper"] == pytest.approx(64 + math.sqrt(744))
    assert b["lower"] == pytest.approx(64 - math.sqrt(744))
    assert b["upper_sampling"] == pytest.approx(64 + math.sqrt(930))


def test_percentiles_bucket_default(client, sidx):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}},
                     {"p": {"percentiles_bucket": {"buckets_path": "h>s"}}})
    assert res["p"] == {"values": {"1.0": 30.0, "5.0": 30.0, "25.0": 40.0, "50.0": 60.0, "75.0": 90.0,
                                   "95.0": 100.0, "99.0": 100.0}}


def test_percentiles_bucket_custom_percents(client, sidx):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}},
                     {"p": {"percentiles_bucket": {"buckets_path": "h>s", "percents": [25, 75]}}})
    assert res["p"] == {"values": {"25.0": 40.0, "75.0": 90.0}}


def test_percentiles_bucket_keyed_false_keeps_request_order(client, sidx):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}},
                     {"p": {"percentiles_bucket": {"buckets_path": "h>s", "percents": [0, 50, 100, 10],
                                                   "keyed": False}}})
    assert res["p"] == {"values": [
        {"key": 0.0, "value": 30.0},
        {"key": 50.0, "value": 60.0},
        {"key": 100.0, "value": 100.0},
        {"key": 10.0, "value": 30.0},
    ]}


def test_sibling_format(client, sidx):
    _, res = _series(client, sidx, {"s": {"sum": {"field": "v"}}},
                     {"p": {"avg_bucket": {"buckets_path": "h>s", "format": "0.0"}}})
    assert res["p"] == {"value": 64.0, "value_as_string": "64.0"}


@pytest.mark.parametrize("path", ["h._count", "h>_count"])
def test_sibling_on_count_includes_empty_buckets(client, sidx, path):
    _, res = _series(client, sidx, {}, {"p": {"avg_bucket": {"buckets_path": path}},
                                        "mn": {"min_bucket": {"buckets_path": path}}})
    assert res["p"]["value"] == pytest.approx(8 / 6)
    assert res["mn"] == {"value": 0.0, "keys": ["2024-02"]}


def test_max_bucket_on_count(client, sidx):
    _, res = _series(client, sidx, {}, {"p": {"max_bucket": {"buckets_path": "h>_count"}}})
    assert res["p"] == {"value": 3.0, "keys": ["2024-04"]}


def test_sibling_multi_value_metric_key(client, sidx):
    _, res = _series(client, sidx, {"st": {"stats": {"field": "v"}}, "p": {"percentiles": {
        "field": "v", "percents": [50]}}}, {
        "mb": {"max_bucket": {"buckets_path": "h>st.avg"}},
        "pb": {"avg_bucket": {"buckets_path": "h>p[50.0]"}},
        "pb2": {"avg_bucket": {"buckets_path": "h>p.50"}},
        "sc": {"sum_bucket": {"buckets_path": "h>st.count"}},
    })
    assert res["mb"] == {"value": 100.0, "keys": ["2024-06"]}
    # tdigest median of [10, 20] is 20
    assert res["pb"] == {"value": 50.0}
    assert res["pb2"] == {"value": 50.0}
    assert res["sc"] == {"value": 8.0}


def test_sibling_on_terms_only_returned_buckets(client, sidx):
    res = _aggs(client, sidx, {"t": {"terms": {"field": "k", "size": 2}, "aggs": {"s": {"sum": {"field": "v"}}}},
                               "sm": {"sum_bucket": {"buckets_path": "t>s"}}})
    assert [b["key"] for b in res["t"]["buckets"]] == ["x", "w"]
    assert res["t"]["sum_other_doc_count"] == 3
    assert res["sm"] == {"value": 240.0}


def test_max_bucket_terms_keys(client, sidx):
    res = _aggs(client, sidx, {"t": {"terms": {"field": "k"}, "aggs": {"s": {"sum": {"field": "v"}}}},
                               "mx": {"max_bucket": {"buckets_path": "t>s"}},
                               "mn": {"min_bucket": {"buckets_path": "t>s"}}})
    assert res["mx"] == {"value": 140.0, "keys": ["w"]}
    assert res["mn"] == {"value": 10.0, "keys": ["z"]}


@pytest.mark.parametrize("order,keys", [("asc", ["w", "y"]), ("desc", ["y", "w"])])
def test_max_bucket_ties_keys_follow_bucket_order(client, sidx, order, keys):
    res = _aggs(client, sidx, {"t": {"terms": {"field": "k", "order": {"_key": order}}},
                               "mx": {"max_bucket": {"buckets_path": "t>_count"}}},
                query={"bool": {"must_not": {"term": {"k": "x"}}}})
    assert res["mx"] == {"value": 2.0, "keys": keys}


def test_min_bucket_ties(client, sidx):
    res = _aggs(client, sidx, {"t": {"terms": {"field": "k"}},
                               "mn": {"min_bucket": {"buckets_path": "t>_count"}}},
                query={"terms": {"k": ["w", "y"]}})
    assert res["mn"] == {"value": 2.0, "keys": ["w", "y"]}


def test_nested_buckets_path_through_filter(client, sidx):
    res = _aggs(client, sidx, {
        "t": {"terms": {"field": "k", "order": {"_key": "asc"}}, "aggs": {
            "f": {"filter": {"range": {"v": {"gte": 30}}}, "aggs": {"s": {"sum": {"field": "v"}}}}}},
        "sb": {"sum_bucket": {"buckets_path": "t>f>s"}},
        "mb": {"max_bucket": {"buckets_path": "t>f._count"}},
    })
    assert res["sb"] == {"value": 280.0}
    assert res["mb"] == {"value": 2.0, "keys": ["w", "x"]}


def test_sibling_inside_terms_bucket(client, sidx):
    res = _aggs(client, sidx, {
        "t": {"terms": {"field": "k", "order": {"_key": "asc"}}, "aggs": {
            "h": {"date_histogram": {"field": "ts", "calendar_interval": "month", "format": "yyyy-MM"},
                  "aggs": {"s": {"sum": {"field": "v"}}}},
            "mb": {"max_bucket": {"buckets_path": "h>s"}}}},
        "tot": {"sum_bucket": {"buckets_path": "t>mb"}},
    })
    got = [(b["key"], b["mb"]["value"], b["mb"]["keys"]) for b in res["t"]["buckets"]]
    assert got == [("w", 100.0, ["2024-06"]), ("x", 60.0, ["2024-03"]), ("y", 50.0, ["2024-04"]),
                   ("z", 10.0, ["2024-04"])]
    assert res["tot"] == {"value": 220.0}


def test_sibling_pipelines_on_empty_histogram(client, sidx):
    res = _aggs(client, sidx, {"f": {"filter": {"range": {"v": {"gte": 1000}}}, "aggs": {
        "h": {"date_histogram": {"field": "ts", "calendar_interval": "month"},
              "aggs": {"s": {"sum": {"field": "v"}}}},
        "a": {"avg_bucket": {"buckets_path": "h>s"}},
        "m": {"max_bucket": {"buckets_path": "h>s"}},
        "st": {"stats_bucket": {"buckets_path": "h>s"}},
        "sm": {"sum_bucket": {"buckets_path": "h>s"}},
        "pb": {"percentiles_bucket": {"buckets_path": "h>s", "percents": [50]}},
        "es": {"extended_stats_bucket": {"buckets_path": "h>s"}},
    }}})["f"]
    assert res["doc_count"] == 0
    assert res["h"] == {"buckets": []}
    assert res["a"] == {"value": None}
    assert res["m"] == {"value": None, "keys": []}
    assert res["st"] == {"count": 0, "min": None, "max": None, "avg": None, "sum": 0.0}
    assert res["sm"] == {"value": 0.0}
    assert res["pb"] == {"values": {"50.0": None}}
    assert res["es"]["count"] == 0 and res["es"]["variance"] is None


def test_sibling_on_numeric_histogram_keys(client, midx):
    res = _aggs(client, midx, {"h": {"histogram": {"field": "price", "interval": 20},
                                     "aggs": {"s": {"sum": {"field": "price"}}}},
                               "mx": {"max_bucket": {"buckets_path": "h>s"}}})
    assert res["mx"] == {"value": 90.0, "keys": ["40.0"]}


# ===========================================================================
# buckets_path errors
# ===========================================================================

def _hist_with(extra):
    h = {"date_histogram": {"field": "ts", "calendar_interval": "month"},
         "aggs": {"s": {"sum": {"field": "v"}}, "st": {"stats": {"field": "v"}}}}
    return dict({"h": h}, **extra)


@pytest.mark.parametrize(
    "path,reason",
    [
        ("h>nope", "Validation Failed: 1: No aggregation [nope] found for path [h>nope];"),
        ("nope>s", "Validation Failed: 1: No aggregation found for path [nope>s];"),
    ],
)
def test_sibling_unknown_path(client, sidx, path, reason):
    err = _err(client, sidx, _hist_with({"x": {"avg_bucket": {"buckets_path": path}}}))
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == reason


def test_sibling_path_to_multi_value_metric(client, sidx):
    err = _err(client, sidx, _hist_with({"x": {"avg_bucket": {"buckets_path": "h>st"}}}), status=500)
    assert err["type"] == "search_phase_execution_exception"
    assert err["caused_by"]["type"] == "aggregation_execution_exception"
    assert "but [st] contains multiple values. Please specify which to use." in err["caused_by"]["reason"]


def test_sibling_path_to_unknown_stats_key(client, sidx):
    err = _err(client, sidx, _hist_with({"x": {"avg_bucket": {"buckets_path": "h>st.nope"}}}))
    assert err["caused_by"]["type"] == "illegal_argument_exception"
    assert err["caused_by"]["reason"].startswith("No enum constant")
    assert err["caused_by"]["reason"].endswith("InternalStats.Metrics.nope")


def test_sibling_path_to_multi_bucket_agg(client, sidx):
    err = _err(client, sidx, _hist_with({"x": {"avg_bucket": {"buckets_path": "h"}}}), status=500)
    assert err["caused_by"]["type"] == "aggregation_execution_exception"
    assert err["caused_by"]["reason"] == (
        "buckets_path must reference either a number value or a single value numeric metric aggregation, "
        "got: [Bucket] at aggregation [h]")


def test_sibling_missing_buckets_path(client, sidx):
    err = _err(client, sidx, _hist_with({"x": {"avg_bucket": {}}}))
    assert err["type"] == "parsing_exception"
    assert err["reason"] == "Missing required field [buckets_path] for aggregation [x]"


def test_invalid_gap_policy(client, sidx):
    err = _err(client, sidx, _hist_with({"x": {"avg_bucket": {"buckets_path": "h>s", "gap_policy": "bogus"}}}))
    assert err["type"] == "parsing_exception"
    assert err["reason"] == "Invalid gap policy: [bogus], accepted values: [insert_zeros, skip]"


def test_parent_pipeline_at_top_level(client, sidx):
    err = _err(client, sidx, {"d": {"derivative": {"buckets_path": "s"}}})
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == "Validation Failed: 1: No aggregation found for path [s];"


@pytest.mark.parametrize(
    "name,body",
    [
        ("derivative", {"buckets_path": "s"}),
        ("cumulative_sum", {"buckets_path": "s"}),
        ("serial_diff", {"buckets_path": "s"}),
        ("moving_fn", {"buckets_path": "s", "window": 2, "script": "1"}),
    ],
)
def test_parent_pipeline_requires_histogram_parent(client, sidx, name, body):
    err = _err(client, sidx, {"t": {"terms": {"field": "k"}, "aggs": {
        "s": {"sum": {"field": "v"}}, "d": {name: body}}}})
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == (f"Validation Failed: 1: {name} aggregation [d] must have a histogram, "
                             "date_histogram or auto_date_histogram as parent;")


def test_parent_pipeline_unknown_path_inside_histogram(client, sidx):
    h = dict(HIST)
    h["aggs"] = {"d": {"derivative": {"buckets_path": "nope"}}}
    err = _err(client, sidx, {"h": h})
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == "Validation Failed: 1: No aggregation found for path [nope];"
