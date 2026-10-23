//! Exact equality with a bounded in-memory fast path and a portable disk hash table.
use crate::harness::store::bytes::Spill as Owner;
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use anyhow::{bail, Context, Result};
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};

const MEMORY_BYTES: usize = 1024 * 1024;
const SLOT_BYTES: u64 = 24;
const COMPARE_BYTES: usize = 8192;
const APPEND_BYTES: usize = 64 * 1024;

pub(crate) struct ExactStrings {
    memory: HashSet<String>,
    bytes: usize,
    spill: Option<Disk>,
    hashes: std::collections::hash_map::RandomState,
    limit: usize,
}
impl Default for ExactStrings {
    fn default() -> Self {
        Self {
            memory: HashSet::new(),
            bytes: 0,
            spill: None,
            hashes: std::collections::hash_map::RandomState::new(),
            limit: MEMORY_BYTES,
        }
    }
}
impl ExactStrings {
    pub(crate) fn with_memory_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    /// Checkpoints can retain the bounded fast path; spilled keys remain operation-owned.
    pub(crate) fn memory_values(&self) -> Option<&HashSet<String>> {
        self.spill.is_none().then_some(&self.memory)
    }

    pub(crate) fn contains(&mut self, text: &str) -> Result<bool> {
        match self.spill.as_mut() {
            Some(disk) => {
                let hash = Self::hash(&self.hashes, text);
                Ok(disk.find(text.as_bytes(), hash)?.1)
            }
            None => Ok(self.memory.contains(text)),
        }
    }

    /// True only on first insertion. Empty strings, embedded NUL and hash collisions are exact.
    pub(crate) fn insert(&mut self, text: impl Into<crate::Text>) -> Result<bool> {
        match text.into() {
            crate::Text::Inline(text) => self.insert_text(Cow::Owned(text)),
            text => self.insert_source(&text, &crate::Resolver::new(None)),
        }
    }

    /// Borrow keys that only need writing/comparison after spill. In-memory retention still owns
    /// its exact bytes, and uses the same payload-plus-slot budget as owned insertion.
    pub(crate) fn insert_borrowed(&mut self, text: &str) -> Result<bool> {
        self.insert_text(Cow::Borrowed(text))
    }

    /// Small checked keys keep the bounded memory fast path; larger keys stage once
    /// into the existing disk backend. Neither path publishes before the producer succeeds.
    pub(crate) fn insert_source(&mut self, text: &crate::Text, resolver: &crate::lazy::Resolver) -> Result<bool> {
        if let Some(text) = text.inline_str() {
            return self.insert_borrowed(text);
        }
        // Checked JSON text's raw extent bounds decoded bytes, including wire-form spans.
        let bounded_len = match text {
            crate::Text::Source(source) => Some(source.raw_len()),
            crate::Text::Span(span) => usize::try_from(span.len).ok(),
            crate::Text::Inline(_) => unreachable!(),
        };
        if let Some(bound) = bounded_len.filter(|&bound| self.spill.is_none() && self.memory_cost(bound) <= self.limit)
        {
            let mut decoded = String::with_capacity(bound);
            text.try_for_each_chunk(resolver, APPEND_BYTES, |chunk| {
                anyhow::ensure!(
                    chunk.len() <= bound - decoded.len(),
                    "decoded key exceeds its bounded source extent"
                );
                decoded.push_str(chunk);
                Ok(())
            })?;
            return self.insert(decoded);
        }
        self.spill_memory()?;
        let hasher = self.hashes.build_hasher();
        self.spill.as_mut().unwrap().insert_stream(
            |visit| text.try_for_each_chunk(resolver, APPEND_BYTES, |chunk| visit(chunk.as_bytes())),
            hasher,
        )
    }

    fn spill_memory(&mut self) -> Result<()> {
        if self.spill.is_some() {
            return Ok(());
        }
        let mut disk = Disk::new(self.memory.len())?;
        for old in &self.memory {
            disk.insert(old.as_bytes(), Self::hash(&self.hashes, old))?;
        }
        self.spill = Some(disk);
        self.memory = HashSet::new();
        self.bytes = 0;
        Ok(())
    }

