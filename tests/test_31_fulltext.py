"""Full-text query behaviour of OpenSearch 3.9.0 (compatibility spec).

Covers match / match_phrase / match_phrase_prefix / match_bool_prefix,
multi_match (all types), combined_fields, query_string, simple_query_string,
intervals, span queries, BM25 relevance sanity, explain and Unicode handling
of the standard analyzer.

All indices are single-shard with no replicas so that scoring is deterministic.
"""

import json
import uuid

import pytest

pytestmark = pytest.mark.fulltext


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def _create(client, mappings, docs, settings=None):
    name = f"slopensearch-ft31-{uuid.uuid4().hex[:10]}"
    s = {"number_of_shards": 1, "number_of_replicas": 0}
    if settings:
        s.update(settings)
    r = client.put(f"/{name}", json={"settings": s, "mappings": mappings})
    assert r.status_code == 200, r.text
    lines = []
    for i, d in enumerate(docs):
        lines.append(json.dumps({"index": {"_id": str(i + 1)}}))
        lines.append(json.dumps(d, ensure_ascii=False))
    r = client.post(
        f"/{name}/_bulk",
        content=("\n".join(lines) + "\n").encode(),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200, r.text
    assert r.json()["errors"] is False, r.text
    return name


def _drop(client, name):
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def _search(client, index, query, **extra):
    body = {"query": query, "size": 100}
    body.update(extra)
    return client.post(f"/{index}/_search", json=body)


def _ok(client, index, query, **extra):
    r = _search(client, index, query, **extra)
    assert r.status_code == 200, r.text
    return r.json()


def ids(client, index, query, **extra):
    """Ids in ranking order."""
    return [h["_id"] for h in _ok(client, index, query, **extra)["hits"]["hits"]]


def idset(client, index, query, **extra):
    return set(ids(client, index, query, **extra))


def scored(client, index, query, **extra):
    return [(h["_id"], h["_score"]) for h in _ok(client, index, query, **extra)["hits"]["hits"]]


def _err(client, index, query, status, etype, root_type=None, reason_part=None):
    r = _search(client, index, query)
    assert r.status_code == status, r.text
    err = r.json()["error"]
    assert err["type"] == etype, err
    if root_type is not None:
        assert err["root_cause"][0]["type"] == root_type, err
    if reason_part is not None:
        text = json.dumps(err)
        assert reason_part in text, err
    return err


# --------------------------------------------------------------------------
# main dataset
# --------------------------------------------------------------------------

MAIN_MAPPING = {
    "properties": {
        "title": {"type": "text", "fields": {"raw": {"type": "keyword"}}},
        "body": {"type": "text"},
        "tags": {"type": "text"},
        "tags0": {"type": "text", "position_increment_gap": 0},
        "stop": {"type": "text", "analyzer": "stop"},
        "eng": {"type": "text", "analyzer": "english"},
        "num": {"type": "integer"},
        "date": {"type": "date"},
        "flag": {"type": "boolean"},
        "kw": {"type": "keyword"},
    }
}

MAIN_DOCS = [
    {  # 1
        "title": "quick brown fox",
        "body": "the quick brown fox jumps over the lazy dog",
        "num": 1,
        "date": "2024-01-01",
        "flag": True,
        "kw": "Alpha",
        "tags": ["red apple", "green pear"],
        "tags0": ["red apple", "green pear"],
        "stop": "the quick fox",
        "eng": "running foxes",
    },
    {  # 2
        "title": "brown dog",
        "body": "a lazy brown dog sleeps all day",
        "num": 2,
        "date": "2024-02-01",
        "flag": False,
        "kw": "beta",
        "tags": ["apple pie"],
        "stop": "a dog",
        "eng": "the runner ran",
    },
    {  # 3
        "title": "quick red fox",
        "body": "red fox is very quick and clever",
        "num": 3,
        "date": "2024-03-01",
        "flag": True,
        "kw": "gamma",
    },
    {  # 4
        "title": "lazy cat",
        "body": "the cat sleeps",
        "num": 4,
        "date": "2024-04-01",
        "flag": False,
        "kw": "delta",
    },
    {"title": "fox", "body": "fox fox fox", "num": 5, "kw": "alpha"},  # 5
]

ALL_MAIN = {"1", "2", "3", "4", "5"}


@pytest.fixture(scope="module")
def main(client):
    name = _create(client, MAIN_MAPPING, MAIN_DOCS)
    yield name
    _drop(client, name)


def M(field, **params):
    return {"match": {field: params}}


# --------------------------------------------------------------------------
# match: basics, operator
# --------------------------------------------------------------------------


@pytest.mark.smoke
def test_match_default_operator_is_or(client, main):
    assert idset(client, main, {"match": {"title": "quick fox"}}) == {"1", "3", "5"}


@pytest.mark.parametrize("op", ["and", "AND", "And"])
def test_match_operator_and_case_insensitive(client, main, op):
    q = M("title", query="quick fox", operator=op)
    assert idset(client, main, q) == {"1", "3"}


@pytest.mark.parametrize("op", ["or", "OR"])
def test_match_operator_or_explicit(client, main, op):
    q = M("title", query="quick fox", operator=op)
    assert idset(client, main, q) == {"1", "3", "5"}


def test_match_invalid_operator_error(client, main):
    _err(
        client, main, M("title", query="quick fox", operator="xor"), 400,
        "illegal_argument_exception",
        reason_part="No enum constant org.opensearch.index.query.Operator.XOR",
    )


def test_match_is_case_insensitive_on_text(client, main):
    assert idset(client, main, {"match": {"title": "QUICK"}}) == {"1", "3"}


def test_match_punctuation_is_ignored(client, main):
    assert idset(client, main, {"match": {"title": "quick!!! ,,, (fox)"}}) == {"1", "3", "5"}


def test_match_short_and_long_form_identical(client, main):
    a = scored(client, main, {"match": {"body": "lazy dog"}})
    b = scored(client, main, M("body", query="lazy dog"))
    assert a == b
    assert {i for i, _ in a} == {"1", "2"}


def test_match_boost_scales_score(client, main):
    a = dict(scored(client, main, M("title", query="fox")))
    b = dict(scored(client, main, M("title", query="fox", boost=3)))
    assert a.keys() == b.keys()
    for k in a:
        assert b[k] == pytest.approx(a[k] * 3, rel=1e-4)


def test_match_multiple_fields_rejected(client, main):
    _err(
        client, main, {"match": {"title": "fox", "body": "fox"}}, 400, "parsing_exception",
        reason_part="[match] query doesn't support multiple fields, found [title] and [body]",
    )


def test_match_unknown_parameter_rejected(client, main):
    _err(client, main, M("title", query="fox", foo=1), 400, "parsing_exception",
         reason_part="[match] query does not support [foo]")


@pytest.mark.parametrize("q", [{"match": {"title": None}}, M("title", operator="and"), {"match": {"title": {}}}])
def test_match_without_text_rejected(client, main, q):
    _err(client, main, q, 400, "parsing_exception", reason_part="No text specified for text query")


def test_match_array_value_is_server_error(client, main):
    r = _search(client, main, {"match": {"title": ["fox", "dog"]}})
    assert r.status_code == 500
    assert r.json()["error"]["type"] == "illegal_state_exception"


def test_match_missing_field_returns_nothing(client, main):
    res = _ok(client, main, {"match": {"does_not_exist": "fox"}})
    assert res["hits"]["total"]["value"] == 0
    assert res["hits"]["hits"] == []


def test_match_empty_string_matches_nothing(client, main):
    assert ids(client, main, {"match": {"title": ""}}) == []


def test_match_empty_string_zero_terms_all(client, main):
    assert idset(client, main, M("title", query="", zero_terms_query="all")) == ALL_MAIN


# --------------------------------------------------------------------------
# match: minimum_should_match
# --------------------------------------------------------------------------

# query "quick brown fox" on title: doc1 has 3 terms, doc3 has 2 (quick fox),
# doc2 has 1 (brown), doc5 has 1 (fox), doc4 has 0
@pytest.mark.parametrize(
    "msm,expected",
    [
        (1, {"1", "2", "3", "5"}),
        (2, {"1", "3"}),
        ("2", {"1", "3"}),
        (3, {"1"}),
        (5, set()),  # more than number of clauses -> nothing
        (-1, {"1", "3"}),
        (-2, {"1", "2", "3", "5"}),
        (-5, {"1", "2", "3", "5"}),  # clamps to at least one clause
        ("67%", {"1", "3"}),  # 3*0.67=2.01 -> 2
        ("66%", {"1", "2", "3", "5"}),  # 1.98 rounds down -> 1
        ("100%", {"1"}),
        ("0%", {"1", "2", "3", "5"}),
        ("-34%", {"1", "3"}),  # 1.02 -> 1 optional may be missing
        ("-33%", {"1"}),  # 0.99 -> 0 -> all required
        ("2<67%", {"1", "3"}),  # 3 clauses > 2 -> 67%
        ("3<67%", {"1"}),  # 3 clauses not > 3 -> all required
        ("2<-1", {"1", "3"}),
        ("3<-1", {"1"}),
        ("1<-1 3<1", {"1", "3"}),  # 3 clauses: only the "1<-1" rule applies
    ],
)
def test_match_minimum_should_match(client, main, msm, expected):
    q = M("title", query="quick brown fox", minimum_should_match=msm)
    assert idset(client, main, q) == expected


def test_match_minimum_should_match_invalid(client, main):
    _err(
        client, main, M("title", query="quick brown fox", minimum_should_match="abc"),
        400, "search_phase_execution_exception", root_type="query_shard_exception",
        reason_part='For input string: \\"abc\\"',
    )


def test_match_minimum_should_match_ignored_with_single_term(client, main):
    assert idset(client, main, M("title", query="fox", minimum_should_match=3)) == {"1", "3", "5"}


# --------------------------------------------------------------------------
# match: fuzziness
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "query,fuzz,expected",
    [
        ("qick", "AUTO", {"1", "3"}),  # 4 chars -> 1 edit
        ("qick", 0, set()),
        ("qick", "0", set()),
        ("qick", 1, {"1", "3"}),
        ("qick", "1", {"1", "3"}),
        ("fxo", "AUTO", {"1", "3", "5"}),  # transposition counts as one edit
        ("fo", "AUTO", set()),  # <=2 chars -> exact only
        ("fx", "AUTO", set()),
        ("fx", "AUTO:1,3", {"1", "3", "5"}),
        ("brwn", "AUTO", {"1", "2"}),
        ("brwn", "AUTO:5,8", set()),
        ("bxxwn", "AUTO", set()),  # 5 chars -> only 1 edit
        ("bxxwn", 2, {"1", "2"}),
        ("bxxwn", 3, {"1", "2"}),  # capped at 2
        ("BRWN", "AUTO", {"1", "2"}),  # analyzed (lowercased) before fuzzing
    ],
)
def test_match_fuzziness(client, main, query, fuzz, expected):
    assert idset(client, main, M("title", query=query, fuzziness=fuzz)) == expected


