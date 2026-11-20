// Author: Nicola Antonello

//! The relay's search (ADR-0062) against PostgreSQL's own snapshots: the first search reads
//! every delivery not finished, and each later one only the transactions that had not
//! completed at the previous search's snapshot, with a transaction left open here, and one
//! in another database. The system role searches, as the relay will.

mod common;

use std::error::Error;

use common::outbox::{disable, event, insert_endpoint, new_payment, succeed, write_event};
use common::{delivery_id, endpoint_id, event_id, runtime_pool, runtime_transaction};
use payments_core::deliveries::search;
use payments_core::relay::{Delivery, Search, Snapshot, XactId};
use sqlx::{Connection, PgConnection, PgPool};

type TestResult = Result<(), Box<dyn Error>>;

/// Where the endpoints are. No test here posts to them.
const URL: &str = "http://localhost:8080/webhooks";

/// Delivery `n` of event `event` to endpoint `endpoint`, as a search returns it: with the
/// transaction that wrote it, as `pool` reads it.
async fn delivery(
    pool: &PgPool,
    n: u32,
    endpoint: u32,
    event: u32,
) -> Result<Delivery, Box<dyn Error>> {
    Ok(Delivery {
        id: delivery_id(n),
        endpoint: endpoint_id(endpoint),
        event: event_id(event),
        xact: writer_of(pool, n).await?,
    })
}

