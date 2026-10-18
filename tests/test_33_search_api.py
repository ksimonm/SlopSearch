"""Search request / response features of OpenSearch 3.x (not query types).

Every assertion encodes behaviour observed on a real OpenSearch 3.9.0 node.
"""

import json
import uuid

import pytest

pytestmark = pytest.mark.search_api


# ---------------------------------------------------------------------------
# Shared corpus
# ---------------------------------------------------------------------------

MAPPING = {
    "properties": {
        "title": {"type": "text", "fields": {"raw": {"type": "keyword"}}},
        "body": {"type": "text", "term_vector": "with_positions_offsets"},
        "tag": {"type": "keyword"},
        "n": {"type": "integer"},
        "nums": {"type": "integer"},
        "price": {"type": "float"},
        "date": {"type": "date"},
        "loc": {"type": "geo_point"},
        "items": {
            "type": "nested",
            "properties": {"name": {"type": "keyword"}, "qty": {"type": "integer"}},
        },
        "obj": {"properties": {"a": {"type": "keyword"}, "b": {"type": "integer"}}},
    }
}

DOCS = {
    "1": {
        "title": "quick brown fox",
        "body": "the quick brown fox jumps over the lazy dog",
        "tag": "a",
        "n": 1,
        "nums": [3, 10],
        "price": 1.5,
        "date": "2024-01-01T00:00:00Z",
        "loc": {"lat": 0, "lon": 0},
        "items": [{"name": "x", "qty": 5}, {"name": "y", "qty": 1}],
        "obj": {"a": "p", "b": 1},
    },
    "2": {
        "title": "lazy brown dog",
        "body": "a lazy dog sleeps all day long",
        "tag": "b",
        "n": 2,
        "nums": [1, 20],
        "price": 2.5,
        "date": "2024-02-01T00:00:00Z",
        "loc": {"lat": 0, "lon": 1},
        "items": [{"name": "x", "qty": 2}],
        "obj": {"a": "q", "b": 2},
    },
    "3": {
        "title": "quick red fox",
        "body": "red foxes are quick and clever animals",
        "tag": "a",
        "n": 3,
        "nums": [5],
        "price": 3.5,
        "date": "2024-03-01T00:00:00Z",
        "loc": {"lat": 0, "lon": 2},
        "items": [{"name": "y", "qty": 9}],
        "obj": {"a": "p", "b": 3},
    },
    "4": {
        "title": "slow green turtle",
        "body": "turtles are slow but steady",
        "tag": "c",
        "n": 4,
        "price": 4.5,
        "date": "2024-04-01T00:00:00Z",
        "loc": {"lat": 0, "lon": 3},
    },
    "5": {"title": "Zebra", "tag": "B", "n": 5, "nums": [], "date": "2024-05-01T00:00:00Z"},
}

SINGLE = {"number_of_shards": 1, "number_of_replicas": 0}
ND = {"Content-Type": "application/x-ndjson"}

MAX_WINDOW_MSG = (
    "Result window is too large, from + size must be less than or equal to: [{lim}] "
    "but was [{got}]. See the scroll api for a more efficient way to request large data "
    "sets. This limit can be set by changing the [index.max_result_window] index level setting."
)


def _uid(prefix="sa"):
    return f"{prefix}-{uuid.uuid4().hex[:10]}"


def _bulk_ids(client, index, docs, routing=None):
    lines = []
    for _id, doc in docs.items():
        meta = {"_id": _id}
        if routing:
            meta["routing"] = routing
        lines.append(json.dumps({"index": meta}))
        lines.append(json.dumps(doc, ensure_ascii=False))
    r = client.post(
        f"/{index}/_bulk",
        content=("\n".join(lines) + "\n").encode(),
        headers=ND,
        params={"refresh": "true"},
    )
    assert r.status_code == 200, r.text
    assert r.json()["errors"] is False, r.text


def _make_index(client, name, mapping, docs, settings=None):
    r = client.put(f"/{name}", json={"settings": settings or SINGLE, "mappings": mapping})
    assert r.status_code == 200, r.text
    if docs:
        _bulk_ids(client, name, docs)
    return name


@pytest.fixture(scope="module")
def corpus(client):
    name = _uid("sa-corpus")
    _make_index(client, name, MAPPING, DOCS)
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


@pytest.fixture(scope="module")
def corpus_pair(client, corpus):
    """Second index sharing the prefix of the first, plus two aliases."""
    other = corpus + "-b"
    _make_index(client, other, MAPPING, {"10": {"title": "fox in other index", "tag": "z", "n": 100}})
    alias = corpus + "-al"
    falias = corpus + "-fal"
    r = client.post(
        "/_aliases",
        json={
            "actions": [
                {"add": {"index": corpus, "alias": alias}},
                {"add": {"index": other, "alias": alias}},
                {"add": {"index": corpus, "alias": falias, "filter": {"term": {"tag": "a"}}}},
            ]
        },
    )
    assert r.status_code == 200, r.text
    yield {"a": corpus, "b": other, "alias": alias, "falias": falias}
    client.post(
        "/_aliases",
        json={
            "actions": [
                {"remove": {"index": corpus, "alias": alias}},
                {"remove": {"index": other, "alias": alias}},
                {"remove": {"index": corpus, "alias": falias}},
            ]
        },
    )
    client.delete(f"/{other}", params={"ignore_unavailable": "true"})


@pytest.fixture(scope="module")
def big(client):
    name = _uid("sa-big")
    _make_index(
        client,
        name,
        {"properties": {"v": {"type": "integer"}}},
        {str(i): {"v": i % 7} for i in range(10050)},
    )
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


@pytest.fixture
def scrolls(client):
    """Collects scroll ids; clears them at teardown."""
    ids = []
    yield ids
    if ids:
        client.request("DELETE", "/_search/scroll", json={"scroll_id": list(ids)})


@pytest.fixture
def pits(client):
    ids = []
    yield ids
    if ids:
        client.request("DELETE", "/_search/point_in_time", json={"pit_id": list(ids)})


@pytest.fixture
def stored_scripts(client):
    ids = []
    yield ids
    for sid in ids:
        client.delete(f"/_scripts/{sid}")


def _search(client, index, body=None, method="POST", **params):
    kw = {}
    if body is not None:
        kw["json"] = body
    return client.request(method, f"/{index}/_search", params=params or None, **kw)


def _ok(r):
    assert r.status_code == 200, r.text
    return r.json()


def _s(client, index, body=None, **params):
    return _ok(_search(client, index, body, **params))


def _ids(res):
    return [h["_id"] for h in res["hits"]["hits"]]


def _sorts(res):
    return [h.get("sort") for h in res["hits"]["hits"]]


def _id_sorts(res):
    return [(h["_id"], h.get("sort")) for h in res["hits"]["hits"]]


def _err(r, status, etype=None, root_type=None):
    assert r.status_code == status, r.text
    body = r.json()
    assert body["status"] == status
    if etype is not None:
        assert body["error"]["type"] == etype, r.text
    if root_type is not None:
        assert body["error"]["root_cause"][0]["type"] == root_type, r.text
    return body["error"]


def _root_reason(err):
    return err["root_cause"][0]["reason"]


def _msearch(client, lines, path="/_msearch", **params):
    payload = "\n".join(json.dumps(x) for x in lines) + "\n"
    return client.post(path, content=payload.encode(), headers=ND, params=params or None)


# ---------------------------------------------------------------------------
# Response shape
# ---------------------------------------------------------------------------


def test_response_top_level_shape(client, corpus):
    res = _s(client, corpus, method="GET")
    assert set(res) == {"took", "timed_out", "_shards", "hits"}
    assert isinstance(res["took"], int) and res["took"] >= 0
    assert res["timed_out"] is False
    assert res["_shards"] == {"total": 1, "successful": 1, "skipped": 0, "failed": 0}
    assert res["hits"]["total"] == {"value": 5, "relation": "eq"}
    assert res["hits"]["max_score"] == 1.0
    assert set(res["hits"]) == {"total", "max_score", "hits"}


def test_hit_shape_exact_keys(client, corpus):
    res = _s(client, corpus, {"query": {"ids": {"values": ["1"]}}})
    hit = res["hits"]["hits"][0]
    assert set(hit) == {"_index", "_id", "_score", "_source"}
    assert hit["_index"] == corpus
    assert hit["_id"] == "1"
    assert hit["_source"] == DOCS["1"]


def test_source_preserves_original_json_exactly(client, corpus):
    res = _s(client, corpus, {"query": {"ids": {"values": ["5"]}}})
    assert res["hits"]["hits"][0]["_source"] == {
        "title": "Zebra", "tag": "B", "n": 5, "nums": [], "date": "2024-05-01T00:00:00Z"
    }


def test_default_order_match_all_is_index_order(client, corpus):
    assert _ids(_s(client, corpus)) == ["1", "2", "3", "4", "5"]


def test_max_score_null_and_score_null_when_sorting(client, corpus):
    res = _s(client, corpus, {"sort": [{"n": "desc"}], "size": 2})
    assert res["hits"]["max_score"] is None
    assert [h["_score"] for h in res["hits"]["hits"]] == [None, None]
    assert _id_sorts(res) == [("5", [5]), ("4", [4])]


def test_max_score_null_when_size_zero(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}, "size": 0})
    assert res["hits"] == {"total": {"value": 2, "relation": "eq"}, "max_score": None, "hits": []}


def test_max_score_null_when_no_hits(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "nothingzz"}}})
    assert res["hits"] == {"total": {"value": 0, "relation": "eq"}, "max_score": None, "hits": []}


def test_scores_descending_and_max_score_is_first(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "brown fox"}}})
    scores = [h["_score"] for h in res["hits"]["hits"]]
    assert scores == sorted(scores, reverse=True)
    assert res["hits"]["max_score"] == scores[0]
    assert _ids(res)[0] == "1"


def test_get_with_body_equals_post(client, corpus):
    body = {"query": {"term": {"tag": "a"}}}
    g = _ok(client.request("GET", f"/{corpus}/_search", json=body))
    p = _s(client, corpus, body)
    assert _ids(g) == _ids(p) == ["1", "3"]


def test_post_with_empty_body_is_match_all(client, corpus):
    r = client.post(f"/{corpus}/_search", content=b"")
    assert _ok(r)["hits"]["total"]["value"] == 5


def test_empty_object_body_is_match_all(client, corpus):
    assert _s(client, corpus, {})["hits"]["total"] == {"value": 5, "relation": "eq"}


def test_default_size_is_ten(client, index_factory, bulk_index):
    name = index_factory(settings=SINGLE, mappings={"properties": {"i": {"type": "integer"}}})
    bulk_index(name, [{"i": i} for i in range(12)])
    res = _s(client, name)
    assert len(res["hits"]["hits"]) == 10
    assert res["hits"]["total"] == {"value": 12, "relation": "eq"}


def test_invalid_json_body(client, corpus):
    r = client.post(
        f"/{corpus}/_search", content=b"{bad json", headers={"Content-Type": "application/json"}
    )
    _err(r, 400, "json_parse_exception")


def test_array_body_rejected(client, corpus):
    r = client.post(f"/{corpus}/_search", content=b"[]", headers={"Content-Type": "application/json"})
    err = _err(r, 400, "parsing_exception")
    assert err["reason"] == "Expected [START_OBJECT] but found [START_ARRAY]"


def test_unknown_body_key_rejected(client, corpus):
    err = _err(_search(client, corpus, {"query": {"match_all": {}}, "bogus_key": 1}), 400, "parsing_exception")
    assert err["reason"] == "Unknown key for a VALUE_NUMBER in [bogus_key]."


def test_empty_query_clause_rejected(client, corpus):
    err = _err(_search(client, corpus, {"query": {}}), 400, "illegal_argument_exception")
    assert err["reason"].startswith("query malformed, empty clause found")


def test_null_query_rejected(client, corpus):
    err = _err(_search(client, corpus, {"query": None}), 400, "parsing_exception")
    assert err["reason"] == "Unknown key for a VALUE_NULL in [query]."


def test_unknown_url_parameter_rejected(client, corpus):
    err = _err(_search(client, corpus, bogus_param="1"), 400, "illegal_argument_exception")
    assert err["reason"] == f"request [/{corpus}/_search] contains unrecognized parameter: [bogus_param]"


@pytest.mark.parametrize("param", ["profile", "min_score"])
def test_body_only_options_not_accepted_as_url_params(client, corpus, param):
    err = _err(_search(client, corpus, **{param: "true" if param == "profile" else "1"}), 400,
               "illegal_argument_exception")
    assert f"unrecognized parameter: [{param}]" in err["reason"]


# ---------------------------------------------------------------------------
# URI search
# ---------------------------------------------------------------------------


def test_uri_q_simple(client, corpus):
    assert sorted(_ids(_s(client, corpus, method="GET", q="fox"))) == ["1", "3"]


@pytest.mark.parametrize(
    "q,op,expected",
    [
        ("brown fox", "AND", ["1"]),
        ("brown fox", "and", ["1"]),
        ("brown fox", "OR", ["1", "2", "3"]),
        ("quick fox", "AND", ["1", "3"]),
    ],
)
def test_uri_q_default_operator(client, corpus, q, op, expected):
    res = _s(client, corpus, method="GET", q=q, df="title", default_operator=op)
    assert sorted(_ids(res)) == expected


def test_uri_bad_default_operator(client, corpus):
    err = _err(_search(client, corpus, method="GET", q="a", default_operator="bogus"), 400,
               "illegal_argument_exception")
    assert err["reason"] == "No enum constant org.opensearch.index.query.Operator.BOGUS"


def test_uri_df_analyzes_query(client, corpus):
    assert sorted(_ids(_s(client, corpus, method="GET", q="FOX", df="title"))) == ["1", "3"]


@pytest.mark.parametrize(
    "q,sort,expected",
    [
        ("n:[2 TO 4]", "n", ["2", "3", "4"]),
        ("n:>3", "n", ["4", "5"]),
        ("-tag:a", "n", ["2", "4", "5"]),
        ("tag:a OR tag:c", "n:desc", ["4", "3", "1"]),
        ("obj.a:p", "_id:desc", ["3", "1"]),
        ("title:quick AND tag:a", "n", ["1", "3"]),
        ("tag:B", "n", ["5"]),
    ],
)
def test_uri_q_syntax_with_sort(client, corpus, q, sort, expected):
    assert _ids(_s(client, corpus, method="GET", q=q, sort=sort)) == expected


def test_uri_q_exists(client, corpus):
    assert _s(client, corpus, method="GET", q="_exists_:price")["hits"]["total"]["value"] == 4


def test_uri_q_cannot_reach_nested_fields(client, corpus):
    assert _s(client, corpus, method="GET", q="items.name:x")["hits"]["total"]["value"] == 0


def test_uri_q_type_mismatch_is_lenient(client, corpus):
    res = _s(client, corpus, method="GET", q="n:abc")
    assert res["hits"]["total"] == {"value": 0, "relation": "eq"}


def test_uri_q_empty_matches_nothing(client, corpus):
    assert _s(client, corpus, method="GET", q="")["hits"]["total"]["value"] == 0


def test_uri_q_syntax_error(client, corpus):
    err = _err(_search(client, corpus, method="GET", q="title:(fox"), 400,
               "search_phase_execution_exception", "query_shard_exception")
    assert _root_reason(err) == "Failed to parse query [title:(fox]"


def test_uri_sort_size_from_source_false(client, corpus):
    res = _s(client, corpus, method="GET", q="brown", sort="n:desc", size="1", **{"from": "1", "_source": "false"})
    assert res["hits"]["total"]["value"] == 2
    assert res["hits"]["hits"] == [{"_index": corpus, "_id": "1", "_score": None, "sort": [1]}]


def test_uri_sort_multiple_fields(client, corpus):
    res = _s(client, corpus, method="GET", sort="tag:asc,n:desc")
    assert _id_sorts(res) == [("5", ["B", 5]), ("3", ["a", 3]), ("1", ["a", 1]), ("2", ["b", 2]), ("4", ["c", 4])]


@pytest.mark.parametrize("sort", ["n:DESC", "n:sideways"])
def test_uri_sort_with_non_lowercase_order_is_silently_ignored(client, corpus, sort):
    res = _s(client, corpus, method="GET", sort=sort)
    assert res["hits"]["max_score"] == 1.0
    assert _ids(res) == ["1", "2", "3", "4", "5"]
    assert all("sort" not in h for h in res["hits"]["hits"])


def test_uri_source_includes(client, corpus):
    res = _s(client, corpus, method="GET", q="brown", _source_includes="tag,n")
    assert [h["_source"] for h in res["hits"]["hits"]] == [{"tag": "a", "n": 1}, {"tag": "b", "n": 2}]


def test_uri_source_list(client, corpus):
    res = _s(client, corpus, method="GET", q="_id:1", _source="tag,n")
    assert res["hits"]["hits"][0]["_source"] == {"tag": "a", "n": 1}


def test_uri_source_excludes(client, corpus):
    res = _s(client, corpus, method="GET", q="_id:1",
             _source_excludes="title,body,items,loc,obj,nums,date,price")
    assert res["hits"]["hits"][0]["_source"] == {"tag": "a", "n": 1}


def test_uri_source_includes_and_excludes(client, corpus):
    res = _s(client, corpus, method="GET", q="_id:1", _source_includes="obj", _source_excludes="obj.b")
    assert res["hits"]["hits"][0]["_source"] == {"obj": {"a": "p"}}


def test_uri_source_false_wins_over_includes(client, corpus):
    res = _s(client, corpus, method="GET", q="_id:1", _source="false", _source_includes="tag")
    assert "_source" not in res["hits"]["hits"][0]


