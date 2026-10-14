# P0 findings: Trilium internals relevant to the TUI

Source studied: TriliumNext/Trilium `main` @ 7c4de64 (2026-10-12), latest release v0.106.0.
Core logic now lives in `packages/trilium-core` (shared by server, desktop, standalone).
Verified by reading source, not yet by running anything.

## 1. Architecture of the official client
- Desktop = bundled server + local SQLite. It is a full replica that syncs with a server.
- Remote use = either browser against the server, or desktop sync.
- Our TUI mirrors desktop: **local SQLite replica + background sync**. Local-only mode is the same
  minus sync. The UI never talks to the network directly.

## 2. Database
- Schema: `packages/trilium-core/src/assets/schema.sql`. Migrations in `trilium-core/src/migrations`
  (latest 0241). Our client must check the DB version and refuse to open newer/older unknown ones
  (never run migrations ourselves in v1; use a server-created DB or seed via sync).
- Tables: notes, branches, attributes, blobs (content lives here, keyed by content hash),
  attachments, revisions, options, entity_changes, etapi_tokens, recent_notes.
- Soft delete via `isDeleted`/`deleteId`. Branch = tree edge (clones = multiple branches per note).
- Note content: `notes.blobId -> blobs.content`. Text notes are HTML (`mime text/html`).
- `blobId` = first 20 chars of sha512(content) base64 with `+`->`X`, `/`->`Y`
  (`hashedBlobId`, services/utils/index.ts).

## 3. Write path (what must be replicated exactly)
Every entity save also writes an `entity_changes` row:
`entityName, entityId, hash, isErased, changeId(random 12), componentId, instanceId, isSynced, utcDateChanged`.
- Entity hash = base64(sha1(`|`+props joined, optionally `|deleted`)) first 10 chars, text NFC-normalized.
  Hashed props: notes `noteId,title,isProtected,type,mime,blobId`; branches `branchId,noteId,parentNoteId,prefix`;
  attributes `attributeId,noteId,type,name,value,isInheritable`;
  attachments `attachmentId,ownerId,role,mime,title,blobId,utcDateScheduledForErasureSince`;
  revisions: long list (see brevision.ts); blobs `blobId,content`.
- Reordering emits a `note_reordering` change with hash "N/A".
- Sync health check = per (entityName, first-char sector) hash of all `hash+isErased` strings
  (content_hash.ts). Any divergence in our hashing shows up as failed sector checks on the server.
- Consequence: a conformance harness against a real Trilium is mandatory before enabling writes
  against a real replica.

## 4. Sync protocol (`services/sync.ts`, `sync_update.ts`)
- First-time seed: `GET /api/setup/sync-seed` (credentials/rate-limited) returns `documentId` + `documentSecret`;
  `createDatabaseForSync` builds an empty schema; then a normal pull fills it.
- Login each cycle: `POST /api/login/sync {timestamp, syncVersion, hash=HMAC(documentSecret, timestamp)}`
  -> `{instanceId, maxEntityChangeId}`, session cookie.
- Pull: `GET /api/sync/changed?instanceId&lastEntityChangeId&logMarkerId` (chunked, entity rows + base64 blobs).
- Push: `PUT /api/sync/update` with entity changes; then `POST /api/sync/finished`.
- Verify: `GET /api/sync/check`; repair via `POST /api/sync/queue-sector/:entity/:sector`.
- Need to confirm in P0b: exact request/response JSON shapes (`ChangesResponse`, `EntityChangeRecord`),
  `SYNC_VERSION` value, conflict rule in `sync_update.ts` (last-write-wins by utcDateChanged?).

## 5. Other APIs
- ETAPI (`/etapi`, token auth): notes/branches/attributes/attachments/revisions/search/calendar/export.
  Stable but no sync, no live push. Useful as a fallback "thin client" and for CLI scripting.
- Internal `/api/*` (session+CSRF) powers the web UI; undocumented. Not our primary path.
- The reference CLI (`perfectra1n/triliumnext-cli`) wraps ETAPI only, unlicensed TS/Bun. Not reused.

## 6. Test server
- Reachable on the LAN at **port 8080** (port 80 refused). `/api/app-info` requires login.
- No credentials used so far. Real instance is only to be accessed read-only or via a DB copy.

## 7. Open questions for P0b
1. Exact sync JSON shapes and conflict resolution.
2. Protected-note crypto (deferred, but schema keeps `isProtected`; we must not corrupt protected notes
   and must display them as locked).
3. Keyboard shortcut definitions: find `keyboard_actions` source in `apps/client` and extract to JSON.
4. Search syntax implementation (Trilium search is a large DSL; decide: reimplement subset in Rust vs ask server).
5. Concurrent access if desktop app has the same DB open (we will keep our own separate replica instead).

## 8. Proposed next steps
- P0b: capture real sync traffic against a throwaway Docker Trilium (`docker run triliumnext/trilium`),
  write Rust reproducing login+pull, assert our computed hashes equal the server's `entity_changes.hash`.
- P0c: keyboard-shortcut extraction and feature matrix.

## 9. P0b results (sync shapes, conflicts, shortcuts)
- `SYNC_VERSION = 40` (app_info.ts). Sent in `/api/login/sync`.
- Pull response: `{ entityChanges: [{entityChange, entity?}], lastEntityChangeId, outstandingPullCount }`.
  Blob `content` is base64 on the wire. Oversized blobs can be stubbed to "" if client sets a max size.
- Check response: `{ maxEntityChangeId, entityHashes: {entityName: {sectorChar: hash}} }`.
- Conflict rule (`sync_update.ts`): per entity, last-writer-wins on `entity_changes.utcDateChanged`
  (local <= remote -> apply remote via REPLACE, record the change with the remote instanceId;
  local newer -> re-queue ours for others). Already-seen `changeId`s are skipped. Erased entities are DELETEd.
  `note_reordering` change = map branchId->notePosition applied to branches.
