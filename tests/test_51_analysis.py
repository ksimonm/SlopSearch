"""Text analysis compatibility spec (OpenSearch 3.9.0 reference behaviour).

Covers the _analyze API (global and index-scoped), built-in analyzers,
language analyzers, tokenizers, token filters, char filters, normalizers and
the effect of index/search/quote analyzers on queries.
"""
import uuid

import pytest

pytestmark = pytest.mark.analysis


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def _analyze_raw(client, body, index=None):
    path = f"/{index}/_analyze" if index else "/_analyze"
    return client.post(path, json=body)


def _analyze(client, body, index=None):
    r = _analyze_raw(client, body, index)
    assert r.status_code == 200, r.text
    return r.json()


def _toks(client, body, index=None):
    return [t["token"] for t in _analyze(client, body, index)["tokens"]]


def _full(client, body, index=None):
    """(token, start, end, type, position) tuples."""
    return [
        (t["token"], t["start_offset"], t["end_offset"], t["type"], t["position"])
        for t in _analyze(client, body, index)["tokens"]
    ]


def _graph(client, body, index=None):
    """(token, start, end, type, position, positionLength-or-None) tuples."""
    return [
        (
            t["token"],
            t["start_offset"],
            t["end_offset"],
            t["type"],
            t["position"],
            t.get("positionLength"),
        )
        for t in _analyze(client, body, index)["tokens"]
    ]


def _error(resp, status, etype, reason):
    assert resp.status_code == status, resp.text
    err = resp.json()["error"]
    assert err["type"] == etype, err
    assert err["reason"] == reason, err
    assert err["root_cause"][0]["type"] == etype
    assert err["root_cause"][0]["reason"] == reason
    return err


def _ids(client, index, query):
    r = client.post(f"/{index}/_search", json={"query": query, "size": 100})
    assert r.status_code == 200, r.text
    return sorted(h["_id"] for h in r.json()["hits"]["hits"])


def _unique(prefix="a51"):
    return f"slopsearch-test-{prefix}-{uuid.uuid4().hex[:8]}"


# --------------------------------------------------------------------------
# built-in analyzers
# --------------------------------------------------------------------------

SAMPLE = "The QUICK brown-fox's e-mail: foo@bar.com, 3.14 jumped! the fox"

BUILTIN_ANALYZERS = [
    (
        "standard",
        [
            ("the", 0, 3, "<ALPHANUM>", 0),
            ("quick", 4, 9, "<ALPHANUM>", 1),
            ("brown", 10, 15, "<ALPHANUM>", 2),
            ("fox's", 16, 21, "<ALPHANUM>", 3),
            ("e", 22, 23, "<ALPHANUM>", 4),
            ("mail", 24, 28, "<ALPHANUM>", 5),
            ("foo", 30, 33, "<ALPHANUM>", 6),
            ("bar.com", 34, 41, "<ALPHANUM>", 7),
            ("3.14", 43, 47, "<NUM>", 8),
            ("jumped", 48, 54, "<ALPHANUM>", 9),
            ("the", 56, 59, "<ALPHANUM>", 10),
            ("fox", 60, 63, "<ALPHANUM>", 11),
        ],
    ),
    (
        "simple",
        [
            ("the", 0, 3, "word", 0),
            ("quick", 4, 9, "word", 1),
            ("brown", 10, 15, "word", 2),
            ("fox", 16, 19, "word", 3),
            ("s", 20, 21, "word", 4),
            ("e", 22, 23, "word", 5),
            ("mail", 24, 28, "word", 6),
            ("foo", 30, 33, "word", 7),
            ("bar", 34, 37, "word", 8),
            ("com", 38, 41, "word", 9),
            ("jumped", 48, 54, "word", 10),
            ("the", 56, 59, "word", 11),
            ("fox", 60, 63, "word", 12),
        ],
    ),
    (
        "whitespace",
        [
            ("The", 0, 3, "word", 0),
            ("QUICK", 4, 9, "word", 1),
            ("brown-fox's", 10, 21, "word", 2),
            ("e-mail:", 22, 29, "word", 3),
            ("foo@bar.com,", 30, 42, "word", 4),
            ("3.14", 43, 47, "word", 5),
            ("jumped!", 48, 55, "word", 6),
            ("the", 56, 59, "word", 7),
            ("fox", 60, 63, "word", 8),
        ],
    ),
    (
        "stop",
        [
            ("quick", 4, 9, "word", 1),
            ("brown", 10, 15, "word", 2),
            ("fox", 16, 19, "word", 3),
            ("s", 20, 21, "word", 4),
            ("e", 22, 23, "word", 5),
            ("mail", 24, 28, "word", 6),
            ("foo", 30, 33, "word", 7),
            ("bar", 34, 37, "word", 8),
            ("com", 38, 41, "word", 9),
            ("jumped", 48, 54, "word", 10),
            ("fox", 60, 63, "word", 12),
        ],
    ),
    ("keyword", [(SAMPLE, 0, 63, "word", 0)]),
    (
        "pattern",
        [
            ("the", 0, 3, "word", 0),
            ("quick", 4, 9, "word", 1),
            ("brown", 10, 15, "word", 2),
            ("fox", 16, 19, "word", 3),
            ("s", 20, 21, "word", 4),
            ("e", 22, 23, "word", 5),
            ("mail", 24, 28, "word", 6),
            ("foo", 30, 33, "word", 7),
            ("bar", 34, 37, "word", 8),
            ("com", 38, 41, "word", 9),
            ("3", 43, 44, "word", 10),
            ("14", 45, 47, "word", 11),
            ("jumped", 48, 54, "word", 12),
            ("the", 56, 59, "word", 13),
            ("fox", 60, 63, "word", 14),
        ],
    ),
    (
        "fingerprint",
        [("3.14 bar.com brown e foo fox fox's jumped mail quick the", 0, 63, "fingerprint", 0)],
    ),
]


@pytest.mark.parametrize("analyzer,expected", BUILTIN_ANALYZERS, ids=[a for a, _ in BUILTIN_ANALYZERS])
def test_builtin_analyzer_full_token_shape(client, analyzer, expected):
    assert _full(client, {"analyzer": analyzer, "text": SAMPLE}) == expected


def test_token_object_has_exactly_the_five_keys(client):
    tokens = _analyze(client, {"analyzer": "standard", "text": "Hello"})["tokens"]
    assert tokens == [
        {"token": "hello", "start_offset": 0, "end_offset": 5, "type": "<ALPHANUM>", "position": 0}
    ]


def test_default_analyzer_is_standard_when_nothing_given(client):
    assert _full(client, {"text": "Hello World"}) == [
        ("hello", 0, 5, "<ALPHANUM>", 0),
        ("world", 6, 11, "<ALPHANUM>", 1),
    ]


def test_empty_text_yields_no_tokens(client):
    assert _analyze(client, {"analyzer": "standard", "text": ""}) == {"tokens": []}


def test_whitespace_only_text_yields_no_tokens(client):
    assert _analyze(client, {"analyzer": "standard", "text": "   \t\n "}) == {"tokens": []}


def test_standard_analyzer_long_token_split_at_255(client):
    toks = _full(client, {"analyzer": "standard", "text": "x" * 600})
    assert [(t[1], t[2], t[4]) for t in toks] == [(0, 255, 0), (255, 510, 1), (510, 600, 2)]
    assert toks[2][0] == "x" * 90


def test_get_analyze_with_json_body(client):
    r = client.request("GET", "/_analyze", json={"analyzer": "whitespace", "text": "Hello World"})
    assert r.status_code == 200
    assert [t["token"] for t in r.json()["tokens"]] == ["Hello", "World"]


# --------------------------------------------------------------------------
# language analyzers
# --------------------------------------------------------------------------

LANGUAGE_ANALYZERS = [
    ("english", "The foxes were running quickly through the libraries' gardens",
     ["fox", "were", "run", "quickli", "through", "librari", "garden"]),
    ("russian", "Красивые девушки бегали по зелёным полям", ["красив", "девушк", "бега", "зелен", "пол"]),
    ("german", "Die Häuser und Bäume standen im Straßenverkehr", ["haus", "baum", "stand", "strassenverkehr"]),
    ("french", "L'avion volait au-dessus des montagnes enneigées",
     ["avion", "volait", "desu", "montagn", "eneig"]),
    ("spanish", "Los niños corrían rápidamente por las calles", ["niñ", "corrian", "rapidament", "call"]),
    ("italian", "Le ragazze cantavano canzoni bellissime nell'aria",
     ["ragazz", "cantavan", "canzon", "bellissim", "aria"]),
    ("portuguese", "As crianças brincavam alegremente nas praias", ["crianc", "brincavam", "alegr", "prai"]),
    ("brazilian", "As crianças brincavam", ["crianc", "brinc"]),
    ("dutch", "De kinderen fietsten snel door de straten", ["kinder", "fietst", "snel", "strat"]),
    ("arabic", "الكتاب الجديد في المكتبة", ["كتاب", "جديد", "مكتب"]),
    ("greek", "Οι γάτες έτρεχαν γρήγορα", ["γατ", "ετρεχ", "γρηγορ"]),
    ("turkish", "İstanbul'da güzel kitaplar", ["istanbul", "güzel", "kitap"]),
    ("swedish", "Barnen sprang snabbt", ["barn", "sprang", "snabbt"]),
    ("norwegian", "Barna løp fort", ["barn", "løp", "fort"]),
    ("danish", "Børnene løb hurtigt", ["børn", "løb", "hurt"]),
    ("finnish", "Talot ja kadut", ["talo", "kadu"]),
    ("hungarian", "A gyerekek futottak", ["gyerek", "futott"]),
    ("romanian", "Copiii alergau repede", ["copii", "alerg", "reped"]),
    ("czech", "Děti běhaly rychle", ["dět", "běhal", "rychl"]),
    ("bulgarian", "Децата тичаха бързо", ["дец", "тичах", "бърз"]),
    ("hindi", "बच्चे खेल रहे हैं", ["बचच", "खेल"]),
    ("persian", "کتاب‌های خوب", ["كتاب", "خوب"]),
    ("sorani", "کوردستان", ["کوردس"]),
    # precomposed YYA in output (normalization folds YA + NUKTA)
    ("bengali", "আমি বাংলায় গান গাই", ["আম", "\u09ac\u09be\u0982\u09b2\u09be\u09df", "গান", "গাই"]),
    ("thai", "ภาษาไทยง่ายนิดเดียว", ["ภาษา", "ไทย", "ง่าย", "นิดเดียว"]),
    ("irish", "Na hÉireannaigh tAthair", ["héireannaigh", "athair"]),
    ("catalan", "Els nens corrien", ["nen", "corr"]),
    ("basque", "Haurrak korrika", ["haurr", "korri"]),
    ("galician", "Os nenos corrían", ["nen", "corri"]),
    ("armenian", "Երեխաները վազում էին", ["երեխ", "վազ"]),
    ("estonian", "Lapsed jooksid", ["lapse", "jook"]),
    ("lithuanian", "Vaikai bėgo", ["vaik", "bėg"]),
    ("latvian", "Bērni skrēja", ["bērn", "skrēj"]),
    ("indonesian", "Anak-anak berlari dengan cepat", ["anak", "anak", "lari", "cepat"]),
    ("cjk", "我爱北京天安门 東京タワー",
     ["我爱", "爱北", "北京", "京天", "天安", "安门", "東京", "京タ", "タワ", "ワー"]),
]


@pytest.mark.parametrize(
    "analyzer,text,expected", LANGUAGE_ANALYZERS, ids=[c[0] for c in LANGUAGE_ANALYZERS]
)
def test_language_analyzer_stems(client, analyzer, text, expected):
    assert _toks(client, {"analyzer": analyzer, "text": text}) == expected


@pytest.mark.parametrize("analyzer", ["ukrainian", "polish", "japanese", "kuromoji", "smartcn", "nori"])
def test_plugin_language_analyzers_absent_in_min_distribution(client, analyzer):
    r = _analyze_raw(client, {"analyzer": analyzer, "text": "x"})
    _error(r, 400, "illegal_argument_exception", f"failed to find global analyzer [{analyzer}]")


def test_english_analyzer_positions_skip_stopwords(client):
    assert _full(client, {"analyzer": "english", "text": "the cats are running"}) == [
        ("cat", 4, 8, "<ALPHANUM>", 1),
        ("run", 13, 20, "<ALPHANUM>", 3),
    ]


def test_english_analyzer_stem_exclusion(client, index_factory):
    name = index_factory(
        settings={
            "analysis": {
                "analyzer": {
                    "en": {"type": "english", "stem_exclusion": ["running"], "stopwords": ["foxes"]}
                }
            }
        }
    )
    assert _toks(client, {"analyzer": "en", "text": "the running foxes jumped"}, name) == [
        "the",
        "running",
        "jump",
    ]


def test_cjk_analyzer_mixed_scripts(client):
    assert _full(client, {"analyzer": "cjk", "text": "日本語 ABC 한국"}) == [
        ("日本", 0, 2, "<DOUBLE>", 0),
        ("本語", 1, 3, "<DOUBLE>", 1),
        ("abc", 4, 7, "<ALPHANUM>", 2),
        ("한국", 8, 10, "<DOUBLE>", 3),
    ]


# --------------------------------------------------------------------------
# standard tokenizer corner cases
# --------------------------------------------------------------------------

