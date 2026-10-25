//! Signing and checking the office's session tokens.
//!
//! Consumers arrive in Task 7; drop this allow once the server is wired up.
#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use jsonwebtoken::errors::Error;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode, get_current_timestamp,
};
use serde::{Deserialize, Serialize};

const KEY_LEN: usize = 32;
const TOKEN_LIFETIME: u64 = 30 * 24 * 60 * 60;

// A loser of the create race can see the winner's file before it is written.
// These bound how long a short read waits for that write to land.
const KEY_SETTLE_ATTEMPTS: u32 = 10;
const KEY_SETTLE_PAUSE: Duration = Duration::from_millis(10);

#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
}

/// Load the HS256 signing key from `<dir>/office.key`, creating a random one
/// (32 bytes, file mode 0o600) the first time.
pub fn load_or_create_key(dir: &Path) -> io::Result<Vec<u8>> {
    fs::create_dir_all(dir)?;
    let path = dir.join("office.key");
    match read_key(&path) {
        Ok(key) => return Ok(key),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut key = [0u8; KEY_LEN];
    rand::fill(&mut key);

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&key)?;
            Ok(key.to_vec())
        }
        // Another caller created it first; use theirs rather than overwrite.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_key(&path),
        Err(error) => Err(error),
    }
}

pub fn issue(key: &[u8], subject: &str) -> Result<String, Error> {
    let claims = Claims {
        sub: subject.to_string(),
        exp: (get_current_timestamp() + TOKEN_LIFETIME) as usize,
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(key),
    )
}

pub fn verify(key: &[u8], token: &str) -> Result<Claims, Error> {
    let validation = Validation::new(Algorithm::HS256);
    decode::<Claims>(token, &DecodingKey::from_secret(key), &validation).map(|data| data.claims)
}

/// A short file is an error, never replaced: it may be a key the caller must
/// recover, and silently minting a new one would log every browser out.
fn read_key(path: &Path) -> io::Result<Vec<u8>> {
    for attempt in 0..=KEY_SETTLE_ATTEMPTS {
        let key = fs::read(path)?;
        if key.len() == KEY_LEN {
            return Ok(key);
        }
        if attempt < KEY_SETTLE_ATTEMPTS {
            thread::sleep(KEY_SETTLE_PAUSE);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "office.key is not 32 bytes",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_this_key_issued_verifies() {
        let key = b"0123456789abcdef0123456789abcdef";
        let token = issue(key, "browser").expect("issue");
        assert_eq!(verify(key, &token).expect("verify").sub, "browser");
    }

    #[test]
    fn a_token_from_another_key_is_refused() {
        let token = issue(b"0123456789abcdef0123456789abcdef", "browser").expect("issue");
        assert!(verify(b"ffffffffffffffffffffffffffffffff", &token).is_err());
    }

    #[test]
    fn an_expired_token_is_refused() {
        let key = b"0123456789abcdef0123456789abcdef";
        let claims = Claims {
            sub: "browser".into(),
            exp: 1,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(key),
        )
        .expect("issue");
        assert!(verify(key, &token).is_err());
    }

    #[test]
    fn a_tampered_token_is_refused() {
        let key = b"0123456789abcdef0123456789abcdef";
        let mut token = issue(key, "browser").expect("issue");
        token.pop();
        assert!(verify(key, &token).is_err());
    }

    #[test]
    fn the_key_is_created_once_and_stays_private() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = load_or_create_key(dir.path()).expect("create");
        assert_eq!(first.len(), 32);
        let second = load_or_create_key(dir.path()).expect("load");
        assert_eq!(second, first, "the second call must return the created key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("office.key"))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_short_key_file_is_an_error_and_is_left_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("office.key");
        std::fs::write(&path, b"short").expect("write");
        assert!(load_or_create_key(dir.path()).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), b"short");
    }
}
