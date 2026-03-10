//! C-compatible Foreign Function Interface for mobile SDKs (Android/iOS).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use ed25519_dalek::SigningKey;

use crate::message::signing::sign_envelope;
use crate::message::types::{ProtocolMessage, TransactionEnvelope};

/// Initializes a new node identity, returning the public key as a hex string.
/// Caller is responsible for calling `sc_free_string()` on the returned pointer.
#[no_mangle]
pub extern "C" fn sc_generate_identity() -> *mut c_char {
    let signing_key = SigningKey::generate(&mut rand::thread_rng());
    let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());
    CString::new(pubkey_hex).unwrap().into_raw()
}

/// Creates a signed `TransactionEnvelope` from a raw XDR string and a keypair seed.
/// Returns the serialized MessagePack bytes.
/// Caller is responsible for calling `sc_free_bytes()` on the returned pointer.
#[no_mangle]
pub extern "C" fn sc_create_envelope(
    tx_xdr_ptr: *const c_char,
    secret_seed_ptr: *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    if tx_xdr_ptr.is_null() || secret_seed_ptr.is_null() || out_len.is_null() {
        return ptr::null_mut();
    }

    let tx_xdr_c_str = unsafe { CStr::from_ptr(tx_xdr_ptr) };
    let tx_xdr = match tx_xdr_c_str.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return ptr::null_mut(),
    };

    let secret_seed_c_str = unsafe { CStr::from_ptr(secret_seed_ptr) };
    let secret_seed_hex = match secret_seed_c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let seed_bytes = match hex::decode(secret_seed_hex) {
        Ok(b) if b.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&b);
            arr
        }
        _ => return ptr::null_mut(),
    };

    let signing_key = SigningKey::from_bytes(&seed_bytes);
    let origin_pubkey = signing_key.verifying_key().to_bytes();

    // Create a message ID
    let mut message_id = [0u8; 32];
    message_id[0..4].copy_from_slice(&origin_pubkey[0..4]);
    // A simplified deterministic ID for the envelope.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut envelope = TransactionEnvelope {
        message_id,
        origin_pubkey,
        tx_xdr,
        ttl_hops: 10,
        timestamp,
        signature: [0u8; 64],
    };

    if sign_envelope(&signing_key, &mut envelope).is_err() {
        return ptr::null_mut();
    }

    let msg = ProtocolMessage::Transaction(envelope);
    let bytes = match msg.to_bytes() {
        Ok(b) => b,
        Err(_) => return ptr::null_mut(),
    };

    let mut bytes_vec = bytes.into_boxed_slice();
    let ptr = bytes_vec.as_mut_ptr();
    let len = bytes_vec.len();
    std::mem::forget(bytes_vec);

    unsafe {
        *out_len = len;
    }

    ptr
}

/// Frees a string previously allocated by Rust.
#[no_mangle]
pub extern "C" fn sc_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = CString::from_raw(ptr);
    }
}

/// Frees a byte array previously allocated by Rust.
#[no_mangle]
pub extern "C" fn sc_free_bytes(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    unsafe {
        let _ = Vec::from_raw_parts(ptr, len, len);
    }
}
