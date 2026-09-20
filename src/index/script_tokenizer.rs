// Copyright 2026 Florian R. Hölzlwimmer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A tokenizer that splits text by writing system.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// Splits text into words, and Chinese, Japanese and Korean text into
/// overlapping pairs of characters.
///
/// Those languages write without spaces between words, so a word tokenizer
/// turns a whole sentence into a single token, and a search for a word in it
/// finds nothing. Pairs of characters make the text searchable without a
/// dictionary for every language. Lucene, Elasticsearch and Solr do the same
/// with their CJK bigram filter.
///
/// Pairs stay inside a run of the same script, so Japanese text keeps its
/// kanji and kana apart. A run of a single character becomes one token.
#[derive(Clone, Default)]
pub(crate) struct ScriptTokenizer;

impl Tokenizer for ScriptTokenizer {
    type TokenStream<'a> = ScriptTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        ScriptTokenStream {
            tokens: tokenize(text),
            index: 0,
        }
    }
}

pub(crate) struct ScriptTokenStream {
    tokens: Vec<Token>,
    index: usize,
}

impl TokenStream for ScriptTokenStream {
    fn advance(&mut self) -> bool {
        if self.index >= self.tokens.len() {
            return false;
        }

        self.index += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.tokens[self.index.saturating_sub(1)]
    }

    fn token_mut(&mut self) -> &mut Token {
        let index = self.index.saturating_sub(1);
        &mut self.tokens[index]
    }
}

/// The writing systems that don't separate words with spaces.
#[derive(Clone, Copy, PartialEq)]
enum Script {
    Han,
    Hiragana,
    Katakana,
    Hangul,
}

fn script_of(c: char) -> Option<Script> {
    match c {
        '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{2FA1F}' => Some(Script::Han),
        '\u{3040}'..='\u{309F}' => Some(Script::Hiragana),
        '\u{30A0}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' => Some(Script::Katakana),
        '\u{1100}'..='\u{11FF}'
        | '\u{3130}'..='\u{318F}'
        | '\u{A960}'..='\u{A97F}'
        | '\u{AC00}'..='\u{D7FF}' => Some(Script::Hangul),
        _ => None,
    }
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut position = 0;
    // The characters of the current run, with their byte offsets.
    let mut run: Vec<(usize, char)> = Vec::new();
    let mut script = None;

    for (offset, c) in text.char_indices() {
        let c_script = script_of(c);

        // A separator, or the start of a different script, ends the run.
        if !c.is_alphanumeric() || (!run.is_empty() && c_script != script) {
            if !run.is_empty() {
                flush(&mut tokens, &mut run, script, &mut position);
            }

            if !c.is_alphanumeric() {
                continue;
            }
        }

        script = c_script;
        run.push((offset, c));
    }

    if !run.is_empty() {
        flush(&mut tokens, &mut run, script, &mut position);
    }

    tokens
}

/// Turn the characters of a finished run into tokens.
fn flush(
    tokens: &mut Vec<Token>,
    run: &mut Vec<(usize, char)>,
    script: Option<Script>,
    position: &mut usize,
) {
    let length = run.len();

    match script {
        // A word of a script that separates words with spaces.
        None => add(tokens, run, 0, length, position),
        // Chinese, Japanese or Korean: overlapping pairs of characters.
        Some(_) if length == 1 => add(tokens, run, 0, 1, position),
        Some(_) => {
            for start in 0..length - 1 {
                add(tokens, run, start, start + 2, position);
            }
        }
    }

    run.clear();
}

/// Add the characters `[from, to)` of the run as one token.
fn add(
    tokens: &mut Vec<Token>,
    run: &[(usize, char)],
    from: usize,
    to: usize,
    position: &mut usize,
) {
    let (offset_from, _) = run[from];
    let (last_offset, last_char) = run[to - 1];

    tokens.push(Token {
        offset_from,
        offset_to: last_offset + last_char.len_utf8(),
        position: *position,
        text: run[from..to].iter().map(|(_, c)| c).collect(),
        position_length: 1,
    });

    *position += 1;
}

#[cfg(test)]
fn texts(text: &str) -> Vec<String> {
    tokenize(text).into_iter().map(|token| token.text).collect()
}

#[test]
fn words_stay_words() {
    assert_eq!(
        texts("Our Kubernetes cluster"),
        ["Our", "Kubernetes", "cluster"]
    );
    assert_eq!(texts("e-mail v2.3.1"), ["e", "mail", "v2", "3", "1"]);
    assert_eq!(texts("Straße, Café!"), ["Straße", "Café"]);
}

#[test]
fn cjk_text_becomes_pairs() {
    // Chinese: one run of five characters.
    assert_eq!(texts("我是中国人"), ["我是", "是中", "中国", "国人"]);

    // Japanese: kanji, katakana and hiragana are separate runs.
    assert_eq!(
        texts("東京タワーに行きました"),
        ["東京", "タワ", "ワー", "に", "行", "きま", "まし", "した"]
    );

    // Korean.
    assert_eq!(texts("안녕하세요"), ["안녕", "녕하", "하세", "세요"]);
}

#[test]
fn scripts_are_mixed_in_one_text() {
    assert_eq!(texts("東京 is great"), ["東京", "is", "great"]);
    assert_eq!(
        texts("Kubernetes 클러스터"),
        ["Kubernetes", "클러", "러스", "스터"]
    );
}

#[test]
fn positions_follow_each_other() {
    let tokens = tokenize("東京タワー is great");
    let positions: Vec<usize> = tokens.iter().map(|token| token.position).collect();
    assert_eq!(positions, [0, 1, 2, 3, 4]);

    // The offsets point back at the original text.
    let text = "東京タワー is";
    for token in tokenize(text) {
        assert_eq!(text[token.offset_from..token.offset_to], token.text);
    }
}
