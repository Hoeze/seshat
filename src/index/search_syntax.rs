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
    query::{BooleanQuery, EmptyQuery, Occur, Query, QueryParser},
    TantivyError,
};

type Clauses = Vec<(Occur, Box<dyn Query>)>;

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
pub(crate) fn parse(parser: &QueryParser, term: &str) -> Result<Box<dyn Query>, TantivyError> {
    let mut alternatives = Clauses::new();
    let mut clauses = Clauses::new();
    let mut exclude = false;

    for (i, part) in term.split('"').enumerate() {
        // Every second part was between quotes.
        if i % 2 == 1 {
            add_clause(parser, &mut clauses, part, mem::take(&mut exclude))?;
            continue;
        }

        for word in part.split_whitespace() {
            let stripped = word.trim_start_matches('-');
            exclude |= stripped.len() != word.len();

            if stripped.is_empty() {
                // A lone `-` excludes what comes next, e.g. a phrase.
                continue;
            } else if !exclude && stripped.eq_ignore_ascii_case("or") {
                add_alternative(&mut alternatives, &mut clauses);
            } else if !exclude && stripped.eq_ignore_ascii_case("and") {
                continue;
            } else {
                add_clause(parser, &mut clauses, stripped, mem::take(&mut exclude))?;
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

/// Add a word or phrase to the current alternative.
fn add_clause(
    parser: &QueryParser,
    clauses: &mut Clauses,
    text: &str,
    exclude: bool,
) -> Result<(), TantivyError> {
    // Inside quotes the query parser has no operators, so it takes the text
    // literally. Text with several tokens, like "e-mail", becomes a phrase.
    let query = parser.parse_query(&format!("\"{}\"", text.replace('\\', "\\\\")))?;

    // Text without any tokens, like ":)", doesn't restrict the search.
    if query.downcast_ref::<EmptyQuery>().is_none() {
        let occur = if exclude { Occur::MustNot } else { Occur::Must };
        clauses.push((occur, query));
    }

    Ok(())
}

/// Close the current alternative, the next clauses start a new one.
fn add_alternative(alternatives: &mut Clauses, clauses: &mut Clauses) {
    if !clauses.is_empty() {
        let alternative = BooleanQuery::new(mem::take(clauses));
        alternatives.push((Occur::Should, Box::new(alternative)));
    }
}
