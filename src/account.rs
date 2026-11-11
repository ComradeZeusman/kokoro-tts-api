use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration, Months, Utc};
use rand::random;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct AccountStore {
    path: PathBuf,
}

#[derive(serde::Serialize)]
pub struct Registration {
    pub account_id: String,
    pub api_key: String,
}

#[derive(serde::Serialize)]
pub struct AuthenticatedAccount {
    pub account_id: String,
    pub status: String,
}

#[derive(serde::Serialize)]
pub struct QuotaStatus {
    pub subscription_id: i64,
    pub character_limit: i64,
    pub characters_used: i64,
    pub characters_remaining: i64,
    pub expires_at: i64,
}

#[derive(serde::Serialize)]
pub struct AccountSummary {
    pub id: String,
    pub name: String,
    pub email: String,
    pub status: String,
    pub created_at: i64,
}

#[derive(serde::Serialize)]
pub struct SubscriptionSummary {
    pub id: i64,
    pub starts_at: i64,
    pub expires_at: i64,
    pub character_limit: i64,
    pub characters_used: i64,
    pub stt_limit_ms: i64,
    pub stt_used_ms: i64,
    pub status: String,
}

#[derive(serde::Serialize)]
pub struct AccountDetails {
    pub account: AccountSummary,
    pub subscription: Option<SubscriptionSummary>,
    pub active_api_keys: i64,
}

#[derive(Clone, Copy)]
pub enum SubscriptionDuration {
    Days(u32),
    Months(u32),
}

