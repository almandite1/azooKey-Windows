# fixtures/

Test data that more than one language's test suite has to agree on.

`default-settings.json` is the settings document `AppConfig::default()`
produces. It is read by:

- `crates/shared/src/lib.rs` — asserts that this build still serializes its
  defaults to exactly this, so a changed default has to be changed here too
- `server-swift/Tests/azookey-serverTests/config_tests.swift` — applies it to
  a fresh `EngineConfig` and asserts nothing moves

That pair is the point. The defaults used to be written out three times — the
Rust struct, the Swift struct, and the settings app's initial state — and
nothing compared them, so "the default profile is empty" was true in three
places until one of them quietly was not. Rust owns the values; this file is
how the other side is held to them.