STANDARD_TOKENIZER = [
    ("emails_urls", "john.doe@example.com visit https://example.com/path?q=1", [
        ("john.doe", 0, 8, "<ALPHANUM>", 0),
        ("example.com", 9, 20, "<ALPHANUM>", 1),
        ("visit", 21, 26, "<ALPHANUM>", 2),
        ("https", 27, 32, "<ALPHANUM>", 3),
        ("example.com", 35, 46, "<ALPHANUM>", 4),
        ("path", 47, 51, "<ALPHANUM>", 5),
        ("q", 52, 53, "<ALPHANUM>", 6),
        ("1", 54, 55, "<NUM>", 7),
    ]),
    ("numbers", "1,234.56 and 3.14 and 192.168.0.1 and v2.0", [
        ("1,234.56", 0, 8, "<NUM>", 0),
        ("and", 9, 12, "<ALPHANUM>", 1),
        ("3.14", 13, 17, "<NUM>", 2),
        ("and", 18, 21, "<ALPHANUM>", 3),
        ("192.168.0.1", 22, 33, "<NUM>", 4),
        ("and", 34, 37, "<ALPHANUM>", 5),
        ("v2.0", 38, 42, "<ALPHANUM>", 6),
    ]),
    ("apostrophes", "don't O'Reilly's rock'n'roll", [
        ("don't", 0, 5, "<ALPHANUM>", 0),
        ("O'Reilly's", 6, 16, "<ALPHANUM>", 1),
        ("rock'n'roll", 17, 28, "<ALPHANUM>", 2),
    ]),
    ("hyphens_underscores", "wi-fi state-of-the-art foo_bar snake_case_name", [
        ("wi", 0, 2, "<ALPHANUM>", 0),
        ("fi", 3, 5, "<ALPHANUM>", 1),
        ("state", 6, 11, "<ALPHANUM>", 2),
        ("of", 12, 14, "<ALPHANUM>", 3),
        ("the", 15, 18, "<ALPHANUM>", 4),
        ("art", 19, 22, "<ALPHANUM>", 5),
        ("foo_bar", 23, 30, "<ALPHANUM>", 6),
        ("snake_case_name", 31, 46, "<ALPHANUM>", 7),
    ]),
    ("emoji", "I ❤️ 🍕 pizza 👍🏽", [
        ("I", 0, 1, "<ALPHANUM>", 0),
        ("❤️", 2, 4, "<EMOJI>", 1),
        ("🍕", 5, 7, "<EMOJI>", 2),
        ("pizza", 8, 13, "<ALPHANUM>", 3),
        ("👍🏽", 14, 18, "<EMOJI>", 4),
    ]),
    ("cjk_scripts", "東京タワー 서울 ภาษาไทย", [
        ("東", 0, 1, "<IDEOGRAPHIC>", 0),
        ("京", 1, 2, "<IDEOGRAPHIC>", 1),
        ("タワー", 2, 5, "<KATAKANA>", 2),
        ("서울", 6, 8, "<HANGUL>", 3),
        ("ภาษาไทย", 9, 16, "<SOUTHEAST_ASIAN>", 4),
    ]),
    ("punctuation_ws", "hello...world  tab\there\nnew", [
        ("hello", 0, 5, "<ALPHANUM>", 0),
        ("world", 8, 13, "<ALPHANUM>", 1),
        ("tab", 15, 18, "<ALPHANUM>", 2),
        ("here", 19, 23, "<ALPHANUM>", 3),
        ("new", 24, 27, "<ALPHANUM>", 4),
    ]),
    ("symbols", "$100 50% #hashtag @mention C++ C#", [
        ("100", 1, 4, "<NUM>", 0),
        ("50", 5, 7, "<NUM>", 1),
        ("hashtag", 10, 17, "<ALPHANUM>", 2),
        ("mention", 19, 26, "<ALPHANUM>", 3),
        ("C", 27, 28, "<ALPHANUM>", 4),
        ("C", 31, 32, "<ALPHANUM>", 5),
    ]),
]


@pytest.mark.parametrize("case,text,expected", STANDARD_TOKENIZER, ids=[c[0] for c in STANDARD_TOKENIZER])
def test_standard_tokenizer_corner_cases(client, case, text, expected):
    assert _full(client, {"tokenizer": "standard", "text": text}) == expected


def test_standard_tokenizer_preserves_case(client):
    assert _toks(client, {"tokenizer": "standard", "text": "HeLLo WORLD"}) == ["HeLLo", "WORLD"]


def test_standard_tokenizer_max_token_length(client):
    body = {"tokenizer": {"type": "standard", "max_token_length": 5}, "text": "abcdefghijkl xyz"}
    assert _full(client, body) == [
        ("abcde", 0, 5, "<ALPHANUM>", 0),
        ("fghij", 5, 10, "<ALPHANUM>", 1),
        ("kl", 10, 12, "<ALPHANUM>", 2),
        ("xyz", 13, 16, "<ALPHANUM>", 3),
    ]


def test_offsets_are_utf16_code_units(client):
    assert _full(client, {"tokenizer": "whitespace", "text": "😀a b"}) == [
        ("😀a", 0, 3, "word", 0),
        ("b", 4, 5, "word", 1),
    ]


# --------------------------------------------------------------------------
# other built-in tokenizers (names)
# --------------------------------------------------------------------------

NAMED_TOKENIZERS = [
    ("letter", "it's 42 jumbo-jet über_cool", [
        ("it", 0, 2, "word", 0), ("s", 3, 4, "word", 1), ("jumbo", 8, 13, "word", 2),
        ("jet", 14, 17, "word", 3), ("über", 18, 22, "word", 4), ("cool", 23, 27, "word", 5),
    ]),
    ("lowercase", "The QUICK Brown-Fox 42", [
        ("the", 0, 3, "word", 0), ("quick", 4, 9, "word", 1),
        ("brown", 10, 15, "word", 2), ("fox", 16, 19, "word", 3),
    ]),
    ("whitespace", "  multiple   spaces\tand\nnewlines  ", [
        ("multiple", 2, 10, "word", 0), ("spaces", 13, 19, "word", 1),
        ("and", 20, 23, "word", 2), ("newlines", 24, 32, "word", 3),
    ]),
    ("uax_url_email", "Email john.doe@example.com or visit https://example.com/a?b=c now", [
        ("Email", 0, 5, "<ALPHANUM>", 0),
        ("john.doe@example.com", 6, 26, "<EMAIL>", 1),
        ("or", 27, 29, "<ALPHANUM>", 2),
        ("visit", 30, 35, "<ALPHANUM>", 3),
        ("https://example.com/a?b=c", 36, 61, "<URL>", 4),
        ("now", 62, 65, "<ALPHANUM>", 5),
    ]),
    ("classic", "john.doe@example.com Wi-Fi XY-1234 O'Reilly's I.B.M. AT&T 3.14", [
        ("john.doe@example.com", 0, 20, "<EMAIL>", 0),
        ("Wi", 21, 23, "<ALPHANUM>", 1),
        ("Fi", 24, 26, "<ALPHANUM>", 2),
        ("XY-1234", 27, 34, "<NUM>", 3),
        ("O'Reilly's", 35, 45, "<APOSTROPHE>", 4),
        ("I.B.M.", 46, 52, "<ACRONYM>", 5),
        ("AT&T", 53, 57, "<COMPANY>", 6),
        ("3.14", 58, 62, "<HOST>", 7),
    ]),
    ("keyword", "Keep  It  Whole", [("Keep  It  Whole", 0, 15, "word", 0)]),
    ("thai", "ภาษาไทยง่ายนิดเดียว test", [
        ("ภาษา", 0, 4, "word", 0), ("ไทย", 4, 7, "word", 1), ("ง่าย", 7, 11, "word", 2),
        ("นิดเดียว", 11, 19, "word", 3), ("test", 20, 24, "word", 4),
    ]),
    ("path_hierarchy", "/usr/local/bin", [
        ("/usr", 0, 4, "word", 0), ("/usr/local", 0, 10, "word", 1), ("/usr/local/bin", 0, 14, "word", 2),
    ]),
    ("ngram", "a b", [
        ("a", 0, 1, "word", 0), ("a ", 0, 2, "word", 1), (" ", 1, 2, "word", 2),
        (" b", 1, 3, "word", 3), ("b", 2, 3, "word", 4),
    ]),
    ("edge_ngram", "ab cd", [("a", 0, 1, "word", 0), ("ab", 0, 2, "word", 1)]),
    ("char_group", "a-b c", [("a-b c", 0, 5, "word", 0)]),
]


@pytest.mark.parametrize("tokenizer,text,expected", NAMED_TOKENIZERS, ids=[c[0] for c in NAMED_TOKENIZERS])
def test_named_tokenizer(client, tokenizer, text, expected):
    assert _full(client, {"tokenizer": tokenizer, "text": text}) == expected


# --------------------------------------------------------------------------
# inline tokenizer definitions
# --------------------------------------------------------------------------

