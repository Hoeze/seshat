// Copyright 2019 The Matrix.org Foundation C.I.C.
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

#[cfg(feature = "encryption")]
mod encrypted_dir;
#[cfg(feature = "encryption")]
mod encrypted_stream;
mod search_syntax;

use std::{
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};

use lru_cache::LruCache;
use tantivy as tv;
use tantivy::{
    collector::{Count, MultiCollector, TopDocs},
    schema::Value,
    Order, Term,
};
use uuid::Uuid;

#[cfg(feature = "encryption")]
use crate::index::encrypted_dir::{EncryptedMmapDirectory, PBKDF_COUNT};
use crate::{
    config::{Config, Language, SearchConfig, TokenizerMode},
    events::{Event, EventId, EventType},
};

// Tantivy requires at least 15MB of heap per writer thread and returns an
// error if we give it less. We use a single writer thread with 50MB of heap.
const TANTIVY_WRITER_HEAP_SIZE: usize = 50_000_000;

// Tantivy doesn't behave nicely if `commit()` is called too often on the index
// writer. A commit means that Tantivy will spawn threads that will try to merge
// index segments together, that is, it tries to merge a bunch of smaller files
// into one larger.
//
// If users call commit faster than those threads manage to merge the segments
// the number of spawned threads keeps on increasing.
//
// To mitigate this we limit the commit rate using the following constants. We
// either wait for 500 events to be queued up or wait 5 seconds.
//
// Those constants have been picked empirically by running the database example.
// The COMMIT_TIME is fairly conservative. This does mean that users will have
// to wait 5 seconds before they will manage to see search results for newly
// added events.

/// How many events should we add to the index before we are allowed to commit.
const COMMIT_RATE: usize = 500;
/// How long should we wait between commits if there aren't enough events
/// committed.
const COMMIT_TIME: Duration = Duration::from_secs(5);

/// How many searches should be cached so pagination is supported.
const SEARCH_CACHE_SIZE: usize = 100;
/// How much should the result limit increase every time we need to find more
/// results due to a paginated search.
const SEARCH_LIMIT_INCREMENT: usize = 50;

#[cfg(test)]
use tempfile::TempDir;

#[cfg(test)]
use crate::events::{EVENT, TOPIC_EVENT};

pub(crate) struct Index {
    index: tv::Index,
    reader: tv::IndexReader,
    body_field: tv::schema::Field,
    topic_field: tv::schema::Field,
    name_field: tv::schema::Field,
    event_id_field: tv::schema::Field,
    sender_field: tv::schema::Field,
    date_field: tv::schema::Field,
    room_id_field: tv::schema::Field,
    search_cache: Arc<RwLock<LruCache<Uuid, Search>>>,
}

#[derive(Clone)]
struct Search {
    search_term: Arc<String>,
    search_config: Arc<SearchConfig>,
    event_ids: Arc<Vec<String>>,
}

#[derive(Debug)]
pub(crate) struct SearchResult {
    pub(crate) count: usize,
    pub(crate) results: Vec<(f32, EventId)>,
    pub(crate) next_batch: Option<Uuid>,
}

/// One page of a search of the index.
struct Page {
    /// How many events the query matched.
    count: usize,
    /// The events of this page, with their score.
    results: Vec<(f32, EventId)>,
    /// The ids of those events, to skip them on the next page.
    event_ids: Vec<EventId>,
}

pub(crate) struct Writer {
    inner: tv::IndexWriter,
    body_field: tv::schema::Field,
    topic_field: tv::schema::Field,
    name_field: tv::schema::Field,
    event_id_field: tv::schema::Field,
    sender_field: tv::schema::Field,
    date_field: tv::schema::Field,

    /// Number of events added or deleted since the last commit
    events_pending_commit: usize,

    commit_timestamp: std::time::Instant,
    room_id_field: tv::schema::Field,

    /// Commit once this many events are pending, or after this much time.
    commit_rate: usize,
    commit_time: Duration,
}

impl Writer {
    pub fn commit(&mut self) -> Result<bool, tv::TantivyError> {
        self.commit_helper(false)
    }