/// The ID of the transaction that `conn` is in, which PostgreSQL assigns if it has not.
async fn transaction_of(conn: &mut PgConnection) -> Result<XactId, Box<dyn Error>> {
    let id = sqlx::query_scalar!(r#"SELECT pg_current_xact_id()::text::bigint AS "id!""#)
        .fetch_one(conn)
        .await?;
    Ok(XactId::new(u64::try_from(id)?))
}

/// The ID of the transaction that wrote delivery `n`.
async fn writer_of(pool: &PgPool, n: u32) -> Result<XactId, Box<dyn Error>> {
    let id = sqlx::query_scalar!(
        r#"SELECT xact_id::text::bigint AS "id!" FROM webhook_deliveries WHERE id = $1"#,
        delivery_id(n).get()
    )
    .fetch_one(pool)
    .await?;
    Ok(XactId::new(u64::try_from(id)?))
}

#[sqlx::test]
async fn the_first_search_reads_every_delivery_not_finished(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (merchant, payment) = new_payment(&system).await?;
    for n in 1..=3 {
        insert_endpoint(&system, endpoint_id(n), merchant, URL).await?;
    }
    event(
        &system,
        merchant,
        payment,
        1,
        event_id(1),
        &[
            (delivery_id(1), endpoint_id(1)),
            (delivery_id(2), endpoint_id(2)),
            (delivery_id(3), endpoint_id(3)),
        ],
    )
    .await?;
    event(
        &system,
        merchant,
        payment,
        2,
        event_id(2),
        &[(delivery_id(4), endpoint_id(1))],
    )
    .await?;
    // Delivery 1 succeeded, and endpoint 2 is disabled: neither delivery is left to send.
    succeed(&system, delivery_id(1)).await?;
    disable(&system, endpoint_id(2), "operator").await?;

    let (_, found) = search(&system, &Search::Full).await?;
    assert_eq!(
        found,
        [
            delivery(&system, 3, 3, 1).await?,
            delivery(&system, 4, 1, 2).await?
        ]
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_later_search_reads_only_what_completed_after_the_previous_snapshot(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (merchant, payment) = new_payment(&system).await?;
    for n in 1..=3 {
        insert_endpoint(&system, endpoint_id(n), merchant, URL).await?;
    }
    event(
        &system,
        merchant,
        payment,
        1,
        event_id(1),
        &[(delivery_id(1), endpoint_id(1))],
    )
    .await?;
    let (first, found) = search(&system, &Search::Full).await?;
    assert_eq!(found, [delivery(&system, 1, 1, 1).await?]);

    event(
        &system,
        merchant,
        payment,
        2,
        event_id(2),
        &[
            (delivery_id(2), endpoint_id(1)),
            (delivery_id(3), endpoint_id(2)),
            (delivery_id(4), endpoint_id(3)),
        ],
    )
    .await?;
    // Of the deliveries written since, delivery 3 succeeded and endpoint 3 is disabled.
    succeed(&system, delivery_id(3)).await?;
    disable(&system, endpoint_id(3), "operator").await?;
    let (second, found) = search(&system, &Search::Since(first)).await?;
    // Delivery 1 is not finished, but its transaction completed before the first snapshot.
    assert_eq!(found, [delivery(&system, 2, 1, 2).await?]);
    // Nothing completed after the second snapshot.
    let (_, found) = search(&system, &Search::Since(second)).await?;
    assert!(found.is_empty(), "{found:?}");
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_later_search_reads_its_previous_snapshots_list_and_from_its_xmax_up(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (merchant, payment) = new_payment(&system).await?;
    insert_endpoint(&system, endpoint_id(1), merchant, URL).await?;
    for (n, position) in [(1, 1), (2, 2), (3, 3)] {
        event(
            &system,
            merchant,
            payment,
            position,
            event_id(n),
            &[(delivery_id(n), endpoint_id(1))],
        )
        .await?;
    }
    // A snapshot made up from the three writers, in their order: the first completed,
    // the second in progress below xmax, and the third exactly at xmax, which no
    // transaction at or above had completed.
    let [_, second, third] = [
        writer_of(&system, 1).await?,
        writer_of(&system, 2).await?,
        writer_of(&system, 3).await?,
    ];
    let previous = Snapshot {
        xmax: third,
        in_progress: vec![second],
    };

    let (_, found) = search(&system, &Search::Since(previous)).await?;
    assert_eq!(
        found,
        [
            delivery(&system, 2, 1, 2).await?,
            delivery(&system, 3, 1, 3).await?
        ]
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_transaction_held_open_in_another_database_holds_no_later_search_back(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (merchant, payment) = new_payment(&system).await?;
    insert_endpoint(&system, endpoint_id(1), merchant, URL).await?;
    // A write transaction in the maintenance database, open before event 1 is written,
    // holds back the xmin of every snapshot on the server, as it held back ADR-0052's mark:
    // a search from xmin would read delivery 1 again at every search.
    let options = (*pool.connect_options()).clone().database("postgres");
    let mut elsewhere = PgConnection::connect_with(&options).await?;
    let mut open = elsewhere.begin().await?;
    let held = transaction_of(&mut open).await?;
    event(
        &system,
        merchant,
        payment,
        1,
        event_id(1),
        &[(delivery_id(1), endpoint_id(1))],
    )
    .await?;

    let (first, _) = search(&system, &Search::Full).await?;
    // Event 1's transaction completed after the open one began, so the open one is below
    // the snapshot's xmax, in its list.
    assert!(first.in_progress.contains(&held), "{held:?} in {first:?}");
    event(
        &system,
        merchant,
        payment,
        2,
        event_id(2),
        &[(delivery_id(2), endpoint_id(1))],
    )
    .await?;
    let (_, found) = search(&system, &Search::Since(first)).await?;
    // Delivery 2 alone: not delivery 1, whose transaction completed before the first
    // snapshot, though the open transaction began before both.
    assert_eq!(found, [delivery(&system, 2, 1, 2).await?]);
    open.rollback().await?;
    elsewhere.close().await?;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_event_whose_transaction_is_open_is_found_once_it_commits(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (merchant, payment) = new_payment(&system).await?;
    insert_endpoint(&system, endpoint_id(1), merchant, URL).await?;
    // The catch-up's transaction writes the event and its delivery, and stays open
    // through the first search.
    let mut open = runtime_transaction(&pool).await?;
    write_event(
        &mut open,
        merchant,
        payment,
        1,
        event_id(1),
        &[(delivery_id(1), endpoint_id(1))],
    )
    .await?;
    let writer = transaction_of(&mut open).await?;
    // A later transaction completes first, which puts the open one below the snapshot's
    // xmax, in its list: the later search reads it through the list.
    insert_endpoint(&system, endpoint_id(2), merchant, URL).await?;

    let (first, found) = search(&system, &Search::Full).await?;
    assert!(found.is_empty(), "{found:?}");
    assert!(
        first.in_progress.contains(&writer),
        "{writer:?} in {first:?}"
    );
    open.commit().await?;
    let (_, found) = search(&system, &Search::Since(first)).await?;
    assert_eq!(found, [delivery(&system, 1, 1, 1).await?]);
    system.close().await;
    Ok(())
}
