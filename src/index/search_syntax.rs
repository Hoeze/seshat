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
        TermQuery,
    },
    schema::{Field, IndexRecordOption},
    tokenizer::{TextAnalyzer, TokenStream},
    TantivyError, Term,
};

type Clauses = Vec<(Occur, Box<dyn Query>)>;

/// A shorter last word only matches as a whole word. Almost every message
/// has a word that starts with a given character.
const MIN_PREFIX_CHARS: usize = 2;

/// The words an event has to contain for a match inside words to be real,
/// one list per alternative of the search term. An event that holds every
/// word of any one list contains what was searched for.
pub(crate) type Needles = Vec<Vec<String>>;

/// Matches beyond the exact words, for a search that runs while the user
/// types.
pub(crate) struct Fuzziness {
    /// The text fields to match prefixes and typos in.
    pub(crate) fields: Vec<Field>,
    /// Splits and normalizes a word like the indexed text, without stemming.
    pub(crate) normalizer: TextAnalyzer,
    /// The field that holds the text as groups of characters.
    pub(crate) substring_field: Field,
    /// Cuts text into the same groups of characters as that field.
    pub(crate) grams: TextAnalyzer,
    /// Normalizes a whole text the way those groups are normalized.
    pub(crate) folder: TextAnalyzer,
    /// The last word also matches as the start of a word.
    pub(crate) prefix: bool,
    /// Words also match with typos.
    pub(crate) typos: bool,
    /// Words also match inside words.
    pub(crate) substring: bool,
    /// Whether a word that matches inside words has to appear in the text of
    /// the event. A stemmer or a typo makes a match that doesn't, so both
    /// turn this off.
    pub(crate) verify: bool,
    /// The alternatives that are parsed already. Starts empty.
    pub(crate) needles: Needles,
    /// The alternative that is being parsed. Starts empty.
    pub(crate) current: Vec<String>,
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

    /// The text lowercased and without accents.
    fn fold(&mut self, text: &str) -> String {
        let mut stream = self.folder.token_stream(text);
        match stream.next() {
            Some(token) => token.text.clone(),
            None => String::new(),
        }
    }

    /// Remember that the text has to appear in an event, if a match inside
    /// words really does mean that it appears.
    ///
    /// Only text that survives the tokenizer as one token qualifies. Text
    /// the tokenizer cuts matches events that write it differently, and
    /// those don't hold the text itself: "e-mail" also matches "e mail",
    /// the phrase `"one two"` also matches "one, two", and CJK text becomes
    /// pairs of characters. Those go unchecked, and keep the events that
    /// hold their groups of characters in another order.
    fn needle(&mut self, text: &str, tokens: &[String]) {
        if !self.verify {
            return;
        }

        let folded = self.fold(text);

        if tokens.len() == 1 && tokens[0] == folded {
            self.current.push(folded);
        }
    }

    /// Close the alternative that is being parsed.
    fn close(&mut self) {
        self.needles.push(mem::take(&mut self.current));
    }

    /// The words to check the text of an event against.
    ///
    /// Empty when nothing can be checked, so that a search that matches
    /// inside words without a word to check keeps every event it finds.
    ///
    /// One alternative without a word does the same for the whole term,
    /// because nothing says which alternative an event matched. So
    /// `form or "es clu"` keeps what `form` on its own would drop. Keeping
    /// an event too many beats dropping one the user searched for.
    pub(crate) fn take_needles(&mut self) -> Needles {
        let needles = mem::take(&mut self.needles);

        if needles.iter().all(|alternative| alternative.is_empty()) {
            return Needles::new();
        }

        needles
    }

    /// The groups of characters of the text, without duplicates. Text that
    /// is shorter than one group has none.
    fn grams(&mut self, text: &str) -> Vec<String> {
        let mut stream = self.grams.token_stream(text);
        let mut grams = Vec::new();
        while let Some(token) = stream.next() {
            grams.push(token.text.clone());
        }
        grams.sort();
        grams.dedup();
        grams
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
/// With `fuzziness`, words may also match as a prefix or with typos. A typo
/// needs a word the tokenizer keeps whole, so "e-mail" and CJK text match
/// exactly. Phrases and excluded words always match exactly as well.
///
/// The word at the end is the one the user is typing, unless the term ends
/// with whitespace, so `or` and `and` are ordinary words there.
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
            let exclude = mem::take(&mut exclude);
            // A phrase matches as text as well, spaces and punctuation
            // included, so `"es clu"` finds "Kubernetes cluster".
            let inside = fuzziness.as_deref_mut().filter(|_| !exclude);
            add_clause(parser, &mut clauses, part, exclude, inside)?;
            continue;
        }

