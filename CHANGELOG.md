# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

- **BREAKING**: Upgrade Tantivy from 0.12 to 0.26. Tantivy 0.26 can't read
  indexes in the old format, so the database version goes up to 5. Opening an
  existing database returns `ReindexError`. Rebuild the index with
  `RecoveryDatabase`. `serde` and `serde_json` are no longer pinned to exact
  versions.
  [[#143](https://github.com/matrix-org/seshat/issues/143)]

- **BREAKING**: Search terms use the syntax of Synapse's server-side search
  by default, so Element's "all rooms" search treats encrypted and
  unencrypted rooms the same way. All words have to match, where they used
  to be combined with OR. `"double quotes"` make a phrase, a leading `-`
  excludes a word or phrase, and `or` separates alternatives. Everything else
  is text, so no search term fails with a syntax error anymore, including
  URLs, `don't`, `<3` or `:)`.

- Tantivy's query syntax is available with `SearchConfig::query_syntax(true)`,
  or `query_syntax: true` in the Node bindings. It adds phrase prefix queries
  like `"phrase pre"*` and field filters like `sender:"@alice:example.org"`.
  All words have to match there as well.

- The room filter is a separate query now, so a search term can't escape it
  anymore.

- Search in the N-gram tokenizer mode is case-insensitive now, like in the
  language-based mode. For example, `kubernetes` finds "Kubernetes". Existing
  N-gram indexes get rebuilt anyway, because of the Tantivy upgrade.

- Search ignores accents now, in both tokenizer modes. For example, `cafe`
  finds "Café" and `strasse` finds "Straße". Existing indexes get rebuilt
  anyway, because of the Tantivy upgrade.

- Two new search options help a search that runs while the user types.
  `SearchConfig::prefix_search()` lets the last word also match as the start
  of a word, from 2 characters on, so `kuber` finds "Kubernetes".
  `SearchConfig::typo_tolerance()`
  lets words match with typos, always or only if nothing matches exactly.
  The Node bindings take `prefix_search` and `typo_tolerance`. Both options
  only work with the default syntax in the language-based tokenizer mode.

- A reindex commits every 2,000 events instead of every 500, which makes it
  about 25% faster.

## 6.0.1 - 2026-09-11

- Fix the npm publish jobs failing to find `.node-version`.
  [[#206](https://github.com/matrix-org/seshat/pull/206)]

## 6.0.0 - 2026-07-22

*This release does not have an associated NPM/Cargo release as it was never published.*

- **BREAKING**: The NPM package for the node bindings renamed from `matrix-seshat` to `@matrix-org/seshat`,
  which now includes per-platform binary packages. As a result, there is no longer a post-install script.
  [[#190](https://github.com/matrix-org/seshat/pull/184)]

## 5.0.0 - 2026-07-22

- **BREAKING**: Change some methods on `Database` to take `&self` instead of
  `&mut self`.
  [[#190](https://github.com/matrix-org/seshat/pull/190)]

- Fix panic that could occur in the writer thread when deleting or replacing events.
  [[#195](https://github.com/matrix-org/seshat/pull/195)]

## 4.3.0 - 2026-05-18

### Changed

- Panics that occur when applications attempt to communicate with the writer
thread (when adding, removing, and committing events) will now generate a
clearer error message.
[[#174](https://github.com/matrix-org/seshat/pull/174), [#176](https://github.com/matrix-org/seshat/pull/176)]

## 4.2.0 - 2026-04-14

- Include the lock file in the package of the node bindings.
  [[#166](https://github.com/matrix-org/seshat/pull/166)]

## 4.1.0 - 2026-04-13

### Added

- Add N-gram tokenizer mode for CJK (Japanese, Chinese, Korean) language support.
  Users can now choose between language-based and N-gram tokenization when
  initializing the search index.
  [[#150](https://github.com/matrix-org/seshat/pull/150)]
- Handle `m.replace` edit events to remove original messages from search index,
  preventing edited messages from appearing in search results with stale content.
  [[#151](https://github.com/matrix-org/seshat/pull/151)]

### Changed

- Panics that occur while enqueuing a task on the JavaScript queue in the Node
  bindings will now generate a clearer error message.
  [[#162](https://github.com/matrix-org/seshat/pull/162)]

## 4.0.1 - 2025-02-18

- Update the used OpenSSL version for improved ARM64 compatibility.
  [[#144](https://github.com/matrix-org/seshat/pull/144)]

## 4.0.0 - 2024-06-07

- [[#136]] Upgrade mechanism for SQLCipher, this allows our users to upgrade
  SQLCipher since SQLCipher has some breaking changes with regards to the cipher
  settings. Otherwise a database that was created with a previous major
  SQLCipher version will refuse to work with a new version of SQLCipher.

- [f9e193e] Removed support for the Japanese language, the tokenizer we were using caused
  problems.

[#136]: https://github.com/matrix-org/seshat/pull/136
[f9e193e]: https://github.com/matrix-org/seshat/commit/f9e193e0b485e66bb50416023e13ba20bbc4c9e7

## 3.0.1 - 2023-05-15

- [[#128]] Allow the avatar URL and display name to be null

[#128]: https://github.com/matrix-org/seshat/pull/128

## 3.0.0 - 2023-04-21

No notable changes were included in this release, the build system for the
Node JS bindigns has changed which requires a major version bump.

## 2.4.0 - 2023-04-17

- [[#117]] Add a feature to build with static SQLCipher and OpenSSL

[#117]: https://github.com/matrix-org/seshat/pull/117

## 2.3.3 - 2022-01-24

- [[#106]] Bracket search terms so multiple words in a search term don't cause a syntax error
- [[#107]] Add precommit hooks

[#106]: https://github.com/matrix-org/seshat/pull/106
[#107]: https://github.com/matrix-org/seshat/pull/107

## 2.3.2 - 2022-01-14

- [[#09d03ae]] Fix the seshat-node build by removing the path dependency

[#09d03ae]: https://github.com/matrix-org/seshat/commit/09d03ae52700e5fd11d137841a02b5e9133d059c

## 2.3.1 - 2022-01-13

- [[#104]] Fix quote search when passing a room_id
- [[#103]] Upgrade our deps

[#104]: https://github.com/matrix-org/seshat/pull/104
[#103]: https://github.com/matrix-org/seshat/pull/103

## 2.3.0 - 2021-07-21

- [[#99]] Switch to a fork of neon-serde
- [[#95]] Update neon and switch to n-api

[#99]: https://github.com/matrix-org/seshat/pull/99
[#95]: https://github.com/matrix-org/seshat/pull/95

## 2.2.4 - 2021-03-26

- [[#82]] Replace the invalid usage of the used attribute

[#82]: https://github.com/matrix-org/seshat/pull/82

## 2.2.3 - 2021-01-15

- [[#78]] Revert to Tantivy 0.12.0 for now

[#78]: https://github.com/matrix-org/seshat/pull/78

## 2.2.2 - 2021-01-12

- [[#77]] Bump our deps and pin serde due to a module becoming private.

[#77]: https://github.com/matrix-org/seshat/pull/77

## 2.2.1 - 2020-10-05

- [[#74]] Improve the load times for the event context.
- [[#75]] Make the IV initialization in the encrypted directory more robust against overflows.

[#74]: https://github.com/matrix-org/seshat/pull/74
[#75]: https://github.com/matrix-org/seshat/pull/75

## 2.1.0 - 2020-06-24

### Added

- [[#69]] Add support to get the user version from the recovery database and add
  a shutdown method to the recovery database.

[#69]: https://github.com/matrix-org/seshat/pull/69

## 2.0.0 - 2020-06-19

### Added

- [[#67]] Added support to store user specific versions in the database.
- [[#65]] Added a method to check if a particular room is already indexed.
- [[#64]] Exposed a method to change the passphrase in the js bindings.
- [[#63]] Added support to paginate search results.
- [[#60]] Added serde serialize/deserialize implementations for most of our
  structs.

### Changed

- [[#66]] Changed the return type of the search methods, they now return a
  struct instead of a tuple. **This is a breaking change**.
- [[#62]] Made all encryption specific dependencies optional.
- [[#59]] Switched from failure to thiserror for our error types.

[#67]: https://github.com/matrix-org/seshat/pull/67
[#66]: https://github.com/matrix-org/seshat/pull/66
[#65]: https://github.com/matrix-org/seshat/pull/65
[#64]: https://github.com/matrix-org/seshat/pull/64
[#63]: https://github.com/matrix-org/seshat/pull/63
[#62]: https://github.com/matrix-org/seshat/pull/62
[#60]: https://github.com/matrix-org/seshat/pull/60
[#59]: https://github.com/matrix-org/seshat/pull/59