- Shortcuts: `crates/trilium-tui/assets/keyboard-actions.json` (109 actions, 65 with default keys), generated by
  `tools/extract_shortcuts.py` from `trilium-core/src/services/keyboard_actions.ts` (non-Mac branch; Mac keys kept separately).
  Caveat: terminals cannot distinguish many chords (Ctrl+Shift+*, Ctrl+Tab, Ctrl+.) without the kitty keyboard
  protocol. Omarchy terminals (Alacritty/Kitty/Ghostty) support it; the TUI should enable it and have a fallback map.
- Blocker: Docker socket needs the `docker` group (user is only in `wheel`); throwaway Trilium for the harness
  can't start yet.

## 10. P0c results: Rust prototype against a real server (v0.106.0, built from source)
Prototype: `crates/trilium-core/examples/p0_sync.rs` (`cargo run -p trilium-core --example p0_sync -- <url> <password>`).
- Seed (`GET /api/setup/sync-seed` with `trilium-cred: base64("u:<password>")`) -> `documentSecret` works.
- Login: HMAC-SHA256 keyed with the secret's **ASCII bytes of the base64 string as-is** over the timestamp string
  (`YYYY-MM-DD HH:MM:SS.mmmZ`), output base64. Server enforces +-5 min clock skew and `syncVersion == 40`.
- Full pull of the demo DB: 2851 changes, ids up to 3679, one cookie-session, no special headers.
- Our entity hash reproduces the server's for **all** notes (584), branches (599), attributes (1430).
  Booleans (`isProtected`, `isInheritable`) hash as the words `true`/`false` (entity classes coerce with `!!x`), NOT 0/1.
  (An earlier note here said 0/1; that was wrong and is fixed.)
