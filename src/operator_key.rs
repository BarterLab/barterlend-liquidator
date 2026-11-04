//! Explicit, bounded key-file input. Secret bytes are never formatted into errors.
use std::{io::Read, path::Path};
use alloy::{primitives::{Address, hex}, signers::local::PrivateKeySigner};
use color_eyre::eyre::{Result, eyre};
use zeroize::Zeroizing;

pub fn read_private_key_file(path: &Path, expected: Address) -> Result<PrivateKeySigner> {
    if expected.is_zero() { return Err(eyre!("key file requires configured signer")); }
    let signer = load_private_key_file(path)?;
    if signer.address() != expected { return Err(eyre!("operator key does not match configured signer")); }
    Ok(signer)
}

/// Recover only public metadata after interrupted key-file creation, without exposing key bytes.
pub fn private_key_file_address(path: &Path) -> Result<Address> { Ok(load_private_key_file(path)?.address()) }

fn load_private_key_file(path: &Path) -> Result<PrivateKeySigner> {
    if !path.is_absolute() { return Err(eyre!("key file requires an absolute path")); }
    #[cfg(not(unix))]
    { let _ = path; return Err(eyre!("safe key-file input requires Unix file permissions")); }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        // NOFOLLOW closes the leaf-symlink race; NONBLOCK prevents a replaced FIFO from hanging.
        let mut file = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)
            .map_err(|_| eyre!("cannot safely open operator key file"))?;
        let before = file.metadata().map_err(|_| eyre!("cannot inspect operator key file"))?;
        // SAFETY: geteuid has no arguments or memory effects.
        let uid = unsafe { libc::geteuid() };
        if !before.is_file() || before.uid() != uid || before.mode() & 0o077 != 0 || before.nlink() != 1 || before.len() > 256 {
            return Err(eyre!("operator key file must be owned, private, regular, single-link and bounded"));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        (&mut file).take(257).read_to_end(&mut bytes).map_err(|_| eyre!("cannot read operator key file"))?;
        let after = file.metadata().map_err(|_| eyre!("cannot recheck operator key file"))?;
        if bytes.len() > 256 || before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            return Err(eyre!("operator key file changed or exceeds size limit"));
        }
        let mut text = bytes.as_slice().trim_ascii();
        if text.starts_with(b"0x") { text = &text[2..]; }
        if text.len() != 64 { return Err(eyre!("invalid operator key file")); }
        let mut secret = Zeroizing::new([0u8; 32]);
        hex::decode_to_slice(text, secret.as_mut()).map_err(|_| eyre!("invalid operator key file"))?;
        let signer = PrivateKeySigner::from_slice(secret.as_ref()).map_err(|_| eyre!("invalid operator key file"))?;
        Ok(signer)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn fixture(text: &[u8]) -> (tempfile::TempDir, std::path::PathBuf, Address) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test-key");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (directory, path, PrivateKeySigner::from_slice(&[1u8; 32]).unwrap().address())
    }

    #[test]
    fn private_hex_file_accepts_prefix_newline_and_exact_signer() {
        for text in ["01".repeat(32), format!(" \n0x{}\r\n", "01".repeat(32))] {
            let (_directory, path, address) = fixture(text.as_bytes());
            assert_eq!(read_private_key_file(&path, address).unwrap().address(), address);
            assert_eq!(private_key_file_address(&path).unwrap(), address);
            assert!(read_private_key_file(&path, Address::repeat_byte(7)).is_err());
        }
    }

    #[test]
    fn key_file_invalid_input_is_bounded_and_redacted() {
        for text in ["PRIVATE_KEY_SENTINEL_DO_NOT_LOG".to_string(), "00".repeat(32), "x".repeat(64), "01".repeat(129)] {
            let (_directory, path, address) = fixture(text.as_bytes());
            let error = read_private_key_file(&path, address).unwrap_err().to_string();
            assert!(!error.contains(&text));
        }
        assert!(read_private_key_file(Path::new("relative-key"), Address::repeat_byte(1)).is_err());
    }

    #[test]
    fn key_file_rejects_other_permissions_symlink_hardlink_directory_and_fifo() {
        let (directory, path, address) = fixture("01".repeat(32).as_bytes());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_key_file(&path, address).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let alias = directory.path().join("alias");
        symlink(&path, &alias).unwrap();
        assert!(read_private_key_file(&alias, address).is_err());
        std::fs::remove_file(&alias).unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(read_private_key_file(&path, address).is_err());
        std::fs::remove_file(&alias).unwrap();
        assert!(read_private_key_file(directory.path(), address).is_err());
        let fifo = directory.path().join("fifo");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        assert!(read_private_key_file(&fifo, address).is_err());
    }
}
