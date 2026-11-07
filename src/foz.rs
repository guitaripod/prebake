use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: [u8; 12] = [0x81, b'F', b'O', b'S', b'S', b'I', b'L', b'I', b'Z', b'E', b'D', b'B'];
const VERSION: u8 = 6;
const MIN_VERSION: u8 = 5;
const HEADER_LEN: u64 = 16;
const NAME_LEN: usize = 40;
const ENTRY_HEADER_LEN: usize = NAME_LEN + 16;

pub const TAG_GRAPHICS_PIPELINE: u32 = 6;
pub const TAG_COMPUTE_PIPELINE: u32 = 7;
pub const TAG_RAYTRACING_PIPELINE: u32 = 9;

/// Identifies one Fossilize entry: the resource tag and the 64-bit object hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub tag: u32,
    pub hash: u64,
}

impl Key {
    pub fn is_pipeline(&self) -> bool {
        matches!(self.tag, TAG_GRAPHICS_PIPELINE | TAG_COMPUTE_PIPELINE | TAG_RAYTRACING_PIPELINE)
    }
}

/// Calls `visit` with the key of every complete entry in a Fossilize stream archive.
/// A truncated last entry is ignored, as Fossilize itself does. Returns false when the file is
/// empty or is not a Fossilize archive.
pub fn for_each_key(path: &Path, mut visit: impl FnMut(Key)) -> Result<bool> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(1 << 16, file);
    if !read_magic(&mut reader, len)? {
        return Ok(false);
    }
    let mut offset = HEADER_LEN;
    let mut header = [0u8; ENTRY_HEADER_LEN];
    while offset + ENTRY_HEADER_LEN as u64 <= len {
        reader.read_exact(&mut header)?;
        let stored = stored_size(&header);
        let end = offset + ENTRY_HEADER_LEN as u64 + stored;
        if end > len {
            break;
        }
        if let Some(key) = parse_name(&header[..NAME_LEN]) {
            visit(key);
        }
        reader.seek_relative(stored as i64)?;
        offset = end;
    }
    Ok(true)
}

/// Tells the kernel the file's pages will not be needed again, so scanning gigabytes of
/// shader databases does not push the desktop's working set out of the page cache.
pub fn forget(path: &Path) {
    if let Ok(file) = File::open(path) {
        let _ = nix::fcntl::posix_fadvise(&file, 0, 0, nix::fcntl::PosixFadviseAdvice::POSIX_FADV_DONTNEED);
    }
}

/// Collects the keys of every pipeline entry in the given archives.
pub fn pipeline_keys(paths: &[PathBuf]) -> Result<HashSet<Key>> {
    let mut keys = HashSet::new();
    for path in paths {
        for_each_key(path, |key| {
            if key.is_pipeline() {
                keys.insert(key);
            }
        })?;
    }
    Ok(keys)
}

/// Collects every key in an archive and in the numbered fragments Fossilize writes next to it
/// (`<stem>.<n>.foz`), which is everything a replayer cache or whitelist records.
pub fn keys_with_fragments(base: &Path) -> Result<HashSet<Key>> {
    let mut keys = HashSet::new();
    let mut files = fragments(base)?;
    if base.exists() {
        files.push(base.to_path_buf());
    }
    for path in files {
        for_each_key(&path, |key| {
            keys.insert(key);
        })?;
    }
    Ok(keys)
}

/// Lists `<stem>.<n>.foz` fragments of `base` (`<stem>.foz`), oldest number first.
pub fn fragments(base: &Path) -> Result<Vec<PathBuf>> {
    let (Some(dir), Some(stem)) = (
        base.parent(),
        base.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".foz")),
    ) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{stem}.");
    let mut found = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(number) = name.strip_prefix(&prefix).and_then(|rest| rest.strip_suffix(".foz")) else {
            continue;
        };
        if let Ok(number) = number.parse::<u64>() {
            found.push((number, entry.path()));
        }
    }
    found.sort();
    Ok(found.into_iter().map(|(_, path)| path).collect())
}