def test_match_fuzziness_transpositions_toggle(client, main):
    assert idset(client, main, M("title", query="qucik", fuzziness=1)) == {"1", "3"}
    assert idset(client, main, M("title", query="qucik", fuzziness=1, fuzzy_transpositions=False)) == set()
    assert idset(client, main, M("title", query="qucik", fuzziness=2, fuzzy_transpositions=False)) == {"1", "3"}


def test_match_fuzziness_prefix_length(client, main):
    assert idset(client, main, M("title", query="wuick", fuzziness=1, prefix_length=0)) == {"1", "3"}
    assert idset(client, main, M("title", query="wuick", fuzziness=1, prefix_length=1)) == set()


def test_match_fuzziness_exact_scores_higher_than_fuzzy(client, main):
    # "fox" exact in title of 1,3,5; fuzzy "box"->fox
    res = ids(client, main, M("title", query="fox", fuzziness="AUTO"))
    assert set(res) == {"1", "3", "5"}
    assert res[0] == "5"


def test_match_invalid_fuzziness(client, main):
    _err(client, main, M("title", query="x", fuzziness="foo"), 400, "illegal_argument_exception",
         reason_part="Invalid fuzziness value: foo")


def test_match_fuzziness_with_operator_and(client, main):
    q = M("title", query="qick fxo", fuzziness="AUTO", operator="and")
    assert idset(client, main, q) == {"1", "3"}


# --------------------------------------------------------------------------
# match: zero_terms_query, analyzer
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "ztq,expected",
    [(None, set()), ("none", set()), ("NONE", set()), ("all", ALL_MAIN), ("ALL", ALL_MAIN)],
)
def test_match_zero_terms_query_stopwords_only(client, main, ztq, expected):
    params = {"query": "the a"}
    if ztq is not None:
        params["zero_terms_query"] = ztq
    assert idset(client, main, M("stop", **params)) == expected


def test_match_zero_terms_query_invalid(client, main):
    _err(client, main, M("stop", query="the", zero_terms_query="some"), 400, "parsing_exception",
         reason_part="Unsupported zero_terms_query value [some]")


def test_match_zero_terms_all_ignored_when_terms_remain(client, main):
    assert idset(client, main, M("stop", query="the fox", zero_terms_query="all")) == {"1"}


def test_match_analyzer_override_stop(client, main):
    # "the" removed by stop analyzer -> only "fox" remains
    assert idset(client, main, M("title", query="the fox", analyzer="stop")) == {"1", "3", "5"}
    assert idset(client, main, M("title", query="the fox", analyzer="stop", operator="and")) == {"1", "3", "5"}
    assert idset(client, main, M("title", query="the fox", operator="and")) == set()


def test_match_analyzer_override_keyword(client, main):
    assert idset(client, main, M("title", query="quick fox", analyzer="keyword")) == set()
    assert idset(client, main, M("title", query="fox", analyzer="keyword")) == {"1", "3", "5"}
    # keyword analyzer does not lowercase
    assert idset(client, main, M("title", query="FOX", analyzer="keyword")) == set()


def test_match_analyzer_unknown(client, main):
    _err(client, main, M("title", query="fox", analyzer="nope"), 400, "search_phase_execution_exception",
         root_type="query_shard_exception", reason_part="[match] analyzer [nope] not found")


@pytest.mark.parametrize("word", ["run", "runs", "running"])
def test_match_english_analyzer_stems_both_sides(client, main, word):
    assert idset(client, main, {"match": {"eng": word}}) == {"1"}


def test_match_english_field_with_standard_search_analyzer(client, main):
    # indexed "foxes" stems to "fox"; plain "foxes" through standard analyzer no longer matches
    assert idset(client, main, M("eng", query="fox", analyzer="standard")) == {"1"}
    assert idset(client, main, M("eng", query="foxes", analyzer="standard")) == set()


# --------------------------------------------------------------------------
# match on non-text fields
# --------------------------------------------------------------------------


@pytest.mark.parametrize("value", ["3", 3])
def test_match_numeric_field(client, main, value):
    assert ids(client, main, {"match": {"num": value}}) == ["3"]


def test_match_numeric_field_with_text_errors(client, main):
    _err(client, main, {"match": {"num": "abc"}}, 400, "search_phase_execution_exception",
         root_type="query_shard_exception", reason_part="number_format_exception")


def test_match_numeric_field_with_text_lenient(client, main):
    res = _ok(client, main, M("num", query="abc", lenient=True))
    assert res["hits"]["total"]["value"] == 0


def test_match_numeric_field_rejects_fuzziness(client, main):
    _err(client, main, M("num", query="3", fuzziness=2), 400, "search_phase_execution_exception",
         root_type="query_shard_exception",
         reason_part="Can only use fuzzy queries on keyword and text fields - not on [num] which is of type [integer]")


def test_match_date_field(client, main):
    assert ids(client, main, {"match": {"date": "2024-03-01"}}) == ["3"]


def test_match_date_field_bad_value(client, main):
    _err(client, main, {"match": {"date": "notadate"}}, 400, "search_phase_execution_exception",
         root_type="parse_exception", reason_part="failed to parse date field [notadate]")


def test_match_date_field_bad_value_lenient(client, main):
    assert ids(client, main, M("date", query="notadate", lenient=True)) == []


@pytest.mark.parametrize("value", ["true", True])
def test_match_boolean_field(client, main, value):
    assert idset(client, main, {"match": {"flag": value}}) == {"1", "3"}


def test_match_boolean_field_bad_value(client, main):
    _err(client, main, {"match": {"flag": "yes"}}, 400, "search_phase_execution_exception",
         root_type="query_shard_exception", reason_part="Can't parse boolean value [yes]")


def test_match_keyword_field_exact_and_case_sensitive(client, main):
    assert ids(client, main, {"match": {"kw": "alpha"}}) == ["5"]
    assert ids(client, main, {"match": {"kw": "Alpha"}}) == ["1"]
    assert ids(client, main, {"match": {"kw": "ALPHA"}}) == []


def test_match_keyword_subfield_whole_value_only(client, main):
    assert ids(client, main, {"match": {"title.raw": "quick brown fox"}}) == ["1"]
    assert ids(client, main, {"match": {"title.raw": "quick"}}) == []
    assert ids(client, main, {"match": {"title.raw": "Quick Brown Fox"}}) == []
    # the text parent matches partially
    assert idset(client, main, {"match": {"title": "Quick"}}) == {"1", "3"}


def test_match_integer_and_boolean_values_on_text_field(client, idx_misc):
    assert ids(client, idx_misc, {"match": {"t": 123}}) == ["1"]
    assert ids(client, idx_misc, {"match": {"t": True}}) == ["2"]


# --------------------------------------------------------------------------
# match_phrase
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "field,query,slop,expected",
    [
        ("title", "quick brown", 0, {"1"}),
        ("title", "QUICK, Brown!", 0, {"1"}),
        ("title", "brown quick", 0, set()),
        ("title", "brown quick", 1, set()),
        ("title", "brown quick", 2, {"1"}),  # reversed order needs slop 2
        ("title", "quick fox", 0, set()),
        ("title", "quick fox", 1, {"1", "3"}),
        ("title", "fox", 0, {"1", "3", "5"}),
        ("body", "fox dog", 4, {"1"}),
        ("body", "fox dog", 3, set()),
        ("body", "the lazy", 0, {"1"}),
    ],
)
def test_match_phrase_slop(client, main, field, query, slop, expected):
    q = {"match_phrase": {field: {"query": query, "slop": slop}}}
    assert idset(client, main, q) == expected


def test_match_phrase_short_form(client, main):
    assert ids(client, main, {"match_phrase": {"title": "quick brown"}}) == ["1"]


