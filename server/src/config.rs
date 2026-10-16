use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::SocketAddr, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    pub env: Option<String>,
    pub file: Option<String>,
}
impl SecretRef {
    pub fn resolve(&self) -> anyhow::Result<SecretString> {
        let value = match (&self.env, &self.file) {
            (Some(name), None) => {
                std::env::var(name).map_err(|_| anyhow::anyhow!("缺少所需环境变量"))?
            }
            (None, Some(path)) => {
                std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("无法读取密钥文件"))?
            }
            _ => anyhow::bail!("密钥必须且只能配置 env 或 file 引用"),
        };
        let value = value.trim_end_matches(['\r', '\n']).to_owned();
        anyhow::ensure!(!value.is_empty(), "密钥不能为空");
        Ok(value.into())
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub id: String,
    pub email: String,
    pub display_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub imap_host: String,
    #[serde(default = "imap_port")]
    pub imap_port: u16,
    pub smtp_host: String,
    #[serde(default = "smtp_port")]
    pub smtp_port: u16,
    pub password: SecretRef,
    pub folders: Vec<String>,
}
fn imap_port() -> u16 {
    993
}
fn smtp_port() -> u16 {
    465
}
fn default_bind() -> SocketAddr {
    "127.0.0.1:8080".parse().unwrap()
}
fn default_hosts() -> Vec<String> {
    vec!["127.0.0.1".into(), "localhost".into(), "[::1]".into()]
}
fn default_interval() -> u64 {
    60
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    pub database_path: String,
    pub read_token: SecretRef,
    pub write_token: SecretRef,
    #[serde(default)]
    pub oauth: Option<crate::oauth::OAuthConfig>,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_hosts")]
    pub allowed_hosts: Vec<String>,
    #[serde(default = "default_interval")]
    pub sync_interval_seconds: u64,
    #[serde(default)]
    pub import_history: bool,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}
impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|_| anyhow::anyhow!("配置 JSON 无效"))?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.bind.ip().is_loopback(),
            "服务必须绑定 loopback，通过 HTTPS 反向代理访问"
        );
        anyhow::ensure!(self.accounts.len() <= 10, "账户数量必须为 0 到 10");
        anyhow::ensure!(self.sync_interval_seconds >= 10, "同步间隔不能小于 10 秒");
        let mut ids = HashSet::new();
        let mut emails = HashSet::new();
        for account in &self.accounts {
            anyhow::ensure!(
                crate::account_settings::valid_aliases(&account.email, &account.aliases),
                "邮箱别名无效或重复"
            );
            anyhow::ensure!(
                !account.id.is_empty() && ids.insert(&account.id),
                "账户 id 不能为空或重复"
            );
            anyhow::ensure!(
                emails.insert(account.email.to_lowercase())
                    && !account.email.is_empty()
                    && !account.imap_host.is_empty()
                    && !account.smtp_host.is_empty(),
                "账户配置不完整"
            );
            anyhow::ensure!(
                !account.folders.is_empty() && account.folders.iter().all(|s| !s.trim().is_empty()),
                "必须配置同步文件夹"
            );
        }
        let read = self.read_token.resolve()?;
        let write = self.write_token.resolve()?;
        anyhow::ensure!(
            read.expose_secret().len() >= 32 && write.expose_secret().len() >= 32,
            "访问令牌至少需要 32 字符"
        );
        anyhow::ensure!(
            read.expose_secret() != write.expose_secret(),
            "只读和可写令牌必须不同"
        );
        if let Some(oauth) = &self.oauth {
            oauth.validate()?;
            let secret = oauth.approval_secret.resolve()?;
            anyhow::ensure!(
                secret.expose_secret() != read.expose_secret()
                    && secret.expose_secret() != write.expose_secret(),
                "OAuth 授权口令必须独立于访问令牌"
            );
        }
        Ok(())
    }
    pub fn mail_account(&self, id: &str) -> anyhow::Result<crate::mail::MailAccount> {
        let a = self
            .accounts
            .iter()
            .find(|a| a.id == id)
            .ok_or_else(|| anyhow::anyhow!("未知账户"))?;
        Ok(crate::mail::MailAccount {
            id: a.id.clone(),
            email: a.email.clone(),
            aliases: a.aliases.clone(),
            imap_host: a.imap_host.clone(),
            imap_port: a.imap_port,
            smtp_host: a.smtp_host.clone(),
            smtp_port: a.smtp_port,
            password: a.password.resolve()?,
        })
    }
}
