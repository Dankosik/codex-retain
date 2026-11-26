//! Conservative ownership and incoming-reference inventory for local rollouts.
//!
//! This is a filesystem snapshot, not a cross-process fork reservation. Callers
//! must separately enforce archived eligibility and mutation coordination.

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat, fstat, openat, statat};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use smallvec::SmallVec;
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use uuid::Uuid;

#[inline]
pub(crate) fn wymix(a: u64, b: u64) -> u64 {
    let wide = u128::from(a).wrapping_mul(u128::from(b));
    (wide as u64) ^ ((wide >> 64) as u64)
}

pub(crate) fn random_hash_seeds() -> (u64, u64) {
    let state = RandomState::new();
    let s0 = state.hash_one(0x9e37_79b9_7f4a_7c15u64) | 1;
    let s1 = state.hash_one(0xbf58_476d_1ce4_e5b9u64) | 1;
    (s0, s1)
}

static FAST_HASH_SEEDS: LazyLock<(u64, u64)> = LazyLock::new(random_hash_seeds);

#[derive(Clone, Copy)]
pub(crate) struct FastBuildHasher {
    seed0: u64,
    seed1: u64,
}

impl Default for FastBuildHasher {
    #[inline]
    fn default() -> Self {
        let (seed0, seed1) = *FAST_HASH_SEEDS;
        Self { seed0, seed1 }
    }
}

pub(crate) struct FastHasher {
    acc: u64,
    seed1: u64,
}

impl BuildHasher for FastBuildHasher {
    type Hasher = FastHasher;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        FastHasher {
            acc: self.seed0,
            seed1: self.seed1,
        }
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        wymix(self.acc, self.seed1 ^ 0xa076_1d64_78bd_642f)
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() == 16 {
            let lo = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
            let hi = u64::from_le_bytes(bytes[8..].try_into().expect("8 bytes"));
            self.acc = wymix(self.acc ^ lo, self.seed1 ^ hi);
            return;
        }
        self.acc = hash_bytes_keyed(self.acc, self.seed1, bytes.len() as u64, bytes);
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.acc = wymix(self.acc ^ u64::from(i), self.seed1);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.acc = wymix(self.acc ^ u64::from(i), self.seed1);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.acc = wymix(self.acc ^ i, self.seed1);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.acc = wymix(self.acc ^ (i as u64), self.seed1);
    }

    #[inline]
    fn write_u128(&mut self, i: u128) {
        self.acc = wymix(self.acc ^ (i as u64), self.seed1 ^ ((i >> 64) as u64));
    }
}

pub(crate) type FastHashMap<K, V> = HashMap<K, V, FastBuildHasher>;
pub(crate) type FastHashSet<T> = HashSet<T, FastBuildHasher>;

#[inline]
pub(crate) fn hash_bytes_keyed(seed0: u64, seed1: u64, domain: u64, bytes: &[u8]) -> u64 {
    let mut acc = wymix(domain ^ seed0, (bytes.len() as u64) ^ seed1);
    let (chunks, rem) = bytes.as_chunks::<8>();
    for &chunk in chunks {
        let word = u64::from_le_bytes(chunk);
        acc = wymix(acc ^ word, seed1);
    }
    if !rem.is_empty() {
        let mut tail = [0u8; 8];
        tail[..rem.len()].copy_from_slice(rem);
        acc = wymix(acc ^ u64::from_le_bytes(tail), seed0);
    }
    wymix(acc ^ seed0, seed1 ^ 0xa076_1d64_78bd_642f)
}

const BYTE_SLAB_SIZE: usize = 65_536;

/// Slab-backed byte arena storing slices up to 65,535 bytes without geometric
/// doubling reallocations.
#[derive(Default)]
pub(crate) struct ChunkedBytes {
    slabs: Vec<Vec<u8>>,
}

