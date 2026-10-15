use crate::{Error, Result};
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use rand::RngCore;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
const MAGIC: &[u8; 4] = b"QCK2";
const MAX_SESSION_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) struct LocalStore {
    dir: PathBuf,
}
fn io_error(_: std::io::Error) -> Error {
    Error::new("STORAGE", "本地登录资料读写失败，请检查文件权限")
}
impl LocalStore {
    pub fn open(dir: &Path) -> Result<(Self, Value)> {
        let store = Self {
            dir: dir.to_owned(),
        };
        if let Ok(meta) = fs::symlink_metadata(dir) {
            if !meta.file_type().is_dir() {
                return Err(Error::new("STORAGE", "本地资料目录必须为普通目录"));
            }
        }
        let file = dir.join("session.bin");
        match fs::symlink_metadata(&file) {
            Ok(meta) if !meta.file_type().is_file() || meta.len() > MAX_SESSION_BYTES => {
                return Err(Error::new("STORAGE", "本地登录资料文件无效，原文件已保留"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((store, json!({}))),
            Err(e) => return Err(io_error(e)),
        }
        let bytes = match fs::read(file) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((store, json!({}))),
            Err(e) => return Err(io_error(e)),
        };
        if bytes.starts_with(b"QCK1") {
            return Err(Error::new(
                "LEGACY_SESSION",
                "旧版系统密文请在原客户端导出或在新客户端重新登录；原文件已保留",
            ));
        }
        let doc = if bytes.starts_with(MAGIC) {
            if bytes.len() < 32 {
                return Err(Error::new("STORAGE", "本地登录资料已损坏，原文件已保留"));
            }
            let key = store.read_key()?;
            let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
            let mut encrypted = bytes[32..].to_vec();
            encrypted.extend_from_slice(&bytes[16..32]);
            let plain = cipher
                .decrypt(
                    Nonce::from_slice(&bytes[4..16]),
                    Payload {
                        msg: &encrypted,
                        aad: MAGIC,
                    },
                )
                .map_err(|_| Error::new("STORAGE", "本地登录资料无法校验，原文件和密钥已保留"))?;
            let doc = serde_json::from_slice::<Value>(&plain)
                .map_err(|_| Error::new("STORAGE", "本地登录资料格式损坏"))?;
            if doc["v"] != 2 {
                return Err(Error::new(
                    "STORAGE",
                    "本地登录资料版本不受支持，原文件已保留",
                ));
            }
            doc
        } else {
            return Err(Error::new(
                "STORAGE",
                "本地登录资料格式不受支持，原文件已保留",
            ));
        };
        let document = doc["data"].clone();
        if !document.is_object() {
            return Err(Error::new("STORAGE", "本地登录资料格式损坏，原文件已保留"));
        }
        Ok((store, document))
    }
    fn read_key(&self) -> Result<Vec<u8>> {
        let path = self.dir.join("session.key");
        let meta = fs::symlink_metadata(&path).map_err(io_error)?;
        if !meta.file_type().is_file() {
            return Err(Error::new("STORAGE", "本地登录资料密钥必须为普通文件"));
        }
        let key = fs::read(path).map_err(io_error)?;
        if key.len() != 32 {
            return Err(Error::new("STORAGE", "本地登录资料密钥损坏，原文件已保留"));
        }
        Ok(key)
    }
    fn key(&self) -> Result<Vec<u8>> {
        if self.dir.join("session.key").exists() {
            return self.read_key();
        }
        private_dir(&self.dir)?;
        let mut key = vec![0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        let temp = self
            .dir
            .join(format!(".session.key.{}.tmp", uuid::Uuid::new_v4()));
        write_new(&temp, &key)?;
        let linked = fs::hard_link(&temp, self.dir.join("session.key"));
        let _ = fs::remove_file(&temp);
        if let Err(e) = linked {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(io_error(e));
            }
        }
        sync_dir(&self.dir)?;
        self.read_key()
    }
    pub fn write(&self, data: &Value) -> Result<()> {
        private_dir(&self.dir)?;
        let cipher = Aes256Gcm::new_from_slice(&self.key()?).unwrap();
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let plain = serde_json::to_vec(&json!({"v":2,"data":data}))
            .map_err(|_| Error::new("STORAGE", "本地资料无法序列化"))?;
        if plain.len() as u64 > MAX_SESSION_BYTES - 32 {
            return Err(Error::new("STORAGE", "本地登录资料过大，原文件已保留"));
        }
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: MAGIC,
                },
            )
            .map_err(|_| Error::new("STORAGE", "本地资料加密失败"))?;
        let split = encrypted.len() - 16;
        let mut raw = MAGIC.to_vec();
        raw.extend_from_slice(&nonce);
        raw.extend_from_slice(&encrypted[split..]);
        raw.extend_from_slice(&encrypted[..split]);
        let temp = self
            .dir
            .join(format!(".session.{}.tmp", uuid::Uuid::new_v4()));
        write_new(&temp, &raw)?;
        let result = fs::rename(&temp, self.dir.join("session.bin"))
            .map_err(io_error)
            .and_then(|_| sync_dir(&self.dir));
        let _ = fs::remove_file(&temp);
        result
    }
}
fn private_dir(dir: &Path) -> Result<()> {
    if fs::symlink_metadata(dir).is_ok_and(|meta| !meta.file_type().is_dir()) {
        return Err(Error::new("STORAGE", "本地资料目录必须为普通目录"));
    }
    fs::create_dir_all(dir).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)
            .and_then(|f| f.sync_all())
            .map_err(io_error)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_is_randomized_and_no_credentials_are_in_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = LocalStore::open(dir.path()).unwrap();
        let doc = json!({"cookies":[{"name":"test_only", "value":"synthetic-not-a-real-secret"}]});
        store.write(&doc).unwrap();
        let first = fs::read(dir.path().join("session.bin")).unwrap();
        store.write(&doc).unwrap();
        let second = fs::read(dir.path().join("session.bin")).unwrap();
        assert_ne!(first, second);
        assert!(!second
            .windows(b"synthetic-not-a-real-secret".len())
            .any(|w| w == b"synthetic-not-a-real-secret"));
        assert_eq!(LocalStore::open(dir.path()).unwrap().1, doc);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["session.key", "session.bin"] {
                assert_eq!(
                    fs::metadata(dir.path().join(name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn legacy_unknown_and_oversized_sessions_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.bin");
        for raw in [b"QCK1legacy".as_slice(), b"unknown-format".as_slice()] {
            fs::write(&file, raw).unwrap();
            assert!(LocalStore::open(dir.path()).is_err());
            assert_eq!(fs::read(&file).unwrap(), raw);
        }
        let large = fs::File::create(&file).unwrap();
        large.set_len(MAX_SESSION_BYTES + 1).unwrap();
        assert!(LocalStore::open(dir.path()).is_err());
        assert_eq!(fs::metadata(file).unwrap().len(), MAX_SESSION_BYTES + 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_profile_session_and_key_are_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = root.path().join("linked-profile");
        symlink(&target, &link).unwrap();
        assert!(LocalStore::open(&link).is_err());
        let (store, _) = LocalStore::open(&target).unwrap();
        let key_target = root.path().join("private-key");
        fs::write(&key_target, [7u8; 32]).unwrap();
        symlink(&key_target, target.join("session.key")).unwrap();
        assert!(store.write(&json!({})).is_err());
        assert_eq!(fs::read(&key_target).unwrap(), [7u8; 32]);
        fs::remove_file(target.join("session.key")).unwrap();
        let session_target = root.path().join("private-session");
        fs::write(&session_target, b"untouched").unwrap();
        symlink(&session_target, target.join("session.bin")).unwrap();
        assert!(LocalStore::open(&target).is_err());
        assert_eq!(fs::read(session_target).unwrap(), b"untouched");
    }
}