def test_uri_q_overrides_body_query(client, corpus):
    res = _s(client, corpus, {"query": {"match_all": {}}}, q="tag:c")
    assert _ids(res) == ["4"]


@pytest.mark.parametrize(
    "body,params,check",
    [
        ({"size": 1}, {"size": "3"}, lambda r: len(r["hits"]["hits"]) == 3),
        ({"from": 1}, {"from": "3"}, lambda r: _ids(r) == ["4", "5"]),
        ({"track_total_hits": True, "size": 0}, {"track_total_hits": "false"}, lambda r: "total" not in r["hits"]),
        ({"version": True, "query": {"ids": {"values": ["1"]}}}, {"version": "false"},
         lambda r: "_version" not in r["hits"]["hits"][0]),
    ],
)
def test_url_params_override_body(client, corpus, body, params, check):
    assert check(_s(client, corpus, body, **params))


def test_uri_sort_is_appended_to_body_sort(client, corpus):
    res = _s(client, corpus, {"sort": ["n"]}, sort="tag:desc")
    assert _sorts(res) == [[1, "a"], [2, "b"], [3, "a"], [4, "c"], [5, "B"]]


def test_uri_size_not_a_number(client, corpus):
    err = _err(_search(client, corpus, size="abc"), 400, "illegal_argument_exception")
    assert err["reason"] == "Failed to parse int parameter [size] with value [abc]"


def test_source_url_param_with_content_type(client, corpus):
    src = json.dumps({"query": {"term": {"tag": "a"}}, "size": 0})
    res = _s(client, corpus, method="GET", source=src, source_content_type="application/json")
    assert res["hits"]["total"]["value"] == 2


def test_source_url_param_without_content_type_is_500(client, corpus):
    src = json.dumps({"query": {"term": {"tag": "a"}}})
    err = _err(_search(client, corpus, method="GET", source=src), 500, "illegal_state_exception")
    assert err["reason"] == "source and source_content_type parameters are required"


# ---------------------------------------------------------------------------
# Multiple indices, wildcards, _all, aliases, indices_boost
# ---------------------------------------------------------------------------


def test_search_two_indices_comma(client, corpus_pair):
    a, b = corpus_pair["a"], corpus_pair["b"]
    res = _s(client, f"{a},{b}", {"query": {"match": {"title": "fox"}}})
    assert res["_shards"] == {"total": 2, "successful": 2, "skipped": 0, "failed": 0}
    assert sorted((h["_index"], h["_id"]) for h in res["hits"]["hits"]) == [(a, "1"), (a, "3"), (b, "10")]


def test_search_wildcard_index(client, corpus_pair):
    res = _s(client, corpus_pair["a"] + "*", {"size": 0})
    # prefix matches corpus and corpus-b (aliases are not indices)
    assert res["hits"]["total"]["value"] == 6
    assert res["_shards"]["total"] == 2


def test_search_all_and_wildcard_star(client, index_factory):
    name = index_factory(settings=SINGLE)
    marker = _uid("sa-unique")
    client.put(f"/{name}/_doc/{marker}", params={"refresh": "true"}, json={"n": 999})
    for target in ("_all", "*"):
        res = _s(client, target, {"query": {"ids": {"values": [marker]}}})
        assert res["hits"]["total"]["value"] == 1
        assert res["hits"]["hits"][0]["_index"] == name


def test_search_missing_index_404(client, corpus):
    missing = _uid("sa-missing")
    err = _err(_search(client, f"{corpus},{missing}", {"size": 0}), 404, "index_not_found_exception")
    assert err["reason"] == f"no such index [{missing}]"
    assert err["index"] == missing
    assert err["resource.type"] == "index_or_alias"


def test_search_ignore_unavailable(client, corpus):
    res = _s(client, f"{corpus},{_uid('sa-missing')}", {"size": 0}, ignore_unavailable="true")
    assert res["_shards"]["total"] == 1
    assert res["hits"]["total"]["value"] == 5


def test_search_only_missing_with_ignore_unavailable(client):
    res = _s(client, _uid("sa-missing"), {"size": 0}, ignore_unavailable="true")
    assert res["_shards"] == {"total": 0, "successful": 0, "skipped": 0, "failed": 0}
    # quirk: max_score is 0.0 (not null) when no shards are searched
    assert res["hits"] == {"total": {"value": 0, "relation": "eq"}, "max_score": 0.0, "hits": []}


def test_search_unmatched_wildcard_is_empty(client):
    res = _s(client, _uid("sa-nomatch") + "*")
    assert res["_shards"]["total"] == 0
    assert res["hits"] == {"total": {"value": 0, "relation": "eq"}, "max_score": 0.0, "hits": []}


def test_search_unmatched_wildcard_allow_no_indices_false(client):
    pattern = _uid("sa-nomatch") + "*"
    err = _err(_search(client, pattern, allow_no_indices="false"), 404, "index_not_found_exception")
    assert err["reason"] == f"no such index [{pattern}]"


def test_search_alias_over_two_indices(client, corpus_pair):
    res = _s(client, corpus_pair["alias"], {"size": 0})
    assert res["hits"]["total"]["value"] == 6
    assert res["_shards"]["total"] == 2


def test_search_filtered_alias(client, corpus_pair):
    res = _s(client, corpus_pair["falias"])
    assert _ids(res) == ["1", "3"]
    assert all(h["_index"] == corpus_pair["a"] for h in res["hits"]["hits"])


def test_search_alias_and_index_overlap_deduplicated(client, corpus_pair):
    res = _s(client, f"{corpus_pair['alias']},{corpus_pair['a']}", {"size": 0})
    assert res["hits"]["total"]["value"] == 6
    assert res["_shards"]["total"] == 2


@pytest.mark.parametrize("form", ["list", "list_wildcard", "object"])
def test_indices_boost(client, corpus_pair, form):
    a, b = corpus_pair["a"], corpus_pair["b"]
    boost = {
        "list": [{b: 10}],
        "list_wildcard": [{b[:-1] + "b*": 10}],
        "object": {b: 10},
    }[form]
    plain = _s(client, f"{a},{b}", {"query": {"match": {"title": "fox"}}})
    assert plain["hits"]["hits"][-1]["_index"] == b
    res = _s(client, f"{a},{b}", {"query": {"match": {"title": "fox"}}, "indices_boost": boost})
    assert res["hits"]["hits"][0]["_index"] == b
    assert res["hits"]["hits"][0]["_id"] == "10"


def test_indices_boost_unknown_index_404(client, corpus):
    missing = _uid("sa-missing")
    _err(_search(client, corpus, {"indices_boost": [{missing: 2}]}), 404, "index_not_found_exception")


# ---------------------------------------------------------------------------
# track_total_hits / rest_total_hits_as_int
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "tth,expected",
    [
        (2, {"value": 2, "relation": "gte"}),
        (4, {"value": 4, "relation": "gte"}),
        (5, {"value": 5, "relation": "eq"}),
        (6, {"value": 5, "relation": "eq"}),
        (0, {"value": 0, "relation": "gte"}),
        (True, {"value": 5, "relation": "eq"}),
    ],
)
def test_track_total_hits_values(client, corpus, tth, expected):
    res = _s(client, corpus, {"size": 0, "track_total_hits": tth})
    assert res["hits"]["total"] == expected


def test_track_total_hits_false_omits_total(client, corpus):
    res = _s(client, corpus, {"size": 0, "track_total_hits": False})
    assert res["hits"] == {"max_score": None, "hits": []}


def test_track_total_hits_url_int(client, corpus):
    res = _s(client, corpus, size="0", track_total_hits="3")
    assert res["hits"]["total"] == {"value": 3, "relation": "gte"}


def test_track_total_hits_threshold_does_not_limit_hits(client, corpus):
    res = _s(client, corpus, {"track_total_hits": 2})
    assert res["hits"]["total"] == {"value": 2, "relation": "gte"}
    assert len(res["hits"]["hits"]) == 5


def test_track_total_hits_negative(client, corpus):
    r = _search(client, corpus, {"size": 0, "track_total_hits": -5})
    assert r.status_code == 400
    assert "value must be >= 0, got -5" in r.text


def test_track_total_hits_garbage_body(client, corpus):
    err = _err(_search(client, corpus, {"size": 0, "track_total_hits": "abc"}), 400, "number_format_exception")
    assert err["reason"] == 'For input string: "abc"'


def test_track_total_hits_garbage_url(client, corpus):
    err = _err(_search(client, corpus, size="0", track_total_hits="abc"), 400, "illegal_argument_exception")
    assert err["reason"] == "Failed to parse int parameter [track_total_hits] with value [abc]"


def test_rest_total_hits_as_int(client, corpus):
    res = _s(client, corpus, {"size": 0}, rest_total_hits_as_int="true")
    assert res["hits"]["total"] == 5


def test_rest_total_hits_as_int_with_tracking_disabled_is_minus_one(client, corpus):
    res = _s(client, corpus, {"size": 0, "track_total_hits": False}, rest_total_hits_as_int="true")
    assert res["hits"]["total"] == -1


def test_rest_total_hits_as_int_rejects_int_threshold(client, corpus):
    err = _err(_search(client, corpus, {"size": 0, "track_total_hits": 2}, rest_total_hits_as_int="true"),
               400, "illegal_argument_exception")
    assert err["reason"] == "[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got 2"


@pytest.mark.parametrize(
    "body,params,expected",
    [
        ({"size": 0}, {}, {"value": 10000, "relation": "gte"}),
        ({"size": 0, "track_total_hits": True}, {}, {"value": 10050, "relation": "eq"}),
        ({"size": 0, "track_total_hits": 10000}, {}, {"value": 10000, "relation": "gte"}),
        ({"size": 0, "track_total_hits": 10050}, {}, {"value": 10050, "relation": "eq"}),
        ({"size": 0, "track_total_hits": 10051}, {}, {"value": 10050, "relation": "eq"}),
        ({"size": 0, "query": {"term": {"v": 3}}}, {}, {"value": 1436, "relation": "eq"}),
        ({"size": 0}, {"rest_total_hits_as_int": "true"}, 10050),
    ],
)
def test_total_hits_default_10000_boundary(client, big, body, params, expected):
    assert _s(client, big, body, **params)["hits"]["total"] == expected


def test_count_is_always_exact_above_10000(client, big):
    assert _ok(client.get(f"/{big}/_count"))["count"] == 10050


def test_deep_page_at_window_edge(client, big):
    res = _s(client, big, {"from": 9990, "size": 10, "sort": ["_doc"], "_source": False})
    assert len(res["hits"]["hits"]) == 10


# ---------------------------------------------------------------------------
# size / from / max_result_window
# ---------------------------------------------------------------------------


def test_size_zero(client, corpus):
    res = _s(client, corpus, {"size": 0})
    assert res["hits"]["hits"] == []
    assert res["hits"]["total"]["value"] == 5


def test_from_beyond_total_returns_empty_page(client, corpus):
    res = _s(client, corpus, {"from": 50})
    assert res["hits"]["hits"] == []
    assert res["hits"]["total"]["value"] == 5


@pytest.mark.parametrize("frm,size,got", [(9999, 2, 10001), (10001, 0, 10001), (0, 10001, 10001)])
def test_max_result_window_exceeded(client, corpus, frm, size, got):
    err = _err(_search(client, corpus, {"from": frm, "size": size}), 400,
               "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err) == MAX_WINDOW_MSG.format(lim=10000, got=got)
    assert err["phase"] == "query"


def test_max_result_window_exact_limit_ok(client, corpus):
    res = _s(client, corpus, {"from": 10000, "size": 0})
    assert res["hits"]["total"]["value"] == 5


def test_max_result_window_custom_setting(client, index_factory, bulk_index):
    name = index_factory(settings={**SINGLE, "index.max_result_window": 3})
    bulk_index(name, [{"i": i} for i in range(5)])
    assert len(_s(client, name, {"from": 1, "size": 2})["hits"]["hits"]) == 2
    err = _err(_search(client, name, {"from": 2, "size": 2}), 400, "search_phase_execution_exception")
    assert _root_reason(err) == MAX_WINDOW_MSG.format(lim=3, got=4)


def test_scroll_batch_size_limited_by_max_result_window(client, index_factory, bulk_index):
    name = index_factory(settings={**SINGLE, "index.max_result_window": 3})
    bulk_index(name, [{"i": i} for i in range(5)])
    err = _err(_search(client, name, {"size": 4}, scroll="1m"), 400, "search_phase_execution_exception")
    assert _root_reason(err).startswith(
        "Batch size is too large, size must be less than or equal to: [3] but was [4]."
    )


@pytest.mark.parametrize("field,where", [("size", "body"), ("from", "body"), ("size", "url"), ("from", "url")])
@pytest.mark.parametrize("value", [-1, -5])
def test_negative_size_from(client, corpus, field, where, value):
    if where == "body":
        r = _search(client, corpus, {field: value})
    else:
        r = _search(client, corpus, **{field: str(value)})
    err = _err(r, 400, "illegal_argument_exception")
    assert err["reason"] == f"[{field}] parameter cannot be negative, found [{value}]"


def test_size_as_string_and_float_are_coerced(client, corpus):
    assert len(_s(client, corpus, {"size": "2"})["hits"]["hits"]) == 2
    assert len(_s(client, corpus, {"size": 2.5})["hits"]["hits"]) == 2


def test_size_null_rejected(client, corpus):
    err = _err(_search(client, corpus, {"size": None}), 400, "parsing_exception")
    assert err["reason"] == "Unknown key for a VALUE_NULL in [size]."


# ---------------------------------------------------------------------------
# Sort
# ---------------------------------------------------------------------------


def test_sort_multiple_keys(client, corpus):
    res = _s(client, corpus, {"sort": [{"tag": "asc"}, {"n": "desc"}]})
    assert _id_sorts(res) == [("5", ["B", 5]), ("3", ["a", 3]), ("1", ["a", 1]), ("2", ["b", 2]), ("4", ["c", 4])]


@pytest.mark.parametrize(
    "sort",
    [["tag", {"n": "desc"}], [{"tag": {"order": "asc"}}, {"n": {"order": "desc"}}]],
)
def test_sort_syntax_variants(client, corpus, sort):
    assert _ids(_s(client, corpus, {"sort": sort})) == ["5", "3", "1", "2", "4"]


def test_sort_as_plain_string(client, corpus):
    assert _id_sorts(_s(client, corpus, {"sort": "n"}))[0] == ("1", [1])


def test_sort_as_single_object(client, corpus):
    assert _sorts(_s(client, corpus, {"sort": {"n": "desc"}, "size": 1})) == [[5]]


def test_sort_order_uppercase_in_body_accepted(client, corpus):
    assert _ids(_s(client, corpus, {"sort": [{"n": "DESC"}]})) == ["5", "4", "3", "2", "1"]


def test_sort_invalid_order(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"n": "up"}]}), 400, "illegal_argument_exception")
    assert err["reason"] == "No enum constant org.opensearch.search.sort.SortOrder.UP"


def test_sort_empty_list_is_score_order(client, corpus):
    res = _s(client, corpus, {"sort": []})
    assert res["hits"]["max_score"] == 1.0
    assert all("sort" not in h for h in res["hits"]["hits"])


def test_sort_by_score_only_has_no_sort_values(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox brown"}}, "sort": ["_score"]})
    assert _ids(res) == ["1", "2", "3"] or _ids(res) == ["1", "3", "2"]
    assert all("sort" not in h for h in res["hits"]["hits"])
    assert res["hits"]["max_score"] is not None


def test_sort_score_asc_with_tiebreaker(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "brown fox"}}, "sort": [{"_score": "asc"}, {"n": "desc"}]})
    assert _ids(res) == ["3", "2", "1"]
    for h in res["hits"]["hits"]:
        assert h["sort"][0] == h["_score"]
    # quirk: max_score is only reported when the primary sort is _score desc
    assert res["hits"]["max_score"] is None


def test_sort_secondary_score_tracks_scores_but_no_max_score(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "brown fox"}}, "sort": [{"n": "desc"}, "_score"]})
    assert _ids(res) == ["3", "2", "1"]
    assert all(h["sort"][1] == h["_score"] and h["_score"] > 0 for h in res["hits"]["hits"])
    assert res["hits"]["max_score"] is None


def test_sort_score_then_field_has_sort_values(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}, "sort": ["_score", "n"]})
    assert [h["sort"][1] for h in res["hits"]["hits"]] == [1, 3]
    assert all(h["sort"][0] == h["_score"] for h in res["hits"]["hits"])


def test_sort_doc_order(client, corpus):
    asc = _s(client, corpus, {"sort": ["_doc"]})
    desc = _s(client, corpus, {"sort": [{"_doc": "desc"}]})
    assert _ids(asc) == ["1", "2", "3", "4", "5"]
    assert _ids(desc) == ["5", "4", "3", "2", "1"]
    assert all(isinstance(s[0], int) for s in _sorts(asc))


def test_sort_by_id(client, corpus):
    assert _id_sorts(_s(client, corpus, {"sort": [{"_id": "desc"}]}))[:2] == [("5", ["5"]), ("4", ["4"])]


