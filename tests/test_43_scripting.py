"""Scripting compatibility spec: Painless, expressions, stored scripts, mustache.

Every expectation below was taken from a real OpenSearch 3.9.0 node.
"""

import json
import uuid

import pytest

pytestmark = pytest.mark.scripting

# Contexts whose dynamic-compilation rate limit (default 75/5m) we raise for the
# duration of this module: the suite compiles hundreds of distinct scripts.
_RATE_CONTEXTS = [
    "painless_test",
    "filter",
    "score",
    "field",
    "update",
    "ingest",
    "aggs",
    "aggs_init",
    "aggs_map",
    "aggs_combine",
    "aggs_reduce",
    "number_sort",
    "string_sort",
    "bucket_aggregation",
    "aggregation_selector",
    "template",
    "terms_set",
    "processor_conditional",
]


@pytest.fixture(scope="module", autouse=True)
def _raise_compilation_rate(client):
    keys = [f"script.context.{c}.max_compilations_rate" for c in _RATE_CONTEXTS]
    client.put(
        "/_cluster/settings",
        json={"transient": {k: "100000/1m" for k in keys}},
    )
    yield
    client.put("/_cluster/settings", json={"transient": {k: None for k in keys}})


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def _uid():
    return uuid.uuid4().hex[:8]


def execute(client, source, params=None, context=None, setup=None, lang=None):
    script = {"source": source}
    if params is not None:
        script["params"] = params
    if lang is not None:
        script["lang"] = lang
    body = {"script": script}
    if context is not None:
        body["context"] = context
    if setup is not None:
        body["context_setup"] = setup
    return client.post("/_scripts/painless/_execute", json=body)


def run(client, source, params=None):
    r = execute(client, source, params)
    assert r.status_code == 200, r.text
    return r.json()["result"]


def script_error(client, source, params=None):
    r = execute(client, source, params)
    assert r.status_code == 400, r.text
    return r.json()["error"]


def shard_reason(resp):
    """Reason object of the first failed shard of a failed search."""
    body = resp.json()
    assert resp.status_code == 400, resp.text
    assert body["error"]["type"] == "search_phase_execution_exception"
    return body["error"]["failed_shards"][0]["reason"]


def _ndjson(lines):
    return ("\n".join(json.dumps(x) for x in lines) + "\n").encode()


# --------------------------------------------------------------------------
# shared search index
# --------------------------------------------------------------------------

MAPPING = {
    "properties": {
        "name": {"type": "keyword"},
        "price": {"type": "integer"},
        "qty": {"type": "long"},
        "rating": {"type": "double"},
        "tags": {"type": "keyword"},
        "nums": {"type": "integer"},
        "title": {"type": "text"},
        "date": {"type": "date"},
        "active": {"type": "boolean"},
    }
}

DOCS = [
    {
        "name": "apple",
        "price": 10,
        "qty": 3,
        "rating": 4.5,
        "tags": ["red", "fruit"],
        "nums": [5, 1, 3],
        "title": "red apple",
        "date": "2021-01-15",
        "active": True,
    },
    {
        "name": "banana",
        "price": 20,
        "qty": 0,
        "rating": 3.0,
        "tags": ["yellow", "fruit"],
        "nums": [2],
        "title": "yellow banana",
        "date": "2021-06-01",
        "active": False,
    },
    {
        "name": "carrot",
        "price": 5,
        "qty": 10,
        "rating": 4.0,
        "tags": ["orange", "veg"],
        "nums": [],
        "title": "orange carrot",
        "date": "2022-03-10",
        "active": True,
    },
    {"name": "durian", "price": 50, "title": "smelly durian fruit"},
]


@pytest.fixture(scope="module")
def sidx(client):
    name = f"slopensearch-scripting-{_uid()}"
    r = client.put(
        f"/{name}",
        json={
            "settings": {"number_of_shards": 1, "number_of_replicas": 0},
            "mappings": MAPPING,
        },
    )
    assert r.status_code == 200, r.text
    lines = []
    for i, d in enumerate(DOCS):
        lines.append({"index": {"_id": str(i + 1)}})
        lines.append(d)
    r = client.post(
        f"/{name}/_bulk",
        content=_ndjson(lines),
        headers={"Content-Type": "application/x-ndjson"},
        params={"refresh": "true"},
    )
    assert r.status_code == 200 and not r.json()["errors"], r.text
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def search(client, index, body, **params):
    return client.post(f"/{index}/_search", json=body, params=params)


def script_field_values(client, index, script):
    """{name: fields-or-None} for a single script field 'x', sorted by name."""
    r = search(
        client,
        index,
        {"script_fields": {"x": {"script": script}}, "sort": [{"name": "asc"}]},
    )
    assert r.status_code == 200, r.text
    return {
        h["sort"][0]: (h["fields"]["x"] if "fields" in h else None)
        for h in r.json()["hits"]["hits"]
    }


@pytest.fixture
def stored_script(client):
    created = []

    def _put(body, context=None, sid=None):
        sid = sid or f"slopensearch-script-{_uid()}"
        path = f"/_scripts/{sid}" + (f"/{context}" if context else "")
        r = client.put(path, json=body)
        created.append(sid)
        return sid, r

    yield _put
    for sid in created:
        client.delete(f"/_scripts/{sid}")


@pytest.fixture
def plain_index(client):
    created = []

    def _create(docs=None, mappings=None):
        name = f"slopensearch-scripting-{_uid()}"
        body = {"settings": {"number_of_shards": 1, "number_of_replicas": 0}}
        if mappings:
            body["mappings"] = mappings
        r = client.put(f"/{name}", json=body)
        assert r.status_code == 200, r.text
        created.append(name)
        for i, d in enumerate(docs or []):
            r = client.put(f"/{name}/_doc/{i + 1}", json=d)
            assert r.status_code == 201, r.text
        client.post(f"/{name}/_refresh")
        return name

    def _track(name):
        created.append(name)

    _create.track = _track
    yield _create
    for name in created:
        client.delete(f"/{name}", params={"ignore_unavailable": "true"})


def all_docs(client, index):
    client.post(f"/{index}/_refresh")
    r = search(client, index, {"sort": [{"_id": "asc"}], "size": 100})
    assert r.status_code == 200, r.text
    return {h["_id"]: h["_source"] for h in r.json()["hits"]["hits"]}


# ==========================================================================
# 1. Painless language semantics via _execute (painless_test context)
# ==========================================================================

ARITHMETIC = [
    ("1+1", "2"),
    ("7/2", "3"),
    ("-7/2", "-3"),
    ("7%3", "1"),
    ("-7%3", "-1"),
    ("7.0/2", "3.5"),
    ("5 / 2.0", "2.5"),
    ("5L / 2", "2"),
    ("5 / 2f", "2.5"),
    ("10 / 4 * 4", "8"),
    ("10 % 3.5", "3.0"),
    ("0.1+0.2", "0.30000000000000004"),
    ("0.1f + 0.2f", "0.3"),
    ("1/3.0f", "0.33333334"),
    ("100.0f/3", "33.333332"),
    ("(double)1/3", "0.3333333333333333"),
    ("1.0 * 3", "3.0"),
    ("3 * 1.0f", "3.0"),
    ("Integer.MAX_VALUE+1", "-2147483648"),
    ("Integer.MAX_VALUE * 2", "-2"),
    ("Integer.MAX_VALUE - Integer.MIN_VALUE", "-1"),
    ("Integer.MIN_VALUE / -1", "-2147483648"),
    ("Long.MAX_VALUE+1", "-9223372036854775808"),
    ("Long.MAX_VALUE * 2", "-2"),
    ("2147483647L+1", "2147483648"),
    ("2147483647 + 1L", "2147483648"),
    ("int a = 1000000; a * a", "-727379968"),
    ("long a = 1000000; a * a", "1000000000000"),
    ("long a = 3000000000L; a * 2", "6000000000"),
    ("1.0/0", "Infinity"),
    ("0.0/0", "NaN"),
    ("1e308*10", "Infinity"),
    ("-0.0", "-0.0"),
    ("0.0 == -0.0", "true"),
    ("Double.NaN == Double.NaN", "false"),
    ("int x = 3; x++ + ++x", "8"),
    ("int x = 10; x -= 3; x *= 2; x /= 3; x", "4"),
    ("int x = 7; x %= 4; x <<= 2; x", "12"),
    ("byte b = 127; b++; b", "-128"),
    ("short s = 32767; s += 1; s", "-32768"),
]


@pytest.mark.parametrize("source,expected", ARITHMETIC)
def test_painless_arithmetic(client, source, expected):
    assert run(client, source) == expected


BITWISE_AND_LOGIC = [
    ("1 << 3", "8"),
    ("1 << 33", "2"),
    ("1L << 40", "1099511627776"),
    ("-16 >> 2", "-4"),
    ("-7 >> 1", "-4"),
    ("-16 >>> 28", "15"),
    ("-7 >>> 30", "3"),
    ("5 & 3", "1"),
    ("5 | 3", "7"),
    ("5 ^ 3", "6"),
    ("~5", "-6"),
    ("true && false", "false"),
    ("10 > 5 && 5 > 1 || false", "true"),
    ("!true", "false"),
    ("true ^ true", "false"),
    ("def b = false; b &= true; b", "false"),
    ("def x = 1; x <<= 4; x", "16"),
    ("1 == 1L", "true"),
    ("def a = 1; def b = 1L; a == b", "true"),
    ("def x = 1000; def y = 1000; x === y", "false"),
    ("def a = [1]; def b = [1]; a == b", "true"),
    ("def a = [1]; def b = [1]; a === b", "false"),
    ("'x' == 'x'", "true"),
]


@pytest.mark.parametrize("source,expected", BITWISE_AND_LOGIC)
def test_painless_bitwise_and_logic(client, source, expected):
    assert run(client, source) == expected


CASTS_AND_LITERALS = [
    ("(int)3.9", "3"),
    ("(int)-3.9", "-3"),
    ("(long)1e10", "10000000000"),
    ("(byte)300", "44"),
    ("(byte) -129", "127"),
    ("(short) 70000", "4464"),
    ("(int) 2147483648L", "-2147483648"),
    ("(int) 1e20", "2147483647"),
    ("(long) Double.NaN", "0"),
    ("(char)65", "A"),
    ("(char) 97", "a"),
    ("(char) 66 + (char) 1", "67"),
    ("(int) 'Z'.charAt(0)", "90"),
    ("char c = (char)'a'; (int)c", "97"),
    ("(float)0.1", "0.1"),
    ("(double)(float)0.1", "0.10000000149011612"),
    ("int x = 1; long y = x; y", "1"),
    ("long y = 5; int x = (int) y; x", "5"),
    ("double d = 3; d", "3.0"),
    ("float f = 1.1f; f", "1.1"),
    ("1.0", "1.0"),
    ("1.5f", "1.5"),
    ("10L", "10"),
    ("1e3", "1000.0"),
    ("017", "15"),
    ("2.0e-5", "2.0E-5"),
    ("1e21", "1.0E21"),
    ("12345678.9", "1.23456789E7"),
    ("Float.MAX_VALUE", "3.4028235E38"),
    ("Double.MIN_VALUE", "4.9E-324"),
    ("Long.MIN_VALUE", "-9223372036854775808"),
    ("Integer.MAX_VALUE", "2147483647"),
    ("true ? 1 : 2.5", "1.0"),
    ("false ? 1 : 2.5", "2.5"),
    ("return null;", "null"),
]


@pytest.mark.parametrize("source,expected", CASTS_AND_LITERALS)
def test_painless_casts_and_literals(client, source, expected):
    assert run(client, source) == expected


STRINGS = [
    ("'a'+'b'", "ab"),
    ("'abc'.length()", "3"),
    ("'café'.length()", "4"),
    ("'日本語'.substring(1)", "本語"),
    ("'Ünïcödé'.toUpperCase()", "ÜNÏCÖDÉ"),
    ("'Hello'.toUpperCase()", "HELLO"),
    ("'Abc'.toLowerCase()", "abc"),
    ("'abc'.substring(1)", "bc"),
    ("'abc'.substring(1,2)", "b"),
    ("'abc'.charAt(1)", "b"),
    ("'abc'.indexOf('c')", "2"),
    ("'abc'.indexOf('z')", "-1"),
    ("'aXbXc'.lastIndexOf('X')", "3"),
    ("'abc'.contains('bc')", "true"),
    ("'abc'.startsWith('ab')", "true"),
    ("'abc'.endsWith('bc')", "true"),
    ("'abc'.compareTo('abd')", "-1"),
    ("'abc'.replace('b','X')", "aXc"),
    ("'a-b-c'.replace('-', '')", "abc"),
    ("'  pad  '.trim()", "pad"),
    ("'abc' + 1 + 2", "abc12"),
    ("1 + 2 + 'abc'", "3abc"),
    ("'7' + 1", "71"),
    ("'x' + null", "xnull"),
    ("null + 'x'", "nullx"),
    ("'a'.equalsIgnoreCase('A')", "true"),
    ("'abc'.isEmpty()", "false"),
    ("'abc'.chars().sum()", "294"),
    ("'abc'.hashCode()", "96354"),
    ("'hello'.encodeBase64()", "aGVsbG8="),
    ("'aGVsbG8='.decodeBase64()", "hello"),
    ("'a,b,,c'.splitOnToken(',').length", "4"),
    ("'one two'.splitOnToken(' ')[1]", "two"),
    ("'a::b::c'.splitOnToken('::', 2).length", "2"),
    ("String.join('-', ['a','b','c'])", "a-b-c"),
    ("String.valueOf(5)", "5"),
    ("String.valueOf(1.0)", "1.0"),
    ("String.valueOf((Object) null)", "null"),
    ("'abc'.toCharArray().length", "3"),
    ("new StringBuilder().append('a').append(1).toString()", "a1"),
    (
        "def sb = new StringBuilder(); for (int i = 0; i < 3; i++) sb.append(i); "
        "sb.reverse().toString()",
        "210",
    ),
    ('"double quoted"', "double quoted"),
    ("'it\\'s'", "it's"),
    ("def x = 'a'; x += 'b'; x", "ab"),
    ("Integer.parseInt('42')", "42"),
    ("Long.parseLong('9999999999') + 1", "10000000000"),
    ("Double.parseDouble('1.5')", "1.5"),
    ("Boolean.parseBoolean('TRUE')", "true"),
    ("Integer.toString(255,16)", "ff"),
    ("Integer.toBinaryString(10)", "1010"),
    ("Integer.toHexString(-1)", "ffffffff"),
    ("Double.toString(1e-7)", "1.0E-7"),
    ("Character.isDigit('5'.charAt(0))", "true"),
]


@pytest.mark.parametrize("source,expected", STRINGS)
def test_painless_strings(client, source, expected):
    assert run(client, source) == expected


REGEX = [
    (r"'abc' =~ /b/", "true"),
    (r"'abc' ==~ /b/", "false"),
    (r"'abc' ==~ /abc/", "true"),
    (r"/b/.matcher('abc').find()", "true"),
    (r"/A/i.matcher('xa').find()", "true"),
    (r"/a+/.matcher('caaab').replaceAll('X')", "cXb"),
    (r"'Hello'.replaceAll(/l/, m -> m.group().toUpperCase())", "HeLLo"),
    (r"/^\d{3}$/.matcher('123').matches()", "true"),
    (r"/^\d{3}$/.matcher('1234').matches()", "false"),
    (
        r"def m = /(\d+)-(\d+)/.matcher('10-20'); m.matches() ? m.group(2) : 'no'",
        "20",
    ),
    (
        r"'2021-03-04'.replaceAll(/(\d+)-(\d+)-(\d+)/, m -> m.group(3) + '/' + m.group(2))",
        "04/03",
    ),
    (
        r"def m = /(\w+)@(\w+)\.com/.matcher('joe@example.com'); m.find() ? m.group(2) : null",
        "example",
    ),
    (r"params.s =~ /World$/", "true"),
]


@pytest.mark.parametrize("source,expected", REGEX)
def test_painless_regex(client, source, expected):
    assert run(client, source, {"s": "Hello World"}) == expected


def test_regex_enabled_setting_default_is_limited(client):
    r = client.get(
        "/_cluster/settings",
        params={"include_defaults": "true", "flat_settings": "true"},
    )
    assert r.status_code == 200
    assert r.json()["defaults"]["script.painless.regex.enabled"] == "limited"
    assert r.json()["defaults"]["script.painless.regex.limit-factor"] == "6"


def test_regex_limit_factor_trips_circuit_breaker(client):
    err = script_error(
        client, "/(a+)+b/.matcher('aaaaaaaaaaaaaaaaaaaaaaaaaaaaac').matches()"
    )
    assert err["type"] == "script_exception"
    assert err["reason"] == "runtime error"
    assert err["caused_by"]["type"] == "circuit_breaking_exception"
    reason = err["caused_by"]["reason"]
    assert "Regular expression considered too many characters" in reason
    assert "limit factor: [6], char limit: [180], count: [181]" in reason


