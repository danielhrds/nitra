//! A fixed key signs a fixed challenge: the room server's own tests check
//! this exact signature (`worker/test/worker.test.ts`), so the app and the
//! server build the signed message byte for byte the same.

use ed25519_dalek::{Signer, SigningKey};
use rooms::protocol::{auth_message, base32, hex};

const ROOM: &str = "abcdefghijklmnopqrstuvwxyz";
/// The secret key: 32 bytes of 7 (test only).
const SECRET: [u8; 32] = [7; 32];
const KEY: &str = "5jfgyy7ctrjavpxvkb5rglwf7gkuo5vox27hxescd3vgsfcg2iwa";
const SIGNATURE: &str = "1005512887a20e0083e01c726a4152a677a06c484b7152a98b6d960fe51682ac\
                         c468982bebe2bf3ccbdf15e1740ff4c9032bf32f52270cbe2188438b9e49ed0b";

#[test]
fn the_entry_signature_matches_the_shared_vector() {
    let key = SigningKey::from_bytes(&SECRET);
    let challenge: Vec<u8> = (0..32).collect();
    assert_eq!(base32(key.verifying_key().as_bytes()), KEY);
    assert_eq!(
        hex(&key.sign(&auth_message(ROOM, &challenge)).to_bytes()),
        SIGNATURE
    );
}
