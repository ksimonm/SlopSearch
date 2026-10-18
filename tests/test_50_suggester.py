import pytest

TEXT_MAPPING = {"properties": {"body": {"type": "text"}}}

TEXT_DOCS = [
    {"body": "mechanical keyboard"},
    {"body": "wireless keyboard"},
    {"body": "gaming mouse"},
]

COMPLETION_MAPPING = {"properties": {"suggest": {"type": "completion"}}}

COMPLETION_DOCS = [
    {"suggest": {"input": ["laptop", "notebook"], "weight": 10}},
    {"suggest": {"input": ["laptop stand"], "weight": 5}},
    {"suggest": {"input": ["mouse"], "weight": 3}},
]


@pytest.fixture
def text_index(client, index_factory, bulk_index):
    name = index_factory(mappings=TEXT_MAPPING)
    bulk_index(name, TEXT_DOCS)
    return name


@pytest.fixture
def completion_index(client, index_factory, bulk_index):
    name = index_factory(mappings=COMPLETION_MAPPING)
    bulk_index(name, COMPLETION_DOCS)
    return name


def _suggest(client, index, suggest):
    r = client.post(f"/{index}/_search", json={"suggest": suggest})
    assert r.status_code == 200, r.text
    return r.json()["suggest"]


@pytest.mark.smoke
def test_term_suggester_fixes_typo(client, text_index):
    res = _suggest(
        client,
        text_index,
        {"s": {"text": "keybord", "term": {"field": "body"}}},
    )
    texts = [o["text"] for o in res["s"][0]["options"]]
    assert "keyboard" in texts


def test_phrase_suggester(client, text_index):
    res = _suggest(
        client,
        text_index,
        {"s": {"text": "mechanical keybord", "phrase": {"field": "body"}}},
    )
    options = res["s"][0]["options"]
    assert isinstance(options, list)
    if options:
        assert any("keyboard" in o["text"] for o in options)


def test_completion_suggester(client, completion_index):
    res = _suggest(
        client,
        completion_index,
        {"s": {"prefix": "lap", "completion": {"field": "suggest"}}},
    )
    texts = [o["text"] for o in res["s"][0]["options"]]
    assert "laptop" in texts


def test_completion_suggester_ranking(client, completion_index):
    res = _suggest(
        client,
        completion_index,
        {"s": {"prefix": "lap", "completion": {"field": "suggest"}}},
    )
    scores = [o["_score"] for o in res["s"][0]["options"]]
    assert scores == sorted(scores, reverse=True)