- Attachments (19 mismatches) explained: the server hashes a freshly created attachment with
  `utcDateScheduledForErasureSince` = `undefined` (string "undefined"), but a reloaded one as `null`.
  Confirmed by hand (`...|undefined` gives the server's hash). So when WE create attachments we must hash with
  "undefined" to match official clients, and when verifying received ones accept either. Latent upstream
  inconsistency; worth remembering for any field that is optional in the entity class.
- Not yet checked: blob hash (`blobId`+content), revisions, push (`PUT /api/sync/update`), reorder changes.

### Throwaway test server (not Docker)
Built from source with a pinned pnpm in the scratchpad; data dir `scratchpad/tdata`, listening on
127.0.0.1:8082 (the `start-prod` script hardcodes the port), throwaway password. Disposable.

### Local replica via `trilium-cli` (P0d)
- `trilium-cli sync <url> <db>` seeds, logs in, pulls into a Trilium-schema SQLite file, then runs `/api/sync/check`:
  demo DB = 2851 changes in ~0.6s, all 256 sector hashes match the server.
- `trilium-cli verify <db>` recomputes every entity/blob hash from the stored rows and compares to `entity_changes.hash`
  (this is what proves our *write-side* hashing; sync check alone only proves transport).
- The DB file holds `documentSecret` (in our own `tui_state` table); it is chmod 600.
- Licensing: Trilium is AGPL-3.0 and `schema.sql` is copied from it, so this project is AGPL-3.0-or-later.

## 11. Push path (P0e) — verified against a real server
- Implemented in `write.rs` (create_note, set_note_title, set_note_content) and `sync.rs::push`
  (`PUT /api/sync/update` with Trilium's paging headers `pageCount/pageIndex/requestId`, then `POST /api/sync/finished`).
- Verified end to end: notes we create/edit locally and push read back correctly through the server's own ETAPI
  (`/etapi/notes/:id`, `/content`), including non-ASCII content.
- **Bug found and fixed:** blob `content` is base64 on the wire *always*. Sending a TEXT-stored blob unencoded made the
  server base64-decode it into garbage (undetected by `/api/sync/check`, which only compares the hashes we send).
  Regression test: `blob_content_is_base64_on_the_wire_even_when_stored_as_text`.
- **Verification lesson:** `/api/sync/check` and `/api/sync/check-entity-changes` do NOT recompute hashes (the latter only checks
  that rows and entity_changes exist). The real oracle is: have the server create entities itself (ETAPI), pull them, and
  confirm our hash code reproduces its hashes. Done for fresh note, branch, label attribute, attachment, blob: all match.
- Orphaned blobs: when an edit replaces a blob we delete it and its change row locally (as upstream does); the server
  purges its copy itself. Old garbage blob from the bug above sits on the throwaway server until that sweep.
- Not yet implemented: revisions on edit, note deletion/undelete, reordering (`note_reordering`), clones, attribute and
  attachment writes, protected notes, conflict handling when the same note is edited on both sides (pull applies
  last-writer-wins by `utcDateChanged`, matching upstream, but we don't surface conflicts).

## 12. P2 editing (verified)
Ported from upstream and covered by tests (`crates/trilium-core/src/edit.rs`):
- **Revisions** (`saveRevisionIfNeeded`/`saveRevision`): snapshot once per `revisionSnapshotTimeInterval` (default 600 s), copies attachments.
  `revisionSnapshotNumberLimit >= 0` (erasing old revisions) is NOT applied yet; the default is -1 (keep all).
- **Link relations** (`saveLinks`): text and markdown notes keep `internalLink`/`imageLink`/`includeNoteLink` relations in sync and strip absolute
  server URLs from `href`/`src` exactly like the web app. Not ported: downloading external images, attachment extraction, bookmarks.
- **Deletion** (`deleteBranch`): soft delete with a shared `deleteId`; last strong placement deletes the note, children, own attributes,
  incoming relations and attachments; clones elsewhere survive. Root refuses.
- **Reordering/moving**: sibling `notePosition` shifts do NOT touch `utcDateModified` (positions aren't hashed; order syncs via a
  `note_reordering` change). Cross-parent moves = new branch + soft-deleted old one. Cycles and duplicate placement are rejected.
- Verified against a live server: edits, link relation creation, prefix, deletion (ETAPI 404), order, then a restart ran Trilium's own
  consistency checker: "All consistency checks passed with no errors detected".
- TUI: Enter rename, Ctrl+O/P create after/into, Delete (confirm), Ctrl+Arrows reorder/move, F2 prefix, Alt+L/Alt+R labels/relations,
  `e` edits the exact stored text in $EDITOR (trailing-newline added by editors is not treated as an edit).
- Perf on a synthetic 100k-note / 270 MB database: startup to first paint ~0.4 s, model load 0.29 s, full-text LIKE search 0.2-0.3 s,
  jump (fuzzy over all titles) keystroke latency ~60 ms including tmux overhead. A real index is future work.

## 13. Journal, templates, tabs, palette, CLI (verified)
- `journal.rs` ports `date_notes.ts` for year/month/day notes (title patterns, `#dateNote`/`#monthNote`/`#yearNote`, `#sorted`,
  `yearTemplate`/`monthTemplate`/`dateTemplate` relations) and the creation-time rules they depend on:
  `child:` attribute inheritance (so `~child:child:child:template` on a calendar root reaches day notes), `~template` content/type copy
  into empty notes, template subtree duplication, and auto-sorting of `#sorted` parents (title order; other sort criteria not ported).
  Not ported: week/quarter notes (the API refuses with a clear error), `#workspaceCalendarRoot`, non-English titles.
- Verified on the live test server: a day note created locally via `trilium-cli daily` picked up the demo journal's own
  `dateTemplate` content; after pushing, a server restart ran its consistency checker: "All consistency checks passed".
- Nestable savepoint transactions (`Db::begin`) so composite operations (journal creation, templates) stay atomic.
- TUI: tabs (Ctrl+T/W, Ctrl+Tab, Ctrl+1..9), command palette (Ctrl+Shift+J or `:`), `t` / `--today` / config `openTodayOnStartup`,
  include-note embeds rendered inline, syntax highlighting uses the terminal palette so it follows Omarchy themes.
- CLI: `search get ls new set append rename rm label daily sync verify`, `--json`, `--sync`.
- Packaging for Omarchy prepared but NOT installed: `packaging/install.sh`, `packaging/trilium-tui.desktop`.

## 14. Not done yet (honest list vs "all abilities of the browser version")
Trilium search syntax (`#label`, `~rel`, `note.title =`, saved searches) and a fast index; revisions UI (list/diff/restore) and
backlinks/note-info panels; clone/cut/copy/paste/multi-select; attachments and image preview; collection views (table/board/calendar/
geomap); note map and link graph; import/export; built-in editor (only $EDITOR); protected notes (deferred by decision);
surfacing of sync conflicts; scripting (JS notes cannot run in a TUI); week/quarter journal notes; revision erasure limits.

## 15. Real-data safeguards (verified)
- Writes are refused if the server's `dbVersion` (read from `/api/app-info` at each sync login, stored as `serverDbVersion`) exceeds
  `KNOWN_DB_VERSION` (241): read-only until the build is updated. Sync itself already refuses a different `syncVersion`.
- One consistent backup per day (`<db>.bak-YYYY-MM-DD.db`, `VACUUM INTO`, chmod 600, newest 7 kept) is made when the TUI starts and
  before any CLI write command.
- Sync conflicts (remote change older than a newer, not-yet-pushed local edit to the same entity; last writer wins like upstream) are
  reported in the status line and appended to `conflicts.log` beside the replica instead of being dropped silently.

## 16. Search engine (ported from `services/search/`, verified against Trilium's own engine)
- `trilium-core/src/search/`: `lex` (exact port), `parse` (syntax tree incl. `#label`, `~relation.prop`, `note.*` properties,
  `parents/children/ancestors/links/backlinks`, `AND/OR/NOT`, `orderBy`, `limit`, date keywords `today/month/year/now ± n`),
  `cmp` (= != *=* =* *= > >= < <= %= ~= ~*), `eval` (set algebra over an in-memory corpus), `index` (FTS5 trigram index), `text`.
- **Oracle test:** 121 queries run against both this engine and a real Trilium v0.106.0 server (`GET /etapi/notes?search=`) on the demo
  database: 120 identical result sets. The one difference is an arbitrary tie at a `limit` boundary between equally-titled notes.
  This caught real gaps: `_hidden` itself is visible; `getAttributes()` follows `~template`/`~inherit` and parent chains; `stripTags`
  turns block tags into spaces; canvas/mind-map notes contribute only their text (lower-cased JSON, key `nodedata`);
  the built-in relation list decides what counts as a link; and the progressive-search rules below.
- **Upstream quirks replicated on purpose** (so results match the web app): queries starting with `#` run once; any other query
  runs exact first and, if fewer than 5 results score >= 10 (every result does when there is no text, as every title "starts with"
  the empty query), adds typo-tolerant matches and RE-SORTS by score/depth/path title, which discards an `orderBy` for result
  sets under 5. `note.ancestors.title = x` includes the matched note itself.
- Not ported: fuzzy matching of attribute *values* in flat text, OCR content, link-title/icon text injection into the index (an index
  entry is keyed by content hash and must not depend on other notes' titles), saved-search options beyond `#searchString`.
- Index: FTS5 trigram over preprocessed text, one row per distinct blob (never stale); version = fingerprint of the preprocessor's output
  for probe inputs, so any preprocessing change rebuilds it. Built off the UI thread at startup; until then searches scan content.
  Costs about 1x the text size in extra disk. Tokens shorter than 3 characters fall back to scanning.
- Perf, 100k notes / 277 MB: corpus build 0.35 s once; label/property queries 0.3-2 ms; rare words 6-10 ms; title phrases 50 ms;
  a term matching every note ~0.7 s (worst case). First search after startup pays ~0.3 s to warm per-note flat text.
- Performance bug found along the way: `SELECT .. FROM branches WHERE noteId=? AND isDeleted=0` picked the `isDeleted` index (full
  scan) without table statistics; making creation quadratic. Fixed with `+isDeleted` (forces the noteId index).

## 17. Note panel, links, attachments (verified)
- Panel (`i`, `Alt+A`, or `b`/`v` in the tree): Attributes (own + inherited, add/edit/delete), Backlinks (sources of relations incl. content links),
  Revisions (list, diff against current as rendered-text diff, restore), Info (ids, dates, size, all note paths), Attachments (list, save to Downloads).
- `restore_revision` ports `restoreRevision`: snapshot current state as source "restore", replace attachments, then title/type/mime/content from the revision.
- Link following in rendered notes: `]`/`[` select, Enter/Ctrl+Enter follow (internal -> open note, http(s)/mailto -> xdg-open).
- New CLI: `revisions restore snapshot backlinks attrs attach attachments attachment-get`.
- Verified on a real server: restore, attribute edit/delete and a binary attachment (26 bytes incl. NULs) read back byte-exact via ETAPI; revisions
  present in the server DB (manual, manual, restore); server consistency checker: "All consistency checks passed".

## 18. Clone, move, duplicate, clipboard, multi-select (verified)
- Core: `clone_note` (second placement, validated against cycles/duplicates/search parents), `move_branch_to` (end of new parent), `duplicate_subtree`
  (fresh ids, links/relations inside the copied tree remapped, "(dup)" suffix, placed right after the original). `#sorted` parents are re-sorted.
- TUI: `m` / Shift+Up/Down / Ctrl+A mark notes; Ctrl+C / Ctrl+X / Ctrl+V (copy+paste clones, cut+paste moves); Ctrl+Shift+C / Ctrl+Shift+X pick a target note;
  duplicate from the palette; Delete works on the whole selection. Ctrl+C no longer quits (it copies); `q` / Ctrl+Q quit.
- Verified against a real server: clone shows two parents, move leaves one, duplicate copies the subtree; consistency checker passed.

## 19. Images and files (verified in tests; sixel/kitty/iTerm2 encoders exercised, half-blocks seen in a real session)
- `ratatui-image` (without its chafa dependency) draws pictures with sixel (foot), kitty graphics, iTerm2 or half-block characters. The terminal is
  probed only when it is a known quick responder (foot, kitty, ghostty, wezterm, iTerm, konsole) and never inside tmux; `TRILIUM_TUI_IMAGES=halfblocks|query`
  overrides. Startup measured 49 ms.
- Image notes are shown in the pane (Enter zooms); `<img>` in text notes are selectable like links (`]`, Enter) and resolve `api/images/<note>/..`,
  `api/attachments/<id>/image/..`, `/download`, and `data:` URLs from the replica. SVG and undecodable files explain themselves; `x` saves (never
  overwriting), `O` opens in the default application via a private temp dir that is removed on exit.
- `create_file_note` / `set_note_bytes` create `image`/`file` notes from raw bytes (also the basis for import).

## 20. Import / export (verified in both directions against the real engine)
- `transfer.rs`: Trilium-compatible ZIP export (HTML format: `!!!meta.json` formatVersion 2, per-note data files in the same
  directory layout, attachments, `.clone.html` placeholders, `navigation.html`/`index.html`/`style.css`, relative link/image rewriting,
  sanitize-filename rules, unique names) and import (Trilium zips with metadata; any zip, folder or single file without it).
- Plain imports are lossless: `.md` becomes a markdown code note (not converted), `.html` a text note, images image notes, unknown types file notes.
- Oracle A: ETAPI `GET /notes/:id/export?format=html` on three subtrees (54 notes, 16 attachments, a clone, canvas/mind-map/code/file notes) imported with
  my importer and compared field by field with the originals: 0 differences (whitespace-normalised; Trilium pretty-prints exported HTML).
- Oracle B: a 9-note tree exported by my code was imported by the real server (`POST /etapi/notes/root/import` -> 201), synced back and compared: 0 differences.
- Found by the oracle: Trilium's importer replaces an included note's id globally in the content, which rewrote `href="#root"` when an include-note
  pointed at root; ours only remaps ids of notes that are part of the import (relations to outside notes keep their target if it exists here).
- Archive safety: entries with `..` or absolute paths are dropped, 4 GB uncompressed cap, symlinks and dotfiles are skipped in folder imports.
- CLI `export`/`import`; TUI palette "Export subtree to a ZIP file..." / "Import a ZIP, folder or file here...".
- Not ported: markdown-format export, share export, OPML/ENEX/Notion/Obsidian importers, `safeImport` HTML sanitising (imported HTML is stored as given).

## 21. Collection views (verified in tests; calendar viewed on the real demo Journal)
- `#viewType`: list/grid (titles with text previews), table (promoted `label:x` definitions, else the most common labels), geoMap (title +
  `#geolocation`), board (groups by `#board:groupBy`, default `status`; column order from the `board.json` view-config attachment; H/L move a
  card by rewriting its label, n adds a card), calendar (month grid; journal roots open or create the day note on Enter, other calendars show
  `#startDate`/`#endDate` ranges and add events with n). First weekday follows the synced `firstDayOfWeek` option.
- A bug found by testing: when the "(no value)" board column emptied it vanished and shifted the cursor; the cursor now follows the card by value.
- `set_note_type`, `set_label_value`, `view_config`, `existing_day_note`, `day_notes_in_month` were added to the core for this.
- Not ported: week/day time-grid calendar modes, drag and drop, table column editing/sorting, a real map, presentation view.

## 22. Markdown, quick capture, stress test
- Markdown notes (code notes with a markdown mime, incl. imported `.md`) open rendered through the same renderer (tables, task lists, footnotes,
  `[[noteId]]` wiki links as followable links); `s` shows the exact source. Quick capture: `a` appends a line to the open note, `c` captures into today's note.
- `crates/trilium-core/tests/stress.rs`: 5 seeds x 250 random operations (create, rename, edit, move, clone, delete, attributes, duplicate, restore revision,
  attach) with the change-log hash check plus tree invariants (no dangling branches, no orphans, no cycles, no live attributes on deleted notes) after every step,
  finishing with an export/import round trip. It found a real crash (duplicating a note into its own subtree), now refused with an error.
- `Db::ensure_root` creates the root note (with its change record) for brand-new local-only databases.

## 23. Built-in editor, independent review, hardening (2026-10-12)
- `editbuf.rs` + `edit_view.rs`: the built-in editor (`e`; `E`/Ctrl+E still hands off to `$EDITOR`). It edits the stored source byte for byte: lines split on `\n` only,
  a `\r` stays attached to its line, new lines in a CRLF note get CRLF. Selection, undo/redo (typing runs coalesce), find (case-folding safe for chars like `İ`),
  go to line, indent, clipboard through `wl-copy`/`wl-paste` (xclip fallback), bracketed paste, mouse. Saving over a note that changed underneath (a sync landed) needs a second Ctrl+S.
- A subagent reviewed the whole project critically (read-only, with repro tests in a scratch copy). Verified findings, all fixed with regression tests:
  duplicating a subtree corrupted binary notes and dropped attachments (now byte-exact, attachments copied and re-pointed); `set_note_content` accepted file/image notes;
  protected notes could be renamed/retyped (now refused, also for descendants of a duplicate); moves/attributes/attachments could target missing notes (orphans were
  pushed to the server); the editor panicked in find on case-folding chars; edits were lost when the write lock was busy (the outermost transaction is now
  `BEGIN IMMEDIATE`, waiting out the sync thread) or when a save failed (text is kept under `lost-edits/`); remote changes silently replaced unpushed local edits
  (now the old text is saved under `overwritten/` and logged); the DB and its `-wal` were briefly world-readable (created 0600); a panic left mouse/keyboard modes on;
  opening today's note before the first sync could duplicate a day note; import/export read unbounded sizes and an export failure could truncate the target file;
  `/tmp` was used predictably for attachments (now a private unpredictable directory; runnable file types are never auto-opened).
- Confirmed safe: escape-sequence injection (ratatui drops every cell containing a control character; a regression test renders hostile titles/content/attributes),
  zip-slip, URL opening (only http/https/mailto, single argument).
- Flaky tests found and fixed (shared-millisecond timestamps in fixtures). A second review round covers the new code.

## 24. Week/quarter journals, OPML, ENEX, static-site export (2026-10-13)
- `journal.rs`: `getWeekNote`/`getQuarterNote` ported (week numbers from the `firstDayOfWeek`/`firstWeekOfYear`/`minDaysInFirstWeek` options, week years across New Year,
  a week spanning two months is cloned into both). Verified against the real Trilium 0.106 server through ETAPI `calendar/days/<date>`: identical Calendar > year > quarter > month >
  week > day chains for 9 dates including 2025-12-30, 2026-01-01 and 2027-01-01 (which Trilium files under 2025/2026 December because week 1 contains Jan 1).
- `htmltree.rs`: forgiving tokenizer/tree and `sanitize_html` (scripts, frames, event handlers, `javascript:`/`data:text/html` URLs removed) shared by the importers.
- `formats.rs`: OPML 1.0/1.1/2.0 import and OPML 2.0 export; ENEX import ported from `enex.ts`/`enex_converter.ts` (legacy `<en-todo>` runs and `--en-todo` lists to todo lists, code/math/mermaid
  blocks, callouts, toggles, task groups, OneNote checkbox images, images as image attachments and other resources as file attachments with reference links, `source-url`/tags as labels,
  created/updated dates, Evernote internal links resolved by title unless ambiguous). Trilium's own sample `.enex` files are in `crates/trilium-core/tests/fixtures/enex/` with tests that mirror its spec.
- `share.rs`: "share" export as a self-contained static site (page per note, tree and search from two small scripts, files and attachments under `files/`, excluded/protected notes left out, active content stripped).
  Trilium's own share export needs its web app's templates, so this is an equivalent rather than a byte-compatible copy.
- End to end: a fresh replica synced from the server, ENEX samples + an OPML file imported and a day note created, 60 changes pushed; the server's consistency checker then reported no errors.

## 25. Markdown/ENEX export, Obsidian and Notion import (2026-10-13)
- `mdhtml.rs` (Markdown → CKEditor HTML) and `htmlmd.rs` (HTML → Markdown) follow `markdown_renderer.ts` / the turndown-based exporter. Checked against the real 0.106 server through ETAPI: the Markdown export of 8 sample notes is
  byte-identical (list markers `*   `, fenced code, GFM tables, admonitions, to-do items, escapes) except links with spaces in the URL (ours are `%20`-escaped so they stay valid Markdown), and importing Trilium's Markdown export
  gives the same HTML in both implementations.
- `obsidian.rs` and `notion.rs` port the two structured importers. The real server's importers (driven through its internal `/api/notes/:id/notes-import?format=...` with a session + CSRF token) were run over two Obsidian vaults and a
  Notion export with all common block types and database properties: titles, note order, labels, promoted definitions, relations, attachments and HTML bodies match; remaining differences are intentional (script *text* is dropped rather
  than kept, inline `style` on spans is kept only where the allow-list permits, language ids are matched case-insensitively).
- `htmltree::sanitize_html` is now Trilium's allow-list sanitiser (tags, attributes, style properties with value patterns, URL schemes, h1→h2 shifting) plus extra rules (no HTML/SVG `data:` URLs, no form/media tags, scripts and their text dropped).
- `enexout.rs`: ENEX export (ENML with `<en-media>` resources, `<en-todo>`, Evernote code-block markup, tags from valueless labels, source URL). It re-imports through our own ENEX importer.
- Import format is auto-detected for archives (`.obsidian/` folder → Obsidian; `Title <32 hex>.html` pages → Notion) and can be forced with `--format`.
- End to end: a replica synced from the server received vault, Notion and Markdown imports and an ENEX round trip (144 changes pushed); the server's consistency checker then reported no errors.
- Review of this batch (all fixed with regression tests): the tokenizer let a `data-x"` attribute name swallow a `>` so a following `<script>`/`<iframe>` survived sanitising (start tags are now parsed with the HTML
  rules, quotes only after `=`, attribute and element names restricted to plain names); numeric entities without `;` (`javascript&#58`) hid a scheme; ENEX export wrote control characters and unknown elements and merged
  attachments with identical bytes; Markdown export left text that re-imports as syntax (`2.`, `==x==`, `$x$`, `---`) unescaped; Markdown import broke links containing math and leaked placeholders, and split bare URLs at escapes;
  an Obsidian vault imported as a folder wasn't detected; nested Notion zips each got a fresh size budget; quadratic scans on unterminated tags.

## 26. Anytype, Google Keep and Excalidraw import, help text (2026-10-13)
- `keep.rs`, `anytype.rs` and the Excalidraw part of `obsidian.rs` (with a `lz-string` decompressor checked against output of the real library) complete the importer set. Each was run beside the real 0.106 server's importer
  (internal import route, same zips): Keep and Anytype exports (text/checklist/colour/dates/attachments; blocks, marks incl. UTF-16 offsets, lists, toggles, callouts, tables, bookmarks, math, collections with a board view,
  select/multi-select/date/number/checkbox/url/file properties, file members, mentions and links) came out with no differences beyond cosmetic `<img />` / missing-space quirks of the reference; an Excalidraw drawing matches up to JSON key order.
- Archives are auto-detected: `.obsidian/` -> Obsidian, `Title <id>.html` -> Notion, `objects/*.pb.json` -> Anytype, Takeout note JSON -> Keep; `--format` forces one.
- Help text: every `trilium-cli` argument and option is documented (`--help` of each subcommand), and `trilium-tui --help` explains all flags.

## 27. Remaining importers (2026-10-13)
- `fileconv.rs` + `spreadsheet.rs`: the generic importer now converts loose files like Trilium's `single.ts`/`zip.ts`: Markdown (+ YAML front matter as labels), HTML, plain text (single files), CSV and XLSX (Univer workbook JSON: values,
  formulas with cached results, merges, hidden/sized rows and columns, date number formats), Mermaid, Excalidraw. Compared with the real 0.106 server over a mixed zip and an exceljs-generated workbook: no differences (notes are also created in
  archive order now). `--keep-sources` restores the old lossless behaviour.
- `notion.rs`: Notion's Markdown & CSV export (pages from `.md`, databases from `.csv` with column types inferred from the values, relations from the CSV's page links, `_all.csv` duplicates ignored).
- `obsidian.rs`: `.canvas` whiteboards become Excalidraw canvas notes (nodes as boxes with text, edges as bound arrows), `.base` files keep their YAML; relative Markdown links between notes are resolved (also used for Anytype's Markdown export, which is a plain folder of Markdown files).
- `anytype_pb.rs`: Anytype Protobuf export via a small wire decoder. The message field numbers are from memory of Anytype's `.proto` files (no sample export was available), so a snapshot is only accepted when its block tree is coherent;
  otherwise the import stops with a message asking for the JSON export.
- Not applicable: Trilium's OneNote importer talks to Microsoft's cloud API with an account login.
- Review of this batch (fixed, with regression tests): Protobuf numbers decoded as floats hid layouts/relation formats/collections; loose converted Markdown/HTML never had relative links and images pointed at the imported notes (now a post-pass); Notion Markdown mode took the first CSV row for duplicate titles and stripped body lines shaped like properties;
  UTF-16 (BOM) text files became file notes; XLSX parsed a worksheet once per `<sheet>` element (now once per part, 256 sheets max), had overflow/size problems in cell and column references (Excel's limits are enforced) and cells without references overwrote each other;
  the lz-string decoder could be driven to gigabytes (output and dictionary capped); collapsible class matched `data-class`; `.pb` Anytype exports weren't auto-detected; `Foo.md`/`Foo.txt` next to folder `Foo` collided (md > html > txt); links climbing above a vault root resolved to root notes.

## 28. Protected notes

- `crypto.rs` (scrypt N=16384 r=8 p=1; AES-128-CBC, base64(iv16 || ct) with a 4-byte SHA-1 prefix inside the plaintext; 13-byte legacy IVs accepted) and `protect.rs` (session, `save_blob_for`, `protect_note`). Values taken from Trilium v0.106.0 sources: protected blobId = hashedBlobId("t$[nvQg7q)&_ENCRYPTED_?M:Bf&j3jr_" + plaintext); stored content is the ciphertext text; entity hashes cover the stored content.
- Encrypted: note/attachment/revision titles and blob contents. Not encrypted (as in Trilium): attributes, prefixes, revision descriptions, types and mimes.
- Writers (`set_note_title/content/bytes`, `create_*`, `add_attachment`, `save_revision`, `restore_revision`, import of attachments, `store_imported_html`, templates) route through `save_blob_for[_note]`, so nothing can write plaintext into a protected note; locked sessions fail with the `LOCKED` error. `secure_delete` is on and `protect_note` truncates the WAL.
- Open: no live-server interop check yet; duplicating a protected note works while unlocked (the copy is re-encrypted); the search index excludes protected notes.

## 29. Live check against the released server (2026-10-13)

- The v0.106.0 release (`TriliumNotes-Server-…-linux-x64.tar.xz`, port 8083) reports syncVersion 39, our previous fixed 40 came from the development branch (bumped for include-note captions, content only). Login now retries with the version the server names (39 or 40). Sync, push and the content-hash check pass against the release.
- Protected notes: created/edited/renamed/protected/unprotected here and in the server (`/api/login/protected` session) in both directions; titles, content, attachments and revisions decrypt on the other side and the blobIds agree. The protected password is the account password.

## 30. XLSX parity and the share theme (2026-10-13)

- `xlsx.rs` replaces the old reader in `spreadsheet.rs` and follows `parse_from_xlsx.ts` and the exceljs behaviour behind it: inline cell styles (font, fill with Office theme palette and tint, alignment, borders, number formats incl. exceljs' built-in table), shared-formula sliding, hyperlinks (http/https/mailto only), data validation (grouped and coalesced ranges), floating images (one/two-cell anchors with exceljs' EMU maths) and exceljs defaults (column width 9, row `ht` without `customHeight`, comparison validations default to `between`). Differential tests against the live v0.106.0 server (hand-built and openpyxl files) are identical except where Trilium has bugs: rich-text cells become `[object Object]`, date-typed hyperlink cells and `t="d"` cells and date validation formulas become garbage, `nan` becomes null. We emit the sensible value there.
- Trilium's importer fails on some openpyxl images and on out-of-range shared-string indexes; ours is tolerant.
- `share.rs` now emits Trilium's page structure (`page.ejs`) with its stylesheet (`share_theme.css`, from the v0.106.0 build, AGPL-3.0-or-later): header, left tree and search, ToC pane with active tracking, prev/next in reading order, subpage links, light/dark switch. Tree and search are built client-side from `site-data.js`. Markdown code notes are rendered.

## 31. Aligning the small gaps with Trilium (2026-10-13)

- Protected-note search follows the real server (checked through its internal search API with and without a protected session): unlocked, titles and text of protected notes match with the normal syntax (`note.content *=* x`, `note.title *=* x`, plain words); locked, they match neither (not even as "[protected]"), but `#label` searches still find them because attributes aren't encrypted. The search engine (`search/eval.rs`) decrypts protected content only for the scan; the corpus holds an empty title for locked protected notes. The earlier plain-word helper is gone.
- The server's own search of notes that arrived by sync as BLOB rows finds their titles but not their protected content (it decrypts only string rows); that is the server's behaviour, so notes pushed from here are readable and openable there but their text isn't found by server-side search until the note is next saved on the server.
- Share export: note icons follow `getNoteIcon` (`#iconClass`, home/folder/note, code mime icons, file and image mime icons, geo pin/shapes, per-type defaults) with the boxicons font and a glyph table taken from the v0.106.0 server bundle (`share_icons.css`, `boxicons.woff2`; CC BY 4.0, notice in the site and the README).
- XLSX: conditional formatting, filters, charts, comments, frozen panes and defined names are dropped by Trilium's importer too (see its header comment), so nothing is missing relative to it.

## 32. Review round 7 (XLSX, protected notes, share/search/sync) — all verified findings fixed

- XLSX: per-cell text capped at Excel's 32,767 characters, workbook caps on cells (1M) and text (128 MB) so a 22 KB file can no longer need 8 GB, repeated sheet parts count against the budget, `<col>` ranges are filled once, validation rules are capped and expanded within a per-sheet cell budget, images are capped (200 per sheet, 64 MB total, empty ones skipped) with sizes in a `Dims` table instead of per-call loops, per-part XML cap 64 MiB; namespace-prefixed SpreadsheetML (`x:row`) is read; phonetic runs of inline strings skipped; CDATA literal; `date1904` workbooks shifted; a hyperlink without a usable URL no longer turns a number into text; `<b val="0"/>`/`<u val="none"/>` honoured (Trilium ignores `val` there — deliberate deviation); shared-formula sliding is overflow-safe and leaves references that would leave the sheet alone. Kept as in Trilium: alpha-`00` colours are dropped, indexed colours unsupported.
- Protected notes: protect/unprotect now also converts attachments owned by the note's revisions and repairs mixed notes on a second run; `restore_revision` converts attachments saved in the other state; the text index (`tui_fts`) is pruned and optimized when a note is protected and `drop_blob_if_unused` removes index rows; locking closes panels/pickers/images and drops other tabs' protected views; zip/markdown/ENEX exports skip protected notes' attachments and protected image sources; `conflicts.log` is 0600 and never names protected notes; protected template children are copied decrypted and re-protected; `set_password` refuses on a linked replica that hasn't synced yet; "open for editing" reports a decrypt failure instead of opening an empty note.
- Share/sync/backups: note and attachment ids must be `[A-Za-z0-9_]+` (and not `index`) to become pages/files, archive names with `..` or a leading `/` are refused, hrefs are escaped, file links percent-encoded, heading ids are unique against the theme's own ids; sync login follows the server's version once (no flip-flop loop); only `<stem>.bak-YYYY-MM-DD.db` files count as backups.
- Left as is on purpose: CSV stray quotes (same reader as Trilium's `parseCsv`), `#excludeFromExport` own-label only (Trilium: `hasOwnedLabel`), `absoluteAnchor` images / x14 validations / chart sheets unsupported, site-data.js grows with the tree (search text is capped per note), key copies in memory are not zeroized, "open with default app" of a protected attachment still writes a private temp file under XDG_RUNTIME_DIR.

## 33. Review round 8 (sync/integrity, importers/sanitiser, TUI/CLI, regressions) — verified findings fixed

- Sync: pulls now sweep blobs nothing refers to (the server drops them silently; otherwise `verify` fails and a note's old plaintext stays in the replica); a blob that is handed out again while unreferenced is announced again (otherwise the server could end up with a note pointing at a blob it had dropped, and other replicas lose the content); a malformed or unknown remote record is reported and skipped instead of stalling every pull (INTEGER columns are type-checked); a remote erase can't delete a blob a local row still uses; the pull cursor is reset when it is ahead of the server; the pull loop ends on an empty batch and after 100k batches; server error text is shortened and stripped of control characters.
- Importers: `decode_entities` reads every digit of a numeric reference (an 8-digit cap let `&#x0000000003A;` hide a `javascript:` scheme: stored XSS everywhere `sanitize_html` is used); Trilium-zip and loose `.html` imports are sanitised like Trilium's safe import; the sanitiser unwraps `form`/`fieldset`/`picture`/`video`/... instead of deleting their text (only script-like and raw-text containers drop with content); exported names are unique even for `Same`/`same`/`1_same`, names that sanitise to nothing, `index`/`navigation`, and `con `; archive paths over 64 folders deep, OPML nested over 100 levels and Anytype blocks nested over 64 deep are refused or cut (the last overflowed the stack); Anytype mark counts are capped; zip budget is 1 GiB; exports that would silently drop a tree deeper than their limit now say so; OPML attributes keep newlines; canvas links keep only web URLs.
- TUI/CLI: a sync that deletes the open note no longer panics the draw; markup nested past 200 levels is cut and a page embeds at most 40 notes (both used to hang or abort); a failed save shows its error inside the editor; a crashed external editor's text is kept; attachment names are cut to a safe length and lose bidi/format characters; only known viewer types open by themselves; long titles are cut by characters before measuring; the tree list stops at 200,000 rows; Ctrl+D clears the selection anchor; user key bindings win over defaults in a fixed order; CLI listings print one record per line with control characters blanked even when piped.
- Left as is: read-modify-write windows of a few milliseconds against the sync thread, delete/duplicate of trees deeper than their guards (they refuse), a crash between commit and cursor write losing the "overwritten" copy, `render_html` on a single million-character word.

## 34. Editing in place (2026-10-13)
`e` now edits a text note in the note pane instead of its HTML source; `Ctrl+e` is the old source editor and `F2` toggles between them (unsaved edits travel with the switch), `E` is still `$EDITOR`.

- **Model** (`trilium-core/src/richtext.rs`): the note stays an `htmltree` tree. A cursor is `(textblock, unit)`; a textblock is a run of inline siblings (a paragraph's content, a list item's own text, a table cell's text), a unit is a character, `<br>` or an atom (image, note link). Every edit rewrites only the run it touches, so untouched markup serialises exactly as stored. Opaque elements (rules, included notes, nested tables, anything unknown) are skipped by the cursor and never altered; ordinary tables are grids of editable cells (merges, deletes and selections never cross a cell border or remove a cell).
- **Typing** mirrors CKEditor: spaces that would collapse become `&nbsp;`, an emptied paragraph/item/cell keeps `&nbsp;` as filler (treated as empty), Enter at the end of a heading starts a paragraph, Enter on an empty list item leaves the list (or un-nests it), Enter in a cell is `<br>`, typing at the end of a link leaves the link, typing at the end of bold continues the bold. `Ctrl+B` with nothing selected applies to the text typed next.
- **Which notes**: `unsupported_reason()` refuses a note (opening the source editor with the reason) when it is over 400 KB, nested deeper than 48 levels, contains comments/`<!`/`<?`, or re-serialising it would change more than the spelling of void tags (`<br />`→`<br>`) and whitespace inside tags (compared at token level, attributes decoded). On the user's real data (1,197 text notes) 1,195 open in place; the two others are a 444 KB note and one with an HTML comment. 33 of those carry the cosmetic void-tag respelling that Trilium's own editor rewrites on its next save as well.
- **View** (`trilium-tui/src/rich_view.rs`): the layout wraps each textblock into lines (quote bars, list markers and to-do boxes in front), lays tables out as column grids, and maps cursor <-> screen cell. Objects are rendered by the normal renderer. Undo is snapshot based and only records edits that changed the tree; typing/deleting runs share one step; a save ends the run.
- **Safety**: same guards as the source editor (second Ctrl+S if the note changed underneath, discard prompt, unsaved text kept under `lost-edits/` on kill/panic, encrypted for protected notes; locking closes an in-place edit of a protected note after keeping its text, auto-lock waits while an editor is open).
- **Keys**: `Alt+I` is italic as well because `Ctrl+I` is `Tab` in terminals without the kitty keyboard protocol (the app enables that where available). Alt chords: `Alt+1..6` headings, `Alt+0` paragraph, `Alt+U/O/T` bullet/numbered/to-do list, `Alt+Q` quote, `Alt+S/C/H` strike/code/highlight.
- **Verification**: 968,746 edit operations (insert, split, delete, merge, nest, convert, format, link, range-delete at start/middle/end of every block) over every editable real note: no panic and every result re-parses to itself. 7.6 million cursor positions (every position of every note at widths 20/45/90) map to a screen cell and back into the same block (52 sit inside objects clipped to a narrow table column). Typing costs ~3.6 ms per key including redraw on an 85 KB / 535-block note (release). The opt-in tools are `RICH_NOTES_DIR=<dir of exported note HTML> cargo test --release real_notes -- --ignored --nocapture` (core and tui), `show_note` (`RICH_SHOW=file.html`) and `type_speed`.
- **Bugs the tests found on the way**: range delete used a stale block index after removing middle blocks; deleting inside a to-do item dropped its label structure; split left a plain space at a block edge (collapses in HTML); the remembered Up/Down column survived horizontal moves; typing after a save merged into the pre-save undo step; an oversized object in a narrow table column pushed the cursor into the next cell.

## 35. Links, pictures and inline pictures in the editor (2026-10-14)
- **Note links** (`Alt+L`): the note picker; without a selection it inserts Trilium's `<a class="reference-link" href="#root/<id>">Title</a>` (an atom in the editor, shown by its title), with a selection the text becomes a plain internal link. **URL links** (`Ctrl+K`) now also work with nothing selected: the address is inserted as the link text. `Ctrl+O` / `Ctrl+Enter` follow the link under the cursor (note links leave the editor only when nothing is unsaved; URLs open with xdg-open) or zoom a picture.
- **Pictures** (`Alt+P`): a file path (quotes, `file://`, `\ ` escapes and `~/` are understood, so a file dropped onto the terminal works) or, when empty, the clipboard's PNG via `wl-paste` / `xclip`. PNG/JPEG/GIF/WebP by magic bytes, readable header, at most 20 MB. The bytes become an attachment (role `image`; encrypted for protected notes, verified) and the note gets `<figure class="image"><img src="api/attachments/<id>/image/<name>"></figure>` when the cursor is in a plain paragraph, an inline `<img>` elsewhere. Attachments created during an edit session are tracked and deleted again when the editor closes without the saved note referencing them (discarded edit, removed picture); a killed app keeps them because the recovery text refers to them.
- **Inline display** (`Alt+V`, also in the palette; default off, remembered in config.json): both views reserve rows under a line that is only a picture and draw it there; the label stays as a caption line (that is where the cursor, `]` selection and Enter-zoom live). Sizes are fitted into at most 2/3 of the pane height and the pane width, never past the picture's own size, except for half-blocks, whose natural size is one pixel across and two down per cell. Pictures are decoded when first drawn and at most 16 are kept; only pictures that fit on screen entirely are drawn; remote, SVG and undecodable pictures stay labels. The editor lays out with a host trait (`ImageHost`) so key handling works without one.
- **Found on the way**: the half-block encoder writes a space with a coloured background for flat colour (tests must look at colours, not `▀`); `StatefulImage` with `Fit` never enlarges, so reserved areas need `Scale`; the reading view's `[image: name]` is wrapped into several spans sharing one link id.
- **Config safety**: the app only writes the config file when run as the real program (`persist_settings`); tests use temp paths.

