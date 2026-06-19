# Implement Persistent Peer Ban Enforcement Across Restarts

Closes #102

## Problem

The peer ban system stored bans only in memory. When the daemon restarted after
detecting a malicious peer, the ban was lost and the attacker could reconnect
immediately — making the 24-hour strike-based ban effectively a "ban until next
crash".

## Solution

Bans are now written to a new `banned_peers` SQLite table at the moment they are
issued and removed from that table when they expire. On every daemon startup the
database is queried and any still-active bans are restored into the in-memory
`PeerList` before the node begins accepting connections.

---

## Changes

### `src/persistence/db.rs`

- **Schema**: Added `banned_peers` table to the migration executed by
  `MeshDatabase::init`.

  ```sql
  CREATE TABLE IF NOT EXISTS banned_peers (
      pubkey      BLOB NOT NULL PRIMARY KEY,
      banned_at   INTEGER NOT NULL,
      expires_at  INTEGER NOT NULL,
      reason      TEXT NOT NULL
  );
  ```

- **`PersistedBan` struct**: New public struct returned by `load_active_bans`.

- **`save_ban(pubkey, expires_at, reason)`**: Upserts a ban record; called
  immediately when a peer is banned so the record survives any restart.

- **`remove_ban(pubkey)`**: Deletes the row; called when a ban expires in memory
  so the table stays tidy.

- **`load_active_bans()`**: Returns only rows whose `expires_at` timestamp is
  in the future; used at startup to restore the in-memory ban set.

- **`new_stub`** (test helper): Updated to include the `banned_peers` table so
  existing unit tests using the stub continue to work.

### `src/security/peer_ban.rs` *(new file)*

Provides two public async functions that keep memory and database in sync:

- **`ban_and_persist(peer_list, db, pubkey, duration_sec, reason)`** — Inserts
  the peer into the list if absent, applies the in-memory ban, then calls
  `db.save_ban()`. The `db` argument is `Option<&MeshDatabase>` so callers
  that do not have a DB handle (e.g., tests) can pass `None` without a code
  path change.

- **`restore_bans_from_db(peer_list, db)`** — Calls `load_active_bans()`, and
  for every returned entry whose `expires_at` is still in the future inserts the
  peer and bans it for the remaining duration. Returns the count of restored
  bans.

### `src/security/mod.rs`

Added `pub mod peer_ban;` to export the new module.

### `src/gossip/protocol.rs`

- **`process_transaction_envelope`**: Added `db: Option<Arc<MeshDatabase>>`
  parameter. When `strike_tracker.record_failure` triggers a ban, the function
  now delegates to `peer_ban::ban_and_persist` (which handles both the memory
  ban and the DB write) before disconnecting the transport.

- **`run_gossip_loop`**: Added `db: Option<Arc<MeshDatabase>>` parameter.
  The 60-second `ban_check_timer` branch now:
  1. Drops the `PeerList` lock before doing async work.
  2. For each peer returned by `check_ban_expirations()`, calls
     `db.remove_ban()` so the expired row is cleaned from the database.

- All existing tests updated to pass `None` for the new `db` parameter —
  no existing test behaviour changes.

### `src/bin/stellarconduitd.rs`

- Replaced the `// TODO: 2. Initialize Database` placeholder with a real
  `MeshDatabase::init(&args.db_path).await?` call.
- Immediately after init, calls `peer_ban::restore_bans_from_db` and logs how
  many active bans were reloaded: `"Restored N active peer ban(s) from database."`.

---

## Tests

All 176 existing lib tests continue to pass. Four new tests were added in
`src/persistence/db.rs::ban_tests`:

| Test | What it verifies |
|---|---|
| `test_ban_persisted_to_db` | `save_ban` writes a row that `load_active_bans` returns |
| `test_ban_removed_after_expiry` | After a 1-second ban expires, `check_ban_expirations` + `remove_ban` leaves `load_active_bans` empty |
| `test_ban_restored_on_startup` | A row written directly to the DB is reflected as an in-memory ban after `restore_bans_from_db` |
| `test_expired_ban_not_restored` | A row with `expires_at` in the past is NOT loaded into `PeerList` during startup restore |

Run with:

```
cargo test --lib persistence
```

---

## Acceptance Criteria

- [x] A peer banned for 24 hours survives a daemon restart.
- [x] At startup, the daemon logs how many active bans were restored.
- [x] Expired bans are removed from the database when they expire in memory.
- [x] A peer whose ban timestamp is already past is NOT loaded on restart.

---

## Commit breakdown

| Commit | File | Description |
|---|---|---|
| `00c96f6` | `src/persistence/db.rs` | Add banned_peers table and ban CRUD methods |
| `9230c27` | `src/security/peer_ban.rs`, `src/security/mod.rs` | Add peer_ban module with persistence-aware helpers |
| `018fb53` | `src/gossip/protocol.rs` | Wire ban persistence into protocol event loop |
| `a12705f` | `src/bin/stellarconduitd.rs` | Initialize database and restore peer bans on startup |