def test_sort_by_index_name(client, corpus):
    res = _s(client, corpus, {"sort": [{"_index": "desc"}, {"n": "asc"}], "size": 1})
    assert _sorts(res) == [[corpus, 1]]


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"order": "asc"}, [("1", 1.5), ("2", 2.5), ("3", 3.5), ("4", 4.5), ("5", "Infinity")]),
        ({"order": "desc"}, [("4", 4.5), ("3", 3.5), ("2", 2.5), ("1", 1.5), ("5", "-Infinity")]),
        ({"order": "asc", "missing": "_first"}, [("5", "-Infinity"), ("1", 1.5), ("2", 2.5), ("3", 3.5), ("4", 4.5)]),
        ({"order": "desc", "missing": "_last"}, [("4", 4.5), ("3", 3.5), ("2", 2.5), ("1", 1.5), ("5", "-Infinity")]),
        ({"order": "asc", "missing": 3}, [("1", 1.5), ("2", 2.5), ("5", 3.0), ("3", 3.5), ("4", 4.5)]),
    ],
)
def test_sort_missing_on_float(client, corpus, spec, expected):
    res = _s(client, corpus, {"sort": [{"price": spec}]})
    assert [(i, s[0]) for i, s in _id_sorts(res)] == expected


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"order": "asc"}, [("1", "p"), ("3", "p"), ("2", "q"), ("4", None), ("5", None)]),
        ({"order": "desc", "missing": "_last"}, [("2", "q"), ("1", "p"), ("3", "p"), ("4", None), ("5", None)]),
        ({"order": "desc", "missing": "zz"}, [("4", "zz"), ("5", "zz"), ("2", "q"), ("1", "p"), ("3", "p")]),
        ({"order": "asc", "missing": "_first"}, [("4", None), ("5", None), ("1", "p"), ("3", "p"), ("2", "q")]),
    ],
)
def test_sort_missing_on_keyword(client, corpus, spec, expected):
    res = _s(client, corpus, {"sort": [{"obj.a": spec}]})
    assert [(i, s[0]) for i, s in _id_sorts(res)] == expected


@pytest.mark.parametrize(
    "mode,order,expected",
    [
        (None, "asc", [("2", 1), ("1", 3), ("3", 5), ("4", 2147483647), ("5", 2147483647)]),
        (None, "desc", [("2", 20), ("1", 10), ("3", 5), ("4", -2147483648), ("5", -2147483648)]),
        ("min", "asc", [("2", 1), ("1", 3), ("3", 5), ("4", 2147483647), ("5", 2147483647)]),
        ("max", "asc", [("3", 5), ("1", 10), ("2", 20), ("4", 2147483647), ("5", 2147483647)]),
        ("avg", "asc", [("3", 5), ("1", 7), ("2", 11), ("4", 2147483647), ("5", 2147483647)]),
        ("sum", "asc", [("3", 5), ("1", 13), ("2", 21), ("4", 2147483647), ("5", 2147483647)]),
        ("median", "asc", [("3", 5), ("1", 7), ("2", 11), ("4", 2147483647), ("5", 2147483647)]),
        ("sum", "desc", [("2", 21), ("1", 13), ("3", 5), ("4", -2147483648), ("5", -2147483648)]),
    ],
)
def test_sort_mode_on_int_arrays(client, corpus, mode, order, expected):
    spec = {"order": order}
    if mode:
        spec["mode"] = mode
    res = _s(client, corpus, {"sort": [{"nums": spec}]})
    assert [(i, s[0]) for i, s in _id_sorts(res)] == expected


def test_sort_mode_invalid(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"n": {"order": "asc", "mode": "bogus"}}]}), 400,
               "x_content_parse_exception")
    assert err["caused_by"]["reason"] == "Unknown SortMode [bogus]"


def test_sort_mode_avg_on_keyword_rejected(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"tag": {"order": "asc", "mode": "avg"}}]}), 400,
               "search_phase_execution_exception", "query_shard_exception")
    assert _root_reason(err) == "we only support AVG, MEDIAN and SUM on number based fields"


def test_sort_unmapped_field_error(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"nope": "asc"}]}), 400,
               "search_phase_execution_exception", "query_shard_exception")
    assert _root_reason(err) == "No mapping found for [nope] in order to sort on"


def test_sort_unmapped_type(client, corpus):
    res = _s(client, corpus, {"sort": [{"nope": {"order": "asc", "unmapped_type": "long"}}]})
    assert _ids(res) == ["1", "2", "3", "4", "5"]
    assert _sorts(res) == [[9223372036854775807]] * 5


def test_sort_unmapped_type_across_indices(client, corpus, index_factory):
    other = index_factory(settings=SINGLE, mappings={"properties": {"only": {"type": "long"}}})
    _bulk_ids(client, other, {"m": {"only": 7}})
    res = _s(client, f"{corpus},{other}", {"sort": [{"only": {"order": "asc", "unmapped_type": "long"}}], "size": 2})
    assert _id_sorts(res) == [("m", [7]), ("1", [9223372036854775807])]
    # without unmapped_type the shard lacking the field fails -> partial results (200)
    res = _s(client, f"{corpus},{other}", {"sort": [{"only": "asc"}]})
    assert res["_shards"]["total"] == 2
    assert res["_shards"]["successful"] == 1
    assert res["_shards"]["failed"] == 1
    failure = res["_shards"]["failures"][0]
    assert failure["index"] == corpus and failure["shard"] == 0
    assert failure["reason"]["type"] == "query_shard_exception"
    assert failure["reason"]["reason"] == "No mapping found for [only] in order to sort on"
    assert _ids(res) == ["m"]
    assert res["hits"]["total"] == {"value": 1, "relation": "eq"}


def test_partial_results_disallowed(client, corpus, index_factory):
    other = index_factory(settings=SINGLE, mappings={"properties": {"only": {"type": "long"}}})
    _bulk_ids(client, other, {"m": {"only": 7}})
    r = _search(client, f"{corpus},{other}", {"sort": [{"only": "asc"}]}, allow_partial_search_results="false")
    err = _err(r, 400, "search_phase_execution_exception", "query_shard_exception")
    assert _root_reason(err) == "No mapping found for [only] in order to sort on"


def test_sort_on_text_field_rejected(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"title": "asc"}]}), 400,
               "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err).startswith("Text fields are not optimised for operations that require per-document field data")


def test_sort_on_keyword_subfield(client, corpus):
    res = _s(client, corpus, {"sort": [{"title.raw": "asc"}]})
    assert [s[0] for s in _sorts(res)] == [
        "Zebra", "lazy brown dog", "quick brown fox", "quick red fox", "slow green turtle"
    ]


def test_sort_keyword_unicode_and_case_is_binary_order(client, index_factory):
    name = index_factory(settings=SINGLE, mappings={"properties": {"k": {"type": "keyword"}}})
    values = ["b", "B", "a", "A", "é", "e", "Z", "z", "ä", "10", "9", "_", "日本", "ß", "", " "]
    _bulk_ids(client, name, {str(i): {"k": v} for i, v in enumerate(values)})
    res = _s(client, name, {"sort": [{"k": "asc"}], "size": 50, "_source": False})
    assert [s[0] for s in _sorts(res)] == [
        "", " ", "10", "9", "A", "B", "Z", "_", "a", "b", "e", "z", "ß", "ä", "é", "日本"
    ]


@pytest.mark.parametrize(
    "spec,expected",
    [
        ({"order": "desc", "nested": {"path": "items"}},
         [("3", 9), ("1", 5), ("2", 2), ("4", -2147483648), ("5", -2147483648)]),
        ({"order": "asc", "nested": {"path": "items"}},
         [("1", 1), ("2", 2), ("3", 9), ("4", 2147483647), ("5", 2147483647)]),
        ({"order": "asc", "mode": "sum", "nested": {"path": "items"}},
         [("2", 2), ("1", 6), ("3", 9), ("4", 2147483647), ("5", 2147483647)]),
        ({"order": "desc", "mode": "max", "nested": {"path": "items", "filter": {"term": {"items.name": "x"}}}},
         [("1", 5), ("2", 2), ("3", -2147483648), ("4", -2147483648), ("5", -2147483648)]),
        ({"order": "asc", "nested_path": "items"},
         [("1", 1), ("2", 2), ("3", 9), ("4", 2147483647), ("5", 2147483647)]),
    ],
)
def test_sort_on_nested_field(client, corpus, spec, expected):
    res = _s(client, corpus, {"sort": [{"items.qty": spec}]})
    assert [(i, s[0]) for i, s in _id_sorts(res)] == expected


def test_sort_on_nested_field_without_nested_sees_nothing(client, corpus):
    res = _s(client, corpus, {"sort": [{"items.qty": {"order": "desc"}}]})
    assert _sorts(res) == [[-2147483648]] * 5


def test_sort_nested_wrong_path(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"items.qty": {"order": "asc", "nested": {"path": "wrong"}}}]}),
               400, "search_phase_execution_exception", "query_shard_exception")
    assert _root_reason(err) == "[nested] failed to find nested object under path [wrong]"


def test_sort_date_values_are_epoch_millis(client, corpus):
    res = _s(client, corpus, {"sort": [{"date": "desc"}], "size": 2})
    assert _id_sorts(res) == [("5", [1714521600000]), ("4", [1711929600000])]


def test_sort_format_option_not_supported(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"date": {"order": "desc", "format": "yyyy-MM-dd"}}]}), 400,
               "x_content_parse_exception")
    assert err["reason"].endswith("[field_sort] unknown field [format]")


def test_sort_numeric_type_date_nanos(client, corpus):
    res = _s(client, corpus, {"sort": [{"date": {"order": "asc", "numeric_type": "date_nanos"}}], "size": 2})
    assert _sorts(res) == [[1704067200000000000], [1706745600000000000]]


@pytest.mark.parametrize(
    "numeric_type,expected",
    [
        ("long", [4, 3, 2, 1, -9223372036854775808]),
        ("double", [4.5, 3.5, 2.5, 1.5, "-Infinity"]),
    ],
)
def test_sort_numeric_type_on_float(client, corpus, numeric_type, expected):
    res = _s(client, corpus, {"sort": [{"price": {"order": "desc", "numeric_type": numeric_type}}]})
    assert [s[0] for s in _sorts(res)] == expected


def test_sort_track_scores(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}, "sort": [{"n": "asc"}], "track_scores": True})
    assert _ids(res) == ["1", "3"]
    assert all(isinstance(h["_score"], float) and h["_score"] > 0 for h in res["hits"]["hits"])
    assert res["hits"]["max_score"] == max(h["_score"] for h in res["hits"]["hits"])


def test_sort_script_number(client, corpus):
    res = _s(client, corpus, {"sort": [{"_script": {"type": "number", "script": {"source": "doc['n'].value * -1"},
                                                     "order": "asc"}}]})
    assert _id_sorts(res) == [("5", [-5.0]), ("4", [-4.0]), ("3", [-3.0]), ("2", [-2.0]), ("1", [-1.0])]


def test_sort_script_string(client, corpus):
    res = _s(client, corpus, {"sort": [{"_script": {"type": "string", "script": {"source": "doc['tag'].value"},
                                                     "order": "desc"}}, {"n": "asc"}]})
    assert _id_sorts(res) == [("4", ["c", 4]), ("2", ["b", 2]), ("1", ["a", 1]), ("3", ["a", 3]), ("5", ["B", 5])]


def test_sort_script_with_params(client, corpus):
    body = {"sort": [{"_script": {"type": "number", "order": "desc",
                                  "script": {"source": "Math.abs(doc['n'].value - params.p)", "params": {"p": 3}}}},
                     {"n": "asc"}]}
    assert _ids(_s(client, corpus, body)) == ["1", "5", "2", "4", "3"]


def test_sort_geo_distance_asc_km(client, corpus):
    res = _s(client, corpus, {"sort": [{"_geo_distance": {"loc": {"lat": 0, "lon": 3}, "order": "asc", "unit": "km"}}]})
    assert _ids(res) == ["4", "3", "2", "1", "5"]
    sorts = [s[0] for s in _sorts(res)]
    assert sorts[0] == 0.0
    assert 111.0 < sorts[1] < 111.4
    assert 333.0 < sorts[3] < 334.0
    assert sorts[4] == "Infinity"


def test_sort_geo_distance_desc_puts_missing_first(client, corpus):
    res = _s(client, corpus, {"sort": [{"_geo_distance": {"loc": [3, 0], "order": "desc", "unit": "m"}}]})
    assert _ids(res) == ["5", "1", "2", "3", "4"]
    assert _sorts(res)[0] == ["Infinity"]


def test_sort_geo_distance_string_point(client, corpus):
    res = _s(client, corpus, {"sort": [{"_geo_distance": {"loc": "0,2", "unit": "km"}}]})
    assert _ids(res)[0] == "3"
    assert _sorts(res)[0] == [0.0]


def test_sort_geo_distance_multiple_points_min(client, corpus):
    res = _s(client, corpus, {"sort": [{"_geo_distance": {"loc": [{"lat": 0, "lon": 0}, {"lat": 0, "lon": 3}],
                                                          "mode": "min", "unit": "km"}}, {"n": "asc"}]})
    assert _ids(res) == ["1", "4", "2", "3", "5"]


def test_sort_geo_distance_bad_unit(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"_geo_distance": {"loc": {"lat": 0, "lon": 0}, "unit": "parsecs"}}]}),
               400, "illegal_argument_exception")
    assert err["reason"] == "No distance unit match [parsecs]"


def test_sort_geo_distance_unmapped_field(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"_geo_distance": {"nope": {"lat": 0, "lon": 0}}}]}), 400,
               "search_phase_execution_exception")
    assert _root_reason(err) == "failed to find mapper for [nope] for geo distance based sort"
    res = _s(client, corpus, {"sort": [{"_geo_distance": {"nope": {"lat": 0, "lon": 0}, "ignore_unmapped": True}}]})
    assert _sorts(res) == [["Infinity"]] * 5


# ---------------------------------------------------------------------------
# search_after
# ---------------------------------------------------------------------------


def test_search_after_with_tiebreaker(client, corpus):
    res = _s(client, corpus, {"sort": [{"tag": "asc"}, {"n": "asc"}], "search_after": ["a", 1]})
    assert _id_sorts(res) == [("3", ["a", 3]), ("2", ["b", 2]), ("4", ["c", 4])]


def test_search_after_full_pagination(client, corpus):
    seen, after = [], None
    while True:
        body = {"sort": [{"n": "desc"}], "size": 2}
        if after:
            body["search_after"] = after
        res = _s(client, corpus, body)
        if not res["hits"]["hits"]:
            break
        seen += _ids(res)
        after = res["hits"]["hits"][-1]["sort"]
        assert res["hits"]["total"]["value"] == 5
    assert seen == ["5", "4", "3", "2", "1"]


@pytest.mark.parametrize("after", [["2024-03-01T00:00:00Z"], [1709251200000]])
def test_search_after_date_accepts_string_or_millis(client, corpus, after):
    res = _s(client, corpus, {"sort": [{"date": "asc"}], "search_after": after})
    assert _ids(res) == ["4", "5"]


def test_search_after_with_explicit_from_zero(client, corpus):
    res = _s(client, corpus, {"sort": [{"n": "asc"}], "search_after": [3], "from": 0})
    assert _ids(res) == ["4", "5"]


def test_search_after_with_from_is_500(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"n": "asc"}], "search_after": [3], "from": 1}), 500,
               "search_phase_execution_exception", "search_exception")
    assert _root_reason(err) == "`from` parameter must be set to 0 when `search_after` is used."


def test_search_after_wrong_length(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"tag": "asc"}], "search_after": ["a", 1]}), 400,
               "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err) == "search_after has 2 value(s) but sort has 1."


