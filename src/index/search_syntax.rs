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

//! The default search syntax. It is the syntax of Synapse's server-side
//! search, so that Element can send the same search term to both.

use std::mem;

use tantivy::{
    query::{
        BooleanQuery, EmptyQuery, FuzzyTermQuery, Occur, PhrasePrefixQuery, Query, QueryParser,
    },
    schema::Field,
    tokenizer::{TextAnalyzer, TokenStream},
    TantivyError, Term,
};

type Clauses = Vec<(Occur, Box<dyn Query>)>;

/// A shorter last word only matches as a whole word. Almost every message
/// has a word that starts with a given character.
const MIN_PREFIX_CHARS: usize = 2;

/// Matches beyond the exact words, for a search that runs while the user
/// types.
pub(crate) struct Fuzziness {
    /// The text fields to match prefixes and typos in.
    pub(crate) fields: Vec<Field>,
    /// Splits and normalizes a word like the indexed text, without stemming.
    pub(crate) normalizer: TextAnalyzer,
    /// The last word also matches as the start of a word.
    pub(crate) prefix: bool,
    /// Words also match with typos.
    pub(crate) typos: bool,
}

impl Fuzziness {
    fn tokens(&mut self, text: &str) -> Vec<String> {
        let mut stream = self.normalizer.token_stream(text);
        let mut tokens = Vec::new();
        while let Some(token) = stream.next() {
            tokens.push(token.text.clone());
        }
        tokens
    }
}

/// Parse a search term with the syntax of Synapse's server-side search.
///
/// * All words and phrases have to match.
/// * Text in double quotes is a phrase. An unclosed quote makes the rest of
///   the term a phrase.
/// * A leading `-` excludes the next word or phrase.
/// * `or` separates alternatives, `and` is optional. Both are
///   case-insensitive.
///
/// Everything else is text, so no search term is a syntax error. Unlike
/// Synapse, words are only split at whitespace. The tokenizer of the index
/// handles punctuation, the same way it does for the indexed text.
///
/// With `fuzziness`, words may also match as a prefix or with typos. Phrases
/// and excluded words always match exactly.
pub(crate) fn parse(
    parser: &QueryParser,
    term: &str,
    mut fuzziness: Option<&mut Fuzziness>,
) -> Result<Box<dyn Query>, TantivyError> {
    let mut alternatives = Clauses::new();
    let mut clauses = Clauses::new();
    let mut exclude = false;

    let parts: Vec<&str> = term.split('"').collect();

    for (i, part) in parts.iter().enumerate() {
        // Every second part was between quotes.
        if i % 2 == 1 {
            add_clause(parser, &mut clauses, part, mem::take(&mut exclude))?;
            continue;
        }

        let words: Vec<&str> = part.split_whitespace().collect();

        for (j, word) in words.iter().enumerate() {
            // The user may still be typing the last word, unless the term
            // ends with whitespace.
            let last = i == parts.len() - 1
                && j == words.len() - 1
                && !part.ends_with(char::is_whitespace);

            let stripped = word.trim_start_matches('-');
            exclude |= stripped.len() != word.len();

            if stripped.is_empty() {
                // A lone `-` excludes what comes next, e.g. a phrase.
                continue;
            } else if !exclude && stripped.eq_ignore_ascii_case("or") {
                add_alternative(&mut alternatives, &mut clauses);
            } else if !exclude && stripped.eq_ignore_ascii_case("and") {
                continue;
            } else if exclude {
                add_clause(parser, &mut clauses, stripped, mem::take(&mut exclude))?;
            } else if let Some(fuzziness) = fuzziness.as_deref_mut() {
                add_word(parser, &mut clauses, stripped, fuzziness, last)?;
            } else {
                add_clause(parser, &mut clauses, stripped, false)?;
            }
        }
    }

    add_alternative(&mut alternatives, &mut clauses);

    Ok(match alternatives.len() {
        0 => Box::new(EmptyQuery),
        1 => alternatives.remove(0).1,
        _ => Box::new(BooleanQuery::new(alternatives)),
    })
}

/// Parse text as an exact word or phrase.
fn exact_query(parser: &QueryParser, text: &str) -> Result<Box<dyn Query>, TantivyError> {
    // Inside quotes the query parser has no operators, so it takes the text
    // literally. Text with several tokens, like "e-mail", becomes a phrase.
    Ok(parser.parse_query(&format!("\"{}\"", text.replace('\\', "\\\\")))?)
}

/// Add a word or phrase to the current alternative.
fn add_clause(
    parser: &QueryParser,
    clauses: &mut Clauses,
    text: &str,
    exclude: bool,
) -> Result<(), TantivyError> {
    let query = exact_query(parser, text)?;

    // Text without any tokens, like ":)", doesn't restrict the search.
    if query.downcast_ref::<EmptyQuery>().is_none() {
        let occur = if exclude { Occur::MustNot } else { Occur::Must };
        clauses.push((occur, query));
    }

    Ok(())
}

/// Add a word that matches exactly, or as a prefix or with typos.
fn add_word(
    parser: &QueryParser,
    clauses: &mut Clauses,
    text: &str,
    fuzziness: &mut Fuzziness,
    last: bool,
) -> Result<(), TantivyError> {
    let exact = exact_query(parser, text)?;
    let tokens = fuzziness.tokens(text);
    let prefix = last
        && fuzziness.prefix
        && tokens
            .last()
            .is_some_and(|token| token.chars().count() >= MIN_PREFIX_CHARS);

    let mut matches = Clauses::new();

    if exact.downcast_ref::<EmptyQuery>().is_none() {
        matches.push((Occur::Should, exact));
    }

    match tokens.as_slice() {
        [token] => {
            let distance = if fuzziness.typos {
                typo_distance(token)
            } else {
                0
            };

            for field in &fuzziness.fields {
                let term = Term::from_field_text(*field, token);
                let query = if prefix {
                    FuzzyTermQuery::new_prefix(term, distance, true)
                } else if distance > 0 {
                    FuzzyTermQuery::new(term, distance, true)
                } else {
                    continue;
                };
                matches.push((Occur::Should, Box::new(query)));
            }
        }
        // A word with several tokens, like "e-ma", is a phrase whose last
        // token is a prefix.
        [_, _, ..] if prefix => {
            for field in &fuzziness.fields {
                let terms = tokens
                    .iter()
                    .map(|token| Term::from_field_text(*field, token))
                    .collect();
                matches.push((Occur::Should, Box::new(PhrasePrefixQuery::new(terms))));
            }
        }
        _ => (),
    }

    match matches.len() {
        // Text without any tokens, like ":)", doesn't restrict the search.
        0 => (),
        1 => clauses.push((Occur::Must, matches.remove(0).1)),
        _ => clauses.push((Occur::Must, Box::new(BooleanQuery::new(matches)))),
    }

    Ok(())
}

/// How many typos a word may have: none up to 4 characters, one for 5 to 8
/// characters, and two from 9 characters on. These are Meilisearch's
/// defaults.
fn typo_distance(word: &str) -> u8 {
    match word.chars().count() {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

/// Close the current alternative, the next clauses start a new one.
fn add_alternative(alternatives: &mut Clauses, clauses: &mut Clauses) {
    if !clauses.is_empty() {
        let alternative = BooleanQuery::new(mem::take(clauses));
        alternatives.push((Occur::Should, Box::new(alternative)));
    }
}
