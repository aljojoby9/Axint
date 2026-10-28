//! 32-bit invertible Bloom lookup table.
//!
//! Keys are 32-byte digests. Each cell stores a count, the XOR of the keys,
//! and a 32-bit checksum. Subtracting two sketches leaves the symmetric
//! difference, which peels in time proportional to the number of differences.

use axint_core::AxintHash;

use crate::error::NetError;

const HASH_COUNT: usize = 4;

#[derive(Clone, Copy, Debug, Default)]
struct Cell {
    count: i32,
    key_xor: [u8; 32],
    checksum: u32,
}

/// Fixed-size set sketch.
#[derive(Clone, Debug)]
pub struct Iblt {
    cells: Vec<Cell>,
}

impl Iblt {
    /// Creates an empty table with `cells` buckets.
    ///
    /// A difference of `k` keys needs about `k * 2` cells to peel reliably.
    /// The constructor rounds the size up to at least 8.
    #[must_use]
    pub fn new(cells: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cells.max(8)],
        }
    }

    /// Inserts `key`.
    pub fn insert(&mut self, key: &AxintHash) {
        self.add(key, 1);
    }

    /// Removes `key`. Used when a peer's sketch is subtracted.
    pub fn remove(&mut self, key: &AxintHash) {
        self.add(key, -1);
    }

    /// Subtracts `other` from `self`. Both tables must have the same length.
    ///
    /// The result lists keys only in one of the two original sets. A positive
    /// count means the key was only in `self`. A negative count means it was
    /// only in `other`.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::NotPeeled`] when the difference does not fit.
    pub fn subtract(&self, other: &Self) -> Result<Vec<(AxintHash, i32)>, NetError> {
        if self.cells.len() != other.cells.len() {
            return Err(NetError::NotPeeled);
        }
        let mut mixed = self.clone();
        for (left, right) in mixed.cells.iter_mut().zip(other.cells.iter()) {
            left.count -= right.count;
            xor_assign(&mut left.key_xor, &right.key_xor);
            left.checksum ^= right.checksum;
        }
        mixed.peel()
    }

    /// Lists every key still in the table.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::NotPeeled`] when pure cells run out before the
    /// table is empty.
    pub fn peel(&self) -> Result<Vec<(AxintHash, i32)>, NetError> {
        let mut cells = self.cells.clone();
        let mut found = Vec::new();
        while let Some(index) = cells.iter().position(|cell| cell.count == 1 || cell.count == -1) {
            let cell = cells[index];
            if checksum(&cell.key_xor) != cell.checksum {
                return Err(NetError::NotPeeled);
            }
            let key = AxintHash::from_bytes(cell.key_xor);
            let sign = cell.count;
            found.push((key, sign));
            for slot in indexes(&key, cells.len()) {
                let target = &mut cells[slot];
                target.count -= sign;
                xor_assign(&mut target.key_xor, key.as_bytes());
                target.checksum ^= cell.checksum;
            }
        }
        if cells.iter().any(|cell| cell.count != 0) {
            return Err(NetError::NotPeeled);
        }
        found.sort_by_key(|(key, sign)| (*sign, *key));
        Ok(found)
    }

    fn add(&mut self, key: &AxintHash, sign: i32) {
        let sum = checksum(key.as_bytes());
        for slot in indexes(key, self.cells.len()) {
            let cell = &mut self.cells[slot];
            cell.count += sign;
            xor_assign(&mut cell.key_xor, key.as_bytes());
            cell.checksum ^= sum;
        }
    }
}

fn indexes(key: &AxintHash, length: usize) -> [usize; HASH_COUNT] {
    let mut slots = [0_usize; HASH_COUNT];
    let mut filled = 0_usize;
    let mut salt = 0_u64;
    while filled < HASH_COUNT && salt < 64 {
        let slot = hash_slot(key, salt, length);
        if !slots[..filled].contains(&slot) {
            slots[filled] = slot;
            filled += 1;
        }
        salt += 1;
    }
    while filled < HASH_COUNT {
        let fallback = filled % length.max(1);
        slots[filled] = fallback;
        filled += 1;
    }
    slots
}

fn hash_slot(key: &AxintHash, salt: u64, length: usize) -> usize {
    let bytes = key.as_bytes();
    let mixed = u64::from(bytes[0])
        | u64::from(bytes[8]) << 8
        | u64::from(bytes[16]) << 16
        | u64::from(bytes[24]) << 24;
    let value = mixed
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(salt.wrapping_mul(0xBF58_476D));
    usize::try_from(value % u64::try_from(length.max(1)).unwrap_or(1)).unwrap_or(0)
}

fn checksum(bytes: &[u8; 32]) -> u32 {
    let mut sum = 0xA410_u32;
    for (index, byte) in bytes.iter().enumerate() {
        sum = sum.rotate_left(5) ^ u32::from(*byte) ^ u32::try_from(index).unwrap_or(0);
    }
    sum
}

fn xor_assign(left: &mut [u8; 32], right: &[u8; 32]) {
    for (slot, byte) in left.iter_mut().zip(right.iter()) {
        *slot ^= *byte;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> AxintHash {
        let mut bytes = [0_u8; 32];
        bytes[0] = byte;
        bytes[31] = byte.wrapping_mul(3);
        AxintHash::from_bytes(bytes)
    }

    #[test]
    fn identical_sets_peel_to_empty() -> Result<(), NetError> {
        let mut left = Iblt::new(64);
        let mut right = Iblt::new(64);
        for value in 1..20 {
            left.insert(&key(value));
            right.insert(&key(value));
        }
        assert!(left.subtract(&right)?.is_empty());
        Ok(())
    }

    #[test]
    fn difference_is_recovered() -> Result<(), NetError> {
        let mut left = Iblt::new(128);
        let mut right = Iblt::new(128);
        for value in 1..30 {
            left.insert(&key(value));
            right.insert(&key(value));
        }
        left.insert(&key(100));
        left.insert(&key(101));
        right.insert(&key(200));
        let diff = left.subtract(&right)?;
        assert_eq!(diff.len(), 3);
        assert!(diff.iter().any(|(hash, sign)| *hash == key(100) && *sign == 1));
        assert!(diff.iter().any(|(hash, sign)| *hash == key(101) && *sign == 1));
        assert!(diff.iter().any(|(hash, sign)| *hash == key(200) && *sign == -1));
        Ok(())
    }
}