def test_match_phrase_negative_slop(client, main):
    _err(client, main, {"match_phrase": {"title": {"query": "a b", "slop": -1}}}, 400,
         "illegal_argument_exception", reason_part="No negative slop allowed.")


def test_match_phrase_empty(client, main):
    assert ids(client, main, {"match_phrase": {"title": ""}}) == []


def test_match_phrase_zero_terms_all(client, main):
    q = {"match_phrase": {"stop": {"query": "the a", "zero_terms_query": "all"}}}
    assert idset(client, main, q) == ALL_MAIN


def test_match_phrase_keyword_analyzer(client, main):
    q = {"match_phrase": {"title": {"query": "quick brown", "analyzer": "keyword"}}}
    assert ids(client, main, q) == []


def test_match_phrase_stopwords_leave_position_gaps(client, main):
    # stop field doc1 = "the quick fox" -> quick@1 fox@2
    assert ids(client, main, {"match_phrase": {"stop": "quick fox"}}) == ["1"]
    assert ids(client, main, {"match_phrase": {"stop": "the quick fox"}}) == ["1"]
    # "a" removed but leaves a hole: quick@0 fox@2 -> no match
    assert ids(client, main, {"match_phrase": {"stop": "quick a fox"}}) == []
    q = {"match_phrase": {"stop": {"query": "quick a fox", "slop": 1}}}
    assert ids(client, main, q) == ["1"]


def test_match_phrase_on_keyword_and_numeric(client, main):
    assert ids(client, main, {"match_phrase": {"kw": "Alpha"}}) == ["1"]
    assert ids(client, main, {"match_phrase": {"num": 3}}) == ["3"]


@pytest.mark.parametrize(
    "field,slop,expected",
    [("tags", 0, set()), ("tags", 99, set()), ("tags", 100, {"1"}), ("tags0", 0, {"1"})],
)
def test_match_phrase_across_array_values(client, main, field, slop, expected):
    # tags = ["red apple", "green pear"]; default position_increment_gap = 100
    q = {"match_phrase": {field: {"query": "apple green", "slop": slop}}}
    assert idset(client, main, q) == expected


def test_match_phrase_within_single_array_value(client, main):
    assert idset(client, main, {"match_phrase": {"tags": "apple pie"}}) == {"2"}
    assert idset(client, main, {"match_phrase": {"tags": "green pear"}}) == {"1"}


def test_match_and_operator_across_array_values(client, main):
    q = M("tags", query="apple green", operator="and")
    assert idset(client, main, q) == {"1"}


def test_match_phrase_slop_covers_equal_gaps(client, main):
    q = {"match_phrase": {"title": {"query": "quick fox", "slop": 2}}}
    res = ids(client, main, q)
    assert set(res) == {"1", "3"}


# --------------------------------------------------------------------------
# match_phrase_prefix / match_bool_prefix
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    "query,expected",
    [
        ("quick b", {"1"}),
        ("qu", {"1", "3"}),
        ("fo", {"1", "3", "5"}),
        ("brown d", {"2"}),
        ("quick r", {"3"}),
        ("QUICK BR", {"1"}),
        ("zzz", set()),
        ("", set()),
    ],
)
def test_match_phrase_prefix(client, main, query, expected):
    assert idset(client, main, {"match_phrase_prefix": {"title": query}}) == expected


def test_match_phrase_prefix_requires_order(client, main):
    assert ids(client, main, {"match_phrase_prefix": {"title": "fox qu"}}) == []


def test_match_phrase_prefix_slop(client, main):
    q = {"match_phrase_prefix": {"title": {"query": "quick f", "slop": 1}}}
    assert idset(client, main, q) == {"1", "3"}
    q = {"match_phrase_prefix": {"title": {"query": "quick f"}}}
    assert idset(client, main, q) == set()


@pytest.fixture(scope="module")
def idx_prefix(client):
    docs = [
        {"t": "foo apple"},
        {"t": "foo apricot"},
        {"t": "foo avocado"},
        {"t": "bar apple"},
    ]
    name = _create(client, {"properties": {"t": {"type": "text"}}}, docs)
    yield name
    _drop(client, name)


@pytest.mark.parametrize("max_exp,expected", [(1, {"1"}), (2, {"1", "2"}), (50, {"1", "2", "3"})])
def test_match_phrase_prefix_max_expansions(client, idx_prefix, max_exp, expected):
    q = {"match_phrase_prefix": {"t": {"query": "foo a", "max_expansions": max_exp}}}
    assert idset(client, idx_prefix, q) == expected


def test_match_phrase_prefix_single_term_is_prefix_query(client, idx_prefix):
    q = {"match_phrase_prefix": {"t": {"query": "a"}}}
    assert idset(client, idx_prefix, q) == {"1", "2", "3", "4"}


@pytest.mark.parametrize(
    "params,expected",
    [
        ({"query": "fox qu"}, {"1", "3", "5"}),
        ({"query": "fox qu", "operator": "and"}, {"1", "3"}),
        ({"query": "brown qui", "operator": "and"}, {"1"}),  # order irrelevant
        ({"query": "red fox qu", "minimum_should_match": 3}, {"3"}),
        ({"query": "b"}, {"1", "2"}),
        ({"query": "quikc b", "fuzziness": 1, "operator": "and"}, {"1"}),
        ({"query": "QUICK R", "operator": "and"}, {"3"}),
    ],
)
def test_match_bool_prefix(client, main, params, expected):
    assert idset(client, main, {"match_bool_prefix": {"title": params}}) == expected


def test_match_bool_prefix_short_form(client, main):
    assert idset(client, main, {"match_bool_prefix": {"title": "lazy c"}}) == {"4"}


# --------------------------------------------------------------------------
# multi_match / combined_fields
# --------------------------------------------------------------------------

MM_MAPPING = {
    "properties": {
        "title": {"type": "text"},
        "title_alt": {"type": "text"},
        "body": {"type": "text"},
        "first": {"type": "text"},
        "last": {"type": "text"},
        "num": {"type": "integer"},
        "kw": {"type": "keyword"},
    }
}
MM_DOCS = [
    {"title": "apple", "body": "banana banana cherry", "first": "john", "last": "smith", "num": 1, "kw": "apple"},
    {"title": "banana", "body": "apple", "first": "smith", "last": "jones", "num": 2},
    {"title": "apple banana", "body": "cherry", "first": "john", "last": "jones", "num": 3},
    {"title": "cherry", "title_alt": "apple", "body": "nothing here", "first": "will", "last": "smith", "num": 4},
    {"title": "date", "body": "apple pie recipe", "first": "john smith", "last": "x", "num": 5},
]


@pytest.fixture(scope="module")
def mm(client):
    name = _create(client, MM_MAPPING, MM_DOCS)
    yield name
    _drop(client, name)


def MM(query, fields=None, **kw):
    body = {"query": query}
    if fields is not None:
        body["fields"] = fields
    body.update(kw)
    return {"multi_match": body}


def test_multi_match_best_fields_default(client, mm):
    assert idset(client, mm, MM("apple", ["title", "body"])) == {"1", "2", "3", "5"}


def test_multi_match_best_fields_equals_max_of_single_fields(client, mm):
    a = dict(scored(client, mm, M("title", query="apple")))
    b = dict(scored(client, mm, M("body", query="apple")))
    mmres = dict(scored(client, mm, MM("apple", ["title", "body"])))
    for k, v in mmres.items():
        assert v == pytest.approx(max(a.get(k, 0), b.get(k, 0)), rel=1e-5)


def test_multi_match_tie_breaker_one_equals_most_fields(client, mm):
    a = scored(client, mm, MM("apple banana", ["title", "body"], tie_breaker=1))
    b = scored(client, mm, MM("apple banana", ["title", "body"], type="most_fields"))
    assert [i for i, _ in a] == [i for i, _ in b]
    for (_, x), (_, y) in zip(a, b):
        assert x == pytest.approx(y, rel=1e-5)


def test_multi_match_tie_breaker_changes_ranking(client, mm):
    no_tie = ids(client, mm, MM("apple banana", ["title", "body"]))
    with_tie = ids(client, mm, MM("apple banana", ["title", "body"], tie_breaker=1))
    assert no_tie[0] == "1" and with_tie[0] == "1"
    assert no_tie.index("3") < no_tie.index("2")
    assert with_tie.index("2") < with_tie.index("3")


def test_multi_match_per_field_boost_changes_ranking(client, mm):
    plain = ids(client, mm, MM("apple", ["title", "body"]))
    assert plain[0] == "2"
    title_boost = ids(client, mm, MM("apple", ["title^10", "body"]))
    assert title_boost[:2] == ["1", "3"]
    body_boost = ids(client, mm, MM("apple", ["title", "body^5"]))
    assert body_boost[:2] == ["2", "5"]


def test_multi_match_best_fields_operator_and_is_per_field(client, mm):
    assert ids(client, mm, MM("apple banana", ["title", "body"], operator="and")) == ["3"]
    assert ids(client, mm, MM("john smith", ["first", "last"], operator="and")) == ["5"]


def test_multi_match_most_fields_operator_and(client, mm):
    q = MM("john smith", ["first", "last"], operator="and", type="most_fields")
    assert ids(client, mm, q) == ["5"]


