// Author: Nicola Antonello

//! Database tests for `ledger_transactions` and `ledger_entries`
//! (ADR-0004, ADR-0010, ADR-0012, ADR-0013).
//!
//! The postings here are written with raw SQL, bypassing the domain type,
//! the way a buggy code path or a manual session could write them. They run
//! as the runtime role, `payments_app`, unless a test says otherwise. Each
//! rejected write breaks a single rule. The test asserts the SQLSTATE and,
//! where PostgreSQL reports one, the constraint or trigger that rejects the
//! write; privilege errors and NOT NULL violations carry none. For the
//! ledger's own guards it also asserts the table and schema, and the DETAIL
//! where the error reports numbers (ADR-0013).

mod common;

use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE,
    NOT_NULL_VIOLATION, UNIQUE_VIOLATION, assert_guard, assert_violation, runtime_connection,
    runtime_transaction, sqlstate,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// SQLSTATE of an incomplete ledger transaction (ADR-0013).
const INCOMPLETE: &str = "LG002";
/// SQLSTATE of an unbalanced ledger transaction (ADR-0013).
const UNBALANCED: &str = "LG003";
/// The trigger that checks each ledger transaction at COMMIT.
const COMMIT_CHECK: &str = "ledger_transactions_commit_check";
/// The largest magnitude of a single amount (ADR-0003).
const CAP: i64 = 1_000_000_000_000_000;

/// The accounts the postings move money between: three in EUR, one in USD.
struct Accounts {
    receivable: Uuid,
    payable: Uuid,
    fees: Uuid,
    usd_receivable: Uuid,
}

async fn open_account(pool: &PgPool, kind: &str, currency: &str, side: &str) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO accounts (id, kind, party, currency, normal_side) \
         VALUES ($1, $2, 'platform', $3, $4)",
        id,
        kind,
        currency,
        side
    )
    .execute(pool)
    .await?;
    Ok(id)
}

async fn open_accounts(pool: &PgPool) -> sqlx::Result<Accounts> {
    Ok(Accounts {
        receivable: open_account(pool, "psp_receivable", "EUR", "debit").await?,
        payable: open_account(pool, "merchant_payable", "EUR", "credit").await?,
        fees: open_account(pool, "fee_revenue", "EUR", "credit").await?,
        usd_receivable: open_account(pool, "psp_receivable", "USD", "debit").await?,
    })
}

/// A ledger transaction as raw SQL writes it: a header, then one entry per
/// line, numbered from 1.
#[derive(Debug, Clone)]
struct Posting {
    id: Uuid,
    entry_count: i32,
    currency: &'static str,
    /// The account and the amount of lines 1, 2, and so on.
    lines: Vec<(Uuid, i64)>,
}

impl Posting {
    /// An EUR posting with one line per amount, declaring as many entries as
    /// it has lines. The lines take the EUR accounts in turn.
    fn eur(accounts: &Accounts, amounts: &[i64]) -> Self {
        let eur_accounts = [accounts.receivable, accounts.payable, accounts.fees];
        Self {
            id: Uuid::now_v7(),
            entry_count: i32::try_from(amounts.len()).expect("a handful of lines"),
            currency: "EUR",
            lines: amounts
                .iter()
                .zip(eur_accounts.iter().cycle())
                .map(|(&amount, &account)| (account, amount))
                .collect(),
        }
    }

    /// A capture of 100.00 EUR: 97.00 owed to the merchant, 3.00 of fees.
    fn capture(accounts: &Accounts) -> Self {
        Self::eur(accounts, &[10_000, -9_700, -300])
    }

    async fn insert_header(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, $3)",
            self.id,
            self.entry_count,
            self.currency
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Inserts one entry of this ledger transaction. The account, currency
    /// and amount are explicit, so a test can break one of them.
    async fn insert_line(
        &self,
        conn: &mut PgConnection,
        line_no: i32,
        account_id: Uuid,
        currency: &str,
        amount: i64,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO ledger_entries \
             (transaction_id, line_no, entry_count, account_id, currency, amount) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            self.id,
            line_no,
            self.entry_count,
            account_id,
            currency,
            amount
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Writes the header, then every line.
    async fn write(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        self.insert_header(conn).await?;
        for (line_no, &(account_id, amount)) in (1..).zip(&self.lines) {
            self.insert_line(conn, line_no, account_id, self.currency, amount)
                .await?;
        }
        Ok(())
    }
}

/// How many headers and entries are stored.
async fn stored(pool: &PgPool) -> sqlx::Result<(i64, i64)> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM ledger_transactions) AS "headers!",
                  (SELECT count(*) FROM ledger_entries) AS "entries!""#
    )
    .fetch_one(pool)
    .await?;
    Ok((row.headers, row.entries))
}