COLLECTIONS = [
    ("[1,2,3]", "[1, 2, 3]"),
    ("[1,2,3].size()", "3"),
    ("['a':1]", "{a=1}"),
    ("[1, 'a', 2.5, null, true]", "[1, a, 2.5, null, true]"),
    ("['k': [1, ['n': null]]]", "{k=[1, {n=null}]}"),
    ("[1,2,3].contains(2)", "true"),
    ("[1,2,3].indexOf(3)", "2"),
    ("def l = [1,2]; l.add(0, 9); l", "[9, 1, 2]"),
    ("def l = [1,2,3]; l.remove(0); l", "[2, 3]"),
    ("def l = [1,2,3]; l[-1]", "3"),
    ("def l = [1,2,3]; l.subList(1,3)", "[2, 3]"),
    ("def l = [3,1,2]; l.sort(null); l", "[1, 2, 3]"),
    ("def l = [3,1,2]; Collections.sort(l); l", "[1, 2, 3]"),
    ("def l = [3,1,2]; Collections.reverse(l); l", "[2, 1, 3]"),
    ("Collections.max([3,9,2])", "9"),
    ("def l = ['bb','a','ccc']; l.sort((x,y) -> x.length() - y.length()); l", "[a, bb, ccc]"),
    ("[1,2,3].sum()", "6.0"),
    ("[1,2,3].collect(x -> x * 10)", "[10, 20, 30]"),
    ("[1,2,3,4].findAll(x -> x > 2)", "[3, 4]"),
    ("[1,2,3].find(x -> x > 1)", "2"),
    ("[1,2,3].any(x -> x > 2)", "true"),
    ("[1,2,3].every(x -> x > 0)", "true"),
    ("['a','b'].join('|')", "a|b"),
    ("def l = []; l.isEmpty()", "true"),
    ("def s = new HashSet(); s.add(1); s.add(1); s.size()", "1"),
    ("List l = new ArrayList(); l.add('x'); l.get(0)", "x"),
    ("List l = [1,2]; l instanceof ArrayList", "true"),
    ("int[] a = new int[] {1,2,3}; a.length", "3"),
    ("def a = new int[2][3]; a[1].length", "3"),
    ("['a':1,'b':2].values()", "[1, 2]"),
    ("def m = ['a':1]; m.put('b', 2); m.size()", "2"),
    ("def m = ['a':1]; m.b = 5; m", "{a=1, b=5}"),
    ("def m = ['a':1]; m['c']", "null"),
    ("def m = ['a':1]; m.getOrDefault('z', 42)", "42"),
    ("def m = ['a':1]; m.containsKey('a')", "true"),
    ("def m = ['a':1, 'b':2]; m.remove('a'); m", "{b=2}"),
    ("def m = [:]; m.isEmpty()", "true"),
    ("Map m = [:]; m instanceof HashMap", "true"),
    # map literals are java.util.HashMap: iteration order is hash order
    ("def m = ['z':1,'a':2,'m':3]; m.keySet()", "[a, z, m]"),
    ("def m = new TreeMap(); m.put('z',1); m.put('a',2); m", "{a=2, z=1}"),
    ("def m = ['a':1,'b':2]; int s = 0; for (e in m.entrySet()) { s += e.getValue() } s", "3"),
    ("def m = ['a':1,'b':2]; m.collect((k,v) -> k + v)", "[a1, b2]"),
    ("def m = ['a':1,'b':2]; m.findAll((k,v) -> v > 1)", "{b=2}"),
    ("def m = ['a':1]; m.computeIfAbsent('b', k -> 7); m.b", "7"),
]


@pytest.mark.parametrize("source,expected", COLLECTIONS)
def test_painless_collections(client, source, expected):
    assert run(client, source) == expected


STREAMS_AND_LAMBDAS = [
    ("[3,1,2].stream().map(x -> x*2).collect(Collectors.toList())", "[6, 2, 4]"),
    ("[5,1,4].stream().sorted().collect(Collectors.toList())", "[1, 4, 5]"),
    ("[1,2,3,4].stream().filter(x -> x % 2 == 0).count()", "2"),
    ("[1,2,3].stream().mapToInt(x -> x).sum()", "6"),
    ("[1,2,3].stream().reduce(0, (a,b) -> a+b)", "6"),
    ("[1,2,3].stream().mapToInt(x -> x).max().getAsInt()", "3"),
    ("[1,2,3,4].stream().mapToInt(x -> x).average().getAsDouble()", "2.5"),
    (
        "['b','a','c'].stream().sorted(Comparator.reverseOrder())"
        ".collect(Collectors.joining(','))",
        "c,b,a",
    ),
    (
        "[[1,2],[3]].stream().flatMap(x -> x.stream()).collect(Collectors.toList())",
        "[1, 2, 3]",
    ),
    ("int fact(int n) { return n <= 1 ? 1 : n * fact(n - 1); } return fact(10);", "3628800"),
]


@pytest.mark.parametrize("source,expected", STREAMS_AND_LAMBDAS)
def test_painless_streams_and_lambdas(client, source, expected):
    assert run(client, source) == expected


CONTROL_FLOW = [
    ("int s=0; for (int i=0;i<10;i++) s+=i; s", "45"),
    ("def s=0; for (x in [1,2,3]) s+=x; s", "6"),
    ("int i=0; while(i<5) i++; i", "5"),
    ("int i=0; do { i+=2 } while(i<5); i", "6"),
    ("int i = 0; for (;;) { if (++i > 3) break; } i", "4"),
    ("def r = []; for (def i = 3; i > 0; i--) r.add(i); r", "[3, 2, 1]"),
    ("int c = 0; for (int i = 0; i < 10; i++) { if (i % 2 == 0) continue; c++; } c", "5"),
    ("1 > 0 ? 'y' : 'n'", "y"),
    ("def s = 'b'; s == 'a' ? 1 : s == 'b' ? 2 : 3", "2"),
    ("if (params.a > 0) { return 'pos' } else { return 'neg' }", "pos"),
]


@pytest.mark.parametrize("source,expected", CONTROL_FLOW)
def test_painless_control_flow(client, source, expected):
    assert run(client, source, {"a": 1}) == expected


DEF_AND_NULL_SAFETY = [
    ("def x = 5; x instanceof Integer", "true"),
    ("def x = 5L; x / 2", "2"),
    ("def x = 3; x / 2", "1"),
    ("def x = 3; (double) x / 2", "1.5"),
    ("def a = 7; def b = 2.0; a / b", "3.5"),
    ("def x = 1; x = 'str'; x", "str"),
    ("def x = 1; x += 1.5; x", "2"),
    ("def x = 1; x instanceof def", "true"),
    ("def x = (Object) 'str'; x instanceof String", "true"),
    ("def a = null; a?.length()", "null"),
    ("def l = [1,2,3]; l?.size()", "3"),
    ("def a = null; a ?: 'd'", "d"),
    ("def a = null; a == null ? 'isnull' : 'not'", "isnull"),
    ("def m = ['a': ['b': null]]; m.a?.b?.c", "null"),
    ("def m = null; m?.a?.b ?: 'fallback'", "fallback"),
    # elvis only replaces null, not "falsy" values
    ("def x = false; x ?: 'elvis'", "false"),
    ("def x = ''; x ?: 'elvis'", ""),
    ("def x = 0; x ?: 'elvis'", "0"),
]


@pytest.mark.parametrize("source,expected", DEF_AND_NULL_SAFETY)
def test_painless_def_typing_and_null_safety(client, source, expected):
    assert run(client, source) == expected


MATH = [
    ("Math.max(3,4)", "4.0"),
    ("Math.max(3,4L)", "4.0"),
    ("Math.min(1.5, 2)", "1.5"),
    ("Math.abs(-5)", "5.0"),
    ("Math.abs(Integer.MIN_VALUE)", "2.147483648E9"),
    ("Math.pow(2,10)", "1024.0"),
    ("Math.sqrt(2)", "1.4142135623730951"),
    ("Math.round(2.5)", "3"),
    ("Math.round(-2.5)", "-2"),
    ("Math.round(1.4999)", "1"),
    ("Math.round(2.5f)", "3"),
    ("Math.floor(-1.5)", "-2.0"),
    ("Math.floor(2.7)", "2.0"),
    ("Math.ceil(1.2)", "2.0"),
    ("Math.rint(2.5)", "2.0"),
    ("Math.log(Math.E)", "1.0"),
    ("Math.log10(1000)", "3.0"),
    ("Math.exp(0)", "1.0"),
    ("Math.signum(-3.2)", "-1.0"),
    ("Math.hypot(3, 4)", "5.0"),
    ("Math.cbrt(27)", "3.0"),
    ("Math.toDegrees(Math.PI)", "180.0"),
    ("Math.sin(0)", "0.0"),
    ("Math.atan2(1, 1)", "0.7853981633974483"),
    ("Math.min(-0.0, 0.0)", "-0.0"),
    ("Math.PI", "3.141592653589793"),
    ("Math.E", "2.718281828459045"),
    ("Math.random() < 1", "true"),
    ("Integer.bitCount(255)", "8"),
    ("Integer.compare(3, 5)", "-1"),
    ("Double.compare(Double.NaN, 1.0)", "1"),
    ("Double.isNaN(0.0/0)", "true"),
    ("Long.toString(Long.MIN_VALUE)", "-9223372036854775808"),
    ("Objects.equals(null, null)", "true"),
]


@pytest.mark.parametrize("source,expected", MATH)
def test_painless_math_and_boxed_helpers(client, source, expected):
    assert run(client, source) == expected


DATES = [
    ("Instant.ofEpochMilli(0).toString()", "1970-01-01T00:00:00Z"),
    ("ZonedDateTime.parse('2020-01-02T03:04:05Z').getYear()", "2020"),
    (
        "ZonedDateTime.parse('2020-01-02T03:04:05Z').plusDays(30).toString()",
        "2020-02-01T03:04:05Z",
    ),
    ("ZonedDateTime.parse('2020-01-02T03:04:05Z').getDayOfWeek()", "THURSDAY"),
    ("LocalDate.of(2020, 2, 28).plusDays(1).toString()", "2020-02-29"),
    ("Duration.ofHours(2).toMinutes()", "120"),
    ("ChronoUnit.DAYS.between(LocalDate.of(2020,1,1), LocalDate.of(2021,1,1))", "366"),
    ("DateTimeFormatter.ofPattern('yyyy/MM/dd').format(LocalDate.of(2021, 3, 4))", "2021/03/04"),
]


@pytest.mark.parametrize("source,expected", DATES)
def test_painless_java_time(client, source, expected):
    assert run(client, source) == expected


# ---- params ---------------------------------------------------------------

PARAMS_CASES = [
    ("params.a + params.b", {"a": 1, "b": 2}, "3"),
    ("params['a']", {"a": 1}, "1"),
    ("params.size()", {"a": 1, "b": 2, "c": 3}, "3"),
    ("params.missing", {"a": 1}, "null"),
    ("return params", None, "{}"),
    ("params.x", {"x": [1, {"a": None}]}, "[1, {a=null}]"),
    ("params.x", {"x": 1.0}, "1.0"),
    ("params.x / 2", {"x": 7}, "3"),
    ("params.x / 2", {"x": 7.0}, "3.5"),
    ("params.x / 2", {"x": 10000000000}, "5000000000"),
    ("params.x + 1", {"x": 2147483647}, "-2147483648"),
    ("params.x instanceof Integer", {"x": 1}, "true"),
    ("params.x instanceof Long", {"x": 2147483648}, "true"),
    ("params.x instanceof Double", {"x": 1.5}, "true"),
    ("params.x + params.y", {"x": "a", "y": 1}, "a1"),
    ("params.x == null", {"x": None}, "true"),
    ("params.x.size()", {"x": {"a": 1, "b": 2}}, "2"),
    ("params.m.k", {"m": {"k": "v"}}, "v"),
    ("def x = params.l; x.add(4); x", {"l": [1, 2, 3]}, "[1, 2, 3, 4]"),
    ("params.s.length()", {"s": "Hello World"}, "11"),
    ("params.x", {"x": True}, "true"),
]


@pytest.mark.parametrize("source,params,expected", PARAMS_CASES)
def test_painless_params(client, source, params, expected):
    assert run(client, source, params) == expected


def test_same_script_different_params(client):
    assert run(client, "params.v * 3", {"v": 2}) == "6"
    assert run(client, "params.v * 3", {"v": 5}) == "15"
    assert run(client, "params.v * 3", {"v": 1.5}) == "4.5"


def test_execute_result_is_always_string_in_painless_test(client):
    r = execute(client, "[1:2]")
    assert r.status_code == 200
    assert r.json() == {"result": "{1=2}"}
    r = execute(client, "true")
    assert r.json() == {"result": "true"}


# ---- error shapes ---------------------------------------------------------

COMPILE_ERRORS = [
    # constant folding happens at compile time
    ("1/0", "arithmetic_exception", "/ by zero"),
    ("x = ", "illegal_argument_exception", "unexpected end of script."),
    ("doc['x']", "illegal_argument_exception", "cannot resolve symbol [doc]"),
    ("int x = 'a'", "class_cast_exception", "Cannot cast from [java.lang.String] to [int]."),
    ("long y = 5; int x = y; x", "class_cast_exception", "Cannot cast from [long] to [int]."),
    ("float f = 1.1; f", "class_cast_exception", "Cannot cast from [double] to [float]."),
    ("char c = 'a'; c", "class_cast_exception", "Cannot cast from [java.lang.String] to [char]."),
    ("(String)5", "class_cast_exception", "Cannot cast from [int] to [java.lang.String]."),
    (
        "'x' * 3",
        "class_cast_exception",
        "Cannot cast from [java.lang.String] to [int].",
    ),
    (
        "'abc' < 'abd'",
        "class_cast_exception",
        "cannot apply the less than operator [<] to the types "
        "[java.lang.String] and [java.lang.String]",
    ),
    (
        "false | true",
        "class_cast_exception",
        "cannot apply the boolean or operator [|] to the types [boolean] and [boolean]",
    ),
    (
        "String.format('%05d', 42)",
        "class_cast_exception",
        "Cannot cast from [int] to [def[]].",
    ),
    (
        "Math.floorDiv(7, 2)",
        "illegal_argument_exception",
        "static method [java.lang.Math, floorDiv/2] not found",
    ),
    (
        "Math.addExact(Integer.MAX_VALUE,1)",
        "illegal_argument_exception",
        "static method [java.lang.Math, addExact/2] not found",
    ),
    (
        "'a b  c'.split(' ').length",
        "illegal_argument_exception",
        "member method [java.lang.String, split/1] not found",
    ),
    (
        "'x'.repeat(3)",
        "illegal_argument_exception",
        "member method [java.lang.String, repeat/1] not found",
    ),
    (
        "'abc'.matches('a.c')",
        "illegal_argument_exception",
        "member method [java.lang.String, matches/1] not found",
    ),
    (
        "Arrays.asList(3,1,2).size()",
        "illegal_argument_exception",
        "static method [java.util.Arrays, asList/3] not found",
    ),
    (
        "Objects.hash(1,2)",
        "illegal_argument_exception",
        "static method [java.util.Objects, hash/2] not found",
    ),
    ("new Thread()", "illegal_argument_exception", "Not a type [Thread]."),
    ("Runtime.getRuntime()", "illegal_argument_exception", "cannot resolve symbol [Runtime]"),
    (
        "Class.forName('java.lang.String')",
        "illegal_argument_exception",
        "cannot resolve symbol [Class]",
    ),
    ("null == null", "illegal_argument_exception", "extraneous comparison of [null] constants"),
    (
        "String s = 'abc'; s[1]",
        "illegal_argument_exception",
        "Illegal array access on type [java.lang.String].",
    ),
    (
        "String s = null; s?.length() ?: -1",
        "illegal_argument_exception",
        "Result of null safe operator must be nullable",
    ),
    (
        "int x = 5; switch (x) { case 5: return 'five'; default: return 'other'; }",
        "illegal_argument_exception",
        "unexpected token ['{'] was expecting one of [{<EOF>, ';'}].",
    ),
    (
        "def f = x -> x + 1; f(1)",
        "illegal_argument_exception",
        "unexpected token ['->'] was expecting one of [{<EOF>, ';'}].",
    ),
    (
        "1_000",
        "illegal_argument_exception",
        "unexpected token ['_000'] was expecting one of [{<EOF>, ';'}].",
    ),
    (
        '"a\\nb".length()',
        "illegal_argument_exception",
        'unexpected character ["a\\n]. The only valid escape sequences in strings '
        'starting with ["] are [\\\\] and [\\"].',
    ),
    # ternary branches are not boxed implicitly
    (
        "params.a == 1 ? null : 5",
        "class_cast_exception",
        "Cannot cast from [int] to [java.lang.Object].",
    ),
    (
        "params.a == 1 ? 'x' : true",
        "class_cast_exception",
        "Cannot cast from [boolean] to [java.lang.Object].",
    ),
    (
        "Integer x = 1000; Integer y = 1000; x == y",
        "class_cast_exception",
        "Cannot cast from [int] to [java.lang.Integer].",
    ),
]