    fn insert_text(&mut self, text: Cow<'_, str>) -> Result<bool> {
        if self.spill.is_none() && self.memory.contains(text.as_ref()) {
            return Ok(false);
        }
        // Account table slots as well as payloads. This conservative upper bound exceeds the
        // HashSet's bucket/control allocation even when the next insert grows an empty table.
        let estimated = self.memory_cost(match &text {
            Cow::Owned(text) => text.capacity(),
            Cow::Borrowed(text) => text.len(),
        });
        if self.spill.is_none() && estimated > self.limit {
            self.spill_memory()?;
        }
        match self.spill.as_mut() {
            Some(disk) => {
                let hash = Self::hash(&self.hashes, &text);
                disk.insert(text.as_bytes(), hash)
            }
            None => {
                let text = text.into_owned();
                self.bytes += text.capacity();
                Ok(self.memory.insert(text))
            }
        }
    }

    fn memory_cost(&self, additional: usize) -> usize {
        let slots = self
            .memory
            .len()
            .saturating_add(1)
            .checked_next_power_of_two()
            .unwrap_or(usize::MAX)
            .saturating_mul(2);
        self.bytes
            .saturating_add(additional)
            .saturating_add(slots.saturating_mul(std::mem::size_of::<String>() + 16))
    }

    #[cfg(test)]
    pub(crate) fn is_spilled(&self) -> bool {
        self.spill.is_some()
    }

    pub(crate) fn clear(&mut self) {
        self.spill.take();
        self.memory = HashSet::new();
        self.bytes = 0;
    }

    fn hash(hashes: &std::collections::hash_map::RandomState, text: &str) -> u64 {
        let mut hasher = hashes.build_hasher();
        // Fixed write boundaries make even hashers whose write calls are significant
        // produce the same hash for arbitrary decoded source chunk partitions.
        for chunk in text.as_bytes().chunks(COMPARE_BYTES) {
            hasher.write(chunk);
        }
        hasher.finish()
    }
}

#[derive(Clone, Copy, Default)]
struct Slot {
    hash: u64,
    offset: u64,
    len: u64,
}
impl Slot {
    fn read(file: &mut Index, index: u64) -> Result<Self> {
        let bytes = file.read(index)?;
        Ok(Self {
            hash: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            offset: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            len: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        })
    }
    fn write(self, file: &mut Index, index: u64) -> Result<()> {
        let mut bytes = [0; SLOT_BYTES as usize];
        bytes[0..8].copy_from_slice(&self.hash.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.offset.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.len.to_le_bytes());
        file.write(index, &bytes)
    }
}
// Index pages have independent offsets; positional I/O avoids a seek syscall for every
// cache miss/dirty eviction without changing the bounded cache or exact comparison contract.
#[cfg(unix)]
fn read_page_at(file: &mut File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(bytes, offset)
}
#[cfg(unix)]
fn write_page_at(file: &mut File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(bytes, offset)
}
#[cfg(windows)]
fn read_page_at(file: &mut File, mut bytes: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        match file.seek_read(bytes, offset) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                offset += n as u64;
                bytes = &mut bytes[n..];
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
#[cfg(windows)]
fn write_page_at(file: &mut File, mut bytes: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        match file.seek_write(bytes, offset) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                offset += n as u64;
                bytes = &bytes[n..];
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
#[cfg(not(any(unix, windows)))]
fn read_page_at(file: &mut File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(bytes)
}
#[cfg(not(any(unix, windows)))]
fn write_page_at(file: &mut File, bytes: &[u8], offset: u64) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)
}

