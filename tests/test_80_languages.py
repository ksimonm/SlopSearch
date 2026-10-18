import json
import unicodedata
from urllib.parse import quote

import pytest

pytestmark = pytest.mark.i18n

LANGUAGES = [
    ("uk", "Привіт Світ", ["привіт", "світ"]),
    ("el", "Καλημέρα Κόσμε", ["καλημέρα", "κόσμε"]),
    ("fr", "Café Crème", ["café", "crème"]),
    ("de", "Straße Größe", ["straße", "größe"]),
    ("ar", "مرحبا بالعالم", ["مرحبا", "بالعالم"]),
    ("zh", "你好世界", ["你", "好", "世", "界"]),
    ("ja", "こんにちは世界", ["こ", "ん", "に", "ち", "は", "世", "界"]),
    ("ko", "안녕하세요 세계", ["안녕하세요", "세계"]),
]
IDS = [case[0] for case in LANGUAGES]


def _total(client, index, body):
    r = client.post(f"/{index}/_search", json=body)
    assert r.status_code == 200, r.text
    return r.json()["hits"]["total"]["value"]


@pytest.mark.parametrize("lang,text,tokens", LANGUAGES, ids=IDS)
def test_analyze_standard_multilingual(client, index_factory, lang, text, tokens):
    name = index_factory()
    r = client.post(f"/{name}/_analyze", json={"analyzer": "standard", "text": text})
    assert r.status_code == 200, r.text
    assert [t["token"] for t in r.json()["tokens"]] == tokens


@pytest.mark.parametrize("lang,text,tokens", LANGUAGES, ids=IDS)
def test_match_and_term_multilingual(client, index_factory, lang, text, tokens):
    name = index_factory(
        mappings={"properties": {"body": {"type": "text"}, "tag": {"type": "keyword"}}}
    )
    client.put(f"/{name}/_doc/1", json={"body": text, "tag": text}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": tokens[0]}}}) == 1
    assert _total(client, name, {"query": {"term": {"tag": text}}}) == 1


@pytest.mark.parametrize("lang,text,tokens", LANGUAGES, ids=IDS)
def test_match_is_case_folded(client, index_factory, lang, text, tokens):
    name = index_factory(mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": text}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": text.lower()}}}) == 1


def test_latin_uppercase_folds_to_lowercase(client, index_factory):
    name = index_factory(mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": "Café Crème"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": "CAFÉ CRÈME"}}}) == 1


def test_cyrillic_text_case_insensitive_match(client, index_factory):
    name = index_factory(mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": "Привіт Світ"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": "ПРИВІТ"}}}) == 1
    assert _total(client, name, {"query": {"match": {"body": "світ"}}}) == 1


def test_cyrillic_keyword_is_case_sensitive(client, index_factory):
    name = index_factory(mappings={"properties": {"tag": {"type": "keyword"}}})
    client.put(f"/{name}/_doc/1", json={"tag": "Київ"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"term": {"tag": "Київ"}}}) == 1
    assert _total(client, name, {"query": {"term": {"tag": "київ"}}}) == 0


def test_cjk_substring_match(client, index_factory):
    name = index_factory(mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": "你好世界"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": "世界"}}}) == 1
    assert _total(client, name, {"query": {"match": {"body": "宇宙"}}}) == 0


def test_multilingual_terms_agg(client, index_factory, bulk_index):
    name = index_factory(mappings={"properties": {"tag": {"type": "keyword"}}})
    bulk_index(name, [{"tag": "Київ"}, {"tag": "Київ"}, {"tag": "Львів"}])
    r = client.post(f"/{name}/_search", json={"size": 0, "aggs": {"t": {"terms": {"field": "tag"}}}})
    buckets = {b["key"]: b["doc_count"] for b in r.json()["aggregations"]["t"]["buckets"]}
    assert buckets == {"Київ": 2, "Львів": 1}


