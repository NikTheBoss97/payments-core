// Author: Nicola Antonello

//! On whose behalf a unit of work runs (ADR-0025).
//!
//! A merchant's requests hold a [`Tenant`], whose pool logs in as the tenant role.
//! That role sees nothing a merchant owns until a transaction names its merchant,
//! so a path that forgets fails closed. The system paths pass a plain pool, which
//! logs in as the system role, and name nobody.

use sqlx::{PgPool, Postgres, Transaction};

use crate::idempotency::Seal;
use crate::ids::MerchantId;

/// A merchant, and the pool that its requests run on.
#[derive(Debug, Clone, Copy)]
pub struct Tenant<'a> {
    pool: &'a PgPool,
    merchant: MerchantId,
}

impl<'a> Tenant<'a> {
    #[must_use]
    pub fn new(pool: &'a PgPool, merchant: MerchantId) -> Self {
        Self { pool, merchant }
    }

    /// This merchant's scope, for a command that also stores its final answer under
    /// an idempotency key, in the same transaction as its effect (ADR-0026).
    #[must_use]
    pub fn sealed(self, seal: &'a Seal<'a>) -> Scope<'a> {
        Scope {
            seal: Some(seal),
            ..self.into()
        }
    }

    pub(crate) fn merchant(self) -> MerchantId {
        self.merchant
    }
}

/// Where a unit of work runs: a [`Tenant`], or a plain pool for the system paths.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pool: &'a PgPool,
    merchant: Option<MerchantId>,
    seal: Option<&'a Seal<'a>>,
}

impl<'a> From<&'a PgPool> for Scope<'a> {
    fn from(pool: &'a PgPool) -> Self {
        Self {
            pool,
            merchant: None,
            seal: None,
        }
    }
}

impl<'a> From<Tenant<'a>> for Scope<'a> {
    fn from(tenant: Tenant<'a>) -> Self {
        Self {
            pool: tenant.pool,
            merchant: Some(tenant.merchant),
            seal: None,
        }
    }
}

impl<'a> Scope<'a> {
    /// The pool, for statements on tables outside the policies, such as `accounts`.
    pub(crate) fn pool(self) -> &'a PgPool {
        self.pool
    }

    /// The seal of a command under a key, with the merchant that owns the key.
    pub(crate) fn seal(self) -> Option<(MerchantId, &'a Seal<'a>)> {
        self.merchant.zip(self.seal)
    }

    /// Begins a database transaction that may read and write what this scope owns.
    pub(crate) async fn begin(self) -> sqlx::Result<Transaction<'static, Postgres>> {
        let mut tx = self.pool.begin().await?;
        self.name_merchant(&mut tx).await?;
        Ok(tx)
    }

    /// Names this scope's merchant, if it has one, for the rest of this database
    /// transaction. Never for the session: a pooled connection would keep it, and
    /// the next request could inherit another merchant.
    pub(crate) async fn name_merchant(
        self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> sqlx::Result<()> {
        if let Some(merchant) = self.merchant {
            sqlx::query!(
                "SELECT set_config('app.merchant', $1, true)",
                merchant.get().to_string()
            )
            .fetch_one(&mut **tx)
            .await?;
        }
        Ok(())
    }
}
