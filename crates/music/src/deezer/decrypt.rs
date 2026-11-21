//! Decryption of Deezer's `BF_CBC_STRIPE` audio: the stream is split into 2048-byte blocks
//! and only every third block (0, 3, 6, …) is encrypted with Blowfish CBC, the cipher reset
//! per block against a fixed IV. The per-track key derives from the track id and a master
//! secret extracted at runtime from Deezer's own web player bundle.

use blowfish::Blowfish;
use blowfish::cipher::{BlockModeDecrypt, InnerIvInit, KeyInit, block_padding::NoPadding};
use md5::{Digest, Md5};

/// Deezer's encryption block size.
pub const BLOCK: usize = 2048;

/// Every third block is encrypted.
const STRIPE: usize = 3;

/// The fixed IV of the BF_CBC_STRIPE scheme.
const IV: [u8; 8] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];

/// The master secret shared by every track. The web player's script bundle carries it too, but
/// the one value that passes its checksum is this one, so nothing is fetched to learn it.
pub(crate) const SECRET: Secret = *b"g4el58wc0zvf9na1";

/// The 16-byte master secret shared by every track.
pub type Secret = [u8; 16];

/// Derives the Blowfish key of one track: the two halves of the MD5 hex of the track id,
/// XORed against each other and against the master secret.
pub fn track_key(track_id: &str, secret: &Secret) -> Secret {
    let hash: String = Md5::digest(track_id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let hash = hash.as_bytes();
    let mut key = [0u8; 16];
    for i in 0..16 {
        key[i] = hash[i] ^ hash[i + 16] ^ secret[i];
    }
    key
}

/// One track's Blowfish, keyed once so the key schedule is not rebuilt for every stripe.
/// Each encrypted block still starts its CBC chain afresh from the fixed IV.
pub struct Cipher {
    blowfish: Blowfish,
}

impl Cipher {
    pub fn new(key: &Secret) -> Self {
        let blowfish =
            Blowfish::new_from_slice(key).expect("a 16-byte key is within blowfish's range");
        Self { blowfish }
    }

    /// Decrypts one block in place if its index lands on an encrypted stripe. Partial blocks
    /// (only possible at the very end of a track) arrive unencrypted.
    pub fn decrypt_block(&self, block: &mut [u8], index: u64) {
        if !index.is_multiple_of(STRIPE as u64) || block.len() < BLOCK {
            return;
        }
        let decryptor =
            cbc::Decryptor::<Blowfish>::inner_iv_init(self.blowfish.clone(), &IV.into());
        if let Err(error) = decryptor.decrypt_padded::<NoPadding>(block) {
            log::warn!("deezer: cannot decrypt block {index}: {error}");
        }
    }
}

/// The body of a Deezer track: every complete stripe decrypted as it lands, so what the
/// buffer holds is always in the clear.
///
/// A partial block is kept back until the rest of it arrives, because only whole blocks are
/// encrypted, and whatever is left at the end of the track is appended as it is.
pub struct Striped {
    cipher: Cipher,
    /// The tail of the last chunk, short of a whole block.
    pending: Vec<u8>,
    /// How many blocks have gone by, which is what decides an encrypted stripe.
    blocks: u64,
}

impl Striped {
    pub fn new(key: &Secret) -> Self {
        Self {
            cipher: Cipher::new(key),
            pending: Vec::with_capacity(2 * BLOCK),
            blocks: 0,
        }
    }
}

impl crate::stream::Body for Striped {
    /// Decrypts every whole block in place where it sits in `pending`, then moves them out in
    /// one go, so a chunk costs no buffer of its own however many blocks it holds.
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        self.pending.extend_from_slice(chunk);
        let whole = self.pending.len() - self.pending.len() % BLOCK;
        for block in self.pending[..whole].as_chunks_mut::<BLOCK>().0 {
            self.cipher.decrypt_block(block, self.blocks);
            self.blocks += 1;
        }
        out.extend_from_slice(&self.pending[..whole]);
        self.pending.drain(..whole);
    }

    fn flush(&mut self, out: &mut Vec<u8>) {
        // The tail is a partial block, and partial blocks are never encrypted.
        out.extend_from_slice(&self.pending);
        self.pending.clear();
    }

    fn confidential(&self) -> bool {
        true
    }
}