def test_multilingual_highlight(client, index_factory):
    name = index_factory(mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": "Привіт Світ"}, params={"refresh": "true"})
    r = client.post(
        f"/{name}/_search",
        json={"query": {"match": {"body": "світ"}}, "highlight": {"fields": {"body": {}}}},
    )
    hits = r.json()["hits"]["hits"]
    assert hits and "body" in hits[0]["highlight"]


# ---------------------------------------------------------------------------
# Helpers for the extended language suite
# ---------------------------------------------------------------------------



def _analyze(client, **body):
    r = client.post("/_analyze", json=body)
    assert r.status_code == 200, r.text
    return [t["token"] for t in r.json()["tokens"]]


def _analyze_full(client, **body):
    r = client.post("/_analyze", json=body)
    assert r.status_code == 200, r.text
    return r.json()["tokens"]


def _ids(client, index, body):
    r = client.post(f"/{index}/_search", json=body)
    assert r.status_code == 200, r.text
    return sorted(h["_id"] for h in r.json()["hits"]["hits"])


SINGLE_SHARD = {"number_of_shards": 1, "number_of_replicas": 0}


# ---------------------------------------------------------------------------
# Standard analyzer on many scripts: exact tokens and token types
# ---------------------------------------------------------------------------

STANDARD_SCRIPTS = [
    ('ru', 'Съешь же ещё этих мягких булок', ['съешь', 'же', 'ещё', 'этих', 'мягких', 'булок'], ['<ALPHANUM>']),
    ('be', 'Беларусь і Мінск', ['беларусь', 'і', 'мінск'], ['<ALPHANUM>']),
    ('pl', 'Zażółć gęślą jaźń', ['zażółć', 'gęślą', 'jaźń'], ['<ALPHANUM>']),
    ('cs', 'Příliš žluťoučký kůň', ['příliš', 'žluťoučký', 'kůň'], ['<ALPHANUM>']),
    ('sk', 'Ďateľ a ľaliový kôň', ['ďateľ', 'a', 'ľaliový', 'kôň'], ['<ALPHANUM>']),
    ('bg', 'Здравей свят', ['здравей', 'свят'], ['<ALPHANUM>']),
    ('sr_cyrl', 'Ђорђе и Љубица', ['ђорђе', 'и', 'љубица'], ['<ALPHANUM>']),
    ('sr_latn', 'Đorđe i Ljubica', ['đorđe', 'i', 'ljubica'], ['<ALPHANUM>']),
    ('hr', 'Čovjek šećer', ['čovjek', 'šećer'], ['<ALPHANUM>']),
    ('ro', 'Țară și învățătură', ['țară', 'și', 'învățătură'], ['<ALPHANUM>']),
    ('hu', 'Árvíztűrő tükörfúrógép', ['árvíztűrő', 'tükörfúrógép'], ['<ALPHANUM>']),
    ('fi', 'Hyvää päivää', ['hyvää', 'päivää'], ['<ALPHANUM>']),
    ('et', 'Tere õhtust', ['tere', 'õhtust'], ['<ALPHANUM>']),
    ('lv', 'Ķēniņš ģērbjas', ['ķēniņš', 'ģērbjas'], ['<ALPHANUM>']),
    ('lt', 'Ąžuolas ėjo į mišką', ['ąžuolas', 'ėjo', 'į', 'mišką'], ['<ALPHANUM>']),
    ('sv', 'Smörgåsbord är gott', ['smörgåsbord', 'är', 'gott'], ['<ALPHANUM>']),
    ('no', 'Blåbærsyltetøy', ['blåbærsyltetøy'], ['<ALPHANUM>']),
    ('da', 'Ærø og Øresund', ['ærø', 'og', 'øresund'], ['<ALPHANUM>']),
    ('is', 'Þjóðin hefur ð og þ', ['þjóðin', 'hefur', 'ð', 'og', 'þ'], ['<ALPHANUM>']),
    ('nl', 'IJsselmeer en ĳs', ['ijsselmeer', 'en', 'ĳs'], ['<ALPHANUM>']),
    ('es', '¿Qué tal, señor?', ['qué', 'tal', 'señor'], ['<ALPHANUM>']),
    ('pt', 'Ação São João', ['ação', 'são', 'joão'], ['<ALPHANUM>']),
    ('it', 'Perché così', ['perché', 'così'], ['<ALPHANUM>']),
    ('ca', 'Col·legi l·l', ['col·legi', 'l·l'], ['<ALPHANUM>']),
    ('eu', 'Euskal Herria', ['euskal', 'herria'], ['<ALPHANUM>']),
    ('gl', 'Galiza é nosa', ['galiza', 'é', 'nosa'], ['<ALPHANUM>']),
    ('ga', 'Dia dhuit a Sheáin', ['dia', 'dhuit', 'a', 'sheáin'], ['<ALPHANUM>']),
    ('tr', 'İstanbul Işık ılık', ['istanbul', 'işık', 'ılık'], ['<ALPHANUM>']),
    ('az', 'Azərbaycan dili', ['azərbaycan', 'dili'], ['<ALPHANUM>']),
    ('kk', 'Қазақстан Республикасы', ['қазақстан', 'республикасы'], ['<ALPHANUM>']),
    ('ka', 'გამარჯობა მსოფლიო', ['გამარჯობა', 'მსოფლიო'], ['<ALPHANUM>']),
    ('hy', 'Բարեւ աշխարհ', ['բարեւ', 'աշխարհ'], ['<ALPHANUM>']),
    ('he', 'שלום עולם', ['שלום', 'עולם'], ['<ALPHANUM>']),
    ('fa', 'سلام دنیا', ['سلام', 'دنیا'], ['<ALPHANUM>']),
    ('ur', 'ہیلو دنیا', ['ہیلو', 'دنیا'], ['<ALPHANUM>']),
    ('hi', 'नमस्ते दुनिया', ['नमस्ते', 'दुनिया'], ['<ALPHANUM>']),
    ('bn', 'হ্যালো বিশ্ব', ['হ্যালো', 'বিশ্ব'], ['<ALPHANUM>']),
    ('ta', 'வணக்கம் உலகம்', ['வணக்கம்', 'உலகம்'], ['<ALPHANUM>']),
    ('th', 'สวัสดีชาวโลก', ['สวัสดีชาวโลก'], ['<SOUTHEAST_ASIAN>']),
    ('lo', 'ສະບາຍດີ', ['ສະບາຍດີ'], ['<SOUTHEAST_ASIAN>']),
    ('km', 'សួស្តីពិភពលោក', ['សួស្តីពិភពលោក'], ['<SOUTHEAST_ASIAN>']),
    ('my', 'မင်္ဂလာပါ', ['မင်္ဂလာပါ'], ['<SOUTHEAST_ASIAN>']),
    ('vi', 'Tiếng Việt có dấu', ['tiếng', 'việt', 'có', 'dấu'], ['<ALPHANUM>']),
    ('id', 'Selamat pagi dunia', ['selamat', 'pagi', 'dunia'], ['<ALPHANUM>']),
    ('ms', 'Selamat datang', ['selamat', 'datang'], ['<ALPHANUM>']),
    ('tl', 'Kumusta ka mundo', ['kumusta', 'ka', 'mundo'], ['<ALPHANUM>']),
    ('sw', 'Habari ya dunia', ['habari', 'ya', 'dunia'], ['<ALPHANUM>']),
    ('am', 'ሰላም ዓለም', ['ሰላም', 'ዓለም'], ['<ALPHANUM>']),
    ('mn', 'Сайн байна уу', ['сайн', 'байна', 'уу'], ['<ALPHANUM>']),
    ('zh_hant', '繁體中文測試', ['繁', '體', '中', '文', '測', '試'], ['<IDEOGRAPHIC>']),
    ('zh_hans', '简体中文测试', ['简', '体', '中', '文', '测', '试'], ['<IDEOGRAPHIC>']),
    ('ja_mixed', '東京タワーへ行きますTokyo', ['東', '京', 'タワー', 'へ', '行', 'き', 'ま', 'す', 'tokyo'], ['<ALPHANUM>', '<HIRAGANA>', '<IDEOGRAPHIC>', '<KATAKANA>']),
    ('ko', '대한민국 서울', ['대한민국', '서울'], ['<HANGUL>']),
    ('emoji', 'I ❤️ pizza 🍕 👨\u200d👩\u200d👧', ['i', '❤️', 'pizza', '🍕', '👨\u200d👩\u200d👧'], ['<ALPHANUM>', '<EMOJI>']),
    ('mixed_rtl', 'Hello שלום world', ['hello', 'שלום', 'world'], ['<ALPHANUM>']),
    ('fullwidth', 'ＡＢＣ１２３', ['ａｂｃ１２３'], ['<ALPHANUM>']),
    ('lig', 'ﬁnance ﬂow', ['ﬁnance', 'ﬂow'], ['<ALPHANUM>']),
    ('de_ss', 'STRASSE Straße ẞ', ['strasse', 'straße', 'ß'], ['<ALPHANUM>']),
    ('el_sigma', 'ΟΔΟΣ οδός', ['οδοσ', 'οδός'], ['<ALPHANUM>']),
    ('ar_digits', '٤٥٦ ۱۲۳', ['٤٥٦', '۱۲۳'], ['<NUM>']),
    ('cjk_extb', '𠀀𠀁 𪚥', ['𠀀', '𠀁', '𪚥'], ['<IDEOGRAPHIC>']),
    ('hangul_jamo', '한국어', ['한국어'], ['<HANGUL>']),
    ('katakana_hw', 'ｶﾀｶﾅ', ['ｶﾀｶﾅ'], ['<KATAKANA>']),
]


@pytest.mark.parametrize(
    "lang,text,tokens,types", STANDARD_SCRIPTS, ids=[c[0] for c in STANDARD_SCRIPTS]
)
def test_standard_analyzer_scripts(client, lang, text, tokens, types):
    out = _analyze_full(client, analyzer="standard", text=text)
    assert [t["token"] for t in out] == tokens
    assert sorted({t["type"] for t in out}) == types


# ---------------------------------------------------------------------------
# Built-in language analyzers: stemming + stopword removal (exact tokens)
# ---------------------------------------------------------------------------

LANGUAGE_ANALYZERS = [
    ("arabic", "الكتاب في المكتبات والمدرسة", ["كتاب", "مكتب", "مدرس"]),
    ("armenian", "Քաղաքներ և սեղանի վրա են", ["քաղ", "սեղ"]),
    ("basque", "Etxeak eta mendian ederrak dira", ["etxe", "mendi", "ederr"]),
    ("bengali", "আমি বইগুলো এবং কলমগুলো পড়ি", ["আম", "বই", "এবং", "কলম", "পর"]),
    ("brazilian", "Os meninos e as meninas correram", ["menin", "menin", "corr"]),
    ("bulgarian", "Книгите и на масата са нови", ["книг", "мас", "нови"]),
    ("catalan", "Els gats i les cases de l'home", ["gat", "case", "hom"]),
    ("cjk", "我们的世界和平 and the ＡＢＣ", ["我们", "们的", "的世", "世界", "界和", "和平", "abc"]),
    ("czech", "Kočky a psi jsou v zahradách", ["kočk", "psi", "zahrad"]),
    ("danish", "Bøgerne og hundene er på bordet", ["bøg", "hund", "bord"]),
    ("dutch", "De katten en de honden lopen", ["kat", "hond", "lop"]),
    ("english", "The running dogs and the cats", ["run", "dog", "cat"]),
    ("estonian", "Raamatud ja koerad on laual", ["raama", "koera", "on", "laual"]),
    ("finnish", "Kirjat ja koirat ovat talossa", ["kirj", "koira", "talo"]),
    ("french", "Les chevaux et l'avion des étudiants", ["cheval", "avion", "etudiant"]),
    ("galician", "Os nenos e as casas grandes", ["nen", "cas", "grand"]),
    ("german", "Die Häuser und der Bäume sind schön", ["haus", "baum", "schon"]),
    ("greek", "Τα σπίτια και οι δρόμοι της πόλης", ["σπιτ", "δρομ", "πολ"]),
    ("hindi", "लड़कों और लड़कियों के लिए किताबें", ["लडक", "लडक", "किताब"]),
    ("hungarian", "A házak és a kertek szépek", ["ház", "kert", "szép"]),
    ("indonesian", "Buku-buku dan permainan di rumah", ["buku", "buku", "main", "rumah"]),
    ("irish", "Na tithe agus an t-athair i gcathair", ["tithe", "athair", "cathair"]),
    ("italian", "I gatti e l'amica della casa", ["gatti", "amica", "casa"]),
    ("latvian", "Grāmatas un mājas ir lielas", ["grāmat", "māj", "liel"]),
    ("lithuanian", "Knygos ir namai yra dideli", ["knyg", "nam", "didel"]),
    ("norwegian", "Bøkene og hundene er på bordet", ["bøk", "hund", "bord"]),
    ("persian", "کتاب‌ها و مدرسه‌ها در شهر", ["كتاب", "مدرسه", "شهر"]),
    ("portuguese", "Os meninos e as meninas correram", ["menin", "menin", "correram"]),
    ("romanian", "Cărțile și caselor sunt mari", ["cărț", "cas", "mar"]),
    ("russian", "Книги и столы в комнатах", ["книг", "стол", "комнат"]),
    ("sorani", "کتێبەکان و قوتابخانەکان", ["کتێب", "قوتابخان"]),
    ("spanish", "Los niños y las canciones grandes", ["niñ", "cancion", "grand"]),
    ("swedish", "Böckerna och hundarna är på bordet", ["böck", "hund", "bordet"]),
    ("turkish", "Kitaplar ve evler çok güzel", ["kitap", "ev", "güzel"]),
    ("thai", "ภาษาไทยง่ายนิดเดียว", ["ภาษา", "ไทย", "ง่าย", "นิดเดียว"]),
    ("chinese", "你好世界 The Quick and", ["你", "好", "世", "界", "quick"]),
]


@pytest.mark.parametrize(
    "analyzer,text,tokens", LANGUAGE_ANALYZERS, ids=[c[0] for c in LANGUAGE_ANALYZERS]
)
def test_language_analyzer_tokens(client, analyzer, text, tokens):
    assert _analyze(client, analyzer=analyzer, text=text) == tokens


def test_language_analyzer_stopwords_leave_position_gaps(client):
    out = _analyze_full(client, analyzer="english", text="The running dogs and the cats")
    assert [(t["token"], t["position"]) for t in out] == [("run", 1), ("dog", 2), ("cat", 5)]
    out = _analyze_full(client, analyzer="russian", text="Книги и столы в комнатах")
    assert [(t["token"], t["position"]) for t in out] == [("книг", 0), ("стол", 2), ("комнат", 4)]


@pytest.mark.parametrize(
    "name",
    ["ukrainian", "polish", "serbian", "croatian", "slovak", "hebrew", "vietnamese",
     "japanese", "korean", "kuromoji", "nori", "smartcn", "icu_analyzer"],
)
def test_unavailable_language_analyzers_rejected(client, name):
    r = client.post("/_analyze", json={"analyzer": name, "text": "x"})
    assert r.status_code == 400, r.text
    assert r.json()["error"]["type"] == "illegal_argument_exception"
    assert f"failed to find global analyzer [{name}]" in r.json()["error"]["reason"]


# ---------------------------------------------------------------------------
# End-to-end: inflected query finds a differently inflected document via the
# language analyzer, while the standard analyzer does not.
# ---------------------------------------------------------------------------

STEM_SEARCH = [
    ("arabic", "الكتابات", "والكتاب"),
    ("armenian", "քաղաքներ", "քաղաքի"),
    ("basque", "etxeak", "etxe"),
    ("bengali", "বইগুলো", "বই"),
    ("brazilian", "meninos", "menino"),
    ("bulgarian", "книгите", "книга"),
    ("catalan", "gats", "gat"),
    ("czech", "kočkami", "kočky"),
    ("danish", "hundene", "hunden"),
    ("dutch", "katten", "kat"),
    ("english", "running", "runs"),
    ("estonian", "raamatud", "raamatut"),
    ("finnish", "talossa", "talo"),
    ("french", "chevaux", "cheval"),
    ("galician", "nenos", "neno"),
    ("german", "Häuser", "Haus"),
    ("greek", "σπιτιού", "σπίτια"),
    ("hindi", "लड़कियों", "लड़का"),
    ("hungarian", "házak", "ház"),
    ("indonesian", "permainan", "main"),
    ("irish", "gcathair", "cathair"),
    ("italian", "dell'amica", "amica"),
    ("latvian", "grāmatas", "grāmata"),
    ("lithuanian", "knygos", "knyga"),
    ("norwegian", "hundene", "hunden"),
    ("persian", "کتاب‌ها", "کتاب"),
    ("portuguese", "meninos", "menino"),
    ("romanian", "caselor", "casa"),
    ("russian", "комнатах", "комната"),
    ("sorani", "کتێبەکان", "کتێب"),
    ("spanish", "canciones", "canción"),
    ("swedish", "hundarna", "hund"),
    ("turkish", "kitaplardan", "kitap"),
    ("thai", "สวัสดีชาวโลก", "ชาว"),
    ("cjk", "안녕하세요 세계", "안녕"),
]


@pytest.mark.parametrize("analyzer,doc,query", STEM_SEARCH, ids=[c[0] for c in STEM_SEARCH])
def test_language_analyzer_search_inflected_forms(client, index_factory, analyzer, doc, query):
    name = index_factory(
        settings=SINGLE_SHARD,
        mappings={
            "properties": {
                "lang": {"type": "text", "analyzer": analyzer},
                "std": {"type": "text", "analyzer": "standard"},
            }
        },
    )
    r = client.put(f"/{name}/_doc/1", json={"lang": doc, "std": doc}, params={"refresh": "true"})
    assert r.status_code == 201, r.text
    assert _total(client, name, {"query": {"match": {"lang": query}}}) == 1
    assert _total(client, name, {"query": {"match": {"std": query}}}) == 0


SEARCH_ANALYZER_QUIRKS = [
    # (analyzer, document, query): the analyzer folds the pair, standard does not
    ("german", "Straße", "STRASSE"),
    ("german", "schön", "schon"),
    ("greek", "ΟΔΟΣ", "οδός"),
    ("turkish", "IŞIK", "ışık"),
    ("french", "étudiants", "etudiant"),
    ("cjk", "ＡＢＣ１２３", "abc123"),
    ("cjk", "ｶﾀｶﾅ", "カタカナ"),
    ("arabic", "أحمد", "احمد"),
    ("persian", "کتاب", "كتاب"),
    ("hindi", "क़िला", "किला"),
    ("german", "Mädchen", "Maedchen"),
    ("greek", "Άνθρωπος", "ΑΝΘΡΩΠΟΣ"),
    ("russian", "ёлка", "елка"),
    ("sorani", "كتێب", "کتێب"),
    ("bengali", "বাংলাদেশ", "বাংলাদেস"),
    ("arabic", "مدرسة", "مدرسه"),
    ("persian", "مدرسة", "مدرسه"),
    ("irish", "nAthair", "n-athair"),
]


@pytest.mark.parametrize(
    "analyzer,doc,query",
    SEARCH_ANALYZER_QUIRKS,
    ids=[f"{c[0]}-{i}" for i, c in enumerate(SEARCH_ANALYZER_QUIRKS)],
)
def test_language_analyzer_normalizes_variants(client, index_factory, analyzer, doc, query):
    name = index_factory(
        settings=SINGLE_SHARD,
        mappings={
            "properties": {
                "lang": {"type": "text", "analyzer": analyzer},
                "std": {"type": "text"},
            }
        },
    )
    client.put(f"/{name}/_doc/1", json={"lang": doc, "std": doc}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"lang": query}}}) == 1
    assert _total(client, name, {"query": {"match": {"std": query}}}) == 0