def test_search_after_without_sort(client, corpus):
    err = _err(_search(client, corpus, {"search_after": [1]}), 400,
               "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err) == "Sort must contain at least one field."


def test_search_after_bad_value_type(client, corpus):
    err = _err(_search(client, corpus, {"sort": [{"n": "asc"}], "search_after": ["x"]}), 400,
               "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err) == "Failed to parse search_after value for field [n]."


def test_search_after_in_scroll_is_500(client, corpus):
    err = _err(_search(client, corpus, {"size": 2, "sort": ["n"], "search_after": [1]}, scroll="1m"), 500,
               "search_phase_execution_exception", "search_exception")
    assert _root_reason(err) == "`search_after` cannot be used in a scroll context."


# ---------------------------------------------------------------------------
# Scroll
# ---------------------------------------------------------------------------


def test_scroll_initial_and_continue(client, corpus, scrolls):
    first = _s(client, corpus, {"size": 2, "sort": ["n"]}, scroll="1m")
    assert list(first)[0] == "_scroll_id"
    sid = first["_scroll_id"]
    scrolls.append(sid)
    assert _ids(first) == ["1", "2"]
    assert first["hits"]["total"] == {"value": 5, "relation": "eq"}

    second = _ok(client.post("/_search/scroll", json={"scroll": "1m", "scroll_id": sid}))
    assert set(second) == {"_scroll_id", "took", "timed_out", "_shards", "hits"}
    assert _ids(second) == ["3", "4"]
    assert second["_scroll_id"] == sid
    assert second["hits"]["total"] == {"value": 5, "relation": "eq"}

    third = _ok(client.get(f"/_search/scroll/{sid}", params={"scroll": "1m"}))
    assert _ids(third) == ["5"]

    exhausted = _ok(client.post("/_search/scroll", json={"scroll": "1m", "scroll_id": sid}))
    assert exhausted["hits"]["hits"] == []
    assert exhausted["hits"]["total"]["value"] == 5


def test_scroll_id_via_query_params(client, corpus, scrolls):
    sid = _s(client, corpus, {"size": 2}, scroll="1m")["_scroll_id"]
    scrolls.append(sid)
    res = _ok(client.post("/_search/scroll", params={"scroll": "1m", "scroll_id": sid}))
    assert len(res["hits"]["hits"]) == 2


def test_scroll_rest_total_hits_as_int(client, corpus, scrolls):
    sid = _s(client, corpus, {"size": 2}, scroll="1m")["_scroll_id"]
    scrolls.append(sid)
    res = _ok(client.post("/_search/scroll", json={"scroll": "1m", "scroll_id": sid},
                          params={"rest_total_hits_as_int": "true"}))
    assert res["hits"]["total"] == 5


def test_scroll_clear_single(client, corpus):
    sid = _s(client, corpus, {"size": 2}, scroll="1m")["_scroll_id"]
    r = client.request("DELETE", "/_search/scroll", json={"scroll_id": sid})
    assert r.status_code == 200
    assert r.json() == {"succeeded": True, "num_freed": 1}
    # clearing again: 404 but still "succeeded"
    r = client.request("DELETE", "/_search/scroll", json={"scroll_id": [sid]})
    assert r.status_code == 404
    assert r.json() == {"succeeded": True, "num_freed": 0}


def test_scroll_clear_multiple_ids(client, corpus):
    s1 = _s(client, corpus, {"size": 1}, scroll="1m")["_scroll_id"]
    s2 = _s(client, corpus, {"size": 1}, scroll="1m")["_scroll_id"]
    r = client.request("DELETE", "/_search/scroll", json={"scroll_id": [s1, s2]})
    assert r.status_code == 200
    assert r.json() == {"succeeded": True, "num_freed": 2}


def test_scroll_clear_via_path(client, corpus):
    sid = _s(client, corpus, {"size": 1}, scroll="1m")["_scroll_id"]
    r = client.delete(f"/_search/scroll/{sid}")
    assert r.status_code == 200
    assert r.json() == {"succeeded": True, "num_freed": 1}


def test_scroll_after_clear_is_missing_context(client, corpus):
    sid = _s(client, corpus, {"size": 1}, scroll="1m")["_scroll_id"]
    client.request("DELETE", "/_search/scroll", json={"scroll_id": sid})
    err = _err(client.post("/_search/scroll", json={"scroll": "1m", "scroll_id": sid}), 404,
               "search_phase_execution_exception", "search_context_missing_exception")
    assert _root_reason(err).startswith("No search context found for id [")


def test_scroll_invalid_id(client):
    err = _err(client.post("/_search/scroll", json={"scroll": "1m", "scroll_id": "garbage"}), 400,
               "illegal_argument_exception")
    assert err["reason"] == "Cannot parse scroll id"


def test_scroll_missing_id(client):
    err = _err(client.post("/_search/scroll", json={"scroll": "1m"}), 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: scrollId is missing;"


def test_clear_scroll_invalid_id(client):
    err = _err(client.request("DELETE", "/_search/scroll", json={"scroll_id": "garbage"}), 400,
               "illegal_argument_exception")
    assert err["reason"] == "Cannot parse scroll id"


def test_clear_scroll_no_ids(client):
    err = _err(client.request("DELETE", "/_search/scroll", json={}), 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: no scroll ids specified;"


@pytest.mark.parametrize(
    "body,params,reason",
    [
        ({"size": 2, "from": 1}, {}, "Validation Failed: 1: using [from] is not allowed in a scroll context;"),
        ({"size": 2}, {"request_cache": "true"}, "Validation Failed: 1: [request_cache] cannot be used in a scroll context;"),
        ({"size": 0}, {}, "Validation Failed: 1: [size] cannot be [0] in a scroll context;"),
        ({"size": 2}, {"track_total_hits": "2"},
         "Validation Failed: 1: disabling [track_total_hits] is not allowed in a scroll context;"),
    ],
)
def test_scroll_validation_errors(client, corpus, body, params, reason):
    err = _err(_search(client, corpus, body, scroll="1m", **params), 400, "action_request_validation_exception")
    assert err["reason"] == reason


def test_scroll_with_collapse_is_500(client, corpus):
    err = _err(_search(client, corpus, {"collapse": {"field": "tag"}}, scroll="1m"), 500,
               "search_phase_execution_exception", "search_exception")
    assert _root_reason(err) == "cannot use `collapse` in a scroll context"


def test_scroll_keep_alive_too_large(client, corpus):
    err = _err(_search(client, corpus, {"size": 2}, scroll="2d"), 400, "search_phase_execution_exception")
    assert _root_reason(err).startswith("Keep alive for request (2d) is too large. It must be less than (1d).")


def test_scroll_keep_alive_unparseable(client, corpus):
    err = _err(_search(client, corpus, {"size": 2}, scroll="bogus"), 400, "illegal_argument_exception")
    assert err["reason"] == "failed to parse [bogus]"


def test_scroll_continue_with_too_large_keep_alive_kills_context(client, corpus, scrolls):
    sid = _s(client, corpus, {"size": 2}, scroll="1m")["_scroll_id"]
    scrolls.append(sid)
    _err(client.post("/_search/scroll", json={"scroll": "2d", "scroll_id": sid}), 400)
    _err(client.post("/_search/scroll", json={"scroll": "1m", "scroll_id": sid}), 404,
         "search_phase_execution_exception", "search_context_missing_exception")


def test_sliced_scroll_partitions_all_docs(client, corpus, scrolls):
    seen = []
    for i in range(2):
        res = _s(client, corpus, {"size": 10, "slice": {"id": i, "max": 2}}, scroll="1m")
        scrolls.append(res["_scroll_id"])
        seen += _ids(res)
    assert sorted(seen) == ["1", "2", "3", "4", "5"]


@pytest.mark.parametrize(
    "slc,reason",
    [({"id": 2, "max": 2}, "max must be greater than id"), ({"id": 0, "max": 1}, "max must be greater than 1")],
)
def test_sliced_scroll_invalid(client, corpus, slc, reason):
    err = _err(_search(client, corpus, {"size": 10, "slice": slc}, scroll="1m"), 400, "x_content_parse_exception")
    assert err["caused_by"]["reason"] == reason


def test_slice_without_scroll_or_pit(client, corpus):
    err = _err(_search(client, corpus, {"slice": {"id": 0, "max": 2}}), 500,
               "search_phase_execution_exception", "search_exception")
    assert _root_reason(err) == "`slice` cannot be used outside of a scroll context or PIT context"


# ---------------------------------------------------------------------------
# Point in time
# ---------------------------------------------------------------------------


def _open_pit(client, index, pits, keep_alive="1m"):
    r = client.post(f"/{index}/_search/point_in_time", params={"keep_alive": keep_alive})
    body = _ok(r)
    pits.append(body["pit_id"])
    return body


def test_pit_create_shape(client, corpus, pits):
    body = _open_pit(client, corpus, pits)
    assert set(body) == {"pit_id", "_shards", "creation_time"}
    assert body["_shards"] == {"total": 1, "successful": 1, "skipped": 0, "failed": 0}
    assert isinstance(body["creation_time"], int)


def test_pit_requires_keep_alive(client, corpus):
    err = _err(client.post(f"/{corpus}/_search/point_in_time"), 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: keep alive not specified;"


def test_pit_bad_keep_alive(client, corpus):
    err = _err(client.post(f"/{corpus}/_search/point_in_time", params={"keep_alive": "bogus"}), 400,
               "illegal_argument_exception")
    assert err["reason"] == "failed to parse [bogus]"


def test_pit_keep_alive_too_large(client, corpus):
    err = _err(client.post(f"/{corpus}/_search/point_in_time", params={"keep_alive": "2d"}), 400,
               "illegal_argument_exception")
    assert err["reason"].startswith("Keep alive for request (2d) is too large. It must be less than (1d).")
    assert "[point_in_time.max_keep_alive]" in err["reason"]


def test_pit_missing_index(client):
    _err(client.post(f"/{_uid('sa-missing')}/_search/point_in_time", params={"keep_alive": "1m"}), 404,
         "index_not_found_exception")


def test_pit_search_is_snapshot(client, index_factory, pits):
    name = index_factory(settings=SINGLE, mappings=MAPPING)
    _bulk_ids(client, name, DOCS)
    pit = _open_pit(client, name, pits)["pit_id"]
    client.put(f"/{name}/_doc/6", params={"refresh": "true"}, json={"n": 6})
    res = _ok(client.post("/_search", json={"pit": {"id": pit, "keep_alive": "1m"}, "sort": [{"n": "asc"}]}))
    assert list(res)[0] == "pit_id"
    assert res["pit_id"] == pit
    assert _ids(res) == ["1", "2", "3", "4", "5"]
    assert _s(client, name, {"size": 0})["hits"]["total"]["value"] == 6


def test_pit_search_without_keep_alive(client, corpus, pits):
    pit = _open_pit(client, corpus, pits)["pit_id"]
    res = _ok(client.post("/_search", json={"pit": {"id": pit}, "size": 2, "sort": [{"n": "asc"}]}))
    assert _ids(res) == ["1", "2"]


def test_pit_with_index_in_path_rejected(client, corpus, pits):
    pit = _open_pit(client, corpus, pits)["pit_id"]
    err = _err(_search(client, corpus, {"pit": {"id": pit}}), 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: [indices] cannot be used with point in time;"


def test_shard_doc_requires_pit(client, corpus):
    err = _err(_search(client, corpus, {"sort": ["_shard_doc"]}), 400, "action_request_validation_exception")
    assert err["reason"] == (
        "Validation Failed: 1: _shard_doc is only supported with point-in-time (PIT). Add a PIT or remove _shard_doc.;"
    )


def test_pit_search_after_with_shard_doc(client, corpus, pits):
    pit = _open_pit(client, corpus, pits)["pit_id"]
    body = {"pit": {"id": pit}, "size": 2, "sort": [{"n": "asc"}, {"_shard_doc": "asc"}]}
    first = _ok(client.post("/_search", json=body))
    assert _ids(first) == ["1", "2"]
    assert all(len(s) == 2 and isinstance(s[1], int) for s in _sorts(first))
    second = _ok(client.post("/_search", json={**body, "search_after": first["hits"]["hits"][-1]["sort"]}))
    assert _ids(second) == ["3", "4"]


def test_pit_slices_partition(client, corpus, pits):
    pit = _open_pit(client, corpus, pits)["pit_id"]
    seen = []
    for i in range(3):
        res = _ok(client.post("/_search", json={"pit": {"id": pit}, "size": 10, "slice": {"id": i, "max": 3}}))
        seen += _ids(res)
    assert sorted(seen) == ["1", "2", "3", "4", "5"]


def test_pit_list_all_contains_ours(client, corpus, pits):
    pit = _open_pit(client, corpus, pits)["pit_id"]
    res = _ok(client.get("/_search/point_in_time/_all"))
    mine = [p for p in res["pits"] if p["pit_id"] == pit]
    assert len(mine) == 1
    assert mine[0]["keep_alive"] == 60000
    assert isinstance(mine[0]["creation_time"], int)


def test_pit_delete_and_search_after_delete(client, corpus):
    pit = client.post(f"/{corpus}/_search/point_in_time", params={"keep_alive": "1m"}).json()["pit_id"]
    r = client.request("DELETE", "/_search/point_in_time", json={"pit_id": [pit]})
    assert _ok(r) == {"pits": [{"successful": True, "pit_id": pit}]}
    _err(client.post("/_search", json={"pit": {"id": pit}}), 404,
         "search_phase_execution_exception", "search_context_missing_exception")
    # quirk: deleting an already deleted PIT still reports success
    r = client.request("DELETE", "/_search/point_in_time", json={"pit_id": [pit]})
    assert _ok(r) == {"pits": [{"successful": True, "pit_id": pit}]}


def test_pit_invalid_id(client):
    err = _err(client.post("/_search", json={"pit": {"id": "garbage"}}), 400, "illegal_argument_exception")
    assert err["reason"] == "invalid id: [garbage]"
    err = _err(client.request("DELETE", "/_search/point_in_time", json={"pit_id": ["garbage"]}), 400,
               "illegal_argument_exception")
    assert err["reason"] == "invalid id: [garbage]"


def test_pit_delete_without_ids(client):
    err = _err(client.request("DELETE", "/_search/point_in_time", json={}), 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: no pit ids specified;"


def test_pit_multi_shard_shard_doc_pagination(client, index_factory, pits):
    name = index_factory(settings={"number_of_shards": 2, "number_of_replicas": 0},
                         mappings={"properties": {"i": {"type": "integer"}}})
    _bulk_ids(client, name, {f"a{i}": {"i": i} for i in range(11)}, routing="a")
    _bulk_ids(client, name, {f"b{i}": {"i": i} for i in range(11)}, routing="b")
    pit = _open_pit(client, name, pits)["pit_id"]
    seen, after = [], None
    while True:
        body = {"pit": {"id": pit}, "size": 7, "sort": [{"_shard_doc": "asc"}], "_source": False}
        if after:
            body["search_after"] = after
        res = _ok(client.post("/_search", json=body))
        assert res["_shards"]["total"] == 2
        if not res["hits"]["hits"]:
            break
        seen += _ids(res)
        after = res["hits"]["hits"][-1]["sort"]
    assert len(seen) == 22 and len(set(seen)) == 22


# ---------------------------------------------------------------------------
# min_score / post_filter / named queries
# ---------------------------------------------------------------------------


def test_min_score_filters_hits_and_total(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox brown"}}, "min_score": 0.5})
    assert _ids(res) == ["1"]
    assert res["hits"]["total"] == {"value": 1, "relation": "eq"}


@pytest.mark.parametrize("min_score,total", [(1.0, 5), (1.01, 0), (0, 5)])
def test_min_score_with_match_all(client, corpus, min_score, total):
    res = _s(client, corpus, {"min_score": min_score})
    assert res["hits"]["total"]["value"] == total


def test_post_filter_does_not_affect_aggs(client, corpus):
    res = _s(client, corpus, {"post_filter": {"term": {"tag": "a"}}, "aggs": {"t": {"terms": {"field": "tag"}}}})
    assert _ids(res) == ["1", "3"]
    assert res["hits"]["total"] == {"value": 2, "relation": "eq"}
    assert res["aggregations"]["t"]["buckets"] == [
        {"key": "a", "doc_count": 2}, {"key": "B", "doc_count": 1},
        {"key": "b", "doc_count": 1}, {"key": "c", "doc_count": 1},
    ]


def test_query_does_affect_aggs(client, corpus):
    res = _s(client, corpus, {"query": {"term": {"tag": "a"}}, "aggs": {"t": {"terms": {"field": "tag"}}}})
    assert res["aggregations"]["t"]["buckets"] == [{"key": "a", "doc_count": 2}]


def test_post_filter_does_not_change_scores(client, corpus):
    plain = _s(client, corpus, {"query": {"match": {"title": "fox brown"}}})
    pf = _s(client, corpus, {"query": {"match": {"title": "fox brown"}}, "post_filter": {"term": {"tag": "a"}}})
    scores = {h["_id"]: h["_score"] for h in plain["hits"]["hits"]}
    assert {h["_id"]: h["_score"] for h in pf["hits"]["hits"]} == {"1": scores["1"], "3": scores["3"]}


def test_named_queries_matched_queries(client, corpus):
    body = {
        "query": {"bool": {"should": [
            {"match": {"title": {"query": "fox", "_name": "q_fox"}}},
            {"term": {"tag": {"value": "b", "_name": "q_b"}}},
            {"range": {"n": {"gte": 3, "_name": "q_n"}}},
        ]}},
        "sort": ["n"],
    }
    res = _s(client, corpus, body)
    got = [(h["_id"], sorted(h["matched_queries"])) for h in res["hits"]["hits"]]
    assert got == [("1", ["q_fox"]), ("2", ["q_b"]), ("3", ["q_fox", "q_n"]), ("4", ["q_n"]), ("5", ["q_n"])]


def test_named_queries_with_scores(client, corpus):
    res = _s(client, corpus, {"query": {"bool": {"should": [{"match": {"title": {"query": "fox", "_name": "q_fox"}}}]}}},
             include_named_queries_score="true")
    for h in res["hits"]["hits"]:
        assert set(h["matched_queries"]) == {"q_fox"}
        assert isinstance(h["matched_queries"]["q_fox"], float)


def test_named_queries_include_post_filter_and_filter_clauses(client, corpus):
    res = _s(client, corpus, {"query": {"match_all": {"_name": "all"}},
                              "post_filter": {"term": {"tag": {"value": "a", "_name": "pf"}}}})
    assert [sorted(h["matched_queries"]) for h in res["hits"]["hits"]] == [["all", "pf"], ["all", "pf"]]
    res = _s(client, corpus, {"query": {"bool": {"filter": [{"term": {"tag": {"value": "a", "_name": "f1"}}}],
                                                 "must_not": [{"term": {"n": {"value": 3, "_name": "mn"}}}]}}})
    assert [(h["_id"], h["matched_queries"]) for h in res["hits"]["hits"]] == [("1", ["f1"])]


def test_no_matched_queries_key_without_names(client, corpus):
    res = _s(client, corpus, {"query": {"term": {"tag": "a"}}})
    assert all("matched_queries" not in h for h in res["hits"]["hits"])


# ---------------------------------------------------------------------------
# Highlighting
# ---------------------------------------------------------------------------


def _hl(client, index, query, highlight, **params):
    res = _s(client, index, {"query": query, "highlight": highlight}, **params)
    return [(h["_id"], h.get("highlight")) for h in res["hits"]["hits"]]


LAZY = {"match": {"body": "lazy"}}
LAZY_EXPECTED = [
    ("2", {"body": ["a <em>lazy</em> dog sleeps all day long"]}),
    ("1", {"body": ["the quick brown fox jumps over the <em>lazy</em> dog"]}),
]


@pytest.mark.parametrize("hl_type", [None, "unified", "plain", "fvh"])
def test_highlight_types_basic(client, corpus, hl_type):
    hl = {"fields": {"body": {}}}
    if hl_type:
        hl["type"] = hl_type
    assert _hl(client, corpus, LAZY, hl) == LAZY_EXPECTED


def test_highlight_type_per_field(client, corpus):
    assert _hl(client, corpus, LAZY, {"fields": {"body": {"type": "plain"}}}) == LAZY_EXPECTED


def test_highlight_fvh_requires_term_vectors(client, corpus):
    err = _err(_search(client, corpus, {"query": {"match": {"title": "fox"}},
                                        "highlight": {"type": "fvh", "fields": {"title": {}}}}), 400,
               "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err) == (
        "the field [title] should be indexed with term vector with position offsets to be used with fast vector highlighter"
    )


def test_highlight_unknown_type(client, corpus):
    err = _err(_search(client, corpus, {"query": LAZY, "highlight": {"type": "bogus", "fields": {"body": {}}}}), 400,
               "search_phase_execution_exception")
    assert _root_reason(err) == "unknown highlighter type [bogus] for the field [body]"


@pytest.mark.parametrize("level", ["global", "field"])
def test_highlight_custom_tags(client, corpus, level):
    tags = {"pre_tags": ["["], "post_tags": ["]"]}
    hl = {**tags, "fields": {"body": {}}} if level == "global" else {"fields": {"body": tags}}
    assert _hl(client, corpus, LAZY, hl) == [
        ("2", {"body": ["a [lazy] dog sleeps all day long"]}),
        ("1", {"body": ["the quick brown fox jumps over the [lazy] dog"]}),
    ]


def test_highlight_styled_tags_schema(client, corpus):
    res = _hl(client, corpus, LAZY, {"tags_schema": "styled", "fields": {"body": {}}})
    assert res[0][1] == {"body": ['a <em class="hlt1">lazy</em> dog sleeps all day long']}


def test_highlight_unified_uses_first_tag_for_all_terms(client, corpus):
    res = _hl(client, corpus, {"match": {"body": "lazy dog"}},
              {"pre_tags": ["<1>", "<2>"], "post_tags": ["</1>", "</2>"], "fields": {"body": {}}})
    assert res[0][1] == {"body": ["a <1>lazy</1> <1>dog</1> sleeps all day long"]}


def test_highlight_pre_tags_without_post_tags(client, corpus):
    err = _err(_search(client, corpus, {"query": LAZY, "highlight": {"fields": {"body": {"pre_tags": ["<x>"]}}}}), 400,
               "x_content_parse_exception", "parsing_exception")
    assert _root_reason(err) == "pre_tags are set but post_tags are not set"


@pytest.mark.parametrize(
    "hl_type,expected",
    [
        ("unified", [("3", ["foxes are <em>quick</em>"]), ("1", ["the <em>quick</em> brown"])]),
        ("plain", [("3", [" are <em>quick</em>"]), ("1", ["the <em>quick</em>"])]),
    ],
)
def test_highlight_fragment_size(client, corpus, hl_type, expected):
    res = _hl(client, corpus, {"match": {"body": "quick"}},
              {"fields": {"body": {"type": hl_type, "fragment_size": 10}}})
    assert [(i, h["body"]) for i, h in res] == expected


@pytest.mark.parametrize("hl_type", ["unified", "plain", "fvh"])
def test_highlight_number_of_fragments_zero_returns_whole_field(client, corpus, hl_type):
    res = _hl(client, corpus, {"match": {"body": "quick"}},
              {"fields": {"body": {"type": hl_type, "number_of_fragments": 0, "fragment_size": 5}}})
    assert res == [
        ("3", {"body": ["red foxes are <em>quick</em> and clever animals"]}),
        ("1", {"body": ["the <em>quick</em> brown fox jumps over the lazy dog"]}),
    ]


def test_highlight_no_match_size(client, corpus):
    res = _hl(client, corpus, {"match": {"title": "fox"}}, {"fields": {"body": {"no_match_size": 9}}})
    assert res == [("1", {"body": ["the quick"]}), ("3", {"body": ["red foxes"]})]


def test_highlight_require_field_match_default_true(client, corpus):
    res = _hl(client, corpus, {"match": {"title": "fox"}}, {"fields": {"body": {}, "title": {}}})
    assert res == [("1", {"title": ["quick brown <em>fox</em>"]}), ("3", {"title": ["quick red <em>fox</em>"]})]


def test_highlight_require_field_match_false(client, corpus):
    res = _hl(client, corpus, {"match": {"title": "fox"}},
              {"require_field_match": False, "fields": {"body": {}, "title": {}}})
    assert res == [
        ("1", {"body": ["the quick brown <em>fox</em> jumps over the lazy dog"], "title": ["quick brown <em>fox</em>"]}),
        ("3", {"title": ["quick red <em>fox</em>"]}),
    ]


def test_highlight_require_field_match_false_with_no_match_size(client, corpus):
    res = _hl(client, corpus, {"match": {"title": "fox"}},
              {"require_field_match": False, "fields": {"body": {"no_match_size": 9}}})
    assert res == [("1", {"body": ["the quick brown <em>fox</em> jumps over the lazy dog"]}),
                   ("3", {"body": ["red foxes"]})]


def test_highlight_highlight_query(client, corpus):
    res = _hl(client, corpus, LAZY, {"fields": {"body": {"highlight_query": {"match": {"body": "dog"}}}}})
    assert res == [
        ("2", {"body": ["a lazy <em>dog</em> sleeps all day long"]}),
        ("1", {"body": ["the quick brown fox jumps over the lazy <em>dog</em>"]}),
    ]


def test_highlight_wildcard_fields(client, corpus):
    q = {"query_string": {"query": "fox"}}
    assert _hl(client, corpus, q, {"fields": {"t*": {}}}) == [
        ("1", {"title": ["quick brown <em>fox</em>"]}),
        ("3", {"title": ["quick red <em>fox</em>"]}),
    ]
    star = _hl(client, corpus, q, {"fields": {"*": {}}})
    assert star[0] == ("1", {"title": ["quick brown <em>fox</em>"],
                             "body": ["the quick brown <em>fox</em> jumps over the lazy dog"]})


def test_highlight_fields_as_list_of_objects(client, corpus):
    assert _hl(client, corpus, LAZY, {"fields": [{"body": {}}]}) == LAZY_EXPECTED


def test_highlight_keyword_field(client, corpus):
    assert _hl(client, corpus, {"term": {"tag": "a"}}, {"fields": {"tag": {}}}) == [
        ("1", {"tag": ["<em>a</em>"]}), ("3", {"tag": ["<em>a</em>"]})
    ]


def test_highlight_absent_when_nothing_to_highlight(client, corpus):
    assert _hl(client, corpus, {"term": {"n": 1}}, {"fields": {"n": {}}}) == [("1", None)]
    assert _hl(client, corpus, {"match": {"title": "fox"}}, {"fields": {"title.raw": {}}}) == [("1", None), ("3", None)]
    assert _hl(client, corpus, LAZY, {"fields": {"zzz": {}}}) == [("2", None), ("1", None)]


@pytest.mark.parametrize("hl_type", ["unified", "plain"])
def test_highlight_phrase_marks_each_term(client, corpus, hl_type):
    res = _hl(client, corpus, {"match_phrase": {"body": "lazy dog"}}, {"fields": {"body": {"type": hl_type}}})
    assert res[0] == ("2", {"body": ["a <em>lazy</em> <em>dog</em> sleeps all day long"]})


def test_highlight_encoder_html(client, index_factory):
    name = index_factory(settings=SINGLE, mappings={"properties": {"body": {"type": "text"}}})
    _bulk_ids(client, name, {"1": {"body": "a <tag> & lazy cat"}})
    assert _hl(client, name, {"match": {"body": "cat"}}, {"encoder": "html", "fields": {"body": {}}}) == [
        ("1", {"body": ["a &lt;tag&gt; &amp; lazy <em>cat</em>"]})
    ]
    assert _hl(client, name, {"match": {"body": "cat"}}, {"fields": {"body": {}}}) == [
        ("1", {"body": ["a <tag> & lazy <em>cat</em>"]})
    ]


@pytest.fixture
def long_text(client, index_factory):
    name = index_factory(settings=SINGLE, mappings={"properties": {
        "body": {"type": "text", "term_vector": "with_positions_offsets"}, "t2": {"type": "text"}}})
    _bulk_ids(client, name, {"1": {
        "body": "alpha one. filler words go here to pad things. alpha alpha alpha two. "
                "more filler text is here again and again.",
        "t2": "alpha"}})
    return name


@pytest.mark.parametrize(
    "hl,expected",
    [
        ({"fields": {"body": {"fragment_size": 20, "number_of_fragments": 2}}},
         ["<em>alpha</em> one. filler words", "<em>alpha</em> <em>alpha</em> two. more"]),
        ({"order": "score", "fields": {"body": {"fragment_size": 20, "number_of_fragments": 2}}},
         ["<em>alpha</em> <em>alpha</em> two. more", "<em>alpha</em> one. filler words"]),
        ({"fields": {"body": {"fragment_size": 20, "number_of_fragments": 1}}},
         ["<em>alpha</em> <em>alpha</em> two. more"]),
        ({"fields": {"body": {"type": "fvh", "fragment_size": 20, "number_of_fragments": 2}}},
         ["<em>alpha</em> one. filler words", "things. <em>alpha</em> <em>alpha</em> alpha"]),
        ({"order": "score", "fields": {"body": {"type": "fvh", "fragment_size": 20, "number_of_fragments": 2}}},
         ["things. <em>alpha</em> <em>alpha</em> alpha", "<em>alpha</em> one. filler words"]),
    ],
)
def test_highlight_fragment_order(client, long_text, hl, expected):
    res = _hl(client, long_text, {"match": {"body": "alpha"}}, hl)
    assert res == [("1", {"body": expected})]


def test_highlight_fvh_fragment_size_too_small(client, long_text):
    err = _err(_search(client, long_text, {"query": {"match": {"body": "two"}}, "highlight": {
        "fields": {"body": {"type": "fvh", "boundary_scanner": "sentence", "fragment_size": 5}}}}), 400,
        "search_phase_execution_exception")
    assert _root_reason(err) == "fragCharSize(5) is too small. It must be 18 or higher."


@pytest.mark.parametrize("scanner,expected", [("sentence", ["alpha <em>two</em>"]), ("word", ["<em>two</em>"])])
def test_highlight_boundary_scanner_unified(client, long_text, scanner, expected):
    res = _hl(client, long_text, {"match": {"body": "two"}},
              {"fields": {"body": {"boundary_scanner": scanner, "fragment_size": 5}}})
    assert res == [("1", {"body": expected})]


def test_highlight_per_field_require_field_match(client, long_text):
    assert _hl(client, long_text, {"match": {"body": "alpha"}}, {"fields": {"t2": {}}}) == [("1", None)]
    assert _hl(client, long_text, {"match": {"body": "alpha"}},
               {"fields": {"t2": {"require_field_match": False}}}) == [("1", {"t2": ["<em>alpha</em>"]})]


def test_highlight_max_analyzed_offset_typo_hint(client, corpus):
    err = _err(_search(client, corpus, {"query": LAZY, "highlight": {"max_analyzed_offset": 5, "fields": {"body": {}}}}),
               400, "x_content_parse_exception")
    assert err["reason"].endswith("[highlight] unknown field [max_analyzed_offset] did you mean [max_analyzer_offset]?")


# ---------------------------------------------------------------------------
# _source / stored_fields / docvalue_fields / fields / script_fields
# ---------------------------------------------------------------------------

Q1 = {"query": {"ids": {"values": ["1"]}}}


def _hit1(client, index, extra, **params):
    return _s(client, index, {**Q1, **extra}, **params)["hits"]["hits"][0]


@pytest.mark.parametrize(
    "src,expected",
    [
        (["tag", "obj.*"], {"obj": {"a": "p", "b": 1}, "tag": "a"}),
        ({"includes": ["items.name", "n"]}, {"items": [{"name": "x"}, {"name": "y"}], "n": 1}),
        ({"includes": ["*"], "excludes": ["body", "items", "loc", "title", "nums"]},
         {"date": "2024-01-01T00:00:00Z", "price": 1.5, "obj": {"a": "p", "b": 1}, "tag": "a", "n": 1}),
        ({"excludes": ["*"]}, {}),
        ("obj.a", {"obj": {"a": "p"}}),
        (["nope"], {}),
        ({"includes": ["obj"], "excludes": ["obj.b"]}, {"obj": {"a": "p"}}),
        (True, DOCS["1"]),
    ],
)
def test_source_filtering(client, corpus, src, expected):
    assert _hit1(client, corpus, {"_source": src})["_source"] == expected


def test_source_false_removes_source_key(client, corpus):
    hit = _hit1(client, corpus, {"_source": False})
    assert set(hit) == {"_index", "_id", "_score"}


@pytest.mark.parametrize("sf", ["_none_", ["_none_"]])
def test_stored_fields_none_drops_id_and_source(client, corpus, sf):
    hit = _hit1(client, corpus, {"stored_fields": sf})
    assert hit == {"_index": corpus, "_score": 1.0}


@pytest.mark.parametrize("sf", [["tag"], []])
def test_stored_fields_list_drops_source(client, corpus, sf):
    hit = _hit1(client, corpus, {"stored_fields": sf})
    assert hit == {"_index": corpus, "_id": "1", "_score": 1.0}


def test_stored_fields_none_url_param(client, corpus):
    hit = _s(client, corpus, q="_id:1", stored_fields="_none_")["hits"]["hits"][0]
    assert hit == {"_index": corpus, "_score": 1.0}


def test_stored_fields_none_with_source_true_is_500(client, corpus):
    err = _err(_search(client, corpus, {**Q1, "stored_fields": "_none_", "_source": True}), 500,
               "search_phase_execution_exception", "search_exception")
    assert _root_reason(err) == "[stored_fields] cannot be disabled if [_source] is requested"


def test_stored_fields_returns_stored_values(client, index_factory):
    name = index_factory(settings=SINGLE, mappings={"properties": {
        "s": {"type": "keyword", "store": True}, "u": {"type": "keyword"}}})
    _bulk_ids(client, name, {"1": {"s": "stored", "u": "unstored"}})
    hit = _s(client, name, {"stored_fields": ["s", "u"]})["hits"]["hits"][0]
    assert hit["fields"] == {"s": ["stored"]}
    assert "_source" not in hit


def test_stored_fields_none_with_docvalue_fields(client, corpus):
    hit = _hit1(client, corpus, {"stored_fields": "_none_", "docvalue_fields": ["tag"]})
    assert hit == {"_index": corpus, "_score": 1.0, "fields": {"tag": ["a"]}}


def test_docvalue_fields_with_formats(client, corpus):
    hit = _hit1(client, corpus, {"_source": False, "docvalue_fields": [
        "tag", "n", "nums", {"field": "date", "format": "yyyy-MM-dd"}, {"field": "price", "format": "0.00"}]})
    assert hit["fields"] == {"date": ["2024-01-01"], "price": ["1.50"], "tag": ["a"], "n": [1], "nums": [3, 10]}


@pytest.mark.parametrize(
    "spec,expected",
    [
        ("date", {"date": ["2024-01-01T00:00:00.000Z"]}),
        ({"field": "date", "format": "epoch_millis"}, {"date": ["1704067200000"]}),
        ({"field": "n", "format": "use_field_mapping"}, {"n": [1]}),
        ("loc", {"loc": ["0.0, 0.0"]}),
        ("price", {"price": [1.5]}),
        ("obj.*", {"obj.a": ["p"], "obj.b": [1]}),
        ("title.raw", {"title.raw": ["quick brown fox"]}),
    ],
)
def test_docvalue_fields_variants(client, corpus, spec, expected):
    assert _hit1(client, corpus, {"_source": False, "docvalue_fields": [spec]})["fields"] == expected


def test_docvalue_fields_unmapped_is_silently_empty(client, corpus):
    hit = _hit1(client, corpus, {"_source": False, "docvalue_fields": ["nope"]})
    assert "fields" not in hit


def test_docvalue_fields_on_text_rejected(client, corpus):
    err = _err(_search(client, corpus, {**Q1, "docvalue_fields": ["title"]}), 400,
               "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err).startswith("Text fields are not optimised")


def test_docvalue_fields_url_param(client, corpus):
    hit = _s(client, corpus, q="_id:1", docvalue_fields="tag,n", _source="false")["hits"]["hits"][0]
    assert hit["fields"] == {"tag": ["a"], "n": [1]}


def test_fields_api_basic(client, corpus):
    hit = _hit1(client, corpus, {"_source": False, "fields": [
        "tag", "n", "nums", "title", {"field": "date", "format": "yyyy/MM/dd"}, "obj.*", "loc"]})
    assert hit["fields"] == {
        "date": ["2024/01/01"], "obj.a": ["p"], "obj.b": [1],
        "loc": [{"coordinates": [0.0, 0.0], "type": "Point"}],
        "tag": ["a"], "title": ["quick brown fox"], "n": [1], "nums": [3, 10],
    }
    assert "_source" not in hit


def test_fields_api_wildcard_prefix(client, corpus):
    hit = _hit1(client, corpus, {"_source": False, "fields": ["t*"]})
    assert hit["fields"] == {"title.raw": ["quick brown fox"], "tag": ["a"], "title": ["quick brown fox"]}


def test_fields_api_star_flattens_nested(client, corpus):
    hit = _hit1(client, corpus, {"_source": False, "fields": ["*"]})
    assert hit["fields"]["items.name"] == ["x", "y"]
    assert hit["fields"]["items.qty"] == [5, 1]
    assert "items" not in hit["fields"]
    assert hit["fields"]["price"] == [1.5]
    assert hit["fields"]["body"] == ["the quick brown fox jumps over the lazy dog"]


@pytest.mark.parametrize(
    "spec,expected",
    [
        (["items.name"], {"items.name": ["x", "y"]}),
        ([{"field": "loc", "format": "wkt"}], {"loc": ["POINT (0.0 0.0)"]}),
        ([{"field": "date", "format": "epoch_millis"}], {"date": ["1704067200000"]}),
        (["price", "nums", "obj"], {"price": [1.5], "nums": [3, 10]}),
    ],
)
def test_fields_api_variants(client, corpus, spec, expected):
    assert _hit1(client, corpus, {"_source": False, "fields": spec})["fields"] == expected


def test_fields_api_unmapped_returns_nothing(client, corpus):
    assert "fields" not in _hit1(client, corpus, {"_source": False, "fields": ["nope"]})


def test_fields_api_format_on_integer_rejected(client, corpus):
    err = _err(_search(client, corpus, {**Q1, "fields": [{"field": "n", "format": "bogus"}]}), 400,
               "search_phase_execution_exception")
    assert _root_reason(err) == "Field [n] of type [integer] doesn't support formats."


def test_fields_api_include_unmapped_not_supported(client, corpus):
    err = _err(_search(client, corpus, {**Q1, "fields": [{"field": "*", "include_unmapped": True}]}), 400,
               "x_content_parse_exception")
    assert err["reason"].endswith("[docvalues_field] unknown field [include_unmapped]")


def test_fields_api_ignores_unmapped_source_fields(client, index_factory):
    name = index_factory(settings=SINGLE, mappings={"dynamic": False, "properties": {"a": {"type": "keyword"}}})
    client.put(f"/{name}/_doc/1", params={"refresh": "true"}, json={"a": "v", "un": "hidden"})
    hit = _s(client, name, {"fields": ["*"], "_source": False})["hits"]["hits"][0]
    assert hit["fields"] == {"a": ["v"]}
    hit = _s(client, name, {"fields": ["un"], "_source": False})["hits"]["hits"][0]
    assert "fields" not in hit


def test_script_fields(client, corpus):
    hit = _hit1(client, corpus, {"script_fields": {
        "dbl": {"script": {"source": "doc['n'].value * 2"}},
        "p": {"script": {"source": "params.x + doc['tag'].value", "params": {"x": "T-"}}},
    }})
    assert hit["fields"] == {"p": ["T-a"], "dbl": [2]}
    assert "_source" not in hit


def test_script_fields_with_source_requested(client, corpus):
    hit = _hit1(client, corpus, {"script_fields": {"dbl": {"script": "doc['n'].value * 2"}}, "_source": ["n"]})
    assert hit["_source"] == {"n": 1}
    assert hit["fields"] == {"dbl": [2]}


@pytest.mark.parametrize(
    "script,expected",
    [
        ({"source": "params['_source']['title']"}, ["quick brown fox"]),
        ({"source": "doc['nums']"}, [3, 10]),
        ({"source": "null"}, [None]),
        ({"source": "[1,2]"}, [1, 2]),
        ({"source": "['a':1]"}, [{"a": 1}]),
        ({"source": "doc['price'].value"}, [1.5]),
    ],
)
def test_script_fields_value_shapes(client, corpus, script, expected):
    assert _hit1(client, corpus, {"_source": False, "script_fields": {"x": {"script": script}}})["fields"] == {"x": expected}


def test_script_fields_per_doc(client, corpus):
    res = _s(client, corpus, {"_source": False, "sort": ["n"],
                              "script_fields": {"x": {"script": "doc['nums'].size()"}}})
    assert [h["fields"]["x"] for h in res["hits"]["hits"]] == [[2], [2], [1], [0], [0]]


def test_script_fields_runtime_error(client, corpus):
    err = _err(_search(client, corpus, {**Q1, "script_fields": {"bad": {"script": "doc['nope'].value"}}}), 400,
               "search_phase_execution_exception", "script_exception")
    assert err["root_cause"][0]["lang"] == "painless"


def test_mixed_fields_sources_merge(client, corpus):
    hit = _hit1(client, corpus, {"_source": False, "fields": ["tag"], "docvalue_fields": ["n"],
                                 "script_fields": {"s": {"script": "1"}}})
    assert hit["fields"] == {"s": [1], "tag": ["a"], "n": [1]}


# ---------------------------------------------------------------------------
# explain / version / seq_no_primary_term
# ---------------------------------------------------------------------------


def _check_explanation(e):
    assert set(e) == {"value", "description", "details"}
    assert isinstance(e["description"], str)
    for d in e["details"]:
        _check_explanation(d)


def test_explain_true_shape(client, corpus):
    hit = _hit1(client, corpus, {"explain": True, "_source": False})
    assert hit["_shard"] == f"[{corpus}][0]"
    assert isinstance(hit["_node"], str) and hit["_node"]
    assert hit["_explanation"]["value"] == hit["_score"]
    _check_explanation(hit["_explanation"])


def test_explain_url_param_with_match(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}}, explain="true")
    for h in res["hits"]["hits"]:
        assert abs(h["_explanation"]["value"] - h["_score"]) < 1e-6
        assert "weight(title:fox" in json.dumps(h["_explanation"])


def test_explain_with_sort_has_null_score(client, corpus):
    res = _s(client, corpus, {"sort": [{"n": "asc"}], "explain": True, "size": 2})
    assert [(h["_id"], h["_score"], "_explanation" in h) for h in res["hits"]["hits"]] == [
        ("1", None, True), ("2", None, True)
    ]


def test_explain_false_has_no_extra_keys(client, corpus):
    hit = _hit1(client, corpus, {"explain": False})
    assert "_explanation" not in hit and "_shard" not in hit and "_node" not in hit


def test_version_and_seq_no(client, corpus):
    hit = _hit1(client, corpus, {"version": True, "seq_no_primary_term": True, "_source": False})
    assert hit == {"_index": corpus, "_id": "1", "_version": 1, "_seq_no": 0, "_primary_term": 1, "_score": 1.0}


def test_version_and_seq_no_url_params(client, corpus):
    hit = _s(client, corpus, q="_id:2", version="true", seq_no_primary_term="true", _source="false")["hits"]["hits"][0]
    assert (hit["_version"], hit["_seq_no"], hit["_primary_term"]) == (1, 1, 1)


def test_version_after_update_and_noop(client, index_factory):
    name = index_factory(settings=SINGLE, mappings=MAPPING)
    _bulk_ids(client, name, DOCS)
    client.post(f"/{name}/_update/1", params={"refresh": "true"}, json={"doc": {"n": 1}})  # noop
    client.post(f"/{name}/_update/2", params={"refresh": "true"}, json={"doc": {"n": 22}})
    res = _s(client, name, {"query": {"ids": {"values": ["1", "2"]}}, "version": True,
                            "seq_no_primary_term": True, "_source": False, "sort": ["_id"]})
    assert [(h["_id"], h["_version"], h["_seq_no"], h["_primary_term"]) for h in res["hits"]["hits"]] == [
        ("1", 1, 0, 1), ("2", 2, 5, 1)
    ]


def test_no_version_by_default(client, corpus):
    hit = _hit1(client, corpus, {})
    assert "_version" not in hit and "_seq_no" not in hit and "_primary_term" not in hit


# ---------------------------------------------------------------------------
# collapse
# ---------------------------------------------------------------------------


def test_collapse_on_keyword(client, corpus):
    res = _s(client, corpus, {"collapse": {"field": "tag"}, "sort": ["n"]})
    assert [(h["_id"], h["fields"], h["sort"]) for h in res["hits"]["hits"]] == [
        ("1", {"tag": ["a"]}, [1]), ("2", {"tag": ["b"]}, [2]), ("4", {"tag": ["c"]}, [4]), ("5", {"tag": ["B"]}, [5])
    ]
    # total counts documents, not groups
    assert res["hits"]["total"] == {"value": 5, "relation": "eq"}


def test_collapse_numeric_field_with_missing(client, corpus):
    res = _s(client, corpus, {"collapse": {"field": "price"}, "sort": ["n"]})
    assert _ids(res) == ["1", "2", "3", "4", "5"]
    assert "fields" not in res["hits"]["hits"][4]
    assert res["hits"]["hits"][0]["fields"] == {"price": [1.5]}


def test_collapse_inner_hits(client, corpus):
    res = _s(client, corpus, {"collapse": {"field": "tag", "inner_hits": {
        "name": "ih", "size": 5, "sort": [{"n": "desc"}], "_source": False}}, "sort": ["n"]})
    first = res["hits"]["hits"][0]
    assert first["inner_hits"]["ih"]["hits"] == {
        "total": {"value": 2, "relation": "eq"}, "max_score": None,
        "hits": [{"_index": corpus, "_id": "3", "_score": None, "sort": [3]},
                 {"_index": corpus, "_id": "1", "_score": None, "sort": [1]}],
    }


def test_collapse_multiple_inner_hits_and_max_concurrent(client, corpus):
    res = _s(client, corpus, {"collapse": {"field": "tag", "max_concurrent_group_searches": 2, "inner_hits": [
        {"name": "a", "size": 1}, {"name": "b", "size": 2, "sort": ["n"]}]}, "sort": ["n"], "_source": False})
    got = [(h["_id"], {k: _ids(v) for k, v in h["inner_hits"].items()}) for h in res["hits"]["hits"]]
    assert got == [("1", {"a": ["1"], "b": ["1", "3"]}), ("2", {"a": ["2"], "b": ["2"]}),
                   ("4", {"a": ["4"], "b": ["4"]}), ("5", {"a": ["5"], "b": ["5"]})]


def test_collapse_inner_hits_requires_name(client, corpus):
    err = _err(_search(client, corpus, {"collapse": {"field": "tag", "inner_hits": {"size": 1}}}), 400,
               "illegal_argument_exception")
    assert err["reason"] == "Field name cannot be null"


def test_collapse_with_search_after(client, corpus):
    res = _s(client, corpus, {"collapse": {"field": "tag"}, "sort": ["tag"], "search_after": ["a"]})
    assert _id_sorts(res) == [("2", ["b"]), ("4", ["c"])]


def test_collapse_on_text_rejected(client, corpus):
    err = _err(_search(client, corpus, {"collapse": {"field": "title"}}), 400, "search_phase_execution_exception")
    assert _root_reason(err) == "unknown type for collapse field `title`, only keywords and numbers are accepted"


def test_collapse_unmapped_rejected(client, corpus):
    err = _err(_search(client, corpus, {"collapse": {"field": "nope"}}), 400, "search_phase_execution_exception")
    assert _root_reason(err) == "no mapping found for `nope` in order to collapse on"


def test_collapse_multivalued_is_500(client, corpus):
    err = _err(_search(client, corpus, {"collapse": {"field": "nums"}}), 500,
               "search_phase_execution_exception", "illegal_state_exception")
    assert _root_reason(err).endswith("the collapse field must be single valued")


def test_collapse_with_rescore_is_500(client, corpus):
    err = _err(_search(client, corpus, {"collapse": {"field": "tag"},
                                        "rescore": {"query": {"rescore_query": {"match_all": {}}}}}), 500,
               "search_phase_execution_exception", "search_exception")
    assert _root_reason(err) == "cannot use `collapse` in conjunction with `rescore`"


# ---------------------------------------------------------------------------
# rescore
# ---------------------------------------------------------------------------


def _scores(res):
    return [(h["_id"], h["_score"]) for h in res["hits"]["hits"]]


CS2 = {"constant_score": {"filter": {"match_all": {}}, "boost": 2}}
CS_A3 = {"constant_score": {"filter": {"term": {"tag": "a"}}, "boost": 3}}


@pytest.mark.parametrize(
    "score_mode,expected",
    [
        (None, [("1", 5.0), ("3", 5.0), ("2", 2.0), ("4", 2.0), ("5", 2.0)]),
        ("total", [("1", 5.0), ("3", 5.0), ("2", 2.0), ("4", 2.0), ("5", 2.0)]),
        ("multiply", [("1", 6.0), ("3", 6.0), ("2", 2.0), ("4", 2.0), ("5", 2.0)]),
        ("avg", [("1", 2.5), ("3", 2.5), ("2", 2.0), ("4", 2.0), ("5", 2.0)]),
        ("max", [("1", 3.0), ("3", 3.0), ("2", 2.0), ("4", 2.0), ("5", 2.0)]),
        ("min", [("1", 2.0), ("2", 2.0), ("3", 2.0), ("4", 2.0), ("5", 2.0)]),
    ],
)
def test_rescore_score_modes(client, corpus, score_mode, expected):
    rq = {"rescore_query": CS_A3}
    if score_mode:
        rq["score_mode"] = score_mode
    res = _s(client, corpus, {"query": CS2, "rescore": {"window_size": 10, "query": rq}})
    assert _scores(res) == expected


def test_rescore_weights(client, corpus):
    res = _s(client, corpus, {"query": CS2, "rescore": {"window_size": 10, "query": {
        "rescore_query": CS_A3, "query_weight": 0.5, "rescore_query_weight": 2}}})
    assert _scores(res) == [("1", 7.0), ("3", 7.0), ("2", 1.0), ("4", 1.0), ("5", 1.0)]


def test_rescore_query_weight_zero(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox brown"}}, "rescore": {"window_size": 10, "query": {
        "rescore_query": {"constant_score": {"filter": {"term": {"tag": "a"}}}},
        "query_weight": 0, "rescore_query_weight": 1}}})
    assert _scores(res) == [("1", 1.0), ("3", 1.0), ("2", 0.0)]


def test_rescore_window_size_limits_rescored_docs(client, corpus):
    body = {"query": {"constant_score": {"filter": {"range": {"n": {"lte": 3}}}}}, "sort": []}
    base = _s(client, corpus, body)
    assert _ids(base) == ["1", "2", "3"]
    res = _s(client, corpus, {**body, "rescore": {"window_size": 1, "query": {
        "rescore_query": {"constant_score": {"filter": {"term": {"n": 3}}, "boost": 100}}}}})
    # doc 3 is outside the window: it is not boosted
    assert _scores(res) == [("1", 1.0), ("2", 1.0), ("3", 1.0)]
    res = _s(client, corpus, {**body, "rescore": {"window_size": 3, "query": {
        "rescore_query": {"constant_score": {"filter": {"term": {"n": 3}}, "boost": 100}}}}})
    assert _scores(res)[0] == ("3", 101.0)


def test_rescore_chain(client, corpus):
    res = _s(client, corpus, {"query": {"constant_score": {"filter": {"match_all": {}}}}, "rescore": [
        {"window_size": 10, "query": {"rescore_query": CS_A3}},
        {"window_size": 10, "query": {"rescore_query": {"constant_score": {"filter": {"term": {"n": 4}}, "boost": 10}}}},
    ]})
    assert _scores(res) == [("4", 11.0), ("1", 4.0), ("3", 4.0), ("2", 1.0), ("5", 1.0)]


def test_rescore_with_sort_rejected(client, corpus):
    err = _err(_search(client, corpus, {"sort": ["n"], "rescore": {"query": {"rescore_query": {"match_all": {}}}}}),
               400, "search_phase_execution_exception", "illegal_argument_exception")
    assert _root_reason(err) == "Cannot use [sort] option in conjunction with [rescore]."


# ---------------------------------------------------------------------------
# terminate_after / timeout / profile
# ---------------------------------------------------------------------------


def test_terminate_after_hit(client, corpus):
    res = _s(client, corpus, {"terminate_after": 2})
    assert res["terminated_early"] is True
    assert list(res)[:3] == ["took", "timed_out", "terminated_early"]
    assert res["hits"]["total"] == {"value": 2, "relation": "eq"}
    assert len(res["hits"]["hits"]) == 2


def test_terminate_after_not_reached(client, corpus):
    res = _s(client, corpus, {"terminate_after": 100})
    assert res["terminated_early"] is False
    assert res["hits"]["total"]["value"] == 5


def test_terminate_after_url_param(client, corpus):
    res = _s(client, corpus, terminate_after="1")
    assert res["terminated_early"] is True
    assert res["hits"]["total"]["value"] == 1


def test_terminate_after_zero_is_disabled(client, corpus):
    res = _s(client, corpus, {"terminate_after": 0})
    assert "terminated_early" not in res
    assert res["hits"]["total"]["value"] == 5


def test_count_terminate_after(client, corpus):
    res = _ok(client.post(f"/{corpus}/_count", json={"query": {"term": {"tag": "a"}}}, params={"terminate_after": "1"}))
    assert res["terminated_early"] is True


@pytest.mark.parametrize("where", ["body", "url"])
def test_timeout_harmless(client, corpus, where):
    res = _s(client, corpus, {"timeout": "10s"}) if where == "body" else _s(client, corpus, timeout="5m")
    assert res["timed_out"] is False
    assert res["hits"]["total"]["value"] == 5


def test_timeout_unparseable(client, corpus):
    err = _err(_search(client, corpus, timeout="bogus"), 400, "illegal_argument_exception")
    assert err["reason"] == "failed to parse [bogus]"


def test_profile_shape(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}, "profile": True})
    assert list(res["profile"]) == ["shards"]
    shard = res["profile"]["shards"][0]
    assert shard["id"].endswith(f"[{corpus}][0]")
    assert {"id", "searches", "aggregations", "fetch"} <= set(shard)
    search = shard["searches"][0]
    assert {"query", "rewrite_time", "collector"} <= set(search)
    q = search["query"][0]
    assert q["type"] == "TermQuery"
    assert q["description"] == "title:fox"
    assert isinstance(q["time_in_nanos"], int)
    assert {"create_weight", "build_scorer", "next_doc", "score", "advance", "match"} <= set(q["breakdown"])
    coll = search["collector"][0]
    assert {"name", "reason", "time_in_nanos"} <= set(coll)
    assert shard["aggregations"] == []
    assert shard["fetch"][0]["type"] == "fetch"


def test_profile_bool_children_and_aggs(client, corpus):
    res = _s(client, corpus, {"query": {"bool": {"must": [{"match": {"title": "fox"}}, {"term": {"tag": "a"}}]}},
                              "profile": True, "aggs": {"t": {"terms": {"field": "tag"}}}})
    shard = res["profile"]["shards"][0]
    q = shard["searches"][0]["query"][0]
    assert q["type"] == "BooleanQuery"
    assert q["description"] == "+title:fox +ConstantScore(tag:a)"
    assert [c["type"] for c in q["children"]] == ["TermQuery", "ConstantScoreQuery"]
    assert shard["aggregations"][0]["description"] == "t"
    assert "breakdown" in shard["aggregations"][0]


def test_profile_false_has_no_profile(client, corpus):
    assert "profile" not in _s(client, corpus, {"profile": False})


# ---------------------------------------------------------------------------
# _msearch
# ---------------------------------------------------------------------------


def test_msearch_basic_with_per_request_error(client, corpus):
    missing = _uid("sa-missing")
    r = _msearch(client, [
        {"index": corpus}, {"query": {"term": {"tag": "a"}}, "size": 0},
        {"index": missing}, {"query": {"match_all": {}}},
        {}, {"size": 0},
    ], path=f"/{corpus}/_msearch")
    res = _ok(r)
    assert set(res) == {"took", "responses"}
    resp = res["responses"]
    assert len(resp) == 3
    assert resp[0]["status"] == 200 and resp[0]["hits"]["total"]["value"] == 2
    assert resp[1]["status"] == 404
    assert resp[1]["error"]["type"] == "index_not_found_exception"
    assert resp[1]["error"]["reason"] == f"no such index [{missing}]"
    assert resp[2]["status"] == 200 and resp[2]["hits"]["total"]["value"] == 5


def test_msearch_empty_header_uses_path_index(client, corpus):
    r = client.post(f"/{corpus}/_msearch", content=b'{}\n{"query":{"term":{"tag":"a"}},"size":0}\n', headers=ND)
    assert _ok(r)["responses"][0]["hits"]["total"]["value"] == 2


def test_msearch_response_item_shape(client, corpus):
    res = _ok(_msearch(client, [{"index": corpus}, {"size": 1, "_source": False}]))
    item = res["responses"][0]
    assert set(item) == {"took", "timed_out", "_shards", "hits", "status"}
    assert item["hits"]["hits"] == [{"_index": corpus, "_id": "1", "_score": 1.0}]


def test_msearch_shard_failure_is_per_item(client, corpus):
    res = _ok(_msearch(client, [{}, {"sort": [{"title": "asc"}]}, {}, {"size": 0}], path=f"/{corpus}/_msearch"))
    assert res["responses"][0]["status"] == 400
    assert res["responses"][0]["error"]["type"] == "search_phase_execution_exception"
    assert res["responses"][1]["status"] == 200


def test_msearch_parse_error_fails_whole_request(client, corpus):
    err = _err(_msearch(client, [{"index": corpus}, {"query": {"bogus": {}}}]), 400, "parsing_exception")
    assert err["reason"] == "unknown query [bogus]"


def test_msearch_negative_from_fails_whole_request(client, corpus):
    err = _err(_msearch(client, [{}, {"size": 0, "from": -1}, {}, {"size": 0}], path=f"/{corpus}/_msearch"), 400,
               "illegal_argument_exception")
    assert err["reason"] == "[from] parameter cannot be negative, found [-1]"


def test_msearch_header_only_is_no_requests(client, corpus):
    err = _err(_msearch(client, [{"index": corpus}]), 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: no requests added;"


def test_msearch_empty_body(client):
    r = client.post("/_msearch", content=b"\n", headers=ND)
    err = _err(r, 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: no requests added;"


def test_msearch_requires_trailing_newline(client, corpus):
    payload = json.dumps({"index": corpus}) + "\n" + json.dumps({"size": 0})
    err = _err(client.post("/_msearch", content=payload.encode(), headers=ND), 400, "illegal_argument_exception")
    assert err["reason"] == "The msearch request must be terminated by a newline [\n]"


def test_msearch_unknown_header_key(client, corpus):
    err = _err(_msearch(client, [{"index": corpus, "unknownkey": 1}, {"size": 0}]), 400, "illegal_argument_exception")
    assert err["reason"] == "key [unknownkey] is not supported in the metadata section"


def test_msearch_body_in_header_position(client, corpus):
    r = client.post(f"/{corpus}/_msearch", content=b'\n{"size":0}\n', headers=ND)
    err = _err(r, 400, "illegal_argument_exception")
    assert err["reason"] == "key [size] is not supported in the metadata section"


def test_msearch_accepts_application_json(client, corpus):
    payload = json.dumps({"index": corpus}) + "\n" + json.dumps({"size": 0}) + "\n"
    r = client.post("/_msearch", content=payload.encode(), headers={"Content-Type": "application/json"})
    assert _ok(r)["responses"][0]["hits"]["total"]["value"] == 5


def test_msearch_get_method(client, corpus):
    r = client.request("GET", f"/{corpus}/_msearch", content=b'{}\n{"size":0}\n', headers=ND)
    assert _ok(r)["responses"][0]["hits"]["total"]["value"] == 5


def test_msearch_header_options(client, corpus):
    res = _ok(_msearch(client, [
        {"index": corpus, "search_type": "dfs_query_then_fetch"}, {"size": 0},
        {"index": [corpus, _uid("sa-missing")], "ignore_unavailable": True}, {"size": 0},
    ], max_concurrent_searches="1"))
    assert [r["hits"]["total"]["value"] for r in res["responses"]] == [5, 5]


def test_msearch_rest_total_hits_as_int(client, corpus):
    res = _ok(_msearch(client, [{"index": corpus}, {"size": 0}], rest_total_hits_as_int="true"))
    assert res["responses"][0]["hits"]["total"] == 5


def test_msearch_typed_keys(client, corpus):
    res = _ok(_msearch(client, [{"index": corpus}, {"size": 0, "aggs": {"t": {"terms": {"field": "tag"}}}}],
                       typed_keys="true"))
    assert list(res["responses"][0]["aggregations"]) == ["sterms#t"]


# ---------------------------------------------------------------------------
# _count
# ---------------------------------------------------------------------------


def test_count_shape(client, corpus):
    assert _ok(client.get(f"/{corpus}/_count")) == {
        "count": 5, "_shards": {"total": 1, "successful": 1, "skipped": 0, "failed": 0}
    }


def test_count_with_query(client, corpus):
    assert _ok(client.post(f"/{corpus}/_count", json={"query": {"match": {"title": "fox"}}}))["count"] == 2


@pytest.mark.parametrize("params,expected", [
    ({"q": "tag:a"}, 2),
    ({"q": "quick", "df": "title"}, 2),
    ({"q": "brown fox", "df": "title", "default_operator": "AND"}, 1),
    ({"q": "brown fox", "df": "title"}, 3),
])
def test_count_uri_q(client, corpus, params, expected):
    assert _ok(client.get(f"/{corpus}/_count", params=params))["count"] == expected


def test_count_q_with_body_is_rejected(client, corpus):
    err = _err(client.post(f"/{corpus}/_count", json={"query": {"match_all": {}}}, params={"q": "tag:a"}), 400,
               "illegal_argument_exception")
    assert err["reason"] == f"request [/{corpus}/_count] contains unrecognized parameter: [q]"


def test_count_rejects_size(client, corpus):
    err = _err(client.post(f"/{corpus}/_count", json={"query": {"match_all": {}}, "size": 1}), 400, "parsing_exception")
    assert err["reason"] == "request does not support [size]"


def test_count_on_alias_and_filtered_alias(client, corpus_pair):
    assert _ok(client.get(f"/{corpus_pair['alias']}/_count"))["count"] == 6
    res = _ok(client.get(f"/{corpus_pair['falias']}/_count"))
    assert res["count"] == 2
    assert res["_shards"]["total"] == 1


def test_count_bad_query(client, corpus):
    err = _err(client.post(f"/{corpus}/_count", json={"query": {"bogus": {}}}), 400, "parsing_exception")
    assert err["reason"] == "unknown query [bogus]"


def test_count_type_error(client, corpus):
    err = _err(client.post(f"/{corpus}/_count", json={"query": {"term": {"n": "abc"}}}), 400,
               "search_phase_execution_exception", "query_shard_exception")
    assert _root_reason(err) == 'failed to create query: For input string: "abc"'


def test_count_missing_index(client):
    missing = _uid("sa-missing")
    _err(client.get(f"/{missing}/_count"), 404, "index_not_found_exception")
    assert _ok(client.get(f"/{missing}/_count", params={"ignore_unavailable": "true"})) == {
        "count": 0, "_shards": {"total": 0, "successful": 0, "skipped": 0, "failed": 0}
    }


def test_count_source_param(client, corpus):
    r = client.get(f"/{corpus}/_count", params={"source": json.dumps({"query": {"term": {"tag": "a"}}}),
                                                "source_content_type": "application/json"})
    assert _ok(r)["count"] == 2


def test_count_min_score_param(client, corpus):
    r = client.post(f"/{corpus}/_count", json={"query": {"term": {"tag": "a"}}}, params={"min_score": "1"})
    assert _ok(r)["count"] == 2


# ---------------------------------------------------------------------------
# _validate/query
# ---------------------------------------------------------------------------


def test_validate_valid(client, corpus):
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"match": {"title": "fox"}}})
    assert _ok(r) == {"_shards": {"total": 1, "successful": 1, "failed": 0}, "valid": True}


def test_validate_explain(client, corpus):
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"match": {"title": "the quick fox"}}},
                    params={"explain": "true"})
    assert _ok(r)["explanations"] == [{"index": corpus, "valid": True, "explanation": "title:the title:quick title:fox"}]