        let words: Vec<&str> = part.split_whitespace().collect();

        for (j, word) in words.iter().enumerate() {
            // The user may still be typing the last word, unless the term
            // ends with whitespace.
            let last = i == parts.len() - 1
                && j == words.len() - 1
                && !part.ends_with(char::is_whitespace);

            // A word that is still being typed is the start of a word, not
            // an operator. Otherwise the results vanish on the second
            // keystroke of "orange" and the third of "android".
            let typing = last && fuzziness.as_deref().is_some_and(|f| f.prefix);

            let stripped = word.trim_start_matches('-');
            exclude |= stripped.len() != word.len();

            if stripped.is_empty() {
                // A lone `-` excludes what comes next, e.g. a phrase.
                continue;
            } else if !exclude && !typing && stripped.eq_ignore_ascii_case("or") {
                add_alternative(&mut alternatives, &mut clauses, fuzziness.as_deref_mut());
            } else if !exclude && !typing && stripped.eq_ignore_ascii_case("and") {
                continue;
            } else if let Some(fuzziness) = fuzziness.as_deref_mut().filter(|_| !exclude) {
                add_word(parser, &mut clauses, stripped, fuzziness, last)?;
            } else {
                add_clause(
                    parser,
                    &mut clauses,
                    stripped,
                    mem::take(&mut exclude),
                    None,
                )?;
            }
        }
    }

    add_alternative(&mut alternatives, &mut clauses, fuzziness);

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
    inside: Option<&mut Fuzziness>,
) -> Result<(), TantivyError> {
    let query = exact_query(parser, text)?;
    let empty = query.downcast_ref::<EmptyQuery>().is_some();

    let mut matches = Clauses::new();

    if !empty {
        matches.push((Occur::Should, query));
    }

    if let Some(fuzziness) = inside {
        if let Some(query) = substring_query(fuzziness, text) {
            // The groups of a phrase can sit in an event that doesn't hold
            // the phrase, so remember it and check the text later.
            let tokens = fuzziness.tokens(text);
            fuzziness.needle(text, &tokens);
            matches.push((Occur::Should, query));
        }
    }

    let occur = if exclude { Occur::MustNot } else { Occur::Must };

    // Text without any tokens, like ":)", doesn't restrict the search.
    match matches.len() {
        0 => (),
        1 => clauses.push((occur, matches.remove(0).1)),
        _ => clauses.push((occur, Box::new(BooleanQuery::new(matches)))),
    }

    Ok(())
}

/// Match the text inside the text of an event, as groups of characters.
///
/// A group doesn't know where it sits, so this finds every event that
/// contains the text, plus a few that only contain all of its groups.
fn substring_query(fuzziness: &mut Fuzziness, text: &str) -> Option<Box<dyn Query>> {
    if !fuzziness.substring {
        return None;
    }

    let grams = fuzziness.grams(text);

    if grams.is_empty() {
        return None;
    }

    let clauses: Clauses = grams
        .iter()
        .map(|gram| {
            let term = Term::from_field_text(fuzziness.substring_field, gram);
            let query: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            (Occur::Must, query)
        })
        .collect();

    Some(Box::new(BooleanQuery::new(clauses)))
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
        // A word of several tokens that isn't the one being typed matches
        // exactly. Tantivy has no fuzzy phrase query, so a typo in "e-mail"
        // or in CJK text finds nothing.
        _ => (),
    }

    if let Some(query) = substring_query(fuzziness, text) {
        // The groups of a word can sit in an event that doesn't hold the
        // word, so remember the word and check the text later.
        fuzziness.needle(text, &tokens);
        matches.push((Occur::Should, query));
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
fn add_alternative(
    alternatives: &mut Clauses,
    clauses: &mut Clauses,
    fuzziness: Option<&mut Fuzziness>,
) {
    if clauses.is_empty() {
        return;
    }

    let alternative = BooleanQuery::new(mem::take(clauses));
    alternatives.push((Occur::Should, Box::new(alternative)));

    if let Some(fuzziness) = fuzziness {
        fuzziness.close();
    }
}