    fn commit_helper(&mut self, force: bool) -> Result<bool, tv::TantivyError> {
        if self.events_pending_commit > 0
            && (force
                || self.events_pending_commit >= self.commit_rate
                || self.commit_timestamp.elapsed() >= self.commit_time)
        {
            self.inner.commit()?;
            self.events_pending_commit = 0;
            self.commit_timestamp = std::time::Instant::now();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn force_commit(&mut self) -> Result<(), tv::TantivyError> {
        self.commit_helper(true)?;
        Ok(())
    }

    /// Change how often `commit()` commits, instead of `COMMIT_RATE` and
    /// `COMMIT_TIME`.
    ///
    /// # Arguments
    ///
    /// * `events` - Commit once this many events are pending.
    /// * `time` - Commit once this much time passed since the last commit.
    pub fn set_commit_rate(&mut self, events: usize, time: Duration) {
        self.commit_rate = events;
        self.commit_time = time;
    }

    pub fn add_event(&mut self, event: &Event) -> Result<(), tv::TantivyError> {
        let mut doc = tv::TantivyDocument::default();

        match event.event_type {
            EventType::Message => doc.add_text(self.body_field, &event.content_value),
            EventType::Topic => doc.add_text(self.topic_field, &event.content_value),
            EventType::Name => doc.add_text(self.name_field, &event.content_value),
        }

        doc.add_text(self.event_id_field, &event.event_id);
        doc.add_text(self.room_id_field, &event.room_id);
        doc.add_text(self.sender_field, &event.sender);
        doc.add_u64(self.date_field, event.server_ts as u64);

        self.inner.add_document(doc)?;
        self.events_pending_commit += 1;

        Ok(())
    }

    /// Delete the event with the given event id from the index.
    pub fn delete_event(&mut self, event_id: &str) {
        let term = Term::from_field_text(self.event_id_field, event_id);
        self.inner.delete_term(term);
        self.events_pending_commit += 1;
    }

    pub fn wait_merging_threads(self) -> Result<(), tv::TantivyError> {
        self.inner.wait_merging_threads()
    }
}

pub(crate) struct IndexSearcher {
    inner: tv::Searcher,
    schema: tv::schema::Schema,
    tokenizer: tv::tokenizer::TokenizerManager,
    body_field: tv::schema::Field,
    topic_field: tv::schema::Field,
    name_field: tv::schema::Field,
    room_id_field: tv::schema::Field,
    #[allow(dead_code)]
    sender_field: tv::schema::Field,
    date_field: tv::schema::Field,
    event_id_field: tv::schema::Field,
    search_cache: Arc<RwLock<LruCache<Uuid, Search>>>,
}

impl IndexSearcher {
    fn parse_query(
        &self,
        term: &str,
        config: &SearchConfig,
    ) -> Result<Box<dyn tv::query::Query>, tv::TantivyError> {
        let mut keys = Vec::new();

        if config.keys.is_empty() {
            keys.append(&mut vec![
                self.body_field,
                self.topic_field,
                self.name_field,
            ]);
        } else {
            for key in config.keys.iter() {
                match key {
                    EventType::Message => keys.push(self.body_field),
                    EventType::Topic => keys.push(self.topic_field),
                    EventType::Name => keys.push(self.name_field),
                }
            }
        }

        let query: Box<dyn tv::query::Query> = if term.is_empty() {
            Box::new(tv::query::AllQuery)
        } else {
            let mut query_parser =
                tv::query::QueryParser::new(self.schema.clone(), keys, self.tokenizer.clone());

            if config.query_syntax {
                // All words have to match in both syntaxes.
                query_parser.set_conjunction_by_default();
                query_parser.parse_query(term)?
            } else {
                search_syntax::parse(&query_parser, term)?
            }
        };

        // Add the room filter as a query, not as query syntax, so that the
        // search term can't change what the filter means.
        Ok(match &config.room_id {
            Some(room) => {
                let room = tv::query::TermQuery::new(
                    Term::from_field_text(self.room_id_field, room),
                    tv::schema::IndexRecordOption::Basic,
                );
                Box::new(tv::query::BooleanQuery::new(vec![
                    (tv::query::Occur::Must, Box::new(room)),
                    (tv::query::Occur::Must, query),
                ]))
            }
            None => query,
        })
    }

    fn search_helper(
        &self,
        og_limit: usize,
        limit: usize,
        order_by_recency: bool,
        previous_results: &[EventId],
        query: &dyn tv::query::Query,
    ) -> Result<Page, tv::TantivyError> {
        let mut multicollector = MultiCollector::new();
        let count_handle = multicollector.add_collector(Count);

        let (mut result, top_docs) = if order_by_recency {
            let date_field = self.schema.get_field_name(self.date_field);
            let top_docs_handle = multicollector.add_collector(
                TopDocs::with_limit(limit).order_by_u64_field(date_field, Order::Desc),
            );

            let mut result = self.inner.search(query, &multicollector)?;
            let mut top_docs = top_docs_handle.extract(&mut result);
            (
                result,
                top_docs
                    .drain(..)
                    .map(|(_, address)| (1.0, address))
                    .collect(),
            )
        } else {
            let top_docs_handle =
                multicollector.add_collector(TopDocs::with_limit(limit).order_by_score());
            let mut result = self.inner.search(query, &multicollector)?;

            let top_docs = top_docs_handle.extract(&mut result);
            (result, top_docs)
        };

        let mut docs = Vec::new();
        let mut event_ids = Vec::new();

        let count = count_handle.extract(&mut result);

        let end = count == top_docs.len();

        for (score, docaddress) in top_docs {
            let doc = match self.inner.doc::<tv::TantivyDocument>(docaddress) {
                Ok(d) => d,
                Err(_e) => continue,
            };

            let event_id: EventId = match doc.get_first(self.event_id_field) {
                Some(s) => s.as_str().unwrap().to_owned(),
                None => continue,
            };

            // Skip results that were already returned in a previous search.
            if previous_results.contains(&event_id) {
                continue;
            }

            event_ids.push(event_id.clone());
            docs.push((score, event_id));

            if docs.len() >= og_limit {
                break;
            }
        }

        // A page that isn't full yet, with matches left to look at, needs a
        // wider search.
        if docs.len() < og_limit && !end {
            return self.search_helper(
                og_limit,
                limit + SEARCH_LIMIT_INCREMENT,
                order_by_recency,
                previous_results,
                query,
            );
        }

        Ok(Page {
            count,
            results: docs,
            event_ids,
        })
    }

    pub fn search(
        &self,
        term: &str,
        config: &SearchConfig,
    ) -> Result<SearchResult, tv::TantivyError> {
        let past_search = if let Some(token) = &config.next_batch {
            let mut search_cache = self.search_cache.write().unwrap();
            search_cache.get_mut(token).cloned()
        } else {
            None
        };

        let (page, term, config) = if let Some(past_search) = past_search {
            let query = self.parse_query(term, &past_search.search_config)?;
            let previous_results = &past_search.event_ids;

            let mut page = self.search_helper(
                config.limit,
                config.limit,
                config.order_by_recency,
                previous_results,
                &query,
            )?;

            // Add the previous results to the current ones.
            page.event_ids.extend(previous_results.iter().cloned());

            (
                page,
                past_search.search_term.clone(),
                past_search.search_config.clone(),
            )
        } else {
            let query = self.parse_query(term, config)?;
            (
                self.search_helper(
                    config.limit,
                    config.limit,
                    config.order_by_recency,
                    &[],
                    &query,
                )?,
                Arc::new(term.to_owned()),
                Arc::new(config.clone()),
            )
        };

        let next_batch = if page.event_ids.len() == page.count {
            None
        } else {
            let mut search_cache = self.search_cache.write().unwrap();
            let search = Search {
                search_term: term,
                search_config: config,
                event_ids: Arc::new(page.event_ids),
            };

            let token = Uuid::new_v4();
            search_cache.insert(token, search);
            Some(token)
        };

        Ok(SearchResult {
            count: page.count,
            results: page.results,
            next_batch,
        })
    }
}

impl Index {
    pub fn new<P: AsRef<Path>>(path: P, config: &Config) -> Result<Index, tv::TantivyError> {
        // Determine tokenizer name based on tokenizer mode
        let tokenizer_name = config.tokenizer_mode.as_tokenizer_name(&config.language);

        let text_field_options = Index::create_text_options(&tokenizer_name);
        let mut schemabuilder = tv::schema::Schema::builder();

        let body_field = schemabuilder.add_text_field("body", text_field_options.clone());
        let topic_field = schemabuilder.add_text_field("topic", text_field_options.clone());
        let name_field = schemabuilder.add_text_field("name", text_field_options);

        let date_field = schemabuilder.add_u64_field("date", tv::schema::FAST);

        let sender_field = schemabuilder.add_text_field("sender", tv::schema::STRING);
        let room_id_field =
            schemabuilder.add_text_field("room_id", tv::schema::STORED | tv::schema::STRING);

        let event_id_field =
            schemabuilder.add_text_field("event_id", tv::schema::STORED | tv::schema::STRING);

        let schema = schemabuilder.build();

        let index = Index::open_index(path, config, schema)?;
        let reader = index.reader()?;

        // Register tokenizer based on mode
        match &config.tokenizer_mode {
            TokenizerMode::Ngram { min_gram, max_gram } => {
                // Lowercase the n-grams, so that search is case-insensitive
                // like in the language-based mode.
                let ngram_tokenizer = tv::tokenizer::TextAnalyzer::builder(
                    tv::tokenizer::NgramTokenizer::new(*min_gram, *max_gram, false)?,
                )
                .filter(tv::tokenizer::LowerCaser)
                .build();
                index
                    .tokenizers()
                    .register(&tokenizer_name, ngram_tokenizer);
            }
            TokenizerMode::LanguageBased => {
                match config.language {
                    Language::Unknown => (), // Use default tokenizer
                    _ => {
                        let tokenizer = tv::tokenizer::TextAnalyzer::builder(
                            tv::tokenizer::SimpleTokenizer::default(),
                        )
                        .filter(tv::tokenizer::RemoveLongFilter::limit(40))
                        .filter(tv::tokenizer::LowerCaser)
                        .filter(tv::tokenizer::Stemmer::new(config.language.as_tantivy()))
                        .build();
                        index.tokenizers().register(&tokenizer_name, tokenizer);
                    }
                }
            }
        }

        Ok(Index {
            index,
            reader,
            body_field,
            topic_field,
            name_field,
            event_id_field,
            sender_field,
            date_field,
            room_id_field,
            search_cache: Arc::new(RwLock::new(LruCache::new(SEARCH_CACHE_SIZE))),
        })
    }

    #[cfg(feature = "encryption")]
    fn open_index<P: AsRef<Path>>(
        path: P,
        config: &Config,
        schema: tv::schema::Schema,
    ) -> tv::Result<tv::Index> {
        match &config.passphrase {
            Some(p) => {
                let dir = EncryptedMmapDirectory::open_or_create(path, p, PBKDF_COUNT)?;
                tv::Index::open_or_create(dir, schema)
            }
            None => {
                let dir = tv::directory::MmapDirectory::open(path)?;
                tv::Index::open_or_create(dir, schema)
            }
        }
    }

    #[cfg(not(feature = "encryption"))]
    fn open_index<P: AsRef<Path>>(
        path: P,
        _config: &Config,
        schema: tv::schema::Schema,
    ) -> tv::Result<tv::Index> {
        let dir = tv::directory::MmapDirectory::open(path)?;
        tv::Index::open_or_create(dir, schema)
    }

    #[cfg(feature = "encryption")]
    pub fn change_passphrase<P: AsRef<Path>>(
        path: P,
        old_passphrase: &str,
        new_passphrase: &str,
    ) -> Result<(), tv::TantivyError> {
        EncryptedMmapDirectory::change_passphrase(
            path,
            old_passphrase,
            new_passphrase,
            PBKDF_COUNT,
        )?;
        Ok(())
    }

    fn create_text_options(tokenizer: &str) -> tv::schema::TextOptions {
        let indexing = tv::schema::TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(tv::schema::IndexRecordOption::WithFreqsAndPositions);
        tv::schema::TextOptions::default().set_indexing_options(indexing)
    }

    pub fn get_searcher(&self) -> IndexSearcher {
        let searcher = self.reader.searcher();
        let schema = self.index.schema();
        let tokenizer = self.index.tokenizers().clone();

        IndexSearcher {
            inner: searcher,
            schema,
            tokenizer,
            body_field: self.body_field,
            topic_field: self.topic_field,
            name_field: self.name_field,
            room_id_field: self.room_id_field,
            sender_field: self.sender_field,
            date_field: self.date_field,
            event_id_field: self.event_id_field,
            search_cache: self.search_cache.clone(),
        }
    }

    pub fn reload(&self) -> Result<(), tv::TantivyError> {
        self.reader.reload()
    }

    pub fn get_writer(&self) -> Result<Writer, tv::TantivyError> {
        Ok(Writer {
            inner: self
                .index
                .writer_with_num_threads(1, TANTIVY_WRITER_HEAP_SIZE)?,
            body_field: self.body_field,
            topic_field: self.topic_field,
            name_field: self.name_field,
            event_id_field: self.event_id_field,
            room_id_field: self.room_id_field,
            sender_field: self.sender_field,
            date_field: self.date_field,
            events_pending_commit: 0,
            commit_timestamp: std::time::Instant::now(),
            commit_rate: COMMIT_RATE,
            commit_time: COMMIT_TIME,
        })
    }
}

#[test]
fn add_an_event() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    writer.add_event(&EVENT).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let result = searcher
        .search("Test", &Default::default())
        .unwrap()
        .results;

    let event_id = EVENT.event_id.to_string();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, event_id)
}

