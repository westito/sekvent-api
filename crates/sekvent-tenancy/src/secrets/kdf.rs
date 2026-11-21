//! HKDF-SHA-256 (RFC 5869) and one block of PBKDF2-HMAC-SHA-256 (RFC 8018),
//! written over the pinned `hmac` and `sha2`.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

/// The output length of SHA-256, in bytes.
const HASH_LEN: usize = 32;

/// The longest HKDF output: 255 blocks of one hash length.
const OKM_MAX: usize = 255 * HASH_LEN;

/// An HMAC-SHA-256 keyed with `key`, ready for its message.
fn keyed(key: &[u8]) -> HmacSha256 {
    HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length")
}

/// The tag of `mac`, wiped on drop.
fn finish(mac: HmacSha256) -> Zeroizing<[u8; HASH_LEN]> {
    Zeroizing::new(mac.finalize().into_bytes().into())
}

/// HMAC-SHA-256 of the concatenation of `parts` under `key`, wiped on drop.
pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Zeroizing<[u8; HASH_LEN]> {
    let mut mac = keyed(key);
    for part in parts {
        mac.update(part);
    }
    finish(mac)
}

/// HKDF-Extract: the pseudorandom key. An empty salt stands for a hash
/// length of zero bytes (RFC 5869, 2.2).
fn extract(salt: &[u8], ikm: &[u8]) -> Zeroizing<[u8; HASH_LEN]> {
    let zeros = [0u8; HASH_LEN];
    let salt = if salt.is_empty() { &zeros[..] } else { salt };
    hmac_sha256(salt, &[ikm])
}

/// HKDF-Expand of `prk` and `info` into `okm` (at most [`OKM_MAX`] bytes).
fn expand(prk: &[u8], info: &[u8], okm: &mut [u8]) {
    let mut previous = Zeroizing::new([0u8; HASH_LEN]);
    let mut previous_len = 0;
    for (index, chunk) in okm.chunks_mut(HASH_LEN).enumerate() {
        let counter = u8::try_from(index + 1).expect("HKDF output is at most 255 blocks");
        let block = hmac_sha256(prk, &[&previous[..previous_len], info, &[counter]]);
        chunk.copy_from_slice(&block[..chunk.len()]);
        previous.copy_from_slice(&block[..]);
        previous_len = HASH_LEN;
    }
}

/// HKDF-SHA-256, extract then expand, into `okm`.
///
/// `okm` is at most 255 hash lengths (8160 bytes) long; a longer one is a
/// bug of the caller and panics.
pub(crate) fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], okm: &mut [u8]) {
    assert!(
        okm.len() <= OKM_MAX,
        "HKDF-SHA-256 yields at most {OKM_MAX} bytes"
    );
    let prk = extract(salt, ikm);
    expand(&prk[..], info, okm);
}

/// The first block of PBKDF2-HMAC-SHA-256: SCRAM's `SaltedPassword`. The
/// result and every intermediate block are wiped on drop.
///
/// `iterations` counts from 1; 0 yields the same block as 1.
#[cfg(feature = "lifecycle")]
pub(crate) fn pbkdf2_sha256_block(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Zeroizing<[u8; 32]> {
    let key = keyed(password);
    let mut first = key.clone();
    first.update(salt);
    first.update(&1u32.to_be_bytes());
    let mut block = finish(first);
    let mut result = Zeroizing::new([0u8; HASH_LEN]);
    result.copy_from_slice(&block[..]);
    for _ in 1..iterations {
        let mut next = key.clone();
        next.update(&block[..]);
        block = finish(next);
        for (out, byte) in result.iter_mut().zip(block.iter()) {
            *out ^= byte;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        assert_eq!(text.len() % 2, 0, "hex text has an even length");
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex digits"))
            .collect()
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").expect("writing to a String succeeds");
            text
        })
    }

    /// One RFC 5869 case: PRK and OKM.
    fn check(ikm: &str, salt: &str, info: &str, length: usize, prk: &str, okm: &str) {
        let (ikm, salt, info) = (hex(ikm), hex(salt), hex(info));
        assert_eq!(hex_of(&extract(&salt, &ikm)[..]), prk);
        let mut out = vec![0u8; length];
        hkdf_sha256(&ikm, &salt, &info, &mut out);
        assert_eq!(hex_of(&out), okm);
    }

    fn range(from: u8, to: u8) -> String {
        hex_of(&(from..=to).collect::<Vec<u8>>())
    }

    #[test]
    fn rfc_5869_a1_basic() {
        check(
            "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
            "000102030405060708090a0b0c",
            "f0f1f2f3f4f5f6f7f8f9",
            42,
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
        );
    }

    #[test]
    fn rfc_5869_a2_long_inputs() {
        let ikm = range(0x00, 0x4f);
        let salt = range(0x60, 0xaf);
        let info = range(0xb0, 0xff);
        assert_eq!(
            ikm,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
             202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f\
             404142434445464748494a4b4c4d4e4f"
        );
        check(
            &ikm,
            &salt,
            &info,
            82,
            "06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244",
            "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c\
             59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71\
             cc30c58179ec3e87c14c01d5c1f3434f1d87",
        );
    }

    #[test]
    fn rfc_5869_a3_empty_salt_and_info() {
        check(
            "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b",
            "",
            "",
            42,
            "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8",
        );
    }

    #[test]
    fn an_empty_output_and_the_longest_output_are_allowed() {
        let mut empty: [u8; 0] = [];
        hkdf_sha256(b"ikm", b"salt", b"info", &mut empty);
        let mut longest = vec![0u8; OKM_MAX];
        hkdf_sha256(b"ikm", b"salt", b"info", &mut longest);
        let mut prefix = [0u8; 42];
        hkdf_sha256(b"ikm", b"salt", b"info", &mut prefix);
        assert_eq!(&longest[..42], &prefix[..]);
    }

    #[test]
    #[should_panic(expected = "at most 8160 bytes")]
    fn a_longer_output_panics() {
        let mut too_long = vec![0u8; OKM_MAX + 1];
        hkdf_sha256(b"ikm", b"salt", b"info", &mut too_long);
    }

    #[test]
    fn hmac_parts_are_concatenated() {
        assert_eq!(
            &hmac_sha256(b"key", &[b"or", b"ders"])[..],
            &hmac_sha256(b"key", &[b"orders"])[..]
        );
    }

    #[cfg(feature = "lifecycle")]
    #[test]
    fn pbkdf2_gives_the_rfc_7677_salted_password() {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD;

        // RFC 7677, section 3.
        let salt = STANDARD
            .decode("W22ZaJ0SNY7soEsUEjb6gQ==")
            .expect("the RFC salt is base64");
        let salted = pbkdf2_sha256_block(b"pencil", &salt, 4096);
        assert_eq!(
            hex_of(&salted[..]),
            "c4a49510323ab4f952cac1fa99441939e78ea74d6be81ddf7096e87513dc615d"
        );
    }

    #[cfg(feature = "lifecycle")]
    #[test]
    fn pbkdf2_with_one_iteration_is_the_first_hmac() {
        let one = pbkdf2_sha256_block(b"pencil", b"salt", 1);
        let first = hmac_sha256(b"pencil", &[b"salt", &1u32.to_be_bytes()]);
        assert_eq!(&one[..], &first[..]);
        assert_eq!(&pbkdf2_sha256_block(b"pencil", b"salt", 0)[..], &first[..]);
        assert_ne!(&pbkdf2_sha256_block(b"pencil", b"salt", 2)[..], &first[..]);
    }
}
