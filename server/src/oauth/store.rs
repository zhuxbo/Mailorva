use super::{digest, OAuthError, TokenPair};
use crate::model::now;
use serde::{de::DeserializeOwned, Serialize};
use sqlx::{Row, SqlitePool};

pub struct Store(pub SqlitePool);

impl Store {
    pub async fn new(pool: SqlitePool) -> anyhow::Result<Self> {
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS oauth_requests (hash TEXT PRIMARY KEY, kind TEXT NOT NULL, data TEXT NOT NULL, expires_at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS oauth_tokens (refresh_hash TEXT PRIMARY KEY, access_hash TEXT NOT NULL UNIQUE, client_id TEXT NOT NULL, family TEXT NOT NULL, binding TEXT NOT NULL, access_expires_at INTEGER NOT NULL, refresh_expires_at INTEGER NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS oauth_tokens_family ON oauth_tokens(family);
             CREATE TABLE IF NOT EXISTS oauth_replies (id TEXT PRIMARY KEY, family TEXT NOT NULL, expires_at INTEGER NOT NULL);"
        ).execute(&pool).await?;
        let columns = sqlx::query("PRAGMA table_info(oauth_tokens)")
            .fetch_all(&pool)
            .await?;
        if !columns
            .iter()
            .any(|r| r.get::<String, _>("name") == "scope")
        {
            sqlx::query(
                "ALTER TABLE oauth_tokens ADD COLUMN scope TEXT NOT NULL DEFAULT 'mail:read'",
            )
            .execute(&pool)
            .await?;
        }
        Ok(Self(pool))
    }
    pub async fn save<T: Serialize>(
        &self,
        key: &str,
        kind: &str,
        data: &T,
        ttl: i64,
    ) -> Result<(), OAuthError> {
        sqlx::query("DELETE FROM oauth_requests WHERE expires_at <= ?")
            .bind(now())
            .execute(&self.0)
            .await?;
        sqlx::query("INSERT INTO oauth_requests(hash,kind,data,expires_at) VALUES(?,?,?,?)")
            .bind(digest(key))
            .bind(kind)
            .bind(serde_json::to_string(data).map_err(|_| OAuthError::internal())?)
            .bind(now() + ttl)
            .execute(&self.0)
            .await?;
        Ok(())
    }
    pub async fn take<T: DeserializeOwned>(&self, key: &str, kind: &str) -> Result<T, OAuthError> {
        let row: Option<(String, i64)> = sqlx::query_as(
            "DELETE FROM oauth_requests WHERE hash=? AND kind=? RETURNING data,expires_at",
        )
        .bind(digest(key))
        .bind(kind)
        .fetch_optional(&self.0)
        .await?;
        let (data, expiry) = row.ok_or_else(OAuthError::invalid_grant)?;
        if expiry <= now() {
            return Err(OAuthError::invalid_grant());
        }
        serde_json::from_str(&data).map_err(|_| OAuthError::internal())
    }
    pub async fn issue(
        &self,
        client: &str,
        binding: &str,
        scope: &str,
    ) -> Result<TokenPair, OAuthError> {
        let mut pair = TokenPair::new();
        pair.scope = scope.into();
        let expiry = now() + 30 * 24 * 60 * 60;
        let family = super::random();
        sqlx::query("DELETE FROM oauth_tokens WHERE refresh_expires_at <= ?")
            .bind(now())
            .execute(&self.0)
            .await?;
        sqlx::query("INSERT INTO oauth_tokens(refresh_hash,access_hash,client_id,family,binding,access_expires_at,refresh_expires_at,scope) VALUES(?,?,?,?,?,?,?,?)")
            .bind(digest(&pair.refresh_token)).bind(digest(&pair.access_token)).bind(client).bind(family).bind(binding)
            .bind(now()+3600).bind(expiry).bind(&pair.scope).execute(&self.0).await?;
        Ok(pair)
    }
    pub async fn refresh(
        &self,
        token: &str,
        client: &str,
        binding: &str,
        requested_scope: Option<&str>,
    ) -> Result<TokenPair, OAuthError> {
        let mut tx = self.0.begin().await?;
        let row=sqlx::query("SELECT family,refresh_expires_at,consumed,scope FROM oauth_tokens WHERE refresh_hash=? AND client_id=? AND binding=?")
            .bind(digest(token)).bind(client).bind(binding).fetch_optional(&mut *tx).await?
            .ok_or_else(OAuthError::invalid_grant)?;
        let family: String = row.try_get("family")?;
        let expiry: i64 = row.try_get("refresh_expires_at")?;
        if row.try_get::<bool, _>("consumed")? || expiry <= now() {
            sqlx::query("DELETE FROM oauth_tokens WHERE family=?")
                .bind(family)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(OAuthError::invalid_grant());
        }
        let original_scope: String = row.try_get("scope")?;
        let scope = match requested_scope {
            Some(value) => super::normalized_scope(Some(value))?,
            None => original_scope.clone(),
        };
        if scope == super::REPLY_SCOPE && original_scope != super::REPLY_SCOPE {
            return Err(OAuthError::bad("invalid_scope"));
        }
        sqlx::query("UPDATE oauth_tokens SET consumed=1 WHERE refresh_hash=?")
            .bind(digest(token))
            .execute(&mut *tx)
            .await?;
        let mut pair = TokenPair::new();
        pair.scope = scope;
        pair.expires_in = 3600.min(expiry - now());
        sqlx::query("INSERT INTO oauth_tokens(refresh_hash,access_hash,client_id,family,binding,access_expires_at,refresh_expires_at,scope) VALUES(?,?,?,?,?,?,?,?)")
            .bind(digest(&pair.refresh_token)).bind(digest(&pair.access_token)).bind(client).bind(family).bind(binding)
            .bind(now()+pair.expires_in).bind(expiry).bind(&pair.scope).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(pair)
    }
    pub async fn access(
        &self,
        token: &str,
        binding: &str,
    ) -> Result<Option<super::Grant>, OAuthError> {
        let row: Option<(String, String)> = sqlx::query_as("SELECT family,scope FROM oauth_tokens WHERE access_hash=? AND binding=? AND consumed=0 AND access_expires_at>? AND refresh_expires_at>?")
            .bind(digest(token)).bind(binding).bind(now()).bind(now()).fetch_optional(&self.0).await?;
        Ok(row.map(|(family, scope)| super::Grant {
            family,
            reply: scope == super::REPLY_SCOPE,
        }))
    }
    pub async fn revoke(&self, token: &str, client: &str) -> Result<(), OAuthError> {
        let hash = digest(token);
        sqlx::query("DELETE FROM oauth_tokens WHERE family IN (SELECT family FROM oauth_tokens WHERE (refresh_hash=? OR access_hash=?) AND client_id=?)")
            .bind(&hash).bind(&hash).bind(client).execute(&self.0).await?;
        Ok(())
    }
}