impl ChunkedBytes {
    pub(crate) fn clear(&mut self) {
        if let Some(first) = self.slabs.first_mut() {
            first.clear();
            self.slabs.truncate(1);
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(u32, u16)> {
        let len = u16::try_from(bytes.len()).context("arena slice exceeds 64 KiB")?;
        if bytes.is_empty() {
            return Ok((0, 0));
        }
        let need_new = self
            .slabs
            .last()
            .is_none_or(|slab| BYTE_SLAB_SIZE - slab.len() < bytes.len());
        if need_new {
            ensure!(
                self.slabs.len() <= usize::from(u16::MAX),
                "chunked byte arena exceeded 4 GiB"
            );
            let slab = if self.slabs.is_empty() {
                Vec::new()
            } else {
                Vec::with_capacity(BYTE_SLAB_SIZE)
            };
            self.slabs.push(slab);
        }
        let slab_idx = self.slabs.len() - 1;
        let slab = &mut self.slabs[slab_idx];
        let in_slab_off = slab.len();
        slab.extend_from_slice(bytes);
        let offset = ((slab_idx as u32) << 16) | (in_slab_off as u32);
        Ok((offset, len))
    }

    #[inline]
    pub(crate) fn get(&self, offset: u32, len: u16) -> &[u8] {
        if len == 0 {
            return &[];
        }
        let slab_idx = (offset >> 16) as usize;
        let start = (offset & 0xFFFF) as usize;
        &self.slabs[slab_idx][start..start + usize::from(len)]
    }
}

const UUID_CHUNK_SIZE: usize = 4096; // 64 KiB per chunk

#[derive(Default)]
struct ChunkedUuids {
    chunks: Vec<Vec<Uuid>>,
}

impl ChunkedUuids {
    fn clear_and_release(&mut self) {
        self.chunks = Vec::new();
    }

    fn push(&mut self, id: Uuid) {
        let need_new = self
            .chunks
            .last()
            .is_none_or(|chunk| chunk.len() >= UUID_CHUNK_SIZE);
        if need_new {
            let chunk = if self.chunks.is_empty() {
                Vec::new()
            } else {
                Vec::with_capacity(UUID_CHUNK_SIZE)
            };
            self.chunks.push(chunk);
        }
        self.chunks.last_mut().expect("chunk exists").push(id);
    }

    fn iter(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.chunks.iter().flat_map(|c| c.iter().copied())
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct Rollout {
    pub rollout_id: Uuid,
    pub path: PathBuf,
    pub paginated: bool,
}

const OWNED_NAME_ARENA_FLAG: u64 = 1u64 << 63;

#[derive(Debug)]
pub(crate) struct OwnedRollout {
    pub owner: Uuid,
    pub rollout_id: Uuid,
    pub identity: crate::fsutil::Identity,
    pub bytes: u64,
    // Bit 63 == 0: compact standard rollout name (`uuid == rollout_id`,
    // bits 0..40 = packed timestamp, bit 40 = `.zst`).
    // Bit 63 == 1: fallback arena span in `owned_names` (`off: u32`, `len: u16`).
    name_packed: u64,
    dir_idx: u32,
    pub paginated: bool,
}

const _: () = assert!(std::mem::size_of::<OwnedRollout>() == 72);

#[derive(Default)]
pub(crate) struct LineageIndex {
    owned: Vec<OwnedRollout>,
    owned_dirs: Vec<PathBuf>,
    owned_names: ChunkedBytes,
    incoming: FastHashMap<Uuid, ReferringOwners>,
    // Uncached self-owned rollouts (`rollout_id == owner`) stored in 64 KiB
    // chunks without doubling; cached self-owned files live in `HeaderCache`.
    self_owned: ChunkedUuids,
    // Subset of self-owned rollout IDs actually referenced by `history_base`.
    resolved_self_owned: FastHashSet<Uuid>,
    reverted_owners: FastHashMap<Uuid, Uuid>,
    parents: FastHashSet<(Uuid, Uuid)>,
    references: Vec<(Uuid, Uuid)>,
}

struct ReferringOwners {
    first: Uuid,
    mixed: bool,
}

impl LineageIndex {
    pub(crate) fn clear(&mut self) {
        self.owned.clear();
        self.owned_dirs.clear();
        self.owned_names.clear();
        self.incoming.clear();
        self.self_owned.clear_and_release();
        self.resolved_self_owned.clear();
        self.reverted_owners.clear();
        self.parents.clear();
        self.references.clear();
    }

    /// Release whole-inventory maps no longer needed once dependency edges have
    /// been transferred to `Dependencies` and only candidate `owned` slices remain.
    pub(crate) fn release_graph_scratch(&mut self) {
        self.incoming = FastHashMap::default();
        self.release_owner_scratch();
    }

    /// Release owner/edge scratch while retaining `owned` and `incoming`
    /// (needed by `inspect_owned` for `externally_referenced` checks).
    pub(crate) fn release_owner_scratch(&mut self) {
        self.self_owned.clear_and_release();
        self.resolved_self_owned = FastHashSet::default();
        self.reverted_owners = FastHashMap::default();
        self.parents = FastHashSet::default();
        self.references = Vec::new();
    }

    fn record_owner(&mut self, rollout_id: Uuid, owner: Uuid, in_cache: bool) -> Result<()> {
        if rollout_id == owner {
            if !self.reverted_owners.is_empty() && self.reverted_owners.contains_key(&rollout_id) {
                bail!("rollout identity {rollout_id} has conflicting owners");
            }
            if !in_cache {
                self.self_owned.push(rollout_id);
            }
        } else if let Some(previous) = self.reverted_owners.insert(rollout_id, owner) {
            ensure!(
                previous == owner,
                "rollout identity {rollout_id} has conflicting owners"
            );
        }
        Ok(())
    }

    fn finalize_owners(&mut self, cache: Option<&HeaderCache>) -> Result<()> {
        if self.reverted_owners.is_empty() && self.references.is_empty() {
            self.self_owned.clear_and_release();
            return Ok(());
        }
        let referenced_bases: FastHashSet<Uuid> = if self.references.is_empty() {
            FastHashSet::default()
        } else {
            self.references.iter().map(|&(_, base)| base).collect()
        };
        let has_reverted = !self.reverted_owners.is_empty();
        let has_refs = !referenced_bases.is_empty();
        for id in self.self_owned.iter() {
            if has_reverted && self.reverted_owners.contains_key(&id) {
                bail!("rollout identity {id} has conflicting owners");
            }
            if has_refs && referenced_bases.contains(&id) {
                self.resolved_self_owned.insert(id);
            }
        }
        if let Some(cache) = cache {
            let generation = cache.generation;
            for entry in cache.entries.iter() {
                if entry.generation() == generation && entry.is_self_owned(&cache.relations) {
                    let id = entry.owner;
                    if has_reverted && self.reverted_owners.contains_key(&id) {
                        bail!("rollout identity {id} has conflicting owners");
                    }
                    if has_refs && referenced_bases.contains(&id) {
                        self.resolved_self_owned.insert(id);
                    }
                }
            }
        }
        self.self_owned.clear_and_release();
        Ok(())
    }

    fn owner_of(&self, rollout_id: Uuid) -> Option<Uuid> {
        if self.resolved_self_owned.contains(&rollout_id) {
            Some(rollout_id)
        } else {
            self.reverted_owners.get(&rollout_id).copied()
        }
    }

    pub(crate) fn owned_len(&self) -> usize {
        self.owned.len()
    }

    pub(crate) fn owned_entries(&self, owner: Uuid) -> &[OwnedRollout] {
        let start = self.owned.partition_point(|entry| entry.owner < owner);
        let mut end = start;
        while end < self.owned.len() && self.owned[end].owner == owner {
            end += 1;
        }
        &self.owned[start..end]
    }

    pub(crate) fn rollout_dir(&self, entry: &OwnedRollout) -> &Path {
        &self.owned_dirs[entry.dir_idx as usize]
    }

    pub(crate) fn rollout_name<'a>(
        &'a self,
        entry: &OwnedRollout,
        buf: &'a mut [u8; 74],
    ) -> &'a OsStr {
        decode_owned_name(&self.owned_names, entry, buf)
    }

    #[cfg(test)]
    pub(crate) fn owned(&self, owner: Uuid) -> Vec<Rollout> {
        let mut buf = [0u8; 74];
        self.owned_entries(owner)
            .iter()
            .map(|entry| Rollout {
                rollout_id: entry.rollout_id,
                path: self
                    .rollout_dir(entry)
                    .join(self.rollout_name(entry, &mut buf)),
                paginated: entry.paginated,
            })
            .collect()
    }

    pub(crate) fn externally_referenced(&self, rollout: Uuid, owner: Uuid) -> bool {
        self.incoming
            .get(&rollout)
            .is_some_and(|sources| sources.mixed || sources.first != owner)
    }

    pub(crate) fn add_dependencies(&self, graph: &mut crate::relations::Dependencies) {
        for &(child, parent) in &self.parents {
            graph.add_parent(child, parent);
        }
        for &(dependent, rollout) in &self.references {
            if let Some(source) = self.owner_of(rollout)
                && source != dependent
            {
                graph.add_history(dependent, source);
            }
        }
    }
}

const BUCKET_EMPTY: u32 = 0;
const BUCKET_IDX_MASK: u32 = 0x00FF_FFFF;
const BUCKET_FP_MASK: u32 = 0xFF00_0000;
const BUCKET_MAX_ENTRIES: usize = (BUCKET_IDX_MASK as usize) - 1;

const NSEC_BITS: u32 = 30;
const NSEC_MASK: u64 = (1u64 << NSEC_BITS) - 1;
const MODE_SHIFT: u32 = 30;
const MODE_PERM_MASK: u32 = 0o7777;
const REL_SHIFT: u32 = 42;
const NO_RELATIONS: u32 = 0x0007_FFFF; // 19 bits = 524,287 (> 500,000 inventory cap)
const RELATIONS_MASK: u64 = 0x0007_FFFF;
const FLAG_ARENA_KEY: u64 = 1u64 << 61;
const FLAG_GENERATION: u64 = 1u64 << 62;
const FLAG_PAGINATED: u64 = 1u64 << 63;

const COMPACT_TS_MASK: u64 = (1u64 << 40) - 1;
const FLAG_COMPACT_COMPRESSED: u64 = 1u64 << 40;
const ARENA_LEN_SHIFT: u32 = 32;
const DIR_SHIFT: u32 = 48;
const DIR_MASK: u16 = 0x0FFF;
const MAX_DIR_IDX: u16 = 0x0FFF;
const ENV_SHIFT: u32 = 60;
const MAX_ENV_IDX: u8 = 0x0F;

const ENTRY_CHUNK_SHIFT: usize = 11; // 2048 * 48 B = 96 KiB (6 * 16 KiB VM pages)
const ENTRY_CHUNK_SIZE: usize = 1 << ENTRY_CHUNK_SHIFT;
const ENTRY_CHUNK_MASK: usize = ENTRY_CHUNK_SIZE - 1;

#[inline]
fn pack_bucket(hash: u64, entry_idx: usize) -> u32 {
    debug_assert!(entry_idx < BUCKET_MAX_ENTRIES);
    (((hash >> 56) as u32) << 24) | ((entry_idx as u32) + 1)
}

#[inline]
fn bucket_idx(slot: u32) -> usize {
    ((slot & BUCKET_IDX_MASK) - 1) as usize
}

#[derive(Clone, Copy)]
struct CachedExtra {
    rollout_id: Option<Uuid>,
    base: Option<Uuid>,
    parent: Option<Uuid>,
    // Stored only when ctime differs from mtime (e.g. chmod or restored mtime).
    ctime: Option<(i32, u32)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileEnv {
    device: u64,
    uid: u32,
    gid: u32,
}

#[derive(Clone, Copy)]
enum LookupKey<'a> {
    Compact {
        dir_idx: u16,
        compressed: bool,
        ts: u64,
        uuid: Uuid,
    },
    Arena {
        dir_idx: u16,
        name: &'a [u8],
    },
}

#[inline]
fn digit2_u8(a: u8, b: u8) -> Option<u8> {
    if a.is_ascii_digit() && b.is_ascii_digit() {
        Some((a - b'0') * 10 + (b - b'0'))
    } else {
        None
    }
}

#[inline]
fn pack_rollout_ts(year: u64, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> u64 {
    (year << 26)
        | (u64::from(month) << 22)
        | (u64::from(day) << 17)
        | (u64::from(hour) << 12)
        | (u64::from(minute) << 6)
        | u64::from(second)
}

/// Bijectively pack an ordinary rollout filename
/// `rollout-YYYY-MM-DDTHH-MM-SS-<canonical-uuid>.jsonl[.zst]` into
/// `(40-bit timestamp, owner Uuid, compressed)` with zero heap allocation.
#[inline]
pub(crate) fn parse_compact_rollout_key(name: &[u8]) -> Option<(u64, Uuid, bool)> {
    let compressed = match name.len() {
        70 if name.ends_with(b".jsonl") => false,
        74 if name.ends_with(b".jsonl.zst") => true,
        _ => return None,
    };
    if !name.starts_with(b"rollout-")
        || name[12] != b'-'
        || name[15] != b'-'
        || name[18] != b'T'
        || name[21] != b'-'
        || name[24] != b'-'
        || name[27] != b'-'
    {
        return None;
    }
    let y_hi = digit2_u8(name[8], name[9])?;
    let y_lo = digit2_u8(name[10], name[11])?;
    let year = u64::from(y_hi) * 100 + u64::from(y_lo);
    let month = digit2_u8(name[13], name[14])?;
    let day = digit2_u8(name[16], name[17])?;
    let hour = digit2_u8(name[19], name[20])?;
    let minute = digit2_u8(name[22], name[23])?;
    let second = digit2_u8(name[25], name[26])?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let uuid = parse_canonical_uuid_bytes(&name[28..64])?;
    let ts = pack_rollout_ts(year, month, day, hour, minute, second);
    Some((ts, uuid, compressed))
}

/// Exact inverse of `parse_compact_rollout_key`, formatting into a 74-byte
/// stack buffer without heap allocation.
pub(crate) fn format_compact_rollout_name(
    ts: u64,
    uuid: Uuid,
    compressed: bool,
    buf: &mut [u8; 74],
) -> usize {
    let year = (ts >> 26) as u16;
    let month = ((ts >> 22) & 0xF) as u8;
    let day = ((ts >> 17) & 0x1F) as u8;
    let hour = ((ts >> 12) & 0x1F) as u8;
    let minute = ((ts >> 6) & 0x3F) as u8;
    let second = (ts & 0x3F) as u8;
    buf[0..8].copy_from_slice(b"rollout-");
    let y_hi = (year / 100) as u8;
    let y_lo = (year % 100) as u8;
    buf[8] = b'0' + (y_hi / 10);
    buf[9] = b'0' + (y_hi % 10);
    buf[10] = b'0' + (y_lo / 10);
    buf[11] = b'0' + (y_lo % 10);
    buf[12] = b'-';
    buf[13] = b'0' + (month / 10);
    buf[14] = b'0' + (month % 10);
    buf[15] = b'-';
    buf[16] = b'0' + (day / 10);
    buf[17] = b'0' + (day % 10);
    buf[18] = b'T';
    buf[19] = b'0' + (hour / 10);
    buf[20] = b'0' + (hour % 10);
    buf[21] = b'-';
    buf[22] = b'0' + (minute / 10);
    buf[23] = b'0' + (minute % 10);
    buf[24] = b'-';
    buf[25] = b'0' + (second / 10);
    buf[26] = b'0' + (second % 10);
    buf[27] = b'-';
    uuid.hyphenated().encode_lower(&mut buf[28..64]);
    buf[64..70].copy_from_slice(b".jsonl");
    if compressed {
        buf[70..74].copy_from_slice(b".zst");
        74
    } else {
        70
    }
}

#[inline]
fn lookup_key(dir_idx: u16, name: &[u8]) -> LookupKey<'_> {
    if let Some((ts, uuid, compressed)) = parse_compact_rollout_key(name) {
        LookupKey::Compact {
            dir_idx,
            compressed,
            ts,
            uuid,
        }
    } else {
        LookupKey::Arena { dir_idx, name }
    }
}

#[derive(Clone, Copy)]
struct CachedEntry {
    owner: Uuid,
    inode: u64,
    // bits 0..30: mtime_nsec | 30..42: mode_perm | 42..61: relations_idx
    // bit 61: FLAG_ARENA_KEY | 62: FLAG_GENERATION | 63: FLAG_PAGINATED
    meta_a: u64,
    // Compact: bits 0..40: ts | bit 40: compressed
    // Arena:   bits 0..32: arena_off | bits 32..48: arena_len
    // Common:  bits 48..60: dir_idx | bits 60..64: env_idx
    meta_b: u64,
    mtime_delta: i32,
    len: u32,
}

const _: () = assert!(std::mem::size_of::<CachedEntry>() == 48);

impl CachedEntry {
    #[inline]
    fn mtime_nsec(&self) -> u32 {
        (self.meta_a & NSEC_MASK) as u32
    }

    #[inline]
    fn mode_perm(&self) -> u16 {
        ((self.meta_a >> MODE_SHIFT) & u64::from(MODE_PERM_MASK)) as u16
    }

    #[inline]
    fn relations_idx(&self) -> u32 {
        ((self.meta_a >> REL_SHIFT) & RELATIONS_MASK) as u32
    }

    #[inline]
    fn dir_idx(&self) -> u16 {
        ((self.meta_b >> DIR_SHIFT) as u16) & DIR_MASK
    }

    #[inline]
    fn env_idx(&self) -> usize {
        (self.meta_b >> ENV_SHIFT) as usize
    }

    #[inline]
    fn is_arena_key(&self) -> bool {
        self.meta_a & FLAG_ARENA_KEY != 0
    }

    #[inline]
    fn compressed(&self) -> bool {
        self.meta_b & FLAG_COMPACT_COMPRESSED != 0
    }

    #[inline]
    fn generation(&self) -> bool {
        self.meta_a & FLAG_GENERATION != 0
    }

    #[inline]
    fn set_generation(&mut self, generation: bool) {
        if generation {
            self.meta_a |= FLAG_GENERATION;
        } else {
            self.meta_a &= !FLAG_GENERATION;
        }
    }

    #[inline]
    fn set_relations_idx(&mut self, idx: u32) {
        debug_assert!(idx <= NO_RELATIONS);
        self.meta_a =
            (self.meta_a & !(RELATIONS_MASK << REL_SHIFT)) | (u64::from(idx) << REL_SHIFT);
    }

    #[inline]
    fn compact_ts(&self) -> u64 {
        self.meta_b & COMPACT_TS_MASK
    }

    #[inline]
    fn arena_off(&self) -> u32 {
        self.meta_b as u32
    }

    #[inline]
    fn arena_len(&self) -> u16 {
        (self.meta_b >> ARENA_LEN_SHIFT) as u16
    }

    #[inline]
    fn set_arena_off(&mut self, off: u32) {
        self.meta_b = (self.meta_b & !0xFFFF_FFFF) | u64::from(off);
    }

    #[inline]
    fn is_self_owned(&self, relations: &[CachedExtra]) -> bool {
        let rel_idx = self.relations_idx();
        rel_idx == NO_RELATIONS || relations[rel_idx as usize].rollout_id.is_none()
    }

    #[inline]
    fn matches_key(&self, key: &LookupKey<'_>, key_arena: &[u8]) -> bool {
        match *key {
            LookupKey::Compact {
                dir_idx,
                compressed,
                ts,
                uuid,
            } => {
                !self.is_arena_key()
                    && self.dir_idx() == dir_idx
                    && self.compressed() == compressed
                    && self.owner == uuid
                    && self.compact_ts() == ts
            }
            LookupKey::Arena { dir_idx, name } => {
                if !self.is_arena_key()
                    || self.dir_idx() != dir_idx
                    || usize::from(self.arena_len()) != name.len()
                {
                    return false;
                }
                let start = self.arena_off() as usize;
                &key_arena[start..start + name.len()] == name
            }
        }
    }

    #[inline]
    fn matches_stamp(&self, stamp: &FileStamp, relations: &[CachedExtra]) -> bool {
        if self.inode != stamp.inode
            || self.len != stamp.len
            || self.mtime_delta != stamp.mtime_delta
            || self.mtime_nsec() != stamp.mtime_nsec
            || self.mode_perm() != stamp.mode_perm
        {
            return false;
        }
        let rel_idx = self.relations_idx();
        let (expected_ctime_delta, expected_ctime_nsec) = if rel_idx == NO_RELATIONS {
            (self.mtime_delta, self.mtime_nsec())
        } else if let Some((ct, ct_ns)) = relations[rel_idx as usize].ctime {
            (ct, ct_ns)
        } else {
            (self.mtime_delta, self.mtime_nsec())
        };
        stamp.ctime_delta == expected_ctime_delta && stamp.ctime_nsec == expected_ctime_nsec
    }

    #[inline]
    fn settled_before(
        &self,
        base_epoch: i64,
        cutoff: Option<i64>,
        relations: &[CachedExtra],
    ) -> bool {
        cutoff.is_some_and(|cutoff| {
            let mtime = base_epoch.checked_add(i64::from(self.mtime_delta));
            if !mtime.is_some_and(|m| m < cutoff) {
                return false;
            }
            let rel_idx = self.relations_idx();
            if rel_idx != NO_RELATIONS
                && let Some((ctime_delta, _)) = relations[rel_idx as usize].ctime
            {
                base_epoch
                    .checked_add(i64::from(ctime_delta))
                    .is_some_and(|c| c < cutoff)
            } else {
                true
            }
        })
    }

    fn parsed_and_rollout(&self, relations: &[CachedExtra]) -> (ParsedHeader, Uuid) {
        let rel_idx = self.relations_idx();
        let (rollout_id, base, parent) = if rel_idx == NO_RELATIONS {
            (self.owner, None, None)
        } else {
            let extra = relations[rel_idx as usize];
            (
                extra.rollout_id.unwrap_or(self.owner),
                extra.base,
                extra.parent,
            )
        };
        (
            ParsedHeader {
                owner: self.owner,
                paginated: self.meta_a & FLAG_PAGINATED != 0,
                base,
                parent,
            },
            rollout_id,
        )
    }

    fn identity(&self, envs: &[FileEnv]) -> crate::fsutil::Identity {
        crate::fsutil::Identity {
            device: envs[self.env_idx()].device,
            inode: self.inode,
        }
    }
}

#[derive(Default)]
struct ChunkedEntries {
    chunks: Vec<Vec<CachedEntry>>,
    len: usize,
}

impl ChunkedEntries {
    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn get(&self, idx: usize) -> &CachedEntry {
        &self.chunks[idx >> ENTRY_CHUNK_SHIFT][idx & ENTRY_CHUNK_MASK]
    }

    #[inline]
    fn get_mut(&mut self, idx: usize) -> &mut CachedEntry {
        &mut self.chunks[idx >> ENTRY_CHUNK_SHIFT][idx & ENTRY_CHUNK_MASK]
    }

    fn push(&mut self, entry: CachedEntry) {
        let chunk_idx = self.len >> ENTRY_CHUNK_SHIFT;
        if chunk_idx == self.chunks.len() {
            let chunk = if chunk_idx == 0 {
                Vec::new()
            } else {
                Vec::with_capacity(ENTRY_CHUNK_SIZE)
            };
            self.chunks.push(chunk);
        }
        self.chunks[chunk_idx].push(entry);
        self.len += 1;
    }

    fn swap_remove(&mut self, idx: usize) -> CachedEntry {
        let last_idx = self.len - 1;
        let last_chunk = self.chunks.last_mut().expect("non-empty chunked entries");
        let last = last_chunk.pop().expect("non-empty last chunk");
        if last_chunk.is_empty() {
            self.chunks.pop();
        }
        self.len = last_idx;
        if idx == last_idx {
            last
        } else {
            std::mem::replace(self.get_mut(idx), last)
        }
    }

    fn truncate(&mut self, new_len: usize) {
        if new_len >= self.len {
            return;
        }
        let full_chunks = new_len >> ENTRY_CHUNK_SHIFT;
        let rem = new_len & ENTRY_CHUNK_MASK;
        if rem == 0 {
            self.chunks.truncate(full_chunks);
        } else {
            self.chunks.truncate(full_chunks + 1);
            self.chunks[full_chunks].truncate(rem);
        }
        self.len = new_len;
    }

    fn clear(&mut self) {
        self.chunks.clear();
        self.len = 0;
    }

    fn iter(&self) -> impl Iterator<Item = &CachedEntry> {
        self.chunks.iter().flat_map(|c| c.iter())
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &mut CachedEntry> {
        self.chunks.iter_mut().flat_map(|c| c.iter_mut())
    }
}

/// Run-local header reuse only. Directory discovery, graph construction and
/// candidate validation remain live; SQLite versions cannot describe orphans.
#[derive(Default)]
pub(crate) struct HeaderCache {
    entries: ChunkedEntries,
    buckets: Vec<u32>,
    relations: Vec<CachedExtra>,
    key_arena: Vec<u8>,
    dirs: Vec<(u8, Box<[u8]>)>,
    envs: SmallVec<[FileEnv; 2]>,
    seed0: u64,
    seed1: u64,
    base_epoch: Option<i64>,
    generation: bool,
    stable_before: Option<i64>,
    relations_dirty: bool,
    arena_dirty: bool,
    #[cfg(test)]
    reads: usize,
}

impl HeaderCache {
    #[inline]
    fn ensure_seeds(&mut self) {
        if self.seed0 == 0 {
            let (s0, s1) = random_hash_seeds();
            self.seed0 = s0;
            self.seed1 = s1;
        }
    }

    fn clear_all(&mut self) {
        self.entries.clear();
        self.buckets.clear();
        self.relations.clear();
        self.key_arena.clear();
        self.dirs.clear();
        self.envs.clear();
        self.base_epoch = None;
        self.relations_dirty = false;
        self.arena_dirty = false;
    }

    fn lookup_dir(&self, root_tag: u8, rel_prefix: &[u8]) -> Option<u16> {
        self.dirs
            .iter()
            .position(|(tag, rel)| *tag == root_tag && rel.as_ref() == rel_prefix)
            .and_then(|pos| u16::try_from(pos).ok())
            .filter(|&idx| idx <= MAX_DIR_IDX)
    }

    fn intern_dir(&mut self, root_tag: u8, rel_prefix: &[u8]) -> Option<u16> {
        if let Some(idx) = self.lookup_dir(root_tag, rel_prefix) {
            return Some(idx);
        }
        let idx = u16::try_from(self.dirs.len()).ok()?;
        if idx > MAX_DIR_IDX {
            return None;
        }
        self.dirs.push((root_tag, rel_prefix.into()));
        Some(idx)
    }

    fn intern_env(&mut self, env: FileEnv) -> Option<u8> {
        if let Some(pos) = self.envs.iter().position(|&existing| existing == env) {
            return u8::try_from(pos).ok().filter(|&idx| idx <= MAX_ENV_IDX);
        }
        let idx = u8::try_from(self.envs.len()).ok()?;
        if idx > MAX_ENV_IDX {
            return None;
        }
        self.envs.push(env);
        Some(idx)
    }

    #[inline]
    fn hash_compact(&self, dir_idx: u16, compressed: bool, ts: u64, uuid: Uuid) -> u64 {
        let u = uuid.as_u128();
        let lo = u as u64;
        let hi = (u >> 64) as u64;
        let meta = (ts << 17) | (u64::from(dir_idx) << 1) | u64::from(compressed);
        let h = wymix(lo ^ self.seed0, hi ^ self.seed1);
        wymix(h ^ meta, self.seed0 ^ 0xa076_1d64_78bd_642f)
    }

    #[inline]
    fn hash_arena(&self, dir_idx: u16, name: &[u8]) -> u64 {
        hash_bytes_keyed(self.seed0, self.seed1, (u64::from(dir_idx) << 1) | 1, name)
    }

    #[inline]
    fn hash_lookup_key(&self, key: &LookupKey<'_>) -> u64 {
        match *key {
            LookupKey::Compact {
                dir_idx,
                compressed,
                ts,
                uuid,
            } => self.hash_compact(dir_idx, compressed, ts, uuid),
            LookupKey::Arena { dir_idx, name } => self.hash_arena(dir_idx, name),
        }
    }

    #[inline]
    fn hash_entry(&self, entry: &CachedEntry) -> u64 {
        if entry.is_arena_key() {
            let start = entry.arena_off() as usize;
            let name = &self.key_arena[start..start + usize::from(entry.arena_len())];
            self.hash_arena(entry.dir_idx(), name)
        } else {
            self.hash_compact(
                entry.dir_idx(),
                entry.compressed(),
                entry.compact_ts(),
                entry.owner,
            )
        }
    }

    fn find_slot(&self, key: &LookupKey<'_>, hash: u64) -> Option<(usize, usize)> {
        if self.buckets.is_empty() {
            return None;
        }
        let fp_shifted = ((hash >> 56) as u32) << 24;
        let mask = self.buckets.len() - 1;
        let mut pos = (hash as usize) & mask;
        loop {
            let slot = self.buckets[pos];
            if slot == BUCKET_EMPTY {
                return None;
            }
            if (slot & BUCKET_FP_MASK) == fp_shifted {
                let idx = bucket_idx(slot);
                if self.entries.get(idx).matches_key(key, &self.key_arena) {
                    return Some((pos, idx));
                }
            }
            pos = (pos + 1) & mask;
        }
    }

    #[inline]
    fn bucket_len_for(entries: usize) -> usize {
        let mut len = 16usize;
        while entries * 20 >= len * 17 {
            len *= 2;
        }
        len
    }

    fn ensure_bucket_capacity(&mut self) {
        let needed = self.entries.len() + 1;
        if !self.buckets.is_empty() && needed * 20 < self.buckets.len() * 17 {
            return;
        }
        let mut new_len = (self.buckets.len() * 2).max(16);
        while needed * 20 >= new_len * 17 {
            new_len *= 2;
        }
        self.buckets = Vec::new();
        self.buckets = vec![BUCKET_EMPTY; new_len];
        self.rehash_buckets();
    }

    fn rehash_buckets(&mut self) {
        if self.buckets.is_empty() {
            return;
        }
        self.buckets.fill(BUCKET_EMPTY);
        let mask = self.buckets.len() - 1;
        for idx in 0..self.entries.len() {
            let hash = self.hash_entry(self.entries.get(idx));
            let mut pos = (hash as usize) & mask;
            while self.buckets[pos] != BUCKET_EMPTY {
                pos = (pos + 1) & mask;
            }
            self.buckets[pos] = pack_bucket(hash, idx);
        }
    }

    fn remove_key(&mut self, key: &LookupKey<'_>, hash: u64) {
        let Some((pos, entry_idx)) = self.find_slot(key, hash) else {
            return;
        };
        let removed = *self.entries.get(entry_idx);
        if removed.relations_idx() != NO_RELATIONS {
            self.relations_dirty = true;
        }
        if removed.is_arena_key() {
            self.arena_dirty = true;
        }
        let mask = self.buckets.len() - 1;
        let mut hole = pos;
        let mut cur = (hole + 1) & mask;
        while self.buckets[cur] != BUCKET_EMPTY {
            let slot = self.buckets[cur];
            let candidate = bucket_idx(slot);
            let ideal = (self.hash_entry(self.entries.get(candidate)) as usize) & mask;
            if (hole.wrapping_sub(ideal) & mask) < (cur.wrapping_sub(ideal) & mask) {
                self.buckets[hole] = slot;
                hole = cur;
            }
            cur = (cur + 1) & mask;
        }
        self.buckets[hole] = BUCKET_EMPTY;

        let last_idx = self.entries.len() - 1;
        self.entries.swap_remove(entry_idx);
        if entry_idx != last_idx {
            let moved_hash = self.hash_entry(self.entries.get(entry_idx));
            let expected_old = pack_bucket(moved_hash, last_idx);
            let mut cur = (moved_hash as usize) & mask;
            while self.buckets[cur] != expected_old {
                cur = (cur + 1) & mask;
            }
            self.buckets[cur] = pack_bucket(moved_hash, entry_idx);
        }
    }

    fn retain_generation(&mut self) {
        let generation = self.generation;
        let before_len = self.entries.len();
        let mut write = 0usize;
        for read in 0..before_len {
            let entry = *self.entries.get(read);
            if entry.generation() == generation {
                if write != read {
                    *self.entries.get_mut(write) = entry;
                }
                write += 1;
            } else {
                if entry.relations_idx() != NO_RELATIONS {
                    self.relations_dirty = true;
                }
                if entry.is_arena_key() {
                    self.arena_dirty = true;
                }
            }
        }
        self.entries.truncate(write);
        if self.entries.is_empty() {
            self.clear_all();
            return;
        }
        if self.relations_dirty {
            let mut new_relations = Vec::new();
            for entry in self.entries.iter_mut() {
                let rel_idx = entry.relations_idx();
                if rel_idx != NO_RELATIONS {
                    let next = new_relations.len() as u32;
                    new_relations.push(self.relations[rel_idx as usize]);
                    entry.set_relations_idx(next);
                }
            }
            self.relations = new_relations;
            self.relations_dirty = false;
        }
        if self.arena_dirty {
            let mut new_arena = Vec::new();
            for entry in self.entries.iter_mut() {
                if entry.is_arena_key() {
                    let start = entry.arena_off() as usize;
                    let len = usize::from(entry.arena_len());
                    let next_off = new_arena.len() as u32;
                    new_arena.extend_from_slice(&self.key_arena[start..start + len]);
                    entry.set_arena_off(next_off);
                }
            }
            self.key_arena = new_arena;
            self.arena_dirty = false;
        }
        if write != before_len {
            let target_len = Self::bucket_len_for(self.entries.len());
            if target_len * 2 <= self.buckets.len() && self.buckets.len() > 1024 {
                self.buckets = Vec::new();
                self.buckets = vec![BUCKET_EMPTY; target_len];
            }
            self.rehash_buckets();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn store(
        &mut self,
        dir_idx: Option<u16>,
        name: &[u8],
        pre_key_hash: Option<(LookupKey<'_>, u64)>,
        pre_compact_ts: Option<u64>,
        raw_stamp: RawStamp,
        parsed: ParsedHeader,
        rollout_id: Uuid,
    ) -> bool {
        let Some(dir_idx) = dir_idx else {
            return false;
        };
        if !raw_stamp.settled_before(self.stable_before) {
            // A recent in-place edit may retain both timestamps in one coarse
            // tick. Read it fresh on every scan until its clock window closes.
            if !self.entries.is_empty() {
                let (key, hash) = pre_key_hash.unwrap_or_else(|| {
                    let key = if let Some(ts) = pre_compact_ts {
                        let compressed = name.ends_with(b".zst");
                        debug_assert_eq!(
                            parse_compact_rollout_key(name),
                            Some((ts, parsed.owner, compressed))
                        );
                        LookupKey::Compact {
                            dir_idx,
                            compressed,
                            ts,
                            uuid: parsed.owner,
                        }
                    } else {
                        lookup_key(dir_idx, name)
                    };
                    let hash = self.hash_lookup_key(&key);
                    (key, hash)
                });
                self.remove_key(&key, hash);
            }
            return false;
        }
        self.ensure_seeds();
        let (key, hash) = pre_key_hash.unwrap_or_else(|| {
            let key = if let Some(ts) = pre_compact_ts {
                let compressed = name.ends_with(b".zst");
                debug_assert_eq!(
                    parse_compact_rollout_key(name),
                    Some((ts, parsed.owner, compressed))
                );
                LookupKey::Compact {
                    dir_idx,
                    compressed,
                    ts,
                    uuid: parsed.owner,
                }
            } else {
                lookup_key(dir_idx, name)
            };
            let hash = self.hash_lookup_key(&key);
            (key, hash)
        });
        let Some(base_epoch) = self.base_epoch.or(self.stable_before) else {
            self.remove_key(&key, hash);
            return false;
        };
        self.base_epoch = Some(base_epoch);
        let Some((stamp, env)) = raw_stamp.into_cacheable(base_epoch) else {
            self.remove_key(&key, hash);
            return false;
        };
        let Some(env_idx) = self.intern_env(env) else {
            self.remove_key(&key, hash);
            return false;
        };
        let split_ctime = (stamp.ctime_delta != stamp.mtime_delta
            || stamp.ctime_nsec != stamp.mtime_nsec)
            .then_some((stamp.ctime_delta, stamp.ctime_nsec));
        let reverted_rollout = (rollout_id != parsed.owner).then_some(rollout_id);
        let has_extra = reverted_rollout.is_some()
            || parsed.base.is_some()
            || parsed.parent.is_some()
            || split_ctime.is_some();
        let extra = CachedExtra {
            rollout_id: reverted_rollout,
            base: parsed.base,
            parent: parsed.parent,
            ctime: split_ctime,
        };
        let mut flags = (if parsed.paginated { FLAG_PAGINATED } else { 0 })
            | (if self.generation { FLAG_GENERATION } else { 0 });

        if let Some((_, existing_idx)) = self.find_slot(&key, hash) {
            let old_rel = self.entries.get(existing_idx).relations_idx();
            let relations_idx = if has_extra {
                if old_rel != NO_RELATIONS {
                    self.relations[old_rel as usize] = extra;
                    old_rel
                } else if let Ok(idx) = u32::try_from(self.relations.len())
                    && idx < NO_RELATIONS
                {
                    self.relations.push(extra);
                    idx
                } else {
                    self.remove_key(&key, hash);
                    return false;
                }
            } else {
                if old_rel != NO_RELATIONS {
                    self.relations_dirty = true;
                }
                NO_RELATIONS
            };
            let existing = self.entries.get_mut(existing_idx);
            flags |= existing.meta_a & FLAG_ARENA_KEY;
            existing.owner = parsed.owner;
            existing.inode = stamp.inode;
            existing.mtime_delta = stamp.mtime_delta;
            existing.len = stamp.len;
            existing.meta_a = u64::from(stamp.mtime_nsec)
                | (u64::from(stamp.mode_perm) << MODE_SHIFT)
                | (u64::from(relations_idx) << REL_SHIFT)
                | flags;
            existing.meta_b =
                (existing.meta_b & !(0xFu64 << ENV_SHIFT)) | (u64::from(env_idx) << ENV_SHIFT);
            return true;
        }

        let entry_idx = self.entries.len();
        if entry_idx >= BUCKET_MAX_ENTRIES {
            return false;
        }
        let mut meta_b = (u64::from(dir_idx) << DIR_SHIFT) | (u64::from(env_idx) << ENV_SHIFT);
        let arena_start = match key {
            LookupKey::Compact {
                compressed,
                ts,
                uuid,
                ..
            } => {
                debug_assert_eq!(uuid, parsed.owner);
                debug_assert!(ts <= COMPACT_TS_MASK);
                meta_b |= ts;
                if compressed {
                    meta_b |= FLAG_COMPACT_COMPRESSED;
                }
                None
            }
            LookupKey::Arena { name, .. } => {
                let Ok(off) = u32::try_from(self.key_arena.len()) else {
                    return false;
                };
                let Ok(len) = u16::try_from(name.len()) else {
                    return false;
                };
                self.key_arena.extend_from_slice(name);
                flags |= FLAG_ARENA_KEY;
                meta_b |= u64::from(off) | (u64::from(len) << ARENA_LEN_SHIFT);
                Some(off as usize)
            }
        };
        let relations_idx = if has_extra {
            if let Ok(idx) = u32::try_from(self.relations.len())
                && idx < NO_RELATIONS
            {
                self.relations.push(extra);
                idx
            } else {
                if let Some(start) = arena_start {
                    self.key_arena.truncate(start);
                }
                return false;
            }
        } else {
            NO_RELATIONS
        };
        self.ensure_bucket_capacity();
        let entry = CachedEntry {
            owner: parsed.owner,
            inode: stamp.inode,
            meta_a: u64::from(stamp.mtime_nsec)
                | (u64::from(stamp.mode_perm) << MODE_SHIFT)
                | (u64::from(relations_idx) << REL_SHIFT)
                | flags,
            meta_b,
            mtime_delta: stamp.mtime_delta,
            len: stamp.len,
        };
        let mask = self.buckets.len() - 1;
        let mut pos = (hash as usize) & mask;
        while self.buckets[pos] != BUCKET_EMPTY {
            pos = (pos + 1) & mask;
        }
        self.entries.push(entry);
        self.buckets[pos] = pack_bucket(hash, entry_idx);
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    inode: u64,
    mtime_delta: i32,
    ctime_delta: i32,
    mtime_nsec: u32,
    ctime_nsec: u32,
    len: u32,
    mode_perm: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RawStamp {
    device: u64,
    inode: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
}

#[inline]
fn check_nsec(nsec: i64) -> Option<u32> {
    u32::try_from(nsec).ok().filter(|&n| n < 1_000_000_000)
}

impl RawStamp {
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            len: stat.st_size as u64,
            mtime: stat.st_mtime as i64,
            mtime_nsec: stat.st_mtime_nsec as i64,
            ctime: stat.st_ctime as i64,
            ctime_nsec: stat.st_ctime_nsec as i64,
            mode: stat.st_mode as u32,
            uid: stat.st_uid as u32,
            gid: stat.st_gid as u32,
            links: stat.st_nlink as u64,
        }
    }

    fn identity(&self) -> crate::fsutil::Identity {
        crate::fsutil::Identity {
            device: self.device,
            inode: self.inode,
        }
    }

    #[inline]
    fn settled_before(&self, cutoff: Option<i64>) -> bool {
        cutoff.is_some_and(|cutoff| self.mtime < cutoff && self.ctime < cutoff)
    }

    fn into_cacheable(self, base_epoch: i64) -> Option<(FileStamp, FileEnv)> {
        if self.links != 1 || self.mode & !0o177777 != 0 {
            return None;
        }
        Some((
            FileStamp {
                inode: self.inode,
                mtime_delta: i32::try_from(self.mtime.checked_sub(base_epoch)?).ok()?,
                ctime_delta: i32::try_from(self.ctime.checked_sub(base_epoch)?).ok()?,
                mtime_nsec: check_nsec(self.mtime_nsec)?,
                ctime_nsec: check_nsec(self.ctime_nsec)?,
                len: u32::try_from(self.len).ok()?,
                mode_perm: (self.mode & MODE_PERM_MASK) as u16,
            },
            FileEnv {
                device: self.device,
                uid: self.uid,
                gid: self.gid,
            },
        ))
    }
}

impl FileStamp {
    #[allow(clippy::unnecessary_cast)]
    fn from_regular_stat(stat: &Stat, base_epoch: i64) -> Option<(Self, FileEnv)> {
        let mode = stat.st_mode as u32;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_nlink != 1
            || mode & !0o177777 != 0
        {
            return None;
        }
        Some((
            Self {
                inode: stat.st_ino as u64,
                mtime_delta: i32::try_from((stat.st_mtime as i64).checked_sub(base_epoch)?).ok()?,
                ctime_delta: i32::try_from((stat.st_ctime as i64).checked_sub(base_epoch)?).ok()?,
                mtime_nsec: check_nsec(stat.st_mtime_nsec as i64)?,
                ctime_nsec: check_nsec(stat.st_ctime_nsec as i64)?,
                len: u32::try_from(stat.st_size).ok()?,
                mode_perm: (mode & MODE_PERM_MASK) as u16,
            },
            FileEnv {
                device: stat.st_dev as u64,
                uid: stat.st_uid as u32,
                gid: stat.st_gid as u32,
            },
        ))
    }
}

/// A nanosecond field does not guarantee nanosecond timestamp updates. Keep a
/// complete wall-clock second between a cached stamp and this scan: writes in
/// the current coarse clock tick must never reuse an indistinguishable stamp.
fn cache_cutoff() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .and_then(|seconds| seconds.checked_sub(1))
}

#[cfg(test)]
impl HeaderCache {
    pub(crate) fn after_files_settle(paths: &[&Path]) -> Self {
        use std::os::unix::fs::MetadataExt;
        use std::time::{Duration, Instant};
        let newest = paths
            .iter()
            .map(|path| {
                let metadata = std::fs::metadata(path).unwrap();
                metadata.ctime().max(metadata.mtime())
            })
            .max()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache_cutoff().is_none_or(|cutoff| newest >= cutoff) {
            assert!(
                Instant::now() < deadline,
                "filesystem stamps did not leave the current clock window"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Self::default()
    }
}

fn format_entry_path(root: &Path, rel_prefix: &[u8], name: &[u8]) -> PathBuf {
    let mut path = root.to_path_buf();
    if !rel_prefix.is_empty() {
        path.push(OsStr::from_bytes(rel_prefix));
    }
    path.push(OsStr::from_bytes(name));
    path
}

struct ScannedFile {
    parsed: ParsedHeader,
    rollout_id: Uuid,
    identity: crate::fsutil::Identity,
    bytes: u64,
}

type CacheProbe<'a> = (Option<ScannedFile>, Option<(LookupKey<'a>, u64)>);

#[allow(clippy::too_many_arguments)]
fn try_cache_hit<'a>(
    dir_fd: rustix::fd::BorrowedFd<'_>,
    entry_name: &'a CStr,
    root: &Path,
    rel_prefix: &[u8],
    cache_dir_idx: u16,
    pre_stat: Option<Stat>,
    cache: &mut HeaderCache,
) -> Result<CacheProbe<'a>> {
    let Some(base_epoch) = cache.base_epoch else {
        return Ok((None, None));
    };
    let key = lookup_key(cache_dir_idx, entry_name.to_bytes());
    let hash = cache.hash_lookup_key(&key);
    let Some((_, entry_idx)) = cache.find_slot(&key, hash) else {
        return Ok((None, Some((key, hash))));
    };
    // fstatat(AT_SYMLINK_NOFOLLOW) on every hit detects replaced paths,
    // symlinks, hardlinks, appends, in-place edits and permission changes.
    // Never use directory timestamps or a database-only freshness token.
    // Reuse a fresh stat already performed for FileType::Unknown on DT_UNKNOWN filesystems.
    let stat = match pre_stat {
        Some(stat) => stat,
        None => statat(dir_fd, entry_name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)
            .with_context(|| {
                format!(
                    "inspect rollout {}",
                    format_entry_path(root, rel_prefix, entry_name.to_bytes()).display()
                )
            })?,
    };
    let cached = cache.entries.get_mut(entry_idx);
    if let Some((stamp, env)) = FileStamp::from_regular_stat(&stat, base_epoch)
        && cached.matches_stamp(&stamp, &cache.relations)
        && cache.envs[cached.env_idx()] == env
        && cached.settled_before(base_epoch, cache.stable_before, &cache.relations)
    {
        let (parsed, rollout_id) = cached.parsed_and_rollout(&cache.relations);
        let identity = cached.identity(&cache.envs);
        let bytes = u64::from(cached.len);
        cached.set_generation(cache.generation);
        return Ok((
            Some(ScannedFile {
                parsed,
                rollout_id,
                identity,
                bytes,
            }),
            None,
        ));
    }
    Ok((None, Some((key, hash))))
}

fn read_miss_at(
    dir_fd: rustix::fd::BorrowedFd<'_>,
    entry_name: &CStr,
    root: &Path,
    rel_prefix: &[u8],
    verify_stable: bool,
    reader: &mut crate::metadata::RecordReader,
) -> Result<Option<(ScannedFile, RawStamp, Option<u64>)>> {
    let Ok(name) = simdutf8::basic::from_utf8(entry_name.to_bytes()) else {
        return Ok(None);
    };
    let Some(filename) = filename(name)? else {
        return Ok(None);
    };
    let fd = openat(
        dir_fd,
        entry_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)
    .with_context(|| {
        format!(
            "open {}",
            format_entry_path(root, rel_prefix, entry_name.to_bytes()).display()
        )
    })?;
    let file = File::from(fd);
    let stat = fstat(&file).map_err(std::io::Error::from)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile && stat.st_nlink == 1,
        "expected a regular file with one link: {}",
        format_entry_path(root, rel_prefix, entry_name.to_bytes()).display()
    );
    let raw_stamp = RawStamp::from_stat(&stat);
    let bytes = raw_stamp.len;
    let line = reader
        .read_first_record(&file, filename.compressed)
        .with_context(|| {
            format!(
                "read rollout header {}",
                format_entry_path(root, rel_prefix, entry_name.to_bytes()).display()
            )
        })?;
    let parsed = header(line).with_context(|| {
        format!(
            "invalid rollout header {}",
            format_entry_path(root, rel_prefix, entry_name.to_bytes()).display()
        )
    })?;
    ensure!(
        parsed.owner == filename.owner,
        "rollout filename and metadata owners differ"
    );
    if verify_stable {
        // Bind a successful parse to the same opened object's stable metadata.
        // A changing file fails this scan rather than seeding a stale cache.
        let after = RawStamp::from_stat(&fstat(&file).map_err(std::io::Error::from)?);
        ensure!(
            after == raw_stamp,
            "rollout changed while reading header: {}",
            format_entry_path(root, rel_prefix, entry_name.to_bytes()).display()
        );
    }
    let identity = raw_stamp.identity();
    Ok(Some((
        ScannedFile {
            parsed,
            rollout_id: filename.rollout,
            identity,
            bytes,
        },
        raw_stamp,
        filename.compact_ts,
    )))
}

#[allow(clippy::too_many_arguments)]
fn record_scanned_file<S: BuildHasher>(
    index: &mut LineageIndex,
    candidate_owners: &HashSet<Uuid, S>,
    root: &Path,
    rel_prefix: &[u8],
    dir_idx_cache: &mut Option<u32>,
    name_bytes: &[u8],
    scanned: ScannedFile,
    in_cache: bool,
) -> Result<()> {
    let ScannedFile {
        parsed,
        rollout_id,
        identity,
        bytes,
    } = scanned;
    let ParsedHeader {
        owner,
        paginated,
        base,
        parent,
    } = parsed;
    index.record_owner(rollout_id, owner, in_cache)?;
    if let Some(base) = base {
        index.references.push((owner, base));
        index
            .incoming
            .entry(base)
            .and_modify(|sources| {
                sources.mixed |= sources.first != owner;
            })
            .or_insert(ReferringOwners {
                first: owner,
                mixed: false,
            });
    }
    if let Some(parent) = parent {
        index.parents.insert((owner, parent));
    }
    if candidate_owners.contains(&owner) {
        if index.owned.is_empty() && index.owned.capacity() < candidate_owners.len() {
            index.owned.reserve_exact(candidate_owners.len());
        }
        let dir_idx = match *dir_idx_cache {
            Some(idx) => idx,
            None => {
                let idx = u32::try_from(index.owned_dirs.len())
                    .context("too many lineage directories")?;
                let dir_path = if rel_prefix.is_empty() {
                    root.to_path_buf()
                } else {
                    root.join(OsStr::from_bytes(rel_prefix))
                };
                index.owned_dirs.push(dir_path);
                *dir_idx_cache = Some(idx);
                idx
            }
        };
        let name_packed = if let Some((ts, parsed_uuid, compressed)) =
            parse_compact_rollout_key(name_bytes)
            && parsed_uuid == rollout_id
        {
            ts | if compressed {
                FLAG_COMPACT_COMPRESSED
            } else {
                0
            }
        } else {
            let (name_off, name_len) = index
                .owned_names
                .push(name_bytes)
                .context("rollout filename exceeds 64 KiB")?;
            OWNED_NAME_ARENA_FLAG | (u64::from(name_len) << 32) | u64::from(name_off)
        };
        index.owned.push(OwnedRollout {
            owner,
            rollout_id,
            identity,
            bytes,
            name_packed,
            dir_idx,
            paginated,
        });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn scan<S: BuildHasher>(
    home: &Path,
    candidate_owners: &HashSet<Uuid, S>,
    cache: Option<&mut HeaderCache>,
) -> Result<LineageIndex> {
    let mut index = LineageIndex::default();
    let mut reader = crate::metadata::RecordReader::new();
    scan_into(home, candidate_owners, cache, &mut reader, &mut index)?;
    Ok(index)
}

pub(crate) fn scan_into<S: BuildHasher>(
    home: &Path,
    candidate_owners: &HashSet<Uuid, S>,
    mut cache: Option<&mut HeaderCache>,
    reader: &mut crate::metadata::RecordReader,
    index: &mut LineageIndex,
) -> Result<()> {
    if let Some(cache) = cache.as_mut() {
        cache.generation = !cache.generation;
        cache.stable_before = cache_cutoff();
        if cache.base_epoch.is_none() {
            cache.base_epoch = cache.stable_before;
        }
    }
    let result = scan_inventory_into(home, candidate_owners, cache.as_deref_mut(), reader, index);
    if let Some(cache) = cache {
        if result.is_ok() {
            cache.retain_generation();
        } else {
            cache.clear_all();
        }
    }
    result
}

#[cfg(test)]
fn scan_inventory<S: BuildHasher>(
    home: &Path,
    candidate_owners: &HashSet<Uuid, S>,
    cache: Option<&mut HeaderCache>,
) -> Result<LineageIndex> {
    let mut index = LineageIndex::default();
    let mut reader = crate::metadata::RecordReader::new();
    scan_inventory_into(home, candidate_owners, cache, &mut reader, &mut index)?;
    Ok(index)
}

fn open_subdir_nofollow(
    root_fd: rustix::fd::BorrowedFd<'_>,
    rel_path: &[u8],
) -> rustix::io::Result<rustix::fd::OwnedFd> {
    let mut current: Option<rustix::fd::OwnedFd> = None;
    for component in rel_path.split(|&byte| byte == b'/') {
        let parent = current.as_ref().map_or(root_fd, rustix::fd::AsFd::as_fd);
        let next = openat(
            parent,
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        current = Some(next);
    }
    Ok(current.expect("non-empty relative subdirectory path"))
}

fn scan_inventory_into<S: BuildHasher>(
    home: &Path,
    candidate_owners: &HashSet<Uuid, S>,
    mut cache: Option<&mut HeaderCache>,
    reader: &mut crate::metadata::RecordReader,
    index: &mut LineageIndex,
) -> Result<()> {
    index.clear();
    let reserve_owned = candidate_owners.len().min(100_000);
    if index.owned.capacity() < reserve_owned {
        index.owned.reserve_exact(reserve_owned - index.owned.len());
    }
    let had_cached_entries = cache.as_deref().is_some_and(|c| !c.entries.is_empty());
    let mut entries = 0usize;
    let mut subdirs: Vec<Vec<u8>> = Vec::new();

    for (root_tag, root_name) in [(0u8, "sessions"), (1u8, "archived_sessions")] {
        let root = home.join(root_name);
        let root_fd = match openat(
            CWD,
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                bail!("session root is not a real directory");
            }
            Err(error) => {
                return Err(std::io::Error::from(error)).context("inspect session directory");
            }
        };
        entries += 1;
        ensure!(
            entries <= 500_000,
            "lineage inventory exceeds the 500,000-entry safety limit"
        );

        subdirs.clear();
        // Start with the root directory itself (empty relative prefix), then
        // descend into any subdirectories one open Dir FD at a time.
        let mut current_rel: Option<Vec<u8>> = Some(Vec::new());
        while let Some(rel_prefix) = current_rel {
            let mut dir = if rel_prefix.is_empty() {
                Dir::read_from(&root_fd)
            } else {
                let sub_fd = open_subdir_nofollow(rustix::fd::AsFd::as_fd(&root_fd), &rel_prefix)
                    .map_err(std::io::Error::from)
                    .context("cannot inspect all session lineage paths")?;
                Dir::new(sub_fd)
            }
            .map_err(std::io::Error::from)
            .context("cannot inspect all session lineage paths")?;

            let mut cache_dir_idx = cache
                .as_deref()
                .and_then(|cache| cache.lookup_dir(root_tag, &rel_prefix));
            let mut dir_idx_cache: Option<u32> = None;

            while let Some(entry) = dir.read() {
                let entry = entry
                    .map_err(std::io::Error::from)
                    .context("cannot inspect all session lineage paths")?;
                let c_name = entry.file_name();
                let name_bytes = c_name.to_bytes();
                if name_bytes == b"." || name_bytes == b".." {
                    continue;
                }
                entries += 1;
                ensure!(
                    entries <= 500_000,
                    "lineage inventory exceeds the 500,000-entry safety limit"
                );
                ensure!(
                    !name_bytes.starts_with(b".codex-retain-pending-"),
                    "pending retention artifact prevents lineage verification: {}",
                    format_entry_path(&root, &rel_prefix, name_bytes).display()
                );
                let mut kind = entry.file_type();
                let mut pre_stat: Option<Stat> = None;
                if kind == FileType::Unknown {
                    let dir_fd = dir
                        .fd()
                        .map_err(std::io::Error::from)
                        .context("cannot inspect all session lineage paths")?;
                    let stat = statat(dir_fd, c_name, AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(std::io::Error::from)
                        .context("cannot inspect all session lineage paths")?;
                    kind = FileType::from_raw_mode(stat.st_mode);
                    pre_stat = Some(stat);
                }
                if kind == FileType::Directory {
                    let mut child_rel = Vec::with_capacity(
                        rel_prefix.len() + usize::from(!rel_prefix.is_empty()) + name_bytes.len(),
                    );
                    if !rel_prefix.is_empty() {
                        child_rel.extend_from_slice(&rel_prefix);
                        child_rel.push(b'/');
                    }
                    child_rel.extend_from_slice(name_bytes);
                    subdirs.push(child_rel);
                    continue;
                }
                ensure!(
                    kind == FileType::RegularFile,
                    "lineage inventory contains a symlink or nonregular entry: {}",
                    format_entry_path(&root, &rel_prefix, name_bytes).display()
                );
                if !name_bytes.starts_with(b"rollout-") {
                    continue;
                }
                let dir_fd = dir
                    .fd()
                    .map_err(std::io::Error::from)
                    .context("cannot inspect all session lineage paths")?;
                let mut pre_key_hash = None;
                if had_cached_entries
                    && let Some(cache) = cache.as_deref_mut()
                    && let Some(dir_idx) = cache_dir_idx
                {
                    let (hit, key_hash) = try_cache_hit(
                        dir_fd,
                        c_name,
                        &root,
                        &rel_prefix,
                        dir_idx,
                        pre_stat,
                        cache,
                    )?;
                    if let Some(scanned) = hit {
                        record_scanned_file(
                            index,
                            candidate_owners,
                            &root,
                            &rel_prefix,
                            &mut dir_idx_cache,
                            name_bytes,
                            scanned,
                            true,
                        )?;
                        continue;
                    }
                    pre_key_hash = key_hash;
                }
                let Some((scanned, raw_stamp, compact_ts)) =
                    read_miss_at(dir_fd, c_name, &root, &rel_prefix, cache.is_some(), reader)?
                else {
                    continue;
                };
                let mut in_cache = false;
                if let Some(cache) = cache.as_deref_mut() {
                    if cache_dir_idx.is_none() && raw_stamp.settled_before(cache.stable_before) {
                        cache_dir_idx = cache.intern_dir(root_tag, &rel_prefix);
                    }
                    in_cache = cache.store(
                        cache_dir_idx,
                        name_bytes,
                        pre_key_hash,
                        compact_ts,
                        raw_stamp,
                        scanned.parsed,
                        scanned.rollout_id,
                    );
                    #[cfg(test)]
                    {
                        cache.reads += 1;
                    }
                }
                record_scanned_file(
                    index,
                    candidate_owners,
                    &root,
                    &rel_prefix,
                    &mut dir_idx_cache,
                    name_bytes,
                    scanned,
                    in_cache,
                )?;
            }
            current_rel = subdirs.pop();
        }
    }
    index.finalize_owners(cache.as_deref())?;
    let owned_dirs = &index.owned_dirs;
    let owned_names = &index.owned_names;
    index.owned.sort_unstable_by(|left, right| {
        left.owner.cmp(&right.owner).then_with(|| {
            let mut left_buf = [0u8; 74];
            let mut right_buf = [0u8; 74];
            let left_name = decode_owned_name(owned_names, left, &mut left_buf);
            let right_name = decode_owned_name(owned_names, right, &mut right_buf);
            if left.dir_idx == right.dir_idx {
                left_name.cmp(right_name)
            } else {
                owned_dirs[left.dir_idx as usize]
                    .components()
                    .chain(std::iter::once(std::path::Component::Normal(left_name)))
                    .cmp(
                        owned_dirs[right.dir_idx as usize]
                            .components()
                            .chain(std::iter::once(std::path::Component::Normal(right_name))),
                    )
            }
        })
    });
    Ok(())
}

#[inline]
fn decode_owned_name<'a>(
    owned_names: &'a ChunkedBytes,
    entry: &OwnedRollout,
    buf: &'a mut [u8; 74],
) -> &'a OsStr {
    if entry.name_packed & OWNED_NAME_ARENA_FLAG != 0 {
        let off = entry.name_packed as u32;
        let len = (entry.name_packed >> 32) as u16;
        OsStr::from_bytes(owned_names.get(off, len))
    } else {
        let ts = entry.name_packed & COMPACT_TS_MASK;
        let compressed = entry.name_packed & FLAG_COMPACT_COMPRESSED != 0;
        let len = format_compact_rollout_name(ts, entry.rollout_id, compressed, buf);
        OsStr::from_bytes(&buf[..len])
    }
}

const HEX_INVALID: u8 = 0xFF;
const HEX_LUT: [u8; 256] = {
    let mut lut = [HEX_INVALID; 256];
    let mut i = 0u8;
    while i < 10 {
        lut[(b'0' + i) as usize] = i;
        i += 1;
    }
    let mut i = 0u8;
    while i < 6 {
        lut[(b'a' + i) as usize] = 10 + i;
        i += 1;
    }
    lut
};

#[inline]
fn parse_canonical_uuid_bytes(bytes: &[u8]) -> Option<Uuid> {
    if bytes.len() != 36
        || bytes[8] != b'-'
        || bytes[13] != b'-'
        || bytes[18] != b'-'
        || bytes[23] != b'-'
    {
        return None;
    }
    const OFFSETS: [usize; 16] = [0, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34];
    let mut raw = [0u8; 16];
    let mut invalid = 0u8;
    for (dst, &pos) in raw.iter_mut().zip(&OFFSETS) {
        let hi = HEX_LUT[bytes[pos] as usize];
        let lo = HEX_LUT[bytes[pos + 1] as usize];
        invalid |= hi | lo;
        *dst = (hi << 4) | lo;
    }
    if invalid & 0xF0 != 0 {
        return None;
    }
    Some(Uuid::from_bytes(raw))
}

/// Fast allocation-free parser for canonical lowercase hyphenated UUIDs.
#[inline]
pub(crate) fn parse_canonical_uuid(value: &str) -> Option<Uuid> {
    parse_canonical_uuid_bytes(value.as_bytes())
}

pub(crate) fn canonical_uuid(value: &str) -> Result<Uuid> {
    if let Some(id) = parse_canonical_uuid(value) {
        return Ok(id);
    }
    let id = Uuid::parse_str(value).context("invalid rollout UUID")?;
    ensure!(
        id.hyphenated().encode_lower(&mut [0; 36]) == value,
        "noncanonical rollout UUID"
    );
    Ok(id)
}

pub(crate) struct RolloutName {
    pub owner: Uuid,
    pub rollout: Uuid,
    pub compressed: bool,
    pub compact_ts: Option<u64>,
}

#[inline]
fn is_valid_gregorian_date(year: u16, month: u8, day: u8) -> bool {
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    day >= 1 && day <= max_day
}

#[inline]
fn validate_timestamp(timestamp: &str) -> Result<u64> {
    let b = timestamp.as_bytes();
    if b.len() == 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b'-'
        && b[16] == b'-'
        && let (
            Some(y_hi),
            Some(y_lo),
            Some(month),
            Some(day),
            Some(hour),
            Some(minute),
            Some(second),
        ) = (
            digit2_u8(b[0], b[1]),
            digit2_u8(b[2], b[3]),
            digit2_u8(b[5], b[6]),
            digit2_u8(b[8], b[9]),
            digit2_u8(b[11], b[12]),
            digit2_u8(b[14], b[15]),
            digit2_u8(b[17], b[18]),
        )
    {
        let year = u16::from(y_hi) * 100 + u16::from(y_lo);
        if hour <= 23 && minute <= 59 && second <= 59 && is_valid_gregorian_date(year, month, day) {
            return Ok(pack_rollout_ts(
                u64::from(year),
                month,
                day,
                hour,
                minute,
                second,
            ));
        }
    }
    let dt = jiff::civil::DateTime::strptime("%Y-%m-%dT%H-%M-%S", timestamp)
        .context("invalid rollout filename timestamp")?;
    Ok(pack_rollout_ts(
        dt.year() as u64,
        dt.month() as u8,
        dt.day() as u8,
        dt.hour() as u8,
        dt.minute() as u8,
        dt.second() as u8,
    ))
}

/// Ordinary names end in owner UUID; reverted names end in owner_rollout UUIDs.
pub(crate) fn filename(name: &str) -> Result<Option<RolloutName>> {
    if !name.starts_with("rollout-") {
        return Ok(None);
    }
    let (stem, compressed) = if let Some(stem) = name.strip_suffix(".jsonl.zst") {
        (stem, true)
    } else if let Some(stem) = name.strip_suffix(".jsonl") {
        (stem, false)
    } else {
        return Ok(None);
    };
    let core = stem
        .strip_prefix("rollout-")
        .context("invalid rollout filename")?;
    let timestamp = core.get(..19).context("rollout filename lacks timestamp")?;
    let ts = validate_timestamp(timestamp)?;
    ensure!(
        core.get(19..20) == Some("-"),
        "rollout filename lacks UUID separator"
    );
    let ids = core.get(20..).context("rollout filename lacks UUID")?;
    let (owner, rollout, compact_ts) = match ids.split_once('_') {
        Some((owner, rollout)) => (canonical_uuid(owner)?, canonical_uuid(rollout)?, None),
        None => {
            let id = canonical_uuid(ids)?;
            (id, id, Some(ts))
        }
    };
    Ok(Some(RolloutName {
        owner,
        rollout,
        compressed,
        compact_ts,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedHeader {
    owner: Uuid,
    paginated: bool,
    base: Option<Uuid>,
    parent: Option<Uuid>,
}

// Single-pass selective JSON validation without allocating serde_json::Value.
// Every ignored value still receives complete UTF-8, escape, number-range, and
// recursion-limit validation while duplicate known keys are rejected.

#[derive(Clone, Copy)]
struct Discard;

impl<'de> DeserializeSeed<'de> for Discard {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

struct DiscardKey;

impl<'de> DeserializeSeed<'de> for DiscardKey {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl Visitor<'_> for DiscardKey {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object key")
    }

    fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
}

impl<'de> Visitor<'de> for Discard {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq.next_element_seed(Discard)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        while map.next_key_seed(DiscardKey)?.is_some() {
            map.next_value_seed(Discard)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ParsedUuid {
    Valid(Uuid),
    Invalid,
}

impl ParsedUuid {
    fn from_str(value: &str) -> Self {
        match parse_canonical_uuid(value) {
            Some(id) => Self::Valid(id),
            None => Self::Invalid,
        }
    }

    fn into_result(self) -> Result<Uuid> {
        match self {
            Self::Valid(id) => Ok(id),
            Self::Invalid => bail!("invalid or noncanonical rollout UUID"),
        }
    }
}

struct StrUuidSeed;

impl<'de> DeserializeSeed<'de> for StrUuidSeed {
    type Value = ParsedUuid;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = ParsedUuid;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a UUID string")
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(ParsedUuid::from_str(value))
            }
        }
        deserializer.deserialize_str(V)
    }
}

struct OptStrUuidSeed;

impl<'de> DeserializeSeed<'de> for OptStrUuidSeed {
    type Value = Option<ParsedUuid>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Option<ParsedUuid>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a UUID string or null")
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(Some(ParsedUuid::from_str(value)))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[derive(Clone, Copy)]
enum HistoryMode {
    AbsentOrNull,
    Legacy,
    Paginated,
    Unsupported,
}

struct OptHistoryModeSeed;

impl<'de> DeserializeSeed<'de> for OptHistoryModeSeed {
    type Value = HistoryMode;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = HistoryMode;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a history_mode string or null")
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(HistoryMode::AbsentOrNull)
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(match value {
                    "legacy" => HistoryMode::Legacy,
                    "paginated" => HistoryMode::Paginated,
                    _ => HistoryMode::Unsupported,
                })
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct OptBaseSeed;

impl<'de> DeserializeSeed<'de> for OptBaseSeed {
    type Value = Option<ParsedUuid>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        enum BaseKey {
            ThreadId,
            EndOrdinal,
            EndByte,
            Other,
        }
        impl<'de> Deserialize<'de> for BaseKey {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                struct K;
                impl Visitor<'_> for K {
                    type Value = BaseKey;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a history_base key")
                    }
                    fn visit_str<E>(self, key: &str) -> std::result::Result<Self::Value, E> {
                        Ok(match key {
                            "thread_id" => BaseKey::ThreadId,
                            "end_ordinal_exclusive" => BaseKey::EndOrdinal,
                            "end_byte_offset" => BaseKey::EndByte,
                            _ => BaseKey::Other,
                        })
                    }
                }
                d.deserialize_str(K)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<ParsedUuid>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("history_base object or null")
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut thread_id: Option<ParsedUuid> = None;
                let mut seen_ordinal = false;
                let mut seen_byte = false;
                while let Some(key) = map.next_key::<BaseKey>()? {
                    match key {
                        BaseKey::ThreadId => {
                            if thread_id.is_some() {
                                return Err(de::Error::duplicate_field("thread_id"));
                            }
                            thread_id = Some(map.next_value_seed(StrUuidSeed)?);
                        }
                        BaseKey::EndOrdinal => {
                            if seen_ordinal {
                                return Err(de::Error::duplicate_field("end_ordinal_exclusive"));
                            }
                            let _: u64 = map.next_value()?;
                            seen_ordinal = true;
                        }
                        BaseKey::EndByte => {
                            if seen_byte {
                                return Err(de::Error::duplicate_field("end_byte_offset"));
                            }
                            let _: u64 = map.next_value()?;
                            seen_byte = true;
                        }
                        BaseKey::Other => {
                            map.next_value_seed(Discard)?;
                        }
                    }
                }
                let thread_id = thread_id.ok_or_else(|| de::Error::missing_field("thread_id"))?;
                if !seen_ordinal {
                    return Err(de::Error::missing_field("end_ordinal_exclusive"));
                }
                if !seen_byte {
                    return Err(de::Error::missing_field("end_byte_offset"));
                }
                Ok(Some(thread_id))
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct SpawnSeed;

impl<'de> DeserializeSeed<'de> for SpawnSeed {
    type Value = ParsedUuid;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        enum SpawnKey {
            Parent,
            Other,
        }
        impl<'de> Deserialize<'de> for SpawnKey {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                struct K;
                impl Visitor<'_> for K {
                    type Value = SpawnKey;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a thread_spawn key")
                    }
                    fn visit_str<E>(self, key: &str) -> std::result::Result<Self::Value, E> {
                        Ok(if key == "parent_thread_id" {
                            SpawnKey::Parent
                        } else {
                            SpawnKey::Other
                        })
                    }
                }
                d.deserialize_str(K)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ParsedUuid;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("thread_spawn source must be an object")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut parent: Option<ParsedUuid> = None;
                while let Some(key) = map.next_key::<SpawnKey>()? {
                    match key {
                        SpawnKey::Parent => {
                            if parent.is_some() {
                                return Err(de::Error::duplicate_field("parent_thread_id"));
                            }
                            parent = Some(map.next_value_seed(StrUuidSeed)?);
                        }
                        SpawnKey::Other => {
                            map.next_value_seed(Discard)?;
                        }
                    }
                }
                parent.ok_or_else(|| de::Error::missing_field("parent_thread_id"))
            }
        }
        // Explicitly requires an object (rejects null as well as primitives/arrays).
        deserializer.deserialize_map(V)
    }
}

struct OptSubagentSeed;

impl<'de> DeserializeSeed<'de> for OptSubagentSeed {
    type Value = Option<ParsedUuid>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        enum SubagentKey {
            ThreadSpawn,
            Other,
        }
        impl<'de> Deserialize<'de> for SubagentKey {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                struct K;
                impl Visitor<'_> for K {
                    type Value = SubagentKey;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a subagent key")
                    }
                    fn visit_str<E>(self, key: &str) -> std::result::Result<Self::Value, E> {
                        Ok(if key == "thread_spawn" {
                            SubagentKey::ThreadSpawn
                        } else {
                            SubagentKey::Other
                        })
                    }
                }
                d.deserialize_str(K)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<ParsedUuid>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a subagent string, object, or null")
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut spawn: Option<ParsedUuid> = None;
                let mut seen_spawn = false;
                while let Some(key) = map.next_key::<SubagentKey>()? {
                    match key {
                        SubagentKey::ThreadSpawn => {
                            if seen_spawn {
                                return Err(de::Error::duplicate_field("thread_spawn"));
                            }
                            seen_spawn = true;
                            spawn = Some(map.next_value_seed(SpawnSeed)?);
                        }
                        SubagentKey::Other => {
                            map.next_value_seed(Discard)?;
                        }
                    }
                }
                Ok(spawn)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct OptSourceSeed;

impl<'de> DeserializeSeed<'de> for OptSourceSeed {
    type Value = Option<ParsedUuid>;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        enum SourceKey {
            Subagent,
            Other,
        }
        impl<'de> Deserialize<'de> for SourceKey {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                struct K;
                impl Visitor<'_> for K {
                    type Value = SourceKey;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a source key")
                    }
                    fn visit_str<E>(self, key: &str) -> std::result::Result<Self::Value, E> {
                        Ok(if key == "subagent" {
                            SourceKey::Subagent
                        } else {
                            SourceKey::Other
                        })
                    }
                }
                d.deserialize_str(K)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Option<ParsedUuid>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a source string, object, or null")
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut parent: Option<ParsedUuid> = None;
                let mut seen_subagent = false;
                while let Some(key) = map.next_key::<SourceKey>()? {
                    match key {
                        SourceKey::Subagent => {
                            if seen_subagent {
                                return Err(de::Error::duplicate_field("subagent"));
                            }
                            seen_subagent = true;
                            parent = map.next_value_seed(OptSubagentSeed)?;
                        }
                        SourceKey::Other => {
                            map.next_value_seed(Discard)?;
                        }
                    }
                }
                Ok(parent)
            }
        }
        deserializer.deserialize_any(V)
    }
}