@pytest.mark.parametrize("source,cause_type,cause_reason", COMPILE_ERRORS)
def test_painless_compile_errors(client, source, cause_type, cause_reason):
    err = script_error(client, source)
    assert err["type"] == "script_exception"
    assert err["reason"] == "compile error"
    assert err["lang"] == "painless"
    assert err["script"] == source
    assert err["caused_by"]["type"] == cause_type
    assert err["caused_by"]["reason"] == cause_reason
    assert err["root_cause"][0]["type"] == "script_exception"
    assert err["root_cause"][0]["reason"] == "compile error"
    assert "caused_by" not in err["root_cause"][0]


RUNTIME_ERRORS = [
    ("int a = 1; int b = 0; a/b", "arithmetic_exception", "/ by zero"),
    ("throw new IllegalArgumentException('boom')", "illegal_argument_exception", "boom"),
    (
        "def x = null; x.length()",
        "null_pointer_exception",
        'Cannot invoke "Object.getClass()" because "callArgs[0]" is null',
    ),
    (
        "params.missing.foo",
        "null_pointer_exception",
        'Cannot invoke "Object.getClass()" because "callArgs[0]" is null',
    ),
    (
        "'abcdef'.substring(4, 2)",
        "string_index_out_of_bounds_exception",
        "Range [4, 2) out of bounds for length 6",
    ),
    (
        "def x = 5; x.getClass()",
        "illegal_argument_exception",
        "dynamic method [java.lang.Integer, getClass/0] not found",
    ),
    (
        "params.s.split(' ')[1]",
        "illegal_argument_exception",
        "dynamic method [java.lang.String, split/1] not found",
    ),
    (
        "int i = 0; while (true) { i++; }",
        "painless_error",
        "The maximum number of statements that can be executed in a loop has been reached.",
    ),
    (
        "params.b - 1",
        "class_cast_exception",
        "Cannot cast java.lang.Boolean to java.lang.Integer",
    ),
]


@pytest.mark.parametrize("source,cause_type,cause_reason", RUNTIME_ERRORS)
def test_painless_runtime_errors(client, source, cause_type, cause_reason):
    err = script_error(client, source, {"s": "a b", "b": True})
    assert err["type"] == "script_exception"
    assert err["reason"] == "runtime error"
    assert err["lang"] == "painless"
    assert err["script"] == source
    assert err["caused_by"]["type"] == cause_type
    assert err["caused_by"]["reason"] == cause_reason


def test_runtime_error_index_out_of_bounds_has_java_frames(client):
    err = script_error(client, "[1,2,3][5]")
    assert err["reason"] == "runtime error"
    assert err["caused_by"]["type"] == "index_out_of_bounds_exception"
    stack = err["script_stack"]
    assert stack[-2:] == ["[1,2,3][5]", "       ^---- HERE"]
    assert any("java.util.ArrayList.get" in frame for frame in stack[:-2])


def test_error_position_and_script_stack_exact(client):
    r = execute(client, "def x = null; x.length()")
    assert r.status_code == 400
    body = r.json()
    assert body["status"] == 400
    err = body["error"]
    assert err["script_stack"] == ["x.length()", " ^---- HERE"]
    assert err["position"] == {"offset": 15, "start": 14, "end": 24}
    root = err["root_cause"]
    assert len(root) == 1
    assert root[0]["position"] == {"offset": 15, "start": 14, "end": 24}


def test_compile_error_position_for_unexpected_end(client):
    err = script_error(client, "x = ")
    assert err["script_stack"] == ["x = ", "    ^---- HERE"]
    assert err["position"] == {"offset": 4, "start": 0, "end": 4}
    assert err["caused_by"]["caused_by"]["type"] == "no_viable_alt_exception"


def test_compile_error_long_script_is_truncated_in_stack(client):
    err = script_error(client, "Math.addExact(Integer.MAX_VALUE,1)")
    assert err["script_stack"] == ["Math.addExact(Integer.MAX_VAL ...", "    ^---- HERE"]
    assert err["position"] == {"offset": 4, "start": 0, "end": 29}


def test_debug_explain_shape(client):
    err = script_error(client, "Debug.explain([1,2])")
    assert err["type"] == "script_exception"
    assert err["reason"] == "runtime error"
    assert err["painless_class"] == "java.util.ArrayList"
    assert err["java_class"] == "java.util.ArrayList"
    assert err["to_string"] == "[1, 2]"
    assert err["caused_by"] == {"type": "painless_explain_error", "reason": None}


def test_requires_non_null_npe_has_null_reason(client):
    err = script_error(client, "Objects.requireNonNull(null)")
    assert err["caused_by"]["type"] == "null_pointer_exception"
    assert err["caused_by"].get("reason") is None


def test_script_too_large(client):
    source = "'" + "a" * 70000 + "'"
    r = execute(client, source)
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"].startswith(
        "exceeded max allowed inline script size in bytes [65535] with size [70002]"
    )


# ---- _execute API shape ---------------------------------------------------


def test_execute_missing_body(client):
    r = client.get("/_scripts/painless/_execute")
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "parse_exception"
    assert err["reason"] == "request body or source parameter is required"


def test_execute_missing_script(client):
    r = client.post("/_scripts/painless/_execute", json={})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == "Required [script]"


def test_execute_script_as_string_is_rejected(client):
    r = client.post("/_scripts/painless/_execute", json={"script": "1 + 1"})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "x_content_parse_exception"
    assert "script doesn't support values of type: VALUE_STRING" in err["reason"]


@pytest.mark.parametrize("context", ["bogus", "update"])
def test_execute_unsupported_context(client, context):
    r = client.post(
        "/_scripts/painless/_execute",
        json={"script": {"source": "1"}, "context": context},
    )
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"]["type"] == "unsupported_operation_exception"
    assert err["caused_by"]["reason"] == f"unsupported script context name [{context}]"


def test_execute_stored_script_id_is_rejected(client):
    r = client.post("/_scripts/painless/_execute", json={"script": {"id": "anything"}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == "Validation Failed: 1: only inline scripts are supported;"


def test_execute_other_lang_is_rejected(client):
    r = execute(client, "1+1", lang="expression")
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == (
        "expression engine does not know how to handle script context [painless_test]"
    )
    r = execute(client, "1", lang="mustache")
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == (
        "mustache engine does not know how to handle context [painless_test]"
    )


def test_execute_context_setup_missing_index(client):
    r = execute(
        client,
        "true",
        context="filter",
        setup={"index": f"slopensearch-nope-{_uid()}", "document": {}},
    )
    assert r.status_code == 404
    assert r.json()["error"]["type"] == "index_not_found_exception"


# ---- filter / score contexts with a document --------------------------------

CTX_MAPPING = {
    "properties": {
        "n": {"type": "integer"},
        "tags": {"type": "keyword"},
        "t": {"type": "text"},
        "d": {"type": "date"},
        "f": {"type": "double"},
        "g": {"type": "geo_point"},
        "b": {"type": "boolean"},
    }
}
CTX_DOC = {
    "n": 5,
    "tags": ["z", "a", "m"],
    "t": "hello world",
    "d": "2020-01-02T03:04:05Z",
    "f": 1.5,
    "g": {"lat": 10, "lon": 20},
    "b": True,
}


@pytest.fixture(scope="module")
def ctx_index(client):
    name = f"slopensearch-scripting-ctx-{_uid()}"
    r = client.put(
        f"/{name}",
        json={
            "settings": {"number_of_shards": 1, "number_of_replicas": 0},
            "mappings": CTX_MAPPING,
        },
    )
    assert r.status_code == 200, r.text
    yield name
    client.delete(f"/{name}", params={"ignore_unavailable": "true"})


FILTER_CTX = [
    ("doc['n'].value > 3", True),
    ("doc['n'].value > 7", False),
    ("doc['tags'].size() == 3", True),
    ("doc['tags'][0] == 'a'", True),
    ("doc['tags'][2] == 'z'", True),
    ("doc['d'].value.getYear() == 2020", True),
    ("doc['n'].empty", False),
    ("doc.containsKey('n') && doc.containsKey('nope') == false", True),
    ("doc['b'].value", True),
    ("params.limit < doc['n'].value", True),
]


@pytest.mark.parametrize("source,expected", FILTER_CTX)
def test_execute_filter_context(client, ctx_index, source, expected):
    r = execute(
        client,
        source,
        params={"limit": 4},
        context="filter",
        setup={"index": ctx_index, "document": CTX_DOC},
    )
    assert r.status_code == 200, r.text
    assert r.json() == {"result": expected}


SCORE_CTX = [
    ("doc['n'].value * 2.0", 10.0),
    ("doc['f'].value", 1.5),
    ("doc['b'].value ? 1 : 0", 1.0),
    ("doc['n'].size()", 1.0),
    ("doc['tags'].size() + params.k", 13.0),
]


@pytest.mark.parametrize("source,expected", SCORE_CTX)
def test_execute_score_context(client, ctx_index, source, expected):
    r = execute(
        client,
        source,
        params={"k": 10},
        context="score",
        setup={
            "index": ctx_index,
            "document": CTX_DOC,
            "query": {"match": {"t": "hello"}},
        },
    )
    assert r.status_code == 200, r.text
    assert r.json() == {"result": expected}


def test_execute_score_context_score_variable(client, ctx_index):
    r = execute(
        client,
        "_score * 2",
        context="score",
        setup={"index": ctx_index, "document": CTX_DOC, "query": {"match": {"t": "hello"}}},
    )
    assert r.status_code == 200
    result = r.json()["result"]
    assert isinstance(result, float) and result > 0


def test_execute_score_context_geo_point(client, ctx_index):
    r = execute(
        client,
        "doc['g'].lat + doc['g'].lon",
        context="score",
        setup={"index": ctx_index, "document": CTX_DOC, "query": {"match_all": {}}},
    )
    assert r.status_code == 200
    assert r.json()["result"] == pytest.approx(30.0, abs=1e-6)


CTX_ERRORS = [
    ("filter", "doc['n'].value", "class_cast_exception", "cannot cast def [long] to boolean"),
    (
        "filter",
        "doc['tags'].value",
        "class_cast_exception",
        "cannot cast def [java.lang.String] to boolean",
    ),
    (
        "filter",
        "doc['missing'].size()",
        "illegal_argument_exception",
        "No field found for [missing] in mapping",
    ),
    (
        "filter",
        "params._source.n == 5",
        "null_pointer_exception",
        'Cannot invoke "Object.getClass()" because "callArgs[0]" is null',
    ),
    (
        "filter",
        "doc['n'].value.getClass().getName()",
        "illegal_argument_exception",
        "dynamic method [java.lang.Long, getClass/0] not found",
    ),
    (
        "score",
        "doc['tags']",
        "class_cast_exception",
        "cannot implicitly cast def [org.opensearch.index.fielddata.ScriptDocValues.Strings] "
        "to double",
    ),
]


@pytest.mark.parametrize("context,source,cause_type,cause_reason", CTX_ERRORS)
def test_execute_context_runtime_errors(
    client, ctx_index, context, source, cause_type, cause_reason
):
    setup = {"index": ctx_index, "document": CTX_DOC}
    if context == "score":
        setup["query"] = {"match_all": {}}
    r = execute(client, source, context=context, setup=setup)
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "runtime error"
    assert err["caused_by"]["type"] == cause_type
    assert err["caused_by"]["reason"] == cause_reason


def test_execute_text_field_doc_access_needs_fielddata(client, ctx_index):
    r = execute(
        client,
        "doc['t'].value == 'x'",
        context="filter",
        setup={"index": ctx_index, "document": CTX_DOC},
    )
    assert r.status_code == 400
    cause = r.json()["error"]["caused_by"]
    assert cause["type"] == "illegal_argument_exception"
    assert cause["reason"].startswith("Text fields are not optimised for operations")


# ==========================================================================
# 2. Scripts in search
# ==========================================================================

SCRIPT_FIELDS = [
    (
        "doc['price'].value * 2",
        {"apple": [20], "banana": [40], "carrot": [10], "durian": [100]},
    ),
    (
        "doc['price'].value / 3",
        {"apple": [3], "banana": [6], "carrot": [1], "durian": [16]},
    ),
    (
        "doc['price'].getValue()",
        {"apple": [10], "banana": [20], "carrot": [5], "durian": [50]},
    ),
    # multi-valued doc values come back sorted; empty -> field omitted
    ("doc['nums']", {"apple": [1, 3, 5], "banana": [2], "carrot": None, "durian": None}),
    (
        "doc['tags']",
        {"apple": ["fruit", "red"], "banana": ["fruit", "yellow"], "carrot": ["orange", "veg"], "durian": None},
    ),
    ("doc['nums'].size()", {"apple": [3], "banana": [1], "carrot": [0], "durian": [0]}),
    ("doc['nums'].length", {"apple": [3], "banana": [1], "carrot": [0], "durian": [0]}),
    (
        "doc['nums'].empty ? -1 : doc['nums'].value",
        {"apple": [1], "banana": [2], "carrot": [-1], "durian": [-1]},
    ),
    (
        "doc['name'].value.length()",
        {"apple": [5], "banana": [6], "carrot": [6], "durian": [6]},
    ),
    ("doc['_id'].value", {"apple": ["1"], "banana": ["2"], "carrot": ["3"], "durian": ["4"]}),
    (
        "params._source.title",
        {
            "apple": ["red apple"],
            "banana": ["yellow banana"],
            "carrot": ["orange carrot"],
            "durian": ["smelly durian fruit"],
        },
    ),
    # _source keeps original order; empty list -> omitted; missing -> [null]
    ("params._source.nums", {"apple": [5, 1, 3], "banana": [2], "carrot": None, "durian": [None]}),
    (
        "params['_source']['name'] + '!'",
        {"apple": ["apple!"], "banana": ["banana!"], "carrot": ["carrot!"], "durian": ["durian!"]},
    ),
    ("['a':1]", {k: [{"a": 1}] for k in ("apple", "banana", "carrot", "durian")}),
    ("null", {k: [None] for k in ("apple", "banana", "carrot", "durian")}),
    (
        "doc['date'].size() == 0 ? 'none' : doc['date'].value.getMonthValue() + '/' + doc['date'].value.getYear()",
        {"apple": ["1/2021"], "banana": ["6/2021"], "carrot": ["3/2022"], "durian": ["none"]},
    ),
    (
        "def r = null; if (doc['active'].size() > 0) { r = !doc['active'].value } return r",
        {"apple": [False], "banana": [True], "carrot": [False], "durian": [None]},
    ),
    (
        "doc['rating'].size() == 0 ? 0 : doc['rating'].value * 2",
        {"apple": [9.0], "banana": [6.0], "carrot": [8.0], "durian": [0]},
    ),
]


@pytest.mark.parametrize("source,expected", SCRIPT_FIELDS)
def test_script_fields(client, sidx, source, expected):
    assert script_field_values(client, sidx, {"source": source}) == expected


def test_script_fields_with_params(client, sidx):
    got = script_field_values(
        client, sidx, {"source": "doc['price'].value * params.f", "params": {"f": 0.5}}
    )
    assert got == {"apple": [5.0], "banana": [10.0], "carrot": [2.5], "durian": [25.0]}


@pytest.mark.parametrize("field", ["nums", "qty", "rating", "date", "active"])
def test_script_fields_missing_value_access_fails(client, sidx, field):
    r = search(client, sidx, {"script_fields": {"x": {"script": f"doc['{field}'].value"}}})
    reason = shard_reason(r)
    assert reason["type"] == "script_exception"
    assert reason["reason"] == "runtime error"
    assert reason["caused_by"]["type"] == "illegal_state_exception"
    assert reason["caused_by"]["reason"] == (
        "A document doesn't have a value for a field! "
        "Use doc[<field>].size()==0 to check if a document is missing a field!"
    )


def test_script_fields_text_field_doc_access_fails(client, sidx):
    r = search(client, sidx, {"script_fields": {"x": {"script": "doc['title'].value"}}})
    reason = shard_reason(r)
    assert reason["type"] == "script_exception"
    assert reason["caused_by"]["type"] == "illegal_argument_exception"
    assert "set fielddata=true on [title]" in reason["caused_by"]["reason"]


def test_script_fields_unknown_doc_method(client, sidx):
    r = search(client, sidx, {"script_fields": {"x": {"script": "doc['nums'].min()"}}})
    reason = shard_reason(r)
    assert reason["caused_by"]["reason"] == (
        "dynamic method [org.opensearch.index.fielddata.ScriptDocValues.Longs, min/0] not found"
    )


def test_script_fields_date_value_is_zoned_datetime(client, sidx):
    r = search(client, sidx, {"script_fields": {"x": {"script": "doc['date'].value.millis"}}})
    reason = shard_reason(r)
    assert reason["caused_by"]["reason"] == (
        "dynamic getter [java.time.ZonedDateTime, millis] not found"
    )


def test_script_fields_omit_source_by_default(client, sidx):
    r = search(
        client,
        sidx,
        {"query": {"term": {"name": "apple"}}, "script_fields": {"b": {"script": "2"}}},
    )
    hit = r.json()["hits"]["hits"][0]
    assert "_source" not in hit
    assert hit["fields"] == {"b": [2]}


def test_script_fields_combined_with_source_and_docvalues(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {"term": {"name": "apple"}},
            "script_fields": {"b": {"script": "2"}},
            "_source": ["name"],
            "docvalue_fields": ["price"],
        },
    )
    hit = r.json()["hits"]["hits"][0]
    assert hit["_source"] == {"name": "apple"}
    assert hit["fields"] == {"b": [2], "price": [10]}


def test_script_fields_multiple(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {"term": {"name": "apple"}},
            "script_fields": {"b": {"script": "2"}, "a": {"script": "'one'"}},
        },
    )
    assert r.json()["hits"]["hits"][0]["fields"] == {"a": ["one"], "b": [2]}