def test_validate_uri_q_explain(client, corpus):
    r = client.get(f"/{corpus}/_validate/query", params={"q": "title:fox AND tag:a", "explain": "true"})
    assert _ok(r)["explanations"][0]["explanation"] == "+title:fox +ConstantScore(tag:a)"


def test_validate_unknown_query(client, corpus):
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"bogus": {}}})
    assert _ok(r) == {"valid": False}
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"bogus": {}}}, params={"explain": "true"})
    body = _ok(r)
    assert body["valid"] is False
    assert body["error"].startswith("ParsingException[unknown query [bogus]]")


def test_validate_shard_level_failure(client, corpus):
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"term": {"n": "abc"}}})
    assert _ok(r) == {"_shards": {"total": 1, "successful": 1, "failed": 0}, "valid": False}
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"term": {"n": "abc"}}}, params={"explain": "true"})
    exp = _ok(r)["explanations"][0]
    assert exp["valid"] is False and exp["index"] == corpus
    assert "NumberFormatException" in exp["error"]


def test_validate_uri_syntax_error(client, corpus):
    r = client.get(f"/{corpus}/_validate/query", params={"q": "title:(fox", "explain": "true"})
    body = _ok(r)
    assert body["valid"] is False
    assert "Failed to parse query [title:(fox]" in body["explanations"][0]["error"]