#[test]
fn add_events_to_differing_rooms() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let event_id = EVENT.event_id.to_string();
    let mut writer = index.get_writer().unwrap();

    let mut event2 = EVENT.clone();
    event2.room_id = "!Test2:room".to_string();

    writer.add_event(&EVENT).unwrap();
    writer.add_event(&event2).unwrap();

    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let result = searcher
        .search("Test", SearchConfig::new().for_room(&EVENT.room_id))
        .unwrap()
        .results;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, event_id);

    let result = searcher
        .search("Test", &Default::default())
        .unwrap()
        .results;
    assert_eq!(result.len(), 2);
}

#[test]
fn default_search_syntax() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    let mut event = EVENT.clone();
    event.content_value = "Don't miss https://example.org/a?b=c <3".to_string();
    writer.add_event(&event).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let finds = |term: &str| {
        let result = searcher.search(term, &Default::default());
        assert!(result.is_ok(), "Search for {:?} failed", term);
        !result.unwrap().results.is_empty()
    };

    // Chat input is text, never a syntax error.
    for term in &[
        "don't",
        "https://example.org/a?b=c",
        "<3",
        "\"miss",
        "miss :)",
    ] {
        assert!(finds(term), "Search for {:?} didn't find the event", term);
    }

    // "NOT" is a word here, not an operator.
    assert!(!finds("miss NOT"));

    // All words have to match, unless "or" separates them.
    assert!(finds("miss don't"));
    assert!(finds("miss and don't"));
    assert!(!finds("miss nothing"));
    assert!(finds("nothing OR miss"));

    // Quotes make a phrase, a leading "-" excludes a word or phrase.
    assert!(finds("\"don't miss\""));
    assert!(!finds("\"miss don't\""));
    assert!(finds("miss -nothing"));
    assert!(!finds("miss -don't"));
    assert!(!finds("miss -\"don't miss\""));

    // Terms without anything to match find nothing.
    for term in &["-miss", "->", "-", "or", ":)", "back\\slash", "   "] {
        assert!(!finds(term), "Search for {:?} found the event", term);
    }
}