/// The trial balance: every currency whose entries do not sum to zero
/// across the whole ledger. Invariant I1 says there are none.
async fn unbalanced_currencies(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"SELECT currency AS "currency!" FROM ledger_entries
            GROUP BY currency HAVING sum(amount) <> 0"#
    )
    .fetch_all(pool)
    .await
}

#[sqlx::test]
async fn a_balanced_ledger_transaction_commits(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    Posting::capture(&accounts).write(&mut tx).await?;
    tx.commit().await?;

    assert_eq!(stored(&pool).await?, (1, 3));
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    Ok(())
}

#[sqlx::test]
async fn ledger_transactions_commit_together_and_one_after_another(
    pool: PgPool,
) -> sqlx::Result<()> {
    // The check counts and sums each ledger transaction's own entries, so
    // several in one database transaction, as a settlement batch will post
    // them, and others committed earlier do not disturb it (ADR-0004).
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    Posting::capture(&accounts).write(&mut tx).await?;
    Posting::eur(&accounts, &[500, -500]).write(&mut tx).await?;
    tx.commit().await?;

    let mut tx = runtime_transaction(&pool).await?;
    Posting::capture(&accounts).write(&mut tx).await?;
    tx.commit().await?;

    assert_eq!(stored(&pool).await?, (3, 8));
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    Ok(())
}

#[sqlx::test]
async fn amounts_up_to_the_cap_commit(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    Posting::eur(&accounts, &[CAP, -CAP]).write(&mut tx).await?;
    tx.commit().await?;
    assert_eq!(stored(&pool).await?, (1, 2));
    Ok(())
}