def test_multi_match_cross_fields_operator_and(client, mm):
    q = MM("john smith", ["first", "last"], operator="and", type="cross_fields")
    assert idset(client, mm, q) == {"1", "5"}


def test_multi_match_msm_best_fields_vs_cross_fields(client, mm):
    assert ids(client, mm, MM("apple banana cherry", ["title", "body"], minimum_should_match="100%")) == []
    q = MM("apple banana cherry", ["title", "body"], minimum_should_match="100%", type="cross_fields")
    assert idset(client, mm, q) == {"1", "3"}


def test_multi_match_cross_fields_rejects_fuzziness(client, mm):
    _err(client, mm, MM("aple", ["title", "body"], type="cross_fields", fuzziness="AUTO"), 400,
         "parsing_exception", reason_part="Fuzziness not allowed for type [cross_fields]")


def test_multi_match_fuzziness_best_fields(client, mm):
    assert idset(client, mm, MM("aple", ["title", "body"], fuzziness="AUTO")) == {"1", "2", "3", "5"}


@pytest.mark.parametrize(
    "mtype,query,expected",
    [
        ("phrase", "apple pie", {"5"}),
        ("phrase", "pie apple", set()),
        ("phrase_prefix", "apple p", {"5"}),
        ("phrase_prefix", "ban", {"1", "2", "3"}),
        ("bool_prefix", "che", {"1", "3", "4"}),
        ("bool_prefix", "apple ba", {"1", "2", "3", "5"}),
        ("most_fields", "cherry", {"1", "3", "4"}),
        ("cross_fields", "date recipe", {"5"}),
        ("best_fields", "nothing", {"4"}),
    ],
)
def test_multi_match_types(client, mm, mtype, query, expected):
    assert idset(client, mm, MM(query, ["title", "body"], type=mtype)) == expected


def test_multi_match_phrase_slop(client, mm):
    assert ids(client, mm, MM("pie apple", ["body"], type="phrase", slop=2)) == ["5"]


def test_multi_match_bool_prefix_constant_prefix_score(client, mm):
    res = scored(client, mm, MM("che", ["title", "body"], type="bool_prefix"))
    assert {s for _, s in res} == {1.0}


def test_multi_match_invalid_type(client, mm):
    _err(client, mm, MM("apple", ["title"], type="foo"), 400, "parse_exception",
         reason_part="failed to parse [multi_match] query type [foo]. unknown type.")


def test_multi_match_field_wildcard(client, mm):
    assert idset(client, mm, MM("apple", ["title*"])) == {"1", "3", "4"}


def test_multi_match_star_is_lenient_over_all_fields(client, mm):
    # "*" includes numeric "num" yet does not fail; keyword kw=apple matches doc1
    assert idset(client, mm, MM("apple", ["*"])) == {"1", "2", "3", "4", "5"}


def test_multi_match_without_fields_uses_all(client, mm):
    assert idset(client, mm, MM("apple")) == {"1", "2", "3", "4", "5"}


def test_multi_match_numeric_field_explicit_errors(client, mm):
    _err(client, mm, MM("apple", ["title", "num"]), 400, "search_phase_execution_exception",
         root_type="query_shard_exception", reason_part='For input string: \\"apple\\"')


def test_multi_match_numeric_field_lenient(client, mm):
    assert idset(client, mm, MM("apple", ["title", "num"], lenient=True)) == {"1", "3"}
    assert ids(client, mm, MM("3", ["title", "num"])) == ["3"]


def test_multi_match_unknown_field_only(client, mm):
    assert ids(client, mm, MM("apple", ["nope"])) == []


def test_combined_fields_and(client, mm):
    q = {"combined_fields": {"query": "john smith", "fields": ["first", "last"], "operator": "and"}}
    assert idset(client, mm, q) == {"1", "5"}


def test_combined_fields_or(client, mm):
    q = {"combined_fields": {"query": "apple", "fields": ["title", "body"]}}
    assert idset(client, mm, q) == {"1", "2", "3", "5"}


def test_combined_fields_minimum_should_match(client, mm):
    q = {"combined_fields": {"query": "apple banana cherry", "fields": ["title", "body"], "minimum_should_match": 3}}
    assert idset(client, mm, q) == {"1", "3"}


def test_combined_fields_wildcard_fields(client, mm):
    q = {"combined_fields": {"query": "apple", "fields": ["title*"]}}
    assert idset(client, mm, q) == {"1", "3", "4"}


def test_combined_fields_boost(client, mm):
    q = {"combined_fields": {"query": "apple", "fields": ["title^2", "body"]}}
    res = ids(client, mm, q)
    assert set(res) == {"1", "2", "3", "5"}
    assert set(res[:2]) == {"1", "3"}


def test_combined_fields_boost_below_one_rejected(client, mm):
    q = {"combined_fields": {"query": "apple", "fields": ["title^0.5", "body"]}}
    _err(client, mm, q, 400, "search_phase_execution_exception", root_type="query_shard_exception",
         reason_part="weight must be greater or equal to 1")


@pytest.mark.parametrize("field,ftype", [("num", "integer"), ("kw", "keyword")])
def test_combined_fields_rejects_non_text(client, mm, field, ftype):
    q = {"combined_fields": {"query": "apple", "fields": ["title", field]}}
    _err(client, mm, q, 400, "search_phase_execution_exception", root_type="query_shard_exception",
         reason_part=f"Field [{field}] of type [{ftype}] does not support [combined_fields] queries")


def test_combined_fields_without_fields_matches_nothing(client, mm):
    assert ids(client, mm, {"combined_fields": {"query": "apple"}}) == []


# --------------------------------------------------------------------------
# query_string
# --------------------------------------------------------------------------


def QS(query, **kw):
    return {"query_string": dict(query=query, **kw)}


@pytest.mark.parametrize(
    "query,expected",
    [
        ("fox", {"1", "3", "5"}),
        ("quick fox", {"1", "3", "5"}),
        ("quick AND fox", {"1", "3"}),
        ("quick and fox", {"1", "3", "5"}),  # lowercase "and" is a term (not a stopword)
        ("quick && fox", {"1", "3"}),
        ("quick OR dog", {"1", "2", "3"}),
        ("fox || dog", {"1", "2", "3", "5"}),
        ("fox NOT quick", {"5"}),
        ("fox AND NOT quick", {"5"}),
        ("NOT fox", {"2", "4"}),
        ("-fox", {"2", "4"}),
        ("!fox", {"2", "4"}),
        ("+fox -red", {"1", "5"}),
        ("fox && red", {"3"}),
        ("(quick OR lazy) AND (fox OR cat)", {"1", "3", "4"}),
        ('"quick fox"', set()),
        ('"quick fox"~1', {"1", "3"}),
        ('"brown quick"~2', {"1"}),
        ("qick~", {"1", "3"}),
        ("qick~0", set()),
        ("qick~1", {"1", "3"}),
        ("bxxwn~", set()),  # bare ~ is AUTO
        ("bxxwn~2", {"1", "2"}),
        ("qu*", {"1", "3"}),
        ("QU*", {"1", "3"}),
        ("f?x", {"1", "3", "5"}),
        ("q*k", {"1", "3"}),
        ("*ox", {"1", "3", "5"}),
        ("/f.x/", {"1", "3", "5"}),
        ("/qu.*/", {"1", "3"}),
        ("fox^2.5", {"1", "3", "5"}),
        ("fox~1.5", {"1", "3", "5"}),
        ("\\(fox\\)", {"1", "3", "5"}),
        ("fox\\!", {"1", "3", "5"}),
        ("", set()),
    ],
)
def test_query_string_syntax_default_field(client, main, query, expected):
    assert idset(client, main, QS(query, default_field="title")) == expected


@pytest.mark.parametrize("op", ["AND", "and"])
def test_query_string_default_operator(client, main, op):
    assert idset(client, main, QS("quick fox", default_field="title", default_operator=op)) == {"1", "3"}


def test_query_string_default_operator_invalid(client, main):
    _err(client, main, QS("quick fox", default_field="title", default_operator="XOR"), 400,
         "illegal_argument_exception", reason_part="No enum constant org.opensearch.index.query.Operator.XOR")


@pytest.mark.parametrize(
    "query,expected",
    [
        ("title:fox", {"1", "3", "5"}),
        ("title:(quick OR dog)", {"1", "2", "3"}),
        ("title:(quick AND red)", {"3"}),
        ("title:quick AND body:clever", {"3"}),
        ("title:quick body:dog", {"1", "2", "3"}),
        ("title:quick~1", {"1", "3"}),
        ("title:qu?ck", {"1", "3"}),
        ("title:QUICK", {"1", "3"}),
        ("title:Qu*", {"1", "3"}),
        ('body:"the lazy"', {"1"}),
        ("title:*", ALL_MAIN),
        ("*", ALL_MAIN),
        ("nofield:*", set()),
        ("*:fox", {"1", "3", "5"}),
        ("t\\*:fox", {"1", "3", "5"}),
        ("_exists_:date", {"1", "2", "3", "4"}),
        ("NOT _exists_:date", {"5"}),
        ("_exists_:tags", {"1", "2"}),
        ("kw:Alpha", {"1"}),
        ("kw:alpha", {"5"}),
        ("kw:ALPHA", set()),
        ("title.raw:quick\\ brown\\ fox", {"1"}),
        ('title.raw:"quick brown fox"', {"1"}),
        ("flag:true", {"1", "3"}),
        ("flag:false AND num:<3", {"2"}),
        ("title:fox AND -num:5", {"1", "3"}),
        ("title:fox AND NOT num:[1 TO 3]", {"5"}),
        ("num:3 OR num:4", {"3", "4"}),
        ("num:(3 OR 4)", {"3", "4"}),
        ("num:abc", set()),  # explicit field with unparsable number: no error
        ("title:/[/", set()),
    ],
)
def test_query_string_field_syntax(client, main, query, expected):
    assert idset(client, main, QS(query)) == expected