LANGUAGE_ANALYZER_NON_FOLDING = [
    # (analyzer, document, query): the language analyzer keeps these distinct
    ("norwegian", "blåbær", "blabaer"),
    ("swedish", "smörgås", "smorgas"),
    ("danish", "blåbær", "blaabaer"),
    ("bulgarian", "ёлка", "елка"),
    ("romanian", "Cărțile", "cartile"),
    ("spanish", "niño", "nino"),
]


@pytest.mark.parametrize(
    "analyzer,doc,query",
    LANGUAGE_ANALYZER_NON_FOLDING,
    ids=[c[0] for c in LANGUAGE_ANALYZER_NON_FOLDING],
)
def test_language_analyzer_keeps_diacritics(client, analyzer, doc, query):
    assert _analyze(client, analyzer=analyzer, text=doc) != _analyze(
        client, analyzer=analyzer, text=query
    )


# ---------------------------------------------------------------------------
# Language-specific token filters (standard tokenizer, no lowercase unless set)
# ---------------------------------------------------------------------------

LANGUAGE_FILTERS = [
    ("german_normalization", ["german_normalization"],
     "Straße Größe Äpfel Mädchen ae oe ue Haeuser",
     ["Strasse", "Grosse", "Äpfel", "Madchen", "a", "o", "u", "Hauser"]),
    ("scandinavian_folding", ["scandinavian_folding"],
     "blåbærsyltetøy Ærø aa ää öö", ["blabarsyltetoy", "Aro", "a", "aa", "oo"]),
    ("scandinavian_normalization", ["scandinavian_normalization"],
     "blåbærsyltetøy Ærø aa ää öö ae oe",
     ["blåbærsyltetøy", "Ærø", "å", "ææ", "øø", "æ", "ø"]),
    ("persian_normalization", ["persian_normalization"],
     "كتابي ھ ۀ ي ك", ["كتابي", "ھ", "ه", "ي", "ك"]),
    ("arabic_normalization", ["arabic_normalization"],
     "أحمد إسلام آمال مدرسة ى ـــكتاب", ["احمد", "اسلام", "امال", "مدرسه", "ي", "كتاب"]),
    ("hindi_normalization", ["hindi_normalization"],
     "लड़कों क़िला ख़बर", ["लडकों", "किला", "खबर"]),
    # indic_normalization composes consonant + nukta into the precomposed letter
    ("indic_normalization", ["indic_normalization"],
     "\u0932\u0921\u093c\u0915\u094b\u0902 \u0915\u093c\u093f\u0932\u093e",
     ["\u0932\u095c\u0915\u094b\u0902", "\u0958\u093f\u0932\u093e"]),
    ("sorani_normalization", ["sorani_normalization"], "كتێب ي ه", ["کتێب", "ی", "ە"]),
    ("serbian_normalization", ["serbian_normalization"],
     "Ђорђе Љубица њива ђак džak", ["Ђordje", "Љubica", "njiva", "djak", "dzak"]),
    ("bengali_normalization", ["bengali_normalization"],
     "বাংলাদেশের ড়", ["বাংলাদেসের", "ড়"]),
    ("cjk_width", ["cjk_width"], "ＡＢＣ１２３ ｶﾀｶﾅ ﾊﾟﾋﾟ", ["ABC123", "カタカナ", "パピ"]),
    ("cjk_bigram", ["cjk_bigram"], "东京タワー한국어 abc",
     ["东京", "京タ", "タワ", "ワー", "ー한", "한국", "국어", "abc"]),
    ("decimal_digit", ["decimal_digit"], "٤٥٦ ۱۲۳ १२३ ১২৩", ["456", "123", "123", "123"]),
    ("asciifolding", ["asciifolding"], "Ærø ñ ü Ł ß ﬁ", ["AEro", "n", "u", "L", "ss", "fi"]),
    ("elision_default", ["elision"], "l'avion d'Italie qu'il jusqu'au",
     ["avion", "Italie", "il", "au"]),
    ("elision_custom", [{"type": "elision", "articles": ["l", "d"]}],
     "l'avion d'Italie", ["avion", "Italie"]),
    ("lowercase_greek", [{"type": "lowercase", "language": "greek"}],
     "ΟΔΟΣ Άνθρωπος ΣΟΦΙΑ", ["οδοσ", "ανθρωποσ", "σοφια"]),
    ("lowercase_irish", [{"type": "lowercase", "language": "irish"}],
     "nAthair tEarrach n-Athair", ["n-athair", "t-earrach", "n", "athair"]),
    ("lowercase_turkish", [{"type": "lowercase", "language": "turkish"}],
     "İstanbul IŞIK ılık", ["istanbul", "ışık", "ılık"]),
    ("lowercase_default_on_turkish", ["lowercase"],
     "İstanbul IŞIK ılık", ["istanbul", "işik", "ılık"]),
    ("uppercase_sharp_s", ["uppercase"], "straße ﬁ", ["STRAßE", "ﬁ"]),
]