#[test]
fn query_syntax_search() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    let mut release = EVENT.clone();
    release.event_id = "$release:localhost".to_string();
    release.content_value = "We deploy the release".to_string();

    let mut party = EVENT.clone();
    party.event_id = "$party:localhost".to_string();
    party.content_value = "Release party tonight".to_string();
    party.sender = "@bob:example.org".to_string();

    writer.add_event(&release).unwrap();
    writer.add_event(&party).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let query = |term: &str| {
        searcher
            .search(term, SearchConfig::new().query_syntax(true))
            .map(|r| r.results.into_iter().map(|(_, id)| id).collect::<Vec<_>>())
    };

    assert_eq!(query("release tonight").unwrap(), [party.event_id.clone()]);
    assert_eq!(query("release -party").unwrap(), [release.event_id.clone()]);
    assert_eq!(
        query("\"release party\"").unwrap(),
        [party.event_id.clone()]
    );
    assert_eq!(query("\"release pa\"*").unwrap(), [party.event_id.clone()]);
    assert_eq!(
        query("sender:\"@bob:example.org\"").unwrap(),
        [party.event_id.clone()]
    );
    assert!(query("\"unclosed").is_err());

    // The default syntax has no field filters, "sender:" is text there.
    let result = searcher
        .search("sender:\"@bob:example.org\"", &Default::default())
        .unwrap()
        .results;
    assert!(result.is_empty());
}

