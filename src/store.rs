//! NuDB Store — the main read/write interface.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use xxhash_rust::xxh64::xxh64;

use crate::format::*;

/// Options for creating a new NuDB store.
pub struct StoreOptions {
    /// Key size in bytes (default: 32 for SHA-256/SHA-512-half hashes).
    pub key_size: u16,
    /// Bucket block size in bytes (default: 4096).
    pub block_size: u16,
    /// Application identifier (default: 1).
    pub appnum: u64,
    /// Target load factor as fraction of 65536 (default: 32768 = 0.5).
    pub load_factor: u16,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            key_size: 32,
            block_size: 4096,
            appnum: 1,
            load_factor: 32768, // 0.5
        }
    }
}

/// A NuDB key-value store.
///
/// Write-once, hash-indexed, constant memory regardless of database size.
/// Keys are fixed-size (typically 32-byte hashes). Values are variable-size.
pub struct Store {
    dat_path: PathBuf,
    key_path: PathBuf,
    log_path: PathBuf,
    dat_file: File,
    key_file: File,
    header: KeyHeader,
    /// Current linear hashing level.
    level: u32,
    /// Next bucket to split.
    next: u64,
}

impl Store {
    /// Create a new NuDB store at the given directory.
    pub fn create(dir: &Path, opts: StoreOptions) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;

        let dat_path = dir.join("nudb.dat");
        let key_path = dir.join("nudb.key");
        let log_path = dir.join("nudb.log");

        let uid: u64 = rand_u64();
        let salt: u64 = rand_u64();
        let pepper = xxh64(&salt.to_le_bytes(), 0);

        // Write data file header
        let dat_header = DatHeader {
            version: VERSION,
            uid,
            appnum: opts.appnum,
            key_size: opts.key_size,
        };
        let mut dat_file = File::create(&dat_path)?;
        dat_header.write_to(&mut dat_file)?;

        // Write key file header + one empty bucket
        let key_header = KeyHeader {
            version: VERSION,
            uid,
            appnum: opts.appnum,
            key_size: opts.key_size,
            salt,
            pepper,
            block_size: opts.block_size,
            load_factor: opts.load_factor,
            bucket_count: 1,
            key_count: 0,
        };
        let mut key_file = File::create(&key_path)?;
        key_header.write_to(&mut key_file)?;

        // Write one empty bucket
        let empty_bucket = vec![0u8; opts.block_size as usize];
        key_file.write_all(&empty_bucket)?;
        key_file.flush()?;
        dat_file.flush()?;

        // Re-open for read+write
        let dat_file = OpenOptions::new().read(true).append(true).open(&dat_path)?;
        let key_file = OpenOptions::new().read(true).write(true).open(&key_path)?;