/// Folds the numbered fragments of `base` into `base` the way Steam does after a replay:
/// unique entries are copied byte for byte, the result replaces `base` atomically and the
/// merged fragments are deleted. Returns the number of entries in the merged archive, or
/// `None` when there was nothing to merge.
pub fn merge_fragments(base: &Path) -> Result<Option<usize>> {
    let parts = fragments(base)?;
    if parts.is_empty() {
        return Ok(None);
    }
    let tmp = base.with_extension("foz.prebake-tmp");
    let mut sources = Vec::with_capacity(parts.len() + 1);
    if base.exists() {
        sources.push(base.to_path_buf());
    }
    sources.extend(parts.iter().cloned());
    let count = match write_merged(&tmp, &sources) {
        Ok(count) => count,
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
    };
    fs::rename(&tmp, base).with_context(|| format!("replacing {}", base.display()))?;
    for part in parts {
        fs::remove_file(&part).with_context(|| format!("removing {}", part.display()))?;
    }
    Ok(Some(count))
}

/// Writes the unique entries of `sources` to `tmp` and syncs it. Fails without touching
/// anything else when any source cannot be read or has a format it does not know.
fn write_merged(tmp: &Path, sources: &[PathBuf]) -> Result<usize> {
    let out = File::create(tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut writer = BufWriter::with_capacity(1 << 16, out);
    writer.write_all(&MAGIC)?;
    writer.write_all(&[0, 0, 0, VERSION])?;
    let mut seen = HashSet::new();
    let mut count = 0;
    for source in sources {
        count += copy_unique_entries(source, &mut seen, &mut writer)?;
    }
    let file = writer.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    Ok(count)
}

fn copy_unique_entries(path: &Path, seen: &mut HashSet<[u8; NAME_LEN]>, writer: &mut impl Write) -> Result<usize> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if data.len() < HEADER_LEN as usize {
        return Ok(0);
    }
    if data[..12] != MAGIC || !(MIN_VERSION..=VERSION).contains(&data[15]) {
        bail!(
            "{} is not a Fossilize archive this version understands (format {})",
            path.display(),
            data[15]
        );
    }
    let mut offset = HEADER_LEN as usize;
    let mut copied = 0;
    while offset + ENTRY_HEADER_LEN <= data.len() {
        let header = &data[offset..offset + ENTRY_HEADER_LEN];
        let end = offset + ENTRY_HEADER_LEN + stored_size(header) as usize;
        if end > data.len() {
            break;
        }
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&header[..NAME_LEN]);
        if seen.insert(name) {
            writer.write_all(&data[offset..end])?;
            copied += 1;
        }
        offset = end;
    }
    Ok(copied)
}

fn read_magic(reader: &mut impl Read, len: u64) -> Result<bool> {
    if len < HEADER_LEN {
        return Ok(false);
    }
    let mut header = [0u8; HEADER_LEN as usize];
    reader.read_exact(&mut header)?;
    Ok(header[..12] == MAGIC && (MIN_VERSION..=VERSION).contains(&header[15]))
}

fn stored_size(header: &[u8]) -> u64 {
    u64::from(u32::from_le_bytes([header[40], header[41], header[42], header[43]]))
}