def test_script_fields_source_mutation_leaks_between_fields(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {"term": {"name": "apple"}},
            "script_fields": {
                "a": {"script": "params._source.price = 999; return params._source.price"},
                "b": {"script": "params._source.price"},
            },
        },
    )
    assert r.json()["hits"]["hits"][0]["fields"] == {"a": [999], "b": [999]}


def test_script_fields_ignore_failure(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {"term": {"name": "durian"}},
            "script_fields": {"b": {"script": "doc['qty'].value", "ignore_failure": True}},
        },
    )
    assert r.status_code == 200
    hit = r.json()["hits"]["hits"][0]
    assert hit["_id"] == "4"
    assert "fields" not in hit


def test_script_fields_score_not_available(client, sidx):
    r = search(client, sidx, {"script_fields": {"x": {"script": "_score"}}})
    reason = shard_reason(r)
    assert reason["reason"] == "compile error"
    assert reason["caused_by"]["reason"] == "cannot resolve symbol [_score]"


def test_script_fields_debug_explain_doc_values(client, sidx):
    r = search(
        client,
        sidx,
        {"script_fields": {"x": {"script": "Debug.explain(doc['price'])"}}, "sort": ["name"]},
    )
    reason = shard_reason(r)
    assert reason["painless_class"] == "org.opensearch.index.fielddata.ScriptDocValues.Longs"
    assert reason["java_class"] == "org.opensearch.index.fielddata.ScriptDocValues$Longs"
    assert reason["to_string"] == "[10]"


@pytest.mark.parametrize(
    "lang,reason",
    [
        ("mustache", "mustache engine does not know how to handle context [field]"),
        ("klingon", "script_lang not supported [klingon]"),
    ],
)
def test_script_fields_bad_lang(client, sidx, lang, reason):
    r = search(
        client,
        sidx,
        {"script_fields": {"x": {"script": {"source": "1", "lang": lang}}}},
    )
    shard = shard_reason(r)
    assert shard["type"] == "illegal_argument_exception"
    assert shard["reason"] == reason


# ---- script query ----------------------------------------------------------

SCRIPT_QUERY = [
    ("doc['price'].value > params.min", {"min": 10}, ["banana", "durian"]),
    ("doc['tags'].contains('fruit')", None, ["apple", "banana"]),
    ("doc['nums'].size() > 0", None, ["apple", "banana"]),
    ("doc['price'].value % 2 == 0", None, ["apple", "banana", "durian"]),
    ("return doc['name'].value.startsWith('b')", None, ["banana"]),
    ("doc['qty'].size() == 0", None, ["durian"]),
    ("doc['nums'].size() > 0 && doc['nums'][0] == 1", None, ["apple"]),
    ("false", None, []),
]


@pytest.mark.parametrize("source,params,expected", SCRIPT_QUERY)
def test_script_query_filter(client, sidx, source, params, expected):
    script = {"source": source}
    if params:
        script["params"] = params
    r = search(
        client,
        sidx,
        {
            "query": {"bool": {"filter": {"script": {"script": script}}}},
            "sort": [{"name": "asc"}],
        },
    )
    assert r.status_code == 200, r.text
    assert [h["sort"][0] for h in r.json()["hits"]["hits"]] == expected


def test_script_query_scores_constant_one(client, sidx):
    r = search(
        client,
        sidx,
        {"query": {"script": {"script": "doc['price'].value > 10"}}, "_source": False},
    )
    hits = r.json()["hits"]["hits"]
    assert [(h["_id"], h["_score"]) for h in hits] == [("2", 1.0), ("4", 1.0)]


def test_script_query_non_boolean_result(client, sidx):
    r = search(
        client, sidx, {"query": {"bool": {"filter": {"script": {"script": "doc['price'].value"}}}}}
    )
    reason = shard_reason(r)
    assert reason["caused_by"] == {
        "type": "class_cast_exception",
        "reason": "cannot cast def [long] to boolean",
    }


def test_script_query_no_source_access(client, sidx):
    r = search(
        client,
        sidx,
        {"query": {"bool": {"filter": {"script": {"script": "params._source.price > 10"}}}}},
    )
    reason = shard_reason(r)
    assert reason["caused_by"]["type"] == "null_pointer_exception"


def test_script_query_no_score_access_compile_error(client, sidx):
    r = search(client, sidx, {"query": {"script": {"script": "_score > 0"}}})
    reason = shard_reason(r)
    assert reason["type"] == "query_shard_exception"
    assert reason["reason"] == "failed to create query: compile error"
    assert reason["caused_by"]["caused_by"]["reason"] == "cannot resolve symbol [_score]"
    root = r.json()["error"]["root_cause"][0]
    assert root["type"] == "script_exception"
    assert root["reason"] == "compile error"


def test_script_query_param_type_mismatch(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "script": {
                    "script": {"source": "doc['price'].value > params.x", "params": {"x": "10"}}
                }
            }
        },
    )
    reason = shard_reason(r)
    assert reason["caused_by"]["reason"] == (
        "Cannot apply [>] operation to types [java.lang.Long] and [java.lang.String]."
    )


def test_search_script_error_envelope(client, sidx):
    r = search(client, sidx, {"script_fields": {"x": {"script": "doc['nope'].value"}}})
    assert r.status_code == 400
    body = r.json()
    assert body["status"] == 400
    err = body["error"]
    assert err["type"] == "search_phase_execution_exception"
    assert err["reason"] == "all shards failed"
    assert err["grouped"] is True
    assert err["root_cause"][0]["type"] == "script_exception"
    shard = err["failed_shards"][0]
    assert shard["index"] == sidx
    assert shard["shard"] == 0
    assert shard["reason"]["caused_by"]["reason"] == "No field found for [nope] in mapping"


# ---- script_score ----------------------------------------------------------

SCRIPT_SCORE = [
    ("doc['price'].value", None, [("4", 50.0), ("2", 20.0), ("1", 10.0), ("3", 5.0)]),
    (
        "doc['price'].value * params.f",
        {"f": 0.5},
        [("4", 25.0), ("2", 10.0), ("1", 5.0), ("3", 2.5)],
    ),
    ("_score + 1", None, [("1", 2.0), ("2", 2.0), ("3", 2.0), ("4", 2.0)]),
    (
        "saturation(doc['price'].value, 10)",
        None,
        [("4", 0.8333333), ("2", 0.6666667), ("1", 0.5), ("3", 0.33333334)],
    ),
    (
        "sigmoid(doc['price'].value, 10, 2)",
        None,
        [("4", 0.96153843), ("2", 0.8), ("1", 0.5), ("3", 0.2)],
    ),
    (
        "decayNumericGauss(params.o, params.s, 0, 0.5, doc['price'].value)",
        {"o": 20, "s": 10},
        [("2", 1.0), ("1", 0.5), ("3", 0.2102241), ("4", 0.001953125)],
    ),
    (
        "decayNumericExp(params.o, params.s, 0, 0.5, doc['price'].value)",
        {"o": 20, "s": 10},
        [("2", 1.0), ("1", 0.5), ("3", 0.35355338), ("4", 0.125)],
    ),
    (
        "decayNumericLinear(params.o, params.s, 0, 0.5, doc['price'].value)",
        {"o": 20, "s": 10},
        [("2", 1.0), ("1", 0.5), ("3", 0.25), ("4", 0.0)],
    ),
    (
        "Math.log(doc['price'].value)",
        None,
        [("4", 3.912023), ("2", 2.9957323), ("1", 2.3025851), ("3", 1.609438)],
    ),
    (
        "doc['rating'].size() == 0 ? 0 : doc['rating'].value",
        None,
        [("1", 4.5), ("3", 4.0), ("2", 3.0), ("4", 0.0)],
    ),
]


@pytest.mark.parametrize("source,params,expected", SCRIPT_SCORE)
def test_script_score(client, sidx, source, params, expected):
    script = {"source": source}
    if params:
        script["params"] = params
    r = search(
        client,
        sidx,
        {"query": {"script_score": {"query": {"match_all": {}}, "script": script}}},
    )
    assert r.status_code == 200, r.text
    got = [(h["_id"], h["_score"]) for h in r.json()["hits"]["hits"]]
    assert [g[0] for g in got] == [e[0] for e in expected]
    for (_, g), (_, e) in zip(got, expected):
        assert g == pytest.approx(e, rel=1e-6)


def test_script_score_uses_inner_query_score(client, sidx):
    plain = search(client, sidx, {"query": {"match": {"title": "fruit"}}}).json()
    scored = search(
        client,
        sidx,
        {
            "query": {
                "script_score": {
                    "query": {"match": {"title": "fruit"}},
                    "script": {"source": "_score * 2"},
                }
            }
        },
    ).json()
    assert [h["_id"] for h in scored["hits"]["hits"]] == ["4"]
    assert scored["hits"]["hits"][0]["_score"] == pytest.approx(
        plain["hits"]["hits"][0]["_score"] * 2, rel=1e-6
    )


def test_script_score_min_score(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "script_score": {
                    "query": {"match_all": {}},
                    "script": {"source": "doc['price'].value"},
                    "min_score": 15,
                }
            }
        },
    )
    assert [(h["_id"], h["_score"]) for h in r.json()["hits"]["hits"]] == [
        ("4", 50.0),
        ("2", 20.0),
    ]


def test_script_score_boost(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "script_score": {
                    "query": {"match_all": {}},
                    "script": {"source": "doc['price'].value"},
                    "boost": 2,
                }
            }
        },
    )
    assert [h["_score"] for h in r.json()["hits"]["hits"]] == [100.0, 40.0, 20.0, 10.0]


@pytest.mark.parametrize(
    "source,shown",
    [("-1", "-1.0"), ("doc['price'].value - 15", "-5.0"), ("0.0/0", "NaN")],
)
def test_script_score_negative_or_nan_rejected(client, sidx, source, shown):
    r = search(
        client,
        sidx,
        {"query": {"script_score": {"query": {"match_all": {}}, "script": {"source": source}}}},
    )
    reason = shard_reason(r)
    assert reason["type"] == "illegal_argument_exception"
    assert reason["reason"] == (
        f"script_score script returned an invalid score [{shown}] for doc [0]. "
        "Must be a non-negative score!"
    )


def test_script_score_infinity_serialized_as_string(client, sidx):
    r = search(
        client,
        sidx,
        {"query": {"script_score": {"query": {"match_all": {}}, "script": {"source": "1.0/0"}}}},
    )
    assert r.status_code == 200
    hits = r.json()["hits"]
    assert [h["_score"] for h in hits["hits"]] == ["Infinity"] * 4
    assert hits["max_score"] is None


def test_script_score_string_return_compile_error(client, sidx):
    r = search(
        client,
        sidx,
        {"query": {"script_score": {"query": {"match_all": {}}, "script": {"source": "'abc'"}}}},
    )
    reason = shard_reason(r)
    assert reason["type"] == "query_shard_exception"
    assert reason["caused_by"]["caused_by"] == {
        "type": "class_cast_exception",
        "reason": "Cannot cast from [java.lang.String] to [double].",
    }


def test_script_score_explanation(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "script_score": {
                    "query": {"term": {"name": "apple"}},
                    "script": {"source": "doc['price'].value * 2"},
                }
            },
            "explain": True,
        },
    )
    expl = r.json()["hits"]["hits"][0]["_explanation"]
    assert expl["value"] == 20.0
    assert expl["description"] == (
        "script score function, computed with script:\"Script{type=inline, lang='painless', "
        "idOrCode='doc['price'].value * 2', options={}, params={}}\""
    )
    assert expl["details"] == []


def test_script_score_random_score_is_deterministic(client, sidx):
    body = {
        "query": {
            "script_score": {
                "query": {"match_all": {}},
                "script": {"source": "randomScore(100, '_seq_no')"},
            }
        }
    }
    a = [(h["_id"], h["_score"]) for h in search(client, sidx, body).json()["hits"]["hits"]]
    b = [(h["_id"], h["_score"]) for h in search(client, sidx, body).json()["hits"]["hits"]]
    assert a == b
    assert all(0 <= s < 1 for _, s in a)


# ---- function_score --------------------------------------------------------


def test_function_score_script_score_with_params(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "function_score": {
                    "query": {"match_all": {}},
                    "script_score": {
                        "script": {"source": "doc['price'].value + params.a", "params": {"a": 1}}
                    },
                }
            }
        },
    )
    assert [(h["_id"], h["_score"]) for h in r.json()["hits"]["hits"]] == [
        ("4", 51.0),
        ("2", 21.0),
        ("1", 11.0),
        ("3", 6.0),
    ]


def test_function_score_boost_mode_replace_uses_score(client, sidx):
    plain = search(client, sidx, {"query": {"match": {"title": "fruit"}}}).json()
    r = search(
        client,
        sidx,
        {
            "query": {
                "function_score": {
                    "query": {"match": {"title": "fruit"}},
                    "script_score": {"script": "_score * 10"},
                    "boost_mode": "replace",
                }
            }
        },
    )
    assert r.json()["hits"]["hits"][0]["_score"] == pytest.approx(
        plain["hits"]["hits"][0]["_score"] * 10, rel=1e-6
    )


def test_function_score_functions_sum(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "function_score": {
                    "query": {"match_all": {}},
                    "functions": [
                        {"script_score": {"script": "doc['price'].value"}},
                        {"weight": 2, "filter": {"term": {"name": "apple"}}},
                    ],
                    "score_mode": "sum",
                    "boost_mode": "replace",
                }
            }
        },
    )
    assert [(h["_id"], h["_score"]) for h in r.json()["hits"]["hits"]] == [
        ("4", 50.0),
        ("2", 20.0),
        ("1", 12.0),
        ("3", 5.0),
    ]


def test_function_score_negative_script_rejected(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "function_score": {"query": {"match_all": {}}, "script_score": {"script": "-1"}}
            }
        },
    )
    reason = shard_reason(r)
    assert reason["reason"] == (
        "script score function must not produce negative scores, but got: [-1.0]"
    )


# ---- script sort ------------------------------------------------------------


def _sort(client, index, sort):
    r = search(client, index, {"sort": sort, "_source": False})
    assert r.status_code == 200, r.text
    return [(h["_id"], h["sort"]) for h in r.json()["hits"]["hits"]]


def test_script_sort_number(client, sidx):
    got = _sort(
        client,
        sidx,
        {"_script": {"type": "number", "script": {"source": "doc['price'].value * -1"}, "order": "asc"}},
    )
    assert got == [("4", [-50.0]), ("2", [-20.0]), ("1", [-10.0]), ("3", [-5.0])]


def test_script_sort_number_desc_ties_by_doc(client, sidx):
    got = _sort(
        client,
        sidx,
        {
            "_script": {
                "type": "number",
                "script": {"source": "doc['qty'].size()==0 ? 0 : doc['qty'].value"},
                "order": "desc",
            }
        },
    )
    assert got == [("3", [10.0]), ("1", [3.0]), ("2", [0.0]), ("4", [0.0])]


def test_script_sort_string(client, sidx):
    got = _sort(
        client,
        sidx,
        {
            "_script": {
                "type": "string",
                "script": {"source": "doc['name'].value.substring(1)"},
                "order": "asc",
            }
        },
    )
    assert got == [("2", ["anana"]), ("3", ["arrot"]), ("1", ["pple"]), ("4", ["urian"])]