@pytest.mark.parametrize(
    "query,expected",
    [
        ("num:[2 TO 4]", {"2", "3", "4"}),
        ("num:{2 TO 4}", {"3"}),
        ("num:[2 TO 4}", {"2", "3"}),
        ("num:{2 TO 4]", {"3", "4"}),
        ("num:[* TO 2]", {"1", "2"}),
        ("num:[3 TO *]", {"3", "4", "5"}),
        ("num:>=3", {"3", "4", "5"}),
        ("num:>3", {"4", "5"}),
        ("num:<2", {"1"}),
        ("num:<=2", {"1", "2"}),
        ("date:[2024-02-01 TO 2024-03-31]", {"2", "3"}),
        ("date:>2024-02-15", {"3", "4"}),
        ("date:2024-03-01", {"3"}),
        ("kw:[alpha TO beta]", {"2", "5"}),  # uppercase "Alpha" sorts before
        ("kw:[A TO Z]", {"1"}),
    ],
)
def test_query_string_ranges(client, main, query, expected):
    assert idset(client, main, QS(query)) == expected


@pytest.mark.parametrize(
    "query",
    ["title:(quick", "title:quick)", "a:b:c", "[fox", "fox AND", "fox^", "t*:fox"],
)
def test_query_string_invalid_syntax(client, main, query):
    err = _err(client, main, QS(query, default_field="title"), 400, "search_phase_execution_exception",
               root_type="query_shard_exception")
    assert err["root_cause"][0]["reason"] == f"Failed to parse query [{query}]"
    assert err["caused_by"]["caused_by"]["type"] == "parse_exception"


def test_query_string_invalid_syntax_not_rescued_by_lenient(client, main):
    _err(client, main, QS("title:quick)", lenient=True), 400, "search_phase_execution_exception",
         root_type="query_shard_exception")


def test_query_string_leading_wildcard_disallowed(client, main):
    q = QS("*ox", default_field="title", allow_leading_wildcard=False)
    err = _err(client, main, q, 400, "search_phase_execution_exception", root_type="query_shard_exception")
    assert err["root_cause"][0]["reason"] == "Failed to parse query [*ox]"


def test_query_string_analyze_wildcard(client, main):
    assert idset(client, main, QS("QU*", default_field="title", analyze_wildcard=True)) == {"1", "3"}
    assert idset(client, main, QS("title:Qu*", analyze_wildcard=False)) == {"1", "3"}


def test_query_string_fields_and_boost(client, main):
    assert idset(client, main, QS("fox", fields=["title", "body"])) == {"1", "3", "5"}
    assert idset(client, main, QS("sleeps", fields=["title", "body"])) == {"2", "4"}
    assert ids(client, main, QS("sleeps", fields=["title"])) == []
    assert ids(client, main, QS("dog", fields=["title^5", "body"]))[0] == "2"


@pytest.mark.parametrize(
    "fields,expected",
    [(["t*"], {"1", "3", "5"}), (["title*"], {"1", "3", "5"}), (["nomatch*"], set())],
)
def test_query_string_wildcard_fields(client, main, fields, expected):
    assert idset(client, main, QS("fox", fields=fields)) == expected


def test_query_string_no_field_searches_all(client, main):
    assert idset(client, main, QS("sleeps")) == {"2", "4"}


def test_query_string_term_boost_affects_ranking(client, main):
    plain = ids(client, main, QS("dog OR fox", default_field="title"))
    assert plain[0] == "2"  # dog is the rarer term
    boosted = ids(client, main, QS("dog OR fox^5", default_field="title"))
    assert boosted[0] == "5"
    fox_boosted = ids(client, main, QS("title:(fox)^3 OR dog", default_field="title"))
    assert fox_boosted[0] == "5"


def test_query_string_numeric_field_in_fields_errors(client, main):
    _err(client, main, QS("fox", fields=["title", "num"]), 400, "search_phase_execution_exception",
         root_type="query_shard_exception", reason_part='For input string: \\"fox\\"')


def test_query_string_numeric_field_lenient(client, main):
    assert ids(client, main, QS("abc", fields=["title", "num"], lenient=True)) == []
    assert ids(client, main, QS("2", fields=["num", "title"])) == ["2"]


def test_query_string_minimum_should_match(client, main):
    assert ids(client, main, QS("quick brown", default_field="title", minimum_should_match=2)) == ["1"]
    assert ids(client, main, QS("quick brown fox", default_field="title", minimum_should_match="100%")) == ["1"]


def test_query_string_phrase_slop_param(client, main):
    assert ids(client, main, QS('title:"brown quick"', phrase_slop=2)) == ["1"]
    assert ids(client, main, QS('title:"brown quick"')) == []


def test_query_string_fuzzy_transpositions_false(client, main):
    assert ids(client, main, QS("fxo~1", default_field="title", fuzzy_transpositions=False)) == []
    assert idset(client, main, QS("fxo~1", default_field="title")) == {"1", "3", "5"}


def test_query_string_type_phrase(client, main):
    assert ids(client, main, QS("quick fox", default_field="title", type="phrase")) == []
    assert ids(client, main, QS("quick brown", default_field="title", type="phrase")) == ["1"]


def test_query_string_cross_fields(client, main):
    q = QS("quick brown", fields=["title", "body"], type="cross_fields", default_operator="and")
    assert ids(client, main, q) == ["1"]


def test_query_string_default_field_missing(client, main):
    assert ids(client, main, QS("fox", default_field="nofield")) == []


def test_query_string_empty_phrase(client, main):
    assert ids(client, main, QS('title:""')) == []


# --------------------------------------------------------------------------
# simple_query_string
# --------------------------------------------------------------------------


def SQS(query, fields=("title",), **kw):
    body = {"query": query}
    if fields is not None:
        body["fields"] = list(fields)
    body.update(kw)
    return {"simple_query_string": body}


@pytest.mark.parametrize(
    "query,expected",
    [
        ("quick fox", {"1", "3", "5"}),
        ("quick + fox", {"1", "3"}),
        ("fox | dog", {"1", "2", "3", "5"}),
        ("fox -quick", ALL_MAIN),  # default OR: fox OR (NOT quick)
        ("-quick", {"2", "4", "5"}),
        ("-fox -dog", ALL_MAIN),
        ('"quick fox"', set()),
        ('"quick fox"~1', {"1", "3"}),
        ('"quick brown"', {"1"}),
        ("qu*", {"1", "3"}),
        ("QU*", {"1", "3"}),
        ("qick~1", {"1", "3"}),
        ("qick~", {"1", "3"}),
        ("fox~", {"1", "2", "3", "5"}),  # bare ~ means edit distance 2 here
        ("(fox | dog) + quick", {"1", "3"}),
        ("fox AND dog", {"1", "2", "3", "5"}),  # AND/OR are plain terms
        ("fox OR dog", {"1", "2", "3", "5"}),
        ("fox\\-quick", {"1", "3", "5"}),
        ("fox!", {"1", "3", "5"}),
    ],
)
def test_simple_query_string_operators(client, main, query, expected):
    assert idset(client, main, SQS(query)) == expected


@pytest.mark.parametrize(
    "query,expected",
    [
        ("title:(quick", {"1", "3"}),
        ('"unclosed', set()),
        ("fox)", {"1", "3", "5"}),
        ("+", set()),
        ('""', set()),
        ("", set()),
        ("~~~***", set()),
        ("title:fox", set()),  # no field syntax; single token "title:fox"
    ],
)
def test_simple_query_string_never_errors(client, main, query, expected):
    assert idset(client, main, SQS(query)) == expected


def test_simple_query_string_default_operator_and(client, main):
    assert idset(client, main, SQS("quick fox", default_operator="and")) == {"1", "3"}
    assert idset(client, main, SQS("fox -quick", default_operator="and")) == {"5"}


@pytest.mark.parametrize(
    "query,flags,expected",
    [
        ("qu*", "NONE", set()),
        ("qu*", "PREFIX", {"1", "3"}),
        ("qu*", "ALL", {"1", "3"}),
        ("fox -quick", "NOT", {"1", "3", "5"}),
        ("fox -quick", "OR|AND", {"1", "3", "5"}),
        ("qick~1", "FUZZY", {"1", "3"}),
        ("qick~1", "NONE", set()),
        ('"quick fox"~1', "PHRASE", set()),
        ('"quick fox"~1', "PHRASE|SLOP", {"1", "3"}),
        ("fox | dog", "NONE", {"1", "2", "3", "5"}),
    ],
)
def test_simple_query_string_flags(client, main, query, flags, expected):
    assert idset(client, main, SQS(query, flags=flags)) == expected


def test_simple_query_string_unknown_flag(client, main):
    _err(client, main, SQS("fox", flags="BOGUS"), 400, "illegal_argument_exception",
         reason_part="Unknown simple_query_string flag [BOGUS]")