fn parse_name(name: &[u8]) -> Option<Key> {
    let text = std::str::from_utf8(name).ok()?;
    let tag = u32::from_str_radix(&text[8..24], 16).ok()?;
    let hash = u64::from_str_radix(&text[24..40], 16).ok()?;
    Some(Key { tag, hash })
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Writes a Fossilize archive holding `entries` (tag, hash, payload) for tests.
    pub fn write_archive(path: &Path, entries: &[(u32, u64, &[u8])]) {
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC);
        data.extend_from_slice(&[0, 0, 0, VERSION]);
        for (tag, hash, payload) in entries {
            data.extend_from_slice(entry_bytes(*tag, *hash, payload).as_slice());
        }
        fs::write(path, data).unwrap();
    }

    pub fn entry_bytes(tag: u32, hash: u64, payload: &[u8]) -> Vec<u8> {
        let mut data = format!("{tag:024x}{hash:016x}").into_bytes();
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(payload);
        data
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn keys_of(path: &Path) -> Vec<Key> {
        let mut keys = Vec::new();
        assert!(for_each_key(path, |k| keys.push(k)).unwrap());
        keys
    }

    #[test]
    fn reads_keys_and_skips_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.foz");
        write_archive(&path, &[(4, 0xaa, b"module bytes"), (6, 0xbb, b"pipe"), (9, 0xcc, b"")]);
        assert_eq!(
            keys_of(&path),
            vec![Key { tag: 4, hash: 0xaa }, Key { tag: 6, hash: 0xbb }, Key { tag: 9, hash: 0xcc }]
        );
        let pipelines = pipeline_keys(&[path]).unwrap();
        assert_eq!(pipelines.len(), 2);
        assert!(!pipelines.contains(&Key { tag: 4, hash: 0xaa }));
    }

    #[test]
    fn ignores_a_truncated_last_entry_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.foz");
        write_archive(&path, &[(6, 1, b"one"), (6, 2, b"two")]);
        let len = fs::metadata(&path).unwrap().len();
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 1).unwrap();
        assert_eq!(keys_of(&path), vec![Key { tag: 6, hash: 1 }]);

        let other = dir.path().join("b.foz");
        fs::write(&other, b"not a fossilize database").unwrap();
        assert!(!for_each_key(&other, |_| {}).unwrap());
        let empty = dir.path().join("c.foz");
        fs::write(&empty, b"").unwrap();
        assert!(!for_each_key(&empty, |_| {}).unwrap());
    }

    #[test]
    fn merges_fragments_into_the_base_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("replay_cache.0011.foz");
        write_archive(&base, &[(6, 1, b""), (6, 2, b"")]);
        write_archive(&dir.path().join("replay_cache.0011.1.foz"), &[(6, 2, b""), (6, 3, b"")]);
        write_archive(&dir.path().join("replay_cache.0011.12.foz"), &[(7, 3, b"")]);
        fs::write(dir.path().join("replay_cache.0011.2.foz"), b"").unwrap();
        write_archive(&dir.path().join("replay_cache.ffff.1.foz"), &[(6, 9, b"")]);

        assert_eq!(merge_fragments(&base).unwrap(), Some(4));
        assert_eq!(
            keys_of(&base),
            vec![
                Key { tag: 6, hash: 1 },
                Key { tag: 6, hash: 2 },
                Key { tag: 6, hash: 3 },
                Key { tag: 7, hash: 3 }
            ]
        );
        assert!(fragments(&base).unwrap().is_empty());
        assert!(dir.path().join("replay_cache.ffff.1.foz").exists());
        assert_eq!(merge_fragments(&base).unwrap(), None);
    }

    #[test]
    fn merging_creates_the_base_when_only_fragments_exist() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("steam_pipeline_cache_whitelist.foz");
        write_archive(&dir.path().join("steam_pipeline_cache_whitelist.3.foz"), &[(4, 7, b"x")]);
        assert_eq!(merge_fragments(&base).unwrap(), Some(1));
        assert_eq!(keys_of(&base), vec![Key { tag: 4, hash: 7 }]);
        assert_eq!(keys_with_fragments(&base).unwrap().len(), 1);
    }

    #[test]
    fn refuses_to_merge_an_archive_of_an_unknown_format() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("replay_cache.u.foz");
        write_archive(&base, &[(6, 1, b"")]);
        let fragment = dir.path().join("replay_cache.u.1.foz");
        write_archive(&fragment, &[(6, 2, b"")]);
        let mut data = fs::read(&fragment).unwrap();
        data[15] = 7;
        fs::write(&fragment, &data).unwrap();
        let before = fs::read(&base).unwrap();
        assert!(merge_fragments(&base).is_err());
        assert_eq!(fs::read(&base).unwrap(), before);
        assert!(fragment.exists());
        assert!(!dir.path().join("replay_cache.u.foz.prebake-tmp").exists());
    }

    #[test]
    fn keeps_payload_bytes_intact_when_merging() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("w.foz");
        write_archive(&dir.path().join("w.1.foz"), &[(4, 5, b"payload-five")]);
        merge_fragments(&base).unwrap();
        let data = fs::read(&base).unwrap();
        assert_eq!(&data[16..], entry_bytes(4, 5, b"payload-five").as_slice());
    }
}