struct RawPayload {
    id: ParsedUuid,
    history_mode: HistoryMode,
    history_base: Option<ParsedUuid>,
    explicit_parent: Option<ParsedUuid>,
    source_parent: Option<ParsedUuid>,
}

struct PayloadSeed;

impl<'de> DeserializeSeed<'de> for PayloadSeed {
    type Value = RawPayload;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        enum PayloadKey {
            Id,
            HistoryMode,
            HistoryBase,
            ParentThreadId,
            Source,
            Other,
        }
        impl<'de> Deserialize<'de> for PayloadKey {
            fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                struct K;
                impl Visitor<'_> for K {
                    type Value = PayloadKey;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a payload key")
                    }
                    fn visit_str<E>(self, key: &str) -> std::result::Result<Self::Value, E> {
                        Ok(match key {
                            "id" => PayloadKey::Id,
                            "history_mode" => PayloadKey::HistoryMode,
                            "history_base" => PayloadKey::HistoryBase,
                            "parent_thread_id" => PayloadKey::ParentThreadId,
                            "source" => PayloadKey::Source,
                            _ => PayloadKey::Other,
                        })
                    }
                }
                d.deserialize_str(K)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = RawPayload;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("rollout header and payload must be objects")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut id: Option<ParsedUuid> = None;
                let mut history_mode: Option<HistoryMode> = None;
                let mut history_base: Option<Option<ParsedUuid>> = None;
                let mut explicit_parent: Option<Option<ParsedUuid>> = None;
                let mut source_parent: Option<Option<ParsedUuid>> = None;
                while let Some(key) = map.next_key::<PayloadKey>()? {
                    match key {
                        PayloadKey::Id => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value_seed(StrUuidSeed)?);
                        }
                        PayloadKey::HistoryMode => {
                            if history_mode.is_some() {
                                return Err(de::Error::duplicate_field("history_mode"));
                            }
                            history_mode = Some(map.next_value_seed(OptHistoryModeSeed)?);
                        }
                        PayloadKey::HistoryBase => {
                            if history_base.is_some() {
                                return Err(de::Error::duplicate_field("history_base"));
                            }
                            history_base = Some(map.next_value_seed(OptBaseSeed)?);
                        }
                        PayloadKey::ParentThreadId => {
                            if explicit_parent.is_some() {
                                return Err(de::Error::duplicate_field("parent_thread_id"));
                            }
                            explicit_parent = Some(map.next_value_seed(OptStrUuidSeed)?);
                        }
                        PayloadKey::Source => {
                            if source_parent.is_some() {
                                return Err(de::Error::duplicate_field("source"));
                            }
                            source_parent = Some(map.next_value_seed(OptSourceSeed)?);
                        }
                        PayloadKey::Other => {
                            map.next_value_seed(Discard)?;
                        }
                    }
                }
                Ok(RawPayload {
                    id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                    history_mode: history_mode.unwrap_or(HistoryMode::AbsentOrNull),
                    history_base: history_base.flatten(),
                    explicit_parent: explicit_parent.flatten(),
                    source_parent: source_parent.flatten(),
                })
            }
        }
        deserializer.deserialize_map(V)
    }
}