def test_simple_query_string_unknown_param(client, main):
    _err(client, main, SQS("fox", foo=1), 400, "parsing_exception",
         reason_part="[simple_query_string] unsupported field [foo]")


def test_simple_query_string_fields(client, main):
    assert idset(client, main, SQS("fox", fields=["title^2", "body"])) == {"1", "3", "5"}
    assert idset(client, main, SQS("fox", fields=["title*"])) == {"1", "3", "5"}
    assert idset(client, main, SQS("fox", fields=["*"])) == {"1", "3", "5"}
    assert idset(client, main, SQS("fox", fields=None)) == {"1", "3", "5"}
    assert ids(client, main, SQS("Alpha", fields=["kw"])) == ["1"]


def test_simple_query_string_numeric_field(client, main):
    _err(client, main, SQS("abc", fields=["title", "num"]), 400, "search_phase_execution_exception",
         root_type="query_shard_exception")
    assert ids(client, main, SQS("3", fields=["title", "num"])) == ["3"]
    assert ids(client, main, SQS("abc", fields=["title", "num"], lenient=True)) == []


def test_simple_query_string_minimum_should_match(client, main):
    assert idset(client, main, SQS("quick brown fox", minimum_should_match=2)) == {"1", "3"}


def test_simple_query_string_fuzzy_transpositions(client, main):
    assert ids(client, main, SQS("fxo~1", fuzzy_transpositions=False)) == []
    assert idset(client, main, SQS("fxo~1")) == {"1", "3", "5"}


def test_simple_query_string_quote_field_suffix(client, main):
    assert ids(client, main, SQS('"quick brown fox"', quote_field_suffix=".raw")) == ["1"]
    assert ids(client, main, SQS('"quick brown"', quote_field_suffix=".raw")) == []
    assert idset(client, main, SQS("fox", quote_field_suffix=".raw")) == {"1", "3", "5"}


# --------------------------------------------------------------------------
# intervals
# --------------------------------------------------------------------------

IV_DOCS = [
    {"t": "my favourite food is cold porridge"},  # 1
    {"t": "when it's cold my favourite food is porridge"},  # 2
    {"t": "hot water and cold porridge"},  # 3
    {"t": "porridge is my favourite"},  # 4
    {"t": "the quick brown fox jumps"},  # 5
    {"t": "brown quick"},  # 6
]


@pytest.fixture(scope="module")
def iv(client):
    name = _create(client, {"properties": {"t": {"type": "text"}, "k": {"type": "keyword"}}}, IV_DOCS)
    yield name
    _drop(client, name)


def I(rule):
    return {"intervals": {"t": rule}}


@pytest.mark.parametrize(
    "rule,expected",
    [
        ({"match": {"query": "cold porridge"}}, {"1", "2", "3"}),
        ({"match": {"query": "cold porridge", "ordered": True}}, {"1", "2", "3"}),
        ({"match": {"query": "cold porridge", "ordered": True, "max_gaps": 0}}, {"1", "3"}),
        ({"match": {"query": "porridge cold", "max_gaps": 0}}, {"1", "3"}),
        ({"match": {"query": "porridge cold", "max_gaps": 0, "ordered": True}}, set()),
        ({"match": {"query": "favourite porridge", "max_gaps": 2, "ordered": True}}, {"2"}),
        ({"match": {"query": "favourite porridge", "max_gaps": 3, "ordered": True}}, {"1", "2"}),
        ({"match": {"query": "favourite porridge", "max_gaps": -1, "ordered": True}}, {"1", "2"}),
        ({"match": {"query": "porridge"}}, {"1", "2", "3", "4"}),
        ({"match": {"query": "COLD Porridge", "max_gaps": 0}}, {"1", "3"}),
        ({"match": {"query": "cold porridge", "analyzer": "keyword"}}, set()),
    ],
)
def test_intervals_match(client, iv, rule, expected):
    assert idset(client, iv, I(rule)) == expected


def test_intervals_all_of_any_of_nested(client, iv):
    rule = {
        "all_of": {
            "ordered": True,
            "intervals": [
                {"match": {"query": "my favourite food", "max_gaps": 0, "ordered": True}},
                {"any_of": {"intervals": [{"match": {"query": "hot water"}}, {"match": {"query": "cold porridge"}}]}},
            ],
        }
    }
    assert ids(client, iv, I(rule)) == ["1"]


@pytest.mark.parametrize(
    "rule,expected",
    [
        ({"any_of": {"intervals": [{"match": {"query": "hot water"}}, {"match": {"query": "quick fox", "max_gaps": 1}}]}}, {"3", "5"}),
        ({"all_of": {"intervals": [{"match": {"query": "porridge"}}, {"match": {"query": "favourite"}}]}}, {"1", "2", "4"}),
        ({"all_of": {"ordered": True, "intervals": [{"match": {"query": "porridge"}}, {"match": {"query": "favourite"}}]}}, {"4"}),
        ({"all_of": {"ordered": True, "max_gaps": 0, "intervals": [{"match": {"query": "cold"}}, {"match": {"query": "porridge"}}]}}, {"1", "3"}),
        ({"all_of": {"ordered": True, "max_gaps": 0, "intervals": [{"match": {"query": "quick"}}, {"prefix": {"prefix": "bro"}}]}}, {"5"}),
    ],
)
def test_intervals_combinators(client, iv, rule, expected):
    assert idset(client, iv, I(rule)) == expected


@pytest.mark.parametrize(
    "rule,expected",
    [
        ({"prefix": {"prefix": "porr"}}, {"1", "2", "3", "4"}),
        ({"prefix": {"prefix": "PORR"}}, {"1", "2", "3", "4"}),
        ({"wildcard": {"pattern": "p*ge"}}, {"1", "2", "3", "4"}),
        ({"wildcard": {"pattern": "c?ld"}}, {"1", "2", "3"}),
        ({"regexp": {"pattern": "porr.*"}}, {"1", "2", "3", "4"}),
        ({"fuzzy": {"term": "porrige"}}, {"1", "2", "3", "4"}),
        ({"fuzzy": {"term": "porrige", "fuzziness": 0}}, set()),
        ({"fuzzy": {"term": "qorridge", "prefix_length": 1}}, set()),
        ({"fuzzy": {"term": "qorridge"}}, {"1", "2", "3", "4"}),
    ],
)
def test_intervals_term_rules(client, iv, rule, expected):
    assert idset(client, iv, I(rule)) == expected


@pytest.mark.parametrize(
    "query,flt,expected",
    [
        ("porridge", {"before": {"match": {"query": "favourite"}}}, {"4"}),
        ("porridge", {"after": {"match": {"query": "favourite"}}}, {"1", "2"}),
        ("favourite porridge", {"not_containing": {"match": {"query": "cold"}}}, {"2", "4"}),
        ("favourite porridge", {"containing": {"match": {"query": "food"}}}, {"1", "2"}),
        ("cold", {"contained_by": {"match": {"query": "hot porridge"}}}, {"3"}),
        ("cold", {"not_contained_by": {"match": {"query": "hot porridge"}}}, {"1", "2"}),
        ("porridge", {"not_overlapping": {"match": {"query": "cold porridge", "max_gaps": 0}}}, {"2", "4"}),
        ("porridge", {"script": {"source": "interval.start > 3"}}, {"1", "2", "3"}),
    ],
)
def test_intervals_filters(client, iv, query, flt, expected):
    assert idset(client, iv, I({"match": {"query": query, "filter": flt}})) == expected


def test_intervals_unknown_rule(client, iv):
    _err(client, iv, I({"foo": {"query": "x"}}), 400, "parsing_exception",
         reason_part="Unknown interval type [foo]")


def test_intervals_two_rules_rejected(client, iv):
    _err(client, iv, I({"match": {"query": "x"}, "prefix": {"prefix": "x"}}), 400, "parsing_exception",
         reason_part="Only one interval rule can be specified, found [match] and [prefix]")


def test_intervals_on_keyword_rejected(client, iv):
    _err(client, iv, {"intervals": {"k": {"match": {"query": "x"}}}}, 400, "search_phase_execution_exception",
         root_type="query_shard_exception",
         reason_part="Can only use interval queries on text fields - not on [k] which is of type [keyword]")


def test_intervals_missing_field(client, iv):
    assert ids(client, iv, {"intervals": {"nofield": {"match": {"query": "x"}}}}) == []


def test_intervals_across_array_values_respect_gap(client, idx_misc):
    # m = ["alpha beta", "gamma delta"] -> gamma is ~100 positions after beta
    assert ids(client, idx_misc, {"intervals": {"m": {"match": {"query": "beta gamma", "max_gaps": 5}}}}) == []
    assert ids(client, idx_misc, {"intervals": {"m": {"match": {"query": "beta gamma"}}}}) == ["1"]


# --------------------------------------------------------------------------
# span queries
# --------------------------------------------------------------------------

SP_DOCS = [
    {"t": "the quick brown fox jumps", "t2": "quick"},  # 1
    {"t": "brown quick fox"},  # 2
    {"t": "quick fox brown", "t2": "brown"},  # 3
    {"t": "fox"},  # 4
    {"t": "a b c d e f quick"},  # 5
]


@pytest.fixture(scope="module")
def sp(client):
    mapping = {"properties": {"t": {"type": "text"}, "t2": {"type": "text"}, "k": {"type": "keyword"}, "n": {"type": "integer"}}}
    name = _create(client, mapping, SP_DOCS)
    yield name
    _drop(client, name)