@pytest.mark.parametrize(
    "case,filters,text,tokens", LANGUAGE_FILTERS, ids=[c[0] for c in LANGUAGE_FILTERS]
)
def test_language_token_filters(client, case, filters, text, tokens):
    assert _analyze(client, tokenizer="standard", filter=filters, text=text) == tokens


LANGUAGE_ELISION = [
    ("french", "l'avion d'Italie qu'il", ["avion", "ital"]),
    ("italian", "dell'amica l'uomo", ["amica", "uomo"]),
    ("catalan", "l'home d'Andorra", ["hom", "andorr"]),
    ("irish", "d'fhéach m'athair", ["féach", "athair"]),
]


@pytest.mark.parametrize("analyzer,text,tokens", LANGUAGE_ELISION, ids=[c[0] for c in LANGUAGE_ELISION])
def test_language_analyzer_elision(client, analyzer, text, tokens):
    assert _analyze(client, analyzer=analyzer, text=text) == tokens


# ---------------------------------------------------------------------------
# Predefined stopword sets (_lang_) for the stop filter
# ---------------------------------------------------------------------------

STOP_SETS = [
    ("_arabic_", "في كتاب", ["كتاب"]),
    ("_armenian_", "և գիրք", ["գիրք"]),
    ("_basque_", "eta etxe", ["etxe"]),
    ("_bengali_", "এই বই", ["বই"]),
    ("_brazilian_", "os menino", ["menino"]),
    ("_bulgarian_", "на книга", ["книга"]),
    ("_catalan_", "els gat", ["gat"]),
    ("_czech_", "jsou pes", ["pes"]),
    ("_danish_", "og hund", ["hund"]),
    ("_dutch_", "de kat", ["kat"]),
    ("_english_", "the dog", ["dog"]),
    ("_estonian_", "ja raamat", ["raamat"]),
    ("_finnish_", "ja talo", ["talo"]),
    ("_french_", "les chat", ["chat"]),
    ("_galician_", "os neno", ["neno"]),
    ("_german_", "und haus", ["haus"]),
    ("_greek_", "και σπίτι", ["σπίτι"]),
    ("_hindi_", "और किताब", ["किताब"]),
    ("_hungarian_", "és ház", ["ház"]),
    ("_indonesian_", "dan buku", ["buku"]),
    ("_irish_", "agus teach", ["teach"]),
    ("_italian_", "della casa", ["casa"]),
    ("_latvian_", "un māja", ["māja"]),
    ("_lithuanian_", "ir knyga", ["knyga"]),
    ("_norwegian_", "og hund", ["hund"]),
    ("_persian_", "در شهر", ["شهر"]),
    ("_portuguese_", "os menino", ["menino"]),
    ("_romanian_", "și casa", ["casa"]),
    ("_russian_", "и стол", ["стол"]),
    ("_sorani_", "و کتێب", ["کتێب"]),
    ("_spanish_", "los niño", ["niño"]),
    ("_swedish_", "och hund", ["hund"]),
    ("_thai_", "และ ภาษา", ["ภาษา"]),
    ("_turkish_", "ve kitap", ["kitap"]),
    # unknown set names are not an error: they become a literal stopword
    ("_ukrainian_", "і _ukrainian_", ["і"]),
    ("_cjk_", "and 世界", ["and", "世界"]),
]


@pytest.mark.parametrize("stopwords,text,tokens", STOP_SETS, ids=[c[0] for c in STOP_SETS])
def test_predefined_stopword_sets(client, stopwords, text, tokens):
    out = _analyze(
        client, tokenizer="whitespace", filter=[{"type": "stop", "stopwords": stopwords}], text=text
    )
    assert out == tokens


# ---------------------------------------------------------------------------
# stemmer token filter: per-language variants
# ---------------------------------------------------------------------------

STEMMERS = [
    ("russian", "комнатах", "комнат"),
    ("light_russian", "комнатах", "комнат"),
    ("german", "häuser", "haus"),
    ("german2", "haeuser", "haus"),
    ("light_german", "häuser", "haus"),
    ("minimal_german", "häuser", "haus"),
    ("finnish", "talossa", "talo"),
    ("light_finnish", "talossa", "talo"),
    ("french", "chevaux", "cheval"),
    ("minimal_french", "chevaux", "cheval"),
    ("light_spanish", "canciones", "cancion"),
    ("portuguese_rslp", "meninos", "menin"),
    ("minimal_portuguese", "meninos", "menino"),
    ("italian", "gatti", "gatt"),
    ("light_italian", "gatti", "gatti"),
    ("swedish", "hundarna", "hund"),
    ("light_swedish", "hundarna", "hundarn"),
    ("light_nynorsk", "hundane", "hund"),
    ("minimal_norwegian", "hundene", "hund"),
    ("hungarian", "házak", "ház"),
    ("light_hungarian", "házak", "haz"),
    ("english", "running", "run"),
    ("light_english", "running", "running"),
    ("minimal_english", "dogs", "dog"),
    ("possessive_english", "john's", "john"),
    ("greek", "σπίτια", "σπίτ"),
    ("minimal_galician", "nenos", "neno"),
    ("serbian", "kućama", "kuć"),
    ("irish", "gcathair", "cathair"),
    ("indonesian", "permainan", "main"),
]


