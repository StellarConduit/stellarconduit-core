use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_rusqlite::Connection;

use crate::message::types::{ProtocolMessage, TransactionEnvelope};
use crate::message::RelayChainProof;
use crate::peer::peer_node::Peer;
use crate::persistence::errors::DbError;

pub struct MeshDatabase {
    conn: Connection,
}

pub struct PersistedBan {
    pub pubkey: [u8; 32],
    pub expires_at: u64,
    pub reason: String,
}

impl MeshDatabase {
    /// Initialize the embedded SQLite database, creating tables if they don't exist.
    pub async fn init(db_path: &str) -> Result<Self, DbError> {
        let conn = if db_path == ":memory:" {
            Connection::open_in_memory().await?
        } else {
            Connection::open(Path::new(db_path)).await?
        };

        conn.call(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS peers (
                    pubkey BLOB PRIMARY KEY,
                    reputation INTEGER,
                    last_seen_sec INTEGER,
                    is_banned BOOLEAN,
                    supported_transports INTEGER,
                    is_relay_node BOOLEAN,
                    bytes_sent INTEGER,
                    bytes_received INTEGER
                );

                CREATE TABLE IF NOT EXISTS topology_edges (
                    source_pubkey BLOB,
                    target_pubkey BLOB,
                    last_updated_sec INTEGER,
                    PRIMARY KEY (source_pubkey, target_pubkey)
                );

                CREATE TABLE IF NOT EXISTS pending_messages (
                    message_id BLOB PRIMARY KEY,
                    envelope_bytes BLOB,
                    ttl_hops INTEGER,
                    timestamp_sec INTEGER
                );

                CREATE TABLE IF NOT EXISTS relay_proofs (
                    tx_id BLOB PRIMARY KEY,
                    signature BLOB NOT NULL,
                    chain_hash BLOB NOT NULL,
                    sequence INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS banned_peers (
                    pubkey      BLOB NOT NULL PRIMARY KEY,
                    banned_at   INTEGER NOT NULL,
                    expires_at  INTEGER NOT NULL,
                    reason      TEXT NOT NULL
                );",
            )?;
            Ok(())
        })
        .await?;