        Ok(Self {
            dat_path,
            key_path,
            log_path,
            dat_file,
            key_file,
            header: key_header,
            level: 0,
            next: 0,
        })
    }

    /// Open an existing NuDB store.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let dat_path = dir.join("nudb.dat");
        let key_path = dir.join("nudb.key");
        let log_path = dir.join("nudb.log");

        // Recover from log if present
        if log_path.exists() {
            // TODO: replay log for crash recovery
            let _ = std::fs::remove_file(&log_path);
        }

        let mut key_file = OpenOptions::new().read(true).write(true).open(&key_path)?;
        let header = KeyHeader::read_from(&mut key_file)?;

        let dat_file = OpenOptions::new().read(true).append(true).open(&dat_path)?;

        // Compute linear hashing level and next from bucket_count
        let (level, next) = compute_level_next(header.bucket_count);

        Ok(Self {
            dat_path,
            key_path,
            log_path,
            dat_file,
            key_file,
            header,
            level,
            next,
        })
    }

    /// Insert a key-value pair. Returns true if inserted, false if key exists.
    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> io::Result<bool> {
        assert_eq!(key.len(), self.header.key_size as usize);

        let hash = xxh64(key, self.header.salt);
        let hash48 = hash & 0xFFFF_FFFF_FFFF;

        // Check if key already exists
        let bucket_idx = self.bucket_index(hash);
        let bucket = self.read_bucket(bucket_idx)?;
        if self.key_exists_in_bucket(&bucket, key, hash48)? {
            return Ok(false);
        }

        // Append record to data file
        let dat_offset = self.dat_file.seek(SeekFrom::End(0))?;
        let record_size = (self.header.key_size as u64) + (value.len() as u64);
        self.dat_file.write_all(&write_u48(record_size))?;
        self.dat_file.write_all(key)?;
        self.dat_file.write_all(value)?;

        // Insert into bucket
        let entry = BucketEntry {
            offset: dat_offset,
            size: value.len() as u64,
            hash: hash48,
        };

        let mut bucket = bucket;
        let capacity = self.header.bucket_capacity();

        if bucket.entries.len() >= capacity {
            // Bucket full — spill to data file
            self.spill_bucket(&mut bucket)?;
        }

        bucket.insert(entry);
        self.write_bucket(bucket_idx, &bucket)?;

        // Update key count
        self.header.key_count += 1;
        self.write_key_header()?;

        // Check load factor and split if needed
        self.maybe_split()?;

        Ok(true)
    }

    /// Fetch a value by key. Returns None if not found.
    pub fn fetch(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        assert_eq!(key.len(), self.header.key_size as usize);

        let hash = xxh64(key, self.header.salt);
        let hash48 = hash & 0xFFFF_FFFF_FFFF;
        let bucket_idx = self.bucket_index(hash);
        let bucket = self.read_bucket(bucket_idx)?;

        // Search in bucket entries
        for entry in &bucket.entries {
            if entry.hash == hash48 {
                let (found_key, value) = self.read_record(entry.offset)?;
                if found_key == key {
                    return Ok(Some(value));
                }
            }
        }

        // Search in spill chain
        let mut spill_offset = bucket.spill;
        while spill_offset != 0 {
            let (spill_bucket, next_spill) = self.read_spill(spill_offset)?;
            for entry in &spill_bucket.entries {
                if entry.hash == hash48 {
                    let (found_key, value) = self.read_record(entry.offset)?;
                    if found_key == key {
                        return Ok(Some(value));
                    }
                }
            }
            spill_offset = next_spill;
        }

        Ok(None)
    }

    /// Insert or update a key-value pair. Always succeeds.
    /// If the key exists, the old value becomes dead space in the data file
    /// (reclaimed during database rotation, like rippled's online_delete).
    pub fn upsert(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        assert_eq!(key.len(), self.header.key_size as usize);

        let hash = xxh64(key, self.header.salt);
        let hash48 = hash & 0xFFFF_FFFF_FFFF;
        let bucket_idx = self.bucket_index(hash);
        let mut bucket = self.read_bucket(bucket_idx)?;

        // Append new record to data file
        let dat_offset = self.dat_file.seek(SeekFrom::End(0))?;
        let record_size = (self.header.key_size as u64) + (value.len() as u64);
        self.dat_file.write_all(&write_u48(record_size))?;
        self.dat_file.write_all(key)?;
        self.dat_file.write_all(value)?;

        // Check if key already exists in bucket — update offset if so
        let mut found = false;
        for entry in &mut bucket.entries {
            if entry.hash == hash48 {
                // Verify full key match
                let (found_key, _) = self.read_record(entry.offset)?;
                if found_key == key {
                    entry.offset = dat_offset;
                    entry.size = value.len() as u64;
                    found = true;
                    break;
                }
            }
        }

        // Check spill chain too
        if !found {
            let mut spill_offset = bucket.spill;
            while spill_offset != 0 && !found {
                let (mut spill_bucket, next_spill) = self.read_spill(spill_offset)?;
                for entry in &mut spill_bucket.entries {
                    if entry.hash == hash48 {
                        let (found_key, _) = self.read_record(entry.offset)?;
                        if found_key == key {
                            entry.offset = dat_offset;
                            entry.size = value.len() as u64;
                            found = true;
                            break;
                        }
                    }
                }
                if found {
                    // Rewrite the spill bucket with updated offset
                    let block_size = self.header.block_size as usize;
                    let spill_bytes = spill_bucket.to_bytes(block_size);
                    self.dat_file.seek(SeekFrom::Start(spill_offset + 8))?;
                    self.dat_file.write_all(&spill_bytes)?;
                }
                spill_offset = next_spill;
            }
        }

        if found {
            self.write_bucket(bucket_idx, &bucket)?;
        } else {
            // New key — insert
            let capacity = self.header.bucket_capacity();
            if bucket.entries.len() >= capacity {
                self.spill_bucket(&mut bucket)?;
            }
            bucket.insert(BucketEntry {
                offset: dat_offset,
                size: value.len() as u64,
                hash: hash48,
            });
            self.write_bucket(bucket_idx, &bucket)?;
            self.header.key_count += 1;
            self.write_key_header()?;
            self.maybe_split()?;
        }

        Ok(())
    }

    /// Remove a key. Marks the bucket entry as removed.
    /// The data stays in the data file (append-only) but the key is no longer findable.
    pub fn remove(&mut self, key: &[u8]) -> io::Result<bool> {
        assert_eq!(key.len(), self.header.key_size as usize);

        let hash = xxh64(key, self.header.salt);
        let hash48 = hash & 0xFFFF_FFFF_FFFF;
        let bucket_idx = self.bucket_index(hash);
        let mut bucket = self.read_bucket(bucket_idx)?;

        let before = bucket.entries.len();
        bucket.entries.retain(|e| e.hash != hash48);
        if bucket.entries.len() < before {
            bucket.count = bucket.entries.len() as u16;
            self.write_bucket(bucket_idx, &bucket)?;
            self.header.key_count = self.header.key_count.saturating_sub(1);
            self.write_key_header()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Check if a key exists without reading the value.
    pub fn exists(&mut self, key: &[u8]) -> io::Result<bool> {
        Ok(self.fetch(key)?.is_some())
    }

    /// Number of keys stored.
    pub fn key_count(&self) -> u64 {
        self.header.key_count
    }

    /// Number of buckets.
    pub fn bucket_count(&self) -> u64 {
        self.header.bucket_count
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    /// Compute which bucket a hash maps to (linear hashing).
    fn bucket_index(&self, hash: u64) -> u64 {
        let modulus = 1u64 << self.level;
        let mut idx = hash % modulus;
        if idx < self.next {
            idx = hash % (modulus * 2);
        }
        idx
    }

    /// Read a bucket from the key file.
    fn read_bucket(&mut self, index: u64) -> io::Result<Bucket> {
        let offset = self.header.bucket_offset(index);
        self.key_file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; self.header.block_size as usize];
        self.key_file.read_exact(&mut buf)?;
        Ok(Bucket::from_bytes(&buf))
    }

    /// Write a bucket to the key file.
    fn write_bucket(&mut self, index: u64, bucket: &Bucket) -> io::Result<()> {
        let offset = self.header.bucket_offset(index);
        let buf = bucket.to_bytes(self.header.block_size as usize);
        self.key_file.seek(SeekFrom::Start(offset))?;
        self.key_file.write_all(&buf)?;
        Ok(())
    }

    /// Write the key file header (updates key_count, bucket_count).
    fn write_key_header(&mut self) -> io::Result<()> {
        self.key_file.seek(SeekFrom::Start(0))?;
        self.header.write_to(&mut self.key_file)?;
        Ok(())
    }

    /// Read a record from the data file at the given offset.
    fn read_record(&mut self, offset: u64) -> io::Result<(Vec<u8>, Vec<u8>)> {
        self.dat_file.seek(SeekFrom::Start(offset))?;
        let mut size_buf = [0u8; 6];
        self.dat_file.read_exact(&mut size_buf)?;
        let total_size = read_u48(&size_buf) as usize;

        let key_size = self.header.key_size as usize;
        if total_size < key_size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "record too small"));
        }
        let val_size = total_size - key_size;

        let mut key = vec![0u8; key_size];
        self.dat_file.read_exact(&mut key)?;
        let mut value = vec![0u8; val_size];
        self.dat_file.read_exact(&mut value)?;
        Ok((key, value))
    }

    /// Check if a key exists in a bucket (including spill chain).
    fn key_exists_in_bucket(&mut self, bucket: &Bucket, key: &[u8], hash48: u64) -> io::Result<bool> {
        for entry in &bucket.entries {
            if entry.hash == hash48 {
                let (found_key, _) = self.read_record(entry.offset)?;
                if found_key == key {
                    return Ok(true);
                }
            }
        }
        // Check spill chain
        let mut spill_offset = bucket.spill;
        while spill_offset != 0 {
            let (spill_bucket, next_spill) = self.read_spill(spill_offset)?;
            for entry in &spill_bucket.entries {
                if entry.hash == hash48 {
                    let (found_key, _) = self.read_record(entry.offset)?;
                    if found_key == key {
                        return Ok(true);
                    }
                }
            }
            spill_offset = next_spill;
        }
        Ok(false)
    }

    /// Read a spill bucket from the data file. Returns (bucket, next_spill_offset).
    fn read_spill(&mut self, offset: u64) -> io::Result<(Bucket, u64)> {
        self.dat_file.seek(SeekFrom::Start(offset))?;
        // Spill record: 6 bytes zero + 2 bytes size
        let mut header = [0u8; 8];
        self.dat_file.read_exact(&mut header)?;
        let spill_size = u16::from_be_bytes([header[6], header[7]]) as usize;
        let mut buf = vec![0u8; spill_size];
        self.dat_file.read_exact(&mut buf)?;
        let bucket = Bucket::from_bytes(&buf);
        let spill = bucket.spill;
        Ok((bucket, spill))
    }

    /// Spill a full bucket to the data file.
    fn spill_bucket(&mut self, bucket: &mut Bucket) -> io::Result<()> {
        let spill_offset = self.dat_file.seek(SeekFrom::End(0))?;
        let block_size = self.header.block_size as usize;
        let bucket_bytes = bucket.to_bytes(block_size);

        // Write spill record: 6-byte zero + 2-byte size + bucket data
        self.dat_file.write_all(&[0u8; 6])?;
        self.dat_file.write_all(&(block_size as u16).to_be_bytes())?;
        self.dat_file.write_all(&bucket_bytes)?;

        // Update bucket: clear entries, set spill pointer
        bucket.spill = spill_offset;
        bucket.entries.clear();
        bucket.count = 0;
        Ok(())
    }

    /// Check load factor and split a bucket if needed.
    fn maybe_split(&mut self) -> io::Result<()> {
        let capacity = self.header.bucket_capacity() as u64;
        let total_capacity = self.header.bucket_count * capacity;
        let load = (self.header.key_count as u128 * 65536) / total_capacity.max(1) as u128;

        if load <= self.header.load_factor as u128 {
            return Ok(()); // Under load factor, no split needed
        }

        // Split bucket at index `next`
        let old_bucket = self.read_bucket(self.next)?;
        let mut keep = Bucket::new();
        let mut move_out = Bucket::new();

        let new_modulus = 1u64 << (self.level + 1);

        // Redistribute entries
        for entry in &old_bucket.entries {
            // Re-read the key to get the full hash
            let (key, _) = self.read_record(entry.offset)?;
            let hash = xxh64(&key, self.header.salt);
            let new_idx = hash % new_modulus;
            if new_idx == self.next {
                keep.insert(*entry);
            } else {
                move_out.insert(*entry);
            }
        }

        // Also redistribute spill chain entries
        let mut spill_offset = old_bucket.spill;
        while spill_offset != 0 {
            let (spill_bucket, next_spill) = self.read_spill(spill_offset)?;
            for entry in &spill_bucket.entries {
                let (key, _) = self.read_record(entry.offset)?;
                let hash = xxh64(&key, self.header.salt);
                let new_idx = hash % new_modulus;
                if new_idx == self.next {
                    keep.insert(*entry);
                } else {
                    move_out.insert(*entry);
                }
            }
            spill_offset = next_spill;
        }

        // Write kept entries back to original bucket
        keep.spill = 0; // Spills were consumed
        self.write_bucket(self.next, &keep)?;

        // Append new bucket for moved entries
        let new_bucket_idx = self.header.bucket_count;
        self.header.bucket_count += 1;
        let new_offset = self.header.bucket_offset(new_bucket_idx);
        self.key_file.seek(SeekFrom::Start(new_offset))?;
        move_out.spill = 0;
        let buf = move_out.to_bytes(self.header.block_size as usize);
        self.key_file.write_all(&buf)?;

        // Advance linear hashing state
        self.next += 1;
        if self.next >= (1u64 << self.level) {
            self.level += 1;
            self.next = 0;
        }

        self.write_key_header()?;
        Ok(())
    }
}