#[sqlx::test]
async fn an_incomplete_ledger_transaction_cannot_commit(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let three_declared_two_written = Posting {
        entry_count: 3,
        ..Posting::eur(&accounts, &[100, -100])
    };
    three_declared_two_written.write(&mut tx).await?;

    let err = tx.commit().await.unwrap_err();
    let detail = assert_guard(err, INCOMPLETE, COMMIT_CHECK, "ledger_transactions");
    assert_eq!(detail, "It declares 3 entries and has 2.");
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn a_header_without_entries_cannot_commit(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    Posting::capture(&accounts).insert_header(&mut tx).await?;

    let err = tx.commit().await.unwrap_err();
    let detail = assert_guard(err, INCOMPLETE, COMMIT_CHECK, "ledger_transactions");
    assert_eq!(detail, "It declares 3 entries and has 0.");
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn an_unbalanced_ledger_transaction_cannot_commit(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    // Debits over credits, then credits over debits: the check compares the
    // sum with zero, so it must reject an imbalance of either sign.
    for (amounts, sum) in [([100, -90], 10), ([90, -100], -10)] {
        let mut tx = runtime_transaction(&pool).await?;
        Posting::eur(&accounts, &amounts).write(&mut tx).await?;
        let err = tx.commit().await.unwrap_err();
        let detail = assert_guard(err, UNBALANCED, COMMIT_CHECK, "ledger_transactions");
        assert_eq!(
            detail,
            format!("Its entries sum to {sum} minor units of EUR.")
        );
    }

    // The same entries under a declared count of 3 are reported as
    // incomplete: the check tests completeness first (ADR-0013). That is
    // why every test asserts the code of the rule it breaks.
    let mut tx = runtime_transaction(&pool).await?;
    let unbalanced_and_incomplete = Posting {
        entry_count: 3,
        ..Posting::eur(&accounts, &[100, -90])
    };
    unbalanced_and_incomplete.write(&mut tx).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, INCOMPLETE, COMMIT_CHECK, "ledger_transactions");
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn a_ledger_transaction_id_names_one_header(pool: PgPool) -> sqlx::Result<()> {
    // The commit-time check finds entries by transaction_id alone, so it is
    // sound only while id alone is unique. Without the primary key, a header
    // in another currency could share this id and pass the check with the
    // EUR entries (ADR-0010).
    let accounts = open_accounts(&pool).await?;
    let posting = Posting::eur(&accounts, &[100, -100]);
    let mut tx = runtime_transaction(&pool).await?;
    posting.write(&mut tx).await?;
    tx.commit().await?;

    // Another currency, so that only the primary key can reject it.
    let mut app = runtime_connection(&pool).await?;
    let same_id_in_usd = Posting {
        currency: "USD",
        ..posting
    };
    let err = same_id_in_usd.insert_header(&mut app).await.unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "ledger_transactions_pkey");
    assert_eq!(stored(&pool).await?, (1, 2));
    Ok(())
}

#[sqlx::test]
async fn a_mixed_currency_entry_fails_at_insert(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let posting = Posting::eur(&accounts, &[100, -100]);
    let mut tx = runtime_transaction(&pool).await?;
    posting.insert_header(&mut tx).await?;
    // Line 1 proves that the ID and the count are right, so line 2, which
    // differs only in its currency and its USD account, breaks one rule.
    posting
        .insert_line(&mut tx, 1, accounts.receivable, "EUR", 100)
        .await?;
    let err = posting
        .insert_line(&mut tx, 2, accounts.usd_receivable, "USD", -100)
        .await
        .unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "ledger_entries_header_fkey");
    tx.rollback().await?;

    // The key is not deferrable, so SET CONSTRAINTS cannot postpone it.
    let mut tx = runtime_transaction(&pool).await?;
    sqlx::query!("SET CONSTRAINTS ALL DEFERRED")
        .execute(&mut *tx)
        .await?;
    posting.insert_header(&mut tx).await?;
    posting
        .insert_line(&mut tx, 1, accounts.receivable, "EUR", 100)
        .await?;
    let err = posting
        .insert_line(&mut tx, 2, accounts.usd_receivable, "USD", -100)
        .await
        .unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "ledger_entries_header_fkey");
    tx.rollback().await?;
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn an_entry_needs_its_header(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    let never_inserted = Posting::eur(&accounts, &[100, -100]);
    let err = never_inserted
        .insert_line(&mut app, 1, accounts.receivable, "EUR", 100)
        .await
        .unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "ledger_entries_header_fkey");
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn an_entry_has_its_account_currency(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let posting = Posting::eur(&accounts, &[100, -100]);
    // The second time with every deferrable constraint deferred: the key is
    // not deferrable, so the entry still fails at its INSERT.
    for defer in [false, true] {
        let mut tx = runtime_transaction(&pool).await?;
        if defer {
            sqlx::query!("SET CONSTRAINTS ALL DEFERRED")
                .execute(&mut *tx)
                .await?;
        }
        posting.insert_header(&mut tx).await?;
        // An EUR entry, as its header declares, but on the USD account.
        let err = posting
            .insert_line(&mut tx, 1, accounts.usd_receivable, "EUR", 100)
            .await
            .unwrap_err();
        assert_violation(err, FOREIGN_KEY_VIOLATION, "ledger_entries_account_fkey");
        tx.rollback().await?;
    }
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn a_committed_ledger_transaction_cannot_gain_entries(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let posting = Posting::eur(&accounts, &[100, -100]);
    let mut tx = runtime_transaction(&pool).await?;
    posting.write(&mut tx).await?;
    tx.commit().await?;

    // Each attempt breaks one rule of the seal (ADR-0004).
    let mut app = runtime_connection(&pool).await?;
    // Line 1 holds the receivable entry. The key is (transaction_id,
    // line_no), so the line stays used on any other account too.
    let reused_line = posting
        .insert_line(&mut app, 1, accounts.fees, "EUR", 5)
        .await
        .unwrap_err();
    assert_violation(reused_line, UNIQUE_VIOLATION, "ledger_entries_pkey");

    // One line below the first, and one beyond the declared count.
    for line_no in [0, 3] {
        let outside_the_lines = posting
            .insert_line(&mut app, line_no, accounts.receivable, "EUR", 5)
            .await
            .unwrap_err();
        assert_violation(
            outside_the_lines,
            CHECK_VIOLATION,
            "ledger_entries_line_no_check",
        );
    }

    // A NULL count would make the header's key skip its check (MATCH
    // SIMPLE) and the line_no CHECK pass, so NOT NULL keeps the entry out.
    let null_count = sqlx::query!(
        "INSERT INTO ledger_entries \
         (transaction_id, line_no, entry_count, account_id, currency, amount) \
         VALUES ($1, 3, NULL, $2, 'EUR', 5)",
        posting.id,
        accounts.receivable
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(null_count), NOT_NULL_VIOLATION);

    let claimed_count = Posting {
        entry_count: 3,
        ..posting.clone()
    };
    let wrong_count = claimed_count
        .insert_line(&mut app, 3, accounts.receivable, "EUR", 5)
        .await
        .unwrap_err();
    assert_violation(
        wrong_count,
        FOREIGN_KEY_VIOLATION,
        "ledger_entries_header_fkey",
    );
    assert_eq!(stored(&pool).await?, (1, 2));
    Ok(())
}

#[sqlx::test]
async fn checking_early_lets_no_entry_in_afterwards(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let posting = Posting::eur(&accounts, &[100, -100]);
    let mut tx = runtime_transaction(&pool).await?;
    posting.write(&mut tx).await?;
    // Runs the commit-time check now, while the ledger transaction is
    // complete and balanced.
    sqlx::query!("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *tx)
        .await?;

    let err = posting
        .insert_line(&mut tx, 3, accounts.receivable, "EUR", 5)
        .await
        .unwrap_err();
    assert_violation(err, CHECK_VIOLATION, "ledger_entries_line_no_check");
    tx.rollback().await?;
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn amounts_are_non_zero_and_within_the_cap(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    for amount in [0, CAP + 1, -(CAP + 1)] {
        let posting = Posting::eur(&accounts, &[amount, 1]);
        let mut tx = runtime_transaction(&pool).await?;
        posting.insert_header(&mut tx).await?;
        let err = posting
            .insert_line(&mut tx, 1, accounts.receivable, "EUR", amount)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "ledger_entries_amount_check");
        tx.rollback().await?;
    }

    // A NULL amount would pass the CHECK, which a NULL does not fail, and
    // count(*) would count it while sum() skipped it.
    let posting = Posting::eur(&accounts, &[100, -100]);
    let mut tx = runtime_transaction(&pool).await?;
    posting.insert_header(&mut tx).await?;
    let null_amount = sqlx::query!(
        "INSERT INTO ledger_entries \
         (transaction_id, line_no, entry_count, account_id, currency, amount) \
         VALUES ($1, 1, 2, $2, 'EUR', NULL)",
        posting.id,
        accounts.receivable
    )
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(null_amount), NOT_NULL_VIOLATION);
    tx.rollback().await?;
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn headers_declare_at_least_two_entries_and_a_uuidv7(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    let one_entry = Posting {
        entry_count: 1,
        ..Posting::capture(&accounts)
    };
    let err = one_entry.insert_header(&mut app).await.unwrap_err();
    assert_violation(
        err,
        CHECK_VIOLATION,
        "ledger_transactions_entry_count_check",
    );

    // The nil UUID, and a version 7 without the RFC 9562 variant, make
    // uuid_extract_version return NULL, which the CHECK must not accept.
    let not_rfc_variant = uuid::uuid!("00000000-0000-7000-0000-000000000000");
    let version_4 = uuid::uuid!("00000000-0000-4000-8000-000000000000");
    let version_8 = uuid::uuid!("00000000-0000-8000-8000-000000000000");
    for id in [Uuid::nil(), not_rfc_variant, version_4, version_8] {
        let posting = Posting {
            id,
            ..Posting::capture(&accounts)
        };
        let err = posting.insert_header(&mut app).await.unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "ledger_transactions_id_check");
    }
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn the_runtime_role_cannot_choose_a_header_recorded_at(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let chosen = sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency, recorded_at) \
         VALUES ($1, 2, 'EUR', '2000-01-01 00:00:00+00')",
        Uuid::now_v7()
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(chosen), INSUFFICIENT_PRIVILEGE);

    let default = sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency, recorded_at) \
         VALUES ($1, 2, 'EUR', DEFAULT)",
        Uuid::now_v7()
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(default), INSUFFICIENT_PRIVILEGE);
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn a_header_is_stamped_with_its_transaction_start(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(&pool).await?;
    // Let the clock move on, so that the start of the transaction and the
    // start of the insert are different instants.
    sqlx::query!(r#"SELECT 1 AS "slept!" FROM pg_sleep(0.01)"#)
        .fetch_one(&mut *tx)
        .await?;
    let stamp = sqlx::query!(
        r#"INSERT INTO ledger_transactions (id, entry_count, currency)
           VALUES ($1, 2, 'EUR')
           RETURNING recorded_at = transaction_timestamp() AS "at_transaction_start!",
                     recorded_at < statement_timestamp() AS "before_the_insert!""#,
        Uuid::now_v7()
    )
    .fetch_one(&mut *tx)
    .await?;
    assert!(stamp.at_transaction_start, "not the transaction's start");
    assert!(stamp.before_the_insert, "not earlier than the insert");
    // A header alone could not commit, so the test rolls back.
    tx.rollback().await
}

#[sqlx::test]
async fn a_temporary_table_cannot_stand_in_for_the_entries(pool: PgPool) -> sqlx::Result<()> {
    // The attack the pinned search_path stops (ADR-0004): a temporary table
    // is found before public by unqualified names, so without the pin the
    // commit-time check would read this balanced decoy.
    let accounts = open_accounts(&pool).await?;
    let id = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    sqlx::query!(
        "INSERT INTO public.ledger_transactions (id, entry_count, currency) \
         VALUES ($1, 2, 'EUR')",
        id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO public.ledger_entries \
         (transaction_id, line_no, entry_count, account_id, currency, amount) \
         VALUES ($1, 1, 2, $2, 'EUR', 100), ($1, 2, 2, $3, 'EUR', -90)",
        id,
        accounts.receivable,
        accounts.payable
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        "CREATE TEMPORARY TABLE ledger_entries (LIKE public.ledger_entries) ON COMMIT DROP"
    )
    .execute(&mut *tx)
    .await?;
    // Checked against public.ledger_entries when the test is compiled; at
    // run time the unqualified name finds the temporary table.
    sqlx::query!(
        "INSERT INTO ledger_entries \
         (transaction_id, line_no, entry_count, account_id, currency, amount) \
         VALUES ($1, 1, 2, $2, 'EUR', 100), ($1, 2, 2, $3, 'EUR', -100)",
        id,
        accounts.receivable,
        accounts.payable
    )
    .execute(&mut *tx)
    .await?;

    let err = tx.commit().await.unwrap_err();
    // The check read the real entries, which sum to 10, not the decoy.
    let detail = assert_guard(err, UNBALANCED, COMMIT_CHECK, "ledger_transactions");
    assert_eq!(detail, "Its entries sum to 10 minor units of EUR.");
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn an_operator_in_public_cannot_stand_in_for_the_balance_check(
    pool: PgPool,
) -> sqlx::Result<()> {
    // The other attack on the check's search_path (ADR-0004): public is on
    // it, and PostgreSQL prefers an exact operator anywhere on the path over
    // one that needs a cast. pg_catalog has no <> for numeric and integer,
    // nor for bigint and numeric, so operators like these would decide the
    // balance comparison if it used a plain 0, or held the sum in a bigint;
    // the check compares a numeric sum with 0::numeric instead. The runtime
    // role cannot create objects in public (the catalog tests pin that), so
    // the owner plants the operators, and the runtime role posts.
    let accounts = open_accounts(&pool).await?;
    let mut owner = pool.acquire().await?;
    sqlx::query!(
        "CREATE FUNCTION public.never_differs(numeric, integer) RETURNS boolean \
         LANGUAGE sql IMMUTABLE AS 'SELECT false'"
    )
    .execute(&mut *owner)
    .await?;
    sqlx::query!(
        "CREATE OPERATOR public.<> \
         (LEFTARG = numeric, RIGHTARG = integer, FUNCTION = public.never_differs)"
    )
    .execute(&mut *owner)
    .await?;
    sqlx::query!(
        "CREATE FUNCTION public.never_differs(bigint, numeric) RETURNS boolean \
         LANGUAGE sql IMMUTABLE AS 'SELECT false'"
    )
    .execute(&mut *owner)
    .await?;
    sqlx::query!(
        "CREATE OPERATOR public.<> \
         (LEFTARG = bigint, RIGHTARG = numeric, FUNCTION = public.never_differs)"
    )
    .execute(&mut *owner)
    .await?;
    drop(owner);

    let mut tx = runtime_transaction(&pool).await?;
    Posting::eur(&accounts, &[100, -90]).write(&mut tx).await?;
    let err = tx.commit().await.unwrap_err();
    let detail = assert_guard(err, UNBALANCED, COMMIT_CHECK, "ledger_transactions");
    assert_eq!(detail, "Its entries sum to 10 minor units of EUR.");
    assert_eq!(stored(&pool).await?, (0, 0));
    Ok(())
}

#[sqlx::test]
async fn the_guards_fire_in_replica_mode(pool: PgPool) -> sqlx::Result<()> {
    // session_replication_role = replica stops ordinary triggers, and the
    // internal triggers behind foreign keys. ENABLE ALWAYS keeps the
    // ledger's own triggers firing (ADR-0004). Only a superuser or a role
    // granted SET on the setting can set it; the test pool connects as the
    // bootstrap superuser (ADR-0008).
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    Posting::capture(&accounts).write(&mut tx).await?;
    tx.commit().await?;

    let mut tx = pool.begin().await?;
    sqlx::query!("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    let err = sqlx::query!("UPDATE ledger_entries SET line_no = line_no")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_guard(
        err,
        APPEND_ONLY,
        "ledger_entries_append_only",
        "ledger_entries",
    );
    tx.rollback().await?;

    let mut tx = pool.begin().await?;
    sqlx::query!("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    Posting::eur(&accounts, &[100, -90]).write(&mut tx).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, UNBALANCED, COMMIT_CHECK, "ledger_transactions");
    assert_eq!(stored(&pool).await?, (1, 3));
    Ok(())
}

#[sqlx::test]
async fn the_runtime_role_cannot_change_the_ledger(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    Posting::capture(&accounts).write(&mut tx).await?;
    tx.commit().await?;

    let mut app = runtime_connection(&pool).await?;
    let attempts = [
        sqlx::query!("UPDATE ledger_transactions SET entry_count = entry_count")
            .execute(&mut app)
            .await,
        sqlx::query!("DELETE FROM ledger_transactions")
            .execute(&mut app)
            .await,
        sqlx::query!("TRUNCATE ledger_transactions")
            .execute(&mut app)
            .await,
        sqlx::query!("UPDATE ledger_entries SET line_no = line_no")
            .execute(&mut app)
            .await,
        sqlx::query!("DELETE FROM ledger_entries")
            .execute(&mut app)
            .await,
        sqlx::query!("TRUNCATE ledger_entries")
            .execute(&mut app)
            .await,
    ];
    for attempt in attempts {
        assert_eq!(sqlstate(attempt.unwrap_err()), INSUFFICIENT_PRIVILEGE);
    }
    assert_eq!(stored(&pool).await?, (1, 3));
    Ok(())
}

#[sqlx::test]
async fn the_ledger_is_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    let accounts = open_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    Posting::capture(&accounts).write(&mut tx).await?;
    tx.commit().await?;

    // The test pool connects as the role that owns the tables (ADR-0008).
    let mut owner = pool.acquire().await?;
    let header_update = sqlx::query!("UPDATE ledger_transactions SET entry_count = entry_count")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    let headers = "ledger_transactions";
    assert_guard(
        header_update,
        APPEND_ONLY,
        "ledger_transactions_append_only",
        headers,
    );
    let header_delete = sqlx::query!("DELETE FROM ledger_transactions")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        header_delete,
        APPEND_ONLY,
        "ledger_transactions_append_only",
        headers,
    );
    // CASCADE, because the entries' foreign key refers to the headers.
    let header_truncate = sqlx::query!("TRUNCATE ledger_transactions CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        header_truncate,
        APPEND_ONLY,
        "ledger_transactions_no_truncate",
        headers,
    );

    let entries = "ledger_entries";
    let entry_update = sqlx::query!("UPDATE ledger_entries SET line_no = line_no")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        entry_update,
        APPEND_ONLY,
        "ledger_entries_append_only",
        entries,
    );
    let entry_delete = sqlx::query!("DELETE FROM ledger_entries")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        entry_delete,
        APPEND_ONLY,
        "ledger_entries_append_only",
        entries,
    );
    let entry_truncate = sqlx::query!("TRUNCATE ledger_entries")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        entry_truncate,
        APPEND_ONLY,
        "ledger_entries_no_truncate",
        entries,
    );
    assert_eq!(stored(&pool).await?, (1, 3));
    Ok(())
}