        Ok(Self { conn })
    }

    /// Insert or update a Peer in the database.
    pub async fn save_peer(&self, peer: &Peer) -> Result<(), DbError> {
        let pubkey = peer.identity.pubkey;
        let reputation = peer.reputation;
        let last_seen_sec = peer.last_seen_unix_sec;
        let is_banned = peer.is_banned;
        let transports = peer.supported_transports as u32;
        let is_relay = peer.is_relay_node;
        let sent = peer.bytes_sent as i64;
        let recvd = peer.bytes_received as i64;

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO peers (
                        pubkey, reputation, last_seen_sec, is_banned,
                        supported_transports, is_relay_node, bytes_sent, bytes_received
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                    ON CONFLICT(pubkey) DO UPDATE SET
                        reputation=excluded.reputation,
                        last_seen_sec=excluded.last_seen_sec,
                        is_banned=excluded.is_banned,
                        supported_transports=excluded.supported_transports,
                        is_relay_node=excluded.is_relay_node,
                        bytes_sent=excluded.bytes_sent,
                        bytes_received=excluded.bytes_received",
                    rusqlite::params![
                        pubkey,
                        reputation,
                        last_seen_sec,
                        is_banned,
                        transports,
                        is_relay,
                        sent,
                        recvd,
                    ],
                )?;
                Ok(())
            })
            .await?;

        Ok(())
    }

    /// Load all peers from the database.
    pub async fn load_all_peers(&self) -> Result<Vec<Peer>, DbError> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT * FROM peers")?;
                let peer_iter = stmt.query_map([], |row| {
                    let pubkey_vec: Vec<u8> = row.get(0)?;
                    let mut pubkey = [0u8; 32];
                    if pubkey_vec.len() == 32 {
                        pubkey.copy_from_slice(&pubkey_vec);
                    }

                    let transports: u32 = row.get(4)?;
                    let sent: i64 = row.get(6)?;
                    let recvd: i64 = row.get(7)?;

                    let mut peer = Peer::new(pubkey);
                    peer.reputation = row.get(1)?;
                    peer.last_seen_unix_sec = row.get(2)?;
                    peer.is_banned = row.get(3)?;
                    peer.supported_transports = transports as u8;
                    peer.is_relay_node = row.get(5)?;
                    peer.bytes_sent = sent as u64;
                    peer.bytes_received = recvd as u64;

                    Ok(peer)
                })?;

                let mut peers = Vec::new();
                for peer_result in peer_iter {
                    peers.push(peer_result?);
                }
                Ok(peers)
            })
            .await
            .map_err(Into::into)
    }

    /// Insert a TransactionEnvelope into the pending messages queue.
    pub async fn save_envelope(&self, envelope: &TransactionEnvelope) -> Result<(), DbError> {
        let msg_id = envelope.message_id;
        let hops = envelope.ttl_hops;
        let ts = envelope.timestamp;

        // Serialize the envelope using ProtocolMessage
        let pm = ProtocolMessage::Transaction(envelope.clone());
        let env_bytes = pm.to_bytes().map_err(DbError::from)?;

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO pending_messages (message_id, envelope_bytes, ttl_hops, timestamp_sec)
                    VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(message_id) DO NOTHING",
                    rusqlite::params![msg_id, env_bytes, hops, ts],
                )?;
                Ok(())
            })
            .await?;

        Ok(())
    }

    /// Retrieve all pending transaction envelopes.
    pub async fn load_pending_envelopes(&self) -> Result<Vec<TransactionEnvelope>, DbError> {
        self.conn
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT envelope_bytes FROM pending_messages")?;
                let env_iter = stmt.query_map([], |row| {
                    let bytes: Vec<u8> = row.get(0)?;
                    Ok(bytes)
                })?;

                let mut envelopes = Vec::new();
                for bytes_result in env_iter {
                    let bytes = bytes_result?;
                    envelopes.push(bytes);
                }
                Ok(envelopes)
            })
            .await?
            .into_iter()
            .map(|bytes| {
                let pm = ProtocolMessage::from_bytes(&bytes).map_err(DbError::from)?;
                match pm {
                    ProtocolMessage::Transaction(env) => Ok(env),
                    _ => Err(DbError::InvalidMessageId),
                }
            })
            .collect::<Result<Vec<TransactionEnvelope>, DbError>>()
    }

    /// Delete a successfully routed or expired message from the queue.
    pub async fn delete_envelope(&self, message_id: &[u8; 32]) -> Result<usize, DbError> {
        let msg_id = *message_id;
        let count = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM pending_messages WHERE message_id = ?1",
                    rusqlite::params![msg_id],
                )?;
                Ok(count)
            })
            .await?;
        Ok(count)
    }

    pub async fn save_relay_proof(
        &self,
        tx_id: &[u8; 32],
        proof: &RelayChainProof,
    ) -> Result<(), DbError> {
        let sequence = i64::try_from(proof.sequence).map_err(|_| {
            DbError::InvalidRelayProof("sequence exceeds SQLite INTEGER range".to_string())
        })?;
        let tx_id = *tx_id;
        let signature = proof.signature;
        let chain_hash = proof.chain_hash;

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO relay_proofs (tx_id, signature, chain_hash, sequence)
                    VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(tx_id) DO UPDATE SET
                        signature=excluded.signature,
                        chain_hash=excluded.chain_hash,
                        sequence=excluded.sequence",
                    rusqlite::params![&tx_id[..], &signature[..], &chain_hash[..], sequence],
                )?;
                Ok(())
            })
            .await?;

        Ok(())
    }

    pub async fn get_relay_proof(
        &self,
        tx_id: &[u8; 32],
    ) -> Result<Option<RelayChainProof>, DbError> {
        let tx_id = *tx_id;
        let row = self
            .conn
            .call(move |conn| {
                use rusqlite::OptionalExtension;

                Ok(conn
                    .query_row(
                        "SELECT signature, chain_hash, sequence FROM relay_proofs WHERE tx_id = ?1",
                        rusqlite::params![&tx_id[..]],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, Vec<u8>>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;

        row.map(|(signature, chain_hash, sequence)| {
            let signature = vec_to_array::<64>(signature, "signature")?;
            let chain_hash = vec_to_array::<32>(chain_hash, "chain_hash")?;
            let sequence = u64::try_from(sequence).map_err(|_| {
                DbError::InvalidRelayProof("sequence cannot be negative".to_string())
            })?;

            Ok(RelayChainProof {
                signature,
                chain_hash,
                sequence,
            })
        })
        .transpose()
    }

    /// Persist a newly banned peer to disk.
    pub async fn save_ban(
        &self,
        pubkey: &[u8; 32],
        expires_at: u64,
        reason: &str,
    ) -> Result<(), DbError> {
        let banned_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let id = *pubkey;
        let reason = reason.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO banned_peers (pubkey, banned_at, expires_at, reason)
                    VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(pubkey) DO UPDATE SET
                        banned_at=excluded.banned_at,
                        expires_at=excluded.expires_at,
                        reason=excluded.reason",
                    rusqlite::params![id, banned_at as i64, expires_at as i64, reason],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Remove a ban record (called when a ban expires or is manually lifted).
    pub async fn remove_ban(&self, pubkey: &[u8; 32]) -> Result<(), DbError> {
        let id = *pubkey;
        self.conn
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM banned_peers WHERE pubkey = ?1",
                    rusqlite::params![id],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Load all bans that have not yet expired.
    pub async fn load_active_bans(&self) -> Result<Vec<PersistedBan>, DbError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT pubkey, expires_at, reason FROM banned_peers WHERE expires_at > ?1",
                )?;
                let ban_iter = stmt.query_map(rusqlite::params![now as i64], |row| {
                    let pubkey_vec: Vec<u8> = row.get(0)?;
                    let expires_at: i64 = row.get(1)?;
                    let reason: String = row.get(2)?;
                    Ok((pubkey_vec, expires_at, reason))
                })?;

                let mut bans = Vec::new();
                for result in ban_iter {
                    let (pubkey_vec, expires_at, reason) = result?;
                    let mut pubkey = [0u8; 32];
                    if pubkey_vec.len() == 32 {
                        pubkey.copy_from_slice(&pubkey_vec);
                    }
                    bans.push(PersistedBan {
                        pubkey,
                        expires_at: expires_at as u64,
                        reason,
                    });
                }
                Ok(bans)
            })
            .await
            .map_err(Into::into)
    }

    pub async fn mark_peer_offline(&self, pubkey: &[u8; 32]) -> Result<usize, DbError> {
        let id_val = *pubkey;
        let count = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "UPDATE peers SET is_banned=1 WHERE pubkey=?1",
                    rusqlite::params![id_val],
                )?;
                Ok(count)
            })
            .await?;
        Ok(count)
    }

    pub async fn delete_messages_older_than(&self, cutoff_ts: u64) -> Result<usize, DbError> {
        let count = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "DELETE FROM pending_messages WHERE timestamp_sec < ?1",
                    rusqlite::params![cutoff_ts as i64],
                )?;
                Ok(count)
            })
            .await?;
        Ok(count)
    }

    pub async fn upsert_edge(
        &self,
        source: &[u8; 32],
        target: &[u8; 32],
        last_updated_sec: u64,
    ) -> Result<usize, DbError> {
        let src = *source;
        let tgt = *target;
        let count = self
            .conn
            .call(move |conn| {
                let count = conn.execute(
                    "INSERT INTO topology_edges (source_pubkey, target_pubkey, last_updated_sec)
                    VALUES (?1, ?2, ?3)
                    ON CONFLICT(source_pubkey, target_pubkey) DO UPDATE SET
                    last_updated_sec=excluded.last_updated_sec",
                    rusqlite::params![src, tgt, last_updated_sec as i64],
                )?;
                Ok(count)
            })
            .await?;
        Ok(count)
    }

    pub async fn get_all_edges_since(
        &self,
        _cutoff: u64,
    ) -> Result<Vec<([u8; 32], [u8; 32])>, DbError> {
        // Mock returning empty for now to satisfy the compiler
        Ok(Vec::new())
    }

    #[cfg(test)]
    pub fn new_stub() -> Self {
        // We initialize a blocking in-memory DB just for synchronous test stubs
        let conn =
            futures::executor::block_on(async { Connection::open_in_memory().await.unwrap() });
        futures::executor::block_on(async {
            conn.call(|c| {
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS peers (
                        pubkey BLOB PRIMARY KEY,
                        reputation INTEGER,
                        last_seen_sec INTEGER,
                        is_banned BOOLEAN,
                        supported_transports INTEGER,
                        is_relay_node BOOLEAN,
                        bytes_sent INTEGER,
                        bytes_received INTEGER
                    );
                    CREATE TABLE IF NOT EXISTS pending_messages (
                        message_id BLOB PRIMARY KEY,
                        envelope_bytes BLOB,
                        ttl_hops INTEGER,
                        timestamp_sec INTEGER
                    );
                    CREATE TABLE IF NOT EXISTS relay_proofs (
                        tx_id BLOB PRIMARY KEY,
                        signature BLOB NOT NULL,
                        chain_hash BLOB NOT NULL,
                        sequence INTEGER NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS banned_peers (
                        pubkey      BLOB NOT NULL PRIMARY KEY,
                        banned_at   INTEGER NOT NULL,
                        expires_at  INTEGER NOT NULL,
                        reason      TEXT NOT NULL
                    );",
                )?;
                Ok(Ok::<(), rusqlite::Error>(())?)
            })
            .await
            .unwrap();
        });
        Self { conn }
    }

    #[cfg(test)]
    pub async fn insert_pending_message(&self, message_id: [u8; 32], timestamp_sec: u64) {
        let env = TransactionEnvelope {
            message_id,
            origin_pubkey: [0; 32],
            tx_xdr: String::new(),
            ttl_hops: 0,
            timestamp: timestamp_sec,
            signature: [0; 64],
        };
        let pm = ProtocolMessage::Transaction(env);
        let env_bytes = pm.to_bytes().unwrap();
        self.conn.call(move |c| {
            c.execute(
                "INSERT INTO pending_messages (message_id, envelope_bytes, ttl_hops, timestamp_sec) VALUES (?1, ?2, 0, ?3)",
                rusqlite::params![message_id, env_bytes, timestamp_sec as i64],
            )?;
            Ok(Ok::<(), rusqlite::Error>(())?)
        }).await.unwrap();
    }

    #[cfg(test)]
    pub async fn pending_message_count(&self) -> usize {
        self.conn
            .call(|c| {
                let count: usize =
                    c.query_row("SELECT COUNT(*) FROM pending_messages", [], |r| r.get(0))?;
                Ok(Ok::<usize, rusqlite::Error>(count)?)
            })
            .await
            .unwrap()
    }

    #[cfg(test)]
    pub async fn offline_peer_count(&self) -> usize {
        self.conn
            .call(|c| {
                let count: usize =
                    c.query_row("SELECT COUNT(*) FROM peers WHERE is_banned=1", [], |r| {
                        r.get(0)
                    })?;
                Ok(Ok::<usize, rusqlite::Error>(count)?)
            })
            .await
            .unwrap()
    }
}