@pytest.mark.parametrize("language,word,stem", STEMMERS, ids=[c[0] for c in STEMMERS])
def test_stemmer_filter_languages(client, language, word, stem):
    out = _analyze(
        client,
        tokenizer="standard",
        filter=["lowercase", {"type": "stemmer", "language": language}],
        text=word,
    )
    assert out == [stem]


@pytest.mark.parametrize("language", ["polish", "ukrainian", "lovins", "dutch_kp"])
def test_stemmer_filter_unknown_language_rejected(client, language):
    r = client.post(
        "/_analyze",
        json={"tokenizer": "standard", "filter": [{"type": "stemmer", "language": language}], "text": "x"},
    )
    assert r.status_code == 400, r.text
    assert "Invalid stemmer class specified" in r.json()["error"]["reason"]


def test_serbian_cyrillic_and_latin_match_via_custom_analyzer(client, index_factory):
    name = index_factory(
        settings={
            **SINGLE_SHARD,
            "analysis": {
                "analyzer": {
                    "sr": {
                        "tokenizer": "standard",
                        "filter": ["lowercase", "serbian_normalization", "sr_stem"],
                    }
                },
                "filter": {"sr_stem": {"type": "stemmer", "language": "serbian"}},
            },
        },
        mappings={"properties": {"body": {"type": "text", "analyzer": "sr"}}},
    )
    toks = client.post(f"/{name}/_analyze", json={"analyzer": "sr", "text": "Ђорђе Đorđe кућама kućama"})
    assert [t["token"] for t in toks.json()["tokens"]] == ["đorđ", "đorđ", "kuc", "kuc"]
    client.put(f"/{name}/_doc/1", json={"body": "Ђорђе живи у кућама"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": "Đorđe"}}}) == 1
    assert _total(client, name, {"query": {"match": {"body": "kuća"}}}) == 1


# ---------------------------------------------------------------------------
# Thai / CJK specific tokenization
# ---------------------------------------------------------------------------

THAI_TOKENIZER = [
    ("thai", "ภาษาไทยง่ายนิดเดียว", ["ภาษา", "ไทย", "ง่าย", "นิดเดียว"]),
    ("thai_greeting", "สวัสดีครับ Hello", ["สวัสดี", "ครับ", "Hello"]),
    ("lao_not_split", "ສະບາຍດີ", ["ສະບາຍດີ"]),
    ("khmer_not_split", "សួស្តីពិភពលោក", ["សួស្តីពិភពលោក"]),
]


@pytest.mark.parametrize("case,text,tokens", THAI_TOKENIZER, ids=[c[0] for c in THAI_TOKENIZER])
def test_thai_tokenizer(client, case, text, tokens):
    assert _analyze(client, tokenizer="thai", text=text) == tokens


CJK_ANALYZER = [
    ("single_char_kept", "北", ["北"]),
    ("bigram", "北京", ["北京"]),
    ("halfwidth_katakana", "ｶﾀｶﾅ", ["カタ", "タカ", "カナ"]),
    ("hangul_and_kanji", "한국어 東京", ["한국", "국어", "東京"]),
    ("mixed_kana", "東京タワーへ", ["東京", "京タ", "タワ", "ワー", "ーへ"]),
]


@pytest.mark.parametrize("case,text,tokens", CJK_ANALYZER, ids=[c[0] for c in CJK_ANALYZER])
def test_cjk_analyzer_bigrams(client, case, text, tokens):
    assert _analyze(client, analyzer="cjk", text=text) == tokens


EMOJI_BY_ANALYZER = [
    ("standard", ["i", "❤️", "🍕", "👨‍👩‍👧"]),
    ("english", ["i", "❤️", "🍕", "👨‍👩‍👧"]),
    ("russian", ["i", "❤️", "🍕", "👨‍👩‍👧"]),
    ("cjk", ["i", "❤️", "🍕", "👨‍👩‍👧"]),
    ("arabic", ["i", "❤️", "🍕", "👨‍👩‍👧"]),
    ("turkish", ["ı", "❤️", "🍕", "👨‍👩‍👧"]),
    ("thai", ["i"]),
]


@pytest.mark.parametrize("analyzer,tokens", EMOJI_BY_ANALYZER, ids=[c[0] for c in EMOJI_BY_ANALYZER])
def test_emoji_through_language_analyzers(client, analyzer, tokens):
    assert _analyze(client, analyzer=analyzer, text="I ❤️ 🍕 👨‍👩‍👧") == tokens


EMOJI_SEQUENCES = [
    ("skin_tone", "👍🏽 ok", [("👍🏽", 0, 4), ("ok", 5, 7)]),
    ("flags", "🇺🇦🇯🇵", [("🇺🇦", 0, 4), ("🇯🇵", 4, 8)]),
    ("zwj_family", "👨‍👩‍👧 👨", [("👨‍👩‍👧", 0, 8), ("👨", 9, 11)]),
    ("keycap", "1️⃣ #️⃣", [("1️⃣", 0, 3), ("#️⃣", 4, 7)]),
    ("rainbow_flag", "🏳️‍🌈", [("🏳️‍🌈", 0, 6)]),
    ("emoji_splits_latin", "abc🍕def", [("abc", 0, 3), ("🍕", 3, 5), ("def", 5, 8)]),
    ("emoji_in_cjk", "日本🍕語", [("日", 0, 1), ("本", 1, 2), ("🍕", 2, 4), ("語", 4, 5)]),
    ("zwj_technologist", "👨‍💻dev", [("👨‍💻", 0, 5), ("dev", 5, 8)]),
    ("zwj_inside_word", "a‍b", [("a‍b", 0, 3)]),
    ("soft_hyphen", "x­y", [("x­y", 0, 3)]),
]


@pytest.mark.parametrize("case,text,expected", EMOJI_SEQUENCES, ids=[c[0] for c in EMOJI_SEQUENCES])
def test_emoji_sequences_and_utf16_offsets(client, case, text, expected):
    out = _analyze_full(client, analyzer="standard", text=text)
    assert [(t["token"], t["start_offset"], t["end_offset"]) for t in out] == expected


def test_zwj_family_emoji_is_one_term(client, index_factory):
    name = index_factory(settings=SINGLE_SHARD, mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": "family 👨‍👩‍👧"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": "👨‍👩‍👧"}}}) == 1
    assert _total(client, name, {"query": {"match": {"body": "👨"}}}) == 0


# ---------------------------------------------------------------------------
# Unicode normalization quirks with the standard analyzer (no ICU)
# ---------------------------------------------------------------------------

STANDARD_NON_MATCHES = [
    # (case, document, query, matches) on a standard-analyzed text field
    ("nfc_doc_nfd_query", "café", unicodedata.normalize("NFD", "café"), False),
    ("nfd_doc_nfc_query", unicodedata.normalize("NFD", "café"), "café", False),
    ("nfc_same", "café", "CAFÉ", True),
    ("greek_upper_sigma_vs_final", "ΟΔΟΣ", "οδος", False),
    ("greek_upper_sigma_vs_medial", "ΟΔΟΣ", "οδοσ", True),
    ("greek_accent_kept", "οδός", "οδος", False),
    ("turkish_dotless_upper", "IŞIK", "ışık", False),
    ("turkish_dotless_upper_root_lower", "IŞIK", "işik", True),
    ("turkish_dotted_capital", "İstanbul", "istanbul", True),
    ("fullwidth_vs_ascii", "ＡＢＣ１２３", "abc123", False),
    ("fullwidth_case_folded", "ＡＢＣ１２３", "ａｂｃ１２３", True),
    ("ligature_vs_letters", "ﬁnance", "finance", False),
    ("sharp_s_vs_ss", "Straße", "strasse", False),
    ("capital_sharp_s_lowercases", "STRAẞE", "straße", True),
    ("dz_digraph_titlecase", "ǅemal", "ǆemal", True),
    ("arabic_alef_hamza", "أحمد", "احمد", False),
    ("persian_vs_arabic_kaf", "کتاب", "كتاب", False),
    ("cyrillic_yo", "ёлка", "елка", False),
    ("latin_vs_cyrillic_homoglyph", "Pyx", "Рух", False),
]


@pytest.mark.parametrize(
    "case,doc,query,matches", STANDARD_NON_MATCHES, ids=[c[0] for c in STANDARD_NON_MATCHES]
)
def test_standard_unicode_variants(client, index_factory, case, doc, query, matches):
    name = index_factory(settings=SINGLE_SHARD, mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": doc}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": query}}}) == (1 if matches else 0)


