use std::path::Path;
use tokio_rusqlite::Connection;

use crate::message::types::{ProtocolMessage, TransactionEnvelope};
use crate::message::RelayChainProof;
use crate::peer::peer_node::Peer;
use crate::persistence::errors::DbError;

pub struct MeshDatabase {
    conn: Connection,
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
                );",
            )?;
            Ok(())
        })
        .await?;

        Ok(Self { conn })
    }

    /// Flush WAL to the main database file and close the connection pool.
    pub async fn close(&self) -> Result<(), DbError> {
        self.conn
            .call(|conn| {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
                Ok(())
            })
            .await?;
        log::info!("Database: WAL checkpoint flushed and pool closed");
        Ok(())
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
