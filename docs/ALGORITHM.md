# How the romaji is made

The goal is a search query: the title of a work, written the way a site that lists it in the Latin
alphabet would write it. `チートアイテム管理局のお仕事` should become `cheat item kanri kyoku no oshigoto`.

## The base: cutlet

[cutlet](https://github.com/polm/cutlet) splits the text into words with MeCab + UniDic, reads each
word from the dictionary and writes it in Hepburn romaji. Two things it does matter here:

- **A katakana loanword is written with its foreign spelling.** UniDic keeps it in the lemma
  (`チート-cheat`, `アイテム-item`), so `チート` is `cheat`, not `chiito`.
- **Words get spaces by rules** (a particle is a word, a suffix sticks to the word before it, a prefix
  sticks to the word after it, ...). `romanizer.rs::cutlet_tokens` is that code, line by line.

`romaji.rs` is cutlet's Hepburn table and its kana rules (digraphs, っ, ん before a vowel, ー).

lindera + UniDic replaces MeCab + UniDic. Both are the same dictionary family, but cutlet in Python used
UniDic 2.3.0 (unidic-lite) and lindera embeds UniDic 2.1.2.

### The reading of a word

UniDic 2.3.0 has a `kana` column: the reading as it is spelled (号 is ゴウ, しよう is シヨウ). UniDic 2.1.2 has only
the pronunciation (`pron`, where a long vowel is `ー`: ゴー, ショー) and the reading of the dictionary form
(`lForm`: 見込んで is ミコム, not ミコン). `Word::reading` works the spelled reading out:

1. A word written in kana is read as it is written (しよう is `shiyou`, を is `o`, おねえちゃん is
   `oneechan`). An interjection keeps its pronunciation (こんにちは is `konnichiwa`).
2. A word that is not inflected, or is in its dictionary form, is read from `lForm`, which spells the long
   vowels out (号 is ゴウ, 言う is イウ). Not a numeral: ふ is フ, not フタ.
3. An inflected word is read from `pron`. Each `ー` becomes the vowel `lForm` has there (大きく is オオキク), or
   the vowel the kana before it ends with (聞い is キー, so キイ; 行こう is イコー, so イコウ).

## What is added on top

### 1. A loanword in hiragana is one word

UniDic cuts `ふらすとれーしょん` into `ふらす / とれ / ー / しょ / ん` (a verb, an auxiliary, a symbol, ...), and
cutlet would write `furasutore- sho n`. A hiragana word with a `ー` is almost always a loanword
(`ー` is not used in native Japanese words), so those pieces are joined:

- going back from the `ー`, over the hiragana pieces, up to a dictionary word (`よし` in
  `ナカよしふらすとれーしょん`: a noun that is not a piece);
- going forward over the end pieces, but not over a particle (`の` in `ふらすとれーしょんの話`; the `ん`
  is the exception, UniDic calls it a particle there);
- a small kana that was cut off the kana before it (`ふ / ぁ / みこん` is `ふぁみこん`) joins both.

The joined word is read from its kana directly, so it is `famikon` and not `fu a mikon`.

### 2. `ー` is a hyphen

cutlet repeats the vowel for `ー` (`rikabarii`). Here it stays what it was written as: `rikabari-`,
`furasutore-shon`, `arigato-`. A long vowel written with kana (`ryokou`) is not touched, and neither is a
loanword that becomes an English word.

### 3. `loanwords=english`

The joined hiragana word is turned into katakana and looked up again: if UniDic has every word of it as
a foreign word, the English spelling is used (`ふらすとれーしょん` → `フラストレーション` →
`frustration`; `ふぁんたじー` → `fantasy`). Otherwise the romaji stays.

Turning *all* the hiragana of a text into katakana would make most loanwords English, but it breaks the
ordinary Japanese: MeCab cuts a sentence by its hiragana particles and endings, so `寂しいうさちゃんは…`
becomes one long word and `救われたい` leaves a kanji in the output. Only the part that is already
known to be a loanword is converted.

### 4. Exceptions

UniDic's foreign spelling can be a different foreign word with the same sound: `ラーメン` is `Rahmen`
(German, "frame"), `メイド` is `made`. `BUILT_IN_EXCEPTIONS` (and `[exceptions]` in `config.toml`)
replaces such words. They are matched against a whole word as UniDic splits it.

The built-in exceptions also hold what the sites in the Latin alphabet write in a fixed way, found by comparing the
romaji of the Japanese titles with the English titles of the same works: the magazines (`失楽天` is `shitsurakuten`,
`ビースト` is `beast`, `山賊版` is `sanzokuban`, `オルガ` is `orga`), the tags (`DL版` is `digital`, `中国翻訳` is `chinese`),
and common words (`エロ` is `ero`, `コスプレ` is `cosplay`, `えっち` is `ecchi`, `お姉さん` is `oneesan`, `お母さん` is `okaasan`).

A small tsu that UniDic takes for a symbol, before kana (`た～っぷり`), sticks to the next word and doubles its first letter
(`ta~ppuri`).

An English word followed by a suffix gets a space (`maid san`); cutlet would write `maidsan`.

### 5. The issue of a magazine

`2024年8月号` is written `2024-08` (year, a dash, the month in two digits), the way the sites that use the Latin
alphabet write it (`COMIC Kairakuten 2024-08`). Only `<4 digits>年<month>月号` is changed; `8月号` and `2024年8月` are read
as words. A search still finds it by the numbers: `2024 8` is a search for the runs of digits 2024 and 8.

### 6. A name in brackets written in kana is one word

A circle or an artist in `[ ]`, `( )` or `【 】`, written in kana, is one word to the sites that use the Latin alphabet
(`[Hitsujida (Kumadela)]`), but UniDic cuts it into the words it knows (`ひつじ / だ`, `くま / でら`). Inside such
brackets, the pieces that are next to each other and are all hiragana (or all katakana) are joined, if together they
are at most 8 kana: `[ひつじだ (くまでら)]` is `[hitsujida (kumadera)]`, `[むーにゃん]` is `[mu-nyan]`.

It is left alone when: the piece is a foreign word (`(ブルーアーカイブ)` is `blue archive`), the pieces are of two scripts
(`ちぃず` `オレ`), the text is not in brackets (`ひつじだとくまでら`) or is in a quotation (`「 」`, which is a sentence), or it is longer
than a name. The names of the magazines of the catalogue are in the exceptions instead (`失楽天` is `shitsurakuten`,
`ビースト` is `beast`, `山賊版` is `sanzokuban`).

Matching does not need the spaces (the title key has none), so this is about the output looking like the titles of
the other sites; it measured 0.77 to 0.81 whole words of the romaji found as words of the English title.

### 7. The output

NFKC-normalized input, lowercase output, single spaces. A search does not care about the case, and cutlet
capitalizes proper nouns.

## How close it is to the Python version

The 9,788 work and issue titles of the okazuri_bot catalogue were converted by both servers
(`loanwords=romaji`): **91% are exactly the same**. Most of the rest are the dictionary:

- UniDic 2.1.2 reads `話` after a number as `wa` (第1話 is `dai1wa`; the Python server says `dai1 hanashi`),
  `日` as `nichi`, `新妻` as `niizuma`, `転生` as `tensei`: here it is the better reading.
- It cuts a katakana name that is not in the dictionary into words that are (クリベロン into クリ + ベロン),
  where UniDic 2.3.0 keeps it whole. Such a name goes in the exceptions (クリベロン is there).
- A rare kanji or a name is read differently.

`tests/fixtures/golden.tsv` has the cases the two agree on, and the examples this is made for.

## Known limits

- A rare kanji in a name is read wrong (UniDic does not have the reading). `莉愛` is `lee ai`.
- A word UniDic does not have, written in hiragana without a `ー` or a small kana, is cut at wrong places.
- Only the words that UniDic lists as foreign get an English spelling, and it can be wrong (ラーメン, メイド,
  ナン): see the exceptions.
- A katakana name that UniDic 2.1.2 splits into dictionary words is read as those words.
- UniDic 2.1.2 splits digits and Latin letters one by one (`73` is `7` `3`). The rules for ASCII words and
  digits put them back together, using where the white space was in the text.