def test_nfd_is_not_normalized_by_standard_analyzer(client):
    nfd = unicodedata.normalize("NFD", "Café")
    out = _analyze_full(client, analyzer="standard", text=nfd)
    assert [t["token"] for t in out] == ["café"]
    assert (out[0]["start_offset"], out[0]["end_offset"]) == (0, 5)


def test_nfd_and_nfc_keyword_terms_differ(client, index_factory):
    name = index_factory(settings=SINGLE_SHARD, mappings={"properties": {"tag": {"type": "keyword"}}})
    nfd = unicodedata.normalize("NFD", "café")
    client.put(f"/{name}/_doc/1", json={"tag": "café"}, params={"refresh": "true"})
    client.put(f"/{name}/_doc/2", json={"tag": nfd}, params={"refresh": "true"})
    assert _ids(client, name, {"query": {"term": {"tag": "café"}}}) == ["1"]
    assert _ids(client, name, {"query": {"term": {"tag": nfd}}}) == ["2"]
    r = client.post(f"/{name}/_search", json={"size": 0, "aggs": {"t": {"terms": {"field": "tag"}}}})
    assert len(r.json()["aggregations"]["t"]["buckets"]) == 2


def test_asciifolding_folds_ligatures_but_not_combining_marks(client, index_factory):
    name = index_factory(
        settings={
            **SINGLE_SHARD,
            "analysis": {"analyzer": {"fold": {"tokenizer": "standard", "filter": ["lowercase", "asciifolding"]}}},
        },
        mappings={"properties": {"body": {"type": "text", "analyzer": "fold"}}},
    )
    client.put(f"/{name}/_doc/1", json={"body": "café ﬁnance Straße"}, params={"refresh": "true"})
    for q in ["cafe", "café", "finance", "ﬁnance", "strasse"]:
        assert _total(client, name, {"query": {"match": {"body": q}}}) == 1, q
    # asciifolding leaves a standalone combining mark (U+0301) untouched
    assert _analyze(client, tokenizer="standard", filter=["asciifolding"], text="cafe\u0301") == ["cafe\u0301"]
    assert _total(client, name, {"query": {"match": {"body": unicodedata.normalize("NFD", "café")}}}) == 0


def test_cjk_width_custom_analyzer_matches_fullwidth(client, index_factory):
    name = index_factory(
        settings={
            **SINGLE_SHARD,
            "analysis": {"analyzer": {"w": {"tokenizer": "standard", "filter": ["cjk_width", "lowercase"]}}},
        },
        mappings={"properties": {"body": {"type": "text", "analyzer": "w"}}},
    )
    client.put(f"/{name}/_doc/1", json={"body": "ＡＢＣ１２３ ｶﾀｶﾅ"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": "abc123"}}}) == 1
    assert _total(client, name, {"query": {"match": {"body": "カタカナ"}}}) == 1


def test_turkish_lowercase_filter_search(client, index_factory):
    name = index_factory(
        settings={
            **SINGLE_SHARD,
            "analysis": {
                "filter": {"tr_lower": {"type": "lowercase", "language": "turkish"}},
                "analyzer": {"tr": {"tokenizer": "standard", "filter": ["tr_lower"]}},
            },
        },
        mappings={"properties": {"body": {"type": "text", "analyzer": "tr"}, "std": {"type": "text"}}},
    )
    client.put(f"/{name}/_doc/1", json={"body": "IŞIK İzmir", "std": "IŞIK İzmir"}, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"body": "ışık"}}}) == 1
    assert _total(client, name, {"query": {"match": {"body": "işik"}}}) == 0
    assert _total(client, name, {"query": {"match": {"body": "izmir"}}}) == 1
    assert _total(client, name, {"query": {"match": {"std": "ışık"}}}) == 0
    assert _total(client, name, {"query": {"match": {"std": "işik"}}}) == 1


# ---------------------------------------------------------------------------
# Keywords in non-Latin scripts: sort, terms aggs, range (code point order)
# ---------------------------------------------------------------------------

KEYWORD_VALUES = ["яблуко", "Яблуко", "apple", "Äpfel", "zebra", "ångström", "αβγ", "日本", "中国",
                  "한국", "𠀀", "😀", "Ａ", "", "ﬁ", "עברית", "عربي", "ñ", "n", "Z"]
# Unicode code point order == UTF-8 byte order (NOT UTF-16 order: astral chars sort
# after U+FFxx full-width forms and the private use area).
KEYWORD_ORDER = ["Z", "apple", "n", "zebra", "Äpfel", "ångström", "ñ", "αβγ", "Яблуко", "яблуко",
                 "עברית", "عربي", "中国", "日本", "한국", "", "ﬁ", "Ａ", "😀", "𠀀"]


@pytest.fixture
def keyword_index(client, index_factory, bulk_index):
    name = index_factory(settings=SINGLE_SHARD, mappings={"properties": {"tag": {"type": "keyword"}}})
    bulk_index(name, [{"tag": v} for v in KEYWORD_VALUES])
    return name


def test_keyword_sort_asc_code_point_order(client, keyword_index):
    r = client.post(f"/{keyword_index}/_search", json={"size": 50, "sort": [{"tag": "asc"}]})
    assert [h["sort"][0] for h in r.json()["hits"]["hits"]] == KEYWORD_ORDER


def test_keyword_sort_desc_code_point_order(client, keyword_index):
    r = client.post(f"/{keyword_index}/_search", json={"size": 50, "sort": [{"tag": "desc"}]})
    assert [h["sort"][0] for h in r.json()["hits"]["hits"]] == KEYWORD_ORDER[::-1]


@pytest.mark.parametrize("direction", ["asc", "desc"])
def test_keyword_terms_agg_key_order(client, keyword_index, direction):
    r = client.post(
        f"/{keyword_index}/_search",
        json={"size": 0, "aggs": {"t": {"terms": {"field": "tag", "size": 50, "order": {"_key": direction}}}}},
    )
    keys = [b["key"] for b in r.json()["aggregations"]["t"]["buckets"]]
    assert keys == (KEYWORD_ORDER if direction == "asc" else KEYWORD_ORDER[::-1])


KEYWORD_RANGES = [
    ({"gte": "А", "lt": "я"}, ["Яблуко"]),
    ({"gte": "А", "lte": "яя"}, ["Яблуко", "яблуко"]),
    ({"gt": "z"}, ["zebra", "Äpfel", "ångström", "ñ", "αβγ", "Яблуко", "яблуко", "עברית", "عربي", "中国", "日本",
                   "한국", "", "ﬁ", "Ａ", "😀", "𠀀"]),
    ({"gte": "一", "lt": "가"}, ["中国", "日本"]),
    ({"gte": "￿"}, ["😀", "𠀀"]),
    ({"gte": "", "lt": "\U00010000"}, ["", "ﬁ", "Ａ"]),
]


@pytest.mark.parametrize("rng,expected", KEYWORD_RANGES, ids=[str(i) for i in range(len(KEYWORD_RANGES))])
def test_keyword_range_code_point_order(client, keyword_index, rng, expected):
    r = client.post(
        f"/{keyword_index}/_search",
        json={"size": 50, "query": {"range": {"tag": rng}}, "sort": [{"tag": "asc"}]},
    )
    assert [h["_source"]["tag"] for h in r.json()["hits"]["hits"]] == expected


def test_keyword_case_insensitive_is_ascii_only(client, index_factory):
    name = index_factory(settings=SINGLE_SHARD, mappings={"properties": {"tag": {"type": "keyword"}}})
    client.put(f"/{name}/_doc/1", json={"tag": "Café"}, params={"refresh": "true"})
    client.put(f"/{name}/_doc/2", json={"tag": "Харків"}, params={"refresh": "true"})
    cases = [
        ({"term": {"tag": {"value": "café", "case_insensitive": True}}}, ["1"]),
        ({"term": {"tag": {"value": "CAFÉ", "case_insensitive": True}}}, []),
        ({"prefix": {"tag": {"value": "CAF", "case_insensitive": True}}}, ["1"]),
        ({"prefix": {"tag": {"value": "CAFÉ", "case_insensitive": True}}}, []),
        ({"term": {"tag": {"value": "харків", "case_insensitive": True}}}, []),
        ({"prefix": {"tag": {"value": "ХАРК", "case_insensitive": True}}}, []),
        ({"prefix": {"tag": {"value": "Харк", "case_insensitive": True}}}, ["2"]),
    ]
    for q, expected in cases:
        assert _ids(client, name, {"query": q}) == expected, q