def ST(v, field="t"):
    return {"span_term": {field: v}}


def NEAR(*clauses, **kw):
    return {"span_near": dict(clauses=list(clauses), **kw)}


def test_span_term(client, sp):
    assert idset(client, sp, ST("quick")) == {"1", "2", "3", "5"}
    assert idset(client, sp, {"span_term": {"t": {"value": "quick", "boost": 2}}}) == {"1", "2", "3", "5"}


def test_span_term_not_analyzed(client, sp):
    assert ids(client, sp, ST("Quick")) == []


@pytest.mark.parametrize(
    "a,b,slop,in_order,expected",
    [
        ("quick", "brown", 0, True, {"1"}),
        ("quick", "brown", 0, False, {"1", "2"}),
        ("quick", "brown", 1, True, {"1", "3"}),
        ("quick", "fox", 1, True, {"1", "2", "3"}),
        ("quick", "fox", 0, True, {"2", "3"}),
        ("fox", "quick", 0, True, set()),
        ("fox", "quick", 0, False, {"2", "3"}),
        ("fox", "quick", 1, False, {"1", "2", "3"}),
        ("brown", "fox", 1, True, {"1", "2"}),
        ("brown", "fox", 0, True, {"1"}),
    ],
)
def test_span_near(client, sp, a, b, slop, in_order, expected):
    q = NEAR(ST(a), ST(b), slop=slop, in_order=in_order)
    assert idset(client, sp, q) == expected


def test_span_near_defaults_slop0_in_order(client, sp):
    assert idset(client, sp, NEAR(ST("quick"), ST("fox"))) == {"2", "3"}
    assert idset(client, sp, NEAR(ST("fox"), ST("quick"))) == set()


def test_span_near_empty_clauses(client, sp):
    _err(client, sp, NEAR(slop=0), 400, "parsing_exception", reason_part="span_near must include [clauses]")


def test_span_near_non_span_clause(client, sp):
    _err(client, sp, NEAR({"term": {"t": "quick"}}, slop=0), 400, "parsing_exception",
         reason_part="span_near [clauses] must be of type span query")


def test_span_near_mixed_fields(client, sp):
    _err(client, sp, NEAR(ST("quick"), ST("brown", "t2"), slop=5), 400, "search_phase_execution_exception",
         root_type="query_shard_exception", reason_part="Cannot add clause t2:brown to SpanNearQuery for field t")


def test_span_or(client, sp):
    assert idset(client, sp, {"span_or": {"clauses": [ST("jumps"), ST("f")]}}) == {"1", "5"}


def test_span_or_mixed_fields(client, sp):
    _err(client, sp, {"span_or": {"clauses": [ST("quick"), ST("brown", "t2")]}}, 400,
         "search_phase_execution_exception", reason_part="Clauses must have same field.")


@pytest.mark.parametrize(
    "q,expected",
    [
        ({"include": ST("quick"), "exclude": NEAR(ST("quick"), ST("fox"), slop=0, in_order=True)}, {"1", "5"}),
        ({"include": ST("fox"), "exclude": ST("quick"), "pre": 1}, {"1", "4"}),
        ({"include": ST("quick"), "exclude": ST("fox"), "post": 1}, {"1", "5"}),
        ({"include": ST("quick"), "exclude": ST("brown"), "dist": 1}, {"3", "5"}),
        ({"include": ST("quick"), "exclude": ST("brown")}, {"1", "2", "3", "5"}),
    ],
)
def test_span_not(client, sp, q, expected):
    assert idset(client, sp, {"span_not": q}) == expected


@pytest.mark.parametrize(
    "term,end,expected",
    [("quick", 1, {"3"}), ("quick", 2, {"1", "2", "3"}), ("fox", 1, {"4"}), ("fox", 0, set()), ("quick", 100, {"1", "2", "3", "5"})],
)
def test_span_first(client, sp, term, end, expected):
    assert idset(client, sp, {"span_first": {"match": ST(term), "end": end}}) == expected


@pytest.mark.parametrize(
    "inner,expected",
    [
        ({"prefix": {"t": {"value": "qu"}}}, {"1", "2", "3", "5"}),
        ({"prefix": {"t": "fo"}}, {"1", "2", "3", "4"}),
        ({"wildcard": {"t": "b*n"}}, {"1", "2", "3"}),
        ({"fuzzy": {"t": "quack"}}, {"1", "2", "3", "5"}),
        ({"regexp": {"t": "j.*"}}, {"1"}),
    ],
)
def test_span_multi(client, sp, inner, expected):
    assert idset(client, sp, {"span_multi": {"match": inner}}) == expected


def test_span_multi_inside_span_near(client, sp):
    q = NEAR({"span_multi": {"match": {"prefix": {"t": "qui"}}}}, ST("fox"), slop=0, in_order=True)
    assert idset(client, sp, q) == {"2", "3"}


@pytest.mark.parametrize("inner", [{"term": {"t": "quick"}}, {"match": {"t": "quick"}}])
def test_span_multi_rejects_non_multiterm(client, sp, inner):
    _err(client, sp, {"span_multi": {"match": inner}}, 400, "parsing_exception",
         reason_part="[span_multi] [match] must be of type multi term query")


def test_span_containing(client, sp):
    big = NEAR(ST("quick"), ST("jumps"), slop=5, in_order=True)
    assert ids(client, sp, {"span_containing": {"big": big, "little": ST("fox")}}) == ["1"]
    big2 = NEAR(ST("quick"), ST("brown"), slop=2, in_order=False)
    assert ids(client, sp, {"span_containing": {"big": big2, "little": ST("fox")}}) == ["3"]


def test_span_within(client, sp):
    big = NEAR(ST("quick"), ST("brown"), slop=2, in_order=False)
    assert ids(client, sp, {"span_within": {"big": big, "little": ST("fox")}}) == ["3"]
    big2 = NEAR(ST("quick"), ST("jumps"), slop=5, in_order=True)
    assert ids(client, sp, {"span_within": {"big": big2, "little": ST("brown")}}) == ["1"]


def test_field_masking_span(client, sp):
    q = NEAR(ST("quick"), {"field_masking_span": {"query": ST("brown", "t2"), "field": "t"}}, slop=5, in_order=False)
    assert ids(client, sp, q) == ["3"]


def test_span_term_on_keyword_and_missing(client, sp):
    assert ids(client, sp, {"span_term": {"k": "x"}}) == []
    assert ids(client, sp, {"span_term": {"nofield": "x"}}) == []


def test_span_term_on_numeric_rejected(client, sp):
    _err(client, sp, {"span_term": {"n": 1}}, 400, "search_phase_execution_exception",
         root_type="query_shard_exception", reason_part="Cannot extract a term from a query of type")


def test_span_term_multiple_fields_rejected(client, sp):
    _err(client, sp, {"span_term": {"t": "quick", "t2": "x"}}, 400, "parsing_exception",
         reason_part="[span_term] query doesn't support multiple fields, found [t] and [t2]")


def test_span_queries_across_array_values(client, idx_misc):
    near = {"span_near": {"clauses": [{"span_term": {"m": "beta"}}, {"span_term": {"m": "gamma"}}]}}
    near["span_near"]["slop"] = 99
    assert ids(client, idx_misc, near) == []
    near["span_near"]["slop"] = 100
    assert ids(client, idx_misc, near) == ["1"]
    # gamma sits at position 102 (1 + gap 100 + 1)
    assert ids(client, idx_misc, {"span_first": {"match": {"span_term": {"m": "gamma"}}, "end": 102}}) == []
    assert ids(client, idx_misc, {"span_first": {"match": {"span_term": {"m": "gamma"}}, "end": 103}}) == ["1"]


# --------------------------------------------------------------------------
# misc index: synonym_graph, array positions, coercion
# --------------------------------------------------------------------------


@pytest.fixture(scope="module")
def idx_misc(client):
    mapping = {
        "properties": {
            "syn": {"type": "text", "analyzer": "standard", "search_analyzer": "sg"},
            "t": {"type": "text"},
            "m": {"type": "text"},
        }
    }
    settings = {
        "analysis": {
            "filter": {"s": {"type": "synonym_graph", "synonyms": ["ny, new york"]}},
            "analyzer": {"sg": {"tokenizer": "standard", "filter": ["lowercase", "s"]}},
        }
    }
    docs = [
        {"syn": "i love new york", "t": "abc 123", "m": ["alpha beta", "gamma delta"]},
        {"syn": "ny is great", "t": "true"},
        {"syn": "york new", "t": "abc"},
        {"syn": "new things in york"},
    ]
    name = _create(client, mapping, docs, settings)
    yield name
    _drop(client, name)


@pytest.mark.parametrize(
    "params,expected",
    [
        ({"query": "ny"}, {"1", "2"}),
        ({"query": "new york"}, {"1", "2"}),
        ({"query": "new york", "operator": "and"}, {"1", "2"}),
        ({"query": "ny", "auto_generate_synonyms_phrase_query": False}, {"1", "2", "3", "4"}),
        ({"query": "new york", "auto_generate_synonyms_phrase_query": False}, {"1", "2", "3", "4"}),
    ],
)
def test_match_auto_generate_synonyms_phrase_query(client, idx_misc, params, expected):
    assert idset(client, idx_misc, {"match": {"syn": params}}) == expected