def test_validate_rewrite(client, corpus):
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"fuzzy": {"tag": "a"}}}, params={"rewrite": "true"})
    body = _ok(r)
    assert body["valid"] is True
    assert "tag:a" in body["explanations"][0]["explanation"]


def test_validate_all_shards_includes_shard(client, corpus):
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"match_all": {}}},
                    params={"explain": "true", "all_shards": "true"})
    exp = _ok(r)["explanations"][0]
    assert exp["shard"] == 0 and exp["valid"] is True


def test_validate_extra_body_key_is_invalid(client, corpus):
    r = client.post(f"/{corpus}/_validate/query", json={"query": {"match_all": {}}, "size": 1})
    assert _ok(r) == {"valid": False}


def test_validate_requires_query(client, corpus):
    err = _err(client.post(f"/{corpus}/_validate/query", json={}), 400, "action_request_validation_exception")
    assert err["reason"] == "Validation Failed: 1: query cannot be null;"


def test_validate_missing_index(client):
    _err(client.post(f"/{_uid('sa-missing')}/_validate/query", json={"query": {"match_all": {}}}), 404,
         "index_not_found_exception")


# ---------------------------------------------------------------------------
# Search templates
# ---------------------------------------------------------------------------


def test_search_template_inline_object(client, corpus):
    r = client.post(f"/{corpus}/_search/template", json={
        "source": {"query": {"term": {"tag": "{{t}}"}}, "size": "{{s}}"}, "params": {"t": "a", "s": 1}})
    res = _ok(r)
    assert _ids(res) == ["1"]
    assert res["hits"]["total"] == {"value": 2, "relation": "eq"}


