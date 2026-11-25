//! Content hashing (blake3) for duplicate detection. Only files whose size
//! collides with another member of the same name chain are ever hashed.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

/// Read size per call: large enough for sequential throughput on RAW files.
const BUF_LEN: usize = 1 << 20;

/// Hashes one file, reusing `buf` (grown to [`BUF_LEN`] on first use).
///
/// # Errors
/// Any I/O error opening or reading the file.
pub fn hash_file(path: &Path, buf: &mut Vec<u8>) -> io::Result<blake3::Hash> {
    buf.resize(BUF_LEN, 0);
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    loop {
        let n = match file.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Hashes every path on `pool`. A file that can't be read is left out (and
/// logged), which makes it compare as "different": never a false duplicate.
pub fn hash_all(paths: &[PathBuf], pool: &rayon::ThreadPool) -> HashMap<PathBuf, blake3::Hash> {
    pool.install(|| {
        paths
            .par_iter()
            .map_init(Vec::new, |buf, p| match hash_file(p, buf) {
                Ok(h) => Some((p.clone(), h)),
                Err(e) => {
                    log::warn!("{}: cannot hash for duplicate check: {e}", p.display());
                    None
                }
            })
            .flatten()
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_content_equal_hash() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (d.path().join("a"), d.path().join("b"), d.path().join("c"));
        std::fs::write(&a, b"same").unwrap();
        std::fs::write(&b, b"same").unwrap();
        std::fs::write(&c, b"diff").unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let missing = d.path().join("missing");
        let h = hash_all(&[a.clone(), b.clone(), c.clone(), missing.clone()], &pool);
        assert_eq!(h[&a], h[&b]);
        assert_ne!(h[&a], h[&c]);
        assert!(!h.contains_key(&missing));
    }
}