#[test]
fn search_term_cannot_escape_the_room_filter() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    let mut other_room = EVENT.clone();
    other_room.event_id = "$other_room_event:localhost".to_string();
    other_room.room_id = "!other:room".to_string();

    writer.add_event(&EVENT).unwrap();
    writer.add_event(&other_room).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let mut config = SearchConfig::new();
    config.for_room(&EVENT.room_id).query_syntax(true);

    // This term used to close the group of the room filter and add an
    // unfiltered clause. On its own, it isn't a valid query.
    assert!(searcher.search("Test) OR (body:Test", &config).is_err());

    // A valid query can't widen the room filter either.
    let result = searcher
        .search("Test OR room_id:\"!other:room\"", &config)
        .unwrap()
        .results;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, EVENT.event_id);
}

#[test]
fn switch_languages() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    writer.add_event(&EVENT).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let result = searcher
        .search("Test", &Default::default())
        .unwrap()
        .results;

    let event_id = EVENT.event_id.to_string();

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, event_id);

    drop(index);

    let config = Config::new().set_language(&Language::German);
    let index = Index::new(&tmpdir, &config);

    assert!(index.is_err())
}

#[test]
fn event_count() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    assert_eq!(writer.events_pending_commit, 0);
    writer.add_event(&EVENT).unwrap();
    assert_eq!(writer.events_pending_commit, 1);

    writer.force_commit().unwrap();
    assert_eq!(writer.events_pending_commit, 0);
}

