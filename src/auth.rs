//! Scoped API tokens. Three roles, deliberately few:
//!
//! - `operator` — everything: decisions, decorrelation, unlimited asks, every source.
//! - `analyst`  — decisions and asks; no decorrelation (that is an operator's call).
//! - `client`   — asks only, usually with a daily quota and a `commercial_clean` license
//!   filter so a paying tenant is never handed non-commercial data by accident.
//!
//! The `ENARGEIA_TOKEN` environment variable remains the root operator token. With no root
//! token and no stored tokens the API is open on whatever it is bound to (loopback by default).

use anyhow::{anyhow, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::models::{new_id, now};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Operator,
    Analyst,
    Client,
}

impl Role {
    pub fn parse(s: &str) -> Result<Role> {
        match s {
            "operator" => Ok(Role::Operator),
            "analyst" => Ok(Role::Analyst),
            "client" => Ok(Role::Client),
            other => Err(anyhow!(
                "unknown role {other:?}; expected operator | analyst | client"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Operator => "operator",
            Role::Analyst => "analyst",
            Role::Client => "client",
        }
    }

    pub fn can_decide(self) -> bool {
        matches!(self, Role::Operator | Role::Analyst)
    }

    pub fn can_decorrelate(self) -> bool {
        self == Role::Operator
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Principal {
    /// `None` for the root token and for the open (no-token) mode.
    pub token_id: Option<String>,
    pub name: String,
    pub role: Role,
    pub ask_daily_limit: Option<i64>,
    pub license_filter: Option<String>,
}

impl Principal {
    pub fn root() -> Principal {
        Principal {
            token_id: None,
            name: "root".into(),
            role: Role::Operator,
            ask_daily_limit: None,
            license_filter: None,
        }
    }

    pub fn open() -> Principal {
        Principal {
            name: "anonymous (no token configured)".into(),
            ..Principal::root()
        }
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TokenRow {
    pub id: String,
    pub name: String,
    pub role: String,
    pub ask_daily_limit: Option<i64>,
    pub license_filter: Option<String>,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

pub fn hash_token(plain: &str) -> String {
    let mut h = Sha256::new();
    h.update(plain.as_bytes());
    format!("{:x}", h.finalize())
}

fn generate_plaintext() -> String {
    // 128 bits from two v4 UUIDs' random fields is plenty; the prefix makes leaks greppable.
    let a = uuid::Uuid::new_v4().simple().to_string();
    format!("enk_{a}")
}

/// Creates a token and returns `(id, plaintext)`. The plaintext is not stored.
pub async fn create_token(
    pool: &SqlitePool,
    name: &str,
    role: Role,
    ask_daily_limit: Option<i64>,
    license_filter: Option<&str>,
) -> Result<(String, String)> {
    let plain = generate_plaintext();
    let id = new_id();
    sqlx::query(
        "INSERT INTO wm_api_tokens (id, name, token_hash, role, ask_daily_limit, license_filter, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(hash_token(&plain))
    .bind(role.as_str())
    .bind(ask_daily_limit)
    .bind(license_filter)
    .bind(now())
    .execute(pool)
    .await?;
    Ok((id, plain))
}

pub async fn list_tokens(pool: &SqlitePool) -> Result<Vec<TokenRow>> {
    Ok(sqlx::query_as::<_, TokenRow>(
        "SELECT id, name, role, ask_daily_limit, license_filter, created_at, revoked_at, last_used_at \
         FROM wm_api_tokens ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn revoke_token(pool: &SqlitePool, id_or_name: &str) -> Result<u64> {
    let r = sqlx::query(
        "UPDATE wm_api_tokens SET revoked_at = ? WHERE revoked_at IS NULL AND (id = ? OR name = ?)",
    )
    .bind(now())
    .bind(id_or_name)
    .bind(id_or_name)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

pub async fn any_tokens(pool: &SqlitePool) -> Result<bool> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM wm_api_tokens WHERE revoked_at IS NULL")
            .fetch_one(pool)
            .await?;
    Ok(n > 0)
}

/// Resolves a bearer value to a principal. `Ok(None)` means "not authenticated".
pub async fn authenticate(
    pool: &SqlitePool,
    root_token: Option<&str>,
    bearer: Option<&str>,
) -> Result<Option<Principal>> {
    let Some(bearer) = bearer.filter(|b| !b.is_empty()) else {
        // Open mode only when nothing at all is configured.
        return Ok(if root_token.is_none() && !any_tokens(pool).await? {
            Some(Principal::open())
        } else {
            None
        });
    };
    if let Some(root) = root_token {
        if bearer == root {
            return Ok(Some(Principal::root()));
        }
    }
    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT id, name, role, ask_daily_limit, license_filter, created_at, revoked_at, last_used_at \
         FROM wm_api_tokens WHERE token_hash = ? AND revoked_at IS NULL",
    )
    .bind(hash_token(bearer))
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    sqlx::query("UPDATE wm_api_tokens SET last_used_at = ? WHERE id = ?")
        .bind(now())
        .bind(&row.id)
        .execute(pool)
        .await?;
    Ok(Some(Principal {
        token_id: Some(row.id),
        name: row.name,
        role: Role::parse(&row.role)?,
        ask_daily_limit: row.ask_daily_limit,
        license_filter: row.license_filter,
    }))
}

/// Counts one `ask` against the principal's daily quota. Returns the remaining allowance
/// after this call (`None` = unlimited), or an error if the quota is already spent.
pub async fn charge_ask(pool: &SqlitePool, principal: &Principal) -> Result<Option<i64>> {
    let (Some(token_id), Some(limit)) = (&principal.token_id, principal.ask_daily_limit) else {
        return Ok(None);
    };
    let day = now().get(..10).unwrap_or("").to_string();
    let (used,): (i64,) =
        sqlx::query_as("SELECT COALESCE(asks, 0) FROM wm_api_usage WHERE token_id = ? AND day = ?")
            .bind(token_id)
            .bind(&day)
            .fetch_optional(pool)
            .await?
            .unwrap_or((0,));
    if used >= limit {
        return Err(anyhow!(
            "daily ask quota of {limit} exhausted for {}; resets at 00:00 UTC",
            principal.name
        ));
    }
    sqlx::query(
        "INSERT INTO wm_api_usage (token_id, day, asks) VALUES (?, ?, 1) \
         ON CONFLICT(token_id, day) DO UPDATE SET asks = asks + 1",
    )
    .bind(token_id)
    .bind(&day)
    .execute(pool)
    .await?;
    Ok(Some(limit - used - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_hashing() {
        assert!(Role::parse("client").unwrap() == Role::Client);
        assert!(Role::parse("root").is_err());
        assert!(!Role::Client.can_decide());
        assert!(Role::Analyst.can_decide() && !Role::Analyst.can_decorrelate());
        assert!(Role::Operator.can_decorrelate());
        let p = generate_plaintext();
        assert!(p.starts_with("enk_") && p.len() == 36);
        assert_eq!(hash_token(&p).len(), 64);
        assert_ne!(hash_token(&p), hash_token("enk_other"));
    }
}