// A fixed direct-mapped page cache avoids a syscall per small bucket lookup. Its bound is
// independent of the index/key count; evictions flush dirty pages before exact reads continue.
const SLOTS_PER_PAGE: u64 = 128;
const PAGE_BYTES: usize = SLOTS_PER_PAGE as usize * SLOT_BYTES as usize;
const CACHE_PAGES: usize = 64;
struct Page {
    key: Option<u64>,
    dirty: bool,
    bytes: [u8; PAGE_BYTES],
}
struct Index {
    file: File,
    _owner: Owner,
    pages: Vec<Page>,
}
impl Index {
    fn new(capacity: u64) -> Result<Self> {
        let owner = Owner::create(false).context("creating private spill file")?;
        let file = owner.file.as_ref().unwrap().try_clone()?;
        let count = capacity.div_ceil(SLOTS_PER_PAGE);
        file.set_len(count.checked_mul(PAGE_BYTES as u64).context("spill index overflow")?)?;
        let mut pages = Vec::with_capacity(CACHE_PAGES);
        for _ in 0..CACHE_PAGES {
            pages.push(Page {
                key: None,
                dirty: false,
                bytes: [0; PAGE_BYTES],
            });
        }
        Ok(Self {
            file,
            _owner: owner,
            pages,
        })
    }
    fn page(&mut self, number: u64) -> Result<&mut Page> {
        let page = &mut self.pages[(number % CACHE_PAGES as u64) as usize];
        if page.key != Some(number) {
            if let Some(old) = page.key.filter(|_| page.dirty) {
                let offset = old.checked_mul(PAGE_BYTES as u64).context("spill index overflow")?;
                write_page_at(&mut self.file, &page.bytes, offset).context("writing exact set index page")?;
            }
            let offset = number.checked_mul(PAGE_BYTES as u64).context("spill index overflow")?;
            // read_exact_at may replace a prefix before failing. Publish bytes and tag together.
            let mut replacement = [0; PAGE_BYTES];
            read_page_at(&mut self.file, &mut replacement, offset).context("reading exact set index page")?;
            page.bytes = replacement;
            page.key = Some(number);
            page.dirty = false;
        }
        Ok(page)
    }
    fn read(&mut self, slot: u64) -> Result<[u8; SLOT_BYTES as usize]> {
        let offset = (slot % SLOTS_PER_PAGE) as usize * SLOT_BYTES as usize;
        let page = self.page(slot / SLOTS_PER_PAGE)?;
        Ok(page.bytes[offset..offset + SLOT_BYTES as usize].try_into().unwrap())
    }
    fn write(&mut self, slot: u64, bytes: &[u8; SLOT_BYTES as usize]) -> Result<()> {
        let offset = (slot % SLOTS_PER_PAGE) as usize * SLOT_BYTES as usize;
        let page = self.page(slot / SLOTS_PER_PAGE)?;
        page.bytes[offset..offset + SLOT_BYTES as usize].copy_from_slice(bytes);
        page.dirty = true;
        Ok(())
    }
}
#[cfg(test)]
thread_local! {
    static FAIL_APPEND_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn write_key(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(prefix) = FAIL_APPEND_AFTER.with(|fault| fault.take()) {
        file.write_all(&bytes[..prefix.min(bytes.len())])?;
        return Err(std::io::Error::other("injected partial append"));
    }
    file.write_all(bytes)
}

struct Disk {
    data: File,
    index: Index,
    _data_owner: Owner,
    capacity: u64,
    len: u64,
    end: u64,
    flushed: u64,
    pending: Vec<u8>,
    failed: bool,
}
impl Disk {
    fn new(len: usize) -> Result<Self> {
        let data_owner = Owner::create(false).context("creating private spill file")?;
        let data = data_owner.file.as_ref().unwrap().try_clone()?;
        let capacity = (len as u64)
            .checked_mul(2)
            .context("spill capacity overflow")?
            .max(16)
            .checked_next_power_of_two()
            .context("spill capacity overflow")?;
        let index = Index::new(capacity)?;
        Ok(Self {
            data,
            index,
            _data_owner: data_owner,
            capacity,
            len: 0,
            end: 0,
            flushed: 0,
            pending: Vec::with_capacity(APPEND_BYTES),
            failed: false,
        })
    }
    fn find(&mut self, text: &[u8], hash: u64) -> Result<(u64, bool)> {
        anyhow::ensure!(!self.failed, "exact set backing failed");
        let mut bucket = hash & (self.capacity - 1);
        for _ in 0..self.capacity {
            let slot = Slot::read(&mut self.index, bucket)?;
            if slot.offset == 0 {
                return Ok((bucket, false));
            }
            if slot.hash == hash && slot.len == text.len() as u64 && self.equal(slot, text)? {
                return Ok((bucket, true));
            }
            bucket = (bucket + 1) & (self.capacity - 1);
        }
        bail!("exact set index is full")
    }
    fn equal(&mut self, slot: Slot, text: &[u8]) -> Result<bool> {
        if text.is_empty() {
            return Ok(true);
        }
        let start = slot.offset - 1;
        let disk_len = self.flushed.saturating_sub(start).min(text.len() as u64) as usize;
        if disk_len > 0 {
            self.data.seek(SeekFrom::Start(start))?;
            let mut scratch = [0; COMPARE_BYTES];
            for chunk in text[..disk_len].chunks(COMPARE_BYTES) {
                self.data
                    .read_exact(&mut scratch[..chunk.len()])
                    .context("reading exact set key")?;
                if chunk != &scratch[..chunk.len()] {
                    return Ok(false);
                }
            }
        }
        if disk_len == text.len() {
            return Ok(true);
        }
        let offset =
            usize::try_from(start + disk_len as u64 - self.flushed).context("exact set pending offset overflow")?;
        let end = offset
            .checked_add(text.len() - disk_len)
            .context("exact set pending size overflow")?;
        let pending = self.pending.get(offset..end).context("invalid exact set key range")?;
        Ok(pending == &text[disk_len..])
    }
    fn append(&mut self, mut text: &[u8]) -> Result<()> {
        while !text.is_empty() {
            // Large keys pass directly to the append file; small consecutive IDs share a buffer
            // and repeated recent IDs compare against those exact bytes without another syscall.
            if self.pending.is_empty() && text.len() >= APPEND_BYTES {
                let count = text.len() / APPEND_BYTES * APPEND_BYTES;
                self.data.seek(SeekFrom::Start(self.flushed))?;
                write_key(&mut self.data, &text[..count]).context("writing exact set key")?;
                self.flushed += count as u64;
                text = &text[count..];
                continue;
            }
            let count = text.len().min(APPEND_BYTES - self.pending.len());
            self.pending.extend_from_slice(&text[..count]);
            text = &text[count..];
            if self.pending.len() == APPEND_BYTES {
                self.data.seek(SeekFrom::Start(self.flushed))?;
                write_key(&mut self.data, &self.pending).context("writing exact set key window")?;
                self.flushed += self.pending.len() as u64;
                self.pending.clear();
            }
        }
        Ok(())
    }
    fn insert(&mut self, text: &[u8], hash: u64) -> Result<bool> {
        let (mut bucket, present) = self.find(text, hash)?;
        if present {
            return Ok(false);
        }
        if self.len >= self.capacity / 2 {
            self.grow()?;
            bucket = self.find(text, hash)?.0;
        }
        let start = self.end;
        // Slot offset zero means vacant; occupied offsets are biased by one, including byte zero.
        let offset = start.checked_add(1).context("spill offset overflow")?;
        let end = start.checked_add(text.len() as u64).context("spill size overflow")?;
        let slot = Slot {
            hash,
            offset,
            len: text.len() as u64,
        };
        if let Err(error) = self.append(text).and_then(|()| slot.write(&mut self.index, bucket)) {
            return Err(self.roll_back(start, error));
        }
        self.end = end;
        self.len += 1;
        Ok(true)
    }

    /// The one rule for a key that did not become a member: its bytes past `start` go, so the
    /// next key is written where its slot will say it is. A rollback that fails stops reuse.
    fn roll_back(&mut self, start: u64, error: anyhow::Error) -> anyhow::Error {
        match self.discard_suffix(start) {
            Ok(()) => error,
            Err(cleanup) => error.context(format!("exact set rollback also failed: {cleanup}")),
        }
    }

    fn flush_pending(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            self.data.seek(SeekFrom::Start(self.flushed))?;
            write_key(&mut self.data, &self.pending).context("flushing exact set keys")?;
            self.flushed += self.pending.len() as u64;
            self.pending.clear();
        }
        Ok(())
    }

    /// Discard unpublished physical bytes even when write_all failed before advancing flushed.
    /// Keep the valid pending prefix; failed cleanup permanently prevents reuse.
    fn discard_suffix(&mut self, start: u64) -> Result<()> {
        let boundary = start.min(self.flushed);
        let result = self
            .data
            .set_len(boundary)
            .context("discarding unpublished exact set key");
        if result.is_err() {
            self.failed = true;
        }
        self.pending
            .truncate(usize::try_from(start - boundary).unwrap_or(usize::MAX));
        self.flushed = boundary;
        result
    }

    fn equal_ranges(&mut self, slot: Slot, start: u64, len: u64) -> Result<bool> {
        let mut left = [0; COMPARE_BYTES];
        let mut right = [0; COMPARE_BYTES];
        let mut offset = 0;
        while offset < len {
            let count = (len - offset).min(COMPARE_BYTES as u64) as usize;
            read_page_at(&mut self.data, &mut left[..count], slot.offset - 1 + offset)?;
            read_page_at(&mut self.data, &mut right[..count], start + offset)?;
            if left[..count] != right[..count] {
                return Ok(false);
            }
            offset += count as u64;
        }
        Ok(true)
    }

    /// Stage one decoded key in the same append file as retained keys. Errors and duplicates
    /// discard only unpublished bytes; complete source validation precedes exact membership.
    fn insert_stream(
        &mut self,
        produce: impl FnOnce(&mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()>,
        hasher: impl Hasher,
    ) -> Result<bool> {
        anyhow::ensure!(!self.failed, "exact set backing failed");
        if let Err(error) = self.flush_pending() {
            // A prefix-flush error happened before source staging began. Stop reuse rather
            // than treat partly persisted retained keys as a successful insertion.
            self.failed = true;
            return Err(error);
        }
        let start = self.end;
        let result = (|| {
            let mut hash = ChunkHash::new(hasher);
            let mut len = 0u64;
            produce(&mut |chunk| {
                len = len
                    .checked_add(chunk.len() as u64)
                    .context("exact set key length overflow")?;
                start.checked_add(len).context("exact set key extent overflow")?;
                hash.write(chunk);
                self.append(chunk)
            })?;
            self.flush_pending()?;
            let hash = hash.finish();
            let mut bucket = hash & (self.capacity - 1);
            for _ in 0..self.capacity {
                let slot = Slot::read(&mut self.index, bucket)?;
                if slot.offset == 0 {
                    if self.len >= self.capacity / 2 {
                        self.grow()?;
                        bucket = hash & (self.capacity - 1);
                        while Slot::read(&mut self.index, bucket)?.offset != 0 {
                            bucket = (bucket + 1) & (self.capacity - 1);
                        }
                    }
                    Slot {
                        hash,
                        // Zero is the vacant-slot sentinel; store the byte offset plus one.
                        offset: start.checked_add(1).context("spill offset overflow")?,
                        len,
                    }
                    .write(&mut self.index, bucket)?;
                    self.end = start + len;
                    self.len += 1;
                    return Ok(true);
                }
                if slot.hash == hash && slot.len == len && self.equal_ranges(slot, start, len)? {
                    return Ok(false);
                }
                bucket = (bucket + 1) & (self.capacity - 1);
            }
            bail!("exact set index is full")
        })();
        match result {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.discard_suffix(start)?;
                Ok(false)
            }
            Err(error) => Err(self.roll_back(start, error)),
        }
    }
    fn grow(&mut self) -> Result<()> {
        let capacity = self.capacity.checked_mul(2).context("spill capacity overflow")?;
        let mut index = Index::new(capacity)?;
        // Rehash one slot at a time. Neither the keys nor the index are materialized in RAM.
        for old in 0..self.capacity {
            let slot = Slot::read(&mut self.index, old)?;
            if slot.offset == 0 {
                continue;
            }
            let mut bucket = slot.hash & (capacity - 1);
            while Slot::read(&mut index, bucket)?.offset != 0 {
                bucket = (bucket + 1) & (capacity - 1);
            }
            slot.write(&mut index, bucket)?;
        }
        self.index = index;
        self.capacity = capacity;
        Ok(())
    }
}

