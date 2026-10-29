//! SOPS + age fixtures written the way SOPS writes them, for every test that decrypts one
//! (T-0136, T-2526). A committed encrypted fixture would need its private key committed beside
//! it, so each test generates a keypair and seals its own file with these.

use aes_gcm::aead::{consts::U32, Aead, Payload};
use aes_gcm::KeyInit;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use sha2::{Digest, Sha512};

pub type SopsCipher = aes_gcm::AesGcm<aes_gcm::aes::Aes256, U32>;

pub const DATA_KEY: [u8; 32] = [7u8; 32];
pub const LAST_MODIFIED: &str = "2026-09-06T12:00:00Z";

/// What a secrets file holds under one name.
pub enum Node {
    Value(&'static str),
    Keys(&'static [(&'static str, &'static str)]),
}

/// Encrypts one value the way SOPS does: the path of the value is the additional data, so
/// a ciphertext moved to another field no longer authenticates.
pub fn seal(plaintext: &str, aad: &str, nonce_seed: u8) -> String {
    let cipher = SopsCipher::new(aes_gcm::Key::<SopsCipher>::from_slice(&DATA_KEY));
    let nonce = [nonce_seed; 32];
    let sealed = cipher
        .encrypt(
            aes_gcm::Nonce::<U32>::from_slice(&nonce),
            Payload {
                msg: plaintext.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .expect("encrypt");
    let (data, tag) = sealed.split_at(sealed.len() - 16);
    format!(
        "ENC[AES256_GCM,data:{},iv:{},tag:{},type:str]",
        BASE64.encode(data),
        BASE64.encode(nonce),
        BASE64.encode(tag),
    )
}

/// Writes a SOPS file for `recipient` holding `entries`, in the order given.
pub fn sops_file(entries: &[(&str, Node)], recipient: &age::x25519::Recipient) -> String {
    let mut yaml = String::new();
    let mut mac = Sha512::new();
    let mut seed = 1u8;

    for (name, node) in entries {
        match node {
            Node::Value(value) => {
                mac.update(value.as_bytes());
                yaml.push_str(&format!(
                    "{name}: {}\n",
                    seal(value, &format!("{name}:"), seed)
                ));
                seed += 1;
            }
            Node::Keys(keys) => {
                yaml.push_str(&format!("{name}:\n"));
                for (key, value) in *keys {
                    mac.update(value.as_bytes());
                    let literal = seal(value, &format!("{name}:{key}:"), seed);
                    yaml.push_str(&format!("    {key}: {literal}\n"));
                    seed += 1;
                }
            }
        }
    }

    let mut digest = String::new();
    for byte in mac.finalize() {
        digest.push_str(&format!("{byte:02X}"));
    }

    let armored = age::encrypt_and_armor(recipient, &DATA_KEY).expect("wrap data key");
    let indented: String = armored
        .lines()
        .map(|line| format!("            {line}\n"))
        .collect();

    yaml.push_str("sops:\n    age:\n");
    yaml.push_str(&format!(
        "        - recipient: {recipient}\n          enc: |\n{indented}"
    ));
    yaml.push_str(&format!("    lastmodified: \"{LAST_MODIFIED}\"\n"));
    yaml.push_str(&format!("    mac: {}\n", seal(&digest, LAST_MODIFIED, 200)));
    yaml.push_str("    unencrypted_suffix: _unencrypted\n    version: 3.10.2\n");
    yaml
}