impl AccountStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let store = Self { path };
        store.initialize()?;
        Ok(store)
    }

    fn connection(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(conn)
    }

    fn initialize(&self) -> Result<()> {
        let conn = self.connection()?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS accounts (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                email       TEXT NOT NULL COLLATE NOCASE UNIQUE,
                status      TEXT NOT NULL DEFAULT 'pending'
                            CHECK(status IN ('pending', 'active', 'suspended')),
                created_at  INTEGER NOT NULL,
                updated_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS api_keys (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id   TEXT NOT NULL REFERENCES accounts(id),
                key_prefix   TEXT NOT NULL,
                key_hash     TEXT NOT NULL UNIQUE,
                created_at   INTEGER NOT NULL,
                last_used_at INTEGER,
                revoked_at   INTEGER
            );

            CREATE TABLE IF NOT EXISTS subscriptions (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id      TEXT NOT NULL REFERENCES accounts(id),
                starts_at       INTEGER NOT NULL,
                expires_at      INTEGER NOT NULL,
                character_limit INTEGER NOT NULL CHECK(character_limit > 0),
                stt_limit_ms    INTEGER NOT NULL DEFAULT 0 CHECK(stt_limit_ms >= 0),
                status          TEXT NOT NULL DEFAULT 'active'
                                CHECK(status IN ('active', 'expired', 'cancelled')),
                created_at      INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_subscriptions_account
                ON subscriptions(account_id, status, expires_at);

            CREATE TABLE IF NOT EXISTS usage_events (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id      TEXT NOT NULL REFERENCES accounts(id),
                subscription_id INTEGER NOT NULL REFERENCES subscriptions(id),
                characters      INTEGER NOT NULL CHECK(characters > 0),
                format          TEXT NOT NULL,
                voice           TEXT NOT NULL,
                speed           REAL NOT NULL,
                synthesis_ms    INTEGER NOT NULL,
                created_at      INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_usage_subscription
                ON usage_events(subscription_id);

            CREATE TABLE IF NOT EXISTS stt_usage_events (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id      TEXT NOT NULL REFERENCES accounts(id),
                subscription_id INTEGER NOT NULL REFERENCES subscriptions(id),
                audio_ms        INTEGER NOT NULL CHECK(audio_ms > 0),
                language        TEXT NOT NULL,
                task            TEXT NOT NULL,
                processing_ms   INTEGER NOT NULL DEFAULT 0,
                created_at      INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_stt_usage_subscription
                ON stt_usage_events(subscription_id);

            CREATE TABLE IF NOT EXISTS audit_events (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT,
                action     TEXT NOT NULL,
                details    TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS customer_credentials (
                account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
                password_hash TEXT NOT NULL,
                must_change_password INTEGER NOT NULL DEFAULT 1
                    CHECK(must_change_password IN (0, 1)),
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS web_sessions (
                token_hash TEXT PRIMARY KEY,
                principal_type TEXT NOT NULL
                    CHECK(principal_type IN ('customer', 'admin')),
                principal_id TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                revoked_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_web_sessions_principal
                ON web_sessions(principal_type, principal_id, expires_at);
            ",
        )?;

        // Migrate existing databases created before STT quota support.
        let has_stt_limit = {
            let mut stmt = conn.prepare("PRAGMA table_info(subscriptions)")?;
            let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;

            for column in columns {
                if column? == "stt_limit_ms" {
                    found = true;
                    break;
                }
            }
            found
        };

        if !has_stt_limit {
            conn.execute(
                "ALTER TABLE subscriptions
                 ADD COLUMN stt_limit_ms INTEGER NOT NULL DEFAULT 0
                 CHECK(stt_limit_ms >= 0)",
                [],
            )?;
        }

        Ok(())
    }

    /// List all registered accounts, newest first.
    pub fn list_accounts(&self) -> Result<Vec<AccountSummary>> {
        let conn = self.connection()?;

        let mut stmt = conn.prepare(
            "SELECT id, name, email, status, created_at
             FROM accounts
             ORDER BY created_at DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(AccountSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                email: row.get(2)?,
                status: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;

        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(row?);
        }

        Ok(accounts)
    }

    /// Retrieve an account by email address, ignoring email letter case.
    pub fn get_account_by_email(&self, email: &str) -> Result<Option<AccountDetails>> {
        let email = email.trim();
        if email.is_empty() {
            return Ok(None);
        }

        let conn = self.connection()?;
        let account_id = conn
            .query_row(
                "SELECT id FROM accounts WHERE email = ?1 COLLATE NOCASE",
                params![email],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        drop(conn);

        match account_id {
            Some(account_id) => self.get_account(&account_id),
            None => Ok(None),
        }
    }

    /// Retrieve one account and its latest subscription.
    pub fn get_account(&self, account_id: &str) -> Result<Option<AccountDetails>> {
        let conn = self.connection()?;

        let account = conn
            .query_row(
                "SELECT id, name, email, status, created_at
             FROM accounts
             WHERE id = ?1",
                params![account_id],
                |row| {
                    Ok(AccountSummary {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        email: row.get(2)?,
                        status: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )
            .optional()?;

        let Some(account) = account else {
            return Ok(None);
        };

        let subscription = conn
            .query_row(
                "SELECT id, starts_at, expires_at, character_limit, stt_limit_ms, status
             FROM subscriptions
             WHERE account_id = ?1
             ORDER BY created_at DESC
             LIMIT 1",
                params![account_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;

        let subscription =
            if let Some((id, starts_at, expires_at, character_limit, stt_limit_ms, status)) =
                subscription
            {
                let characters_used: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(characters), 0)
                 FROM usage_events
                 WHERE subscription_id = ?1",
                    params![id],
                    |row| row.get(0),
                )?;

                let stt_used_ms: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(audio_ms), 0)
                 FROM stt_usage_events
                 WHERE subscription_id = ?1",
                    params![id],
                    |row| row.get(0),
                )?;

                Some(SubscriptionSummary {
                    id,
                    starts_at,
                    expires_at,
                    character_limit,
                    characters_used,
                    stt_limit_ms,
                    stt_used_ms,
                    status,
                })
            } else {
                None
            };

        let active_api_keys: i64 = conn.query_row(
            "SELECT COUNT(*)
             FROM api_keys
             WHERE account_id = ?1 AND revoked_at IS NULL",
            params![account_id],
            |row| row.get(0),
        )?;

        Ok(Some(AccountDetails {
            account,
            subscription,
            active_api_keys,
        }))
    }

    pub fn register(&self, name: &str, email: &str) -> Result<Registration> {
        let name = name.trim();
        let email = email.trim().to_ascii_lowercase();

        if name.is_empty() || name.len() > 120 {
            bail!("Name must contain between 1 and 120 bytes.");
        }

        if email.is_empty() || email.len() > 254 || !email.contains('@') {
            bail!("A valid email address is required.");
        }

        let now = unix_time()?;
        let account_id = format!("KKR-{}", hex::encode(random::<[u8; 8]>()).to_uppercase());
        let api_key = format!("kk_live_{}", hex::encode(random::<[u8; 32]>()));
        let key_prefix = api_key.chars().take(16).collect::<String>();
        let key_hash = hash_api_key(&api_key);

        let mut conn = self.connection()?;
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO accounts (id, name, email, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?4)",
            params![account_id, name, email, now],
        )?;

        tx.execute(
            "INSERT INTO api_keys (account_id, key_prefix, key_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, key_prefix, key_hash, now],
        )?;

        tx.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'account_registered', 'Pending registration', ?2)",
            params![account_id, now],
        )?;

        tx.commit()?;

        Ok(Registration {
            account_id,
            api_key,
        })
    }

    /// Create a customer account, API key, and initial password atomically.
    pub fn create_customer_account(
        &self,
        name: &str,
        email: &str,
        password_hash: &str,
    ) -> Result<Registration> {
        let name = name.trim();
        let email = email.trim().to_ascii_lowercase();

        if name.is_empty() || name.len() > 120 {
            bail!("Name must contain between 1 and 120 bytes.");
        }

        if email.is_empty() || email.len() > 254 || !email.contains('@') {
            bail!("A valid email address is required.");
        }

        if password_hash.trim().is_empty() {
            bail!("Password hash cannot be empty.");
        }

        let now = unix_time()?;
        let account_id = format!("KKR-{}", hex::encode(random::<[u8; 8]>()).to_uppercase());
        let api_key = format!("kk_live_{}", hex::encode(random::<[u8; 32]>()));
        let key_prefix = api_key.chars().take(16).collect::<String>();
        let key_hash = hash_api_key(&api_key);

        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        tx.execute(
            "INSERT INTO accounts (id, name, email, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?4)",
            params![account_id, name, email, now],
        )?;

        tx.execute(
            "INSERT INTO api_keys (account_id, key_prefix, key_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, key_prefix, key_hash, now],
        )?;

        tx.execute(
            "INSERT INTO customer_credentials
                (account_id, password_hash, must_change_password, updated_at)
             VALUES (?1, ?2, 1, ?3)",
            params![account_id, password_hash, now],
        )?;

        tx.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'account_registered',
                     'Customer account created by administrator', ?2)",
            params![account_id, now],
        )?;

        tx.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'customer_password_set',
                     'Initial customer password configured', ?2)",
            params![account_id, now],
        )?;

        tx.commit()?;

        Ok(Registration {
            account_id,
            api_key,
        })
    }

    /// Authenticate an API key without requiring the account to be active.
    /// Useful for account-status and usage endpoints.
    pub fn authenticate_any_status(&self, api_key: &str) -> Result<AuthenticatedAccount> {
        if !api_key.starts_with("kk_live_") {
            bail!("Invalid API key.");
        }

        let conn = self.connection()?;
        let key_hash = hash_api_key(api_key);

        let account = conn
            .query_row(
                "
                SELECT a.id, a.status
                FROM api_keys k
                JOIN accounts a ON a.id = k.account_id
                WHERE k.key_hash = ?1
                  AND k.revoked_at IS NULL
                ",
                params![key_hash],
                |row| {
                    Ok(AuthenticatedAccount {
                        account_id: row.get(0)?,
                        status: row.get(1)?,
                    })
                },
            )
            .optional()?;

        account.ok_or_else(|| anyhow!("Invalid or revoked API key."))
    }

    /// Authenticate an API key and require an active account.
    pub fn authenticate(&self, api_key: &str) -> Result<AuthenticatedAccount> {
        let account = self.authenticate_any_status(api_key)?;

        if account.status != "active" {
            bail!("Account is not active: {}", account.status);
        }

        Ok(account)
    }

    pub fn check_quota(&self, account_id: &str, requested_chars: i64) -> Result<QuotaStatus> {
        if requested_chars <= 0 {
            bail!("Requested character count must be positive.");
        }

        let conn = self.connection()?;
        let now = unix_time()?;

        let subscription: Option<(i64, i64, i64)> = conn
            .query_row(
                "
                SELECT id, character_limit, expires_at
                FROM subscriptions
                WHERE account_id = ?1
                  AND status = 'active'
                  AND starts_at <= ?2
                  AND expires_at > ?2
                ORDER BY expires_at DESC
                LIMIT 1
                ",
                params![account_id, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        let (subscription_id, character_limit, expires_at) =
            subscription.ok_or_else(|| anyhow!("No active subscription."))?;

        let characters_used: i64 = conn.query_row(
            "SELECT COALESCE(SUM(characters), 0)
             FROM usage_events
             WHERE subscription_id = ?1",
            params![subscription_id],
            |row| row.get(0),
        )?;

        let characters_remaining = character_limit - characters_used;

        if characters_remaining < requested_chars {
            bail!(
                "Character quota exhausted. Remaining: {}, requested: {}.",
                characters_remaining.max(0),
                requested_chars
            );
        }

        Ok(QuotaStatus {
            subscription_id,
            character_limit,
            characters_used,
            characters_remaining,
            expires_at,
        })
    }

    /// Record usage only after synthesis has succeeded.
    /// The quota is checked again inside an IMMEDIATE transaction.
    pub fn record_usage(
        &self,
        account_id: &str,
        characters: i64,
        format: &str,
        voice: &str,
        speed: f64,
        synthesis_ms: i64,
    ) -> Result<()> {
        if characters <= 0 {
            bail!("Character count must be positive.");
        }

        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let now = unix_time()?;

        let account_status: String = tx.query_row(
            "SELECT status FROM accounts WHERE id = ?1",
            params![account_id],
            |row| row.get(0),
        )?;

        if account_status != "active" {
            bail!("Account is not active.");
        }

        let (subscription_id, character_limit): (i64, i64) = tx
            .query_row(
                "
                SELECT id, character_limit
                FROM subscriptions
                WHERE account_id = ?1
                  AND status = 'active'
                  AND starts_at <= ?2
                  AND expires_at > ?2
                ORDER BY expires_at DESC
                LIMIT 1
                ",
                params![account_id, now],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("No active subscription."))?;

        let used: i64 = tx.query_row(
            "SELECT COALESCE(SUM(characters), 0)
             FROM usage_events
             WHERE subscription_id = ?1",
            params![subscription_id],
            |row| row.get(0),
        )?;

        if used + characters > character_limit {
            bail!("Character quota exhausted.");
        }

        tx.execute(
            "
            INSERT INTO usage_events
                (account_id, subscription_id, characters, format, voice, speed,
                 synthesis_ms, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
            params![
                account_id,
                subscription_id,
                characters,
                format,
                voice,
                speed,
                synthesis_ms,
                now
            ],
        )?;

        tx.execute(
            "UPDATE api_keys SET last_used_at = ?1
             WHERE account_id = ?2 AND revoked_at IS NULL",
            params![now, account_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Record STT usage only after transcription succeeds.
    /// The audio-duration quota is rechecked inside an IMMEDIATE transaction.
    pub fn record_stt_usage(
        &self,
        account_id: &str,
        audio_ms: i64,
        language: &str,
        task: &str,
        processing_ms: i64,
    ) -> Result<()> {
        if audio_ms <= 0 {
            bail!("Audio duration must be positive.");
        }

        if processing_ms < 0 {
            bail!("Processing duration cannot be negative.");
        }

        let language = language.trim();
        if language.is_empty() || language.len() > 32 {
            bail!("Language must contain between 1 and 32 bytes.");
        }

        if task != "transcribe" && task != "translate" {
            bail!("Task must be either transcribe or translate.");
        }

        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let now = unix_time()?;

        let account_status: String = tx.query_row(
            "SELECT status FROM accounts WHERE id = ?1",
            params![account_id],
            |row| row.get(0),
        )?;

        if account_status != "active" {
            bail!("Account is not active.");
        }

        let (subscription_id, stt_limit_ms): (i64, i64) = tx
            .query_row(
                "
                SELECT id, stt_limit_ms
                FROM subscriptions
                WHERE account_id = ?1
                  AND status = 'active'
                  AND starts_at <= ?2
                  AND expires_at > ?2
                ORDER BY expires_at DESC
                LIMIT 1
                ",
                params![account_id, now],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("No active subscription."))?;

        if stt_limit_ms <= 0 {
            bail!("STT is not enabled for this subscription.");
        }

        let used_ms: i64 = tx.query_row(
            "SELECT COALESCE(SUM(audio_ms), 0)
             FROM stt_usage_events
             WHERE subscription_id = ?1",
            params![subscription_id],
            |row| row.get(0),
        )?;

        let new_total = used_ms
            .checked_add(audio_ms)
            .ok_or_else(|| anyhow!("STT usage total overflow."))?;

        if new_total > stt_limit_ms {
            bail!(
                "STT audio quota exhausted. Remaining: {} ms, requested: {} ms.",
                (stt_limit_ms - used_ms).max(0),
                audio_ms
            );
        }

        tx.execute(
            "
            INSERT INTO stt_usage_events
                (account_id, subscription_id, audio_ms, language, task,
                 processing_ms, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
            params![
                account_id,
                subscription_id,
                audio_ms,
                language,
                task,
                processing_ms,
                now
            ],
        )?;

        tx.execute(
            "UPDATE api_keys SET last_used_at = ?1
             WHERE account_id = ?2 AND revoked_at IS NULL",
            params![now, account_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Store a customer's password hash and revoke their existing browser sessions.
    pub fn set_customer_password(
        &self,
        account_id: &str,
        password_hash: &str,
        must_change_password: bool,
    ) -> Result<()> {
        if account_id.trim().is_empty() {
            bail!("Account ID cannot be empty.");
        }

        if password_hash.trim().is_empty() {
            bail!("Password hash cannot be empty.");
        }

        let now = unix_time()?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        tx.execute(
            "
            INSERT INTO customer_credentials
                (account_id, password_hash, must_change_password, updated_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(account_id) DO UPDATE SET
                password_hash = excluded.password_hash,
                must_change_password = excluded.must_change_password,
                updated_at = excluded.updated_at
            ",
            params![account_id, password_hash, must_change_password as i64, now],
        )?;

        tx.execute(
            "
            UPDATE web_sessions
            SET revoked_at = ?1
            WHERE principal_type = 'customer'
              AND principal_id = ?2
              AND revoked_at IS NULL
            ",
            params![now, account_id],
        )?;

        tx.execute(
            "
            INSERT INTO audit_events (account_id, action, details, created_at)
            VALUES (?1, 'customer_password_set', 'Customer password updated', ?2)
            ",
            params![account_id, now],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Retrieve a customer's password hash and mandatory-change flag.
    pub fn get_customer_credentials(&self, account_id: &str) -> Result<Option<(String, bool)>> {
        let conn = self.connection()?;

        let credentials = conn
            .query_row(
                "
                SELECT password_hash, must_change_password
                FROM customer_credentials
                WHERE account_id = ?1
                ",
                params![account_id],
                |row| {
                    let hash: String = row.get(0)?;
                    let must_change: i64 = row.get(1)?;
                    Ok((hash, must_change != 0))
                },
            )
            .optional()?;

        Ok(credentials)
    }

    /// Create a browser session and return its raw token exactly once.
    pub fn create_web_session(
        &self,
        principal_type: &str,
        principal_id: &str,
        lifetime_seconds: i64,
    ) -> Result<String> {
        if principal_type != "customer" && principal_type != "admin" {
            bail!("Invalid session principal type.");
        }

        if principal_id.trim().is_empty() {
            bail!("Session principal ID cannot be empty.");
        }

        if lifetime_seconds <= 0 {
            bail!("Session lifetime must be positive.");
        }

        let now = unix_time()?;
        let expires_at = now
            .checked_add(lifetime_seconds)
            .ok_or_else(|| anyhow!("Session expiry overflow."))?;

        // Two independent random 128-bit values produce a 256-bit token.
        let token = format!("{:032x}{:032x}", random::<u128>(), random::<u128>());
        let token_hash = hex::encode(Sha256::digest(token.as_bytes()));

        let conn = self.connection()?;
        conn.execute(
            "
            INSERT INTO web_sessions
                (token_hash, principal_type, principal_id, expires_at, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ",
            params![token_hash, principal_type, principal_id, expires_at, now],
        )?;

        Ok(token)
    }

    /// Authenticate a browser session without exposing its stored token hash.
    pub fn authenticate_web_session(&self, token: &str) -> Result<Option<(String, String)>> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(None);
        }

        let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
        let now = unix_time()?;
        let conn = self.connection()?;

        let session = conn
            .query_row(
                "
                SELECT principal_type, principal_id
                FROM web_sessions
                WHERE token_hash = ?1
                  AND expires_at > ?2
                  AND revoked_at IS NULL
                ",
                params![token_hash, now],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        Ok(session)
    }

    /// Revoke a browser session by its raw token.
    pub fn revoke_web_session(&self, token: &str) -> Result<bool> {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(false);
        }

        let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
        let now = unix_time()?;
        let conn = self.connection()?;

        let changed = conn.execute(
            "
            UPDATE web_sessions
            SET revoked_at = ?1
            WHERE token_hash = ?2 AND revoked_at IS NULL
            ",
            params![now, token_hash],
        )?;

        Ok(changed > 0)
    }

    /// Revoke every active administrator browser session.
    pub fn revoke_all_admin_sessions(&self) -> Result<usize> {
        let now = unix_time()?;
        let conn = self.connection()?;

        let changed = conn.execute(
            "UPDATE web_sessions
             SET revoked_at = ?1
             WHERE principal_type = 'admin' AND revoked_at IS NULL",
            params![now],
        )?;

        Ok(changed)
    }

    /// Activate an account using the original TTS-only subscription settings.
    pub fn activate(
        &self,
        account_id: &str,
        duration: SubscriptionDuration,
        character_limit: i64,
    ) -> Result<()> {
        self.activate_with_stt(account_id, duration, character_limit, 0)
    }

    /// Activate an account and create a subscription with TTS and STT allowances.
    pub fn activate_with_stt(
        &self,
        account_id: &str,
        duration: SubscriptionDuration,
        character_limit: i64,
        stt_limit_ms: i64,
    ) -> Result<()> {
        if character_limit <= 0 {
            bail!("Character limit must be positive.");
        }

        if stt_limit_ms < 0 {
            bail!("STT limit cannot be negative.");
        }

        let now = unix_time()?;
        let expires_at = calculate_expiry(now, duration)?;

        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let changed = tx.execute(
            "UPDATE accounts SET status = 'active', updated_at = ?1 WHERE id = ?2",
            params![now, account_id],
        )?;

        if changed == 0 {
            bail!("Account not found: {}", account_id);
        }

        tx.execute(
            "UPDATE subscriptions SET status = 'expired'
             WHERE account_id = ?1 AND status = 'active'",
            params![account_id],
        )?;

        tx.execute(
            "INSERT INTO subscriptions
                (account_id, starts_at, expires_at, character_limit,
                 stt_limit_ms, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?2)",
            params![account_id, now, expires_at, character_limit, stt_limit_ms],
        )?;

        tx.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'subscription_activated', ?2, ?3)",
            params![
                account_id,
                format!(
                    "Limit: {} characters; STT: {} ms; expires: {}",
                    character_limit, stt_limit_ms, expires_at
                ),
                now
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Extend a subscription without changing its existing STT allowance.
    pub fn extend_subscription(
        &self,
        account_id: &str,
        duration: SubscriptionDuration,
        additional_characters: i64,
    ) -> Result<()> {
        self.extend_subscription_with_stt(account_id, duration, additional_characters, 0)
    }

    /// Extend a subscription and add to its TTS and STT allowances.
    pub fn extend_subscription_with_stt(
        &self,
        account_id: &str,
        duration: SubscriptionDuration,
        additional_characters: i64,
        additional_stt_ms: i64,
    ) -> Result<()> {
        if additional_characters < 0 {
            bail!("Additional characters cannot be negative.");
        }

        if additional_stt_ms < 0 {
            bail!("Additional STT allowance cannot be negative.");
        }

        let now = unix_time()?;
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let subscription: Option<(i64, i64, i64, i64)> = tx
            .query_row(
                "SELECT id, expires_at, character_limit, stt_limit_ms
                 FROM subscriptions
                 WHERE account_id = ?1 AND status = 'active'
                 ORDER BY expires_at DESC
                 LIMIT 1",
                params![account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        let (subscription_id, old_expiry, old_limit, old_stt_limit) =
            subscription.ok_or_else(|| anyhow!("No subscription exists to extend."))?;

        let base = old_expiry.max(now);
        let new_expiry = calculate_expiry(base, duration)?;
        let new_limit = old_limit
            .checked_add(additional_characters)
            .ok_or_else(|| anyhow!("Character limit overflow."))?;

        let new_stt_limit = old_stt_limit
            .checked_add(additional_stt_ms)
            .ok_or_else(|| anyhow!("STT allowance overflow."))?;

        tx.execute(
            "UPDATE subscriptions
             SET expires_at = ?1, character_limit = ?2, stt_limit_ms = ?3
             WHERE id = ?4",
            params![new_expiry, new_limit, new_stt_limit, subscription_id],
        )?;

        tx.execute(
            "UPDATE accounts SET status = 'active', updated_at = ?1 WHERE id = ?2",
            params![now, account_id],
        )?;

        tx.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'subscription_extended', ?2, ?3)",
            params![
                account_id,
                format!(
                    "New expiry: {}; additional characters: {}; additional STT: {} ms",
                    new_expiry, additional_characters, additional_stt_ms
                ),
                now
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    pub fn set_account_status(&self, account_id: &str, status: &str) -> Result<()> {
        if !matches!(status, "active" | "suspended") {
            bail!("Status must be either 'active' or 'suspended'.");
        }

        let now = unix_time()?;
        let conn = self.connection()?;

        let changed = conn.execute(
            "UPDATE accounts SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status, now, account_id],
        )?;

        if changed == 0 {
            bail!("Account not found: {}", account_id);
        }

        conn.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'account_status_changed', ?2, ?3)",
            params![account_id, status, now],
        )?;

        Ok(())
    }

    /// Revoke all currently valid API keys for an account.
    pub fn revoke_api_keys(&self, account_id: &str) -> Result<usize> {
        let now = unix_time()?;
        let conn = self.connection()?;

        let changed = conn.execute(
            "UPDATE api_keys SET revoked_at = ?1
             WHERE account_id = ?2 AND revoked_at IS NULL",
            params![now, account_id],
        )?;

        conn.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'api_keys_revoked', ?2, ?3)",
            params![account_id, format!("{} key(s) revoked", changed), now],
        )?;

        Ok(changed)
    }

    /// Revoke existing keys and issue a new key. The plaintext is returned once.
    pub fn rotate_api_key(&self, account_id: &str) -> Result<String> {
        let now = unix_time()?;
        let api_key = format!("kk_live_{}", hex::encode(random::<[u8; 32]>()));
        let key_prefix = api_key.chars().take(16).collect::<String>();
        let key_hash = hash_api_key(&api_key);

        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
            params![account_id],
            |row| row.get(0),
        )?;

        if !exists {
            bail!("Account not found: {}", account_id);
        }

        tx.execute(
            "UPDATE api_keys SET revoked_at = ?1
             WHERE account_id = ?2 AND revoked_at IS NULL",
            params![now, account_id],
        )?;

        tx.execute(
            "INSERT INTO api_keys (account_id, key_prefix, key_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![account_id, key_prefix, key_hash, now],
        )?;

        tx.execute(
            "INSERT INTO audit_events (account_id, action, details, created_at)
             VALUES (?1, 'api_key_rotated', 'New API key issued', ?2)",
            params![account_id, now],
        )?;

        tx.commit()?;
        Ok(api_key)
    }
}

fn calculate_expiry(base: i64, duration: SubscriptionDuration) -> Result<i64> {
    let date = DateTime::<Utc>::from_timestamp(base, 0)
        .ok_or_else(|| anyhow!("Invalid subscription timestamp."))?;

    let expiry = match duration {
        SubscriptionDuration::Days(days) if days > 0 => {
            date.checked_add_signed(Duration::days(i64::from(days)))
        }
        SubscriptionDuration::Months(months) if months > 0 => {
            date.checked_add_months(Months::new(months))
        }
        _ => bail!("Subscription duration must be greater than zero."),
    }
    .ok_or_else(|| anyhow!("Subscription expiry is out of range."))?;

    Ok(expiry.timestamp())
}

fn hash_api_key(api_key: &str) -> String {
    hex::encode(Sha256::digest(api_key.as_bytes()))
}

fn unix_time() -> Result<i64> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> AccountStore {
        let path = std::env::temp_dir().join(format!(
            "kokoro-account-test-{}-{}.db",
            std::process::id(),
            random::<u64>()
        ));

        AccountStore::new(path).expect("create test database")
    }

    #[test]
    fn atomic_customer_creation_stores_all_credentials() {
        let store = test_store();

        let registration = store
            .create_customer_account("Atomic Customer", "atomic@example.com", "argon2-test-hash")
            .expect("atomic customer creation should succeed");

        assert!(registration.account_id.starts_with("KKR-"));
        assert!(registration.api_key.starts_with("kk_live_"));

        let credentials = store
            .get_customer_credentials(&registration.account_id)
            .expect("credential lookup should succeed")
            .expect("credentials should exist");

        assert_eq!(credentials.0, "argon2-test-hash");
        assert!(credentials.1);

        let authenticated = store
            .authenticate_any_status(&registration.api_key)
            .expect("issued API key should authenticate");

        assert_eq!(authenticated.account_id, registration.account_id);
        assert_eq!(authenticated.status, "pending");
    }

    #[test]
    fn atomic_customer_creation_rolls_back_on_password_insert_failure() {
        let store = test_store();

        let conn = store.connection().expect("database connection");
        conn.execute_batch(
            "
            CREATE TRIGGER reject_customer_credentials
            BEFORE INSERT ON customer_credentials
            BEGIN
                SELECT RAISE(ABORT, 'simulated credential storage failure');
            END;
            ",
        )
        .expect("failure trigger should be installed");
        drop(conn);

        assert!(
            store
                .create_customer_account(
                    "Rollback Customer",
                    "rollback@example.com",
                    "argon2-test-hash",
                )
                .is_err(),
            "creation should fail when password storage fails"
        );

        let conn = store.connection().expect("database connection");

        let account_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE email = ?1",
                params!["rollback@example.com"],
                |row| row.get(0),
            )
            .expect("account count query");

        let key_count: i64 = conn
            .query_row(
                "SELECT COUNT(*)
                 FROM api_keys
                 WHERE account_id IN (
                     SELECT id FROM accounts WHERE email = ?1
                 )",
                params!["rollback@example.com"],
                |row| row.get(0),
            )
            .expect("API key count query");

        assert_eq!(account_count, 0, "partial account must be rolled back");
        assert_eq!(key_count, 0, "partial API key must be rolled back");
    }

    #[test]
    fn customer_password_storage_and_session_invalidation() {
        let store = test_store();

        let registration = store
            .register("Password Customer", "password@example.com")
            .expect("registration should succeed");

        let first_session = store
            .create_web_session("customer", &registration.account_id, 3600)
            .expect("session creation should succeed");

        assert!(
            store
                .authenticate_web_session(&first_session)
                .unwrap()
                .is_some(),
            "new session should authenticate"
        );

        store
            .set_customer_password(&registration.account_id, "argon2-test-hash", true)
            .expect("password should be stored");

        let credentials = store
            .get_customer_credentials(&registration.account_id)
            .expect("credential lookup should succeed")
            .expect("credentials should exist");

        assert_eq!(credentials.0, "argon2-test-hash");
        assert!(credentials.1, "initial password change should be required");

        assert!(
            store
                .authenticate_web_session(&first_session)
                .unwrap()
                .is_none(),
            "password change should invalidate existing sessions"
        );

        store
            .set_customer_password(&registration.account_id, "new-argon2-test-hash", false)
            .expect("password replacement should succeed");

        let updated = store
            .get_customer_credentials(&registration.account_id)
            .unwrap()
            .unwrap();

        assert_eq!(updated.0, "new-argon2-test-hash");
        assert!(!updated.1, "mandatory password change should be cleared");
    }

    #[test]
    fn account_lookup_by_email_is_case_insensitive() {
        let store = test_store();

        let registration = store
            .register("Lookup Customer", "lookup@example.com")
            .expect("registration should succeed");

        let account = store
            .get_account_by_email("  LOOKUP@EXAMPLE.COM  ")
            .expect("email lookup should succeed")
            .expect("account should exist");

        assert_eq!(account.account.id, registration.account_id);
        assert_eq!(account.account.email, "lookup@example.com");

        assert!(
            store
                .get_account_by_email("missing@example.com")
                .unwrap()
                .is_none(),
            "unknown email should return no account"
        );

        assert!(
            store.get_account_by_email("   ").unwrap().is_none(),
            "empty email should return no account"
        );
    }

    #[test]
    fn revoking_admin_sessions_does_not_revoke_customer_sessions() {
        let store = test_store();

        let registration = store
            .register("Session Customer", "admin-revoke@example.com")
            .expect("registration should succeed");

        let admin_session = store
            .create_web_session("admin", "admin@example.com", 3600)
            .expect("admin session creation should succeed");

        let customer_session = store
            .create_web_session("customer", &registration.account_id, 3600)
            .expect("customer session creation should succeed");

        assert_eq!(
            store
                .revoke_all_admin_sessions()
                .expect("admin revocation should succeed"),
            1
        );

        assert!(
            store
                .authenticate_web_session(&admin_session)
                .unwrap()
                .is_none(),
            "admin session should be revoked"
        );

        assert!(
            store
                .authenticate_web_session(&customer_session)
                .unwrap()
                .is_some(),
            "customer session should remain valid"
        );
    }

    #[test]
    fn web_sessions_authenticate_and_revoke() {
        let store = test_store();

        let registration = store
            .register("Session Customer", "session@example.com")
            .expect("registration should succeed");

        let token = store
            .create_web_session("customer", &registration.account_id, 3600)
            .expect("session creation should succeed");

        let principal = store
            .authenticate_web_session(&token)
            .expect("session lookup should succeed")
            .expect("session should be valid");

        assert_eq!(principal.0, "customer");
        assert_eq!(principal.1, registration.account_id);

        assert!(
            store
                .authenticate_web_session("invalid-token")
                .unwrap()
                .is_none(),
            "malformed tokens should be rejected"
        );

        assert!(
            store
                .revoke_web_session(&token)
                .expect("revocation should succeed"),
            "first revocation should update the session"
        );

        assert!(
            store.authenticate_web_session(&token).unwrap().is_none(),
            "revoked sessions should not authenticate"
        );

        assert!(
            !store.revoke_web_session(&token).unwrap(),
            "revoking an already revoked session should return false"
        );
    }

    #[test]
    fn web_sessions_reject_expired_tokens() {
        let store = test_store();

        let registration = store
            .register("Expired Session Customer", "expired-session@example.com")
            .expect("registration should succeed");

        let token = store
            .create_web_session("customer", &registration.account_id, 1)
            .expect("session creation should succeed");

        std::thread::sleep(std::time::Duration::from_secs(2));

        assert!(
            store.authenticate_web_session(&token).unwrap().is_none(),
            "expired sessions should be rejected"
        );
    }

    #[test]
    fn web_sessions_reject_invalid_principals_and_lifetimes() {
        let store = test_store();

        assert!(
            store
                .create_web_session("unknown", "principal", 3600)
                .is_err()
        );

        assert!(
            store
                .create_web_session("customer", "principal", 0)
                .is_err()
        );

        assert!(store.create_web_session("customer", "", 3600).is_err());
    }

    #[test]
    fn registration_activation_usage_and_key_rotation() {
        let store = test_store();

        // Register a new customer.
        let registration = store
            .register("Test Customer", "customer@example.com")
            .expect("registration should succeed");

        assert!(registration.account_id.starts_with("KKR-"));
        assert!(registration.api_key.starts_with("kk_live_"));

        // New registrations must remain pending.
        assert!(store.authenticate(&registration.api_key).is_err());

        // Activate the account with a 30-day subscription.
        store
            .activate(
                &registration.account_id,
                SubscriptionDuration::Days(30),
                100,
            )
            .expect("activation should succeed");

        // The original API key should now authenticate.
        let authenticated = store
            .authenticate(&registration.api_key)
            .expect("authentication should succeed");

        assert_eq!(authenticated.account_id, registration.account_id);
        assert_eq!(authenticated.status, "active");

        // Initial quota should be 100 characters.
        let quota = store
            .check_quota(&registration.account_id, 100)
            .expect("quota check should succeed");

        assert_eq!(quota.character_limit, 100);
        assert_eq!(quota.characters_used, 0);
        assert_eq!(quota.characters_remaining, 100);

        // Record a successful synthesis using 40 characters.
        store
            .record_usage(&registration.account_id, 40, "mp3", "af_heart", 1.0, 500)
            .expect("usage recording should succeed");

        // Verify the remaining quota.
        let quota = store
            .check_quota(&registration.account_id, 60)
            .expect("remaining quota should be available");

        assert_eq!(quota.characters_used, 40);
        assert_eq!(quota.characters_remaining, 60);

        // Requests exceeding the remaining quota must fail.
        assert!(store.check_quota(&registration.account_id, 61).is_err());

        assert!(
            store
                .record_usage(&registration.account_id, 61, "mp3", "af_heart", 1.0, 500,)
                .is_err()
        );

        // Rotate the API key.
        let new_key = store
            .rotate_api_key(&registration.account_id)
            .expect("key rotation should succeed");

        assert_ne!(new_key, registration.api_key);

        // The old key must no longer work.
        assert!(store.authenticate(&registration.api_key).is_err());

        // The new key must work.
        store
            .authenticate(&new_key)
            .expect("new API key should authenticate");

        // Suspended accounts must not authenticate.
        store
            .set_account_status(&registration.account_id, "suspended")
            .expect("suspension should succeed");

        assert!(store.authenticate(&new_key).is_err());
    }

    #[test]
    fn stt_activation_usage_and_quota_enforcement() {
        let store = test_store();

        let registration = store
            .register("STT Customer", "stt@example.com")
            .expect("registration should succeed");

        store
            .activate_with_stt(
                &registration.account_id,
                SubscriptionDuration::Days(30),
                1000,
                60_000,
            )
            .expect("STT activation should succeed");

        store
            .record_stt_usage(&registration.account_id, 20_000, "en", "transcribe", 3_000)
            .expect("STT usage should be recorded");

        let account = store
            .get_account(&registration.account_id)
            .expect("account lookup should succeed");

        let account = account.expect("registered account should exist");

        let subscription = account
            .subscription
            .expect("active subscription should exist");

        assert_eq!(subscription.stt_limit_ms, 60_000);
        assert_eq!(subscription.stt_used_ms, 20_000);

        // Another 40 seconds exactly reaches the allowance.
        store
            .record_stt_usage(&registration.account_id, 40_000, "en", "transcribe", 5_000)
            .expect("usage up to the STT limit should succeed");

        // Any additional audio must be rejected.
        assert!(
            store
                .record_stt_usage(&registration.account_id, 1, "en", "transcribe", 1,)
                .is_err()
        );

        let account = store
            .get_account(&registration.account_id)
            .expect("account lookup should succeed");

        let account = account.expect("registered account should exist");

        let subscription = account
            .subscription
            .expect("active subscription should exist");

        assert_eq!(subscription.stt_used_ms, 60_000);
        assert_eq!(subscription.stt_limit_ms, 60_000);
    }

    #[test]
    fn stt_subscription_extension_preserves_usage_and_adds_allowance() {
        let store = test_store();

        let registration = store
            .register("STT Extension Customer", "stt-extension@example.com")
            .expect("registration should succeed");

        store
            .activate_with_stt(
                &registration.account_id,
                SubscriptionDuration::Days(30),
                1000,
                60_000,
            )
            .expect("activation should succeed");

        store
            .record_stt_usage(&registration.account_id, 20_000, "en", "transcribe", 3_000)
            .expect("initial STT usage should succeed");

        store
            .extend_subscription_with_stt(
                &registration.account_id,
                SubscriptionDuration::Days(15),
                500,
                120_000,
            )
            .expect("STT extension should succeed");

        let account = store
            .get_account(&registration.account_id)
            .expect("account lookup should succeed");

        let account = account.expect("registered account should exist");

        let subscription = account
            .subscription
            .expect("active subscription should exist");

        assert_eq!(subscription.character_limit, 1500);
        assert_eq!(subscription.stt_limit_ms, 180_000);
        assert_eq!(subscription.stt_used_ms, 20_000);

        // The remaining 160 seconds should be available.
        store
            .record_stt_usage(
                &registration.account_id,
                160_000,
                "en",
                "transcribe",
                10_000,
            )
            .expect("remaining STT allowance should be available");

        assert!(
            store
                .record_stt_usage(&registration.account_id, 1, "en", "transcribe", 1,)
                .is_err()
        );
    }

    #[test]
    fn legacy_activation_and_extension_keep_zero_stt_allowance() {
        let store = test_store();

        let registration = store
            .register("Legacy Customer", "legacy@example.com")
            .expect("registration should succeed");

        store
            .activate(
                &registration.account_id,
                SubscriptionDuration::Days(30),
                1000,
            )
            .expect("legacy activation should succeed");

        store
            .extend_subscription(
                &registration.account_id,
                SubscriptionDuration::Days(15),
                500,
            )
            .expect("legacy extension should succeed");

        let account = store
            .get_account(&registration.account_id)
            .expect("account lookup should succeed");

        let account = account.expect("registered account should exist");

        let subscription = account
            .subscription
            .expect("active subscription should exist");

        assert_eq!(subscription.character_limit, 1500);
        assert_eq!(subscription.stt_limit_ms, 0);
        assert_eq!(subscription.stt_used_ms, 0);
    }

    #[test]
    fn subscription_extension_adds_time_and_quota() {
        let store = test_store();

        let registration = store
            .register("Extension Customer", "extension@example.com")
            .expect("registration should succeed");

        store
            .activate(
                &registration.account_id,
                SubscriptionDuration::Days(30),
                1000,
            )
            .expect("activation should succeed");

        let original = store
            .check_quota(&registration.account_id, 1)
            .expect("initial quota should be available");

        store
            .extend_subscription(
                &registration.account_id,
                SubscriptionDuration::Days(15),
                500,
            )
            .expect("extension should succeed");

        let extended = store
            .check_quota(&registration.account_id, 1)
            .expect("extended subscription should remain available");

        assert_eq!(extended.character_limit, 1500);

        assert!(
            extended.expires_at > original.expires_at,
            "subscription expiry should increase"
        );

        let added_seconds = extended.expires_at - original.expires_at;

        assert!(
            (14 * 86400..=16 * 86400).contains(&added_seconds),
            "expected approximately 15 additional days, got {} seconds",
            added_seconds
        );
    }
}