/// Compute level and next from bucket_count for linear hashing.
fn compute_level_next(bucket_count: u64) -> (u32, u64) {
    if bucket_count <= 1 {
        return (0, 0);
    }
    // Find the highest power of 2 <= bucket_count
    let mut level = 0u32;
    while (1u64 << (level + 1)) < bucket_count {
        level += 1;
    }
    let base = 1u64 << level;
    let next = bucket_count - base;
    // If next == base, we've completed this level
    if next >= base {
        (level + 1, 0)
    } else {
        (level, next)
    }
}

/// Simple random u64 (not crypto-grade, just for UID/salt).
fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut v = t.as_nanos() as u64;
    v ^= v >> 13;
    v = v.wrapping_mul(0x7feb352d_u64);
    v ^= v >> 15;
    v
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let id = CTR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nudb_test_{id}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn create_and_open() {
        let dir = temp_dir();
        {
            let store = Store::create(&dir, StoreOptions::default()).unwrap();
            assert_eq!(store.key_count(), 0);
            assert_eq!(store.bucket_count(), 1);
        }
        {
            let store = Store::open(&dir).unwrap();
            assert_eq!(store.key_count(), 0);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn insert_and_fetch() {
        let dir = temp_dir();
        let mut store = Store::create(&dir, StoreOptions::default()).unwrap();

        let key = [0x42u8; 32];
        let value = b"hello world";
        assert!(store.insert(&key, value).unwrap());
        assert!(!store.insert(&key, value).unwrap()); // duplicate

        let fetched = store.fetch(&key).unwrap().unwrap();
        assert_eq!(fetched, value);
        assert_eq!(store.key_count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fetch_missing() {
        let dir = temp_dir();
        let mut store = Store::create(&dir, StoreOptions::default()).unwrap();
        let key = [0xAB; 32];
        assert!(store.fetch(&key).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn many_inserts() {
        let dir = temp_dir();
        let mut store = Store::create(&dir, StoreOptions::default()).unwrap();

        for i in 0u32..1000 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            let value = format!("value_{i}");
            assert!(store.insert(&key, value.as_bytes()).unwrap());
        }

        assert_eq!(store.key_count(), 1000);
        assert!(store.bucket_count() > 1, "should have split buckets");

        // Verify all can be fetched
        for i in 0u32..1000 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            let value = store.fetch(&key).unwrap().expect("missing key");
            assert_eq!(value, format!("value_{i}").as_bytes());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_persists() {
        let dir = temp_dir();
        {
            let mut store = Store::create(&dir, StoreOptions::default()).unwrap();
            for i in 0u32..100 {
                let mut key = [0u8; 32];
                key[..4].copy_from_slice(&i.to_le_bytes());
                store.insert(&key, b"data").unwrap();
            }
        }
        {
            let mut store = Store::open(&dir).unwrap();
            assert_eq!(store.key_count(), 100);
            for i in 0u32..100 {
                let mut key = [0u8; 32];
                key[..4].copy_from_slice(&i.to_le_bytes());
                assert!(store.fetch(&key).unwrap().is_some(), "key {i} missing after reopen");
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_updates_value() {
        let dir = temp_dir();
        let mut store = Store::create(&dir, StoreOptions::default()).unwrap();

        let key = [0x01; 32];
        store.upsert(&key, b"version1").unwrap();
        assert_eq!(store.fetch(&key).unwrap().unwrap(), b"version1");

        store.upsert(&key, b"version2").unwrap();
        assert_eq!(store.fetch(&key).unwrap().unwrap(), b"version2");

        // Key count should still be 1 (update, not new insert)
        assert_eq!(store.key_count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_key() {
        let dir = temp_dir();
        let mut store = Store::create(&dir, StoreOptions::default()).unwrap();

        let key = [0x02; 32];
        store.insert(&key, b"data").unwrap();
        assert!(store.exists(&key).unwrap());

        assert!(store.remove(&key).unwrap());
        assert!(!store.exists(&key).unwrap());
        assert_eq!(store.key_count(), 0);

        // Remove non-existent
        assert!(!store.remove(&key).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_many() {
        let dir = temp_dir();
        let mut store = Store::create(&dir, StoreOptions::default()).unwrap();

        // Insert 500 keys
        for i in 0u32..500 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            store.upsert(&key, &i.to_le_bytes()).unwrap();
        }
        assert_eq!(store.key_count(), 500);

        // Update all 500
        for i in 0u32..500 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            let new_val = (i * 100).to_le_bytes();
            store.upsert(&key, &new_val).unwrap();
        }
        assert_eq!(store.key_count(), 500); // Still 500, not 1000

        // Verify updated values
        for i in 0u32..500 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            let val = store.fetch(&key).unwrap().unwrap();
            assert_eq!(val, (i * 100).to_le_bytes());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn level_next_computation() {
        assert_eq!(compute_level_next(1), (0, 0));
        assert_eq!(compute_level_next(2), (1, 0));
        assert_eq!(compute_level_next(3), (1, 1));
        assert_eq!(compute_level_next(4), (2, 0));
        assert_eq!(compute_level_next(5), (2, 1));
        assert_eq!(compute_level_next(8), (3, 0));
    }
}
