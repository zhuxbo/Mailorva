pub mod account_settings;
pub mod api;
pub mod config;
pub mod db;
pub mod error;
pub mod mail;
pub mod mcp;
pub mod model;
pub mod oauth;
pub mod operations;
pub mod sync;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, OwnedMutexGuard};
pub struct Service {
    pub config: config::Config,
    pub db: db::Database,
    pub transport: Arc<dyn mail::MailTransport>,
    pub oauth: Option<Arc<oauth::OAuth>>,
    account_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    account_changes: Mutex<()>,
}
impl Service {
    pub async fn new(
        config: config::Config,
        transport: Arc<dyn mail::MailTransport>,
    ) -> anyhow::Result<Arc<Self>> {
        config.validate()?;
        let db = db::Database::open(&config.database_path).await?;
        account_settings::initialize(&db, &config.accounts).await?;
        db.cleanup(model::now()).await?;
        let oauth = match &config.oauth {
            Some(config) => Some(oauth::OAuth::new(config.clone(), db.pool.clone()).await?),
            None => None,
        };
        Ok(Arc::new(Self {
            config,
            db,
            transport,
            oauth,
            account_locks: Mutex::new(HashMap::new()),
            account_changes: Mutex::new(()),
        }))
    }
    pub async fn account_lock(&self, id: &str) -> OwnedMutexGuard<()> {
        let lock = self
            .account_locks
            .lock()
            .await
            .entry(id.to_owned())
            .or_default()
            .clone();
        lock.lock_owned().await
    }
    pub async fn accounts(&self, can_write: bool) -> error::Result<serde_json::Value> {
        let existing = self.db.statuses().await?;
        let accounts = self.account_settings().await?;
        let sync: Vec<_> = accounts
            .iter()
            .map(|a| {
                existing
                    .iter()
                    .find(|s| s.account_id == a.id)
                    .cloned()
                    .unwrap_or(model::SyncStatus {
                        account_id: a.id.clone(),
                        status: "pending".into(),
                        last_synced_at: None,
                        error: None,
                    })
            })
            .collect();
        Ok(
            serde_json::json!({"can_write":can_write,"accounts":accounts.iter().map(|a| serde_json::json!({"id":a.id,"email":a.email,"display_name":a.display_name,"folders":a.folders,"aliases":a.aliases})).collect::<Vec<_>>(),"sync":sync}),
        )
    }
}
