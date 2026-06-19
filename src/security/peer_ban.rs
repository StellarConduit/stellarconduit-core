use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use crate::discovery::peer_list::PeerList;
use crate::persistence::db::MeshDatabase;
use crate::persistence::errors::DbError;

fn now_unix_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Ban a peer in memory and optionally persist the ban to the database.
/// Inserts the peer into the list first if not already present.
pub async fn ban_and_persist(
    peer_list: &Arc<Mutex<PeerList>>,
    db: Option<&MeshDatabase>,
    pubkey: &[u8; 32],
    duration_sec: u64,
    reason: &str,
) {
    let expires_at = now_unix_sec() + duration_sec;
    {
        let mut guard = peer_list.lock().await;
        if guard.get_peer(pubkey).is_none() {
            guard.insert_or_update(*pubkey, 0);
        }
        guard.ban_peer(pubkey, duration_sec);
    }
    if let Some(db) = db {
        if let Err(e) = db.save_ban(pubkey, expires_at, reason).await {
            log::warn!("Failed to persist ban for {:?}: {:?}", pubkey, e);
        }
    }
}

/// Load all active bans from the database and re-populate the peer list.
/// Returns the number of bans that were restored.
pub async fn restore_bans_from_db(
    peer_list: &Arc<Mutex<PeerList>>,
    db: &MeshDatabase,
) -> Result<usize, DbError> {
    let active_bans = db.load_active_bans().await?;
    let now = now_unix_sec();
    let count = active_bans.len();
    let mut guard = peer_list.lock().await;
    for ban in active_bans {
        if ban.expires_at > now {
            let remaining_secs = ban.expires_at - now;
            if guard.get_peer(&ban.pubkey).is_none() {
                guard.insert_or_update(ban.pubkey, 0);
            }
            guard.ban_peer(&ban.pubkey, remaining_secs);
        }
    }
    Ok(count)
}