def test_script_sort_with_secondary(client, sidx):
    got = _sort(
        client,
        sidx,
        [
            {"_script": {"type": "number", "script": {"source": "doc['price'].value % 3"}, "order": "asc"}},
            {"name": "desc"},
        ],
    )
    assert got == [
        ("1", [1.0, "apple"]),
        ("4", [2.0, "durian"]),
        ("3", [2.0, "carrot"]),
        ("2", [2.0, "banana"]),
    ]


def test_script_sort_with_params(client, sidx):
    got = _sort(
        client,
        sidx,
        {
            "_script": {
                "type": "number",
                "script": {
                    "source": "Math.abs(doc['price'].value - params.t)",
                    "params": {"t": 18},
                },
                "order": "asc",
            }
        },
    )
    assert got == [("2", [2.0]), ("1", [8.0]), ("3", [13.0]), ("4", [32.0])]


def test_script_sort_string_type_requires_string(client, sidx):
    r = search(
        client,
        sidx,
        {"sort": {"_script": {"type": "string", "script": {"source": "doc['price'].value"}}}},
    )
    reason = shard_reason(r)
    assert reason["caused_by"]["reason"] == "cannot implicitly cast def [long] to java.lang.String"


def test_script_sort_number_type_with_string_compile_error(client, sidx):
    r = search(
        client,
        sidx,
        {"sort": {"_script": {"type": "number", "script": {"source": "'x'"}}}},
    )
    reason = shard_reason(r)
    assert reason["reason"] == "compile error"
    assert reason["caused_by"]["reason"] == "Cannot cast from [java.lang.String] to [double]."


def test_script_sort_requires_type(client, sidx):
    r = search(client, sidx, {"sort": {"_script": {"script": {"source": "1"}}}})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert r.json()["error"]["reason"] == "Required [type]"


def test_script_sort_unknown_type(client, sidx):
    r = search(client, sidx, {"sort": {"_script": {"type": "bogus", "script": {"source": "1"}}}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"]["reason"] == "Unknown ScriptSortType [bogus]"


# ---- aggregations ------------------------------------------------------------


def _aggs(client, index, aggs, query=None):
    body = {"size": 0, "aggs": aggs}
    if query:
        body["query"] = query
    r = search(client, index, body)
    assert r.status_code == 200, r.text
    return r.json()["aggregations"]


def _buckets(agg):
    return [(b["key"], b["doc_count"]) for b in agg["buckets"]]


def test_terms_agg_script_string_keys(client, sidx):
    a = _aggs(
        client,
        sidx,
        {"t": {"terms": {"script": {"source": "doc['price'].value > 15 ? 'expensive' : 'cheap'"}}}},
    )
    assert _buckets(a["t"]) == [("cheap", 2), ("expensive", 2)]


def test_terms_agg_script_multivalued(client, sidx):
    a = _aggs(client, sidx, {"t": {"terms": {"script": "doc['tags']"}}})
    assert _buckets(a["t"]) == [
        ("fruit", 2),
        ("orange", 1),
        ("red", 1),
        ("veg", 1),
        ("yellow", 1),
    ]


def test_terms_agg_script_numeric_without_value_type_is_string(client, sidx):
    a = _aggs(client, sidx, {"t": {"terms": {"script": "doc['price'].value"}}})
    assert _buckets(a["t"]) == [("10", 1), ("20", 1), ("5", 1), ("50", 1)]


def test_terms_agg_script_value_type_long(client, sidx):
    a = _aggs(client, sidx, {"t": {"terms": {"script": "doc['price'].value", "value_type": "long"}}})
    assert _buckets(a["t"]) == [(5, 1), (10, 1), (20, 1), (50, 1)]


def test_terms_agg_script_double_string_keys(client, sidx):
    a = _aggs(client, sidx, {"t": {"terms": {"script": "doc['price'].value / 10.0"}}})
    assert _buckets(a["t"]) == [("0.5", 1), ("1.0", 1), ("2.0", 1), ("5.0", 1)]


def test_terms_agg_value_script(client, sidx):
    a = _aggs(client, sidx, {"t": {"terms": {"field": "price", "script": "_value * 2"}}})
    assert _buckets(a["t"]) == [(10.0, 1), (20.0, 1), (40.0, 1), (100.0, 1)]


def test_terms_agg_script_order_key_desc(client, sidx):
    a = _aggs(
        client, sidx, {"t": {"terms": {"script": "doc['name'].value", "order": {"_key": "desc"}}}}
    )
    assert [k for k, _ in _buckets(a["t"])] == ["durian", "carrot", "banana", "apple"]


@pytest.mark.parametrize(
    "agg,expected",
    [
        ({"sum": {"script": "doc['price'].value * 2"}}, {"value": 170.0}),
        ({"avg": {"field": "price", "script": "_value + 1"}}, {"value": 22.25}),
        (
            {"max": {"script": {"source": "doc['price'].value * params.m", "params": {"m": 3}}}},
            {"value": 150.0},
        ),
        (
            {"stats": {"script": "doc['nums'].size()"}},
            {"count": 4, "min": 0.0, "max": 3.0, "avg": 1.0, "sum": 4.0},
        ),
        ({"min": {"script": "doc['price'].value - 1"}}, {"value": 4.0}),
        ({"value_count": {"script": "doc['price'].value"}}, {"value": 4}),
    ],
)
def test_metric_agg_scripts(client, sidx, agg, expected):
    assert _aggs(client, sidx, {"m": agg})["m"] == expected


def test_histogram_agg_script(client, sidx):
    a = _aggs(client, sidx, {"h": {"histogram": {"script": "doc['price'].value", "interval": 20}}})
    assert _buckets(a["h"]) == [(0.0, 2), (20.0, 1), (40.0, 1)]


def test_range_agg_script(client, sidx):
    a = _aggs(
        client,
        sidx,
        {"r": {"range": {"script": "doc['price'].value", "ranges": [{"to": 10}, {"from": 10}]}}},
    )
    assert a["r"]["buckets"] == [
        {"key": "*-10.0", "to": 10.0, "doc_count": 1},
        {"key": "10.0-*", "from": 10.0, "doc_count": 3},
    ]


def test_filter_agg_with_script_query(client, sidx):
    a = _aggs(client, sidx, {"f": {"filter": {"script": {"script": "doc['price'].value > 10"}}}})
    assert a["f"] == {"doc_count": 2}


def test_agg_script_missing_field_fails(client, sidx):
    r = search(
        client, sidx, {"size": 0, "aggs": {"t": {"terms": {"script": {"source": "doc['nope'].value"}}}}}
    )
    reason = shard_reason(r)
    assert reason["caused_by"]["reason"] == "No field found for [nope] in mapping"


def test_bucket_script(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "h": {
                "terms": {"field": "name"},
                "aggs": {
                    "p": {"sum": {"field": "price"}},
                    "x": {"bucket_script": {"buckets_path": {"p": "p"}, "script": "params.p * 2"}},
                },
            }
        },
    )
    assert [(b["key"], b["x"]["value"]) for b in a["h"]["buckets"]] == [
        ("apple", 20.0),
        ("banana", 40.0),
        ("carrot", 10.0),
        ("durian", 100.0),
    ]


def test_bucket_script_with_count_path_in_histogram(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "h": {
                "histogram": {"field": "price", "interval": 10},
                "aggs": {
                    "x": {
                        "bucket_script": {
                            "buckets_path": {"c": "_count"},
                            "script": "params.c * 10",
                        }
                    }
                },
            }
        },
    )
    assert [(b["key"], b["doc_count"], b["x"]["value"]) for b in a["h"]["buckets"]] == [
        (0.0, 1, 10.0),
        (10.0, 1, 10.0),
        (20.0, 1, 10.0),
        (30.0, 0, 0.0),
        (40.0, 0, 0.0),
        (50.0, 1, 10.0),
    ]


def test_bucket_selector(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "h": {
                "terms": {"field": "name"},
                "aggs": {
                    "p": {"sum": {"field": "price"}},
                    "x": {"bucket_selector": {"buckets_path": {"p": "p"}, "script": "params.p > 15"}},
                },
            }
        },
    )
    assert a["h"]["buckets"] == [
        {"key": "banana", "doc_count": 1, "p": {"value": 20.0}},
        {"key": "durian", "doc_count": 1, "p": {"value": 50.0}},
    ]


@pytest.mark.parametrize(
    "agg_type,source,cause_type",
    [
        ("bucket_selector", "params.p", "class_cast_exception"),
        ("bucket_script", "params.p / params.missing", "null_pointer_exception"),
    ],
)
def test_pipeline_script_errors(client, sidx, agg_type, source, cause_type):
    r = search(
        client,
        sidx,
        {
            "size": 0,
            "aggs": {
                "h": {
                    "terms": {"field": "name"},
                    "aggs": {
                        "p": {"sum": {"field": "price"}},
                        "x": {agg_type: {"buckets_path": {"p": "p"}, "script": source}},
                    },
                }
            },
        },
    )
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "search_phase_execution_exception"
    assert err["caused_by"]["type"] == "script_exception"
    assert err["caused_by"]["reason"] == "runtime error"
    assert err["caused_by"]["caused_by"]["type"] == cause_type


def test_scripted_metric_sum(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "sm": {
                "scripted_metric": {
                    "init_script": "state.total = 0",
                    "map_script": "state.total += doc['price'].value",
                    "combine_script": "return state.total",
                    "reduce_script": "double s = 0; for (t in states) { s += t } return s",
                }
            }
        },
    )
    assert a["sm"] == {"value": 85.0}


def test_scripted_metric_collect_list(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "sm": {
                "scripted_metric": {
                    "init_script": "state.l = []",
                    "map_script": "state.l.add(doc['name'].value)",
                    "combine_script": "return state.l",
                    "reduce_script": "def all = []; for (s in states) all.addAll(s); all.sort(null); return all",
                }
            }
        },
    )
    assert a["sm"] == {"value": ["apple", "banana", "carrot", "durian"]}


def test_scripted_metric_states_list_and_no_init(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "sm": {
                "scripted_metric": {
                    "map_script": "state.c = (state.c ?: 0) + 1",
                    "combine_script": "return state.c",
                    "reduce_script": "return states",
                }
            }
        },
    )
    assert a["sm"] == {"value": [4]}


def test_scripted_metric_params(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "sm": {
                "scripted_metric": {
                    "init_script": "state.x = params.base",
                    "map_script": "state.x += 1",
                    "combine_script": "return state.x",
                    "reduce_script": "return states[0] + params.extra",
                    "params": {"base": 100, "extra": 5},
                }
            }
        },
    )
    assert a["sm"] == {"value": 109}


def test_scripted_metric_respects_query(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "sm": {
                "scripted_metric": {
                    "init_script": "state.c=0",
                    "map_script": "state.c += 1",
                    "combine_script": "return state.c",
                    "reduce_script": "return states.stream().mapToInt(x->x).sum()",
                }
            }
        },
        query={"term": {"name": "apple"}},
    )
    assert a["sm"] == {"value": 1}


def test_scripted_metric_reduce_null(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "sm": {
                "scripted_metric": {
                    "map_script": "state.c = 1",
                    "combine_script": "return state",
                    "reduce_script": "return null",
                }
            }
        },
    )
    assert a["sm"] == {"value": None}


@pytest.mark.parametrize(
    "spec,missing",
    [
        ({"map_script": "state.c = 1"}, "combineScript"),
        ({"map_script": "state.c = 1", "combine_script": "return state"}, "reduceScript"),
    ],
)
def test_scripted_metric_required_scripts(client, sidx, spec, missing):
    r = search(client, sidx, {"size": 0, "aggs": {"sm": {"scripted_metric": spec}}})
    reason = shard_reason(r)
    assert reason["type"] == "illegal_argument_exception"
    assert reason["reason"] == f"[{missing}] must not be null: [sm]"


def test_terms_set_minimum_should_match_script(client, sidx):
    def run_ts(src):
        r = search(
            client,
            sidx,
            {
                "query": {
                    "terms_set": {
                        "tags": {
                            "terms": ["red", "fruit", "veg"],
                            "minimum_should_match_script": {"source": src},
                        }
                    }
                },
                "sort": ["name"],
            },
        )
        assert r.status_code == 200, r.text
        return [h["_id"] for h in r.json()["hits"]["hits"]]

    assert run_ts("Math.min(params.num_terms, 2)") == ["1"]
    assert run_ts("params.num_terms") == []
    assert run_ts("1") == ["1", "2", "3"]


# ==========================================================================
# 3. Expression language
# ==========================================================================

EXPRESSION_FIELDS = [
    ("doc['price'].value * 2", [20.0, 40.0, 10.0, 100.0]),
    ("doc['price']", [10.0, 20.0, 5.0, 50.0]),
    ("7 / 2", [3.5] * 4),
    ("doc['price'].value % 3", [1.0, 2.0, 2.0, 2.0]),
    ("doc['nums'].length", [3.0, 1.0, 0.0, 0.0]),
    ("doc['nums'].min()", [1.0, 2.0, 0.0, 0.0]),
    ("doc['nums'].max()", [5.0, 2.0, 0.0, 0.0]),
    ("doc['nums'].sum()", [9.0, 2.0, 0.0, 0.0]),
    ("doc['nums'].avg()", [3.0, 2.0, 0.0, 0.0]),
    ("doc['nums'].median()", [3.0, 2.0, 0.0, 0.0]),
    ("doc['nums'].count()", [3.0, 1.0, 0.0, 0.0]),
    ("doc['nums'].empty", [0.0, 0.0, 1.0, 1.0]),
    # missing values read as 0 in expressions (no exception)
    ("doc['qty'].value", [3.0, 0.0, 10.0, 0.0]),
    ("doc['date'].value", [1610668800000.0, 1622505600000.0, 1646870400000.0, 0.0]),
    ("doc['date'].date.year", [2021.0, 2021.0, 2022.0, 0.0]),
    ("doc['date'].getYear()", [2021.0, 2021.0, 2022.0, 0.0]),
    ("doc['active'].value", [1.0, 0.0, 1.0, 0.0]),
    ("floor(doc['rating'].value)", [4.0, 3.0, 4.0, 0.0]),
    ("doc['price'].value > 10 ? 1 : 0", [0.0, 1.0, 0.0, 1.0]),
    ("max(doc['price'].value, 15)", [15.0, 20.0, 15.0, 50.0]),
    ("pow(2, 3)", [8.0] * 4),
    ("abs(-3)", [3.0] * 4),
    ("ln(doc['price'].value)", [2.302585092994046, 2.995732273553991, 1.6094379124341003, 3.912023005428146]),
    ("sqrt(doc['price'].value)", [3.1622776601683795, 4.47213595499958, 2.23606797749979, 7.0710678118654755]),
    ("doc['price'].empty ? 0 : 1", [1.0] * 4),
]


@pytest.mark.parametrize("source,expected", EXPRESSION_FIELDS)
def test_expression_script_fields(client, sidx, source, expected):
    got = script_field_values(client, sidx, {"source": source, "lang": "expression"})
    assert [got[n][0] for n in ("apple", "banana", "carrot", "durian")] == expected


def test_expression_numeric_params(client, sidx):
    got = script_field_values(
        client,
        sidx,
        {"source": "doc['price'].value + x", "lang": "expression", "params": {"x": 1}},
    )
    assert got == {"apple": [11.0], "banana": [21.0], "carrot": [6.0], "durian": [51.0]}


EXPRESSION_LINK_ERRORS = [
    ("doc['name'].value", None, "Field [name] must be numeric, date, or geopoint"),
    ("doc['tags'].value", None, "Field [tags] must be numeric, date, or geopoint"),
    ("params._source.price", None, "Unknown variable [params]"),
    ("_source.price", None, "Unknown variable [_source]"),
    ("_score", None, "Unknown variable [_score]"),
    ("x", {"x": "str"}, "Parameter [x] must be a numeric type"),
    ("doc['missingf'].value", None, "Field [missingf] does not exist in mappings"),
]


@pytest.mark.parametrize("source,params,cause", EXPRESSION_LINK_ERRORS)
def test_expression_link_errors(client, sidx, source, params, cause):
    script = {"source": source, "lang": "expression"}
    if params:
        script["params"] = params
    r = search(client, sidx, {"script_fields": {"x": {"script": script}}})
    reason = shard_reason(r)
    assert reason["type"] == "script_exception"
    assert reason["reason"] == "link error"
    assert reason["lang"] == "expression"
    assert reason["caused_by"] == {"type": "parse_exception", "reason": cause}