struct SessionMetaKindSeed;

impl<'de> DeserializeSeed<'de> for SessionMetaKindSeed {
    type Value = bool;

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = bool;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a record type string")
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(value == "session_meta")
            }
        }
        deserializer.deserialize_str(V)
    }
}

enum RootKey {
    Type,
    Payload,
    Other,
}

impl<'de> Deserialize<'de> for RootKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct K;
        impl Visitor<'_> for K {
            type Value = RootKey;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a header key")
            }
            fn visit_str<E>(self, key: &str) -> std::result::Result<Self::Value, E> {
                Ok(match key {
                    "type" => RootKey::Type,
                    "payload" => RootKey::Payload,
                    _ => RootKey::Other,
                })
            }
        }
        d.deserialize_str(K)
    }
}

fn header(line: &[u8]) -> Result<ParsedHeader> {
    struct RootVisitor;
    impl<'de> Visitor<'de> for RootVisitor {
        type Value = (bool, RawPayload);
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("rollout header and payload must be objects")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut is_session_meta: Option<bool> = None;
            let mut payload: Option<RawPayload> = None;
            while let Some(key) = map.next_key::<RootKey>()? {
                match key {
                    RootKey::Type => {
                        if is_session_meta.is_some() {
                            return Err(de::Error::duplicate_field("type"));
                        }
                        is_session_meta = Some(map.next_value_seed(SessionMetaKindSeed)?);
                    }
                    RootKey::Payload => {
                        if payload.is_some() {
                            return Err(de::Error::duplicate_field("payload"));
                        }
                        payload = Some(map.next_value_seed(PayloadSeed)?);
                    }
                    RootKey::Other => {
                        map.next_value_seed(Discard)?;
                    }
                }
            }
            let is_session_meta =
                is_session_meta.ok_or_else(|| de::Error::missing_field("type"))?;
            let payload = payload.ok_or_else(|| de::Error::missing_field("payload"))?;
            Ok((is_session_meta, payload))
        }
    }

    let (is_session_meta, payload) = if let Ok(text) = simdutf8::basic::from_utf8(line) {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let parsed = deserializer.deserialize_map(RootVisitor)?;
        deserializer.end()?;
        parsed
    } else {
        let mut deserializer = serde_json::Deserializer::from_slice(line);
        let parsed = deserializer.deserialize_map(RootVisitor)?;
        deserializer.end()?;
        parsed
    };

    ensure!(is_session_meta, "first record is not session_meta");
    let owner = payload.id.into_result()?;
    let paginated = match payload.history_mode {
        HistoryMode::AbsentOrNull | HistoryMode::Legacy => false,
        HistoryMode::Paginated => true,
        HistoryMode::Unsupported => bail!("unsupported rollout history mode"),
    };
    let base = payload
        .history_base
        .map(ParsedUuid::into_result)
        .transpose()?;
    ensure!(
        paginated || base.is_none(),
        "legacy rollout contains a history reference"
    );
    let explicit_parent = payload
        .explicit_parent
        .map(ParsedUuid::into_result)
        .transpose()?;
    let source_parent = payload
        .source_parent
        .map(ParsedUuid::into_result)
        .transpose()?;
    ensure!(
        explicit_parent.is_none() || source_parent.is_none() || explicit_parent == source_parent,
        "conflicting recorded parent identities"
    );
    Ok(ParsedHeader {
        owner,
        paginated,
        base,
        parent: explicit_parent.or(source_parent),
    })
}