# ---------------------------------------------------------------------------
# Term-level queries (prefix / wildcard / regexp / fuzzy) on non-Latin text
# ---------------------------------------------------------------------------

TERM_LEVEL_DOCS = {
    "1": "Київ столиця України",
    "2": "Η Αθήνα είναι πρωτεύουσα",
    "3": "我爱北京天安门",
    "4": "🍕 pizza 𠀀𠀁 世界 😀 end",
    "5": "東京タワーへ行きます",
    "6": "Hello שלום world مرحبا",
}


@pytest.fixture
def term_level_index(client, index_factory):
    name = index_factory(
        settings=SINGLE_SHARD,
        mappings={"properties": {"body": {"type": "text"}, "tag": {"type": "keyword"}}},
    )
    for doc_id, text in TERM_LEVEL_DOCS.items():
        client.put(f"/{name}/_doc/{doc_id}", json={"body": text, "tag": text})
    client.post(f"/{name}/_refresh")
    return name


TERM_LEVEL_QUERIES = [
    ("prefix_cyrillic", {"prefix": {"body": "ки"}}, ["1"]),
    ("prefix_cyrillic_upper_not_analyzed", {"prefix": {"body": "КИ"}}, []),
    ("prefix_cyrillic_case_insensitive_ascii_only",
     {"prefix": {"body": {"value": "КИ", "case_insensitive": True}}}, []),
    ("prefix_greek", {"prefix": {"body": "αθ"}}, ["2"]),
    ("prefix_greek_accent_matters", {"prefix": {"body": "αθη"}}, []),
    ("wildcard_cyrillic", {"wildcard": {"body": "сто*ця"}}, ["1"]),
    ("wildcard_greek_question", {"wildcard": {"body": "αθ?να"}}, ["2"]),
    ("wildcard_greek_accented", {"wildcard": {"body": "αθή?α"}}, ["2"]),
    ("regexp_cyrillic", {"regexp": {"body": "ки.*"}}, ["1"]),
    ("fuzzy_cyrillic_substitution", {"fuzzy": {"body": "стулиця"}}, ["1"]),
    ("fuzzy_cyrillic_transposition", {"fuzzy": {"body": "сотлиця"}}, ["1"]),
    ("fuzzy_greek_deletion", {"fuzzy": {"body": {"value": "ρωτεύουσα", "fuzziness": 1}}}, ["2"]),
    ("match_fuzziness_auto_cyrillic", {"match": {"body": {"query": "укроїни", "fuzziness": "AUTO"}}}, ["1"]),
    ("fuzzy_single_cjk_char", {"fuzzy": {"body": "世"}}, ["4"]),
    ("term_cjk_ext_b", {"term": {"body": "𠀀"}}, ["4"]),
    ("match_cjk_ext_b", {"match": {"body": "𠀁"}}, ["4"]),
    ("match_hebrew", {"match": {"body": "שלום"}}, ["6"]),
    ("match_arabic", {"match": {"body": "مرحبا"}}, ["6"]),
    ("keyword_prefix_cjk", {"prefix": {"tag": "我爱"}}, ["3"]),
    ("keyword_wildcard_cjk", {"wildcard": {"tag": "*天安*"}}, ["3"]),
    ("keyword_wildcard_emoji", {"wildcard": {"tag": "🍕*"}}, ["4"]),
    ("keyword_wildcard_question_is_one_code_point", {"wildcard": {"tag": "? pizza*"}}, ["4"]),
    ("keyword_wildcard_astral_question", {"wildcard": {"tag": "*𠀀?*"}}, ["4"]),
    ("keyword_regexp_cjk", {"regexp": {"tag": "我.北京.*"}}, ["3"]),
    ("keyword_regexp_dot_astral", {"regexp": {"tag": ". pizza .*"}}, ["4"]),
    ("keyword_prefix_rtl", {"prefix": {"tag": "Hello ש"}}, ["6"]),
]


@pytest.mark.parametrize("case,query,expected", TERM_LEVEL_QUERIES, ids=[c[0] for c in TERM_LEVEL_QUERIES])
def test_term_level_queries_non_latin(client, term_level_index, case, query, expected):
    assert _ids(client, term_level_index, {"query": query}) == expected


PHRASE_QUERIES = [
    ("cjk_phrase_in_order", {"match_phrase": {"body": "北京"}}, ["3"]),
    ("cjk_phrase_reversed", {"match_phrase": {"body": "京北"}}, []),
    ("cjk_phrase_three_chars", {"match_phrase": {"body": "天安门"}}, ["3"]),
    ("cjk_match_and_ignores_order", {"match": {"body": {"query": "京北", "operator": "and"}}}, ["3"]),
    ("japanese_phrase_kanji_katakana", {"match_phrase": {"body": "東京タワー"}}, ["5"]),
    ("japanese_phrase_katakana_hiragana", {"match_phrase": {"body": "タワーへ"}}, ["5"]),
    ("japanese_phrase_gap", {"match_phrase": {"body": "東京へ"}}, []),
    ("japanese_phrase_slop", {"match_phrase": {"body": {"query": "東京へ", "slop": 1}}}, ["5"]),
    ("emoji_phrase", {"match_phrase": {"body": "🍕 pizza"}}, ["4"]),
    ("astral_phrase", {"match_phrase": {"body": "𠀀𠀁"}}, ["4"]),
    ("rtl_ltr_phrase", {"match_phrase": {"body": "hello שלום world"}}, ["6"]),
]


@pytest.mark.parametrize("case,query,expected", PHRASE_QUERIES, ids=[c[0] for c in PHRASE_QUERIES])
def test_phrase_queries_non_latin(client, term_level_index, case, query, expected):
    assert _ids(client, term_level_index, {"query": query}) == expected


def test_cjk_analyzer_phrase_and_single_char(client, index_factory):
    name = index_factory(
        settings=SINGLE_SHARD,
        mappings={"properties": {"cjk": {"type": "text", "analyzer": "cjk"}, "std": {"type": "text"}}},
    )
    client.put(f"/{name}/_doc/1", json={"cjk": "我爱北京天安门", "std": "我爱北京天安门"}, params={"refresh": "true"})
    # bigram index: a single character query is a unigram and finds nothing
    assert _total(client, name, {"query": {"match": {"cjk": "北"}}}) == 0
    assert _total(client, name, {"query": {"match": {"std": "北"}}}) == 1
    # non-adjacent characters: unigrams all present, bigram absent
    assert _total(client, name, {"query": {"match": {"cjk": {"query": "北天", "operator": "and"}}}}) == 0
    assert _total(client, name, {"query": {"match": {"std": {"query": "北天", "operator": "and"}}}}) == 1
    assert _total(client, name, {"query": {"match_phrase": {"cjk": "北京天安"}}}) == 1
    assert _total(client, name, {"query": {"match_phrase": {"cjk": "天安北京"}}}) == 0


# ---------------------------------------------------------------------------
# Highlighting: offsets are UTF-16 based, astral chars must not be split
# ---------------------------------------------------------------------------

HIGHLIGHTS = [
    ("pizza", "🍕 <em>pizza</em> 𠀀𠀁 世界 😀 end"),
    ("𠀀", "🍕 pizza <em>𠀀</em>𠀁 世界 😀 end"),
    ("𠀁", "🍕 pizza 𠀀<em>𠀁</em> 世界 😀 end"),
    ("😀", "🍕 pizza 𠀀𠀁 世界 <em>😀</em> end"),
    ("🍕", "<em>🍕</em> pizza 𠀀𠀁 世界 😀 end"),
    ("世界", "🍕 pizza 𠀀𠀁 <em>世</em><em>界</em> 😀 end"),
    ("end", "🍕 pizza 𠀀𠀁 世界 😀 <em>end</em>"),
]


@pytest.mark.parametrize("highlighter", ["unified", "plain"])
@pytest.mark.parametrize("query,fragment", HIGHLIGHTS, ids=[f"h{i}" for i in range(len(HIGHLIGHTS))])
def test_highlight_astral_and_cjk(client, term_level_index, highlighter, query, fragment):
    r = client.post(
        f"/{term_level_index}/_search",
        json={"query": {"match": {"body": query}}, "highlight": {"fields": {"body": {"type": highlighter}}}},
    )
    hits = r.json()["hits"]["hits"]
    assert [h["_id"] for h in hits] == ["4"]
    assert hits[0]["highlight"]["body"] == [fragment]


