use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use brgr_protocol::ArtifactRef;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{StoreError, format_sha256, private_directory, private_file};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub(crate) fn open(store_root: &Path) -> Result<Self, StoreError> {
        let root = store_root.join("artifacts").join("sha256");
        private_directory(&root)?;
        Ok(Self { root })
    }

    pub(crate) fn seal_path(
        &self,
        source: &Path,
        media_type: &str,
        max_bytes: u64,
    ) -> Result<ArtifactRef, StoreError> {
        let metadata = fs::symlink_metadata(source)?;
        if !metadata.file_type().is_file() {
            return Err(StoreError::ArtifactNotRegularFile {
                path: source.to_path_buf(),
            });
        }
        if metadata.len() > max_bytes {
            return Err(StoreError::ArtifactTooLarge {
                max_bytes,
                observed_bytes: metadata.len(),
            });
        }

        let file = File::open(source)?;
        if !same_regular_file(&metadata, &file.metadata()?) {
            return Err(StoreError::ArtifactNotRegularFile {
                path: source.to_path_buf(),
            });
        }
        self.seal_reader(file, media_type, max_bytes)
    }

    pub(crate) fn seal_reader(
        &self,
        mut reader: impl Read,
        media_type: &str,
        max_bytes: u64,
    ) -> Result<ArtifactRef, StoreError> {
        if max_bytes == 0 {
            return Err(StoreError::InvalidArtifactLimit);
        }
        if media_type.trim().is_empty() {
            return Err(StoreError::InvalidMediaType);
        }

        let staging = self.root.join(".tmp");
        private_directory(&staging)?;
        let mut temporary = NamedTempFile::new_in(&staging)?;
        private_file(temporary.path())?;

        let mut hasher = Sha256::new();
        let mut bytes = 0_u64;
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES].into_boxed_slice();
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes = bytes
                .checked_add(u64::try_from(read).map_err(|_| StoreError::ArtifactSizeOverflow)?)
                .ok_or(StoreError::ArtifactSizeOverflow)?;
            if bytes > max_bytes {
                return Err(StoreError::ArtifactTooLarge {
                    max_bytes,
                    observed_bytes: bytes,
                });
            }
            temporary.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
        }

        temporary.as_file_mut().sync_all()?;
        let hash = hasher.finalize();
        let digest = format_sha256(hash.as_ref());
        let hex_digest = digest.trim_start_matches("sha256:");
        let digest_directory = self.root.join(&hex_digest[..2]);
        private_directory(&digest_directory)?;
        let destination = digest_directory.join(&hex_digest[2..]);

        match temporary.persist_noclobber(&destination) {
            Ok(file) => {
                file.sync_all()?;
                private_file(&destination)?;
                sync_directory(&digest_directory)?;
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                let reference = ArtifactRef {
                    digest: digest.clone(),
                    bytes,
                    media_type: media_type.to_owned(),
                    store_relative_path: format!(
                        "artifacts/sha256/{}/{}",
                        &hex_digest[..2],
                        &hex_digest[2..]
                    ),
                };
                self.read_verified(&reference, max_bytes)
                    .map_err(|_| StoreError::ArtifactDigestCollision(digest.clone()))?;
                private_file(&destination)?;
            }
            Err(error) => return Err(StoreError::Io(error.error)),
        }

        let relative = destination
            .strip_prefix(self.store_parent())
            .map_err(|_| StoreError::ArtifactPathOutsideStore)?;
        Ok(ArtifactRef {
            digest,
            bytes,
            media_type: media_type.to_owned(),
            store_relative_path: relative.to_string_lossy().into_owned(),
        })
    }

    fn store_parent(&self) -> &Path {
        self.root
            .parent()
            .and_then(Path::parent)
            .expect("artifact root always has a store parent")
    }

    pub(crate) fn read_verified(
        &self,
        reference: &ArtifactRef,
        max_bytes: u64,
    ) -> Result<Vec<u8>, StoreError> {
        let hex = reference
            .digest
            .strip_prefix("sha256:")
            .filter(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
            .ok_or(StoreError::InvalidArtifactReference)?;
        let expected = format!("artifacts/sha256/{}/{}", &hex[..2], &hex[2..]);
        if reference.store_relative_path != expected || reference.media_type.trim().is_empty() {
            return Err(StoreError::InvalidArtifactReference);
        }
        if reference.bytes > max_bytes {
            return Err(StoreError::ArtifactTooLarge {
                max_bytes,
                observed_bytes: reference.bytes,
            });
        }
        let directory = self.root.join(&hex[..2]);
        for path in [
            self.store_parent(),
            self.root.parent().expect("sha256 root has parent"),
            self.root.as_path(),
            directory.as_path(),
        ] {
            if !fs::symlink_metadata(path)?.file_type().is_dir() {
                return Err(StoreError::InvalidArtifactReference);
            }
        }
        let path = directory.join(&hex[2..]);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(StoreError::ArtifactNotRegularFile { path });
        }
        if metadata.len() != reference.bytes {
            return Err(StoreError::ArtifactIntegrityMismatch);
        }
        let mut file = File::open(&path)?;
        if !same_regular_file(&metadata, &file.metadata()?) {
            return Err(StoreError::ArtifactNotRegularFile { path });
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).map_err(|_| StoreError::ArtifactSizeOverflow)?
            != reference.bytes
            || format_sha256(Sha256::digest(&bytes).as_ref()) != reference.digest
        {
            return Err(StoreError::ArtifactIntegrityMismatch);
        }
        Ok(bytes)
    }
}