INLINE_TOKENIZERS = [
    ("ngram_default", {"type": "ngram"}, "Quick", [
        ("Q", 0, 1, "word", 0), ("Qu", 0, 2, "word", 1), ("u", 1, 2, "word", 2), ("ui", 1, 3, "word", 3),
        ("i", 2, 3, "word", 4), ("ic", 2, 4, "word", 5), ("c", 3, 4, "word", 6), ("ck", 3, 5, "word", 7),
        ("k", 4, 5, "word", 8),
    ]),
    ("ngram_2_3", {"type": "ngram", "min_gram": 2, "max_gram": 3}, "abcd", [
        ("ab", 0, 2, "word", 0), ("abc", 0, 3, "word", 1), ("bc", 1, 3, "word", 2),
        ("bcd", 1, 4, "word", 3), ("cd", 2, 4, "word", 4),
    ]),
    ("ngram_token_chars", {"type": "ngram", "min_gram": 2, "max_gram": 3, "token_chars": ["letter", "digit"]},
     "ab-12 c", [("ab", 0, 2, "word", 0), ("12", 3, 5, "word", 1)]),
    ("ngram_custom_token_chars",
     {"type": "ngram", "min_gram": 2, "max_gram": 2, "token_chars": ["letter", "custom"], "custom_token_chars": "+"},
     "c++ x", [("c+", 0, 2, "word", 0), ("++", 1, 3, "word", 1)]),
    ("ngram_custom_chars_ignored_without_custom_class",
     {"type": "ngram", "min_gram": 3, "max_gram": 3, "token_chars": ["letter"], "custom_token_chars": "+-"},
     "c++ a-b", []),
    ("edge_ngram_default", {"type": "edge_ngram"}, "Quick", [("Q", 0, 1, "word", 0), ("Qu", 0, 2, "word", 1)]),
    ("edge_ngram_letters", {"type": "edge_ngram", "min_gram": 2, "max_gram": 4, "token_chars": ["letter"]},
     "Hello World 42", [
         ("He", 0, 2, "word", 0), ("Hel", 0, 3, "word", 1), ("Hell", 0, 4, "word", 2),
         ("Wo", 6, 8, "word", 3), ("Wor", 6, 9, "word", 4), ("Worl", 6, 10, "word", 5),
     ]),
    ("pattern_default_W", {"type": "pattern"}, "foo,bar baz_qux", [
        ("foo", 0, 3, "word", 0), ("bar", 4, 7, "word", 1), ("baz_qux", 8, 15, "word", 2),
    ]),
    ("pattern_comma_drops_empty", {"type": "pattern", "pattern": ","}, "a,b,,c, d", [
        ("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1), ("c", 5, 6, "word", 2), (" d", 7, 9, "word", 3),
    ]),
    ("pattern_group_1", {"type": "pattern", "pattern": "\"((?:\\\\\"|[^\"]|\\\\\")+)\"", "group": 1},
     'comment "hello world" and "x y"', [("hello world", 9, 20, "word", 0), ("x y", 27, 30, "word", 1)]),
    ("pattern_group_0", {"type": "pattern", "pattern": "[a-z]+", "group": 0}, "abc123def", [
        ("abc", 0, 3, "word", 0), ("def", 6, 9, "word", 1),
    ]),
    ("pattern_flags", {"type": "pattern", "pattern": "X", "flags": "CASE_INSENSITIVE"}, "aXbxc", [
        ("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1), ("c", 4, 5, "word", 2),
    ]),
    ("simple_pattern", {"type": "simple_pattern", "pattern": "[0123456789]{3}"}, "fd-786-335-514-x", [
        ("786", 3, 6, "word", 0), ("335", 7, 10, "word", 1), ("514", 11, 14, "word", 2),
    ]),
    ("simple_pattern_split", {"type": "simple_pattern_split", "pattern": "_"}, "an_underscored_phrase", [
        ("an", 0, 2, "word", 0), ("underscored", 3, 14, "word", 1), ("phrase", 15, 21, "word", 2),
    ]),
    ("simple_pattern_split_plus", {"type": "simple_pattern_split", "pattern": "_+"}, "a__b___c_", [
        ("a", 0, 1, "word", 0), ("b", 3, 4, "word", 1), ("c", 7, 8, "word", 2),
    ]),
    ("char_group_chars", {"type": "char_group", "tokenize_on_chars": ["whitespace", "-", "\n"]},
     "The QUICK brown-fox", [
         ("The", 0, 3, "word", 0), ("QUICK", 4, 9, "word", 1), ("brown", 10, 15, "word", 2), ("fox", 16, 19, "word", 3),
     ]),
    ("char_group_classes", {"type": "char_group", "tokenize_on_chars": ["punctuation", "digit"]}, "a.b1c,d", [
        ("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1), ("c", 4, 5, "word", 2), ("d", 6, 7, "word", 3),
    ]),
    ("char_group_max_len", {"type": "char_group", "tokenize_on_chars": ["whitespace"], "max_token_length": 3},
     "abcdefg hi", [
         ("abc", 0, 3, "word", 0), ("def", 3, 6, "word", 1), ("g", 6, 7, "word", 2), ("hi", 8, 10, "word", 3),
     ]),
    ("path_skip_replacement", {"type": "path_hierarchy", "delimiter": "-", "replacement": "/", "skip": 2},
     "one-two-three-four-five", [
         ("/three", 7, 13, "word", 0), ("/three/four", 7, 18, "word", 1), ("/three/four/five", 7, 23, "word", 2),
     ]),
    ("path_reverse", {"type": "path_hierarchy", "reverse": True, "delimiter": "."}, "www.example.com", [
        ("www.example.com", 0, 15, "word", 0), ("example.com", 4, 15, "word", 1), ("com", 12, 15, "word", 2),
    ]),
    ("path_reverse_skip", {"type": "path_hierarchy", "reverse": True, "delimiter": ".", "skip": 1},
     "www.example.com", [("www.example.", 0, 12, "word", 0), ("example.", 4, 12, "word", 1)]),
    ("path_trailing_delim", {"type": "path_hierarchy"}, "a/b/", [
        ("a", 0, 1, "word", 0), ("a/b", 0, 3, "word", 1), ("a/b/", 0, 4, "word", 2),
    ]),
    ("path_double_delim", {"type": "path_hierarchy"}, "/a//b", [
        ("/a", 0, 2, "word", 0), ("/a/", 0, 3, "word", 1), ("/a//b", 0, 5, "word", 2),
    ]),
    ("uax_max_len", {"type": "uax_url_email", "max_token_length": 5}, "john@x.io abcdefgh", [
        ("john", 0, 4, "<ALPHANUM>", 0), ("x.io", 5, 9, "<URL>", 1),
        ("abcde", 10, 15, "<ALPHANUM>", 2), ("fgh", 15, 18, "<ALPHANUM>", 3),
    ]),
    ("classic_max_len_drops", {"type": "classic", "max_token_length": 4}, "abcdefg hi", [
        ("hi", 8, 10, "<ALPHANUM>", 1),
    ]),
    ("whitespace_max_len_splits", {"type": "whitespace", "max_token_length": 3}, "abcdefg hi", [
        ("abc", 0, 3, "word", 0), ("def", 3, 6, "word", 1), ("g", 6, 7, "word", 2), ("hi", 8, 10, "word", 3),
    ]),
    ("letter_unicode", {"type": "letter"}, "naïve façade", [
        ("naïve", 0, 5, "word", 0), ("façade", 6, 12, "word", 1),
    ]),
    ("keyword_buffer_size", {"type": "keyword", "buffer_size": 2}, "abcdef", [("abcdef", 0, 6, "word", 0)]),
]


@pytest.mark.parametrize("case,tokenizer,text,expected", INLINE_TOKENIZERS, ids=[c[0] for c in INLINE_TOKENIZERS])
def test_inline_tokenizer(client, case, tokenizer, text, expected):
    assert _full(client, {"tokenizer": tokenizer, "text": text}) == expected


def test_ngram_max_diff_enforced_globally(client):
    r = _analyze_raw(client, {"tokenizer": {"type": "ngram", "min_gram": 1, "max_gram": 3}, "text": "abcd"})
    _error(
        r, 400, "illegal_argument_exception",
        "The difference between max_gram and min_gram in NGram Tokenizer must be less than or equal to: [1] "
        "but was [2]. This limit can be set by changing the [index.max_ngram_diff] index level setting.",
    )


def test_edge_ngram_min_greater_than_max(client):
    r = _analyze_raw(client, {"tokenizer": {"type": "edge_ngram", "min_gram": 3, "max_gram": 2}, "text": "ab"})
    _error(r, 400, "illegal_argument_exception", "minGram must not be greater than maxGram")


def test_ngram_custom_token_chars_required(client):
    body = {"tokenizer": {"type": "ngram", "min_gram": 2, "max_gram": 2, "token_chars": ["custom"]}, "text": "ab"}
    _error(
        _analyze_raw(client, body), 400, "illegal_argument_exception",
        "Token type: 'custom' requires setting `custom_token_chars`",
    )


def test_ngram_unknown_token_chars_class(client):
    body = {"tokenizer": {"type": "ngram", "min_gram": 2, "max_gram": 2, "token_chars": ["bogus"]}, "text": "ab"}
    r = _analyze_raw(client, body)
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"].startswith("Unknown token type: 'bogus', must be one of [")


def test_pattern_tokenizer_invalid_regex(client):
    r = _analyze_raw(client, {"tokenizer": {"type": "pattern", "pattern": "("}, "text": "x"})
    _error(r, 400, "pattern_syntax_exception", "Unclosed group near index 1\n(")


def test_ngram_tokenizer_with_raised_max_ngram_diff_in_index(client, index_factory):
    name = index_factory(
        settings={
            "index.max_ngram_diff": 2,
            "analysis": {"tokenizer": {"tri": {"type": "ngram", "min_gram": 1, "max_gram": 3}}},
        }
    )
    assert _toks(client, {"tokenizer": "tri", "text": "abc"}, name) == [
        "a", "ab", "abc", "b", "bc", "c",
    ]


# --------------------------------------------------------------------------
# token filters: case, folding, stop
# --------------------------------------------------------------------------

CASE_FILTERS = [
    ("greek_lowercase", [{"type": "lowercase", "language": "greek"}], "ΟΔΥΣΣΕΥΣ", ["οδυσσευσ"]),
    ("plain_lowercase_greek", ["lowercase"], "ΟΔΥΣΣΕΥΣ", ["οδυσσευσ"]),
    ("turkish_lowercase", [{"type": "lowercase", "language": "turkish"}], "ISTANBUL İstanbul",
     ["ıstanbul", "istanbul"]),
    ("plain_lowercase_turkish", ["lowercase"], "ISTANBUL İstanbul", ["istanbul", "istanbul"]),
    ("irish_lowercase", [{"type": "lowercase", "language": "irish"}], "nAthair tAthair", ["n-athair", "t-athair"]),
    ("uppercase", ["uppercase"], "straße über", ["STRAßE", "ÜBER"]),
    ("uppercase_ligatures", ["uppercase"], "ﬁ ß ǆ", ["ﬁ", "ß", "Ǆ"]),
    ("lowercase_titlecase_digraph", ["lowercase"], "ǅungla ΣΊΣΥΦΟΣ", ["ǆungla", "σίσυφοσ"]),
    ("asciifolding", ["asciifolding"], "Açaí à la carte Ærøskøbing ﬁ ß",
     ["Acai", "a", "la", "carte", "AEroskobing", "fi", "ss"]),
    ("asciifolding_then_lowercase", ["asciifolding", "lowercase"], "ÀÉÎÕÜ Ñ Ø Œ Đ Ł ẞ",
     ["aeiou", "n", "o", "oe", "d", "l", "ss"]),
    ("german_normalization", ["german_normalization"], "Schäfer Straße", ["Schafer", "Strasse"]),
    ("scandinavian_folding", ["scandinavian_folding"], "blåbærsyltetøj", ["blabarsyltetoj"]),
    ("scandinavian_normalization", ["scandinavian_normalization"], "blåbærsyltetøj", ["blåbærsyltetøj"]),
    ("arabic_normalization", ["arabic_normalization"], "أحمد", ["احمد"]),
    ("serbian_normalization", ["serbian_normalization"], "ђура", ["djura"]),
    ("sorani_normalization", ["sorani_normalization"], "ي", ["ی"]),
    ("decimal_digit", ["decimal_digit"], "٣٤ ४२ 12", ["34", "42", "12"]),
]


@pytest.mark.parametrize("case,filters,text,expected", CASE_FILTERS, ids=[c[0] for c in CASE_FILTERS])
def test_normalizing_filters(client, case, filters, text, expected):
    assert _toks(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


def test_asciifolding_preserve_original_order_and_positions(client):
    body = {
        "tokenizer": "whitespace",
        "filter": [{"type": "asciifolding", "preserve_original": True}],
        "text": "café naive",
    }
    assert _full(client, body) == [
        ("cafe", 0, 4, "word", 0),
        ("café", 0, 4, "word", 0),
        ("naive", 5, 10, "word", 1),
    ]


STOP_FILTERS = [
    ("default_english_case_sensitive", ["stop"], "The quick and the dead is not a fox", ["The", "quick", "dead", "fox"]),
    ("custom_list", [{"type": "stop", "stopwords": ["foo", "Bar"]}], "foo bar Bar baz", ["bar", "baz"]),
    ("ignore_case", [{"type": "stop", "stopwords": ["foo", "bar"], "ignore_case": True}], "FOO Bar baz", ["baz"]),
    ("keep_trailing", [{"type": "stop", "stopwords": "_english_", "remove_trailing": False}], "a fox in the",
     ["fox", "the"]),
    ("remove_trailing_default", [{"type": "stop", "stopwords": "_english_"}], "a fox in the", ["fox"]),
    ("russian_list", [{"type": "stop", "stopwords": "_russian_"}], "я и ты не кот", ["кот"]),
    ("none_list", [{"type": "stop", "stopwords": "_none_"}], "the a", ["the", "a"]),
]


@pytest.mark.parametrize("case,filters,text,expected", STOP_FILTERS, ids=[c[0] for c in STOP_FILTERS])
def test_stop_filter(client, case, filters, text, expected):
    assert _toks(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


def test_stop_filter_leaves_position_holes(client):
    body = {"tokenizer": "whitespace", "filter": ["stop"], "text": "fox in the hole"}
    assert _full(client, body) == [("fox", 0, 3, "word", 0), ("hole", 11, 15, "word", 3)]


def test_stop_filter_missing_stopwords_path(client):
    body = {"tokenizer": "standard", "filter": [{"type": "stop", "stopwords_path": "nope.txt"}], "text": "x"}
    _error(_analyze_raw(client, body), 400, "illegal_argument_exception",
           "IOException while reading stopwords_path: file not readable")


# --------------------------------------------------------------------------
# stemmers
# --------------------------------------------------------------------------

STEM_TEXT = "running ponies caresses relational generalization"

STEMMERS = [
    ("porter_stem", ["porter_stem"], STEM_TEXT, ["run", "poni", "caress", "relat", "gener"]),
    ("kstem", ["kstem"], STEM_TEXT, ["running", "pony", "caress", "relational", "generalization"]),
    ("snowball_english", [{"type": "snowball", "language": "English"}], STEM_TEXT,
     ["run", "poni", "caress", "relat", "general"]),
    ("stemmer_default", ["stemmer"], "running ponies", ["run", "poni"]),
    ("stemmer_english", [{"type": "stemmer", "language": "english"}], "running ponies caresses relational",
     ["run", "poni", "caress", "relat"]),
    ("light_english", [{"type": "stemmer", "language": "light_english"}], "running ponies caresses relational",
     ["running", "pony", "caress", "relational"]),
    ("minimal_english", [{"type": "stemmer", "language": "minimal_english"}], "running ponies caresses relational",
     ["running", "pony", "caresse", "relational"]),
    ("possessive_english", [{"type": "stemmer", "language": "possessive_english"}], "John's dogs' cat's",
     ["John", "dogs", "cat"]),
    ("porter2", [{"type": "stemmer", "language": "porter2"}], "running ponies caresses relational generously",
     ["run", "poni", "caress", "relat", "generous"]),
    ("light_german", [{"type": "stemmer", "language": "light_german"}], "häuser bäume", ["haus", "baum"]),
    ("russian", [{"type": "stemmer", "language": "russian"}], "красивые девушки", ["красив", "девушк"]),
    ("light_russian", [{"type": "stemmer", "language": "light_russian"}], "красивые девушки", ["красив", "девушк"]),
    ("name_alias", [{"type": "stemmer", "name": "light_french"}], "chevaux", ["cheval"]),
    ("light_spanish", [{"type": "stemmer", "language": "light_spanish"}], "niños", ["niñ"]),
    ("minimal_portuguese", [{"type": "stemmer", "language": "minimal_portuguese"}], "crianças", ["criança"]),
    ("arabic", [{"type": "stemmer", "language": "arabic"}], "الكتاب", ["كتاب"]),
    ("snowball_russian", [{"type": "snowball", "language": "Russian"}], "бегали", ["бега"]),
    ("snowball_german2", [{"type": "snowball", "language": "German2"}], "Häuser", ["Haus"]),
    ("porter_is_case_sensitive", ["porter_stem"], "Running", ["Run"]),
    ("kstem_then_porter", ["kstem", "porter_stem"], "ponies", ["poni"]),
    ("russian_stem", ["russian_stem"], "красивые", ["красив"]),
    ("czech_stem", ["czech_stem"], "děti", ["dět"]),
    ("french_stem", ["french_stem"], "chevaux", ["cheval"]),
    ("german_stem", ["german_stem"], "häuser", ["hau"]),
    ("dutch_stem", ["dutch_stem"], "kinderen", ["kinder"]),
    ("brazilian_stem", ["brazilian_stem"], "crianças", ["crianc"]),
]


@pytest.mark.parametrize("case,filters,text,expected", STEMMERS, ids=[c[0] for c in STEMMERS])
def test_stemmer_filters(client, case, filters, text, expected):
    assert _toks(client, {"tokenizer": "standard", "filter": filters, "text": text}) == expected


@pytest.mark.parametrize("language,cls", [("klingon", "Klingon"), ("lovins", "Lovins"), ("ukrainian", "Ukrainian")])
def test_stemmer_unknown_language(client, language, cls):
    body = {"tokenizer": "standard", "filter": [{"type": "stemmer", "language": language}], "text": "x"}
    err = _error(_analyze_raw(client, body), 400, "illegal_argument_exception",
                 f"Invalid stemmer class specified: {cls}")
    assert err["caused_by"] == {
        "type": "class_not_found_exception",
        "reason": f"org.tartarus.snowball.ext.{cls}Stemmer",
    }


def test_hunspell_without_dictionary_is_500(client):
    body = {"tokenizer": "standard", "filter": [{"type": "hunspell", "locale": "en_US"}], "text": "running"}
    _error(_analyze_raw(client, body), 500, "illegal_state_exception",
           "Failed to load hunspell dictionary for locale: en_US")


KEYWORD_PROTECTION = [
    ("keyword_repeat_dedup", ["keyword_repeat", "porter_stem", "remove_duplicates"], "running fox", [
        ("running", 0, 7, "word", 0), ("run", 0, 7, "word", 0), ("fox", 8, 11, "word", 1),
    ]),
    ("keyword_repeat_dupes", ["keyword_repeat", "porter_stem"], "running fox", [
        ("running", 0, 7, "word", 0), ("run", 0, 7, "word", 0), ("fox", 8, 11, "word", 1), ("fox", 8, 11, "word", 1),
    ]),
    ("keyword_marker_list", [{"type": "keyword_marker", "keywords": ["running"]}, "porter_stem"], "running jumping", [
        ("running", 0, 7, "word", 0), ("jump", 8, 15, "word", 1),
    ]),
    ("keyword_marker_pattern", [{"type": "keyword_marker", "keywords_pattern": "^run.*"}, "porter_stem"],
     "running jumping", [("running", 0, 7, "word", 0), ("jump", 8, 15, "word", 1)]),
    ("keyword_marker_ignore_case",
     [{"type": "keyword_marker", "keywords": ["Running"], "ignore_case": True}, "porter_stem"], "running",
     [("running", 0, 7, "word", 0)]),
    ("stemmer_override",
     [{"type": "stemmer_override", "rules": ["running => runner", "mice, micex => mouse"]}, "porter_stem"],
     "running mice jumping",
     [("runner", 0, 7, "word", 0), ("mouse", 8, 12, "word", 1), ("jump", 13, 20, "word", 2)]),
]


@pytest.mark.parametrize("case,filters,text,expected", KEYWORD_PROTECTION, ids=[c[0] for c in KEYWORD_PROTECTION])
def test_keyword_protection_filters(client, case, filters, text, expected):
    assert _full(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


# --------------------------------------------------------------------------
# synonyms
# --------------------------------------------------------------------------

SYNONYMS = [
    ("expand", [{"type": "synonym", "synonyms": ["quick, fast, speedy"]}], "the quick fox", [
        ("the", 0, 3, "word", 0, None), ("quick", 4, 9, "word", 1, None), ("fast", 4, 9, "SYNONYM", 1, None),
        ("speedy", 4, 9, "SYNONYM", 1, None), ("fox", 10, 13, "word", 2, None),
    ]),
    ("no_expand_maps_to_first", [{"type": "synonym", "synonyms": ["quick, fast, speedy"], "expand": False}],
     "the speedy fox", [
         ("the", 0, 3, "word", 0, None), ("quick", 4, 10, "SYNONYM", 1, None), ("fox", 11, 14, "word", 2, None),
     ]),
    ("explicit_mapping", [{"type": "synonym", "synonyms": ["quick, fast => rapid"]}], "quick fast slow", [
        ("rapid", 0, 5, "SYNONYM", 0, None), ("rapid", 6, 10, "SYNONYM", 1, None), ("slow", 11, 15, "word", 2, None),
    ]),
    ("synonym_multiword_single_input_squashed", [{"type": "synonym", "synonyms": ["ny, new york"]}],
     "visit ny today", [
         ("visit", 0, 5, "word", 0, None), ("ny", 6, 8, "word", 1, None), ("new", 6, 8, "SYNONYM", 1, None),
         ("today", 9, 14, "word", 2, None), ("york", 9, 14, "SYNONYM", 2, None),
     ]),
    ("synonym_multiword_input", [{"type": "synonym", "synonyms": ["ny, new york"]}], "visit new york today", [
        ("visit", 0, 5, "word", 0, None), ("new", 6, 9, "word", 1, None), ("ny", 6, 14, "SYNONYM", 1, 2),
        ("york", 10, 14, "word", 2, None), ("today", 15, 20, "word", 3, None),
    ]),
    ("graph_single_input", [{"type": "synonym_graph", "synonyms": ["ny, new york"]}], "visit ny today", [
        ("visit", 0, 5, "word", 0, None), ("new", 6, 8, "SYNONYM", 1, None), ("ny", 6, 8, "word", 1, 2),
        ("york", 6, 8, "SYNONYM", 2, None), ("today", 9, 14, "word", 3, None),
    ]),
    ("graph_multiword_input", [{"type": "synonym_graph", "synonyms": ["ny, new york"]}], "visit new york today", [
        ("visit", 0, 5, "word", 0, None), ("ny", 6, 14, "SYNONYM", 1, 2), ("new", 6, 9, "word", 1, None),
        ("york", 10, 14, "word", 2, None), ("today", 15, 20, "word", 3, None),
    ]),
    ("graph_contraction", [{"type": "synonym_graph", "synonyms": ["new york => ny"]}], "visit new york today", [
        ("visit", 0, 5, "word", 0, None), ("ny", 6, 14, "SYNONYM", 1, None), ("today", 15, 20, "word", 2, None),
    ]),
    ("wordnet", [{"type": "synonym", "format": "wordnet", "synonyms": [
        "s(100000001,1,'abstain',v,1,0).", "s(100000001,2,'refrain',v,1,0).", "s(100000001,3,'desist',v,1,0).",
    ]}], "please refrain", [
        ("please", 0, 6, "word", 0, None), ("refrain", 7, 14, "word", 1, None),
        ("abstain", 7, 14, "SYNONYM", 1, None), ("desist", 7, 14, "SYNONYM", 1, None),
    ]),
    ("not_transitive", [{"type": "synonym", "synonyms": ["a, b", "b, c"]}], "a", [
        ("a", 0, 1, "word", 0, None), ("b", 0, 1, "SYNONYM", 0, None),
    ]),
    ("rules_analyzed_by_preceding_filters", ["lowercase", {"type": "synonym", "synonyms": ["Foo, bar"]}], "FOO", [
        ("foo", 0, 3, "word", 0, None), ("bar", 0, 3, "SYNONYM", 0, None),
    ]),
    ("multiword_lhs_contraction", [{"type": "synonym", "synonyms": ["i-pod, i pod => ipod"]}], "my i pod", [
        ("my", 0, 2, "word", 0, None), ("ipod", 3, 8, "SYNONYM", 1, None),
    ]),
]


@pytest.mark.parametrize("case,filters,text,expected", SYNONYMS, ids=[c[0] for c in SYNONYMS])
def test_synonym_filters(client, case, filters, text, expected):
    assert _graph(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


@pytest.mark.parametrize("rule", ["foo => ", "a => b => c"])
def test_synonym_invalid_rules(client, rule):
    body = {"tokenizer": "whitespace", "filter": [{"type": "synonym", "synonyms": [rule]}], "text": "foo"}
    _error(_analyze_raw(client, body), 400, "illegal_argument_exception", "Failed to build synonyms")


def test_synonym_missing_synonyms_path(client):
    body = {"tokenizer": "standard", "filter": [{"type": "synonym", "synonyms_path": "nope.txt"}], "text": "x"}
    _error(_analyze_raw(client, body), 400, "illegal_argument_exception",
           "IOException while reading synonyms_path_path: file not readable")


# --------------------------------------------------------------------------
# shingles / ngrams / graph-ish filters
# --------------------------------------------------------------------------

SHINGLES = [
    ("default", ["shingle"], "a b c", [
        ("a", 0, 1, "word", 0, None), ("a b", 0, 3, "shingle", 0, 2), ("b", 2, 3, "word", 1, None),
        ("b c", 2, 5, "shingle", 1, 2), ("c", 4, 5, "word", 2, None),
    ]),
    ("2_3_no_unigrams", [{"type": "shingle", "min_shingle_size": 2, "max_shingle_size": 3, "output_unigrams": False}],
     "a b c d", [
         ("a b", 0, 3, "shingle", 0, None), ("a b c", 0, 5, "shingle", 0, 2), ("b c", 2, 5, "shingle", 1, None),
         ("b c d", 2, 7, "shingle", 1, 2), ("c d", 4, 7, "shingle", 2, None),
     ]),
    ("separator", [{"type": "shingle", "token_separator": "_"}], "a b c", [
        ("a", 0, 1, "word", 0, None), ("a_b", 0, 3, "shingle", 0, 2), ("b", 2, 3, "word", 1, None),
        ("b_c", 2, 5, "shingle", 1, 2), ("c", 4, 5, "word", 2, None),
    ]),
    ("unigrams_if_no_shingles", [{"type": "shingle", "output_unigrams_if_no_shingles": True, "output_unigrams": False}],
     "solo", [("solo", 0, 4, "word", 0, None)]),
    ("no_unigrams_single_token", [{"type": "shingle", "output_unigrams": False}], "solo", []),
    ("filler_token_custom", ["stop", {"type": "shingle", "filler_token": "X"}], "a fox in the hole", [
        ("X fox", 2, 5, "shingle", 0, 2), ("fox", 2, 5, "word", 1, None), ("fox X", 2, 13, "shingle", 1, 2),
        ("X hole", 13, 17, "shingle", 2, 2), ("hole", 13, 17, "word", 3, None),
    ]),
    ("filler_token_default", ["stop", "shingle"], "fox in hole", [
        ("fox", 0, 3, "word", 0, None), ("fox _", 0, 7, "shingle", 0, 2), ("_ hole", 7, 11, "shingle", 1, 2),
        ("hole", 7, 11, "word", 2, None),
    ]),
]


@pytest.mark.parametrize("case,filters,text,expected", SHINGLES, ids=[c[0] for c in SHINGLES])
def test_shingle_filter(client, case, filters, text, expected):
    assert _graph(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


def test_shingle_max_diff_error(client):
    body = {"tokenizer": "whitespace", "filter": [{"type": "shingle", "max_shingle_size": 5}], "text": "a b"}
    _error(
        _analyze_raw(client, body), 400, "illegal_argument_exception",
        "In Shingle TokenFilter the difference between max_shingle_size and min_shingle_size (and +1 if "
        "outputting unigrams) must be less than or equal to: [3] but was [4]. This limit can be set by changing "
        "the [index.max_shingle_diff] index level setting.",
    )


def test_shingle_min_greater_than_max_error(client):
    body = {"tokenizer": "whitespace",
            "filter": [{"type": "shingle", "min_shingle_size": 3, "max_shingle_size": 2}], "text": "a b"}
    _error(_analyze_raw(client, body), 400, "illegal_argument_exception", "Min shingle size must be <= max shingle size")


NGRAM_FILTERS = [
    ("edge_default", ["edge_ngram"], "Quick", [("Q", 0, 5, "word", 0)]),
    ("edge_2_3_drops_short", [{"type": "edge_ngram", "min_gram": 2, "max_gram": 3}], "abcd e", [
        ("ab", 0, 4, "word", 0), ("abc", 0, 4, "word", 0),
    ]),
    ("edge_preserve_original", [{"type": "edge_ngram", "min_gram": 2, "max_gram": 3, "preserve_original": True}],
     "abcd e", [("ab", 0, 4, "word", 0), ("abc", 0, 4, "word", 0), ("abcd", 0, 4, "word", 0), ("e", 5, 6, "word", 1)]),
    ("edge_side_back", [{"type": "edge_ngram", "min_gram": 1, "max_gram": 2, "side": "back"}], "abc", [
        ("c", 0, 3, "word", 0), ("bc", 0, 3, "word", 0),
    ]),
    ("ngram_default", ["ngram"], "abc", [
        ("a", 0, 3, "word", 0), ("ab", 0, 3, "word", 0), ("b", 0, 3, "word", 0), ("bc", 0, 3, "word", 0),
        ("c", 0, 3, "word", 0),
    ]),
    ("ngram_2_3", [{"type": "ngram", "min_gram": 2, "max_gram": 3}], "abcd", [
        ("ab", 0, 4, "word", 0), ("abc", 0, 4, "word", 0), ("bc", 0, 4, "word", 0), ("bcd", 0, 4, "word", 0),
        ("cd", 0, 4, "word", 0),
    ]),
]


@pytest.mark.parametrize("case,filters,text,expected", NGRAM_FILTERS, ids=[c[0] for c in NGRAM_FILTERS])
def test_ngram_token_filters_keep_original_offsets(client, case, filters, text, expected):
    assert _full(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


def test_ngram_filter_max_diff_error(client):
    body = {"tokenizer": "whitespace", "filter": [{"type": "ngram", "min_gram": 1, "max_gram": 3}], "text": "abcd"}
    _error(
        _analyze_raw(client, body), 400, "illegal_argument_exception",
        "The difference between max_gram and min_gram in NGram Tokenizer must be less than or equal to: [1] "
        "but was [2]. This limit can be set by changing the [index.max_ngram_diff] index level setting.",
    )


WORD_DELIMITER = [
    ("wd_default", ["word_delimiter"], "Wi-Fi PowerShot500 O'Neil's SD500 wi_fi", [
        ("Wi", 0, 2, "word", 0, None), ("Fi", 3, 5, "word", 1, None), ("Power", 6, 11, "word", 2, None),
        ("Shot", 11, 15, "word", 3, None), ("500", 15, 18, "word", 4, None), ("O", 19, 20, "word", 5, None),
        ("Neil", 21, 25, "word", 6, None), ("SD", 28, 30, "word", 7, None), ("500", 30, 33, "word", 8, None),
        ("wi", 34, 36, "word", 9, None), ("fi", 37, 39, "word", 10, None),
    ]),
    ("wdg_default", ["word_delimiter_graph"], "Wi-Fi PowerShot500 O'Neil's SD500 wi_fi", [
        ("Wi", 0, 2, "word", 0, None), ("Fi", 3, 5, "word", 1, None), ("Power", 6, 11, "word", 2, None),
        ("Shot", 11, 15, "word", 3, None), ("500", 15, 18, "word", 4, None), ("O", 19, 20, "word", 5, None),
        ("Neil", 21, 25, "word", 6, None), ("SD", 28, 30, "word", 7, None), ("500", 30, 33, "word", 8, None),
        ("wi", 34, 36, "word", 9, None), ("fi", 37, 39, "word", 10, None),
    ]),
    ("wd_catenate_words", [{"type": "word_delimiter", "catenate_words": True}], "wi-fi-net", [
        ("wi", 0, 2, "word", 0, None), ("wifinet", 0, 9, "word", 0, None), ("fi", 3, 5, "word", 1, None),
        ("net", 6, 9, "word", 2, None),
    ]),
    ("wdg_catenate_words", [{"type": "word_delimiter_graph", "catenate_words": True}], "wi-fi-net", [
        ("wifinet", 0, 9, "word", 0, 3), ("wi", 0, 2, "word", 0, None), ("fi", 3, 5, "word", 1, None),
        ("net", 6, 9, "word", 2, None),
    ]),
    ("wdg_catenate_all_preserve",
     [{"type": "word_delimiter_graph", "catenate_all": True, "preserve_original": True}], "Super-Duper-500", [
         ("Super-Duper-500", 0, 15, "word", 0, 3), ("SuperDuper500", 0, 15, "word", 0, 3),
         ("Super", 0, 5, "word", 0, None), ("Duper", 6, 11, "word", 1, None), ("500", 12, 15, "word", 2, None),
     ]),
    ("wd_catenate_numbers", [{"type": "word_delimiter", "catenate_numbers": True}], "500-42", [
        ("500", 0, 3, "word", 0, None), ("50042", 0, 6, "word", 0, None), ("42", 4, 6, "word", 1, None),
    ]),
    ("wdg_no_split_numerics", [{"type": "word_delimiter_graph", "split_on_numerics": False}], "SD500 j2se", [
        ("SD500", 0, 5, "word", 0, None), ("j2se", 6, 10, "word", 1, None),
    ]),
    ("wdg_no_case_split", [{"type": "word_delimiter_graph", "split_on_case_change": False}], "PowerShot", [
        ("PowerShot", 0, 9, "word", 0, None),
    ]),
    ("wdg_no_number_parts", [{"type": "word_delimiter_graph", "generate_number_parts": False}], "abc-123-def", [
        ("abc", 0, 3, "word", 0, None), ("def", 8, 11, "word", 1, None),
    ]),
    ("wdg_no_word_parts", [{"type": "word_delimiter_graph", "generate_word_parts": False}], "abc-123-def", [
        ("123", 4, 7, "word", 0, None),
    ]),
    ("wdg_keep_possessive", [{"type": "word_delimiter_graph", "stem_english_possessive": False}], "O'Neil's", [
        ("O", 0, 1, "word", 0, None), ("Neil", 2, 6, "word", 1, None), ("s", 7, 8, "word", 2, None),
    ]),
    ("wdg_protected_words", [{"type": "word_delimiter_graph", "protected_words": ["wi-fi"]}], "wi-fi hi-fi", [
        ("wi-fi", 0, 5, "word", 0, None), ("hi", 6, 8, "word", 1, None), ("fi", 9, 11, "word", 2, None),
    ]),
    ("wdg_type_table", [{"type": "word_delimiter_graph", "type_table": ["- => ALPHA"]}], "wi-fi hi_fi", [
        ("wi-fi", 0, 5, "word", 0, None), ("hi", 6, 8, "word", 1, None), ("fi", 9, 11, "word", 2, None),
    ]),
    ("wdg_no_adjust_offsets", [{"type": "word_delimiter_graph", "adjust_offsets": False}], "ab-cd", [
        ("ab", 0, 5, "word", 0, None), ("cd", 0, 5, "word", 1, None),
    ]),
]


@pytest.mark.parametrize("case,filters,text,expected", WORD_DELIMITER, ids=[c[0] for c in WORD_DELIMITER])
def test_word_delimiter_filters(client, case, filters, text, expected):
    assert _graph(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


COMMON_GRAMS = [
    ("index_mode", [{"type": "common_grams", "common_words": ["the", "is"]}], "the quick fox is", [
        ("the", 0, 3, "word", 0, None), ("the_quick", 0, 9, "gram", 0, 2), ("quick", 4, 9, "word", 1, None),
        ("fox", 10, 13, "word", 2, None), ("fox_is", 10, 16, "gram", 2, 2), ("is", 14, 16, "word", 3, None),
    ]),
    ("query_mode", [{"type": "common_grams", "common_words": ["the", "is"], "query_mode": True}], "the quick fox is", [
        ("the_quick", 0, 9, "gram", 0, None), ("quick", 4, 9, "word", 1, None), ("fox_is", 10, 16, "gram", 2, None),
    ]),
    ("ignore_case", [{"type": "common_grams", "common_words": ["The"], "ignore_case": True}], "the fox", [
        ("the", 0, 3, "word", 0, None), ("the_fox", 0, 7, "gram", 0, 2), ("fox", 4, 7, "word", 1, None),
    ]),
]


@pytest.mark.parametrize("case,filters,text,expected", COMMON_GRAMS, ids=[c[0] for c in COMMON_GRAMS])
def test_common_grams(client, case, filters, text, expected):
    assert _graph(client, {"tokenizer": "whitespace", "filter": filters, "text": text}) == expected


CJK_FILTERS = [
    ("cjk_width", ["cjk_width"], "ｶﾀｶﾅ ＡＢＣ１２３", [
        ("カタカナ", 0, 4, "<KATAKANA>", 0, None), ("ABC123", 5, 11, "<ALPHANUM>", 1, None),
    ]),
    ("cjk_bigram", ["cjk_bigram"], "東京タワー 한국어 abc", [
        ("東京", 0, 2, "<DOUBLE>", 0, None), ("京タ", 1, 3, "<DOUBLE>", 1, None), ("タワ", 2, 4, "<DOUBLE>", 2, None),
        ("ワー", 3, 5, "<DOUBLE>", 3, None), ("한국", 6, 8, "<DOUBLE>", 4, None), ("국어", 7, 9, "<DOUBLE>", 5, None),
        ("abc", 10, 13, "<ALPHANUM>", 6, None),
    ]),
    ("cjk_bigram_unigrams", [{"type": "cjk_bigram", "output_unigrams": True}], "東京都", [
        ("東", 0, 1, "<SINGLE>", 0, None), ("東京", 0, 2, "<DOUBLE>", 0, 2), ("京", 1, 2, "<SINGLE>", 1, None),
        ("京都", 1, 3, "<DOUBLE>", 1, 2), ("都", 2, 3, "<SINGLE>", 2, None),
    ]),
    ("cjk_bigram_ignored_scripts", [{"type": "cjk_bigram", "ignored_scripts": ["katakana"]}], "東京タワー", [
        ("東京", 0, 2, "<DOUBLE>", 0, None), ("タワー", 2, 5, "<KATAKANA>", 1, None),
    ]),
]


@pytest.mark.parametrize("case,filters,text,expected", CJK_FILTERS, ids=[c[0] for c in CJK_FILTERS])
def test_cjk_filters(client, case, filters, text, expected):
    assert _graph(client, {"tokenizer": "standard", "filter": filters, "text": text}) == expected


# --------------------------------------------------------------------------
# misc token filters
# --------------------------------------------------------------------------

MISC_FILTERS = [
    ("unique", "whitespace", ["unique"], "a b a c b", [("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1), ("c", 6, 7, "word", 2)]),
    ("unique_same_position_keeps_repeats", "whitespace", [{"type": "unique", "only_on_same_position": True}], "a b a", [
        ("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1), ("a", 4, 5, "word", 2),
    ]),
    ("unique_same_position_dedups_stack", "whitespace",
     [{"type": "asciifolding", "preserve_original": True}, {"type": "unique", "only_on_same_position": True}],
     "cafe café", [("cafe", 0, 4, "word", 0), ("cafe", 5, 9, "word", 1), ("café", 5, 9, "word", 1)]),
    ("length", "whitespace", [{"type": "length", "min": 2, "max": 4}], "a bb ccc dddd eeeee", [
        ("bb", 2, 4, "word", 1), ("ccc", 5, 8, "word", 2), ("dddd", 9, 13, "word", 3),
    ]),
    ("length_default_keeps_all", "whitespace", ["length"], "a bb", [("a", 0, 1, "word", 0), ("bb", 2, 4, "word", 1)]),
    ("length_counts_utf16_units", "whitespace", [{"type": "length", "min": 0, "max": 2}], "ab abc é😀", [
        ("ab", 0, 2, "word", 0),
    ]),
    ("truncate", "whitespace", [{"type": "truncate", "length": 3}], "abcdef ab", [
        ("abc", 0, 6, "word", 0), ("ab", 7, 9, "word", 1),
    ]),
    ("truncate_default_10", "whitespace", ["truncate"], "abcdefghijklmnop", [("abcdefghij", 0, 16, "word", 0)]),
    ("truncate_utf16_units", "whitespace", [{"type": "truncate", "length": 2}], "😀😀😀 éé", [
        ("😀", 0, 6, "word", 0), ("éé", 7, 9, "word", 1),
    ]),
    ("trim_keeps_offsets", "keyword", ["trim"], "  padded  ", [("padded", 0, 10, "word", 0)]),
    ("reverse", "whitespace", ["reverse"], "hello world", [("olleh", 0, 5, "word", 0), ("dlrow", 6, 11, "word", 1)]),
    ("reverse_surrogates", "whitespace", ["reverse"], "😀a", [("a😀", 0, 3, "word", 0)]),
    ("elision_default", "standard", ["elision"], "l'avion d'Artagnan j'aime qu'il", [
        ("avion", 0, 7, "<ALPHANUM>", 0), ("Artagnan", 8, 18, "<ALPHANUM>", 1),
        ("aime", 19, 25, "<ALPHANUM>", 2), ("il", 26, 31, "<ALPHANUM>", 3),
    ]),
    ("elision_custom_articles", "standard", [{"type": "elision", "articles": ["x"]}], "x'foo l'avion", [
        ("foo", 0, 5, "<ALPHANUM>", 0), ("l'avion", 6, 13, "<ALPHANUM>", 1),
    ]),
    ("elision_articles_case", "standard", [{"type": "elision", "articles": ["L"], "articles_case": True}],
     "l'avion L'avion", [("avion", 0, 7, "<ALPHANUM>", 0), ("avion", 8, 15, "<ALPHANUM>", 1)]),
    ("apostrophe", "standard", ["apostrophe"], "Türkiye'de O'Reilly's", [
        ("Türkiye", 0, 10, "<ALPHANUM>", 0), ("O", 11, 21, "<ALPHANUM>", 1),
    ]),
    ("classic_filter", "classic", ["classic"], "I.B.M. O'Reilly's dogs", [
        ("IBM", 0, 6, "<ACRONYM>", 0), ("O'Reilly", 7, 17, "<APOSTROPHE>", 1), ("dogs", 18, 22, "<ALPHANUM>", 2),
    ]),
    ("limit", "whitespace", [{"type": "limit", "max_token_count": 2}], "a b c d", [
        ("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1),
    ]),
    ("limit_default_1", "whitespace", ["limit"], "a b c", [("a", 0, 1, "word", 0)]),
    ("limit_consume_all", "standard", [{"type": "limit", "max_token_count": 2, "consume_all_tokens": True}], "a b c", [
        ("a", 0, 1, "<ALPHANUM>", 0), ("b", 2, 3, "<ALPHANUM>", 1),
    ]),
    ("pattern_replace_groups", "whitespace",
     [{"type": "pattern_replace", "pattern": "(\\d+)", "replacement": "<$1>"}], "a1b22 c", [
         ("a<1>b<22>", 0, 5, "word", 0), ("c", 6, 7, "word", 1),
     ]),
    ("pattern_replace_first_only", "whitespace",
     [{"type": "pattern_replace", "pattern": "a", "replacement": "", "all": False}], "banana", [
         ("bnana", 0, 6, "word", 0),
     ]),
    ("pattern_replace_default_empty", "whitespace", [{"type": "pattern_replace", "pattern": "a"}], "banana", [
        ("bnn", 0, 6, "word", 0),
    ]),
    ("pattern_capture_preserve", "whitespace", [{"type": "pattern_capture", "preserve_original": True, "patterns": [
        "(\\p{Ll}+|\\p{Lu}\\p{Ll}+|\\p{Lu}+)", "(\\d+)"]}], "camelCaseABC123", [
        ("camelCaseABC123", 0, 15, "word", 0), ("camel", 0, 15, "word", 0), ("Case", 0, 15, "word", 0),
        ("ABC", 0, 15, "word", 0), ("123", 0, 15, "word", 0),
    ]),
    ("pattern_capture_no_preserve", "whitespace", [{"type": "pattern_capture", "preserve_original": False,
                                                    "patterns": ["([a-z]+)@([a-z.]+)"]}], "joe@ex.com", [
        ("joe", 0, 10, "word", 0), ("ex.com", 0, 10, "word", 0),
    ]),
    ("fingerprint_filter", "whitespace", ["fingerprint"], "b a c a", [("a b c", 0, 7, "fingerprint", 0)]),
    ("fingerprint_separator", "whitespace", [{"type": "fingerprint", "separator": "+", "max_output_size": 5}], "b a c", [
        ("a+b+c", 0, 5, "fingerprint", 0),
    ]),
    ("fingerprint_too_long_dropped", "whitespace",
     [{"type": "fingerprint", "separator": "+", "max_output_size": 4}], "b a c", []),
    ("condition", "whitespace", [{"type": "condition", "filter": ["lowercase"],
                                  "script": {"source": "token.getTerm().length() < 5"}}], "THE QUICK BROWN FOX", [
        ("the", 0, 3, "word", 0), ("QUICK", 4, 9, "word", 1), ("BROWN", 10, 15, "word", 2), ("fox", 16, 19, "word", 3),
    ]),
    ("predicate_length", "whitespace", [{"type": "predicate_token_filter",
                                         "script": {"source": "token.getTerm().length() > 3"}}], "the quick fox jumps", [
        ("quick", 4, 9, "word", 1), ("jumps", 14, 19, "word", 3),
    ]),
    ("predicate_position", "whitespace", [{"type": "predicate_token_filter",
                                           "script": {"source": "token.getPosition() % 2 == 0"}}], "a b c d e", [
        ("a", 0, 1, "word", 0), ("c", 4, 5, "word", 2), ("e", 8, 9, "word", 4),
    ]),
    ("multiplexer", "whitespace", [{"type": "multiplexer", "filters": ["lowercase", "lowercase, porter_stem"]}],
     "Running", [("Running", 0, 7, "word", 0), ("running", 0, 7, "word", 0), ("run", 0, 7, "word", 0)]),
    ("multiplexer_no_original", "whitespace",
     [{"type": "multiplexer", "filters": ["uppercase", "reverse"], "preserve_original": False}], "abc", [
         ("ABC", 0, 3, "word", 0), ("cba", 0, 3, "word", 0),
     ]),
    ("multiplexer_dedups_identical", "whitespace", [{"type": "multiplexer", "filters": ["lowercase"]}], "abc", [
        ("abc", 0, 3, "word", 0),
    ]),
    ("decompounder_case_sensitive", "whitespace",
     [{"type": "dictionary_decompounder", "word_list": ["donau", "dampf", "schiff", "fahrt"]}], "Donaudampfschiff", [
         ("Donaudampfschiff", 0, 16, "word", 0), ("dampf", 0, 16, "word", 0), ("schiff", 0, 16, "word", 0),
     ]),
    ("decompounder_lowercased", "whitespace",
     ["lowercase", {"type": "dictionary_decompounder", "word_list": ["donau", "dampf", "schiff", "fahrt"]}],
     "Donaudampfschiff", [
         ("donaudampfschiff", 0, 16, "word", 0), ("donau", 0, 16, "word", 0), ("dampf", 0, 16, "word", 0),
         ("schiff", 0, 16, "word", 0),
     ]),
    ("delimited_payload_strips", "whitespace", ["delimited_payload"], "the|1 quick|2 fox", [
        ("the", 0, 5, "word", 0), ("quick", 6, 13, "word", 1), ("fox", 14, 17, "word", 2),
    ]),
    ("delimited_payload_custom", "whitespace", [{"type": "delimited_payload", "delimiter": "+", "encoding": "int"}],
     "a+5 b", [("a", 0, 3, "word", 0), ("b", 4, 5, "word", 1)]),
    ("flatten_graph_noop", "whitespace", ["flatten_graph"], "a b", [("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1)]),
]


@pytest.mark.parametrize("case,tokenizer,filters,text,expected", MISC_FILTERS, ids=[c[0] for c in MISC_FILTERS])
def test_misc_token_filters(client, case, tokenizer, filters, text, expected):
    assert _full(client, {"tokenizer": tokenizer, "filter": filters, "text": text}) == expected


def test_min_hash_emits_single_min_hash_token(client):
    body = {
        "tokenizer": "whitespace",
        "filter": [{"type": "min_hash", "hash_count": 1, "bucket_count": 2, "hash_set_size": 1, "with_rotation": False}],
        "text": "a b c",
    }
    toks = _analyze(client, body)["tokens"]
    assert len(toks) == 1
    assert toks[0]["type"] == "MIN_HASH"
    assert (toks[0]["start_offset"], toks[0]["end_offset"], toks[0]["position"]) == (0, 5, 0)
    # deterministic hashing
    assert _analyze(client, body)["tokens"] == toks


def test_hyphenation_decompounder_missing_patterns(client):
    body = {"tokenizer": "whitespace", "filter": [
        {"type": "hyphenation_decompounder", "word_list": ["a"], "hyphenation_patterns_path": "x.xml"}], "text": "abc"}
    _error(_analyze_raw(client, body), 400, "illegal_argument_exception",
           "Exception while reading hyphenation_patterns_path.")


# --------------------------------------------------------------------------
# char filters
# --------------------------------------------------------------------------

CHAR_FILTERS = [
    ("html_keyword", "keyword", ["html_strip"], "<p>I&apos;m so <b>happy</b>!</p>", [
        ("\nI'm so happy!\n", 0, 32, "word", 0),
    ]),
    ("html_standard_offsets", "standard", ["html_strip"], "<p>I&apos;m so <b>happy</b>!</p>", [
        ("I'm", 3, 11, "<ALPHANUM>", 0), ("so", 12, 14, "<ALPHANUM>", 1), ("happy", 18, 27, "<ALPHANUM>", 2),
    ]),
    ("html_escaped_tags", "keyword", [{"type": "html_strip", "escaped_tags": ["b"]}],
     "<p>I'm <b>bold</b> <i>it</i></p>", [("\nI'm <b>bold</b> it\n", 0, 32, "word", 0)]),
    ("html_entities", "keyword", ["html_strip"], "a&amp;b &lt;tag&gt; &#65; &#x42; &nbsp;x &copy; &eacute;", [
        # &nbsp; decodes to a plain space
        ("a&b <tag> A B " + " " + "x © é", 0, 56, "word", 0),
    ]),
    ("html_br", "keyword", ["html_strip"], "line<br>break<br/>end", [("line\nbreak\nend", 0, 21, "word", 0)]),
    ("html_script_comment", "keyword", ["html_strip"], "<script>var x=1;</script>after<!-- c -->z", [
        ("\nafterz", 0, 41, "word", 0),
    ]),
    ("mapping_digits_partial", "keyword", [{"type": "mapping", "mappings": ["٠ => 0", "١ => 1", "٢ => 2"]}],
     "My license plate is ٢٥٠١٥", [("My license plate is 2٥01٥", 0, 25, "word", 0)]),
    ("mapping_emoticons", "whitespace", [{"type": "mapping", "mappings": [":) => _happy_", ":( => _sad_"]}],
     "I'm :) not :(", [
         ("I'm", 0, 3, "word", 0), ("_happy_", 4, 6, "word", 1), ("not", 7, 10, "word", 2), ("_sad_", 11, 13, "word", 3),
     ]),
    ("mapping_unicode_escape_longest", "keyword", [{"type": "mapping", "mappings": ["\\u0020 => _", "ph => f"]}],
     "phone ph", [("fone_f", 0, 8, "word", 0)]),
    ("mapping_delete", "keyword", [{"type": "mapping", "mappings": ["a => ", "b => c"]}], "aabba", [
        ("cc", 2, 5, "word", 0),
    ]),
    ("mapping_escapes", "keyword", [{"type": "mapping", "mappings": ["\\t => TAB", "\\n => NL"]}], "a\tb\nc", [
        ("aTABbNLc", 0, 5, "word", 0),
    ]),
    ("pattern_replace_card", "standard",
     [{"type": "pattern_replace", "pattern": "(\\d+)-(?=\\d)", "replacement": "$1_"}], "My credit card is 123-456-789", [
         ("My", 0, 2, "<ALPHANUM>", 0), ("credit", 3, 9, "<ALPHANUM>", 1), ("card", 10, 14, "<ALPHANUM>", 2),
         ("is", 15, 17, "<ALPHANUM>", 3), ("123_456_789", 18, 29, "<NUM>", 4),
     ]),
    ("pattern_replace_camel_offsets", "standard",
     [{"type": "pattern_replace", "pattern": "(?<=\\p{Lower})(?=\\p{Upper})", "replacement": " "}],
     "camelCaseString foo", [
         ("camel", 0, 4, "<ALPHANUM>", 0), ("Case", 5, 8, "<ALPHANUM>", 1), ("String", 9, 15, "<ALPHANUM>", 2),
         ("foo", 16, 19, "<ALPHANUM>", 3),
     ]),
    ("pattern_replace_delete", "whitespace", [{"type": "pattern_replace", "pattern": "x+", "replacement": ""}],
     "axxxb c", [("ab", 0, 5, "word", 0), ("c", 6, 7, "word", 1)]),
    ("pattern_replace_flags", "whitespace",
     [{"type": "pattern_replace", "pattern": "X", "replacement": "y", "flags": "CASE_INSENSITIVE"}], "aXbx", [
         ("ayby", 0, 4, "word", 0),
     ]),
    ("chained_char_filters", "keyword", ["html_strip", {"type": "mapping", "mappings": ["& => and"]}],
     "<b>R&amp;D</b>", [("RandD", 3, 14, "word", 0)]),
]


@pytest.mark.parametrize("case,tokenizer,char_filters,text,expected", CHAR_FILTERS, ids=[c[0] for c in CHAR_FILTERS])
def test_char_filters(client, case, tokenizer, char_filters, text, expected):
    assert _full(client, {"tokenizer": tokenizer, "char_filter": char_filters, "text": text}) == expected


def test_mapping_char_filter_requires_mappings(client, index_factory):
    r = client.put(f"/{_unique()}", json={"settings": {"analysis": {"char_filter": {"c": {"type": "mapping"}}}}})
    _error(r, 400, "illegal_argument_exception", "mapping requires either `mappings` or `mappings_path` to be configured")


def test_char_filter_only_defaults_to_keyword_tokenizer(client):
    assert _full(client, {"char_filter": ["html_strip"], "text": "<b>Hi</b>"}) == [("Hi", 3, 9, "word", 0)]


def test_filter_only_defaults_to_keyword_tokenizer(client):
    assert _full(client, {"filter": ["lowercase"], "text": "Hello World"}) == [("hello world", 0, 11, "word", 0)]


# --------------------------------------------------------------------------
# _analyze request validation and errors
# --------------------------------------------------------------------------

GLOBAL_ERRORS = [
    ("unknown_analyzer", {"analyzer": "nonexistent", "text": "x"}, "illegal_argument_exception",
     "failed to find global analyzer [nonexistent]"),
    ("unknown_tokenizer", {"tokenizer": "nonexistent", "text": "x"}, "illegal_argument_exception",
     "failed to find global tokenizer under [nonexistent]"),
    ("unknown_filter", {"tokenizer": "whitespace", "filter": ["fake_filter"], "text": "x"},
     "illegal_argument_exception", "failed to find global filter under [fake_filter]"),
    ("unknown_char_filter", {"tokenizer": "keyword", "char_filter": ["nonexistent"], "text": "x"},
     "illegal_argument_exception", "failed to find global char_filter under [nonexistent]"),
    ("unknown_inline_tokenizer_type", {"tokenizer": {"type": "nope"}, "text": "x"}, "illegal_argument_exception",
     "failed to find global tokenizer under [nope]"),
    ("unknown_inline_filter_type", {"tokenizer": "standard", "filter": [{"type": "nope"}], "text": "x"},
     "illegal_argument_exception", "failed to find global filter under [nope]"),
    ("unknown_inline_char_filter_type", {"tokenizer": "standard", "char_filter": [{"type": "nope"}], "text": "x"},
     "illegal_argument_exception", "failed to find global char_filter under [nope]"),
    ("inline_filter_missing_type", {"tokenizer": "standard", "filter": [{"stopwords": ["x"]}], "text": "x"},
     "illegal_argument_exception", 'Missing [type] setting for anonymous filter: {"stopwords":["x"]}'),
    ("missing_text", {"analyzer": "standard"}, "action_request_validation_exception",
     "Validation Failed: 1: text is missing;"),
    ("empty_text_array", {"analyzer": "standard", "text": []}, "action_request_validation_exception",
     "Validation Failed: 1: text is missing;"),
    ("analyzer_plus_components", {"analyzer": "standard", "tokenizer": "keyword", "text": "x"},
     "action_request_validation_exception",
     "Validation Failed: 1: cannot define extra components on a named analyzer;"),
    ("normalizer_without_index", {"normalizer": "lowercase", "text": "Hi"}, "action_request_validation_exception",
     "Validation Failed: 1: index is required if normalizer is specified;"),
    ("normalizer_with_components", {"tokenizer": "keyword", "filter": ["lowercase"], "normalizer": "x", "text": "Hi"},
     "action_request_validation_exception",
     "Validation Failed: 1: index is required if normalizer is specified;2: tokenizer/analyze should be null if "
     "normalizer is specified;3: cannot define extra components on a named normalizer;"),
    ("field_without_index", {"field": "f", "text": "x"}, "illegal_argument_exception",
     "analysis based on a specific field requires an index"),
    ("unknown_body_field", {"analyzer": "standard", "text": "x", "bogus": 1}, "x_content_parse_exception",
     "[1:35] [analyze_request] unknown field [bogus]"),
]


@pytest.mark.parametrize("case,body,etype,reason", GLOBAL_ERRORS, ids=[c[0] for c in GLOBAL_ERRORS])
def test_analyze_errors(client, case, body, etype, reason):
    _error(_analyze_raw(client, body), 400, etype, reason)


def test_explain_must_be_boolean(client):
    r = _analyze_raw(client, {"analyzer": "standard", "text": "x", "explain": "yes"})
    err = _error(r, 400, "x_content_parse_exception", "[1:45] [analyze_request] failed to parse field [explain]")
    assert err["caused_by"] == {
        "type": "illegal_argument_exception",
        "reason": "Failed to parse value [yes] as only [true] or [false] are allowed.",
    }


def test_analyze_get_without_body(client):
    r = client.get("/_analyze", params={"analyzer": "standard", "text": "Hello"})
    _error(r, 400, "parse_exception", "request body or source parameter is required")


def test_analyze_on_missing_index_is_404(client):
    name = _unique("missing")
    r = _analyze_raw(client, {"analyzer": "standard", "text": "x"}, name)
    _error(r, 404, "index_not_found_exception", f"no such index [{name}]")


def test_analyzer_wins_over_field_globally(client):
    assert _toks(client, {"analyzer": "whitespace", "field": "f", "text": "A B"}) == ["A", "B"]


def test_analyze_token_count_limit_global(client):
    ok = _analyze(client, {"tokenizer": "standard", "text": "a " * 10000})
    assert len(ok["tokens"]) == 10000
    r = _analyze_raw(client, {"tokenizer": "standard", "text": "a " * 10001})
    _error(
        r, 500, "illegal_state_exception",
        "The number of tokens produced by calling _analyze has exceeded the allowed maximum of [10000]. "
        "This limit can be set by changing the [index.analyze.max_token_count] index level setting.",
    )


def test_analyze_token_count_limit_index_setting(client, index_factory):
    name = index_factory(settings={"index.analyze.max_token_count": 3})
    assert len(_analyze(client, {"tokenizer": "standard", "text": "a b c"}, name)["tokens"]) == 3
    r = _analyze_raw(client, {"tokenizer": "standard", "text": "a b c d"}, name)
    _error(
        r, 500, "illegal_state_exception",
        "The number of tokens produced by calling _analyze has exceeded the allowed maximum of [3]. "
        "This limit can be set by changing the [index.analyze.max_token_count] index level setting.",
    )


# --------------------------------------------------------------------------
# multiple texts / position_increment_gap
# --------------------------------------------------------------------------


def test_multi_text_custom_chain_gap_100(client):
    assert _full(client, {"tokenizer": "whitespace", "text": ["x a", "y"]}) == [
        ("x", 0, 1, "word", 0), ("a", 2, 3, "word", 1), ("y", 4, 5, "word", 102),
    ]


def test_multi_text_keyword_tokenizer_gap(client):
    assert _full(client, {"tokenizer": "keyword", "text": ["a b", "c d"]}) == [
        ("a b", 0, 3, "word", 0), ("c d", 4, 7, "word", 101),
    ]


@pytest.mark.parametrize(
    "analyzer,expected",
    [
        ("standard", [("a", 0, 1, "<ALPHANUM>", 0), ("b", 2, 3, "<ALPHANUM>", 1), ("c", 4, 5, "<ALPHANUM>", 2)]),
        ("whitespace", [("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1), ("c", 4, 5, "word", 2)]),
        ("keyword", [("a b", 0, 3, "word", 0), ("c", 4, 5, "word", 1)]),
    ],
)
def test_multi_text_prebuilt_analyzer_has_no_gap(client, analyzer, expected):
    assert _full(client, {"analyzer": analyzer, "text": ["a b", "c"]}) == expected


def test_multi_text_stopword_position_continues(client):
    assert _full(client, {"analyzer": "english", "text": ["x a", "y"]}) == [
        ("x", 0, 1, "<ALPHANUM>", 0), ("y", 4, 5, "<ALPHANUM>", 2),
    ]


def test_multi_text_empty_entries_still_add_gap(client):
    assert _full(client, {"tokenizer": "whitespace", "text": ["", "a", "", "b"]}) == [
        ("a", 1, 2, "word", 100), ("b", 4, 5, "word", 301),
    ]


# --------------------------------------------------------------------------
# explain / attributes
# --------------------------------------------------------------------------


def test_explain_named_analyzer_shape(client):
    body = _analyze(client, {"analyzer": "standard", "text": "Hi there", "explain": True})
    assert body == {
        "detail": {
            "custom_analyzer": False,
            "analyzer": {
                "name": "standard",
                "tokens": [
                    {"token": "hi", "start_offset": 0, "end_offset": 2, "type": "<ALPHANUM>", "position": 0,
                     "bytes": "[68 69]", "positionLength": 1, "termFrequency": 1},
                    {"token": "there", "start_offset": 3, "end_offset": 8, "type": "<ALPHANUM>", "position": 1,
                     "bytes": "[74 68 65 72 65]", "positionLength": 1, "termFrequency": 1},
                ],
            },
        }
    }


def test_explain_custom_chain_shape(client):
    body = _analyze(client, {"tokenizer": "standard", "filter": ["lowercase"], "char_filter": ["html_strip"],
                             "text": "<b>Hi</b> X", "explain": True})
    d = body["detail"]
    assert list(d.keys()) == ["custom_analyzer", "charfilters", "tokenizer", "tokenfilters"]
    assert d["custom_analyzer"] is True
    assert d["charfilters"] == [{"name": "html_strip", "filtered_text": ["Hi X"]}]
    assert d["tokenizer"]["name"] == "standard"
    assert [t["token"] for t in d["tokenizer"]["tokens"]] == ["Hi", "X"]
    assert d["tokenizer"]["tokens"][0] == {
        "token": "Hi", "start_offset": 3, "end_offset": 9, "type": "<ALPHANUM>", "position": 0,
        "bytes": "[48 69]", "positionLength": 1, "termFrequency": 1,
    }
    assert [f["name"] for f in d["tokenfilters"]] == ["lowercase"]
    assert [t["token"] for t in d["tokenfilters"][0]["tokens"]] == ["hi", "x"]
    assert d["tokenfilters"][0]["tokens"][1]["bytes"] == "[78]"


def test_explain_anonymous_filter_name_and_keyword_attribute(client):
    body = _analyze(client, {"tokenizer": "whitespace", "filter": [{"type": "keyword_marker", "keywords": ["x"]}],
                             "text": "x", "explain": True})
    f = body["detail"]["tokenfilters"][0]
    assert f["name"] == "__anonymous__keyword_marker"
    assert f["tokens"] == [{"token": "x", "start_offset": 0, "end_offset": 1, "type": "word", "position": 0,
                            "bytes": "[78]", "keyword": True, "positionLength": 1, "termFrequency": 1}]


def test_explain_language_analyzer_reports_keyword_false(client):
    body = _analyze(client, {"analyzer": "english", "text": "running", "explain": True})
    assert body["detail"]["analyzer"] == {
        "name": "english",
        "tokens": [{"token": "run", "start_offset": 0, "end_offset": 7, "type": "<ALPHANUM>", "position": 0,
                    "bytes": "[72 75 6e]", "keyword": False, "positionLength": 1, "termFrequency": 1}],
    }


def test_explain_tokenizer_only_has_empty_lists(client):
    d = _analyze(client, {"tokenizer": "keyword", "text": "a", "explain": True})["detail"]
    assert d["charfilters"] == []
    assert d["tokenfilters"] == []
    assert d["tokenizer"]["name"] == "keyword"


def test_explain_multi_text_has_no_position_gap(client):
    d = _analyze(client, {"tokenizer": "whitespace", "filter": [{"type": "stop", "stopwords": ["a"]}],
                          "text": ["x a", "y"], "explain": True})["detail"]
    assert [(t["token"], t["position"]) for t in d["tokenizer"]["tokens"]] == [("x", 0), ("a", 1), ("y", 2)]
    assert d["tokenfilters"][0]["name"] == "__anonymous__stop"
    assert [(t["token"], t["start_offset"], t["position"]) for t in d["tokenfilters"][0]["tokens"]] == [
        ("x", 0, 0), ("y", 4, 2),
    ]


def test_explain_charfilters_filtered_text_per_input(client):
    d = _analyze(client, {"tokenizer": "keyword", "char_filter": ["html_strip"], "text": ["<i>a</i>", "<b>b</b>"],
                          "explain": True})["detail"]
    assert d["charfilters"] == [{"name": "html_strip", "filtered_text": ["a", "b"]}]


def test_attributes_limit_explain_output(client):
    body = _analyze(client, {"tokenizer": "standard", "filter": ["lowercase"], "text": "Hi", "explain": True,
                             "attributes": ["keyword"]})
    assert body["detail"]["tokenfilters"] == [
        {"name": "lowercase",
         "tokens": [{"token": "hi", "start_offset": 0, "end_offset": 2, "type": "<ALPHANUM>", "position": 0}]}
    ]


def test_attributes_ignored_without_explain(client):
    body = _analyze(client, {"tokenizer": "standard", "filter": ["lowercase"], "text": "Hi", "attributes": ["keyword"]})
    assert body == {"tokens": [{"token": "hi", "start_offset": 0, "end_offset": 2, "type": "<ALPHANUM>", "position": 0}]}


# --------------------------------------------------------------------------
# index-scoped analysis
# --------------------------------------------------------------------------

INDEX_ANALYSIS = {
    "analysis": {
        "char_filter": {"emo": {"type": "mapping", "mappings": [":) => happy"]}},
        "tokenizer": {"my_ng": {"type": "ngram", "min_gram": 2, "max_gram": 3}},
        "filter": {
            "my_syn": {"type": "synonym", "synonyms": ["tv, television"]},
            "my_stop": {"type": "stop", "stopwords": ["foo"]},
        },
        "analyzer": {
            "my_an": {"type": "custom", "tokenizer": "standard", "char_filter": ["emo"],
                      "filter": ["lowercase", "my_syn", "my_stop"]},
            "my_gap": {"type": "custom", "tokenizer": "standard", "position_increment_gap": 10},
            "my_std": {"type": "standard", "stopwords": ["the"], "max_token_length": 4},
            "my_pat": {"type": "pattern", "pattern": "\\W|_", "lowercase": False},
            "my_stop_an": {"type": "stop", "stopwords": ["x"]},
            "my_fp": {"type": "fingerprint", "separator": "-", "stopwords": "_english_"},
            "my_snow": {"type": "snowball", "language": "English"},
            "my_ws": {"type": "whitespace"},
            "my_kw": {"type": "keyword"},
            "my_simple": {"type": "simple"},
        },
        "normalizer": {"my_norm": {"type": "custom", "char_filter": [], "filter": ["lowercase", "asciifolding"]}},
    }
}

INDEX_MAPPING = {
    "properties": {
        "t": {"type": "text", "analyzer": "my_an"},
        "g": {"type": "text", "analyzer": "my_gap"},
        "z": {"type": "text", "position_increment_gap": 0},
        "k": {"type": "keyword", "normalizer": "my_norm"},
        "plain": {"type": "text"},
        "kw": {"type": "keyword"},
        "num": {"type": "integer"},
        "obj": {"properties": {"inner": {"type": "text", "analyzer": "my_ws"}}},
        "multi": {"type": "text", "fields": {"raw": {"type": "keyword", "normalizer": "my_norm"},
                                              "ws": {"type": "text", "analyzer": "my_ws"}}},
    }
}


@pytest.fixture(scope="module")
def aidx(client):
    name = _unique("idx")
    r = client.put(f"/{name}", json={"settings": INDEX_ANALYSIS, "mappings": INDEX_MAPPING})
    assert r.status_code == 200, r.text
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


INDEX_SCOPED = [
    ("custom_analyzer", {"analyzer": "my_an", "text": "Foo :) TV"}, [
        ("happy", 4, 6, "<ALPHANUM>", 1), ("tv", 7, 9, "<ALPHANUM>", 2), ("television", 7, 9, "SYNONYM", 2),
    ]),
    ("field_uses_mapping_analyzer", {"field": "t", "text": "Foo :) TV"}, [
        ("happy", 4, 6, "<ALPHANUM>", 1), ("tv", 7, 9, "<ALPHANUM>", 2), ("television", 7, 9, "SYNONYM", 2),
    ]),
    ("field_custom_gap", {"field": "g", "text": ["a b", "c"]}, [
        ("a", 0, 1, "<ALPHANUM>", 0), ("b", 2, 3, "<ALPHANUM>", 1), ("c", 4, 5, "<ALPHANUM>", 12),
    ]),
    ("analyzer_custom_gap", {"analyzer": "my_gap", "text": ["a b", "c"]}, [
        ("a", 0, 1, "<ALPHANUM>", 0), ("b", 2, 3, "<ALPHANUM>", 1), ("c", 4, 5, "<ALPHANUM>", 12),
    ]),
    ("field_default_text_gap_100", {"field": "plain", "text": ["a b", "c"]}, [
        ("a", 0, 1, "<ALPHANUM>", 0), ("b", 2, 3, "<ALPHANUM>", 1), ("c", 4, 5, "<ALPHANUM>", 102),
    ]),
    ("field_gap_0", {"field": "z", "text": ["a", "b"]}, [
        ("a", 0, 1, "<ALPHANUM>", 0), ("b", 2, 3, "<ALPHANUM>", 1),
    ]),
    ("field_keyword_normalizer", {"field": "k", "text": "Ünïcode TEXT"}, [("unicode text", 0, 12, "word", 0)]),
    ("named_normalizer", {"normalizer": "my_norm", "text": "Ünïcode TEXT"}, [("unicode text", 0, 12, "word", 0)]),
    ("builtin_lowercase_normalizer", {"normalizer": "lowercase", "text": "Hi There"}, [("hi there", 0, 8, "word", 0)]),
    ("field_keyword_plain", {"field": "kw", "text": "Ünïcode TEXT"}, [("Ünïcode TEXT", 0, 12, "word", 0)]),
    ("field_unmapped_falls_back_to_default", {"field": "missing_field", "text": "Hello World"}, [
        ("hello", 0, 5, "<ALPHANUM>", 0), ("world", 6, 11, "<ALPHANUM>", 1),
    ]),
    ("field_object_subfield", {"field": "obj.inner", "text": "Hello World"}, [
        ("Hello", 0, 5, "word", 0), ("World", 6, 11, "word", 1),
    ]),
    ("field_multifield_keyword", {"field": "multi.raw", "text": "Éclair BIG"}, [("eclair big", 0, 10, "word", 0)]),
    ("field_multifield_text", {"field": "multi.ws", "text": "Éclair BIG"}, [
        ("Éclair", 0, 6, "word", 0), ("BIG", 7, 10, "word", 1),
    ]),
    ("standard_type_params", {"analyzer": "my_std", "text": "The quick brownish fox"}, [
        ("quic", 4, 8, "<ALPHANUM>", 1), ("k", 8, 9, "<ALPHANUM>", 2), ("brow", 10, 14, "<ALPHANUM>", 3),
        ("nish", 14, 18, "<ALPHANUM>", 4), ("fox", 19, 22, "<ALPHANUM>", 5),
    ]),
    ("pattern_no_lowercase", {"analyzer": "my_pat", "text": "Foo_Bar-baz QUX"}, [
        ("Foo", 0, 3, "word", 0), ("Bar", 4, 7, "word", 1), ("baz", 8, 11, "word", 2), ("QUX", 12, 15, "word", 3),
    ]),
    ("stop_type_custom_words", {"analyzer": "my_stop_an", "text": "X x y"}, [("y", 4, 5, "word", 2)]),
    ("fingerprint_type", {"analyzer": "my_fp", "text": "The b a b C"}, [("b-c", 0, 11, "fingerprint", 0)]),
    ("snowball_type", {"analyzer": "my_snow", "text": "running ponies the"}, [
        ("run", 0, 7, "<ALPHANUM>", 0), ("poni", 8, 14, "<ALPHANUM>", 1),
    ]),
    ("whitespace_type", {"analyzer": "my_ws", "text": "A b"}, [("A", 0, 1, "word", 0), ("b", 2, 3, "word", 1)]),
    ("keyword_type", {"analyzer": "my_kw", "text": "A b"}, [("A b", 0, 3, "word", 0)]),
    ("simple_type", {"analyzer": "my_simple", "text": "A1b"}, [("a", 0, 1, "word", 0), ("b", 2, 3, "word", 1)]),
    ("index_tokenizer", {"tokenizer": "my_ng", "text": "abcd"}, [
        ("ab", 0, 2, "word", 0), ("abc", 0, 3, "word", 1), ("bc", 1, 3, "word", 2), ("bcd", 1, 4, "word", 3),
        ("cd", 2, 4, "word", 4),
    ]),
    ("index_filter", {"tokenizer": "standard", "filter": ["my_syn"], "text": "tv"}, [
        ("tv", 0, 2, "<ALPHANUM>", 0), ("television", 0, 2, "SYNONYM", 0),
    ]),
    ("index_char_filter", {"tokenizer": "standard", "char_filter": ["emo"], "text": ":)"}, [
        ("happy", 0, 2, "<ALPHANUM>", 0),
    ]),
    ("global_components_still_available", {"tokenizer": "whitespace", "filter": ["uppercase"], "text": "a b"}, [
        ("A", 0, 1, "word", 0), ("B", 2, 3, "word", 1),
    ]),
    ("no_analyzer_uses_standard", {"text": "Hello World"}, [
        ("hello", 0, 5, "<ALPHANUM>", 0), ("world", 6, 11, "<ALPHANUM>", 1),
    ]),
]


@pytest.mark.parametrize("case,body,expected", INDEX_SCOPED, ids=[c[0] for c in INDEX_SCOPED])
def test_index_scoped_analyze(client, aidx, case, body, expected):
    assert _full(client, body, aidx) == expected


INDEX_ERRORS = [
    ("unknown_normalizer", {"normalizer": "nope", "text": "Hi"}, "failed to find normalizer under [nope]"),
    ("unknown_analyzer", {"analyzer": "nope", "text": "Hi"}, "failed to find analyzer [nope]"),
    ("unknown_filter", {"tokenizer": "standard", "filter": ["nope"], "text": "Hi"}, "failed to find filter under [nope]"),
    ("non_text_field", {"field": "num", "text": "42"},
     "Can't process field [num], Analysis requests are only supported on tokenized fields"),
]


@pytest.mark.parametrize("case,body,reason", INDEX_ERRORS, ids=[c[0] for c in INDEX_ERRORS])
def test_index_scoped_analyze_errors(client, aidx, case, body, reason):
    _error(_analyze_raw(client, body, aidx), 400, "illegal_argument_exception", reason)


def test_index_analyzer_not_visible_globally(client, aidx):
    _error(_analyze_raw(client, {"analyzer": "my_an", "text": "x"}), 400, "illegal_argument_exception",
           "failed to find global analyzer [my_an]")


def test_explain_index_analyzer_uses_component_names(client, aidx):
    d = _analyze(client, {"analyzer": "my_an", "text": "x", "explain": True}, aidx)["detail"]
    assert d["custom_analyzer"] is True
    assert d["charfilters"] == [{"name": "emo", "filtered_text": ["x"]}]
    assert d["tokenizer"]["name"] == "standard"
    assert [f["name"] for f in d["tokenfilters"]] == ["lowercase", "my_syn", "my_stop"]


def test_explain_normalizer(client, aidx):
    d = _analyze(client, {"normalizer": "my_norm", "text": "Ab", "explain": True}, aidx)["detail"]
    assert d["custom_analyzer"] is True
    assert d["charfilters"] == []
    assert d["tokenizer"]["name"] == "keyword"
    assert [(f["name"], [t["token"] for t in f["tokens"]]) for f in d["tokenfilters"]] == [
        ("lowercase", ["ab"]), ("asciifolding", ["ab"]),
    ]


# --------------------------------------------------------------------------
# analysis settings validation
# --------------------------------------------------------------------------

SETTINGS_ERRORS = [
    ("unknown_filter_in_analyzer",
     {"analyzer": {"a": {"type": "custom", "tokenizer": "standard", "filter": ["nope"]}}},
     "Failed to build analyzers: [a]", "[a] Custom Analyzer [a] failed to find filter under name [nope]"),
    ("custom_without_tokenizer", {"analyzer": {"a": {"type": "custom", "filter": ["lowercase"]}}},
     "Failed to build analyzers: [a]", "[a] Custom Analyzer [a] must be configured with a tokenizer"),
    ("unknown_tokenizer_in_analyzer", {"analyzer": {"a": {"type": "custom", "tokenizer": "nope"}}},
     "Failed to build analyzers: [a]", "[a] Custom Analyzer [a] failed to find tokenizer under name [nope]"),
    ("unknown_analyzer_type", {"analyzer": {"a": {"type": "nope"}}}, "Unknown analyzer type [nope] for [a]", None),
    ("unknown_filter_type", {"filter": {"f": {"type": "nope"}}}, "Unknown filter type [nope] for [f]", None),
    ("filter_without_type", {"filter": {"f": {"stopwords": ["x"]}}},
     "filter [f] must specify either an analyzer type, or a tokenizer", None),
    ("normalizer_with_stemmer", {"normalizer": {"n": {"type": "custom", "filter": ["porter_stem"]}}},
     "Custom normalizer [n] may not use filter [porter_stem]", None),
    ("normalizer_with_html_strip",
     {"normalizer": {"n": {"type": "custom", "filter": ["lowercase"], "char_filter": ["html_strip"]}}},
     "Custom normalizer [n] may not use char filter [html_strip]", None),
    ("ngram_tokenizer_diff", {"tokenizer": {"t": {"type": "ngram", "min_gram": 1, "max_gram": 5}}},
     "The difference between max_gram and min_gram in NGram Tokenizer must be less than or equal to: [1] but was "
     "[4]. This limit can be set by changing the [index.max_ngram_diff] index level setting.", None),
]


@pytest.mark.parametrize("case,analysis,reason,suppressed", SETTINGS_ERRORS, ids=[c[0] for c in SETTINGS_ERRORS])
def test_invalid_analysis_settings_rejected(client, case, analysis, reason, suppressed):
    name = _unique("bad")
    r = client.put(f"/{name}", json={"settings": {"analysis": analysis}})
    try:
        err = _error(r, 400, "illegal_argument_exception", reason)
        if suppressed:
            assert err["suppressed"][0]["type"] == "illegal_argument_exception"
            assert err["suppressed"][0]["reason"] == suppressed
        assert client.head(f"/{name}").status_code == 404
    finally:
        client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def test_custom_analyzer_type_is_implicit_with_tokenizer(client, index_factory):
    name = index_factory(settings={"analysis": {"analyzer": {"a": {"tokenizer": "whitespace", "filter": ["uppercase"]}}}})
    assert _toks(client, {"analyzer": "a", "text": "x y"}, name) == ["X", "Y"]


MAPPING_ERRORS = [
    ("unknown_analyzer", {"t": {"type": "text", "analyzer": "nope"}},
     "Failed to parse mapping [_doc]: analyzer [nope] has not been configured in mappings",
     "Failed to parse mapping [_doc]: analyzer [nope] has not been configured in mappings",
     {"type": "illegal_argument_exception", "reason": "analyzer [nope] has not been configured in mappings"}),
    ("unknown_search_analyzer", {"t": {"type": "text", "search_analyzer": "nope"}},
     "Failed to parse mapping [_doc]: analyzer [nope] has not been configured in mappings",
     "Failed to parse mapping [_doc]: analyzer [nope] has not been configured in mappings",
     {"type": "illegal_argument_exception", "reason": "analyzer [nope] has not been configured in mappings"}),
    ("unknown_search_quote_analyzer", {"t": {"type": "text", "search_quote_analyzer": "nope"}},
     "Failed to parse mapping [_doc]: analyzer [nope] has not been configured in mappings",
     "Failed to parse mapping [_doc]: analyzer [nope] has not been configured in mappings",
     {"type": "illegal_argument_exception", "reason": "analyzer [nope] has not been configured in mappings"}),
    ("unknown_normalizer", {"k": {"type": "keyword", "normalizer": "nope"}},
     "normalizer [nope] not found for field [k]",
     "Failed to parse mapping [_doc]: normalizer [nope] not found for field [k]",
     {"type": "mapper_parsing_exception", "reason": "normalizer [nope] not found for field [k]"}),
    ("analyzer_on_keyword", {"k": {"type": "keyword", "analyzer": "standard"}},
     "unknown parameter [analyzer] on mapper [k] of type [keyword]",
     "Failed to parse mapping [_doc]: unknown parameter [analyzer] on mapper [k] of type [keyword]",
     {"type": "mapper_parsing_exception", "reason": "unknown parameter [analyzer] on mapper [k] of type [keyword]"}),
]


@pytest.mark.parametrize("case,props,root_reason,reason,caused_by", MAPPING_ERRORS, ids=[c[0] for c in MAPPING_ERRORS])
def test_mapping_analysis_errors(client, case, props, root_reason, reason, caused_by):
    name = _unique("badmap")
    r = client.put(f"/{name}", json={"mappings": {"properties": props}})
    try:
        assert r.status_code == 400, r.text
        err = r.json()["error"]
        assert err["type"] == "mapper_parsing_exception"
        assert err["reason"] == reason
        assert err["root_cause"][0]["reason"] == root_reason
        assert err["caused_by"] == caused_by
    finally:
        client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def test_search_analyzer_without_analyzer_is_accepted(client, index_factory):
    name = index_factory(mappings={"properties": {"t": {"type": "text", "search_analyzer": "whitespace"}}})
    m = client.get(f"/{name}/_mapping").json()[name]["mappings"]["properties"]["t"]
    assert m == {"type": "text", "analyzer": "default", "search_analyzer": "whitespace"}


def test_update_analysis_on_open_index_rejected(client, index_factory):
    name = index_factory()
    r = client.put(f"/{name}/_settings",
                   json={"analysis": {"analyzer": {"x": {"type": "custom", "tokenizer": "whitespace"}}}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"].startswith(
        "Can't update non dynamic settings [[index.analysis.analyzer.x.tokenizer, index.analysis.analyzer.x.type]] "
        f"for open indices [[{name}/"
    )


def test_update_analysis_on_closed_index_then_use(client, index_factory):
    name = index_factory(mappings={"properties": {"t": {"type": "text"}}})
    assert client.post(f"/{name}/_close").status_code == 200
    r = client.put(f"/{name}/_settings", json={
        "analysis": {"analyzer": {"x": {"type": "custom", "tokenizer": "whitespace", "filter": ["uppercase"]}}}})
    assert r.status_code == 200, r.text
    assert r.json() == {"acknowledged": True}
    r = client.post(f"/{name}/_open", params={"wait_for_active_shards": "1"})
    assert r.status_code == 200, r.text
    assert _full(client, {"analyzer": "x", "text": "a b"}, name) == [("A", 0, 1, "word", 0), ("B", 2, 3, "word", 1)]
    settings = client.get(f"/{name}/_settings").json()[name]["settings"]["index"]["analysis"]
    assert settings == {"analyzer": {"x": {"filter": ["uppercase"], "type": "custom", "tokenizer": "whitespace"}}}
    # new field can use it, existing field's analyzer can't change
    r = client.put(f"/{name}/_mapping", json={"properties": {"u": {"type": "text", "analyzer": "x"}}})
    assert r.status_code == 200, r.text
    r = client.put(f"/{name}/_mapping", json={"properties": {"t": {"type": "text", "analyzer": "x"}}})
    _error(r, 400, "illegal_argument_exception",
           "Mapper for [t] conflicts with existing mapper:\n\tCannot update parameter [analyzer] from [default] to [x]")


def test_update_invalid_analysis_on_closed_index_rejected(client, index_factory):
    name = index_factory()
    assert client.post(f"/{name}/_close").status_code == 200
    r = client.put(f"/{name}/_settings",
                   json={"analysis": {"analyzer": {"bad": {"type": "custom", "tokenizer": "nope"}}}})
    err = _error(r, 400, "illegal_argument_exception", "Failed to build analyzers: [bad]")
    assert err["suppressed"][0]["reason"] == "[bad] Custom Analyzer [bad] failed to find tokenizer under name [nope]"
    client.post(f"/{name}/_open", params={"wait_for_active_shards": "1"})


def test_search_analyzer_can_be_updated_on_open_index(client, index_factory):
    name = index_factory(
        settings={"analysis": {"analyzer": {"ws": {"type": "custom", "tokenizer": "whitespace"}}}},
        mappings={"properties": {"t": {"type": "text", "analyzer": "ws"}}},
    )
    r = client.put(f"/{name}/_mapping", json={"properties": {"t": {"type": "text", "analyzer": "ws",
                                                                    "search_analyzer": "standard"}}})
    assert r.status_code == 200, r.text
    client.put(f"/{name}/_doc/1", json={"t": "Hello"}, params={"refresh": "true"})
    assert _ids(client, name, {"match": {"t": "Hello"}}) == []
    assert _ids(client, name, {"match": {"t": {"query": "Hello", "analyzer": "ws"}}}) == ["1"]


# --------------------------------------------------------------------------
# analyzers in action: index vs search vs quote analyzer, normalizers
# --------------------------------------------------------------------------

SEARCH_SETTINGS = {
    "analysis": {
        "filter": {
            "syn": {"type": "synonym_graph", "synonyms": ["quick, fast"]},
            "eng_stem": {"type": "stemmer", "language": "english"},
            "eg": {"type": "edge_ngram", "min_gram": 1, "max_gram": 10},
        },
        "analyzer": {
            "stemmed": {"tokenizer": "standard", "filter": ["lowercase", "eng_stem"]},
            "syn_search": {"tokenizer": "standard", "filter": ["lowercase", "syn"]},
            "exact": {"tokenizer": "standard", "filter": ["lowercase"]},
            "edge": {"tokenizer": "standard", "filter": ["lowercase", "eg"]},
        },
        "normalizer": {"fold": {"type": "custom", "filter": ["lowercase", "asciifolding"]}},
    }
}

SEARCH_MAPPING = {
    "properties": {
        "a": {"type": "text", "analyzer": "stemmed", "search_analyzer": "stemmed", "search_quote_analyzer": "exact"},
        "s": {"type": "text", "analyzer": "standard", "search_analyzer": "syn_search"},
        "e": {"type": "text", "analyzer": "edge", "search_analyzer": "standard"},
        "e2": {"type": "text", "analyzer": "edge"},
        "k": {"type": "keyword", "normalizer": "fold"},
        "m": {"type": "text"},
        "m0": {"type": "text", "position_increment_gap": 0},
    }
}

SEARCH_DOCS = {
    "1": {"a": "running quickly", "s": "fast car", "e": "elasticsearch", "e2": "elasticsearch", "k": "Café",
          "m": ["John Abraham", "Lincoln Smith"], "m0": ["John Abraham", "Lincoln Smith"]},
    "2": {"a": "runs", "s": "quick fox", "e": "elastic", "e2": "elastic", "k": "CAFE"},
    "3": {"a": "run", "s": "slow", "e": "other", "e2": "other", "k": "cafe"},
    "4": {"a": "x", "s": "x", "e": "x", "e2": "x", "k": "Ébène"},
}


@pytest.fixture(scope="module")
def sidx(client):
    name = _unique("search")
    r = client.put(f"/{name}", json={"settings": SEARCH_SETTINGS, "mappings": SEARCH_MAPPING})
    assert r.status_code == 200, r.text
    for _id, doc in SEARCH_DOCS.items():
        assert client.put(f"/{name}/_doc/{_id}", json=doc).status_code in (200, 201)
    client.post(f"/{name}/_refresh")
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


SEARCH_CASES = [
    ("match_uses_search_analyzer_stem", {"match": {"a": "running"}}, ["1", "2", "3"]),
    ("phrase_uses_quote_analyzer", {"match_phrase": {"a": "running"}}, []),
    ("phrase_with_stem_form", {"match_phrase": {"a": "run"}}, ["1", "2", "3"]),
    ("query_string_quoted_uses_quote_analyzer", {"query_string": {"query": "\"running\"", "default_field": "a"}}, []),
    ("query_string_unquoted", {"query_string": {"query": "running", "default_field": "a"}}, ["1", "2", "3"]),
    ("simple_query_string_quoted", {"simple_query_string": {"query": "\"running\"", "fields": ["a"]}}, []),
    ("query_string_quote_analyzer_override",
     {"query_string": {"query": "\"running\"", "default_field": "a", "quote_analyzer": "stemmed"}}, ["1", "2", "3"]),
    ("search_time_synonym_quick", {"match": {"s": "quick"}}, ["1", "2"]),
    ("search_time_synonym_fast", {"match": {"s": "fast"}}, ["1", "2"]),
    ("query_analyzer_overrides_search_analyzer", {"match": {"s": {"query": "fast", "analyzer": "standard"}}}, ["1"]),
    ("edge_index_standard_search_prefix", {"match": {"e": "elas"}}, ["1", "2"]),
    ("edge_both_sides_overmatches", {"match": {"e2": "elxyz"}}, ["1", "2"]),
    ("edge_full_word_too_long_for_grams", {"match": {"e": "elasticsearch"}}, []),
    ("edge_full_word_within_max", {"match": {"e": "elastic"}}, ["1", "2"]),
    ("normalizer_term", {"term": {"k": "cafe"}}, ["1", "2", "3"]),
    ("normalizer_term_query_normalized", {"term": {"k": "CAFÉ"}}, ["1", "2", "3"]),
    ("normalizer_match", {"match": {"k": "CAFÉ"}}, ["1", "2", "3"]),
    ("normalizer_terms", {"terms": {"k": ["ÉBÈNE"]}}, ["4"]),
    ("normalizer_prefix", {"prefix": {"k": "CA"}}, ["1", "2", "3"]),
    ("normalizer_wildcard", {"wildcard": {"k": "C*"}}, ["1", "2", "3"]),
    ("normalizer_range", {"range": {"k": {"gte": "C", "lte": "CAFEZ"}}}, ["1", "2", "3"]),
    ("phrase_across_values_gap_blocks", {"match_phrase": {"m": "Abraham Lincoln"}}, []),
    ("phrase_across_values_slop_99", {"match_phrase": {"m": {"query": "Abraham Lincoln", "slop": 99}}}, []),
    ("phrase_across_values_slop_100", {"match_phrase": {"m": {"query": "Abraham Lincoln", "slop": 100}}}, ["1"]),
    ("phrase_across_values_gap_0", {"match_phrase": {"m0": "Abraham Lincoln"}}, ["1"]),
]


@pytest.mark.parametrize("case,query,expected", SEARCH_CASES, ids=[c[0] for c in SEARCH_CASES])
def test_search_time_analysis(client, sidx, case, query, expected):
    assert _ids(client, sidx, query) == expected


def test_match_with_unknown_analyzer_errors(client, sidx):
    r = client.post(f"/{sidx}/_search", json={"query": {"match": {"a": {"query": "running", "analyzer": "nope"}}}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "search_phase_execution_exception"
    assert err["root_cause"][0]["type"] == "query_shard_exception"
    assert err["root_cause"][0]["reason"] == "[match] analyzer [nope] not found"


def test_normalizer_terms_aggregation(client, sidx):
    r = client.post(f"/{sidx}/_search", json={"size": 0, "aggs": {"t": {"terms": {"field": "k"}}}})
    assert r.json()["aggregations"]["t"]["buckets"] == [
        {"key": "cafe", "doc_count": 3},
        {"key": "ebene", "doc_count": 1},
    ]


def test_normalizer_sort_values(client, sidx):
    r = client.post(f"/{sidx}/_search", json={"sort": [{"k": "asc"}, {"_id": "asc"}], "_source": False})
    assert [(h["_id"], h["sort"]) for h in r.json()["hits"]["hits"]] == [
        ("1", ["cafe", "1"]), ("2", ["cafe", "2"]), ("3", ["cafe", "3"]), ("4", ["ebene", "4"]),
    ]


def test_normalizer_docvalues_vs_source(client, sidx):
    r = client.post(f"/{sidx}/_search", json={"query": {"term": {"k": "cafe"}}, "docvalue_fields": ["k"],
                                              "_source": ["k"], "sort": ["_id"]})
    assert [(h["_id"], h["_source"], h["fields"]) for h in r.json()["hits"]["hits"]] == [
        ("1", {"k": "Café"}, {"k": ["cafe"]}),
        ("2", {"k": "CAFE"}, {"k": ["cafe"]}),
        ("3", {"k": "cafe"}, {"k": ["cafe"]}),
    ]


def test_termvectors_reflect_index_analyzer(client, sidx):
    r = client.get(f"/{sidx}/_termvectors/1", params={"fields": "a"})
    assert sorted(r.json()["term_vectors"]["a"]["terms"]) == ["quickli", "run"]


def test_default_and_default_search_analyzers(client, index_factory):
    name = index_factory(
        settings={"analysis": {"analyzer": {"default": {"type": "whitespace"}, "default_search": {"type": "keyword"}}}},
        mappings={"properties": {"t": {"type": "text"}}},
    )
    assert _toks(client, {"field": "t", "text": "Hello World"}, name) == ["Hello", "World"]
    assert _toks(client, {"text": "Hello World"}, name) == ["Hello", "World"]
    client.put(f"/{name}/_doc/1", json={"t": "Hello World"}, params={"refresh": "true"})
    assert _ids(client, name, {"match": {"t": "Hello"}}) == ["1"]
    assert _ids(client, name, {"match": {"t": "Hello World"}}) == []
    assert _ids(client, name, {"match": {"t": "hello"}}) == []


def test_lowercase_normalizer_builtin_on_keyword(client, index_factory):
    name = index_factory(mappings={"properties": {"k": {"type": "keyword", "normalizer": "lowercase"}}})
    client.put(f"/{name}/_doc/1", json={"k": "MiXeD Case"}, params={"refresh": "true"})
    assert _ids(client, name, {"term": {"k": "MIXED CASE"}}) == ["1"]
    assert _ids(client, name, {"term": {"k": "mixed"}}) == []


def test_stopwords_affect_match_phrase_positions(client, index_factory):
    name = index_factory(mappings={"properties": {"t": {"type": "text", "analyzer": "english"}}})
    client.put(f"/{name}/_doc/1", json={"t": "the king of the hill"}, params={"refresh": "true"})
    assert _ids(client, name, {"match_phrase": {"t": "king hill"}}) == []
    assert _ids(client, name, {"match_phrase": {"t": "king of the hill"}}) == ["1"]
    assert _ids(client, name, {"match_phrase": {"t": "kings in a hill"}}) == ["1"]