def test_match_phrase_with_synonym_graph(client, idx_misc):
    assert idset(client, idx_misc, {"match_phrase": {"syn": "ny"}}) == {"1", "2"}


def test_match_float_on_text_field(client, idx_misc):
    assert ids(client, idx_misc, {"match": {"t": 1.5}}) == []


def test_match_and_across_array_values(client, idx_misc):
    assert ids(client, idx_misc, {"match": {"m": {"query": "beta gamma", "operator": "and"}}}) == ["1"]
    assert ids(client, idx_misc, {"match_phrase": {"m": "beta gamma"}}) == []
    assert ids(client, idx_misc, {"match_phrase": {"m": {"query": "beta gamma", "slop": 99}}}) == []
    assert ids(client, idx_misc, {"match_phrase": {"m": {"query": "beta gamma", "slop": 100}}}) == ["1"]


# --------------------------------------------------------------------------
# relevance / BM25 sanity
# --------------------------------------------------------------------------

REL_DOCS = [
    {"t": "apple"},  # 1 short
    {"t": "apple banana cherry date elder"},  # 2 long
    {"t": "kiwi kiwi kiwi lemon lime"},  # 3 high tf
    {"t": "kiwi lemon lime mango melon"},  # 4 low tf
    {"t": "zebra quokka"},  # 5 rare term
    {"t": "lemon quokka"},  # 6 common term
]


@pytest.fixture(scope="module")
def rel(client):
    mapping = {"properties": {"t": {"type": "text"}, "k": {"type": "keyword"}}}
    name = _create(client, mapping, REL_DOCS)
    yield name
    _drop(client, name)


def test_bm25_shorter_field_ranks_higher(client, rel):
    assert ids(client, rel, {"match": {"t": "apple"}}) == ["1", "2"]


def test_bm25_higher_term_frequency_ranks_higher(client, rel):
    assert ids(client, rel, {"match": {"t": "kiwi"}}) == ["3", "4"]


def test_bm25_rarer_term_ranks_higher(client, rel):
    res = ids(client, rel, {"match": {"t": "zebra lemon"}})
    assert res[0] == "5"
    assert res.index("6") > 0
    assert set(res) == {"3", "4", "5", "6"}


def test_bm25_more_matching_terms_rank_higher(client, rel):
    res = ids(client, rel, {"match": {"t": "kiwi lemon lime mango"}})
    assert res[0] == "4"


def test_query_boost_flips_ranking(client, rel):
    q = {"bool": {"should": [{"match": {"t": {"query": "lemon", "boost": 10}}}, {"match": {"t": "zebra"}}]}}
    res = ids(client, rel, q)
    assert res[-1] == "5"


def test_scores_descending_and_max_score(client, rel):
    res = _ok(client, rel, {"match": {"t": "kiwi lemon zebra"}})
    scores = [h["_score"] for h in res["hits"]["hits"]]
    assert scores == sorted(scores, reverse=True)
    assert res["hits"]["max_score"] == scores[0]
    assert all(s > 0 for s in scores)


def test_identical_docs_get_identical_scores(client):
    name = _create(client, {"properties": {"t": {"type": "text"}}}, [{"t": "same text"}, {"t": "same text"}, {"t": "other"}])
    try:
        res = scored(client, name, {"match": {"t": "same"}})
        assert {i for i, _ in res} == {"1", "2"}
        assert res[0][1] == res[1][1]
        # ties are broken by internal doc id (index order)
        assert [i for i, _ in res] == ["1", "2"]
    finally:
        _drop(client, name)


def test_explain_in_search(client, rel):
    res = _ok(client, rel, {"match": {"t": "kiwi"}}, explain=True)
    hit = res["hits"]["hits"][0]
    assert {"_shard", "_node", "_explanation"} <= set(hit)
    exp = hit["_explanation"]
    assert exp["value"] == pytest.approx(hit["_score"], rel=1e-5)
    assert exp["description"].startswith("weight(t:kiwi in ")
    assert "[PerFieldSimilarity]" in exp["description"]
    text = json.dumps(exp)
    assert "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:" in text
    assert "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:" in text


def test_explain_api_matching_doc(client, rel):
    r = client.post(f"/{rel}/_explain/3", json={"query": {"match": {"t": "kiwi"}}})
    assert r.status_code == 200
    body = r.json()
    assert body["_index"] == rel
    assert body["_id"] == "3"
    assert body["matched"] is True
    assert body["explanation"]["value"] > 0
    search_score = dict(scored(client, rel, {"match": {"t": "kiwi"}}))["3"]
    assert body["explanation"]["value"] == pytest.approx(search_score, rel=1e-5)


def test_explain_api_non_matching_doc(client, rel):
    r = client.post(f"/{rel}/_explain/1", json={"query": {"match": {"t": "kiwi"}}})
    assert r.status_code == 200
    body = r.json()
    assert body["matched"] is False
    assert body["explanation"] == {"value": 0.0, "description": "no matching term", "details": []}


def test_explain_api_missing_doc(client, rel):
    r = client.post(f"/{rel}/_explain/999", json={"query": {"match": {"t": "kiwi"}}})
    assert r.status_code == 404
    assert r.json() == {"_index": rel, "_id": "999", "matched": False}


def test_explain_bm25_components(client, rel):
    res = _ok(client, rel, {"match": {"t": "zebra"}}, explain=True)
    exp = res["hits"]["hits"][0]["_explanation"]
    inner = exp["details"][0]
    assert inner["description"] == "score(freq=1.0), computed as boost * idf * tf from:"
    idf = next(d for d in inner["details"] if d["description"].startswith("idf"))
    n_vals = {d["description"]: d["value"] for d in idf["details"]}
    assert n_vals == {"n, number of documents containing term": 1, "N, total number of documents with field": 6}
    tf = next(d for d in inner["details"] if d["description"].startswith("tf"))
    tf_vals = {d["description"]: d["value"] for d in tf["details"]}
    assert tf_vals["k1, term saturation parameter"] == pytest.approx(1.2)
    assert tf_vals["b, length normalization parameter"] == pytest.approx(0.75)
    assert tf_vals["dl, length of field"] == pytest.approx(2.0)


# --------------------------------------------------------------------------
# Unicode / standard analyzer
# --------------------------------------------------------------------------

UNI_DOCS = [
    {"t": "Café Über straße naïve", "fold": "café crème"},  # 1
    {"t": "日本語 テキスト 東京", "fold": "cafe"},  # 2
    {"t": "fox-trot e-mail user@example.com 3.14 wi-fi O'Neil", "fold": "CAFÉ"},  # 3
    {"t": "ΑΘΗΝΑ Київ", "fold": "Straße"},  # 4
]


@pytest.fixture(scope="module")
def uni(client):
    mapping = {"properties": {"t": {"type": "text"}, "fold": {"type": "text", "analyzer": "fold"}}}
    settings = {"analysis": {"analyzer": {"fold": {"tokenizer": "standard", "filter": ["lowercase", "asciifolding"]}}}}
    name = _create(client, mapping, UNI_DOCS, settings)
    yield name
    _drop(client, name)


@pytest.mark.parametrize(
    "query,expected",
    [
        ("café", {"1"}),
        ("CAFÉ", {"1"}),
        ("cafe", set()),  # standard analyzer does not fold diacritics
        ("ÜBER", {"1"}),
        ("uber", set()),
        ("straße", {"1"}),
        ("STRASSE", set()),  # no ß -> ss expansion
        ("naive", set()),
        ("日本", {"2"}),
        ("本", {"2"}),  # CJK ideographs are indexed one per token
        ("テキスト", {"2"}),
        ("東京", {"2"}),
        ("trot", {"3"}),
        ("mail", {"3"}),
        ("user", {"3"}),
        ("example.com", {"3"}),
        ("3.14", {"3"}),
        ("3", set()),
        ("wifi", set()),
        ("o'neil", {"3"}),
        ("neil", set()),
        ("αθηνα", {"4"}),
        ("київ", {"4"}),
        ("КИЇВ", {"4"}),
    ],
)
def test_unicode_standard_analyzer(client, uni, query, expected):
    assert idset(client, uni, {"match": {"t": query}}) == expected


@pytest.mark.parametrize(
    "query,expected",
    [("日本", {"2"}), ("日語", set()), ("京東", set()), ("東京", {"2"})],
)
def test_unicode_cjk_phrase(client, uni, query, expected):
    assert idset(client, uni, {"match_phrase": {"t": query}}) == expected


def test_unicode_cjk_tokens(client, uni):
    r = client.post(f"/{uni}/_analyze", json={"field": "t", "text": "日本語 テキスト 東京"})
    assert [t["token"] for t in r.json()["tokens"]] == ["日", "本", "語", "テキスト", "東", "京"]


@pytest.mark.parametrize(
    "query,expected",
    [("cafe", {"1", "2", "3"}), ("café", {"1", "2", "3"}), ("strasse", {"4"}), ("straße", {"4"})],
)
def test_unicode_asciifolding_analyzer(client, uni, query, expected):
    assert idset(client, uni, {"match": {"fold": query}}) == expected


def test_unicode_fuzzy_counts_code_points(client, uni):
    # "cafe" is one edit away from "café"
    assert ids(client, uni, {"match": {"t": {"query": "cafe", "fuzziness": 1}}}) == ["1"]
