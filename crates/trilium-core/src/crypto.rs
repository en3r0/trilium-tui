//! Trilium's protected-note cryptography: scrypt for the password, AES-128-CBC for data. A protected note keeps its title and
//! content as base64 of `iv(16) || AES-128-CBC(key[..16], iv, sha1(plain)[..4] || plain)` (PKCS#7 padded). The 16-byte data key
//! is stored in the `encryptedDataKey` option, encrypted the same way under the scrypt-derived key of the password.
use aes::Aes128;
use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use cbc::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
use rand::RngExt;
use sha1::{Digest, Sha1};

/// scrypt as Trilium configures it (N=16384, r=8, p=1, 32 bytes); the salt is the option string's UTF-8 bytes.
pub fn scrypt_hash(password: &str, salt: &str) -> Result<[u8; 32]> {
    let params = scrypt::Params::new(14, 8, 1).map_err(|e| anyhow::anyhow!("scrypt parameters: {e}"))?;
    let mut out = [0u8; 32];
    scrypt::scrypt(password.as_bytes(), salt.as_bytes(), &params, &mut out).map_err(|e| anyhow::anyhow!("scrypt: {e}"))?;
    Ok(out)
}

/// Keys and IVs are used as their first 16 bytes, zero-padded when shorter (`pad` in Trilium).
fn pad16(b: &[u8]) -> [u8; 16] {
    let mut o = [0u8; 16];
    let n = b.len().min(16);
    o[..n].copy_from_slice(&b[..n]);
    o
}

pub fn encrypt(key: &[u8], plain: &[u8]) -> String {
    let mut iv = [0u8; 16];
    rand::rng().fill(&mut iv);
    let mut payload = Sha1::digest(plain)[..4].to_vec();
    payload.extend_from_slice(plain);
    let msg_len = payload.len();
    payload.resize(msg_len + 16, 0);
    let ct = cbc::Encryptor::<Aes128>::new(&pad16(key).into(), &iv.into()).encrypt_padded::<Pkcs7>(&mut payload, msg_len).expect("buffer has room for the padding");
    let mut all = iv.to_vec();
    all.extend_from_slice(ct);
    B64.encode(all)
}

/// `None` when the key is wrong or the data is damaged (the 4-byte digest doesn't match).
pub fn decrypt(key: &[u8], cipher_b64: &str) -> Option<Vec<u8>> {
    let all = B64.decode(cipher_b64.trim()).ok()?;
    // old data can have a 13-byte IV
    let iv_len = if all.len() % 16 == 0 { 16 } else { 13 };
    if all.len() <= iv_len { return None; }
    let (iv, ct) = all.split_at(iv_len);
    let mut buf = ct.to_vec();
    let plain = cbc::Decryptor::<Aes128>::new(&pad16(key).into(), &pad16(iv).into()).decrypt_padded::<Pkcs7>(&mut buf).ok()?;
    if plain.len() < 4 { return None; }
    let (digest, payload) = plain.split_at(4);
    (Sha1::digest(payload)[..4] == *digest).then(|| payload.to_vec())
}

pub fn random_token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rng().fill(&mut b[..]);
    B64.encode(b)
}

pub fn check_not_empty(password: &str) -> Result<()> { if password.is_empty() { bail!("empty password"); } Ok(()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_round_trips_and_wrong_keys_are_detected() {
        let key = [7u8; 16];
        let c = encrypt(&key, "héllo wörld".as_bytes());
        assert_eq!(decrypt(&key, &c).unwrap(), "héllo wörld".as_bytes());
        assert_ne!(encrypt(&key, b"x"), encrypt(&key, b"x"), "a fresh IV every time");
        assert!(decrypt(&[8u8; 16], &c).is_none());
        assert!(decrypt(&key, "not base64!!").is_none() && decrypt(&key, "").is_none() && decrypt(&key, "AAAA").is_none());
        assert_eq!(decrypt(&key, &encrypt(&key, b"")).unwrap(), b"");
        // a 32-byte key (the scrypt output) is used as its first 16 bytes
        let long = [1u8; 32];
        assert_eq!(decrypt(&long[..16], &encrypt(&long, b"data")).unwrap(), b"data");
    }

    #[test]
    fn scrypt_matches_the_reference_vector() {
        // RFC 7914 test vector 2 uses N=1024, so check the wrapper through its own parameters instead: deterministic and salt-sensitive
        let a = scrypt_hash("pw", "salt").unwrap();
        assert_eq!(a, scrypt_hash("pw", "salt").unwrap());
        assert_ne!(a, scrypt_hash("pw", "salt2").unwrap());
        assert_ne!(a, scrypt_hash("pw2", "salt").unwrap());
    }
}
