use std::ffi::{CStr, CString};

use stellarconduit_core::ffi::{
    sc_create_envelope, sc_free_bytes, sc_free_string, sc_generate_identity,
};

#[test]
fn test_ffi_sc_generate_identity() {
    let raw_ptr = sc_generate_identity();
    assert!(!raw_ptr.is_null());

    let c_str = unsafe { CStr::from_ptr(raw_ptr) };
    let hex_str = c_str.to_str().expect("Expected valid utf8 string");

    // Ed25519 public key in hex should be 64 characters long
    assert_eq!(hex_str.len(), 64);
    assert!(hex::decode(hex_str).is_ok());

    sc_free_string(raw_ptr);
}

#[test]
fn test_ffi_sc_create_envelope() {
    // Generate a valid 32-byte seed in hex
    let seed_hex = hex::encode([1u8; 32]);
    let c_seed = CString::new(seed_hex).unwrap();
    let c_tx_xdr = CString::new("AAAAAQAAAAAAAAAA").unwrap();

    let mut out_len: usize = 0;
    let out_ptr = sc_create_envelope(c_tx_xdr.as_ptr(), c_seed.as_ptr(), &mut out_len);

    assert!(!out_ptr.is_null());
    assert!(out_len > 0);

    // Reconstruct the slice to check if it parses
    let byte_slice = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
    let _: stellarconduit_core::message::types::ProtocolMessage =
        rmp_serde::from_slice(byte_slice).expect("Failed to decode ProtocolMessage");

    sc_free_bytes(out_ptr, out_len);
}

#[test]
fn test_ffi_sc_create_envelope_invalid_seed() {
    let c_seed = CString::new("invalid_hex_string").unwrap();
    let c_tx_xdr = CString::new("AAAAAQAAAAAAAAAA").unwrap();

    let mut out_len: usize = 0;
    let out_ptr = sc_create_envelope(c_tx_xdr.as_ptr(), c_seed.as_ptr(), &mut out_len);

    assert!(out_ptr.is_null());
}
