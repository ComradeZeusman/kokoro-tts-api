use anyhow::{Context, Result, bail};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use rand_core::OsRng;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

fn config_path() -> PathBuf {
    std::env::var_os("KOKORO_ADMIN_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/workspace/data/admin-auth.txt"))
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("Could not hash administrator password: {}", e))?
        .to_string();

    Ok(hash)
}

fn read_config() -> Result<(String, String)> {
    let path = config_path();

    let metadata = fs::metadata(&path)
        .with_context(|| format!("Administrator configuration not found: {}", path.display()))?;

    if metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "Administrator configuration permissions are too open. Run chmod 600 on {}.",
            path.display()
        );
    }

    let contents =
        fs::read_to_string(&path).context("Could not read administrator configuration")?;

    let mut lines = contents.lines();
    let email = lines.next().unwrap_or_default().trim().to_string();
    let hash = lines.next().unwrap_or_default().trim().to_string();

    if email.is_empty() || hash.is_empty() {
        bail!("Administrator configuration is invalid.");
    }

    Ok((email, hash))
}

fn save_config(email: &str, hash: &str, create_only: bool) -> Result<()> {
    let path = config_path();

    let parent = path
        .parent()
        .context("Administrator configuration path has no parent directory")?;

    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .context("Could not secure administrator data directory")?;

    let contents = format!("{}\n{}\n", email, hash);

    if create_only {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| {
                format!(
                    "Could not create administrator configuration at {}. It may already exist.",
                    path.display()
                )
            })?;

        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    } else {
        let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
            .context("Could not create temporary administrator configuration")?;

        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);

        fs::rename(&temp_path, &path).context("Could not replace administrator configuration")?;
    }

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .context("Could not secure administrator configuration")?;

    Ok(())
}

fn prompt_new_password() -> Result<String> {
    let password = rpassword::prompt_password("New administrator password: ")?;

    if password.len() < 12 {
        bail!("Password must contain at least 12 characters.");
    }

    let confirmation = rpassword::prompt_password("Confirm new password: ")?;

    if password != confirmation {
        bail!("Passwords do not match.");
    }

    Ok(password)
}

/// Prompt for a customer password without echoing it.
pub fn prompt_customer_password() -> Result<String> {
    let password = rpassword::prompt_password("Initial customer password: ")?;

    if password.len() < 12 {
        bail!("Password must contain at least 12 characters.");
    }

    let confirmation = rpassword::prompt_password("Confirm customer password: ")?;

    if password != confirmation {
        bail!("Passwords do not match.");
    }

    Ok(password)
}

/// Hash a customer password using Argon2.
pub fn hash_customer_password(password: &str) -> Result<String> {
    hash_password(password)
}

/// Verify a password against an existing Argon2 password hash.
pub fn verify_password_hash(password: &str, stored_hash: &str) -> Result<bool> {
    let parsed_hash = PasswordHash::new(stored_hash)
        .map_err(|e| anyhow::anyhow!("Stored password hash is invalid: {}", e))?;

    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok())
}

pub fn setup(email: &str) -> Result<()> {
    let email = email.trim().to_lowercase();

    if !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        bail!("Please provide a valid administrator email address.");
    }

    let path = config_path();

    if path.exists() {
        bail!("An administrator is already configured. Use change-password instead.");
    }

    println!("Setting up Kokoro CLI administrator: {}", email);

    let password = prompt_new_password()?;
    let hash = hash_password(&password)?;

    save_config(&email, &hash, true)?;

    println!("Administrator configured successfully.");
    println!("Configuration: {}", path.display());

    Ok(())
}

/// Verify administrator credentials for browser or CLI authentication.
pub fn verify_credentials(email: &str, password: &str) -> Result<bool> {
    let (stored_email, stored_hash) = read_config()?;

    let parsed_hash = PasswordHash::new(&stored_hash)
        .map_err(|e| anyhow::anyhow!("Stored administrator password hash is invalid: {}", e))?;

    let password_valid = Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok();

    Ok(email.trim().eq_ignore_ascii_case(&stored_email) && password_valid)
}

pub fn authenticate() -> Result<()> {
    let email = rpassword::prompt_password("Administrator email: ")?
        .trim()
        .to_lowercase();

    let password = rpassword::prompt_password("Administrator password: ")?;

    if !verify_credentials(&email, &password)? {
        bail!("Authentication failed.");
    }

    Ok(())
}

pub fn change_password() -> Result<()> {
    let (email, _) = read_config()?;

    println!("Changing password for {}", email);

    let password = prompt_new_password()?;
    let hash = hash_password(&password)?;

    save_config(&email, &hash, false)?;

    println!("Administrator password changed successfully.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_accepts_correct_password() {
        let password = "VoiceMW-Test-Password-123";
        let hash = hash_customer_password(password).unwrap();

        assert!(verify_password_hash(password, &hash).unwrap());
    }

    #[test]
    fn password_hash_rejects_incorrect_password() {
        let hash = hash_customer_password("VoiceMW-Test-Password-123").unwrap();

        assert!(!verify_password_hash("Wrong-Password-456", &hash).unwrap());
    }

    #[test]
    fn password_hash_rejects_malformed_hash() {
        assert!(verify_password_hash("Some-Password-123", "not-a-valid-hash").is_err());
    }
}