HIGHLIGHT_SCRIPTS = [
    ("cyrillic", "Київ столиця України", "столиця", "Київ <em>столиця</em> України"),
    ("greek", "Η Αθήνα είναι πρωτεύουσα", "αθήνα", "Η <em>Αθήνα</em> είναι πρωτεύουσα"),
    ("hebrew_in_ltr", "Hello שלום world", "שלום", "Hello <em>שלום</em> world"),
    ("arabic", "مرحبا بالعالم", "بالعالم", "مرحبا <em>بالعالم</em>"),
    ("devanagari", "नमस्ते दुनिया", "दुनिया", "नमस्ते <em>दुनिया</em>"),
    ("thai_whole_run", "สวัสดีชาวโลก ok", "สวัสดีชาวโลก", "<em>สวัสดีชาวโลก</em> ok"),
    ("nfd_combining", "café bar", "café", "<em>café</em> bar"),
    ("zwj_emoji", "x 👨‍👩‍👧 y", "👨‍👩‍👧", "x <em>👨‍👩‍👧</em> y"),
]


@pytest.mark.parametrize(
    "case,text,query,fragment", HIGHLIGHT_SCRIPTS, ids=[c[0] for c in HIGHLIGHT_SCRIPTS]
)
def test_highlight_scripts(client, index_factory, case, text, query, fragment):
    name = index_factory(settings=SINGLE_SHARD, mappings={"properties": {"body": {"type": "text"}}})
    client.put(f"/{name}/_doc/1", json={"body": text}, params={"refresh": "true"})
    r = client.post(
        f"/{name}/_search",
        json={"query": {"match": {"body": query}}, "highlight": {"fields": {"body": {}}}},
    )
    hits = r.json()["hits"]["hits"]
    assert hits and hits[0]["highlight"]["body"] == [fragment]


def test_highlight_language_analyzer_stemmed(client, index_factory):
    name = index_factory(
        settings=SINGLE_SHARD,
        mappings={"properties": {"body": {"type": "text", "analyzer": "russian"}}},
    )
    client.put(f"/{name}/_doc/1", json={"body": "Книги лежат в комнатах"}, params={"refresh": "true"})
    r = client.post(
        f"/{name}/_search",
        json={"query": {"match": {"body": "комната"}}, "highlight": {"fields": {"body": {}}}},
    )
    assert r.json()["hits"]["hits"][0]["highlight"]["body"] == ["Книги лежат в <em>комнатах</em>"]


def test_analyze_offsets_are_utf16_code_units(client):
    out = _analyze_full(client, analyzer="standard", text="🍕 pizza 𠀀𠀁 世界 😀 end")
    assert [(t["token"], t["start_offset"], t["end_offset"], t["position"]) for t in out] == [
        ("🍕", 0, 2, 0), ("pizza", 3, 8, 1), ("𠀀", 9, 11, 2), ("𠀁", 11, 13, 3),
        ("世", 14, 15, 4), ("界", 15, 16, 5), ("😀", 17, 19, 6), ("end", 20, 23, 7),
    ]


# ---------------------------------------------------------------------------
# Non-Latin ids, field names and _source round-trip
# ---------------------------------------------------------------------------

UNICODE_IDS = ["Привіт-світ", "日本語ID", "🍕🍕", "עברית_123", "Ελληνικά", "𠀀𠀁", "مرحبا", "café́"]


@pytest.mark.parametrize("doc_id", UNICODE_IDS, ids=[f"id{i}" for i in range(len(UNICODE_IDS))])
def test_unicode_document_ids(client, index_factory, doc_id):
    name = index_factory(settings=SINGLE_SHARD)
    path = f"/{name}/_doc/{quote(doc_id, safe='')}"
    r = client.put(path, json={"v": 1}, params={"refresh": "true"})
    assert r.status_code == 201, r.text
    assert r.json()["_id"] == doc_id
    r = client.get(path)
    assert r.status_code == 200 and r.json()["_id"] == doc_id
    assert _ids(client, name, {"query": {"ids": {"values": [doc_id]}}}) == [doc_id]
    assert _ids(client, name, {"query": {"term": {"_id": doc_id}}}) == [doc_id]


def test_unicode_field_names(client, index_factory):
    name = index_factory(
        settings=SINGLE_SHARD,
        mappings={"properties": {"прізвище": {"type": "text"}, "名前": {"type": "keyword"}}},
    )
    doc = {"прізвище": "Тарас Шевченко", "名前": "太郎", "🍕": "emoji field", "όνομα": {"πρώτο": "Νίκος"}}
    client.put(f"/{name}/_doc/1", json=doc, params={"refresh": "true"})
    assert _total(client, name, {"query": {"match": {"прізвище": "тарас"}}}) == 1
    assert _total(client, name, {"query": {"term": {"名前": "太郎"}}}) == 1
    assert _total(client, name, {"query": {"match": {"🍕": "emoji"}}}) == 1
    assert _total(client, name, {"query": {"match": {"όνομα.πρώτο": "νίκος"}}}) == 1
    assert _total(client, name, {"query": {"term": {"όνομα.πρώτο.keyword": "Νίκος"}}}) == 1
    props = client.get(f"/{name}/_mapping").json()[name]["mappings"]["properties"]
    assert set(props) == {"прізвище", "名前", "🍕", "όνομα"}
    assert props["🍕"]["type"] == "text"
    assert props["όνομα"]["properties"]["πρώτο"]["fields"]["keyword"]["type"] == "keyword"
    r = client.post(f"/{name}/_search", json={"size": 0, "aggs": {"n": {"terms": {"field": "名前"}}}})
    assert [b["key"] for b in r.json()["aggregations"]["n"]["buckets"]] == ["太郎"]


ROUND_TRIP = {
    "ru": "Съешь же ещё этих мягких булок", "be": "Беларусь і Мінск", "pl": "Zażółć gęślą jaźń",
    "cs": "Příliš žluťoučký kůň", "sr": "Ђорђе / Đorđe", "ro": "Țară și învățătură",
    "is": "Þjóðin ð þ", "tr": "İstanbul Işık ılık", "az": "Azərbaycan", "kk": "Қазақстан",
    "ka": "გამარჯობა", "hy": "Բարեւ", "he": "שלום עולם", "fa": "کتاب‌ها", "ur": "ہیلو",
    "hi": "नमस्ते", "bn": "হ্যালো", "ta": "வணக்கம்", "th": "สวัสดี", "lo": "ສະບາຍດີ",
    "km": "សួស្តី", "my": "မင်္ဂလာပါ", "vi": "Tiếng Việt", "am": "ሰላም", "mn": "Сайн",
    "zh": "繁體 简体", "ja": "東京タワーへ", "ko": "대한민국", "emoji": "👨‍👩‍👧 🇺🇦 👍🏽",
    "astral": "𠀀𠀁𪚥", "nfd": "café", "fullwidth": "ＡＢＣ１２３", "lig": "ﬁﬂ",
    "bidi": "abc ‮RTL‬ שלום", "zwj": "a‍b‌c", "nul_like": "  ",
}


def test_source_round_trip_all_scripts(client, index_factory):
    name = index_factory(settings=SINGLE_SHARD)
    r = client.put(f"/{name}/_doc/1", json=ROUND_TRIP, params={"refresh": "true"})
    assert r.status_code == 201, r.text
    assert client.get(f"/{name}/_doc/1").json()["_source"] == ROUND_TRIP
    hits = client.post(f"/{name}/_search", json={"query": {"match_all": {}}}).json()["hits"]["hits"]
    assert hits[0]["_source"] == ROUND_TRIP
    r = client.get(f"/{name}/_source/1")
    assert r.json() == ROUND_TRIP


def test_source_round_trip_raw_utf8_bytes(client, index_factory):
    name = index_factory(settings=SINGLE_SHARD)
    body = json.dumps({"t": "日本 🍕 𠀀 שלום"}, ensure_ascii=False).encode("utf-8")
    r = client.put(
        f"/{name}/_doc/1", content=body,
        headers={"Content-Type": "application/json"}, params={"refresh": "true"},
    )
    assert r.status_code == 201, r.text
    # escaped (\\uXXXX + surrogate pairs) and raw UTF-8 input are equivalent
    escaped = json.dumps({"t": "日本 🍕 𠀀 שלום"}).encode("ascii")
    r = client.put(
        f"/{name}/_doc/2", content=escaped,
        headers={"Content-Type": "application/json"}, params={"refresh": "true"},
    )
    assert r.status_code == 201, r.text
    assert client.get(f"/{name}/_source/1").json() == client.get(f"/{name}/_source/2").json()
    assert _total(client, name, {"query": {"match": {"t": "𠀀"}}}) == 2
