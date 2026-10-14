# Contributing

Bug reports, ideas and pull requests are welcome. For a bug, please use the issue template: it asks for the details that matter (Trilium server version, terminal, steps).

## Build and test

```sh
cargo build --release
cargo test --release
```

You need a Rust toolchain, 1.85 or newer (edition 2024). The workspace has three crates: `trilium-core` (database, sync, import/export), `trilium-tui` (the interface) and `trilium-cli`.

One test, `a_graphics_terminal_is_found_among_the_ancestors_and_tmux_hides_it`, inspects the real parent processes, so it fails when you run the tests from inside a graphics terminal such as foot or kitty. That is a known problem with the test, not with your change.

## Ground rules

- **Never test against your real notes.** Make a throwaway database (`trilium-tui --db /tmp/test.db --new`) or a copy. Writes are pushed to the server on sync, so use a disposable Trilium server for anything that syncs.
- **Match Trilium.** Behaviour that touches stored data (hashes, entity changes, revisions, sanitising, search) is checked against a real Trilium server; [docs/P0-findings.md](docs/P0-findings.md) records how. If you change one of those, say how you verified it.
- **Keep edits lossless.** Anything the editor doesn't understand must be saved byte for byte, or the note opens in the source editor instead.
- Keep changes focused, and add a test when you fix a bug.

## License

By contributing you agree that your contribution is licensed under AGPL-3.0-or-later, like the rest of the project.
