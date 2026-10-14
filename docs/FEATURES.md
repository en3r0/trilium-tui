# trilium-tui reference

The full feature list and safety notes. For a short introduction see the [README](../README.md).


A fast terminal client for [Trilium Next](https://github.com/TriliumNext/Trilium), written in Rust, built for Omarchy and usable anywhere.
It works on a **local replica** of your notes and syncs with a Trilium server using the same protocol as the desktop app, so the UI never waits
on the network. It also works fully offline with no server at all.

## Quick start
```sh
cargo build --release
# with a server:
TRILIUM_PASSWORD=... target/release/trilium-cli sync http://your-server:8080     # creates ~/.local/share/trilium-tui/default.db
target/release/trilium-tui
# without a server:
target/release/trilium-tui --new
```
[`packaging/install.sh`](../packaging/install.sh) installs both binaries and an Omarchy launcher entry for the current user (it prints the optional Hyprland keybinding, it does not edit your config).
The replica holds your sync secret, so it is created chmod 600; a consistent backup is made once a day (`*.bak-DATE.db`, newest 7 kept).

## What it does
- **Browse**: tree, tabs, history, hoisting, fuzzy jump (Ctrl+J), command palette (Ctrl+Shift+J or `:`), links (`]` `[` Enter), clone-aware paths.
- **Read**: HTML notes rendered for the terminal (tables, lists, code, include-note embeds), exact HTML source (`s`), Markdown rendered, code highlighted in your
  terminal's palette (follows the Omarchy theme), images (sixel on foot, kitty/iTerm2 graphics, half-blocks elsewhere), files and attachments.
- **Search**: Trilium's full query syntax (`#label`, `~relation.title`, `note.title *=* x`, `orderBy`, fuzzy phase) - verified against the real engine - with a trigram index.
- **Collections**: list, table, board (move cards with H/L), calendar (journal day notes), geo list.
- **Write**: rename, create, delete, reorder, move, clone, copy/cut/paste, multi-select, duplicate subtree, labels/relations, revisions (view, diff, restore),
  edit text notes **in place** in the note pane (`e`: formatted text, no tags on screen; bold/italic/underline/strike/code, links, headings, bullet/numbered/to-do lists with Tab nesting, quotes, table cells, links to other notes (`Alt+L`) and URLs (`Ctrl+K`), pictures from a file path or the clipboard (`Alt+P`), `- ` / `1. ` / `# ` / `> ` / `[ ] ` typed at the start of a line, find, undo, mouse, paste), the stored HTML/Markdown/code source in the built-in editor (`Ctrl+e`; `F2` switches between the two, carrying unsaved edits), or `$EDITOR` (`E`), quick append (`a`) and capture-to-today (`c`), today's journal note (`t`).
  If the terminal is closed or the process is terminated mid-edit, the unsaved text is kept under `lost-edits/` next to the database.
- **Move data**: import/export Trilium ZIPs, folders and files; sync both ways; conflicts are logged (`conflicts.log`).
  - **Import**: **Obsidian** vaults (callouts, `==highlights==`, wikilinks/embeds, front matter as labels, attachments, Excalidraw drawings and `.canvas` whiteboards as canvas notes, `.base` files as source notes), **Notion** HTML and Markdown & CSV exports (pages, databases as table collections, properties, files), **Anytype** JSON, Protobuf and Markdown exports (blocks, marks, collections as table/list/grid/calendar/board, properties, files), **Google Keep** Takeouts (text, checklists, colours, attachments), **Evernote `.enex`** (checklists, code blocks, callouts, toggles, attachments, links, tags, dates) and **OPML**. Archives are detected automatically (`--format obsidian|notion|anytype|keep|trilium` forces one).
  - **Export**: Trilium ZIP, **Markdown** (a ZIP of `.md` files that Trilium re-imports), **OPML**, **Evernote `.enex`**, and a **static website** (a "share" site: a page per note, tree + search, no server needed).
  - `trilium-cli export <note> out.md.zip --format markdown | out.opml | out.enex | out.site.zip --format site`, `trilium-cli import vault.zip`; the same from the TUI command palette.
  - Imported HTML goes through Trilium's own allow-list sanitiser; The Markdown, Obsidian, Notion, Anytype and Keep importers were compared against a real Trilium 0.106 server and produce the same notes.
- **Journal**: day, month and year notes as in Trilium, plus **week** (`#enableWeekNote`) and **quarter** (`#enableQuarterNote`) notes on the calendar root, following your week options (`trilium-cli daily --week this`, palette: "Open this week's note").
- **Script**: `trilium-cli search | get | ls | new | set | append | daily | label | rename | rm | clone | mv | dup | revisions | restore | attrs | attach | export | import ...`, all with `--json`.

## Keys
Same as the Trilium web app wherever a terminal allows; `F1` lists them. Override with `~/.config/trilium-tui/keys.json` (Trilium action names).
Config: `~/.config/trilium-tui/config.json` (`openTodayOnStartup`, `server`). Best with a terminal that supports the kitty keyboard protocol (foot, Alacritty, Kitty, Ghostty).

## Safety
- Writes replicate Trilium's own bookkeeping (entity changes, hashes, revisions, link relations, `#sorted`, templates) and are tested against a real server,
  including its consistency checker, plus a randomized stress test.
- If a save can't be written, or a note changed underneath your edit, your text is kept in `lost-edits/` next to the database; if a sync replaces one of your unsynced edits, the old text is kept in `overwritten/`.
- Read-only if the server's database is newer than the version this build was verified against. 
- In-place editing rewrites only the text block you touch; everything else in the note is saved byte for byte as stored. A note whose HTML would not survive that (comments, mis-nested tags, very large or deeply nested notes) opens in the source editor instead, with the reason in the status bar. Pictures can be shown inside notes, in the editor and the reading view (`Alt+V`, remembered in `~/.config/trilium-tui/config.json` as `inlineImages`; kitty/sixel/iTerm2 where the terminal supports them, half-blocks otherwise); a picture that is only partly on screen waits until it fits, and one that can't be shown stays an `[image: …]` label. Not done in place: adding/removing table rows and columns, resizing or captioning pictures, included notes and other objects (edit them from the source editor, `Ctrl+e`). Not done: scripting (JS notes can't run in a TUI). Not imported: OneNote (it needs a Microsoft account). Loose files in a folder or archive are converted like Trilium does (Markdown/HTML/TXT to text notes, CSV/XLSX to spreadsheet notes (XLSX keeps values, formulas, fonts, fills, borders, alignment, number formats, merges, sizes, hyperlinks, data validation and images, checked against Trilium's own importer), Mermaid and Excalidraw files to their own note types; `--keep-sources` keeps them as files).
- Sync status is quiet: no times while client and server match, green for a minute after a sync that moved changes, yellow `unsynced`/`syncing` while edits wait or a sync runs long, red with the last good sync time on errors.
- Sidebar width: `<` / `>` resize it and the width is remembered in `config.json` as `sidebarWidth`.
- Pane style: four looks, cycled from the command palette ("Cycle pane style") and remembered in `config.json` as `paneStyle`: `top` (default: a line across the top of each pane with the title set into it, plus one divider), `flat` (header and one divider), `filled` (darker tree panel, no lines), `boxed` (square borders). The status bar shows "protected" (locked) or "unlocked" only while the open note is protected. Plain-text file notes (`.txt`, JSON, XML, ...) are shown read-only.

Details, protocol notes and the verification log: [`P0-findings.md`](P0-findings.md). License: AGPL-3.0-or-later (derived from Trilium's schema and protocol).

## Notices
The database schema (`crates/trilium-core/src/schema.sql`) and the sync/hashing behaviour are derived from [Trilium Next](https://github.com/TriliumNext/Trilium) (AGPL-3.0-or-later); the full license text is in `LICENSE`.

## Protected notes

Trilium's protected notes work like the web app's: title, content, attachments and revisions are AES-128-CBC encrypted with a data key that the password (scrypt) unlocks; the key lives only in memory.

- TUI (command palette): *Unlock protected notes* (asks for a new password the first time), *Lock protected notes*, *Protect / Unprotect the selected note* (or the whole subtree). A 🔒 marks protected notes, the status bar shows 🔒/🔓, and the session locks itself after `protectedSessionTimeout` seconds without input (default 600).
- CLI: set `TRILIUM_PROTECTED_PASSWORD` to unlock for one command; `trilium-cli password set|change`, `protect NOTE [-r]`, `unprotect NOTE [-r]`.
- Protected notes are edited only in the built-in editor (no plaintext temp files); an unsaved edit that can't be stored is kept encrypted in `lost-edits/*.enc`. Children created under a protected note are protected; while locked, creating or editing there is refused.
- Search works as in Trilium: while unlocked, the full search syntax also looks at protected notes' titles and text (decrypted in memory for the scan; nothing is indexed on disk); while locked they match neither text nor title, only their labels and relations, which aren't encrypted. Exports (zip/markdown/OPML/ENEX/site) leave them out.
- Daily backups copy the database as it is: a backup taken before you protected a note still holds its plaintext. Delete old `*.bak-*.db` files afterwards (palette: *Delete the automatic backups*, or `trilium-cli protect NOTE --delete-backups`).
- Sync copies the ciphertext untouched, so protected notes stay compatible with the web app; the password options sync like Trilium's. Verified against a real Trilium v0.106.0 server in both directions (notes, attachments and revisions protected here are decrypted by the web app and vice versa, including edits, renames and unprotecting). **The protected-notes password is your Trilium login password** (the same options), so a synced replica already has it; `password set` is only for local-only databases.

## Server versions

Released Trilium servers (up to v0.106.0) speak sync protocol 39 and the development branch speaks 40; the client tries 40 and switches to the server's version when it is refused, so both work.

## Credits

The static-site export ships Trilium Next's share-theme stylesheet (AGPL-3.0-or-later) and its note icons: Boxicons (https://boxicons.com), free under CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/), unchanged, as bundled with Trilium Next v0.106.0.