def test_expression_compile_error(client, sidx):
    r = search(
        client,
        sidx,
        {"script_fields": {"x": {"script": {"source": "doc['price'].value + ", "lang": "expression"}}}},
    )
    reason = shard_reason(r)
    assert reason["reason"] == "compile error"
    assert reason["caused_by"]["type"] == "parse_exception"
    assert reason["caused_by"]["reason"] == "unexpected end of expression"
    assert "position" not in reason


def test_expression_script_score(client, sidx):
    plain = search(client, sidx, {"query": {"match": {"title": "fruit"}}}).json()
    r = search(
        client,
        sidx,
        {
            "query": {
                "script_score": {
                    "query": {"match": {"title": "fruit"}},
                    "script": {"source": "_score * 10 + doc['price'].value", "lang": "expression"},
                }
            }
        },
    )
    assert r.status_code == 200
    assert r.json()["hits"]["hits"][0]["_score"] == pytest.approx(
        plain["hits"]["hits"][0]["_score"] * 10 + 50, rel=1e-6
    )


def test_expression_sort_number(client, sidx):
    got = _sort(
        client,
        sidx,
        {
            "_script": {
                "type": "number",
                "script": {"source": "doc['price'].value * -1", "lang": "expression"},
                "order": "asc",
            }
        },
    )
    assert got == [("4", [-50.0]), ("2", [-20.0]), ("1", [-10.0]), ("3", [-5.0])]


def test_expression_sort_string_unsupported(client, sidx):
    r = search(
        client,
        sidx,
        {
            "sort": {
                "_script": {
                    "type": "string",
                    "script": {"source": "doc['price'].value", "lang": "expression"},
                }
            }
        },
    )
    reason = shard_reason(r)
    assert reason["reason"] == (
        "expression engine does not know how to handle script context [string_sort]"
    )


def test_expression_filter_query(client, sidx):
    r = search(
        client,
        sidx,
        {
            "query": {
                "bool": {
                    "filter": {
                        "script": {"script": {"source": "doc['price'].value > 10", "lang": "expression"}}
                    }
                }
            },
            "sort": ["name"],
        },
    )
    assert [h["_id"] for h in r.json()["hits"]["hits"]] == ["2", "4"]


def test_expression_aggs(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "s": {"sum": {"script": {"source": "doc['price'].value * 2", "lang": "expression"}}},
            "t": {"terms": {"script": {"source": "doc['price'].value * 2", "lang": "expression"}}},
        },
    )
    assert a["s"] == {"value": 170.0}
    assert _buckets(a["t"]) == [("10.0", 1), ("100.0", 1), ("20.0", 1), ("40.0", 1)]


def test_expression_bucket_script_and_selector(client, sidx):
    a = _aggs(
        client,
        sidx,
        {
            "h": {
                "terms": {"field": "name"},
                "aggs": {
                    "p": {"sum": {"field": "price"}},
                    "x": {
                        "bucket_script": {
                            "buckets_path": {"p": "p"},
                            "script": {"source": "p * 3", "lang": "expression"},
                        }
                    },
                    "sel": {
                        "bucket_selector": {
                            "buckets_path": {"p": "p"},
                            "script": {"source": "p > 15", "lang": "expression"},
                        }
                    },
                },
            }
        },
    )
    assert [(b["key"], b["x"]["value"]) for b in a["h"]["buckets"]] == [
        ("banana", 60.0),
        ("durian", 150.0),
    ]


def test_expression_not_allowed_in_ingest(client):
    r = client.post(
        "/_ingest/pipeline/_simulate",
        json={
            "pipeline": {"processors": [{"script": {"lang": "expression", "source": "1"}}]},
            "docs": [{"_source": {"a": 1}}],
        },
    )
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == (
        "expression engine does not know how to handle script context [ingest]"
    )


# ==========================================================================
# 4. Update / update_by_query / reindex / ingest
# ==========================================================================


@pytest.fixture
def udoc(client, plain_index):
    name = plain_index([{"n": 1, "tags": ["a"], "obj": {"x": 1}, "s": "hi"}])
    return name


def _update(client, index, body, doc_id="1", **params):
    return client.post(
        f"/{index}/_update/{doc_id}", json=body, params=dict({"refresh": "true"}, **params)
    )


def _get_source(client, index, doc_id="1"):
    r = client.get(f"/{index}/_doc/{doc_id}")
    return r.json().get("_source") if r.status_code == 200 else None


UPDATE_CASES = [
    ("ctx._source.n += 1", None, {"n": 2, "tags": ["a"], "obj": {"x": 1}, "s": "hi"}),
    ("ctx._source.n += params.d", {"d": 10}, {"n": 11, "tags": ["a"], "obj": {"x": 1}, "s": "hi"}),
    ("ctx._source.tags.add('b')", None, {"n": 1, "tags": ["a", "b"], "obj": {"x": 1}, "s": "hi"}),
    ("ctx._source.newf = 'x'", None, {"n": 1, "tags": ["a"], "obj": {"x": 1}, "s": "hi", "newf": "x"}),
    ("ctx._source.remove('s')", None, {"n": 1, "tags": ["a"], "obj": {"x": 1}}),
    ("ctx._source.obj.y = 2", None, {"n": 1, "tags": ["a"], "obj": {"x": 1, "y": 2}, "s": "hi"}),
    ("ctx._source.s = ctx._source.s.toUpperCase()", None, {"n": 1, "tags": ["a"], "obj": {"x": 1}, "s": "HI"}),
    ("ctx._source.tags.removeIf(t -> t == 'a')", None, {"n": 1, "tags": [], "obj": {"x": 1}, "s": "hi"}),
    ("ctx._source = ['replaced': true]", None, {"replaced": True}),
    (
        "ctx._source.m_id = ctx._id; ctx._source.m_ver = ctx._version; ctx._source.m_rt = ctx._routing",
        None,
        {"n": 1, "tags": ["a"], "obj": {"x": 1}, "s": "hi", "m_id": "1", "m_ver": 1, "m_rt": None},
    ),
    ("ctx._source.op_was = ctx.op", None, {"n": 1, "tags": ["a"], "obj": {"x": 1}, "s": "hi", "op_was": "index"}),
    ("ctx._id = 'other'; ctx._source.n = 7", None, {"n": 7, "tags": ["a"], "obj": {"x": 1}, "s": "hi"}),
    ("ctx._source.now = ctx._now > 0", None, {"n": 1, "tags": ["a"], "obj": {"x": 1}, "s": "hi", "now": True}),
    (
        "ctx._source.keys = ctx.keySet().stream().sorted().collect(Collectors.toList())",
        None,
        {
            "n": 1,
            "tags": ["a"],
            "obj": {"x": 1},
            "s": "hi",
            "keys": ["_id", "_index", "_now", "_routing", "_source", "_version", "op"],
        },
    ),
]


@pytest.mark.parametrize("source,params,expected", UPDATE_CASES)
def test_update_script_modifies_source(client, udoc, source, params, expected):
    script = {"source": source}
    if params:
        script["params"] = params
    r = _update(client, udoc, {"script": script})
    assert r.status_code == 200, r.text
    assert r.json()["result"] == "updated"
    assert r.json()["_version"] == 2
    assert _get_source(client, udoc) == expected