/// Journal schema 3 cannot infer the stable owner from the filename UUID.
pub(crate) fn validate_owner(line: &[u8], expected: Uuid) -> Result<()> {
    // A committed journal already authorized removal. Recovery binds identity,
    // rather than reinterpreting optional metadata under a newer adapter policy.
    struct OwnerPayloadSeed;
    impl<'de> DeserializeSeed<'de> for OwnerPayloadSeed {
        type Value = ParsedUuid;
        fn deserialize<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error> {
            enum Key {
                Id,
                Other,
            }
            impl<'de> Deserialize<'de> for Key {
                fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                    struct K;
                    impl Visitor<'_> for K {
                        type Value = Key;
                        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                            f.write_str("a payload key")
                        }
                        fn visit_str<E>(self, key: &str) -> std::result::Result<Self::Value, E> {
                            Ok(if key == "id" { Key::Id } else { Key::Other })
                        }
                    }
                    d.deserialize_str(K)
                }
            }
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = ParsedUuid;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a payload object")
                }
                fn visit_map<A: MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<Self::Value, A::Error> {
                    let mut id: Option<ParsedUuid> = None;
                    while let Some(key) = map.next_key::<Key>()? {
                        match key {
                            Key::Id => {
                                if id.is_some() {
                                    return Err(de::Error::duplicate_field("id"));
                                }
                                id = Some(map.next_value_seed(StrUuidSeed)?);
                            }
                            Key::Other => {
                                map.next_value_seed(Discard)?;
                            }
                        }
                    }
                    id.ok_or_else(|| de::Error::missing_field("id"))
                }
            }
            deserializer.deserialize_map(V)
        }
    }

    struct OwnerRootVisitor;
    impl<'de> Visitor<'de> for OwnerRootVisitor {
        type Value = (bool, ParsedUuid);
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a rollout header object")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut is_session_meta: Option<bool> = None;
            let mut id: Option<ParsedUuid> = None;
            while let Some(key) = map.next_key::<RootKey>()? {
                match key {
                    RootKey::Type => {
                        if is_session_meta.is_some() {
                            return Err(de::Error::duplicate_field("type"));
                        }
                        is_session_meta = Some(map.next_value_seed(SessionMetaKindSeed)?);
                    }
                    RootKey::Payload => {
                        if id.is_some() {
                            return Err(de::Error::duplicate_field("payload"));
                        }
                        id = Some(map.next_value_seed(OwnerPayloadSeed)?);
                    }
                    RootKey::Other => {
                        map.next_value_seed(Discard)?;
                    }
                }
            }
            let is_session_meta =
                is_session_meta.ok_or_else(|| de::Error::missing_field("type"))?;
            let id = id.ok_or_else(|| de::Error::missing_field("payload"))?;
            Ok((is_session_meta, id))
        }
    }

    let (is_session_meta, id) = if let Ok(text) = simdutf8::basic::from_utf8(line) {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let parsed = deserializer.deserialize_map(OwnerRootVisitor)?;
        deserializer.end()?;
        parsed
    } else {
        let mut deserializer = serde_json::Deserializer::from_slice(line);
        let parsed = deserializer.deserialize_map(OwnerRootVisitor)?;
        deserializer.end()?;
        parsed
    };

    ensure!(is_session_meta, "staged header is not session_meta");
    let owner = id.into_result()?;
    ensure!(
        owner == expected,
        "staged rollout belongs to a different thread"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    const OWNER: Uuid = Uuid::from_u128(1);
    const CHILD: Uuid = Uuid::from_u128(2);
    const OLD: Uuid = Uuid::from_u128(3);

    fn write(
        home: &Path,
        directory: &str,
        rollout: Uuid,
        owner: Uuid,
        base: Option<Uuid>,
        compressed: bool,
    ) -> PathBuf {
        let directory = home.join(directory);
        fs::create_dir_all(&directory).unwrap();
        let ids = if owner == rollout {
            owner.to_string()
        } else {
            format!("{owner}_{rollout}")
        };
        let path = directory.join(format!(
            "rollout-2026-09-10T00-00-00-{ids}.jsonl{}",
            if compressed { ".zst" } else { "" }
        ));
        let value = json!({"type":"session_meta", "payload": {
            "id":owner, "history_mode":"paginated", "history_base":base.map(|id| json!({
                "thread_id":id,"end_ordinal_exclusive":10,"end_byte_offset":100
            }))
        }});
        let bytes = format!("{value}\nthis transcript is deliberately not JSON\n").into_bytes();
        fs::write(
            &path,
            if compressed {
                zstd::stream::encode_all(bytes.as_slice(), 0).unwrap()
            } else {
                bytes
            },
        )
        .unwrap();
        path
    }

    #[test]
    fn protects_sources_from_active_archived_and_compressed_orphan_rollouts() {
        for (directory, compressed) in [
            ("sessions/2026/09/10", false),
            ("archived_sessions", false),
            ("sessions/2026/09/10", true),
        ] {
            let home = tempfile::tempdir().unwrap();
            write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            write(
                home.path(),
                directory,
                CHILD,
                CHILD,
                Some(OWNER),
                compressed,
            );
            // No database exists: an orphan rollout still carries a real edge.
            let index = scan(home.path(), &HashSet::from([OWNER]), None).unwrap();
            assert!(index.externally_referenced(OWNER, OWNER));
            assert_eq!(index.owned(OWNER).len(), 1);
            assert!(index.owned(OWNER)[0].paginated);
            assert!(index.owned(CHILD).is_empty());
        }
    }

    #[test]
    fn recent_stamps_cannot_authorize_header_reuse() {
        let home = tempfile::tempdir().unwrap();
        let child = write(home.path(), "sessions", CHILD, CHILD, Some(OLD), false);
        let candidates = HashSet::from([OWNER]);
        // Pin the observed cutoff so this test remains deterministic even if
        // the runner crosses a second boundary between the two scans. Both
        // stamps are recent relative to this boundary, regardless of whether
        // this filesystem actually coalesces the following writes.
        let mut cache = HeaderCache {
            stable_before: Some(fs::metadata(&child).unwrap().ctime()),
            ..HeaderCache::default()
        };
        let first = scan_inventory(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert!(!first.externally_referenced(OWNER, OWNER));
        assert!(cache.entries.is_empty());
        let before = fs::metadata(&child).unwrap();
        write(home.path(), "sessions", CHILD, CHILD, Some(OWNER), false);
        assert_eq!(fs::metadata(&child).unwrap().len(), before.len());
        let second = scan_inventory(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert!(second.externally_referenced(OWNER, OWNER));
        assert!(cache.entries.is_empty());
        assert_eq!(cache.reads, 2);

        // Unknown clock state must also keep every read live.
        cache.stable_before = None;
        scan_inventory(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert!(cache.entries.is_empty());
        assert_eq!(cache.reads, 3);
    }

    #[test]
    fn cached_scans_discover_new_copies_and_forget_removed_references() {
        for compressed in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let source = write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            let candidates = HashSet::from([OWNER]);
            let mut cache = HeaderCache::after_files_settle(&[&source]);
            scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert_eq!(cache.reads, 1, "unchanged headers must not be reopened");

            let child = write(
                home.path(),
                "sessions/nested",
                CHILD,
                CHILD,
                Some(OWNER),
                compressed,
            );
            let copy = home
                .path()
                .join("sessions")
                .join(source.file_name().unwrap());
            fs::copy(&source, &copy).unwrap();
            let index = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert!(index.externally_referenced(OWNER, OWNER));
            assert_eq!(index.owned(OWNER).len(), 2);
            assert_eq!(cache.reads, 3);

            fs::remove_file(child).unwrap();
            fs::remove_file(copy).unwrap();
            let index = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert!(!index.externally_referenced(OWNER, OWNER));
            assert_eq!(index.owned(OWNER).len(), 1);
            assert_eq!(cache.entries.len(), 1);
            assert_eq!(cache.reads, 3);
        }
    }

    #[test]
    fn cached_header_edits_cannot_hide_new_dependencies() {
        use std::io::Write;
        for change in [
            "same-size",
            "restored-mtime",
            "replace",
            "append",
            "compressed",
        ] {
            let home = tempfile::tempdir().unwrap();
            let compressed = change == "compressed";
            let source = write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            let child = write(home.path(), "sessions", CHILD, CHILD, Some(OLD), compressed);
            let candidates = HashSet::from([OWNER]);
            let mut cache = HeaderCache::after_files_settle(&[&source, &child]);
            let initial = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert!(!initial.externally_referenced(OWNER, OWNER));
            let before = fs::metadata(&child).unwrap();
            if change == "append" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&child)
                    .unwrap()
                    .write_all(b"another record\n")
                    .unwrap();
            } else {
                if change == "replace" {
                    fs::rename(&child, home.path().join("old-inode")).unwrap();
                }
                write(
                    home.path(),
                    "sessions",
                    CHILD,
                    CHILD,
                    Some(OWNER),
                    compressed,
                );
                if change == "restored-mtime" {
                    fs::File::options()
                        .write(true)
                        .open(&child)
                        .unwrap()
                        .set_modified(before.modified().unwrap())
                        .unwrap();
                }
            }
            if matches!(change, "same-size" | "restored-mtime" | "replace") {
                assert_eq!(fs::metadata(&child).unwrap().len(), before.len());
            }
            let index = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            assert_eq!(
                index.externally_referenced(OWNER, OWNER),
                change != "append",
                "{change}"
            );
            assert_eq!(
                cache.reads, 3,
                "changed header must be read again: {change}"
            );
        }
    }

    #[test]
    fn cache_hits_do_not_bypass_filesystem_or_header_failures() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for change in [
            "symlink",
            "hardlink",
            "permissions",
            "malformed",
            "pending",
            "root-symlink",
        ] {
            let home = tempfile::tempdir().unwrap();
            let source = write(home.path(), "sessions", OWNER, OWNER, None, false);
            let candidates = HashSet::from([OWNER]);
            let mut cache = HeaderCache::after_files_settle(&[&source]);
            scan(home.path(), &candidates, Some(&mut cache)).unwrap();
            match change {
                "symlink" => {
                    let saved = home.path().join("saved");
                    fs::rename(&source, &saved).unwrap();
                    symlink(saved, &source).unwrap();
                }
                "hardlink" => fs::hard_link(&source, home.path().join("extra-link")).unwrap(),
                "permissions" => {
                    fs::set_permissions(&source, fs::Permissions::from_mode(0o000)).unwrap()
                }
                "malformed" => fs::write(&source, b"invalid header\n").unwrap(),
                "pending" => {
                    fs::write(home.path().join("sessions/.codex-retain-pending-test"), b"").unwrap()
                }
                "root-symlink" => {
                    let saved = home.path().join("saved-root");
                    fs::rename(home.path().join("sessions"), &saved).unwrap();
                    symlink(saved, home.path().join("sessions")).unwrap();
                }
                _ => unreachable!(),
            }
            let result = scan(home.path(), &candidates, Some(&mut cache));
            if change == "permissions" {
                fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
                // A privileged test runner may still be able to read mode 000.
                if result.is_ok() {
                    assert_eq!(cache.reads, 2);
                    continue;
                }
            }
            assert!(result.is_err(), "{change}");
            assert!(
                cache.entries.is_empty(),
                "failed scans discard cached headers"
            );
        }
    }

    #[test]
    fn owns_reverted_rollouts_without_treating_internal_edges_as_external() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), "archived_sessions", OLD, OWNER, None, false);
        write(
            home.path(),
            "archived_sessions",
            OWNER,
            OWNER,
            Some(OLD),
            false,
        );
        let candidates = HashSet::from([OWNER]);
        let index = scan(home.path(), &candidates, None).unwrap();
        assert_eq!(index.owned(OWNER).len(), 2);
        assert!(!index.externally_referenced(OLD, OWNER));
        assert_eq!(index.owned(OWNER)[0].rollout_id, OWNER);
        write(home.path(), "sessions", CHILD, CHILD, Some(OLD), false);
        let index = scan(home.path(), &candidates, None).unwrap();
        assert!(index.externally_referenced(OLD, OWNER));
        assert!(!index.externally_referenced(OWNER, OWNER));
    }

    #[test]
    fn retains_same_owner_copies_across_locations_and_encodings() {
        for (directory, compressed) in [("sessions", false), ("archived_sessions", true)] {
            let home = tempfile::tempdir().unwrap();
            write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            write(home.path(), directory, OWNER, OWNER, None, compressed);
            assert_eq!(
                scan(home.path(), &HashSet::from([OWNER]), None)
                    .unwrap()
                    .owned(OWNER)
                    .len(),
                2
            );
        }
    }

    #[test]
    fn rejects_malformed_unrelated_headers() {
        for invalid in [
            "not json\n",
            "{}\n",
            "{\"type\":\"session_meta\",\"payload\":null}\n",
        ] {
            let home = tempfile::tempdir().unwrap();
            let path = write(home.path(), "sessions", CHILD, CHILD, None, false);
            fs::write(path, invalid).unwrap();
            assert!(scan(home.path(), &HashSet::from([OWNER]), None).is_err());
        }
    }

    #[test]
    fn validates_modes_reference_fields_and_duplicate_keys() {
        for payload in [
            json!({"id":OWNER,"history_mode":"future"}),
            json!({"id":OWNER,"history_mode":"paginated","history_base":{}}),
            json!({"id":OWNER,"history_mode":"paginated","history_base":[OLD,1,10]}),
            json!({"id":OWNER,"history_mode":"paginated","history_base":{"thread_id":OLD,"end_ordinal_exclusive":-1,"end_byte_offset":10}}),
            json!({"id":OWNER,"history_base":{"thread_id":OLD,"end_ordinal_exclusive":1,"end_byte_offset":10}}),
            json!({"id":OWNER.to_string().replace('-', "")}),
        ] {
            assert!(
                header(
                    json!({"type":"session_meta","payload":payload})
                        .to_string()
                        .as_bytes()
                )
                .is_err()
            );
        }
        let duplicate = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_base":null,"history_base":null}}}}"#
        );
        assert!(header(duplicate.as_bytes()).is_err());
        let legacy = json!({"type":"session_meta","payload":{"id":OWNER}});
        assert_eq!(
            header(legacy.to_string().as_bytes()).unwrap(),
            ParsedHeader {
                owner: OWNER,
                paginated: false,
                base: None,
                parent: None
            }
        );
    }

    #[test]
    fn parses_native_reverted_names_and_checks_the_filename_owner() {
        for suffix in [".jsonl", ".jsonl.zst"] {
            let name = format!("rollout-2026-09-10T00-00-00-{OWNER}_{OLD}{suffix}");
            let parsed = filename(&name).unwrap().unwrap();
            assert_eq!(parsed.owner, OWNER);
            assert_eq!(parsed.rollout, OLD);
            assert_eq!(parsed.compressed, suffix.ends_with(".zst"));
        }
        let home = tempfile::tempdir().unwrap();
        let original = write(home.path(), "archived_sessions", OLD, OWNER, None, false);
        let wrong =
            original.with_file_name(format!("rollout-2026-09-10T00-00-00-{CHILD}_{OLD}.jsonl"));
        fs::rename(original, wrong).unwrap();
        assert!(scan(home.path(), &HashSet::from([OWNER]), None).is_err());
        for name in [
            format!("rollout-2026-02-31T00-00-00-{OWNER}.jsonl"),
            format!("rollout-2026-09-10T00-00-00-{OWNER}_{OLD}_{CHILD}.jsonl"),
        ] {
            assert!(filename(&name).is_err());
        }
    }

    #[test]
    fn rejects_symlinks_hardlinks_and_pending_artifacts() {
        for kind in ["symlink", "hardlink", "pending"] {
            let home = tempfile::tempdir().unwrap();
            let original = write(home.path(), "sessions", OWNER, OWNER, None, false);
            match kind {
                "symlink" => {
                    std::os::unix::fs::symlink(&original, home.path().join("sessions/link"))
                        .unwrap()
                }
                "hardlink" => fs::hard_link(&original, home.path().join("elsewhere")).unwrap(),
                _ => {
                    fs::write(home.path().join("sessions/.codex-retain-pending-test"), b"").unwrap()
                }
            }
            assert!(scan(home.path(), &HashSet::from([OWNER]), None).is_err());
        }
        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(home.path().join("missing"), home.path().join("sessions"))
            .unwrap();
        assert!(scan(home.path(), &HashSet::new(), None).is_err());
    }

    #[test]
    fn organizational_metadata_survives_missing_sql_edges() {
        for source_only in [false, true] {
            let home = tempfile::tempdir().unwrap();
            write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
            let child = write(home.path(), "sessions", CHILD, CHILD, None, false);
            let parent = if source_only {
                json!({"source":{"subagent":{"thread_spawn":{"parent_thread_id":OWNER,"depth":1}}}})
            } else {
                json!({"parent_thread_id":OWNER})
            };
            let mut payload = json!({"id":CHILD,"history_mode":"paginated"});
            for (key, value) in parent.as_object().unwrap() {
                payload[key] = value.clone();
            }
            fs::write(
                &child,
                format!("{}\n", json!({"type":"session_meta","payload":payload})),
            )
            .unwrap();
            let index = scan(home.path(), &HashSet::from([OWNER]), None).unwrap();
            let mut graph = crate::relations::Dependencies::default();
            index.add_dependencies(&mut graph);
            assert_eq!(
                graph.plan(&HashSet::from([OWNER])).blocked[&OWNER],
                crate::relations::Blocker::Children
            );
        }
    }

    #[test]
    fn contradictory_or_duplicate_parent_metadata_cannot_hide_an_owner() {
        let invalid = json!({"type":"session_meta","payload":{"id":OWNER,"parent_thread_id":CHILD,
            "source":{"subagent":{"thread_spawn":{"parent_thread_id":OLD,"depth":1}}}}});
        assert!(header(invalid.to_string().as_bytes()).is_err());
        let duplicate = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{CHILD}","parent_thread_id":"{OLD}"}}}}}}}}}}"#
        );
        assert!(header(duplicate.as_bytes()).is_err());
        // Recovery checks identity, not new optional-metadata admission rules.
        validate_owner(invalid.to_string().as_bytes(), OWNER).unwrap();
    }

    #[test]
    fn single_pass_header_and_owner_parsers_match_value_plus_struct_validation() {
        #[derive(Deserialize)]
        struct RefHeader {
            #[serde(rename = "type")]
            kind: String,
            payload: RefPayload,
        }
        #[derive(Deserialize)]
        struct RefPayload {
            id: String,
            history_mode: Option<String>,
            history_base: Option<RefBase>,
            parent_thread_id: Option<String>,
            source: Option<RefSource>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RefSource {
            #[allow(dead_code)]
            Name(String),
            Object(RefSourceFields),
        }
        #[derive(Deserialize)]
        struct RefSourceFields {
            subagent: Option<RefSubagent>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RefSubagent {
            #[allow(dead_code)]
            Name(String),
            Object(RefSubagentFields),
        }
        #[derive(Deserialize)]
        struct RefSubagentFields {
            thread_spawn: Option<RefSpawn>,
        }
        #[derive(Deserialize)]
        struct RefSpawn {
            parent_thread_id: String,
        }
        #[derive(Deserialize)]
        struct RefBase {
            thread_id: String,
            end_ordinal_exclusive: u64,
            end_byte_offset: u64,
        }
        fn ref_header(line: &[u8]) -> Result<ParsedHeader> {
            let value: Value = serde_json::from_slice(line)?;
            ensure!(
                value.is_object() && value["payload"].is_object(),
                "rollout header and payload must be objects"
            );
            let base = &value["payload"]["history_base"];
            ensure!(
                base.is_null() || base.is_object(),
                "history_base must be an object or null"
            );
            if let Some(spawn) = value.pointer("/payload/source/subagent/thread_spawn") {
                ensure!(spawn.is_object(), "thread_spawn source must be an object");
            }
            let record: RefHeader = serde_json::from_slice(line)?;
            ensure!(record.kind == "session_meta", "not session_meta");
            let owner = canonical_uuid(&record.payload.id)?;
            let paginated = match record.payload.history_mode.as_deref() {
                None | Some("legacy") => false,
                Some("paginated") => true,
                Some(_) => bail!("unsupported"),
            };
            let base = record
                .payload
                .history_base
                .map(|b| {
                    let _ = (b.end_ordinal_exclusive, b.end_byte_offset);
                    canonical_uuid(&b.thread_id)
                })
                .transpose()?;
            ensure!(paginated || base.is_none(), "legacy with base");
            let explicit_parent = record
                .payload
                .parent_thread_id
                .as_deref()
                .map(canonical_uuid)
                .transpose()?;
            let source_parent = match record.payload.source {
                Some(RefSource::Object(RefSourceFields {
                    subagent:
                        Some(RefSubagent::Object(RefSubagentFields {
                            thread_spawn: Some(spawn),
                        })),
                })) => Some(canonical_uuid(&spawn.parent_thread_id)?),
                _ => None,
            };
            ensure!(
                explicit_parent.is_none()
                    || source_parent.is_none()
                    || explicit_parent == source_parent,
                "conflict"
            );
            Ok(ParsedHeader {
                owner,
                paginated,
                base,
                parent: explicit_parent.or(source_parent),
            })
        }

        let cases = [
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}}}}"#),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_mode":"legacy","source":"cli","unused":[1,2,{{"x":"y"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_mode":"paginated","history_base":{{"thread_id":"{OLD}","end_ordinal_exclusive":0,"end_byte_offset":1}},"parent_thread_id":"{CHILD}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{CHILD}","depth":1}}}}}}}}}}"#
            ),
            format!(
                r#"{{"t\u0079pe":"session_\u006deta","pay\u006coad":{{"i\u0064":"{OWNER}","source":{{"subagent":"review"}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"thread_spawn":null}}}}}}}}"#
            ),
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":123}}}}"#),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":[]}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}},"type":"session_meta"}}"#
            ),
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","id":"{OWNER}"}}}}"#),
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}},"ignored":1e9999}}"#),
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}}}} trailing"#),
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}},"dup":1,"dup":2}}"#),
            // Duplicate keys at every nested level and escaped key variants.
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}},"payload":{{"id":"{OWNER}"}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_mode":"legacy","history_mode":"legacy"}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_mode":null,"history_mode":null}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_base":null,"history_base":null}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","parent_thread_id":null,"parent_thread_id":null}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":null,"source":null}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_mode":"paginated","history_base":{{"thread_id":"{OLD}","thread_id":"{OLD}","end_ordinal_exclusive":0,"end_byte_offset":0}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_mode":"paginated","history_base":{{"thread_id":"{OLD}","end_ordinal_exclusive":0,"end_ordinal_exclusive":0,"end_byte_offset":0}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","history_mode":"paginated","history_base":{{"thread_id":"{OLD}","end_ordinal_exclusive":0,"end_byte_offset":0,"end_byte_offset":0}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":"review","subagent":"review"}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{CHILD}"}},"thread_spawn":{{"parent_thread_id":"{CHILD}"}}}}}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{CHILD}","parent_thread_id":"{CHILD}"}}}}}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"\u0074hread_spawn":null}}}}}}}}"#
            ),
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{}}}}}}"#),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":null}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{}}}}}}}}"#
            ),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"thread_spawn":{{}}}}}}}}}}"#
            ),
            // Lone surrogate and overflowing float inside deeply nested ignored fields.
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","extra":"\uD800"}}}}"#),
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{OWNER}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{CHILD}","extra":1e999}}}}}}}}}}"#
            ),
            "null".into(),
            "[]".into(),
        ];
        for case in &cases {
            assert_eq!(
                header(case.as_bytes()).ok(),
                ref_header(case.as_bytes()).ok(),
                "mismatch on {case}"
            );
        }
        // Verify JSON recursion limit matches serde_json::Value exactly for arrays and objects.
        for depth in [1, 64, 124, 125, 126, 127, 128, 129, 200] {
            let nested_arr = format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
            let nested_obj = format!("{}null{}", r#"{"a":"#.repeat(depth), "}".repeat(depth));
            for nested in [&nested_arr, &nested_obj] {
                let line = format!(
                    r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}},"ignored":{nested}}}"#
                );
                assert_eq!(
                    header(line.as_bytes()).is_ok(),
                    ref_header(line.as_bytes()).is_ok(),
                    "depth {depth}"
                );
                assert_eq!(
                    validate_owner(line.as_bytes(), OWNER).is_ok(),
                    serde_json::from_slice::<Value>(line.as_bytes()).is_ok(),
                    "owner depth {depth}"
                );
            }
        }
        // Verify invalid UTF-8 inside ignored strings is still rejected.
        let mut bad_utf8 =
            format!(r#"{{"type":"session_meta","payload":{{"id":"{OWNER}"}},"ignored":""#)
                .into_bytes();
        bad_utf8.extend_from_slice(b"\xff\"}");
        assert!(header(&bad_utf8).is_err());
        assert!(validate_owner(&bad_utf8, OWNER).is_err());
    }

    #[test]
    fn fast_timestamp_validator_matches_strptime() {
        for ts in [
            "2026-09-10T00-00-00",
            "2024-02-29T23-59-59",
            "2026-02-29T00-00-00",
            "2026-02-31T00-00-00",
            "2026-04-30T23-59-59",
            "2026-04-31T00-00-00",
            "0000-02-29T00-00-00",
            "1900-02-29T00-00-00",
            "2000-02-29T00-00-00",
            "2100-02-29T00-00-00",
            "2026-01-01T24-00-00",
            "2026-01-01T23-59-60",
            "2026-01-01t00-00-00",
            "0000-01-01T00-00-00",
            "9999-12-31T23-59-59",
            "2026-00-01T00-00-00",
            "2026-13-01T00-00-00",
            "abcd-01-01T00-00-00",
        ] {
            let expected = jiff::civil::DateTime::strptime("%Y-%m-%dT%H-%M-%S", ts).is_ok();
            let actual = validate_timestamp(ts).is_ok();
            assert_eq!(actual, expected, "timestamp {ts}");
        }
    }

    #[test]
    fn header_cache_fast_hit_preserves_reverted_rollout_and_relations() {
        let home = tempfile::tempdir().unwrap();
        let reverted = write(home.path(), "archived_sessions", OLD, OWNER, None, false);
        let child = write(home.path(), "sessions", CHILD, CHILD, Some(OLD), false);
        let mut cache = HeaderCache::after_files_settle(&[&reverted, &child]);
        let candidates = HashSet::from([OWNER]);

        let first = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert_eq!(cache.reads, 2);
        assert_eq!(first.owned(OWNER).len(), 1);
        assert_eq!(first.owned(OWNER)[0].rollout_id, OLD);
        assert!(first.externally_referenced(OLD, OWNER));

        let second = scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert_eq!(
            cache.reads, 2,
            "settled reverted and referencing rollouts must hit cache"
        );
        assert_eq!(second.owned(OWNER).len(), 1);
        assert_eq!(second.owned(OWNER)[0].rollout_id, OLD);
        assert!(second.externally_referenced(OLD, OWNER));
        let mut graph = crate::relations::Dependencies::default();
        second.add_dependencies(&mut graph);
        assert!(graph.verify_removal(&candidates).is_err());

        // Ordinary files use zero-arena compact keys; reverted files use the arena.
        assert!(!cache.key_arena.is_empty());
        assert_eq!(cache.relations.len(), 2);

        // Mutating the settled reverted rollout (same length, different owner)
        // must miss cache and fail closed on filename-vs-metadata owner mismatch.
        let other_owner = Uuid::from_u128(99);
        let original = fs::read_to_string(&reverted).unwrap();
        let tampered = original.replacen(&OWNER.to_string(), &other_owner.to_string(), 1);
        assert_eq!(tampered.len(), original.len());
        assert_ne!(tampered, original);
        fs::write(&reverted, &tampered).unwrap();
        assert!(scan(home.path(), &candidates, Some(&mut cache)).is_err());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn header_cache_compacts_relations_arena_and_handles_remove_key_and_invalidations() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let mut ordinary = Vec::new();
        for i in 10u128..34 {
            let id = Uuid::from_u128(i);
            ordinary.push(write(home.path(), "archived_sessions", id, id, None, false));
        }
        let reverted = write(home.path(), "archived_sessions", OLD, OWNER, None, false);
        let child = write(home.path(), "sessions", CHILD, CHILD, Some(OLD), false);

        let mut all_paths: Vec<&Path> = ordinary.iter().map(PathBuf::as_path).collect();
        all_paths.push(&reverted);
        all_paths.push(&child);
        let mut cache = HeaderCache::after_files_settle(&all_paths);
        let candidates = HashSet::from([OWNER]);

        scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert_eq!(cache.entries.len(), ordinary.len() + 2);
        assert_eq!(cache.relations.len(), 2);
        assert!(!cache.key_arena.is_empty());
        let reads_after_first = cache.reads;

        // Touch half the ordinary files so their stamps become unsettled and
        // trigger backward-shift remove_key while the remaining entries stay cached.
        for path in ordinary.iter().take(12) {
            let bytes = fs::read(path).unwrap();
            fs::write(path, bytes).unwrap();
        }
        // Change mode of reverted file -> must miss cache and re-read.
        let mut perms = fs::metadata(&reverted).unwrap().permissions();
        perms.set_mode(0o640);
        fs::set_permissions(&reverted, perms).unwrap();

        scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert_eq!(
            cache.reads,
            reads_after_first + 13,
            "12 touched ordinary + 1 chmod reverted file must re-read; remaining 13 must hit"
        );
        assert_eq!(cache.entries.len(), 13);

        // Remove reverted and child; remaining settled ordinary entries must stay
        // cached while dead relations and arena key bytes compact down to zero.
        fs::remove_file(&reverted).unwrap();
        fs::remove_file(&child).unwrap();
        scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert!(
            cache.relations.is_empty(),
            "relations must compact on eviction"
        );
        assert!(
            cache.key_arena.is_empty(),
            "key_arena must compact on eviction"
        );
        assert_eq!(cache.entries.len(), 12);

        // Hardlinking a cached file must miss and fail closed.
        let hardlink = home.path().join("hardlink-outside");
        fs::hard_link(&ordinary[12], &hardlink).unwrap();
        assert!(scan(home.path(), &candidates, Some(&mut cache)).is_err());
        fs::remove_file(&hardlink).unwrap();

        // Replacing a cached file with a symlink must miss and fail closed.
        let target = home.path().join("symlink-target");
        fs::write(&target, b"{}\n").unwrap();
        let mut cache2 = HeaderCache::after_files_settle(&[&ordinary[13]]);
        scan(home.path(), &candidates, Some(&mut cache2)).unwrap();
        fs::remove_file(&ordinary[13]).unwrap();
        std::os::unix::fs::symlink(&target, &ordinary[13]).unwrap();
        assert!(scan(home.path(), &candidates, Some(&mut cache2)).is_err());
    }

    #[test]
    fn self_owned_and_reverted_conflict_detected_in_both_scan_orders() {
        // Order 1: self-owned in sessions (scanned first), reverted in archived_sessions.
        let home1 = tempfile::tempdir().unwrap();
        write(home1.path(), "sessions", OLD, OLD, None, false);
        write(home1.path(), "archived_sessions", OLD, OWNER, None, false);
        assert!(scan(home1.path(), &HashSet::from([OWNER]), None).is_err());

        // Order 2: reverted in sessions (scanned first), self-owned in archived_sessions.
        let home2 = tempfile::tempdir().unwrap();
        write(home2.path(), "sessions", OLD, OWNER, None, false);
        write(home2.path(), "archived_sessions", OLD, OLD, None, false);
        assert!(scan(home2.path(), &HashSet::from([OWNER]), None).is_err());
    }

    #[test]
    fn compact_rollout_key_roundtrip_and_boundaries() {
        let mut buf = [0u8; 74];
        let cases = [
            (
                "rollout-0000-01-01T00-00-00-00000000-0000-0000-0000-000000000000.jsonl",
                false,
            ),
            (
                "rollout-9999-12-31T23-59-59-ffffffff-ffff-ffff-ffff-ffffffffffff.jsonl.zst",
                true,
            ),
            (
                "rollout-2024-02-29T12-34-56-01950000-0000-7000-8000-000000000042.jsonl",
                false,
            ),
            (
                "rollout-2024-02-29T12-34-56-01950000-0000-7000-8000-000000000042.jsonl.zst",
                true,
            ),
        ];
        for (name, expected_zst) in cases {
            let (ts, uuid, compressed) = parse_compact_rollout_key(name.as_bytes()).expect(name);
            assert_eq!(compressed, expected_zst);
            let len = format_compact_rollout_name(ts, uuid, compressed, &mut buf);
            assert_eq!(&buf[..len], name.as_bytes());
        }
        // Reverted names, bad separators, uppercase hex, and double .zst must not match compact form.
        for rejected in [
            "rollout-2026-09-10T00-00-00-00000000-0000-0000-0000-000000000001_00000000-0000-0000-0000-000000000002.jsonl",
            "rollout-2026-09-10T00-00-00-00000000-0000-0000-0000-00000000000A.jsonl",
            "rollout-2026-09-10T00-00-00-00000000-0000-0000-0000-000000000001.jsonl.zst.zst",
            "rollout-2026-09-10_00-00-00-00000000-0000-0000-0000-000000000001.jsonl",
            "other-2026-09-10T00-00-00-00000000-0000-0000-0000-000000000001.jsonl",
        ] {
            assert!(
                parse_compact_rollout_key(rejected.as_bytes()).is_none(),
                "must reject {rejected}"
            );
        }
    }

    #[test]
    fn header_cache_chunk_boundaries_remove_and_retain_generation() {
        let mut cache = HeaderCache {
            base_epoch: Some(1_700_000_000),
            stable_before: Some(1_700_000_000),
            generation: true,
            ..HeaderCache::default()
        };
        let dir_idx = cache.intern_dir(0, b"").unwrap();
        let make_stamp = |ino: u64| RawStamp {
            device: 7,
            inode: ino,
            len: 512,
            mtime: 1_699_999_000,
            mtime_nsec: 123_456_789,
            ctime: 1_699_999_000,
            ctime_nsec: 987_654_321,
            mode: 0o100600,
            uid: 501,
            gid: 20,
            links: 1,
        };

        // Insert 2,100 entries (> 2,048 chunk size -> spans 2 chunks), mixing compact + arena + relations.
        let total = 2_100usize;
        let mut names = Vec::with_capacity(total);
        for i in 0..total {
            let uuid = Uuid::from_u128(i as u128 + 1);
            let is_reverted = i % 100 == 0;
            let name = if is_reverted {
                let owner = Uuid::from_u128(999_000 + i as u128);
                format!("rollout-2026-09-10T00-00-00-{owner}_{uuid}.jsonl")
            } else {
                format!("rollout-2026-09-10T00-00-00-{uuid}.jsonl")
            };
            let owner = if is_reverted {
                Uuid::from_u128(999_000 + i as u128)
            } else {
                uuid
            };
            let parsed = ParsedHeader {
                owner,
                paginated: i % 3 == 0,
                base: (i % 50 == 0).then_some(Uuid::from_u128(500_000)),
                parent: None,
            };
            assert!(cache.store(
                Some(dir_idx),
                name.as_bytes(),
                None,
                None,
                make_stamp(i as u64 + 1),
                parsed,
                uuid,
            ));
            names.push((name, owner, uuid));
        }
        assert_eq!(cache.entries.len(), total);
        assert!(cache.entries.chunks.len() >= 2);

        // Remove 200 entries across both chunks to exercise swap_remove + backward-shift with fingerprints.
        for (name, _, _) in names.iter().step_by(10) {
            let key = lookup_key(dir_idx, name.as_bytes());
            let hash = cache.hash_lookup_key(&key);
            cache.remove_key(&key, hash);
        }
        assert_eq!(cache.entries.len(), total - total / 10);

        // Flip generation and touch only 50 entries from both chunks, then retain_generation.
        cache.generation = !cache.generation;
        let mut kept_indices = Vec::new();
        for i in (1..total).step_by(41) {
            if i % 10 == 0 {
                continue;
            }
            let key = lookup_key(dir_idx, names[i].0.as_bytes());
            let hash = cache.hash_lookup_key(&key);
            let (_, idx) = cache
                .find_slot(&key, hash)
                .expect("surviving key must be found");
            cache.entries.get_mut(idx).set_generation(cache.generation);
            kept_indices.push(i);
        }
        cache.retain_generation();
        assert_eq!(cache.entries.len(), kept_indices.len());
        assert_eq!(
            cache.entries.chunks.len(),
            1,
            "empty second chunk must be released"
        );
        assert!(
            cache.buckets.len() <= 128,
            "bucket table must shrink after bulk eviction, got {}",
            cache.buckets.len()
        );

        // Verify every kept entry resolves with exact owner/rollout, and evicted ones are absent.
        for &i in &kept_indices {
            let (ref name, expected_owner, expected_rollout) = names[i];
            let key = lookup_key(dir_idx, name.as_bytes());
            let hash = cache.hash_lookup_key(&key);
            let (_, idx) = cache.find_slot(&key, hash).expect("kept entry missing");
            let (parsed, rollout) = cache.entries.get(idx).parsed_and_rollout(&cache.relations);
            assert_eq!(parsed.owner, expected_owner);
            assert_eq!(rollout, expected_rollout);
        }
        let evicted_key = lookup_key(dir_idx, names[2].0.as_bytes());
        let evicted_hash = cache.hash_lookup_key(&evicted_key);
        assert!(cache.find_slot(&evicted_key, evicted_hash).is_none());
    }

    #[test]
    fn cached_self_owned_resolves_history_ref_and_detects_reverted_conflict() {
        // Settled self-owned rollout in sessions, referenced via history_base by a new file.
        let home = tempfile::tempdir().unwrap();
        let base_path = write(home.path(), "sessions", OLD, OLD, None, false);
        let mut cache = HeaderCache::after_files_settle(&[&base_path]);
        // First scan caches OLD (self-owned).
        scan(home.path(), &HashSet::from([OLD]), Some(&mut cache)).unwrap();
        assert_eq!(cache.entries.len(), 1);

        // Add a child rollout referencing OLD via history_base -> second scan hits cache for OLD
        // (so OLD is NOT pushed into self_owned) and must still resolve OLD -> history edge!
        write(
            home.path(),
            "archived_sessions",
            OWNER,
            OWNER,
            Some(OLD),
            false,
        );
        let idx = scan(home.path(), &HashSet::from([OLD, OWNER]), Some(&mut cache)).unwrap();
        let mut graph = crate::relations::Dependencies::default();
        idx.add_dependencies(&mut graph);
        assert!(
            graph.verify_removal(&HashSet::from([OLD])).is_err(),
            "cached self-owned base must block removal when referenced by history_base"
        );

        // Add a conflicting reverted rollout claiming OLD under a different owner -> must fail closed!
        write(home.path(), "archived_sessions", OLD, OWNER, None, false);
        assert!(
            scan(home.path(), &HashSet::from([OLD, OWNER]), Some(&mut cache)).is_err(),
            "conflict between cached self-owned and uncached reverted rollout must fail closed"
        );
    }

    #[test]
    fn parse_canonical_uuid_lut_matches_strict_lowercase_canonical_only() {
        let valid = *b"01950000-0000-7000-8000-0123456789ab";
        assert_eq!(
            parse_canonical_uuid_bytes(&valid),
            Some(Uuid::parse_str("01950000-0000-7000-8000-0123456789ab").unwrap())
        );
        // Every byte value 0..=255 in both hi and lo nibble positions must accept only 0-9, a-f.
        for byte in 0u8..=255 {
            let expected_ok = matches!(byte, b'0'..=b'9' | b'a'..=b'f');
            for pos in [0usize, 1, 34, 35] {
                let mut candidate = valid;
                candidate[pos] = byte;
                let parsed = parse_canonical_uuid_bytes(&candidate);
                assert_eq!(
                    parsed.is_some(),
                    expected_ok,
                    "byte {byte:#04x} at pos {pos}"
                );
                if let Some(uuid) = parsed {
                    assert_eq!(uuid.to_string().as_bytes(), candidate.as_slice());
                }
            }
        }
        // Wrong hyphen positions must fail.
        for hyphen_pos in [8usize, 13, 18, 23] {
            let mut bad = valid;
            bad[hyphen_pos] = b'0';
            assert!(parse_canonical_uuid_bytes(&bad).is_none());
        }
    }

    #[test]
    fn header_cache_split_ctime_caches_settled_files_and_detects_subsequent_edits() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, SystemTime};

        let home = tempfile::tempdir().unwrap();
        let path = write(home.path(), "archived_sessions", OWNER, OWNER, None, false);
        // Backdate mtime so mtime != ctime BEFORE settling (exercises CachedExtra::ctime storage).
        let backdated = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(backdated)
            .unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_ne!(
            meta.mtime(),
            meta.ctime(),
            "test requires distinct mtime and ctime"
        );

        let mut cache = HeaderCache::after_files_settle(&[&path]);
        let candidates = HashSet::from([OWNER]);
        scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert_eq!(cache.reads, 1);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache.relations.len(),
            1,
            "split ctime must be recorded in CachedExtra"
        );

        // Unchanged split-ctime file must hit cache cleanly.
        scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert_eq!(cache.reads, 1, "unchanged split-ctime file must hit cache");

        // Changing permissions updates ctime while preserving mtime -> must miss cache!
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o640);
        fs::set_permissions(&path, perms).unwrap();
        scan(home.path(), &candidates, Some(&mut cache)).unwrap();
        assert_eq!(
            cache.reads, 2,
            "chmod updating ctime on split-ctime entry must miss cache"
        );
    }
}
