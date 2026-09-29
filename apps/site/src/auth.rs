use std::sync::OnceLock;

use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
    password_hash::{SaltString, rand_core::OsRng},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};

const PASSWORD_MEMORY_KIB: u32 = 19 * 1024;
const PASSWORD_ITERATIONS: u32 = 2;
const PASSWORD_LANES: u32 = 1;
const SESSION_TOKEN_BYTES: usize = 32;
const SHARE_LINK_ID_BYTES: usize = 16;

pub(crate) fn validate_username(username: &str) -> Result<(), &'static str> {
    if !(3..=32).contains(&username.len())
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("usernames must be 3 to 32 ASCII letters, numbers, dots, underscores, or hyphens");
    }
    Ok(())
}

pub(crate) fn hash_password(password: &str) -> Result<String, String> {
    if password.len() > 1024 {
        return Err("passwords must be at most 1024 bytes".to_owned());
    }
    let salt = SaltString::generate(&mut OsRng);
    password_hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| format!("password hashing failed: {error}"))
}

pub(crate) fn verify_password(password: &str, encoded_hash: &str) -> bool {
    let Ok(hash) = PasswordHash::new(encoded_hash) else {
        return false;
    };
    password_hasher()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

pub(crate) fn dummy_verify_password(password: &str) {
    static DUMMY_HASH: OnceLock<String> = OnceLock::new();
    let encoded_hash = DUMMY_HASH.get_or_init(|| {
        let salt = SaltString::encode_b64(b"journey-site-dummy-salt")
            .expect("the fixed dummy salt is valid");
        password_hasher()
            .hash_password(b"not-a-real-account-password", &salt)
            .expect("the dummy password hash can be generated")
            .to_string()
    });
    let _ = verify_password(password, encoded_hash);
}

pub(crate) fn new_session_token() -> String {
    let mut bytes = [0_u8; SESSION_TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn new_share_link_secret() -> String {
    new_session_token()
}

pub(crate) fn new_share_link_id() -> String {
    let mut bytes = [0_u8; SHARE_LINK_ID_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn session_token_digest(token: &str) -> String {
    hex_digest(Sha256::digest(token.as_bytes()).as_slice())
}

pub(crate) fn username_throttle_key(username: &str) -> String {
    hex_digest(
        Sha256::digest(format!("username:{}", username.trim().to_ascii_lowercase()).as_bytes())
            .as_slice(),
    )
}

pub(crate) fn address_throttle_key(address: &str) -> String {
    hex_digest(Sha256::digest(format!("address:{address}").as_bytes()).as_slice())
}

fn password_hasher() -> Argon2<'static> {
    let params = Params::new(
        PASSWORD_MEMORY_KIB,
        PASSWORD_ITERATIONS,
        PASSWORD_LANES,
        None,
    )
    .expect("the configured Argon2id parameters are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