def test_update_script_as_plain_string(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.n *= 5"})
    assert r.status_code == 200
    assert _get_source(client, udoc)["n"] == 5


@pytest.mark.parametrize(
    "source",
    [
        "ctx.op = 'noop'",
        "ctx.op = 'none'",
        "ctx.op = 'bogus'",
        "if (ctx._source.n > 100) { ctx._source.big = true } else { ctx.op = 'noop' }",
    ],
)
def test_update_script_noop(client, udoc, source):
    r = _update(client, udoc, {"script": source})
    assert r.status_code == 200
    body = r.json()
    assert body["result"] == "noop"
    assert body["_version"] == 1
    assert _get_source(client, udoc) == {"n": 1, "tags": ["a"], "obj": {"x": 1}, "s": "hi"}


def test_update_script_same_value_is_still_updated(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.n = ctx._source.n"})
    assert r.json()["result"] == "updated"
    assert r.json()["_version"] == 2


def test_update_script_delete(client, udoc):
    r = _update(client, udoc, {"script": "ctx.op = 'delete'"})
    assert r.status_code == 200
    assert r.json()["result"] == "deleted"
    assert r.json()["_version"] == 2
    assert client.get(f"/{udoc}/_doc/1").status_code == 404


def test_update_script_runtime_error_shape(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.missing.x = 1"})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "failed to execute script"
    assert err["root_cause"] == [
        {"type": "illegal_argument_exception", "reason": "failed to execute script"}
    ]
    assert err["caused_by"]["type"] == "script_exception"
    assert err["caused_by"]["reason"] == "runtime error"
    assert err["caused_by"]["caused_by"]["type"] == "null_pointer_exception"
    assert _get_source(client, udoc)["n"] == 1


def test_update_script_compile_error_shape(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.n = 'x' + "})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "failed to execute script"
    assert err["caused_by"]["reason"] == "compile error"
    assert err["caused_by"]["caused_by"]["reason"] == "unexpected end of script."


def test_update_script_unknown_lang(client, udoc):
    r = _update(client, udoc, {"script": {"source": "ctx._source.n++", "lang": "nope"}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["reason"] == "failed to execute script"
    assert err["caused_by"] == {
        "type": "illegal_argument_exception",
        "reason": "script_lang not supported [nope]",
    }


def test_update_script_expression_lang_rejected(client, udoc):
    r = _update(client, udoc, {"script": {"source": "ctx._source.n++", "lang": "expression"}})
    assert r.status_code == 400
    cause = r.json()["error"]["caused_by"]
    assert cause["type"] == "script_exception"
    assert cause["lang"] == "expression"
    assert cause["caused_by"]["type"] == "parse_exception"


def test_update_script_and_doc_conflict(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.n += 1", "doc": {"x": 1}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == "Validation Failed: 1: can't provide both script and doc;"


def test_update_script_missing_doc(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.n = 1"}, doc_id="nope")
    assert r.status_code == 404
    assert r.json()["error"]["type"] == "document_missing_exception"
    assert r.json()["error"]["reason"] == "[nope]: document missing"


def test_update_upsert_skips_script_on_insert(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.n += 1", "upsert": {"n": 0}}, doc_id="u")
    assert r.status_code == 201
    assert r.json()["result"] == "created"
    assert _get_source(client, udoc, "u") == {"n": 0}
    r = _update(client, udoc, {"script": "ctx._source.n += 1", "upsert": {"n": 0}}, doc_id="u")
    assert r.status_code == 200
    assert r.json()["result"] == "updated"
    assert _get_source(client, udoc, "u") == {"n": 1}


def test_scripted_upsert_runs_script_on_insert(client, udoc):
    r = _update(
        client,
        udoc,
        {"script": {"source": "ctx._source.n = 5; ctx._source.op = ctx.op"}, "scripted_upsert": True, "upsert": {}},
        doc_id="su",
    )
    assert r.status_code == 201
    assert _get_source(client, udoc, "su") == {"n": 5, "op": "create"}


def test_scripted_upsert_noop_creates_nothing(client, udoc):
    r = _update(
        client,
        udoc,
        {"script": "ctx.op = 'noop'", "upsert": {"n": 0}, "scripted_upsert": True},
        doc_id="none",
    )
    assert r.status_code == 200
    assert r.json()["result"] == "noop"
    assert client.get(f"/{udoc}/_doc/none").status_code == 404


def test_update_script_with_source_return(client, udoc):
    r = _update(client, udoc, {"script": "ctx._source.n += 1"}, _source="true")
    assert r.status_code == 200
    got = r.json()["get"]
    assert got["found"] is True
    assert got["_source"] == {"n": 2, "tags": ["a"], "obj": {"x": 1}, "s": "hi"}


def test_update_with_stored_script(client, udoc, stored_script):
    sid, r = stored_script(
        {"script": {"lang": "painless", "source": "ctx._source.n += params.d"}}
    )
    assert r.status_code == 200
    r = _update(client, udoc, {"script": {"id": sid, "params": {"d": 41}}})
    assert r.status_code == 200
    assert _get_source(client, udoc)["n"] == 42


# ---- update_by_query -----------------------------------------------------------


@pytest.fixture
def qidx(plain_index):
    return plain_index([{"n": 1, "k": "a"}, {"n": 2, "k": "b"}, {"n": 3, "k": "c"}])


def _ubq(client, index, body):
    return client.post(f"/{index}/_update_by_query", json=body, params={"refresh": "true"})


def test_ubq_script_with_params(client, qidx):
    r = _ubq(client, qidx, {"script": {"source": "ctx._source.n *= params.f", "params": {"f": 10}}})
    assert r.status_code == 200
    b = r.json()
    assert (b["total"], b["updated"], b["noops"], b["deleted"]) == (3, 3, 0, 0)
    assert all_docs(client, qidx) == {
        "1": {"n": 10, "k": "a"},
        "2": {"n": 20, "k": "b"},
        "3": {"n": 30, "k": "c"},
    }


def test_ubq_script_with_query(client, qidx):
    r = _ubq(client, qidx, {"query": {"term": {"k.keyword": "a"}}, "script": "ctx._source.tag = 'A'"})
    assert r.json()["total"] == 1 and r.json()["updated"] == 1
    assert all_docs(client, qidx)["1"] == {"n": 1, "k": "a", "tag": "A"}
    assert all_docs(client, qidx)["2"] == {"n": 2, "k": "b"}


def test_ubq_script_noop_and_delete(client, qidx):
    r = _ubq(
        client,
        qidx,
        {
            "script": "if (ctx._source.n == 1) { ctx.op = 'noop' } "
            "else if (ctx._source.n == 2) { ctx.op = 'delete' } "
            "else { ctx._source.touched = true }"
        },
    )
    b = r.json()
    assert (b["total"], b["updated"], b["noops"], b["deleted"]) == (3, 1, 1, 1)
    assert all_docs(client, qidx) == {
        "1": {"n": 1, "k": "a"},
        "3": {"n": 3, "k": "c", "touched": True},
    }


def test_ubq_script_metadata(client, qidx):
    _ubq(client, qidx, {"script": "ctx._source.meta = ctx._id + '/' + ctx._index; ctx._source.v = ctx._version"})
    docs = all_docs(client, qidx)
    assert docs["2"]["meta"] == f"2/{qidx}"
    assert docs["2"]["v"] == -1


@pytest.mark.parametrize(
    "source,reason",
    [
        ("ctx.op = 'bogus'", "Operation type [bogus] not allowed, only [noop, index, delete] are allowed"),
        ("ctx._id = 'x'", "Modifying [_id] not allowed"),
        ("ctx._index = 'other'", "Modifying [_index] not allowed"),
    ],
)
def test_ubq_illegal_ctx_changes(client, qidx, source, reason):
    r = _ubq(client, qidx, {"script": source})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == reason
    assert all_docs(client, qidx)["1"] == {"n": 1, "k": "a"}


def test_ubq_script_runtime_error(client, qidx):
    r = _ubq(client, qidx, {"script": "ctx._source.n.foo()"})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "runtime error"
    assert err["caused_by"]["reason"] == "dynamic method [java.lang.Integer, foo/0] not found"


def test_ubq_script_compile_error(client, qidx):
    r = _ubq(client, qidx, {"script": "ctx._source.n = 1/0"})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "compile error"
    assert err["caused_by"] == {"type": "arithmetic_exception", "reason": "/ by zero"}


# ---- reindex ---------------------------------------------------------------------


def _reindex(client, plain_index, src, script):
    dest = f"slopensearch-scripting-{_uid()}"
    plain_index.track(dest)
    r = client.post(
        "/_reindex",
        json={"source": {"index": src}, "dest": {"index": dest}, "script": script},
        params={"refresh": "true"},
    )
    return dest, r


def test_reindex_script_modifies_source(client, plain_index, qidx):
    dest, r = _reindex(client, plain_index, qidx, {"source": "ctx._source.n += params.x", "params": {"x": 100}})
    assert r.status_code == 200
    assert r.json()["created"] == 3
    assert all_docs(client, dest) == {
        "1": {"n": 101, "k": "a"},
        "2": {"n": 102, "k": "b"},
        "3": {"n": 103, "k": "c"},
    }


def test_reindex_script_changes_id(client, plain_index, qidx):
    dest, r = _reindex(client, plain_index, qidx, {"source": "ctx._id = ctx._source.k"})
    assert r.status_code == 200
    assert sorted(all_docs(client, dest)) == ["a", "b", "c"]


def test_reindex_script_null_id_autogenerates(client, plain_index, qidx):
    dest, r = _reindex(client, plain_index, qidx, {"source": "ctx._id = null"})
    assert r.status_code == 200
    ids = list(all_docs(client, dest))
    assert len(ids) == 3
    assert not set(ids) & {"1", "2", "3"}


def test_reindex_script_noop(client, plain_index, qidx):
    dest, r = _reindex(client, plain_index, qidx, {"source": "if (ctx._source.n == 2) ctx.op = 'noop'"})
    b = r.json()
    assert (b["total"], b["created"], b["noops"]) == (3, 2, 1)
    assert sorted(all_docs(client, dest)) == ["1", "3"]


def test_reindex_script_delete(client, plain_index, qidx):
    dest, r = _reindex(client, plain_index, qidx, {"source": "if (ctx._source.n == 2) ctx.op = 'delete'"})
    b = r.json()
    assert (b["total"], b["created"], b["deleted"]) == (3, 2, 1)
    assert sorted(all_docs(client, dest)) == ["1", "3"]


def test_reindex_script_remove_field_and_read_index(client, plain_index, qidx):
    dest, r = _reindex(
        client, plain_index, qidx, {"source": "ctx._source.remove('k'); ctx._source.from = ctx._index"}
    )
    assert all_docs(client, dest)["1"] == {"n": 1, "from": qidx}


def test_reindex_script_routes_to_other_index(client, plain_index, qidx):
    other = f"slopensearch-scripting-{_uid()}"
    plain_index.track(other)
    dest, r = _reindex(
        client,
        plain_index,
        qidx,
        {"source": "if (ctx._source.n > 1) { ctx._index = params.o }", "params": {"o": other}},
    )
    assert r.status_code == 200
    assert r.json()["created"] == 3
    assert all_docs(client, dest) == {"1": {"n": 1, "k": "a"}}
    assert sorted(all_docs(client, other)) == ["2", "3"]


def test_reindex_script_version_and_routing(client, plain_index, qidx):
    dest, r = _reindex(
        client, plain_index, qidx, {"source": "ctx._source.ver = ctx._version; ctx._source.rt = ctx._routing"}
    )
    assert all_docs(client, dest)["1"] == {"n": 1, "k": "a", "ver": -1, "rt": None}


def test_reindex_script_bogus_op(client, plain_index, qidx):
    dest, r = _reindex(client, plain_index, qidx, {"source": "ctx.op = 'bogus'"})
    assert r.status_code == 400
    assert r.json()["error"]["reason"] == (
        "Operation type [bogus] not allowed, only [noop, index, delete] are allowed"
    )


# ---- ingest script processor --------------------------------------------------


def _simulate(client, processors, docs):
    return client.post(
        "/_ingest/pipeline/_simulate",
        json={
            "pipeline": {"processors": processors},
            "docs": [{"_index": "i", "_id": "1", "_source": d} for d in docs],
        },
    )


INGEST_CASES = [
    ({"source": "ctx.b = ctx.a * 2"}, {"a": 2}, {"a": 2, "b": 4}),
    ({"source": "ctx.b = ctx.a * params.f", "params": {"f": 3}}, {"a": 2}, {"a": 2, "b": 6}),
    ({"source": "ctx.remove('a')"}, {"a": 2, "c": 1}, {"c": 1}),
    (
        {"source": "ctx.l = ctx.s.toLowerCase(); ctx.n = ctx.s.length()"},
        {"s": "HeLLo"},
        {"s": "HeLLo", "l": "hello", "n": 5},
    ),
    (
        {"source": "ctx.keys = new ArrayList(ctx.keySet()); Collections.sort(ctx.keys)"},
        {"s": "x"},
        {"s": "x", "keys": ["_id", "_index", "s"]},
    ),
    ({"source": "ctx.x = ctx.missing.y", "ignore_failure": True}, {"a": 2}, {"a": 2}),
    ({"source": "ctx.tags = ['a', 'b']; ctx.tags.add('c')"}, {}, {"tags": ["a", "b", "c"]}),
]


@pytest.mark.parametrize("script,doc,expected", INGEST_CASES)
def test_ingest_script_processor(client, script, doc, expected):
    r = _simulate(client, [{"script": script}], [doc])
    assert r.status_code == 200, r.text
    assert r.json()["docs"][0]["doc"]["_source"] == expected


def test_ingest_script_changes_metadata(client):
    r = _simulate(client, [{"script": {"source": "ctx._id = 'new'; ctx._index = 'other'"}}], [{"a": 2}])
    doc = r.json()["docs"][0]["doc"]
    assert (doc["_id"], doc["_index"], doc["_source"]) == ("new", "other", {"a": 2})


def test_ingest_script_conditional_if(client):
    r = _simulate(client, [{"script": {"source": "ctx.x = 1", "if": "ctx.a > 1"}}], [{"a": 2}, {"a": 0}])
    assert [d["doc"]["_source"] for d in r.json()["docs"]] == [{"a": 2, "x": 1}, {"a": 0}]


@pytest.mark.parametrize(
    "source,cause_type,cause_reason",
    [
        (
            "ctx._source.b = 1",
            "null_pointer_exception",
            'Cannot invoke "Object.getClass()" because "callArgs[0]" is null',
        ),
        ("throw new IllegalArgumentException('nope ' + ctx.a)", "illegal_argument_exception", "nope 7"),
    ],
)
def test_ingest_script_runtime_error_per_doc(client, source, cause_type, cause_reason):
    r = _simulate(client, [{"script": {"source": source}}], [{"a": 7}])
    assert r.status_code == 200
    err = r.json()["docs"][0]["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "runtime error"
    assert err["caused_by"] == {"type": cause_type, "reason": cause_reason}


def test_ingest_script_on_failure_message(client):
    r = _simulate(
        client,
        [
            {
                "script": {
                    "source": "ctx.x = ctx.a / 0",
                    "on_failure": [{"set": {"field": "err", "value": "{{_ingest.on_failure_message}}"}}],
                }
            }
        ],
        [{"a": 7}],
    )
    assert r.json()["docs"][0]["doc"]["_source"] == {"a": 7, "err": "/ by zero"}


def test_ingest_script_compile_error_on_simulate(client):
    r = _simulate(client, [{"script": {"source": "ctx.x = 1 +", "tag": "t"}}], [{"a": 2}])
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "compile error"
    assert err["processor_type"] == "script"
    assert err["processor_tag"] == "t"


def test_ingest_script_compile_error_on_put_pipeline(client):
    pid = f"slopensearch-pipe-{_uid()}"
    r = client.put(f"/_ingest/pipeline/{pid}", json={"processors": [{"script": {"source": "ctx.x = 1 +"}}]})
    try:
        assert r.status_code == 400
        assert r.json()["error"]["type"] == "script_exception"
        assert r.json()["error"]["processor_type"] == "script"
        assert client.get(f"/_ingest/pipeline/{pid}").status_code == 404
    finally:
        client.delete(f"/_ingest/pipeline/{pid}")


def test_ingest_script_missing_stored_script(client):
    sid = f"slopensearch-nope-{_uid()}"
    r = _simulate(client, [{"script": {"id": sid}}], [{"a": 1}])
    assert r.status_code == 404
    err = r.json()["error"]
    assert err["type"] == "resource_not_found_exception"
    assert err["reason"] == f"unable to find script [{sid}] in cluster state"


def test_ingest_script_with_stored_script(client, stored_script):
    sid, r = stored_script({"script": {"lang": "painless", "source": "ctx.out = ctx.a + params.p"}})
    assert r.status_code == 200
    r = _simulate(client, [{"script": {"id": sid, "params": {"p": 10}}}], [{"a": 5}])
    assert r.json()["docs"][0]["doc"]["_source"] == {"a": 5, "out": 15}


def test_ingest_script_empty_on_failure_rejected(client):
    r = _simulate(client, [{"script": {"source": "ctx.x = 1", "on_failure": []}}], [{"a": 1}])
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "parse_exception"
    assert err["reason"] == "[on_failure] processors list cannot be empty"


def test_ingest_pipeline_script_applied_on_index(client, plain_index):
    pid = f"slopensearch-pipe-{_uid()}"
    r = client.put(
        f"/_ingest/pipeline/{pid}",
        json={"processors": [{"script": {"source": "ctx.total = ctx.price * ctx.qty"}}]},
    )
    assert r.status_code == 200
    try:
        idx = plain_index()
        r = client.put(
            f"/{idx}/_doc/1",
            json={"price": 3, "qty": 4},
            params={"pipeline": pid, "refresh": "true"},
        )
        assert r.status_code == 201
        assert _get_source(client, idx) == {"price": 3, "qty": 4, "total": 12}
    finally:
        client.delete(f"/_ingest/pipeline/{pid}")


# ==========================================================================
# 5. Stored scripts and listing APIs
# ==========================================================================


def test_stored_script_put_get_delete(client, stored_script):
    sid, r = stored_script({"script": {"lang": "painless", "source": "params.a * 2"}})
    assert r.status_code == 200
    assert r.json() == {"acknowledged": True}
    g = client.get(f"/_scripts/{sid}")
    assert g.status_code == 200
    assert g.json() == {
        "_id": sid,
        "found": True,
        "script": {"lang": "painless", "source": "params.a * 2"},
    }
    d = client.delete(f"/_scripts/{sid}")
    assert d.status_code == 200
    assert d.json() == {"acknowledged": True}
    g = client.get(f"/_scripts/{sid}")
    assert g.status_code == 404
    assert g.json() == {"_id": sid, "found": False}


def test_stored_script_delete_missing(client):
    sid = f"slopensearch-nope-{_uid()}"
    r = client.delete(f"/_scripts/{sid}")
    assert r.status_code == 404
    err = r.json()["error"]
    assert err["type"] == "resource_not_found_exception"
    assert err["reason"] == f"stored script [{sid}] does not exist and cannot be deleted"


def test_stored_script_overwrite_with_post(client, stored_script):
    sid, _ = stored_script({"script": {"lang": "painless", "source": "1"}})
    r = client.post(f"/_scripts/{sid}", json={"script": {"lang": "painless", "source": "2"}})
    assert r.status_code == 200
    assert client.get(f"/_scripts/{sid}").json()["script"]["source"] == "2"


def test_stored_script_used_in_script_fields(client, sidx, stored_script):
    sid, _ = stored_script({"script": {"lang": "painless", "source": "doc['price'].value * params.f"}})
    got = script_field_values(client, sidx, {"id": sid, "params": {"f": 3}})
    assert got == {"apple": [30], "banana": [60], "carrot": [15], "durian": [150]}


def test_stored_script_error_reports_id_as_script(client, sidx, stored_script):
    # compiled scripts are cached by source, so keep the source unique
    source = f"/* {_uid()} */ doc['price'].value * params.f"
    sid, _ = stored_script({"script": {"lang": "painless", "source": source}})
    r = search(client, sidx, {"script_fields": {"x": {"script": {"id": sid}}}})
    reason = shard_reason(r)
    assert reason["script"] == sid
    assert "doc['price'].value * params.f" in reason["script_stack"][0]
    assert reason["caused_by"]["type"] == "null_pointer_exception"


def test_stored_script_used_in_score_and_sort(client, sidx, stored_script):
    sid, _ = stored_script({"script": {"lang": "painless", "source": "doc['price'].value + params.b"}})
    r = search(
        client,
        sidx,
        {"query": {"script_score": {"query": {"match_all": {}}, "script": {"id": sid, "params": {"b": 1}}}}},
    )
    assert [h["_score"] for h in r.json()["hits"]["hits"]] == [51.0, 21.0, 11.0, 6.0]
    got = _sort(client, sidx, {"_script": {"type": "number", "script": {"id": sid, "params": {"b": 0}}}})
    assert [g[0] for g in got] == ["3", "1", "2", "4"]


def test_stored_expression_script(client, sidx, stored_script):
    sid, r = stored_script({"script": {"lang": "expression", "source": "doc['price'].value * 2"}})
    assert r.status_code == 200
    assert client.get(f"/_scripts/{sid}").json()["script"] == {
        "lang": "expression",
        "source": "doc['price'].value * 2",
    }
    got = script_field_values(client, sidx, {"id": sid})
    assert got["apple"] == [20.0]


def test_stored_script_missing_in_search(client, sidx):
    sid = f"slopensearch-nope-{_uid()}"
    r = search(client, sidx, {"script_fields": {"x": {"script": {"id": sid}}}})
    assert r.status_code == 404
    body = r.json()
    assert body["status"] == 404
    assert body["error"]["type"] == "search_phase_execution_exception"
    assert body["error"]["root_cause"][0] == {
        "type": "resource_not_found_exception",
        "reason": f"unable to find script [{sid}] in cluster state",
    }


def test_stored_script_missing_in_update(client, udoc):
    sid = f"slopensearch-nope-{_uid()}"
    r = _update(client, udoc, {"script": {"id": sid}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "failed to execute script"
    assert err["caused_by"] == {
        "type": "resource_not_found_exception",
        "reason": f"unable to find script [{sid}] in cluster state",
    }


def test_script_with_both_id_and_source(client, sidx):
    r = search(client, sidx, {"script_fields": {"x": {"script": {"id": "a", "source": "1"}}}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "x_content_parse_exception"
    assert err["caused_by"]["reason"] == "must only use one of [source, id] when specifying a script"


def test_stored_script_reference_with_lang_rejected(client, sidx):
    r = search(
        client, sidx, {"script_fields": {"x": {"script": {"id": "a", "lang": "painless"}}}}
    )
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == "illegally specified <lang> for a stored script"


@pytest.mark.parametrize(
    "body,reason",
    [
        ({"script": {"lang": "nolang", "source": "1"}}, "unable to put stored script with unsupported lang [nolang]"),
        ({"script": {"source": "1"}}, "must specify lang for stored script"),
        ({"script": {"lang": "painless"}}, "must specify source for stored script"),
        ({"script": {"lang": "painless", "source": ""}}, "source cannot be empty"),
        (
            {"script": {"lang": "painless", "source": "1", "options": {"a": "b"}}},
            "illegal compiler options [{a=b}] specified",
        ),
    ],
)
def test_stored_script_put_validation(client, stored_script, body, reason):
    sid, r = stored_script(body)
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == reason


def test_stored_script_put_string_script_rejected(client, stored_script):
    sid, r = stored_script({"script": "1"})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "parsing_exception"
    assert err["reason"] == "unexpected token [VALUE_STRING], expected [{, <source>]"


def test_stored_script_put_without_context_is_not_compiled(client, stored_script):
    # without a context in the URL the painless source is not compile-checked
    sid, r = stored_script({"script": {"lang": "painless", "source": "1 +"}})
    assert r.status_code == 200
    assert client.get(f"/_scripts/{sid}").json()["script"]["source"] == "1 +"


def test_stored_script_put_with_context_is_compiled(client, stored_script):
    sid, r = stored_script({"script": {"lang": "painless", "source": "'x'"}}, context="score")
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "compile error"
    assert err["caused_by"]["reason"] == "Cannot cast from [java.lang.String] to [double]."
    assert client.get(f"/_scripts/{sid}").status_code == 404


def test_stored_script_put_with_valid_context(client, stored_script):
    sid, r = stored_script({"script": {"lang": "painless", "source": "doc['price'].value"}}, context="score")
    assert r.status_code == 200
    assert client.get(f"/_scripts/{sid}").json()["found"] is True


def test_stored_script_put_unknown_context(client, stored_script):
    sid, r = stored_script({"script": {"lang": "painless", "source": "1"}}, context="nocontext")
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "illegal_argument_exception"
    assert err["reason"] == f"failed to parse/compile stored script [{sid}]"
    assert err["caused_by"]["reason"] == "Unknown context [nocontext]"


def test_stored_script_empty_body_becomes_empty_mustache(client, stored_script):
    sid, r = stored_script({})
    assert r.status_code == 200
    assert client.get(f"/_scripts/{sid}").json()["script"] == {"lang": "mustache", "source": ""}


def test_stored_mustache_object_source_gets_content_type(client, stored_script):
    sid, r = stored_script({"script": {"lang": "mustache", "source": {"query": {"match": {"title": "{{q}}"}}}}})
    assert r.status_code == 200
    assert client.get(f"/_scripts/{sid}").json()["script"] == {
        "lang": "mustache",
        "source": '{"query":{"match":{"title":"{{q}}"}}}',
        "options": {"content_type": "application/json; charset=UTF-8"},
    }


def test_stored_mustache_string_source_has_no_options(client, stored_script):
    src = '{"query":{"match":{"title":"{{q}}"}}}'
    sid, r = stored_script({"script": {"lang": "mustache", "source": src}})
    assert r.status_code == 200
    assert client.get(f"/_scripts/{sid}").json()["script"] == {"lang": "mustache", "source": src}


def test_script_context_listing(client):
    r = client.get("/_script_context")
    assert r.status_code == 200
    contexts = {c["name"]: c for c in r.json()["contexts"]}
    for name in (
        "aggs",
        "field",
        "filter",
        "ingest",
        "number_sort",
        "painless_test",
        "score",
        "string_sort",
        "template",
        "update",
        "bucket_aggregation",
        "aggregation_selector",
    ):
        assert name in contexts
    names = [c["name"] for c in r.json()["contexts"]]
    assert names == sorted(names)
    assert contexts["filter"]["methods"][0] == {
        "name": "execute",
        "return_type": "boolean",
        "params": [],
    }
    assert contexts["ingest"]["methods"][0] == {
        "name": "execute",
        "return_type": "void",
        "params": [{"type": "java.util.Map", "name": "ctx"}],
    }
    assert contexts["template"]["methods"][0]["return_type"] == "java.lang.String"
    assert {"name": "get_score", "return_type": "double", "params": []} in contexts["score"]["methods"]


def test_script_language_listing(client):
    r = client.get("/_script_language")
    assert r.status_code == 200
    body = r.json()
    assert body["types_allowed"] == ["inline", "stored"]
    langs = {x["language"]: x["contexts"] for x in body["language_contexts"]}
    # knn, ranklib and opensearch_compounded_script come from full-distribution plugins
    assert set(langs) == {"expression", "mustache", "painless", "knn", "ranklib", "opensearch_compounded_script"}
    assert langs["knn"] == ["score"]
    assert langs["ranklib"] == ["ranklib"]
    assert langs["opensearch_compounded_script"] == ["aggs", "filter"]
    assert langs["mustache"] == ["template"]
    assert langs["expression"] == [
        "aggregation_selector",
        "aggs",
        "bucket_aggregation",
        "field",
        "filter",
        "number_sort",
        "score",
        "terms_set",
    ]
    assert "painless_test" in langs["painless"]
    assert "update" in langs["painless"]
    assert langs["painless"] == sorted(langs["painless"])


# ==========================================================================
# 6. Mustache templates
# ==========================================================================


def render(client, source=None, params=None, sid=None):
    body = {}
    if source is not None:
        body["source"] = source
    if sid is not None:
        body["id"] = sid
    if params is not None:
        body["params"] = params
    return client.post("/_render/template", json=body)


RENDER_TEXT = [
    ('{"x":"{{q}}"}', {"q": "hello"}, '{"x":"hello"}'),
    ('{"x":"{{q}}"}', {}, '{"x":""}'),
    ('{"x":"{{q}}"}', {"q": 'a"b<c>&\\d'}, '{"x":"a\\"b<c>&\\\\d"}'),
    ('{"x":"{{{q}}}"}', {"q": "a<b>"}, '{"x":"a<b>"}'),
    ('{"x":{{n}}}', {"n": 5}, '{"x":5}'),
    ('{"x":{{n}}}', {"n": 5.5}, '{"x":5.5}'),
    ('{"x":{{n}}}', {"n": True}, '{"x":true}'),
    ('{"x":"{{n}}"}', {"n": None}, '{"x":""}'),
    ('{"x":"{{l}}"}', {"l": [1, 2]}, '{"x":"{0=1, 1=2}"}'),
    ('{"x":"{{m}}"}', {"m": {"a": 1}}, '{"x":"{a=1}"}'),
    ('{"x":"{{m.a}}"}', {"m": {"a": 1}}, '{"x":"1"}'),
    ('{"x":"{{a.b.c}}"}', {"a": {"b": {"c": "deep"}}}, '{"x":"deep"}'),
    ('{"x":"{{l.0}}"}', {"l": ["first", "second"]}, '{"x":"first"}'),
    ('{"t":{{#toJson}}tags{{/toJson}}}', {"tags": ["a", "b"]}, '{"t":["a","b"]}'),
    ('{"t":{{#toJson}}m{{/toJson}}}', {"m": {"x": 1}}, '{"t":{"x":1}}'),
    ('{"t":{{#toJson}}l{{/toJson}}}', {"l": [1, 2.5, True, None, "s"]}, '{"t":[1,2.5,true,null,"s"]}'),
    ('{"x":"{{#toJson}}m{{/toJson}}"}', {"m": {"a": "b"}}, '{"x":"{"a":"b"}"}'),
    ('{"x": {{#toJson}}missing{{/toJson}}}', {}, '{"x": }'),
    ('{"x":"{{#join}}tags{{/join}}"}', {"tags": ["a", "b", "c"]}, '{"x":"a,b,c"}'),
    ('{"x":"{{#join}}l{{/join}}"}', {"l": [1, 2.5, True]}, '{"x":"1,2.5,true"}'),
    (
        "{\"x\":\"{{#join delimiter='|'}}tags{{/join delimiter='|'}}\"}",
        {"tags": ["a", "b", "c"]},
        '{"x":"a|b|c"}',
    ),
    ('{"x":"{{#url}}{{q}}{{/url}}"}', {"q": "a b&c=d/é"}, '{"x":"a+b%26c%3Dd%2F%C3%A9"}'),
    ('{"x":"{{#url}}{{l}}{{/url}}"}', {"l": ["a b", "c"]}, '{"x":"%7B0%3Da+b%2C+1%3Dc%7D"}'),
    (
        '{"from":{{from}}{{^from}}0{{/from}},"size":{{size}}{{^size}}10{{/size}}}',
        {"size": 3},
        '{"from":0,"size":3}',
    ),
    ('{"q":"x"{{#f}},"filter":"{{f}}"{{/f}}}', {"f": "yes"}, '{"q":"x","filter":"yes"}'),
    ('{"q":"x"{{#f}},"filter":"{{f}}"{{/f}}}', {}, '{"q":"x"}'),
    ('{"q":"x"{{#f}},"filter":"y"{{/f}}}', {"f": False}, '{"q":"x"}'),
    ('{"q":"x"{{#f}},"filter":"y"{{/f}}}', {"f": []}, '{"q":"x"}'),
    ('{"q":"x"{{#f}},"filter":"y"{{/f}}}', {"f": ""}, '{"q":"x"}'),
    # 0 is truthy in mustache.java
    ('{"q":"x"{{#f}},"filter":"y"{{/f}}}', {"f": 0}, '{"q":"x","filter":"y"}'),
    ('{"items":[{{#l}}"{{.}}",{{/l}}"end"]}', {"l": ["a", "b"]}, '{"items":["a","b","end"]}'),
    (
        '{"items":[{{#l}}{"n":"{{name}}"},{{/l}}{}]}',
        {"l": [{"name": "a"}, {"name": "b"}]},
        '{"items":[{"n":"a"},{"n":"b"},{}]}',
    ),
    # -last is not supported by mustache.java
    ('{"x":"{{#l}}{{.}}{{^-last}},{{/-last}}{{/l}}"}', {"l": ["a", "b"]}, '{"x":"a,b,"}'),
    ('{"x":"{{#a}}{{b}}{{/a}}"}', {"a": {"b": "inner"}}, '{"x":"inner"}'),
    ('{"x":"{{#a}}{{b}}{{/a}}"}', {"a": {"c": "inner"}, "b": "outer"}, '{"x":"outer"}'),
    ('{"x":"{{! comment }}y"}', {}, '{"x":"y"}'),
    ('{"x":"{{=<% %>=}}<%q%>"}', {"q": "swapped"}, '{"x":"swapped"}'),
    ('{"x":"{{#upper}}a{{/upper}}"}', {}, '{"x":""}'),
    ('{"size": {{s}} }', {"s": "abc"}, '{"size": abc }'),
    ("not json {{q}}", {"q": 1}, "not json 1"),
]


@pytest.mark.parametrize("source,params,expected", RENDER_TEXT)
def test_render_template_string_source(client, source, params, expected):
    r = render(client, source, params)
    assert r.status_code == 200, r.text
    assert r.text == '{"template_output":' + expected + "}"


@pytest.mark.parametrize(
    "source,params,expected",
    [
        ({"query": {"match": {"title": "{{q}}"}}}, {"q": "hello"}, '{"query":{"match":{"title":"hello"}}}'),
        ({"query": {"match": {"title": "{{q}}"}}}, None, '{"query":{"match":{"title":""}}}'),
        ({"size": "{{s}}"}, {"s": 2}, '{"size":"2"}'),
        ({"x": "{{#toJson}}m{{/toJson}}"}, {"m": {"a": "b"}}, '{"x":"{"a":"b"}"}'),
    ],
)
def test_render_template_object_source(client, source, params, expected):
    r = render(client, source, params)
    assert r.status_code == 200, r.text
    assert r.text == '{"template_output":' + expected + "}"


def test_render_template_unclosed_section(client):
    src = '{"x":"{{#unclosed}}"}'
    r = render(client, src, {"q": 1})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "Failed to close 'unclosed' tag @[query-template:1]"
    assert err["lang"] == "mustache"
    assert err["script"] == src
    assert err["script_stack"] == []
    assert err["caused_by"]["type"] == "mustache_exception"


def test_render_template_missing_template(client):
    r = client.post("/_render/template", json={"params": {}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "action_request_validation_exception"
    assert err["reason"] == (
        "Validation Failed: 1: template is missing;2: template's script type is missing;"
    )


def test_render_stored_template_by_path_and_body(client, stored_script):
    sid, r = stored_script(
        {
            "script": {
                "lang": "mustache",
                "source": {"query": {"range": {"price": {"gte": "{{min}}"}}}, "sort": [{"price": "asc"}]},
            }
        }
    )
    assert r.status_code == 200
    expected = '{"template_output":{"query":{"range":{"price":{"gte":"7"}}},"sort":[{"price":"asc"}]}}'
    r1 = client.post(f"/_render/template/{sid}", json={"params": {"min": 7}})
    assert r1.status_code == 200 and r1.text == expected
    r2 = render(client, sid=sid, params={"min": 7})
    assert r2.status_code == 200 and r2.text == expected


def test_render_missing_stored_template(client):
    sid = f"slopensearch-nope-{_uid()}"
    r = client.post(f"/_render/template/{sid}", json={"params": {}})
    assert r.status_code == 404
    assert r.json()["error"] == {
        "root_cause": [
            {
                "type": "resource_not_found_exception",
                "reason": f"unable to find script [{sid}] in cluster state",
            }
        ],
        "type": "resource_not_found_exception",
        "reason": f"unable to find script [{sid}] in cluster state",
    }


def test_render_stored_painless_script_as_template_fails(client, stored_script):
    sid, _ = stored_script({"script": {"lang": "painless", "source": "1"}})
    r = client.post(f"/_render/template/{sid}", json={"params": {}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "script_exception"
    assert err["reason"] == "compile error"
    assert err["caused_by"]["reason"] == "Cannot cast from [int] to [java.lang.String]."


# ---- _search/template -------------------------------------------------------------


def _st(client, index, body, **params):
    return client.post(f"/{index}/_search/template", json=body, params=params)


def _ids(resp):
    assert resp.status_code == 200, resp.text
    return [h["_id"] for h in resp.json()["hits"]["hits"]]


def test_search_template_inline_object(client, sidx):
    r = _st(
        client,
        sidx,
        {"source": {"query": {"match": {"title": "{{q}}"}}, "sort": ["name"]}, "params": {"q": "fruit"}},
    )
    assert _ids(r) == ["4"]
    assert r.json()["status"] == 200


def test_search_template_inline_string_with_tojson_and_default(client, sidx):
    r = _st(
        client,
        sidx,
        {
            "source": '{"query":{"terms":{"name":{{#toJson}}names{{/toJson}}}},'
            '"sort":["name"],"size":{{size}}{{^size}}10{{/size}}}',
            "params": {"names": ["apple", "carrot", "durian"], "size": 2},
        },
    )
    assert _ids(r) == ["1", "3"]
    assert r.json()["hits"]["total"]["value"] == 3


def test_search_template_string_param_coerced_to_size(client, sidx):
    r = _st(client, sidx, {"source": {"query": {"match_all": {}}, "size": "{{s}}"}, "params": {"s": 1}})
    assert len(_ids(r)) == 1


def test_search_template_bad_size_value(client, sidx):
    r = _st(client, sidx, {"source": {"query": {"match_all": {}}, "size": "{{s}}"}, "params": {"s": "x"}})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "number_format_exception"
    assert r.json()["error"]["reason"] == 'For input string: "x"'


def test_search_template_renders_invalid_json(client, sidx):
    r = _st(client, sidx, {"source": '{"query":{"match_all":{}},"size":{{s}}}', "params": {}})
    assert r.status_code == 400
    assert r.json()["error"]["type"] == "json_parse_exception"


def test_search_template_unknown_query(client, sidx):
    r = _st(client, sidx, {"source": {"query": {"bogus_query": {}}}, "params": {}})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "parsing_exception"
    assert err["reason"] == "unknown query [bogus_query]"


def test_search_template_without_params(client, sidx):
    r = _st(client, sidx, {"source": {"query": {"match_all": {}}}})
    assert _ids(r) == ["1", "2", "3", "4"]


def test_search_template_legacy_inline_key(client, sidx):
    r = _st(client, sidx, {"inline": {"query": {"term": {"name": "banana"}}}})
    assert _ids(r) == ["2"]


def test_search_template_unknown_field(client, sidx):
    r = _st(client, sidx, {"source": {"query": {"match_all": {}}}, "bogus": 1})
    assert r.status_code == 400
    err = r.json()["error"]
    assert err["type"] == "x_content_parse_exception"
    assert err["reason"] == "[1:38] [search_template] unknown field [bogus]"


def test_search_template_stored(client, sidx, stored_script):
    sid, _ = stored_script(
        {
            "script": {
                "lang": "mustache",
                "source": {"query": {"range": {"price": {"gte": "{{min}}"}}}, "sort": [{"price": "asc"}]},
            }
        }
    )
    assert _ids(_st(client, sidx, {"id": sid, "params": {"min": 20}})) == ["2", "4"]
    assert _ids(_st(client, sidx, {"id": sid, "params": {"min": 0}})) == ["3", "1", "2", "4"]


def test_search_template_source_wins_over_id(client, sidx, stored_script):
    sid, _ = stored_script(
        {"script": {"lang": "mustache", "source": {"query": {"term": {"name": "apple"}}}}}
    )
    r = _st(client, sidx, {"id": sid, "source": "{}", "params": {}})
    assert r.json()["hits"]["total"]["value"] == 4


def test_search_template_missing_stored(client, sidx):
    sid = f"slopensearch-nope-{_uid()}"
    r = _st(client, sidx, {"id": sid, "params": {}})
    assert r.status_code == 404
    assert r.json()["error"]["type"] == "resource_not_found_exception"


def test_search_template_rest_total_hits_as_int(client, sidx):
    r = _st(client, sidx, {"source": {"query": {"match_all": {}}}}, rest_total_hits_as_int="true")
    assert r.json()["hits"]["total"] == 4


def test_search_template_typed_keys_aggs(client, sidx):
    r = _st(
        client,
        sidx,
        {
            "source": {"size": 0, "aggs": {"a": {"terms": {"field": "{{f}}"}}}},
            "params": {"f": "name"},
        },
        typed_keys="true",
    )
    aggs = r.json()["aggregations"]
    assert list(aggs) == ["sterms#a"]
    assert len(aggs["sterms#a"]["buckets"]) == 4


def test_search_template_explain(client, sidx):
    r = _st(
        client,
        sidx,
        {"source": {"query": {"match": {"title": "{{q}}"}}}, "params": {"q": "fruit"}, "explain": True},
    )
    hit = r.json()["hits"]["hits"][0]
    assert hit["_id"] == "4"
    assert "_explanation" in hit


def test_msearch_template(client, sidx):
    sid = f"slopensearch-nope-{_uid()}"
    payload = _ndjson(
        [
            {"index": sidx},
            {"source": {"query": {"term": {"name": "{{n}}"}}}, "params": {"n": "apple"}},
            {"index": sidx},
            {"id": sid, "params": {}},
            {"index": sidx},
            {"source": '{"query":{"match_all":{}},"size":{{s}}}', "params": {"s": 0}},
        ]
    )
    r = client.post(
        "/_msearch/template", content=payload, headers={"Content-Type": "application/x-ndjson"}
    )
    assert r.status_code == 200
    resp = r.json()["responses"]
    assert resp[0]["status"] == 200
    assert [h["_id"] for h in resp[0]["hits"]["hits"]] == ["1"]
    assert resp[1]["status"] == 404
    assert resp[1]["error"]["type"] == "resource_not_found_exception"
    assert resp[2]["status"] == 200
    assert resp[2]["hits"]["total"]["value"] == 4
    assert resp[2]["hits"]["hits"] == []


def test_msearch_template_index_in_path(client, sidx):
    payload = _ndjson([{}, {"source": {"query": {"term": {"name": "{{n}}"}}}, "params": {"n": "carrot"}}])
    r = client.post(
        f"/{sidx}/_msearch/template", content=payload, headers={"Content-Type": "application/x-ndjson"}
    )
    assert r.status_code == 200
    assert [h["_id"] for h in r.json()["responses"][0]["hits"]["hits"]] == ["3"]
