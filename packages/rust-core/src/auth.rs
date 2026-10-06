//! Bearer-token file handling shared by the CLI and the Node addon.

use std::path::Path;

use anyhow::{Context, Result, ensure};

/// Minimum bearer-token length in bytes.
pub const MIN_TOKEN_BYTES: usize = 32;

/// Read a bearer token from a regular file that only its owner can access (mode 600 on Unix).
///
/// The file is opened once, without following a final symlink, and every check runs on the
/// opened handle, so the path cannot be swapped between the checks and the read. Surrounding
/// whitespace (such as a trailing newline) is trimmed.
///
/// On Windows, file permissions (ACLs) are not checked; only the regular-file and symlink checks
/// apply. Protect the token file with the user profile's ACLs there.
///
/// # Errors
///
/// Returns an error when the path is a symlink or not a regular file, is group- or
/// world-accessible (Unix), cannot be read, or does not hold exactly one whitespace-free token of
/// at least [`MIN_TOKEN_BYTES`].
pub fn read_token_file(path: &Path) -> Result<String> {
    use std::io::Read;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Fails with ELOOP on a symlink; a FIFO would block the open without O_NONBLOCK.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open a symlink itself so the handle check rejects it.
        options.custom_flags(0x0020_0000);
    }
    let mut file = options.open(path).with_context(|| {
        format!(
            "open authentication token file {} (symlinks are not accepted)",
            path.display()
        )
    })?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect authentication token file {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "authentication token path must not be a symlink"
    );
    ensure!(
        metadata.is_file(),
        "authentication token path must be a file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode().trailing_zeros() >= 6,
            "authentication token file {} must not be accessible by group or others (chmod 600)",
            path.display()
        );
    }
    let mut token = String::new();
    file.read_to_string(&mut token)
        .with_context(|| format!("read authentication token file {}", path.display()))?;
    let token = token.trim().to_owned();
    validate_token(&token)?;
    Ok(token)
}

/// Check that a bearer token is at least [`MIN_TOKEN_BYTES`] long and holds no whitespace.
pub(crate) fn validate_token(token: &str) -> Result<()> {
    ensure!(
        token.len() >= MIN_TOKEN_BYTES,
        "authentication token must be at least {MIN_TOKEN_BYTES} bytes"
    );
    ensure!(
        !token.chars().any(char::is_whitespace),
        "authentication token must not contain whitespace"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn token_file(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (directory, path)
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let (_directory, path) = token_file(&format!("  {TOKEN}\n\n"));
        assert_eq!(read_token_file(&path).unwrap(), TOKEN);
    }

    #[test]
    fn short_and_inner_whitespace_tokens_are_rejected() {
        let (_directory, path) = token_file("short\n");
        let error = read_token_file(&path).unwrap_err().to_string();
        assert!(error.contains("at least"), "{error}");
        let (_directory, path) = token_file(&format!("{TOKEN} {TOKEN}"));
        let error = read_token_file(&path).unwrap_err().to_string();
        assert!(error.contains("whitespace"), "{error}");
    }

    #[test]
    fn directory_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_token_file(directory.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn group_or_world_readable_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let (_directory, path) = token_file(TOKEN);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_token_file(&path).unwrap_err().to_string();
        assert!(error.contains("chmod 600"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_rejected_even_to_a_valid_token() {
        let (directory, path) = token_file(TOKEN);
        assert_eq!(read_token_file(&path).unwrap(), TOKEN);
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let error = format!("{:#}", read_token_file(&link).unwrap_err());
        assert!(error.contains("symlink"), "{error}");
    }
}