def test_search_template_tojson(client, corpus):
    r = client.post(f"/{corpus}/_search/template", json={
        "source": '{"query":{"terms":{"tag":{{#toJson}}tags{{/toJson}}}}}', "params": {"tags": ["a", "c"]}})
    assert _ids(_ok(r)) == ["1", "3", "4"]


@pytest.mark.parametrize("params,expected", [({}, 5), ({"sz": 2}, 2)])
def test_search_template_conditional_section(client, corpus, params, expected):
    r = client.post(f"/{corpus}/_search/template", json={
        "source": '{"query":{"match_all":{}}{{#sz}},"size":{{sz}}{{/sz}}}', "params": params})
    assert len(_ok(r)["hits"]["hits"]) == expected


def test_search_template_inverted_section_default(client, corpus):
    r = client.post(f"/{corpus}/_search/template", json={
        "source": {"query": {"term": {"tag": "{{t}}{{^t}}c{{/t}}"}}}, "params": {}})
    assert _ids(_ok(r)) == ["4"]


@pytest.mark.parametrize("opt,key", [("explain", "_explanation"), ("profile", "profile")])
def test_search_template_explain_profile(client, corpus, opt, key):
    r = client.post(f"/{corpus}/_search/template", json={
        "source": {"query": {"match": {"title": "{{q}}"}}}, "params": {"q": "fox"}, opt: True})
    res = _ok(r)
    if key == "profile":
        assert "profile" in res
    else:
        assert all(key in h for h in res["hits"]["hits"])


def test_search_template_rest_total_hits_as_int(client, corpus):
    r = client.post(f"/{corpus}/_search/template", json={
        "source": {"query": {"term": {"tag": "{{t}}"}}}, "params": {"t": "a"}}, params={"rest_total_hits_as_int": "true"})
    assert _ok(r)["hits"]["total"] == 2


def test_search_template_bad_mustache(client, corpus):
    err = _err(client.post(f"/{corpus}/_search/template", json={"source": '{"query": {{bad', "params": {}}), 400,
               "script_exception")
    assert err["lang"] == "mustache"
    assert err["caused_by"]["type"] == "mustache_exception"


def test_render_template_inline(client):
    r = client.post("/_render/template", json={
        "source": {"query": {"term": {"tag": "{{t}}"}}, "size": "{{s}}"}, "params": {"t": "a", "s": 1}})
    # numbers substituted into a quoted placeholder stay strings
    assert _ok(r) == {"template_output": {"query": {"term": {"tag": "a"}}, "size": "1"}}


def test_render_template_functions(client):
    r = client.post("/_render/template", json={
        "source": '{"q":"{{x}}","l":{{#toJson}}l{{/toJson}},"j":"{{#join}}l{{/join}}","u":"{{#url}}{{x}}{{/url}}"}',
        "params": {"x": "a b&c", "l": ["p", "q"]}})
    assert _ok(r) == {"template_output": {"q": "a b&c", "l": ["p", "q"], "j": "p,q", "u": "a+b%26c"}}


@pytest.mark.parametrize("params,expected", [({"x": 'a"b'}, 'a"b'), ({}, "")])
def test_render_template_escaping_and_missing_params(client, params, expected):
    r = client.post("/_render/template", json={"source": '{"q":"{{x}}"}', "params": params})
    assert _ok(r) == {"template_output": {"q": expected}}


def test_stored_template_lifecycle(client, corpus, stored_scripts):
    sid = _uid("sa-tpl")
    stored_scripts.append(sid)
    r = client.put(f"/_scripts/{sid}", json={"script": {"lang": "mustache",
                                                        "source": {"query": {"term": {"tag": "{{t}}"}}}}})
    assert _ok(r) == {"acknowledged": True}
    got = _ok(client.get(f"/_scripts/{sid}"))
    assert got["_id"] == sid and got["found"] is True
    assert got["script"]["lang"] == "mustache"
    assert got["script"]["source"] == '{"query":{"term":{"tag":"{{t}}"}}}'

    assert _ok(client.post(f"/_render/template/{sid}", json={"params": {"t": "b"}})) == {
        "template_output": {"query": {"term": {"tag": "b"}}}}
    assert _ok(client.post("/_render/template", json={"id": sid, "params": {"t": "b"}})) == {
        "template_output": {"query": {"term": {"tag": "b"}}}}
    assert _ids(_ok(client.post(f"/{corpus}/_search/template", json={"id": sid, "params": {"t": "b"}}))) == ["2"]

    payload = "\n".join(json.dumps(x) for x in [
        {"index": corpus}, {"id": sid, "params": {"t": "a"}},
        {"index": corpus}, {"source": {"query": {"term": {"tag": "{{t}}"}}}, "params": {"t": "c"}},
        {"index": _uid("sa-missing")}, {"source": {"query": {"match_all": {}}}},
    ]) + "\n"
    res = _ok(client.post("/_msearch/template", content=payload.encode(), headers=ND))
    assert _ids(res["responses"][0]) == ["1", "3"]
    assert _ids(res["responses"][1]) == ["4"]
    assert res["responses"][2]["status"] == 404
    assert res["responses"][2]["error"]["type"] == "index_not_found_exception"

    assert _ok(client.delete(f"/_scripts/{sid}")) == {"acknowledged": True}
    r = client.get(f"/_scripts/{sid}")
    assert r.status_code == 404 and r.json() == {"_id": sid, "found": False}
    err = _err(client.delete(f"/_scripts/{sid}"), 404, "resource_not_found_exception")
    assert err["reason"] == f"stored script [{sid}] does not exist and cannot be deleted"