fn vec_to_array<const N: usize>(bytes: Vec<u8>, label: &str) -> Result<[u8; N], DbError> {
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        DbError::InvalidRelayProof(format!("{label} must be {N} bytes, got {}", bytes.len()))
    })
}

#[cfg(test)]
mod ban_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    use crate::discovery::peer_list::PeerList;

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    #[tokio::test]
    async fn test_ban_persisted_to_db() {
        let db = MeshDatabase::init(":memory:").await.unwrap();
        let pubkey = [1u8; 32];
        let expires_at = now_secs() + 3600;

        db.save_ban(&pubkey, expires_at, "invalid signatures")
            .await
            .unwrap();

        let bans = db.load_active_bans().await.unwrap();
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].pubkey, pubkey);
        assert_eq!(bans[0].expires_at, expires_at);
        assert_eq!(bans[0].reason, "invalid signatures");
    }

    #[tokio::test]
    async fn test_ban_removed_after_expiry() {
        let db = Arc::new(MeshDatabase::init(":memory:").await.unwrap());
        let pubkey = [2u8; 32];
        let expires_at = now_secs() + 1;

        db.save_ban(&pubkey, expires_at, "short ban").await.unwrap();

        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        {
            let mut guard = peer_list.lock().await;
            guard.insert_or_update(pubkey, 0);
            guard.ban_peer(&pubkey, 1);
        }

        tokio::time::sleep(Duration::from_secs(2)).await;

        // Simulate the ban_check_timer branch in run_gossip_loop
        let unbanned = {
            let mut guard = peer_list.lock().await;
            guard.check_ban_expirations()
        };
        for peer in &unbanned {
            db.remove_ban(&peer.pubkey).await.unwrap();
        }

        let bans = db.load_active_bans().await.unwrap();
        assert!(bans.is_empty());
    }

    #[tokio::test]
    async fn test_ban_restored_on_startup() {
        let db = MeshDatabase::init(":memory:").await.unwrap();
        let pubkey = [3u8; 32];
        let expires_at = now_secs() + 3600;

        db.save_ban(&pubkey, expires_at, "restore test")
            .await
            .unwrap();

        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        crate::security::peer_ban::restore_bans_from_db(&peer_list, &db)
            .await
            .unwrap();

        let guard = peer_list.lock().await;
        assert!(guard.is_peer_banned(&pubkey));
    }

    #[tokio::test]
    async fn test_expired_ban_not_restored() {
        let db = MeshDatabase::init(":memory:").await.unwrap();
        let pubkey = [4u8; 32];
        // expires_at in the past — load_active_bans will not return it
        let expires_at = now_secs().saturating_sub(10);

        db.save_ban(&pubkey, expires_at, "old ban").await.unwrap();

        let peer_list = Arc::new(Mutex::new(PeerList::new(300)));
        crate::security::peer_ban::restore_bans_from_db(&peer_list, &db)
            .await
            .unwrap();

        let guard = peer_list.lock().await;
        assert!(!guard.is_peer_banned(&pubkey));
    }
}
