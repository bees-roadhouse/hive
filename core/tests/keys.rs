// Key-management tests (PR 1.4). Hermetic except the #[ignore]d keychain
// round-trip, which needs a real OS keychain (Secret Service on Linux) and
// therefore can't run on headless CI — run it manually on a desktop:
//   cargo test -p hive-core --test keys -- --ignored

use hive_core::keys::{self, KeySource, KeychainKeySource, MemoryKeySource, WRAPPED_KEY_LEN};

#[test]
fn memory_keysource_returns_its_fixed_key() {
    let ks = MemoryKeySource([9u8; 32]);
    assert_eq!(ks.master_key().unwrap(), [9u8; 32]);
}

#[test]
fn wrap_unwrap_roundtrip_and_tamper_resistance() {
    let master = [1u8; 32];
    let inner = [2u8; 32];

    let wrapped = keys::wrap_key(&master, &inner).unwrap();
    assert_eq!(wrapped.len(), WRAPPED_KEY_LEN);
    assert_eq!(keys::unwrap_key(&master, &wrapped).unwrap(), inner);

    // Deterministic: wrapping is a pure function (the blockstore's dedup
    // relies on byte-identical BlobRefs).
    assert_eq!(wrapped, keys::wrap_key(&master, &inner).unwrap());

    // Wrong wrapping key fails.
    assert!(keys::unwrap_key(&[3u8; 32], &wrapped).is_err());

    // Any flipped bit fails authentication.
    let mut bent = wrapped.clone();
    bent[30] ^= 0x40;
    assert!(keys::unwrap_key(&master, &bent).is_err());

    // Wrong length is rejected outright.
    assert!(keys::unwrap_key(&master, &wrapped[..WRAPPED_KEY_LEN - 1]).is_err());
}

#[test]
fn passphrase_wrap_roundtrip_wrong_passphrase_fails() {
    let master = [7u8; 32];
    let container = keys::passphrase_wrap(&master, "correct horse battery staple").unwrap();
    // version(1) + m/t/p params(12) + salt(16) + wrapped key(72)
    assert_eq!(container.len(), 1 + 12 + 16 + WRAPPED_KEY_LEN);

    let restored = keys::passphrase_unwrap(&container, "correct horse battery staple").unwrap();
    assert_eq!(restored, master);

    assert!(keys::passphrase_unwrap(&container, "wrong passphrase").is_err());
    assert!(keys::passphrase_unwrap(
        &container[..container.len() - 1],
        "correct horse battery staple"
    )
    .is_err());

    // Fresh salt per export: two exports differ, both restore.
    let container2 = keys::passphrase_wrap(&master, "correct horse battery staple").unwrap();
    assert_ne!(container, container2);
    assert_eq!(
        keys::passphrase_unwrap(&container2, "correct horse battery staple").unwrap(),
        master
    );
}

#[test]
fn recovery_code_roundtrip_and_forgiving_parse() {
    let master = *blake3::hash(b"recovery fixture").as_bytes();
    let code = keys::recovery_code(&master);

    // 32 bytes → 52 base32 chars → 13 dash-joined groups of 4.
    let groups: Vec<&str> = code.split('-').collect();
    assert_eq!(groups.len(), 13);
    assert!(groups.iter().all(|g| g.len() == 4));
    assert!(code
        .chars()
        .all(|c| c == '-' || c.is_ascii_digit() || c.is_ascii_uppercase()));

    assert_eq!(keys::parse_recovery_code(&code).unwrap(), master);
    // Lowercase, stray whitespace, missing dashes: all fine.
    let sloppy = code.to_ascii_lowercase().replace('-', " ");
    assert_eq!(keys::parse_recovery_code(&sloppy).unwrap(), master);

    assert!(keys::parse_recovery_code("not-a-code").is_err());
    assert!(keys::parse_recovery_code("").is_err());
}

/// Live keychain round-trip. #[ignore]d: headless CI has no Secret Service;
/// KeychainKeySource is otherwise compile-tested by existing. Uses a
/// throwaway service/user pair and cleans up after itself.
#[test]
#[ignore = "needs a real OS keychain (Secret Service/Keychain/CredMan); run manually on a desktop"]
fn keychain_keysource_generates_then_returns_stable_key() {
    let service = "hive-test";
    let user = format!("master-key-test-{}", std::process::id());
    let ks = KeychainKeySource::with_service_user(service, &user);

    let first = ks.master_key().expect("generate + store on first use");
    let second = ks.master_key().expect("read back on second use");
    assert_eq!(first, second, "keychain must return the stored key");

    // Cleanup so repeated runs start fresh.
    keyring::Entry::new(service, &user)
        .and_then(|e| e.delete_credential())
        .expect("cleanup test keychain entry");
}