def test_search_template_unknown_stored_id(client, corpus):
    sid = _uid("sa-nope")
    for r in (client.post(f"/{corpus}/_search/template", json={"id": sid, "params": {}}),
              client.post("/_render/template", json={"id": sid, "params": {}})):
        err = _err(r, 404, "resource_not_found_exception")
        assert err["reason"] == f"unable to find script [{sid}] in cluster state"


# ---------------------------------------------------------------------------
# _field_caps
# ---------------------------------------------------------------------------


def test_field_caps_single_index(client, corpus):
    r = client.get(f"/{corpus}/_field_caps",
                   params={"fields": "tag,n,title,title.raw,obj.a,items.qty,loc,date,_id,obj,items,price"})
    body = _ok(r)
    assert body["indices"] == [corpus]
    f = body["fields"]
    assert f["tag"] == {"keyword": {"type": "keyword", "searchable": True, "aggregatable": True}}
    assert f["title"] == {"text": {"type": "text", "searchable": True, "aggregatable": False}}
    assert f["title.raw"] == {"keyword": {"type": "keyword", "searchable": True, "aggregatable": True}}
    assert f["n"] == {"integer": {"type": "integer", "searchable": True, "aggregatable": True}}
    assert f["price"] == {"float": {"type": "float", "searchable": True, "aggregatable": True}}
    assert f["date"] == {"date": {"type": "date", "searchable": True, "aggregatable": True}}
    assert f["loc"] == {"geo_point": {"type": "geo_point", "searchable": True, "aggregatable": True}}
    assert f["obj"] == {"object": {"type": "object", "searchable": False, "aggregatable": False}}
    assert f["items"] == {"nested": {"type": "nested", "searchable": False, "aggregatable": False}}
    assert f["items.qty"] == {"integer": {"type": "integer", "searchable": True, "aggregatable": True}}
    assert f["_id"] == {"_id": {"type": "_id", "searchable": True, "aggregatable": True}}


def test_field_caps_conflicting_types_across_indices(client, corpus, index_factory):
    other = index_factory(settings=SINGLE, mappings={"properties": {
        "tag": {"type": "text"}, "n": {"type": "long"},
        "only": {"type": "keyword", "index": False}, "nodv": {"type": "keyword", "doc_values": False}}})
    body = _ok(client.get(f"/{corpus},{other}/_field_caps", params={"fields": "tag,n,only,nodv"}))
    assert sorted(body["indices"]) == sorted([corpus, other])
    f = body["fields"]
    assert f["tag"] == {
        "text": {"type": "text", "searchable": True, "aggregatable": False, "indices": [other]},
        "keyword": {"type": "keyword", "searchable": True, "aggregatable": True, "indices": [corpus]},
    }
    assert f["n"] == {
        "integer": {"type": "integer", "searchable": True, "aggregatable": True, "indices": [corpus]},
        "long": {"type": "long", "searchable": True, "aggregatable": True, "indices": [other]},
    }
    # fields present in only one index carry no "indices" list
    assert f["only"] == {"keyword": {"type": "keyword", "searchable": False, "aggregatable": True}}
    assert f["nodv"] == {"keyword": {"type": "keyword", "searchable": True, "aggregatable": False}}

    body = _ok(client.get(f"/{corpus},{other}/_field_caps", params={"fields": "only", "include_unmapped": "true"}))
    assert body["fields"]["only"] == {
        "keyword": {"type": "keyword", "searchable": False, "aggregatable": True, "indices": [other]},
        "unmapped": {"type": "unmapped", "searchable": False, "aggregatable": False, "indices": [corpus]},
    }


def test_field_caps_wildcard(client, corpus):
    body = _ok(client.get(f"/{corpus}/_field_caps", params={"fields": "t*"}))
    assert set(body["fields"]) == {"title", "title.raw", "tag"}


def test_field_caps_metadata_fields(client, corpus):
    f = _ok(client.get(f"/{corpus}/_field_caps", params={"fields": "_*"}))["fields"]
    assert f["_index"] == {"_index": {"type": "_index", "searchable": True, "aggregatable": True}}
    assert f["_source"] == {"_source": {"type": "_source", "searchable": False, "aggregatable": False}}
    assert f["_seq_no"] == {"_seq_no": {"type": "_seq_no", "searchable": True, "aggregatable": True}}
    assert f["_routing"] == {"_routing": {"type": "_routing", "searchable": True, "aggregatable": False}}


def test_field_caps_unknown_field(client, corpus):
    assert _ok(client.get(f"/{corpus}/_field_caps", params={"fields": "nope"})) == {"indices": [corpus], "fields": {}}


def test_field_caps_requires_fields(client, corpus):
    err = _err(client.get(f"/{corpus}/_field_caps"), 400, "illegal_argument_exception")
    assert err["reason"] == "specified fields can't be null or empty"


def test_field_caps_post_with_index_filter(client, corpus):
    r = client.post(f"/{corpus}/_field_caps", params={"fields": "tag"},
                    json={"index_filter": {"range": {"n": {"gte": 100}}}})
    assert _ok(r)["fields"]["tag"]["keyword"]["type"] == "keyword"


# ---------------------------------------------------------------------------
# _search_shards / routing / multi-shard behaviour
# ---------------------------------------------------------------------------


@pytest.fixture
def two_shard(client, index_factory):
    name = index_factory(settings={"number_of_shards": 2, "number_of_replicas": 0},
                         mappings={"properties": {"t": {"type": "text"}, "k": {"type": "keyword"}}})
    docs_a = {"1": {"t": "foo"}, **{f"x{i}": {"t": "foo bar"} for i in range(10)}}
    docs_b = {"2": {"t": "foo"}, **{f"y{i}": {"t": "bar"} for i in range(10)}}
    _bulk_ids(client, name, docs_a, routing="a")  # routing "a" -> shard 0
    _bulk_ids(client, name, docs_b, routing="b")  # routing "b" -> shard 1
    return name


def test_search_shards_shape(client, two_shard):
    body = _ok(client.get(f"/{two_shard}/_search_shards"))
    assert set(body) == {"nodes", "indices", "shards"}
    assert body["indices"] == {two_shard: {}}
    assert len(body["shards"]) == 2
    copies = [group[0] for group in body["shards"]]
    assert sorted(c["shard"] for c in copies) == [0, 1]
    for c in copies:
        assert c["state"] == "STARTED" and c["primary"] is True and c["index"] == two_shard
        assert c["node"] in body["nodes"]


@pytest.mark.parametrize("routing,shard", [("a", 0), ("b", 1)])
def test_search_shards_with_routing(client, two_shard, routing, shard):
    body = _ok(client.get(f"/{two_shard}/_search_shards", params={"routing": routing}))
    assert [g[0]["shard"] for g in body["shards"]] == [shard]


def test_routing_param_limits_shards(client, two_shard):
    res = _s(client, two_shard, {"size": 0}, routing="b")
    assert res["_shards"]["total"] == 1 and res["hits"]["total"]["value"] == 11
    res = _s(client, two_shard, {"size": 0}, routing="a,b")
    assert res["_shards"]["total"] == 2 and res["hits"]["total"]["value"] == 22
    assert _ok(client.get(f"/{two_shard}/_count", params={"routing": "a"}))["count"] == 11


def test_routed_hits_expose_routing(client, two_shard):
    res = _s(client, two_shard, {"query": {"ids": {"values": ["1"]}}})
    assert res["hits"]["hits"][0]["_routing"] == "a"


def test_preference_shards(client, two_shard):
    res = _s(client, two_shard, {"size": 0}, preference="_shards:0")
    assert res["_shards"]["total"] == 1 and res["hits"]["total"]["value"] == 11
    res = _s(client, two_shard, {"size": 0}, preference="_shards:5")
    assert res["_shards"]["total"] == 0 and res["hits"]["total"]["value"] == 0


def test_dfs_query_then_fetch_equalises_scores(client, two_shard):
    qtf = _s(client, two_shard, {"query": {"match": {"t": "foo"}}, "size": 20})
    dfs = _s(client, two_shard, {"query": {"match": {"t": "foo"}}, "size": 20}, search_type="dfs_query_then_fetch")
    q = {h["_id"]: h["_score"] for h in qtf["hits"]["hits"]}
    d = {h["_id"]: h["_score"] for h in dfs["hits"]["hits"]}
    assert q["2"] > q["1"] * 5  # local IDF differs a lot between shards
    assert d["1"] == d["2"]
    assert dfs["_shards"] == {"total": 2, "successful": 2, "skipped": 0, "failed": 0}


def test_multi_shard_total_and_sort(client, two_shard):
    res = _s(client, two_shard, {"size": 3, "sort": ["_id"], "_source": False}, batched_reduce_size="2")
    assert res["hits"]["total"]["value"] == 22
    assert _ids(res) == ["1", "2", "x0"]


def test_multi_shard_track_total_hits_threshold(client, two_shard):
    res = _s(client, two_shard, {"size": 0, "track_total_hits": 5})
    assert res["hits"]["total"] == {"value": 5, "relation": "gte"}


# ---------------------------------------------------------------------------
# Misc request parameters
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "params",
    [
        {"search_type": "query_then_fetch"},
        {"search_type": "dfs_query_then_fetch"},
        {"preference": "_local"},
        {"preference": "_only_local"},
        {"preference": "_primary"},
        {"preference": "custom-string-xyz"},
        {"preference": "_shards:0"},
        {"request_cache": "true"},
        {"request_cache": "false"},
        {"allow_partial_search_results": "false"},
        {"allow_partial_search_results": "true"},
        {"batched_reduce_size": "2"},
        {"pre_filter_shard_size": "1"},
        {"ccs_minimize_roundtrips": "true"},
        {"ccs_minimize_roundtrips": "false"},
        {"max_concurrent_shard_requests": "1"},
        {"routing": "x"},
        {"stats": "g1"},
        {"typed_keys": "true"},
        {"timeout": "1m"},
        {"expand_wildcards": "open"},
    ],
)
def test_harmless_params(client, corpus, params):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}}, **params)
    assert res["hits"]["total"] == {"value": 2, "relation": "eq"}
    assert sorted(_ids(res)) == ["1", "3"]
    assert res["_shards"]["failed"] == 0


@pytest.mark.parametrize(
    "params,etype,reason",
    [
        ({"search_type": "query_and_fetch"}, "illegal_argument_exception", "Unsupported search type [query_and_fetch]"),
        ({"search_type": "bogus"}, "illegal_argument_exception", "No search type for [bogus]"),
        ({"request_cache": "maybe"}, "illegal_argument_exception",
         "Failed to parse value [maybe] as only [true] or [false] are allowed."),
        ({"batched_reduce_size": "1"}, "illegal_argument_exception", "batchedReduceSize must be >= 2"),
        ({"pre_filter_shard_size": "0"}, "illegal_argument_exception", "preFilterShardSize must be >= 1"),
        ({"max_concurrent_shard_requests": "0"}, "illegal_argument_exception", "maxConcurrentShardRequests must be >= 1"),
    ],
)
def test_invalid_param_values(client, corpus, params, etype, reason):
    err = _err(_search(client, corpus, {"size": 0}, **params), 400, etype)
    assert err["reason"] == reason


def test_dfs_with_explain(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}}, search_type="dfs_query_then_fetch", explain="true")
    assert all("_explanation" in h and h["_shard"] == f"[{corpus}][0]" for h in res["hits"]["hits"])


def test_typed_keys_aggregations(client, corpus):
    res = _s(client, corpus, {"size": 0, "aggs": {
        "t": {"terms": {"field": "tag"}},
        "m": {"max": {"field": "n"}},
        "d": {"date_histogram": {"field": "date", "calendar_interval": "month"}},
        "f": {"filter": {"term": {"tag": "a"}}, "aggs": {"av": {"avg": {"field": "n"}}}},
        "nn": {"terms": {"field": "n"}},
        "p": {"terms": {"field": "price"}},
    }}, typed_keys="true")
    aggs = res["aggregations"]
    assert set(aggs) == {"sterms#t", "max#m", "date_histogram#d", "filter#f", "lterms#nn", "dterms#p"}
    assert aggs["filter#f"] == {"doc_count": 2, "avg#av": {"value": 2.0}}
    assert aggs["max#m"] == {"value": 5.0}


def test_typed_keys_suggest(client, corpus):
    res = _s(client, corpus, {"size": 0, "suggest": {"sg": {"text": "fux", "term": {"field": "title"}}}},
             typed_keys="true")
    assert list(res["suggest"]) == ["term#sg"]


def test_without_typed_keys_plain_names(client, corpus):
    res = _s(client, corpus, {"size": 0, "aggs": {"m": {"max": {"field": "n"}}}})
    assert res["aggregations"] == {"m": {"value": 5.0}}


@pytest.mark.parametrize(
    "fp,expected",
    [
        ("hits.hits._id", {"hits": {"hits": [{"_id": "1"}, {"_id": "3"}]}}),
        ("**._id", {"hits": {"hits": [{"_id": "1"}, {"_id": "3"}]}}),
        ("hits.total", {"hits": {"total": {"value": 2, "relation": "eq"}}}),
        ("nothing.matches", {}),
        ("hits.hits._source.ti*", {"hits": {"hits": [{"_source": {"title": "quick brown fox"}},
                                                      {"_source": {"title": "quick red fox"}}]}}),
        ("hits.hits._id,hits.max_score", None),
    ],
)
def test_filter_path(client, corpus, fp, expected):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}}, filter_path=fp)
    if expected is None:
        assert set(res["hits"]) == {"max_score", "hits"}
        assert res["hits"]["hits"] == [{"_id": "1"}, {"_id": "3"}]
    else:
        assert res == expected


def test_filter_path_sort_values(client, corpus):
    res = _s(client, corpus, {"sort": [{"n": "desc"}], "size": 2}, filter_path="hits.hits.sort")
    assert res == {"hits": {"hits": [{"sort": [5]}, {"sort": [4]}]}}


def test_filter_path_multiple_and_took(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}}, filter_path="hits.total,took")
    assert set(res) == {"took", "hits"}
    assert res["hits"] == {"total": {"value": 2, "relation": "eq"}}


def test_filter_path_exclusion(client, corpus):
    res = _s(client, corpus, {"query": {"match": {"title": "fox"}}}, filter_path="-hits.hits._source,-took,-_shards")
    assert set(res) == {"timed_out", "hits"}
    assert all(set(h) == {"_index", "_id", "_score"} for h in res["hits"]["hits"])


# ---------------------------------------------------------------------------
# _rank_eval
# ---------------------------------------------------------------------------


def test_rank_eval_precision(client, corpus):
    r = client.post(f"/{corpus}/_rank_eval", json={
        "requests": [{"id": "q1", "request": {"query": {"match": {"title": "fox"}}},
                      "ratings": [{"_index": corpus, "_id": "1", "rating": 1},
                                  {"_index": corpus, "_id": "2", "rating": 1}]}],
        "metric": {"precision": {"k": 10, "relevant_rating_threshold": 1}}})
    body = _ok(r)
    assert body["metric_score"] == 0.5
    assert body["failures"] == {}
    d = body["details"]["q1"]
    assert d["metric_score"] == 0.5
    assert d["unrated_docs"] == [{"_index": corpus, "_id": "3"}]
    assert d["metric_details"] == {"precision": {"relevant_docs_retrieved": 1, "docs_retrieved": 2}}
    assert [(h["hit"]["_id"], h["rating"]) for h in d["hits"]] == [("1", 1), ("3", None)]


def test_rank_eval_recall(client, corpus):
    r = client.post(f"/{corpus}/_rank_eval", json={
        "requests": [{"id": "q1", "request": {"query": {"match": {"title": "fox"}}},
                      "ratings": [{"_index": corpus, "_id": "1", "rating": 1}]}],
        "metric": {"recall": {"k": 10}}})
    body = _ok(r)
    assert body["metric_score"] == 1.0
    assert body["details"]["q1"]["metric_details"] == {"recall": {"relevant_docs_retrieved": 1, "relevant_docs": 1}}


def test_rank_eval_mrr(client, corpus):
    r = client.post(f"/{corpus}/_rank_eval", json={
        "requests": [{"id": "q1", "request": {"query": {"match": {"title": "fox"}}, "sort": [{"n": "asc"}]},
                      "ratings": [{"_index": corpus, "_id": "3", "rating": 1}]}],
        "metric": {"mean_reciprocal_rank": {"k": 10}}})
    body = _ok(r)
    assert body["metric_score"] == 0.5
    assert body["details"]["q1"]["metric_details"] == {"mean_reciprocal_rank": {"first_relevant": 2}}


def test_rank_eval_unknown_metric(client, corpus):
    err = _err(client.post(f"/{corpus}/_rank_eval", json={
        "requests": [{"id": "q1", "request": {"query": {"match_all": {}}}, "ratings": []}],
        "metric": {"bogus": {}}}), 400, "x_content_parse_exception", "named_object_not_found_exception")
    assert err["reason"].endswith("[rank_eval] failed to parse field [metric]")