#[test]
fn custom_commit_rate() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();
    writer.set_commit_rate(2, Duration::from_secs(3600));

    writer.add_event(&EVENT).unwrap();
    assert!(!writer.commit().unwrap());

    writer.add_event(&TOPIC_EVENT).unwrap();
    assert!(writer.commit().unwrap());
}

#[test]
fn delete_an_event() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    writer.add_event(&EVENT).unwrap();
    writer.add_event(&TOPIC_EVENT).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let result = searcher
        .search("Test", &Default::default())
        .unwrap()
        .results;

    let event_id = &EVENT.event_id;

    assert_eq!(result.len(), 2);
    assert_eq!(&result[0].1, event_id);

    writer.delete_event(event_id);
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let result = searcher
        .search("Test", &Default::default())
        .unwrap()
        .results;
    assert_eq!(result.len(), 1);
    assert_eq!(&result[0].1, &TOPIC_EVENT.event_id);
}

#[test]
fn paginated_search() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().set_language(&Language::English);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();

    writer.add_event(&EVENT).unwrap();
    writer.add_event(&TOPIC_EVENT).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();
    let first_search = searcher
        .search("Test", SearchConfig::new().limit(1))
        .unwrap();

    assert_eq!(first_search.results.len(), 1);

    let second_search = searcher
        .search(
            "Test",
            SearchConfig::new()
                .limit(1)
                .next_batch(first_search.next_batch.unwrap()),
        )
        .unwrap();
    assert_eq!(second_search.results.len(), 1);
    assert_eq!(&first_search.results[0].1, &EVENT.event_id);
    assert_eq!(&second_search.results[0].1, &TOPIC_EVENT.event_id);
    assert!(second_search.next_batch.is_none());
}

#[test]
fn ngram_tokenizer_mode() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().use_ngram_tokenizer(2, 4);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut writer = index.get_writer().unwrap();
    writer.add_event(&EVENT).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();

    // Search with partial text (ngram should match)
    let result = searcher.search("est", &Default::default()).unwrap().results;

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].1, EVENT.event_id);
}