fn same_regular_file(before: &fs::Metadata, opened: &fs::Metadata) -> bool {
    before.file_type().is_file()
        && opened.file_type().is_file()
        && before.dev() == opened.dev()
        && before.ino() == opened.ino()
        && before.len() == opened.len()
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    OpenOptions::new().read(true).open(path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn artifact_limit_rejects_oversized_input_without_publishing_it() {
        let root = TempDir::new().unwrap();
        let store = ArtifactStore::open(root.path()).unwrap();

        let error = store
            .seal_reader(Cursor::new(b"too large"), "text/plain", 3)
            .unwrap_err();

        assert!(matches!(error, StoreError::ArtifactTooLarge { .. }));
        let published_files = walk_files(&root.path().join("artifacts/sha256"))
            .into_iter()
            .filter(|path| !path.components().any(|part| part.as_os_str() == ".tmp"))
            .count();
        assert_eq!(published_files, 0);
    }

    #[test]
    fn same_content_reuses_one_private_content_addressed_file() {
        let root = TempDir::new().unwrap();
        let store = ArtifactStore::open(root.path()).unwrap();

        let first = store
            .seal_reader(Cursor::new(b"report"), "text/plain", 100)
            .unwrap();
        let second = store
            .seal_reader(Cursor::new(b"report"), "text/plain", 100)
            .unwrap();

        assert_eq!(first, second);
        let artifact_path = root.path().join(&first.store_relative_path);
        assert_eq!(fs::read(&artifact_path).unwrap(), b"report");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(artifact_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(store.read_verified(&first, 100).unwrap(), b"report");
    }

    #[test]
    fn read_rejects_forged_path_modified_bytes_and_symlink() {
        let root = TempDir::new().unwrap();
        let store = ArtifactStore::open(root.path()).unwrap();
        let reference = store
            .seal_reader(Cursor::new(b"report"), "text/plain", 100)
            .unwrap();
        let path = root.path().join(&reference.store_relative_path);

        let mut forged = reference.clone();
        forged.store_relative_path = "../outside".to_owned();
        assert!(matches!(
            store.read_verified(&forged, 100),
            Err(StoreError::InvalidArtifactReference)
        ));
        assert!(matches!(
            store.read_verified(&reference, 3),
            Err(StoreError::ArtifactTooLarge { .. })
        ));

        fs::write(&path, b"change").unwrap();
        assert!(matches!(
            store.read_verified(&reference, 100),
            Err(StoreError::ArtifactIntegrityMismatch)
        ));
        fs::remove_file(&path).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path().join("outside"), &path).unwrap();
            assert!(matches!(
                store.read_verified(&reference, 100),
                Err(StoreError::ArtifactNotRegularFile { .. })
            ));
        }
    }

    #[test]
    fn opened_artifact_must_match_the_checked_inode_and_size() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"other").unwrap();
        let checked = fs::symlink_metadata(&first).unwrap();
        let different = File::open(&second).unwrap().metadata().unwrap();
        assert!(!same_regular_file(&checked, &different));
        assert!(same_regular_file(
            &checked,
            &File::open(&first).unwrap().metadata().unwrap()
        ));
    }

    fn walk_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = fs::read_dir(root) else {
            return files;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk_files(&path));
            } else {
                files.push(path);
            }
        }
        files
    }
}
