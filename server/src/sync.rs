use crate::{
    error::Result,
    model::{now, RETENTION_SECONDS},
    Service,
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
impl Service {
    pub async fn sync_account(&self, account_id: &str) -> Result<()> {
        let _guard = self.account_lock(account_id).await;
        let account = self.mail_account(account_id).await?;
        let cfg = self
            .account_settings()
            .await?
            .into_iter()
            .find(|a| a.id == account_id)
            .ok_or(crate::error::AppError::NotFound)?;
        let since = now() - RETENTION_SECONDS;
        for folder in &cfg.folders {
            let snapshot = self.transport.snapshot(&account, folder, since).await?;
            let cursor = self.db.cursor(account_id, folder).await?;
            let last_uid = match cursor {
                Some((v, uid)) if v == snapshot.uid_validity => uid,
                previous => {
                    if previous.is_some() {
                        self.db.reset_folder(account_id, folder).await?;
                    }
                    let baseline = if self.config.import_history {
                        0
                    } else {
                        snapshot.uid_next.saturating_sub(1)
                    };
                    self.db
                        .set_cursor(account_id, folder, snapshot.uid_validity, baseline)
                        .await?;
                    baseline
                }
            };
            let mut uids = snapshot.uids;
            uids.sort_unstable();
            uids.dedup();
            for uid in uids.into_iter().filter(|uid| *uid > last_uid) {
                let mail = match self
                    .transport
                    .fetch(&account, folder, snapshot.uid_validity, uid)
                    .await
                {
                    Ok(mail) => mail,
                    Err(crate::mail::MailError::TooLarge | crate::mail::MailError::NotFound) => {
                        tracing::warn!(account_id, uid, "跳过已删除或超过大小限制的邮件");
                        self.db
                            .set_cursor(account_id, folder, snapshot.uid_validity, uid)
                            .await?;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                if mail.uid != uid {
                    return Err(crate::error::AppError::Unavailable);
                }
                if mail.received_at >= since {
                    self.db
                        .put(
                            account_id,
                            &account.email,
                            folder,
                            snapshot.uid_validity,
                            mail,
                        )
                        .await?;
                }
                self.db
                    .set_cursor(account_id, folder, snapshot.uid_validity, uid)
                    .await?;
            }
        }
        Ok(())
    }
    pub async fn sync_once(&self) {
        let Ok(accounts) = self.account_settings().await else {
            return;
        };
        for account in accounts {
            let success = self.sync_account(&account.id).await.is_ok();
            let _guard = self.account_lock(&account.id).await;
            let Ok(current_accounts) = self.account_settings().await else {
                tracing::error!("无法读取账户配置");
                continue;
            };
            if !current_accounts.iter().any(|a| a.id == account.id) {
                continue;
            }
            if self.db.set_status(&account.id, success).await.is_err() {
                tracing::error!("无法保存同步状态");
            }
        }
        if self.db.cleanup(now()).await.is_err() {
            tracing::error!("无法清理过期本地邮件");
        }
    }
    pub async fn run_sync(self: Arc<Self>, cancel: CancellationToken) {
        let mut interval =
            tokio::time::interval(Duration::from_secs(self.config.sync_interval_seconds));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = self.sync_once() => {}
                    }
                }
            }
        }
    }
}
