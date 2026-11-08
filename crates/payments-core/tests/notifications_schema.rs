// Author: Nicola Antonello

//! Database tests for `psp_notifications` (ADR-0030, ADR-0031, ADR-0032, ADR-0037).
//!
//! Rows are written with raw SQL as the system role, `payments_app`, which takes the
//! PSP's notifications, the way a buggy code path or a manual session could write
//! them. Each rejected write breaks a single rule. The test asserts the SQLSTATE and,
//! where PostgreSQL reports one, the constraint or trigger that rejects the write;
//! privilege errors carry none.

mod common;

use common::{
    APPEND_ONLY, CHECK_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION, assert_guard,
    assert_violation, runtime_connection, runtime_transaction, sqlstate, tenant_login,
};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

/// A notification as the system role writes it: every column except `recorded_at`.
#[derive(Debug, Clone, Copy)]
struct Notification<'a> {
    psp: &'a str,
    event_id: &'a str,
    payment_id: Uuid,
}

impl<'a> Notification<'a> {
    /// A notification from the mock PSP about a payment that no one created: a
    /// notification names a payment, and references none.
    fn new(event_id: &'a str) -> Self {
        Self {
            psp: "mock",
            event_id,
            payment_id: Uuid::now_v7(),
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO psp_notifications (psp, event_id, payment_id) VALUES ($1, $2, $3)",
            self.psp,
            self.event_id,
            self.payment_id
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// Either end of each range, the two other characters, and an ID of a common shape.
const GOOD_IDS: [&str; 9] = [
    "0",
    "9",
    "A",
    "Z",
    "a",
    "z",
    "_",
    "-",
    "msg_2KWPBgLlAfxdpx2AI54pPJ85f4W",
];
/// A '.', which would let `msg_id.timestamp.payload` read two ways (ADR-0030), and
/// what else is outside the shape.
const BAD_IDS: [&str; 8] = ["", ".", "msg.1", "msg_1.", " ", "a b", "é", "msg/1"];

#[sqlx::test]
async fn an_event_id_has_one_shape_without_a_dot(pool: PgPool) -> sqlx::Result<()> {
    let at_the_limit = "e".repeat(255);
    let over_the_limit = "e".repeat(256);
    let new_line = "a\nb";

    let mut tx = runtime_transaction(&pool).await?;
    for event_id in GOOD_IDS.iter().copied().chain([at_the_limit.as_str()]) {
        Notification::new(event_id).insert(&mut tx).await?;
    }
    tx.rollback().await?;

    let mut app = runtime_connection(&pool).await?;
    let bad = BAD_IDS
        .iter()
        .copied()
        .chain([over_the_limit.as_str(), new_line]);
    for event_id in bad {
        let err = Notification::new(event_id)
            .insert(&mut app)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "psp_notifications_event_id_check");
    }
    Ok(())
}

#[sqlx::test]
async fn an_event_is_recorded_once_per_psp(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let first = Notification::new("evt_1");
    first.insert(&mut app).await?;

    // A replay, and the same event ID naming another payment.
    for copy in [
        first,
        Notification {
            payment_id: Uuid::now_v7(),
            ..first
        },
    ] {
        let err = copy.insert(&mut app).await.unwrap_err();
        assert_violation(err, UNIQUE_VIOLATION, "psp_notifications_pkey");
    }
    let err = Notification {
        psp: "stripe",
        ..Notification::new("evt_2")
    }
    .insert(&mut app)
    .await
    .unwrap_err();
    assert_violation(err, CHECK_VIOLATION, "psp_notifications_psp_check");
    Ok(())
}

#[sqlx::test]
async fn a_notification_may_name_any_payment(pool: PgPool) -> sqlx::Result<()> {
    // The payment may be one that the service never created, or a value that is
    // not even a UUIDv7: it is recorded, so that no retry is asked for, and an
    // operator looks (ADR-0032).
    let mut app = runtime_connection(&pool).await?;
    Notification::new("evt_1").insert(&mut app).await?;
    Notification {
        payment_id: Uuid::nil(),
        ..Notification::new("evt_2")
    }
    .insert(&mut app)
    .await?;
    let recorded = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM psp_notifications"#)
        .fetch_one(&mut app)
        .await?;
    assert_eq!(recorded, 2);
    Ok(())
}

#[sqlx::test]
async fn only_the_system_role_records_one_and_neither_role_changes_one(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut owner = pool.acquire().await?;
    Notification::new("evt_1").insert(&mut owner).await?;
    let mut app = runtime_connection(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    for err in [
        // A merchant has nothing to do with the PSP's notifications.
        sqlx::query!("SELECT event_id FROM psp_notifications")
            .fetch_all(&mut tenant)
            .await
            .unwrap_err(),
        Notification::new("evt_2")
            .insert(&mut tenant)
            .await
            .unwrap_err(),
        // Only the database chooses a recorded_at (ADR-0012).
        sqlx::query!(
            "INSERT INTO psp_notifications (psp, event_id, payment_id, recorded_at) \
             VALUES ('mock', 'evt_3', $1, now())",
            Uuid::now_v7()
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    for conn in [&mut app, &mut tenant] {
        for err in [
            sqlx::query!("UPDATE psp_notifications SET event_id = 'evt_4'")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM psp_notifications")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE psp_notifications")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
        }
    }
    tenant.close().await
}

#[sqlx::test]
async fn notifications_are_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    // A notification forgotten would turn its replay into a new event.
    let mut owner = pool.acquire().await?;
    Notification::new("evt_1").insert(&mut owner).await?;

    let table = "psp_notifications";
    let update = sqlx::query!("UPDATE psp_notifications SET event_id = 'evt_2'")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "psp_notifications_append_only", table);
    let delete = sqlx::query!("DELETE FROM psp_notifications")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "psp_notifications_append_only", table);
    let truncate = sqlx::query!("TRUNCATE psp_notifications")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "psp_notifications_no_truncate",
        table,
    );
    Ok(())
}
