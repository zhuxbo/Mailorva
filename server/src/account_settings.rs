use crate::{
    config::{AccountConfig, SecretRef},
    db::Database,
    error::{AppError, Result},
    mail::MailAccount,
    Service,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashSet;

#[derive(Clone, Serialize, Deserialize)]
pub struct AccountSettings {
    pub id: String,
    pub email: String,
    pub display_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub folders: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountInput {
    pub email: String,
    pub display_name: String,
    pub aliases: Option<Vec<String>>,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub folders: Vec<String>,
    pub password: Option<String>,
}
#[derive(Serialize, Deserialize)]
enum Credential {
    Reference(SecretRef),
    Password(String),
}
#[derive(Serialize, Deserialize)]
struct StoredAccount {
    #[serde(flatten)]
    settings: AccountSettings,
    credential: Credential,
}
pub(crate) fn valid_aliases(email: &str, aliases: &[String]) -> bool {
    let mut seen = HashSet::from([email.to_lowercase()]);
    aliases.len() <= 100
        && aliases.iter().all(|a| {
            a.len() <= 254
                && !a.chars().any(char::is_control)
                && a.parse::<lettre::Address>().is_ok()
                && seen.insert(a.to_lowercase())
        })
}
impl AccountInput {
    fn validate(&self) -> Result<()> {
        let host = |s: &str| {
            !s.is_empty()
                && s.len() <= 253
                && s.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        };
        let mut folders = HashSet::new();
        if self
            .aliases
            .as_ref()
            .is_some_and(|a| !valid_aliases(&self.email, a))
            || self.email.len() > 254
            || self.email.parse::<lettre::Address>().is_err()
            || self.display_name.trim().is_empty()
            || self.display_name.len() > 256
            || self.display_name.chars().any(char::is_control)
            || !host(&self.imap_host)
            || !host(&self.smtp_host)
            || self.imap_port == 0
            || self.smtp_port == 0
            || self.folders.is_empty()
            || self.folders.len() > 100
            || self.folders.iter().any(|s| {
                s.trim().is_empty()
                    || s.len() > 512
                    || s.chars().any(char::is_control)
                    || !folders.insert(s)
            })
            || self
                .password
                .as_ref()
                .is_some_and(|s| s.len() > 4096 || s.contains(['\r', '\n', '\0']))
        {
            return Err(AppError::Invalid);
        }
        Ok(())
    }
    fn settings(self, id: String) -> AccountSettings {
        AccountSettings {
            id,
            email: self.email,
            display_name: self.display_name,
            aliases: self.aliases.unwrap_or_default(),
            imap_host: self.imap_host,
            imap_port: self.imap_port,
            smtp_host: self.smtp_host,
            smtp_port: self.smtp_port,
            folders: self.folders,
        }
    }
}

pub async fn initialize(db: &Database, initial: &[AccountConfig]) -> anyhow::Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS account_configuration (singleton INTEGER PRIMARY KEY CHECK(singleton=1), data TEXT NOT NULL)").execute(&db.pool).await?;
    let mut tx = db.pool.begin().await?;
    if sqlx::query("SELECT singleton FROM account_configuration")
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        let accounts: Vec<_> = initial
            .iter()
            .map(|a| StoredAccount {
                settings: AccountSettings {
                    id: a.id.clone(),
                    email: a.email.clone(),
                    display_name: a.display_name.clone(),
                    aliases: a.aliases.clone(),
                    imap_host: a.imap_host.clone(),
                    imap_port: a.imap_port,
                    smtp_host: a.smtp_host.clone(),
                    smtp_port: a.smtp_port,
                    folders: a.folders.clone(),
                },
                credential: Credential::Reference(a.password.clone()),
            })
            .collect();
        persist_configuration(&mut tx, &accounts).await?;
    }
    tx.commit().await?;
    Ok(())
}
impl Service {
    async fn stored_accounts(&self) -> Result<Vec<StoredAccount>> {
        let row = sqlx::query("SELECT data FROM account_configuration WHERE singleton=1")
            .fetch_one(&self.db.pool)
            .await?;
        Ok(serde_json::from_str(row.get("data"))?)
    }
    pub async fn account_settings(&self) -> Result<Vec<AccountSettings>> {
        Ok(self
            .stored_accounts()
            .await?
            .into_iter()
            .map(|a| a.settings)
            .collect())
    }
    pub async fn mail_account(&self, id: &str) -> Result<MailAccount> {
        let stored = self
            .stored_accounts()
            .await?
            .into_iter()
            .find(|a| a.settings.id == id)
            .ok_or(AppError::NotFound)?;
        let password = match stored.credential {
            Credential::Reference(r) => r.resolve().map_err(|_| AppError::Unavailable)?,
            Credential::Password(p) => p.into(),
        };
        let a = stored.settings;
        Ok(MailAccount {
            id: a.id,
            email: a.email,
            aliases: a.aliases,
            imap_host: a.imap_host,
            imap_port: a.imap_port,
            smtp_host: a.smtp_host,
            smtp_port: a.smtp_port,
            password,
        })
    }
    pub async fn save_account(
        &self,
        id: Option<&str>,
        mut input: AccountInput,
    ) -> Result<AccountSettings> {
        input.validate()?;
        let _change = self.account_changes.lock().await;
        let editing = id.is_some();
        let id = id
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let _guard = self.account_lock(&id).await;
        let mut accounts = self.stored_accounts().await?;
        let position = accounts.iter().position(|a| a.settings.id == id);
        // Caller-supplied IDs are edits only; IDs can never be recycled after deletion.
        if editing && position.is_none() {
            return Err(AppError::NotFound);
        }
        if accounts
            .iter()
            .any(|a| a.settings.id != id && a.settings.email.eq_ignore_ascii_case(&input.email))
        {
            return Err(AppError::AccountExists);
        }
        let password = input.password.take().filter(|s| !s.is_empty());
        if let Some(i) = position {
            let old = &accounts[i].settings;
            if input.email != old.email
                || input.imap_host != old.imap_host
                || input.imap_port != old.imap_port
            {
                return Err(AppError::Invalid);
            }
            if input.aliases.is_none() {
                input.aliases = Some(old.aliases.clone());
            }
            accounts[i].settings = input.settings(id.clone());
            if let Some(password) = password {
                accounts[i].credential = Credential::Password(password);
            }
        } else {
            if accounts.len() >= 10 {
                return Err(AppError::Invalid);
            }
            let password = password.ok_or(AppError::Invalid)?;
            accounts.push(StoredAccount {
                settings: input.settings(id.clone()),
                credential: Credential::Password(password),
            });
        }
        let settings = accounts
            .iter()
            .find(|a| a.settings.id == id)
            .unwrap()
            .settings
            .clone();
        self.transport.invalidate_account(&id).await;
        self.persist_accounts(&accounts).await?;
        Ok(settings)
    }
    pub async fn delete_account(&self, id: &str) -> Result<()> {
        let _change = self.account_changes.lock().await;
        let _guard = self.account_lock(id).await;
        let mut accounts = self.stored_accounts().await?;
        let index = accounts
            .iter()
            .position(|a| a.settings.id == id)
            .ok_or(AppError::NotFound)?;
        accounts.remove(index);
        self.transport.invalidate_account(id).await;
        self.persist_accounts(&accounts).await?;
        Ok(())
    }
    async fn persist_accounts(&self, accounts: &[StoredAccount]) -> Result<()> {
        let mut tx = self.db.pool.begin().await?;
        persist_configuration(&mut tx, accounts).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn persist_configuration(
    connection: &mut sqlx::SqliteConnection,
    accounts: &[StoredAccount],
) -> Result<()> {
    sqlx::query("INSERT INTO account_configuration VALUES(1,?) ON CONFLICT(singleton) DO UPDATE SET data=excluded.data")
            .bind(serde_json::to_string(accounts)?)
            .execute(&mut *connection)
            .await?;
    let rows = sqlx::query(
        "SELECT account_id,folder FROM messages UNION SELECT account_id,folder FROM cursors",
    )
    .fetch_all(&mut *connection)
    .await?;
    for row in rows {
        let id: String = row.get(0);
        let folder: String = row.get(1);
        if !accounts
            .iter()
            .any(|a| a.settings.id == id && a.settings.folders.contains(&folder))
        {
            for table in ["messages", "cursors"] {
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE account_id=? AND folder=?"
                ))
                .bind(&id)
                .bind(&folder)
                .execute(&mut *connection)
                .await?;
            }
        }
    }
    for row in sqlx::query("SELECT account_id FROM sync_status")
        .fetch_all(&mut *connection)
        .await?
    {
        let id: String = row.get(0);
        if !accounts.iter().any(|a| a.settings.id == id) {
            sqlx::query("DELETE FROM sync_status WHERE account_id=?")
                .bind(&id)
                .execute(&mut *connection)
                .await?;
        }
    }
    for row in sqlx::query("SELECT id,payload FROM send_operations WHERE status='prepared'")
        .fetch_all(&mut *connection)
        .await?
    {
        let input: crate::operations::PrepareSend = serde_json::from_str(row.get("payload"))?;
        let missing_parent = if let Some(parent) = &input.reply_to_id {
            sqlx::query("SELECT id FROM messages WHERE id=?")
                .bind(parent)
                .fetch_optional(&mut *connection)
                .await?
                .is_none()
        } else {
            false
        };
        if missing_parent || !accounts.iter().any(|a| a.settings.id == input.account_id) {
            sqlx::query("UPDATE send_operations SET status='cancelled',payload='' WHERE id=?")
                .bind(row.get::<String, _>("id"))
                .execute(&mut *connection)
                .await?;
        }
    }
    Ok(())
}