#[test]
fn ngram_tokenizer_is_case_insensitive() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().use_ngram_tokenizer(2, 4);
    let index = Index::new(&tmpdir, &config).unwrap();

    let mut event = EVENT.clone();
    event.content_value = "Der Maschinenbau-Kurs nutzt Kubernetes".to_string();

    let mut writer = index.get_writer().unwrap();
    writer.add_event(&event).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();

    for term in &["maschine", "MASCHINE", "kubernetes", "KUBERNETES", "bernet"] {
        let result = searcher.search(term, &Default::default()).unwrap().results;
        assert_eq!(
            result.len(),
            1,
            "Search for {:?} didn't find the event",
            term
        );
    }
}

#[test]
fn schema_mismatch_on_tokenizer_mode_change() {
    let tmpdir = TempDir::new().unwrap();

    // Create index with language-based tokenizer
    {
        let config = Config::new().set_language(&Language::English);
        let index = Index::new(&tmpdir, &config).unwrap();
        let mut writer = index.get_writer().unwrap();
        writer.add_event(&EVENT).unwrap();
        writer.force_commit().unwrap();
    }

    // Try to open with ngram tokenizer - should fail with schema mismatch
    {
        let config = Config::new().use_ngram_tokenizer(2, 4);
        let result = Index::new(&tmpdir, &config);
        assert!(result.is_err());
    }
}

#[test]
fn schema_mismatch_on_ngram_size_change() {
    let tmpdir = TempDir::new().unwrap();

    // Create index with ngram tokenizer (2, 4)
    {
        let config = Config::new().use_ngram_tokenizer(2, 4);
        let index = Index::new(&tmpdir, &config).unwrap();
        let mut writer = index.get_writer().unwrap();
        writer.add_event(&EVENT).unwrap();
        writer.force_commit().unwrap();
    }

    // Try to open with different ngram size - should fail with schema mismatch
    {
        let config = Config::new().use_ngram_tokenizer(3, 5);
        let result = Index::new(&tmpdir, &config);
        assert!(
            result.is_err(),
            "Different ngram sizes should cause schema mismatch"
        );
    }

    // Reopen with same ngram size - should succeed
    {
        let config = Config::new().use_ngram_tokenizer(2, 4);
        let result = Index::new(&tmpdir, &config);
        assert!(result.is_ok(), "Same ngram sizes should work");
    }
}

#[test]
fn ngram_tokenizer_japanese() {
    let tmpdir = TempDir::new().unwrap();
    let config = Config::new().use_ngram_tokenizer(2, 4);
    let index = Index::new(&tmpdir, &config).unwrap();

    // Create a Japanese event
    let japanese_event = Event::new(
        EventType::Message,
        "トフォリゲートのことでした",
        Some("m.text"),
        "$japanese_event:example.org",
        "@user:example.org",
        1234567890,
        "!test:example.org",
        "{}",
    );

    let mut writer = index.get_writer().unwrap();
    writer.add_event(&japanese_event).unwrap();
    writer.force_commit().unwrap();
    index.reload().unwrap();

    let searcher = index.get_searcher();

    // Test 1: Full word search
    println!("Test 1: Searching for 'トフォリゲート'");
    let result = searcher
        .search("トフォリゲート", &Default::default())
        .unwrap()
        .results;
    println!("Result count: {}", result.len());
    assert_eq!(result.len(), 1, "Full word search should match");

    // Test 2: Partial search with 4-gram (should match)
    println!("Test 2: Searching for 'トフォリ'");
    let result = searcher
        .search("トフォリ", &Default::default())
        .unwrap()
        .results;
    println!("Result count: {}", result.len());
    assert_eq!(
        result.len(),
        1,
        "4-gram partial search 'トフォリ' should match"
    );

    // Test 3: Partial search with 2-gram (should match)
    println!("Test 3: Searching for 'リゲ'");
    let result = searcher
        .search("リゲ", &Default::default())
        .unwrap()
        .results;
    println!("Result count: {}", result.len());
    assert_eq!(result.len(), 1, "2-gram partial search 'リゲ' should match");

    // Test 4: Partial search with 3-gram (should match)
    println!("Test 4: Searching for 'ゲート'");
    let result = searcher
        .search("ゲート", &Default::default())
        .unwrap()
        .results;
    println!("Result count: {}", result.len());
    assert_eq!(
        result.len(),
        1,
        "3-gram partial search 'ゲート' should match"
    );
}