struct ChunkHash<H> {
    hasher: H,
    pending: Vec<u8>,
}
impl<H: Hasher> ChunkHash<H> {
    fn new(hasher: H) -> Self {
        Self {
            hasher,
            pending: Vec::with_capacity(COMPARE_BYTES),
        }
    }
    fn write(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.pending.is_empty() && bytes.len() >= COMPARE_BYTES {
                self.hasher.write(&bytes[..COMPARE_BYTES]);
                bytes = &bytes[COMPARE_BYTES..];
            } else {
                let count = (COMPARE_BYTES - self.pending.len()).min(bytes.len());
                self.pending.extend_from_slice(&bytes[..count]);
                bytes = &bytes[count..];
                if self.pending.len() == COMPARE_BYTES {
                    self.hasher.write(&self.pending);
                    self.pending.clear();
                }
            }
        }
    }
    fn finish(mut self) -> u64 {
        if !self.pending.is_empty() {
            self.hasher.write(&self.pending);
        }
        self.hasher.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_index_read_preserves_cached_page_on_reuse() {
        let other = CACHE_PAGES as u64;
        let mut index = Index::new((other + 1) * SLOTS_PER_PAGE).unwrap();
        let old = [0xa5; SLOT_BYTES as usize];
        index.write(0, &old).unwrap(); // dirty page zero shares its cache slot with `other`
        let offset = other * PAGE_BYTES as u64;
        index.file.set_len(offset + 128).unwrap();
        write_page_at(&mut index.file, &[0x5a; 128], offset).unwrap();
        assert!(index.read(other * SLOTS_PER_PAGE).is_err()); // prefix read, then EOF
        assert_eq!(
            index.read(0).unwrap(),
            old,
            "failed replacement must not retag mixed bytes"
        );
        // Successful retry/eviction must not flush damaged bytes under the old page's offset.
        index.file.set_len(offset + PAGE_BYTES as u64).unwrap();
        assert_eq!(index.read(other * SLOTS_PER_PAGE).unwrap(), [0x5a; SLOT_BYTES as usize]);
        assert_eq!(index.read(0).unwrap(), old);
    }

    #[derive(Default)]
    struct CollisionHash;
    impl Hasher for CollisionHash {
        fn write(&mut self, _: &[u8]) {}
        fn finish(&self) -> u64 {
            7
        }
    }

    #[test]
    fn streamed_keys_preserve_collisions_partitions_and_failed_source_rollback() {
        let mut disk = Disk::new(0).unwrap();
        assert!(disk.insert(b"existing", 7).unwrap());
        let giant = "key\0λ🌙".repeat(300_000).into_bytes();
        let stream = |disk: &mut Disk, bytes: &[u8], chunk: usize| {
            disk.insert_stream(
                |visit| {
                    for part in bytes.chunks(chunk) {
                        visit(part)?;
                    }
                    Ok(())
                },
                CollisionHash,
            )
        };
        assert!(stream(&mut disk, &giant, 19).unwrap());
        let retained_end = disk.end;
        assert!(!stream(&mut disk, &giant, 65536).unwrap());
        assert_eq!(disk.end, retained_end);
        assert_eq!(disk.data.metadata().unwrap().len(), retained_end);
        let mut different = giant.clone();
        *different.last_mut().unwrap() ^= 1;
        assert!(stream(&mut disk, &different, 8001).unwrap());
        assert!(!stream(&mut disk, b"existing", 1).unwrap());
        let retained_end = disk.end;
        let error = disk
            .insert_stream(
                |visit| {
                    visit(&giant)?;
                    anyhow::bail!("source changed after decoded output")
                },
                CollisionHash,
            )
            .unwrap_err();
        assert!(error.to_string().contains("source changed"));
        assert_eq!(disk.end, retained_end);
        assert_eq!(disk.data.metadata().unwrap().len(), retained_end);
        assert!(disk.find(&giant, 7).unwrap().1);
        assert!(stream(&mut disk, b"after failure", 3).unwrap());
        assert!(stream(&mut disk, b"", 1).unwrap());
        assert!(!stream(&mut disk, b"", 3).unwrap());
    }

    #[test]
    fn partial_append_failure_truncates_physical_suffix_and_keeps_pending_prefix() {
        for retained in [0, 8, APPEND_BYTES + 8] {
            for streamed in [false, true] {
                let mut disk = Disk::new(0).unwrap();
                let prefix = vec![b'r'; retained];
                assert!(disk.insert(&prefix, 1).unwrap());
                if streamed {
                    disk.flush_pending().unwrap();
                }
                let boundary = disk.flushed;
                FAIL_APPEND_AFTER.with(|fault| fault.set(Some(4096)));
                let failed = vec![b'f'; APPEND_BYTES];
                let error = if streamed {
                    // Fail the final flush, after the producer successfully staged a short key.
                    disk.insert_stream(|visit| visit(&failed[..8000]), CollisionHash)
                        .unwrap_err()
                } else {
                    disk.insert(&failed, 2).unwrap_err()
                };
                assert!(format!("{error:#}").contains("injected partial append"));
                assert_eq!(disk.data.metadata().unwrap().len(), boundary);
                assert_eq!(disk.flushed + disk.pending.len() as u64, retained as u64);
                assert!(!disk.failed);
                assert!(disk.find(&prefix, 1).unwrap().1);
                assert!(disk.insert(b"after", 3).unwrap());
                assert!(disk.find(b"after", 3).unwrap().1);
            }
        }
    }

    /// A key whose bytes fail to reach the append file is no member, and leaves no bytes behind
    /// for the next key's offset to miss: `insert` rolls back as `insert_stream` does.
    #[test]
    fn failed_rollback_cleanup_poison_prevents_reuse() {
        let world = cv_testkit::World::new("exact-insert-rollback");
        let mut disk = Disk::new(0).unwrap();
        assert!(disk.insert(b"retained", 1).unwrap());
        let read_only = world.base.join("read-only");
        std::fs::write(&read_only, "").unwrap();
        let writable = std::mem::replace(&mut disk.data, File::open(&read_only).unwrap());
        let failed = vec![b'f'; APPEND_BYTES];
        let error = disk.insert(&failed, 2).unwrap_err();
        assert!(
            format!("{error:#}").contains("writing exact set key window"),
            "{error:#}"
        );
        assert_eq!((disk.end, disk.flushed + disk.pending.len() as u64), (8, 8));
        assert!(disk.failed, "a failed physical truncation must poison reuse");
        disk.data = writable;
        assert!(disk.find(b"retained", 1).is_err());
        assert!(disk.insert(b"after", 3).is_err());
    }

    #[test]
    fn small_checked_sources_keep_memory_fast_path() {
        let world = crate::test_world("exact-small-source");
        let mut set = ExactStrings::default();
        let resolver = crate::lazy::Resolver::new(None);
        for (index, body) in ["short λ\n".to_string(), "escaped \"🌙\"".repeat(1000)]
            .into_iter()
            .enumerate()
        {
            let path = world.home.join(format!("key-{index}.json"));
            std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"key":body})).unwrap()).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let text = source
                .read(ReadInput::Document, Validation::Value, |view| {
                    let crate::Json::Source(range) = view.get("key")?.unwrap().to_json()? else {
                        anyhow::bail!("fixture must retain checked source range");
                    };
                    Ok(crate::Text::Source(crate::lazy::SourceText::new(range)))
                })
                .unwrap();
            assert!(matches!(text, crate::Text::Source(_)));
            assert!(set.insert_source(&text, &resolver).unwrap());
            assert!(!set.insert_source(&text, &resolver).unwrap());
            assert!(!set.is_spilled(), "ordinary checked prompts must not open spill files");
            assert!(set.contains(&body).unwrap());
        }
    }

    #[test]
    fn streamed_prefix_flush_failure_rejects_backend_before_producer_runs() {
        let world = cv_testkit::World::new("exact-flush-failure");
        let path = world.base.join("read-only-data");
        std::fs::write(&path, "unchanged").unwrap();
        let mut disk = Disk::new(0).unwrap();
        assert!(disk.insert(b"pending existing key", 7).unwrap());
        disk.data = File::open(&path).unwrap();
        let mut called = false;
        assert!(disk
            .insert_stream(
                |_| {
                    called = true;
                    Ok(())
                },
                CollisionHash
            )
            .is_err());
        assert!(!called);
        assert!(disk.failed);
        assert!(disk.find(b"pending existing key", 7).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"unchanged");
    }

    #[test]
    fn checked_source_insertion_matches_inline_keys_and_rejects_stale_sources() {
        let world = crate::test_world("exact-checked-source");
        let path = world.home.join("key.json");
        let body = "key\0λ🌙\n".repeat(200_000);
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"key":body})).unwrap()).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let text = source
            .read(ReadInput::Document, Validation::Value, |view| {
                view.get("key")?.unwrap().source_text()?.context("key string")
            })
            .unwrap();
        assert!(matches!(text, crate::Text::Source(_)));
        let resolver = crate::lazy::Resolver::new(Some(path.clone()));
        let mut set = ExactStrings::default();
        assert!(set.insert_borrowed("retained before spill").unwrap());
        assert!(set.insert_source(&text, &resolver).unwrap());
        assert!(set.is_spilled(), "giant checked keys must spill");
        assert!(!set.insert_borrowed(&body).unwrap());
        assert!(!set.insert_source(&text, &resolver).unwrap());
        std::fs::write(&path, "changed backing").unwrap();
        assert!(set.insert_source(&text, &resolver).is_err());
        assert!(set.contains(&body).unwrap());
        assert!(set.contains("retained before spill").unwrap());
        assert!(set.insert_borrowed("after stale source").unwrap());
    }
    #[test]
    fn collisions_growth_empty_and_binary_keys_match_hash_set() {
        let mut disk = Disk::new(0).unwrap();
        let mut oracle = HashSet::new();
        for i in 0..300 {
            let key = format!("{i}\0λ");
            // Deliberately identical hashes require full key comparison, across index growth.
            assert_eq!(disk.insert(key.as_bytes(), 7).unwrap(), oracle.insert(key.clone()));
            assert!(!disk.insert(key.as_bytes(), 7).unwrap());
        }
        assert!(disk.insert(b"", 7).unwrap());
        assert!(!disk.insert(b"", 7).unwrap());
        for key in oracle {
            assert!(disk.find(key.as_bytes(), 7).unwrap().1);
        }
        assert!(!disk.find(b"absent", 7).unwrap().1);
    }
    #[test]
    fn tiny_keys_charge_slots_spill_and_clear() {
        let mut set = ExactStrings {
            limit: 1024,
            ..ExactStrings::default()
        };
        assert!(set.insert(String::new()).unwrap());
        assert!(!set.insert(String::new()).unwrap());
        for i in 0..400 {
            assert!(set.insert(i.to_string()).unwrap());
        }
        assert!(set.is_spilled());
        assert_eq!(set.memory.capacity(), 0);
        for i in 0..400 {
            assert!(set.contains(&i.to_string()).unwrap());
        }
        set.clear();
        assert!(!set.is_spilled());
        assert!(!set.contains("").unwrap());
    }
    #[test]
    fn borrowed_keys_survive_caller_changes_and_spill() {
        let mut set = ExactStrings {
            limit: 1024,
            ..ExactStrings::default()
        };
        let mut caller = String::from("source\0λ");
        assert!(set.insert_borrowed(&caller).unwrap());
        caller.clear();
        assert!(set.contains("source\0λ").unwrap());
        let large = "bodyλ\0".repeat(1024);
        assert!(set.insert_borrowed(&large).unwrap());
        assert!(set.is_spilled());
        assert!(!set.insert_borrowed("source\0λ").unwrap());
        assert!(!set.insert_borrowed(&large).unwrap());
        assert!(set.insert_borrowed("").unwrap());
        assert!(!set.insert_borrowed("").unwrap());
        assert!(!set.contains("source").unwrap());
    }

    #[test]
    fn cache_evictions_and_index_growth_keep_every_key() {
        let mut disk = Disk::new(0).unwrap();
        for i in 0..20000_u64 {
            assert!(disk
                .insert(&i.to_le_bytes(), i.wrapping_mul(0x9e3779b97f4a7c15))
                .unwrap());
        }
        assert!(disk.capacity > SLOTS_PER_PAGE * CACHE_PAGES as u64);
        for i in (0..20000_u64).rev() {
            assert!(
                disk.find(&i.to_le_bytes(), i.wrapping_mul(0x9e3779b97f4a7c15))
                    .unwrap()
                    .1
            );
        }
    }
    #[test]
    fn exact_comparison_crosses_flushed_and_pending_boundaries() {
        let mut disk = Disk::new(0).unwrap();
        for (i, len) in [0, APPEND_BYTES - 3, 11, APPEND_BYTES * 2 + 9, 2]
            .into_iter()
            .enumerate()
        {
            let key = vec![i as u8; len];
            assert!(disk.insert(&key, i as u64).unwrap());
            assert!(!disk.insert(&key, i as u64).unwrap());
        }
        for (i, len) in [0, APPEND_BYTES - 3, 11, APPEND_BYTES * 2 + 9, 2]
            .into_iter()
            .enumerate()
        {
            assert!(disk.find(&vec![i as u8; len], i as u64).unwrap().1);
            if len > 0 {
                assert!(!disk.find(&vec![99; len], i as u64).unwrap().1);
            }
        }
    }
    #[test]
    fn disk_io_error_is_not_a_duplicate() {
        let mut disk = Disk::new(0).unwrap();
        disk.insert(b"key", 1).unwrap();
        disk.index.file.set_len(0).unwrap();
        for page in &mut disk.index.pages {
            page.key = None;
        }
        assert!(disk.find(b"key", 1).is_err());
    }
    #[test]
    fn many_small_strings_spill_and_keep_membership() {
        let mut texts = ExactStrings::default();
        for i in 0..40000 {
            texts.insert(i.to_string()).unwrap();
        }
        assert!(texts.is_spilled(), "small strings still consume entry storage");
        assert!(texts.contains("0").unwrap());
        assert!(texts.contains("39999").unwrap());
    }
    #[test]
    fn spilled_strings_keep_exact_equality_and_release_on_clear() {
        let mut texts = ExactStrings::default();
        texts.insert("first").unwrap();
        let large = "x".repeat(1024 * 1024 + 1);
        texts.insert(large.clone()).unwrap();
        assert!(texts.contains("first").unwrap());
        assert!(texts.contains(&large).unwrap());
        assert!(!texts.contains("FIRST").unwrap());
        assert!(!texts.contains(&(large.clone() + "x")).unwrap());
        texts.insert("nul\0suffix").unwrap();
        assert!(texts.contains("nul\0suffix").unwrap());
        assert!(!texts.contains("nul").unwrap());
        texts.clear();
        assert!(!texts.is_spilled());
        assert!(!texts.contains("first").unwrap());
    }
}
