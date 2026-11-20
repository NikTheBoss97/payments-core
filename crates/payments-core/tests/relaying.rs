// Author: Nicola Antonello

//! The relay's task (ADR-0052, ADR-0053, ADR-0062), against a fake merchant over real HTTP,
//! which checks each delivery with code of its own (ADR-0056), on the system role's pool,
//! which the task runs on. Most tests call the task's events one at a time, with their own
//! times and draws; the last ones run it as the server does, on the clock, and bound each
//! wait. The times stay near the clock, since the fake refuses a signature more than five
//! minutes from its own. Master keys are made at runtime.

mod common;

use std::convert::Infallible;
use std::error::Error;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::outbox::{disable, event, insert_endpoint, master, new_payment};
use common::{delivery_id, endpoint_id, event_id, runtime_pool};
use mock_merchant::{Answer as Scripted, MockMerchant};
use payments_core::deliveries::{DeliveryClient, HANDOVERS, NotSent, TIMEOUT};
use payments_core::ids::DeliveryId;
use payments_core::relay::{Delivery, Effect, FAILING_AFTER, Reason, Relay};
use payments_core::relaying::{Ending, Relaying, Report};
use payments_core::retries::RetryBudget;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// Tokens enough that no retry in a test waits for one.
const PLENTY: u32 = 1_000;
/// Long enough that only the first tick runs during a test.
const HOUR: Duration = Duration::from_secs(3600);
/// Short enough that a test sees many ticks, and the wait between two looks at the database.
const SHORT: Duration = Duration::from_millis(20);

/// A merchant's payment and its endpoint 1 at a fake that trusts the endpoint's secret, on
/// the system role's pool.
struct Shop {
    system: PgPool,
    fake: MockMerchant,
    merchant: Uuid,
    payment: Uuid,
}

impl Shop {
    /// The shop, and the task that delivers to it with a budget of `tokens`, full at
    /// `start`, and one more token an hour.
    async fn open(
        pool: &PgPool,
        tokens: u32,
        start: OffsetDateTime,
    ) -> Result<(Self, Relaying), Box<dyn Error>> {
        let system = runtime_pool(pool).await?;
        let fake = MockMerchant::start().await?;
        let (merchant, payment) = new_payment(&system).await?;
        insert_endpoint(&system, endpoint_id(1), merchant, fake.url()).await?;
        let master = master();
        fake.trust(&master.secret_of(endpoint_id(1)).written())?;
        let relay = Relay::with_budget(RetryBudget::new(tokens, HOUR, start));
        let relaying = Relaying::new(system.clone(), DeliveryClient::new(master, TIMEOUT), relay);
        let shop = Self {
            system,
            fake,
            merchant,
            payment,
        };
        Ok((shop, relaying))
    }

    /// Events `numbers` of the payment, each at that position, with its delivery 1000 + n
    /// to endpoint 1, committed as a catch-up commits them.
    async fn events(
        &self,
        numbers: impl IntoIterator<Item = u32>,
    ) -> Result<Vec<Delivery>, Box<dyn Error>> {
        let mut written = Vec::new();
        for n in numbers {
            let delivery = delivery_id(n.checked_add(1_000).ok_or("a small number")?);
            written.extend(
                event(
                    &self.system,
                    self.merchant,
                    self.payment,
                    i32::try_from(n)?,
                    event_id(n),
                    &[(delivery, endpoint_id(1))],
                )
                .await?,
            );
        }
        Ok(written)
    }

    /// The deliveries whose success is recorded, by ID.
    async fn successes(&self) -> sqlx::Result<Vec<Uuid>> {
        sqlx::query_scalar!(
            "SELECT delivery_id FROM webhook_delivery_successes ORDER BY delivery_id"
        )
        .fetch_all(&self.system)
        .await
    }

    /// Why endpoint 1 is disabled, if it is.
    async fn disabled(&self) -> sqlx::Result<Option<String>> {
        sqlx::query_scalar!(
            "SELECT reason FROM webhook_endpoint_disablements WHERE endpoint_id = $1",
            endpoint_id(1).get()
        )
        .fetch_optional(&self.system)
        .await
    }

    async fn close(self) {
        self.system.close().await;
        self.fake.stop().await;
    }
}

/// `millis` milliseconds after `start`.
fn after(start: OffsetDateTime, millis: u64) -> OffsetDateTime {
    start + Duration::from_millis(millis)
}

/// Waits for each attempt in flight to end, and reads each at `now`, with `draw` for the
/// delay of its retry, which steps: the reports, in order. It waits for the attempts that
/// those steps start too.
async fn settle(relaying: &mut Relaying, now: OffsetDateTime, draw: u64) -> Vec<Report> {
    let mut reports = Vec::new();
    while let Some(ended) = relaying.join().await {
        reports.push(relaying.ended(ended, now, draw).await);
    }
    reports
}

/// The attempts that `reports` started, in order.
fn started(reports: &[Report]) -> Vec<Delivery> {
    reports
        .iter()
        .flat_map(|report| report.started.iter().copied())
        .collect()
}

/// The effects that `reports` recorded, in order.
fn recorded(reports: &[Report]) -> Vec<Effect> {
    reports
        .iter()
        .flat_map(|report| report.recorded.iter().copied())
        .collect()
}

/// A delivery that names no endpoint, as no catch-up writes one: its attempt panics at the
/// `expect` that names the foreign key, a bug of ours.
fn bogus() -> Delivery {
    Delivery {
        id: delivery_id(2_001),
        endpoint: endpoint_id(99),
        event: event_id(1),
    }
}

#[sqlx::test]
async fn a_delivery_handed_over_is_delivered_at_once_and_its_success_recorded_once(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;

    assert_eq!(
        relaying.hand_over(handed.clone(), start).await.started,
        handed
    );
    let reports = settle(&mut relaying, after(start, 100), 0).await;
    assert_eq!(recorded(&reports), [Effect::Record(handed[0].id)]);
    assert_eq!(shop.successes().await?, [handed[0].id.get()]);
    // The fake took the event, once, by its ID: the signature held under the endpoint's
    // secret.
    let taken = shop.fake.events();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].id, event_id(1).to_string());
    // Nothing is recorded or attempted again.
    let later = relaying.step(after(start, 200)).await;
    assert!(later.recorded.is_empty(), "{later:?}");
    assert!(later.started.is_empty(), "{later:?}");
    assert_eq!(shop.fake.arrivals().len(), 1);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_410_disables_the_endpoint_as_gone_and_ends_its_deliveries(pool: PgPool) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events(1..=2).await?;
    shop.fake.answer(Scripted::Status(StatusCode::GONE));

    assert_eq!(
        relaying.hand_over([handed[0]], start).await.started,
        [handed[0]]
    );
    let reports = settle(&mut relaying, after(start, 100), 0).await;
    assert_eq!(
        recorded(&reports),
        [Effect::Disable(endpoint_id(1), Reason::Gone)]
    );
    assert_eq!(shop.disabled().await?.as_deref(), Some("gone"));
    // The endpoint's next delivery is not attempted.
    let next = relaying.hand_over([handed[1]], after(start, 200)).await;
    assert!(next.started.is_empty(), "{next:?}");
    assert_eq!(shop.fake.arrivals().len(), 1);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_failed_attempt_is_tried_again_after_its_delay(pool: PgPool) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;
    shop.fake
        .answer(Scripted::Status(StatusCode::INTERNAL_SERVER_ERROR));

    relaying.hand_over(handed.clone(), start).await;
    // The first retry waits the draw: 2,000 ms, the most that it can.
    let reports = settle(&mut relaying, start, 2_000).await;
    assert!(started(&reports).is_empty(), "{reports:?}");
    shop.fake.answer(Scripted::Status(StatusCode::OK));
    let early = relaying.step(after(start, 1_999)).await;
    assert!(early.started.is_empty(), "{early:?}");
    assert_eq!(relaying.step(after(start, 2_000)).await.started, handed);
    let reports = settle(&mut relaying, after(start, 2_100), 0).await;
    assert_eq!(recorded(&reports), [Effect::Record(handed[0].id)]);
    assert_eq!(shop.fake.arrivals().len(), 2);
    assert_eq!(shop.fake.events().len(), 1);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_after_holds_the_retry_back_at_least_that_long(pool: PgPool) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;
    shop.fake.answer(Scripted::RetryAfter {
        status: StatusCode::SERVICE_UNAVAILABLE,
        seconds: 3,
    });

    relaying.hand_over(handed.clone(), start).await;
    // A draw of 0 would retry at once, but the endpoint asked for three seconds.
    settle(&mut relaying, start, 0).await;
    let early = relaying.step(after(start, 2_999)).await;
    assert!(early.started.is_empty(), "{early:?}");
    assert_eq!(relaying.step(after(start, 3_000)).await.started, handed);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_endpoint_gets_at_most_four_attempts_at_once_and_each_end_starts_the_next(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events(1..=6).await?;
    // Each answer comes a second late, so that the attempts overlap.
    shop.fake.answer(Scripted::Delay(Duration::from_secs(1)));

    assert_eq!(
        relaying.hand_over(handed.clone(), start).await.started,
        handed[..4]
    );
    let reports = settle(&mut relaying, after(start, 1_000), 0).await;
    assert_eq!(started(&reports), handed[4..]);
    assert_eq!(shop.fake.most_at_once(), 4);
    assert_eq!(shop.successes().await?.len(), 6);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_delivery_that_fails_for_three_days_disables_its_endpoint_as_failing(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;
    shop.fake
        .answer(Scripted::Status(StatusCode::INTERNAL_SERVER_ERROR));

    relaying.hand_over(handed.clone(), start).await;
    settle(&mut relaying, start, 2_000).await;
    // Three days after its first failure, the step disables the endpoint before it
    // attempts anything.
    let report = relaying.step(start + FAILING_AFTER).await;
    assert_eq!(
        report.recorded,
        [Effect::Disable(endpoint_id(1), Reason::Failing)]
    );
    assert!(report.started.is_empty(), "{report:?}");
    assert_eq!(shop.disabled().await?.as_deref(), Some("failing"));
    assert_eq!(shop.fake.arrivals().len(), 1);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_that_finds_its_endpoint_disabled_ends_its_deliveries(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events(1..=2).await?;
    disable(&shop.system, endpoint_id(1), "operator").await?;

    assert_eq!(
        relaying.hand_over([handed[0]], start).await.started,
        [handed[0]]
    );
    let reports = settle(&mut relaying, after(start, 100), 0).await;
    // Nothing to record: the operator's disablement stands.
    assert!(recorded(&reports).is_empty(), "{reports:?}");
    let next = relaying.hand_over([handed[1]], after(start, 200)).await;
    assert!(next.started.is_empty(), "{next:?}");
    assert!(shop.fake.arrivals().is_empty());
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_not_sent_while_the_database_refuses_is_tried_again_after_its_delay(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;
    // The system role cannot read the endpoint, in this test's database only.
    sqlx::query!("REVOKE SELECT ON webhook_endpoints FROM payments_app")
        .execute(&pool)
        .await?;

    assert_eq!(
        relaying.hand_over(handed.clone(), start).await.started,
        handed
    );
    // Not sent: it waits like a retry, for the draw of 2,000 ms.
    let reports = settle(&mut relaying, start, 2_000).await;
    assert!(
        matches!(
            reports[..],
            [Report {
                ended: Some((delivery, Ending::NotSent(NotSent::Endpoint(_)))),
                ..
            }] if delivery == handed[0].id
        ),
        "{reports:?}"
    );
    assert!(started(&reports).is_empty(), "{reports:?}");
    assert!(shop.fake.arrivals().is_empty());
    sqlx::query!("GRANT SELECT ON webhook_endpoints TO payments_app")
        .execute(&pool)
        .await?;
    let early = relaying.step(after(start, 1_999)).await;
    assert!(early.started.is_empty(), "{early:?}");
    assert_eq!(relaying.step(after(start, 2_000)).await.started, handed);
    let reports = settle(&mut relaying, after(start, 2_100), 0).await;
    assert_eq!(recorded(&reports), [Effect::Record(handed[0].id)]);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_not_sent_does_not_count_against_its_endpoint(pool: PgPool) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;
    // The system role cannot read the endpoint, in this test's database only.
    sqlx::query!("REVOKE SELECT ON webhook_endpoints FROM payments_app")
        .execute(&pool)
        .await?;

    relaying.hand_over(handed.clone(), start).await;
    settle(&mut relaying, start, 2_000).await;
    // Three days on, nothing is disabled: only the endpoint's answers count against it.
    let later = relaying.step(start + FAILING_AFTER).await;
    assert!(later.recorded.is_empty(), "{later:?}");
    assert!(later.not_recorded.is_none(), "{later:?}");
    assert_eq!(later.started, handed);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_step_stops_recording_at_the_first_record_that_fails(pool: PgPool) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events(1..=2).await?;
    let success = Effect::Record(handed[0].id);
    let gone = Effect::Disable(endpoint_id(1), Reason::Gone);
    // Successes cannot be recorded, in this test's database only; disablements can.
    sqlx::query!("REVOKE INSERT (delivery_id) ON webhook_delivery_successes FROM payments_app")
        .execute(&pool)
        .await?;

    relaying.hand_over([handed[0]], start).await;
    settle(&mut relaying, after(start, 100), 0).await;
    shop.fake.answer(Scripted::Status(StatusCode::GONE));
    relaying.hand_over([handed[1]], after(start, 200)).await;
    // The success comes first and fails, so the step leaves the disablement for later.
    let reports = settle(&mut relaying, after(start, 300), 0).await;
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].recorded.is_empty(), "{reports:?}");
    assert!(
        matches!(reports[0].not_recorded, Some((effect, _)) if effect == success),
        "{reports:?}"
    );
    assert_eq!(shop.disabled().await?, None);
    sqlx::query!("GRANT INSERT (delivery_id) ON webhook_delivery_successes TO payments_app")
        .execute(&pool)
        .await?;
    assert_eq!(
        relaying.step(after(start, 400)).await.recorded,
        [success, gone]
    );
    assert_eq!(shop.disabled().await?.as_deref(), Some("gone"));
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_fails_is_tried_again_at_each_step_and_its_delivery_is_not_sent_again(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;
    let success = Effect::Record(handed[0].id);
    // The system role cannot record a success, in this test's database only.
    sqlx::query!("REVOKE INSERT (delivery_id) ON webhook_delivery_successes FROM payments_app")
        .execute(&pool)
        .await?;

    relaying.hand_over(handed.clone(), start).await;
    let reports = settle(&mut relaying, after(start, 100), 0).await;
    assert!(
        matches!(reports[..], [Report { not_recorded: Some((effect, _)), .. }] if effect == success),
        "{reports:?}"
    );
    // The first search finds the delivery, which is not finished, and leaves it, since the
    // relay holds its success: it is not sent again, and its record fails again.
    let report = relaying.tick(after(start, 1_000), &mut || 0).await;
    assert!(matches!(report.searched, Some(Ok(()))), "{report:?}");
    assert!(report.started.is_empty(), "{report:?}");
    assert!(
        matches!(report.not_recorded, Some((effect, _)) if effect == success),
        "{report:?}"
    );
    assert!(shop.successes().await?.is_empty());
    sqlx::query!("GRANT INSERT (delivery_id) ON webhook_delivery_successes TO payments_app")
        .execute(&pool)
        .await?;
    assert_eq!(relaying.step(after(start, 2_000)).await.recorded, [success]);
    assert_eq!(shop.successes().await?, [handed[0].id.get()]);
    assert_eq!(shop.fake.arrivals().len(), 1);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_that_panics_is_read_as_not_sent_and_its_endpoint_is_not_blamed(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    let (shop, mut relaying) = Shop::open(&pool, PLENTY, start).await?;
    let handed = shop.events([1]).await?;

    let report = relaying.hand_over([handed[0], bogus()], start).await;
    assert_eq!(report.started, [handed[0], bogus()]);
    let reports = settle(&mut relaying, after(start, 100), 2_000).await;
    let panicked: Vec<(DeliveryId, bool)> = reports
        .iter()
        .filter_map(|report| match &report.ended {
            Some((delivery, Ending::Panicked(failed))) => Some((*delivery, failed.is_panic())),
            _ => None,
        })
        .collect();
    assert_eq!(panicked, [(bogus().id, true)], "{reports:?}");
    // The other attempt went on.
    assert_eq!(recorded(&reports), [Effect::Record(handed[0].id)]);
    // It waits like a retry, ...
    let early = relaying.step(after(start, 2_099)).await;
    assert!(early.started.is_empty(), "{early:?}");
    assert_eq!(relaying.step(after(start, 2_100)).await.started, [bogus()]);
    settle(&mut relaying, after(start, 2_200), 2_000).await;
    // ... and its endpoint is not blamed: three days on, nothing is disabled.
    let later = relaying
        .step(start + FAILING_AFTER + Duration::from_secs(1))
        .await;
    assert!(later.recorded.is_empty(), "{later:?}");
    assert!(later.not_recorded.is_none(), "{later:?}");
    assert_eq!(later.started, [bogus()]);
    drop(relaying);
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_first_search_takes_what_it_finds_as_retries_which_wait_for_tokens(
    pool: PgPool,
) -> TestResult {
    let start = OffsetDateTime::now_utc();
    // One token, and the next an hour later.
    let (shop, mut relaying) = Shop::open(&pool, 1, start).await?;
    let found = shop.events(1..=2).await?;

    // Committed before the task started, so perhaps attempted before a restart: each is a
    // retry, due at once with a draw of 0, and the one token goes to the first.
    let report = relaying.tick(start, &mut || 0).await;
    assert!(matches!(report.searched, Some(Ok(()))), "{report:?}");
    assert_eq!(report.started, [found[0]]);
    // A delivery handed over is a first attempt, which takes no token.
    let handed = shop.events([3]).await?;
    assert_eq!(
        relaying
            .hand_over(handed.clone(), after(start, 10))
            .await
            .started,
        handed
    );
    let reports = settle(&mut relaying, after(start, 100), 0).await;
    assert!(started(&reports).is_empty(), "{reports:?}");
    assert_eq!(
        shop.successes().await?,
        [found[0].id.get(), handed[0].id.get()]
    );
    assert_eq!(shop.fake.arrivals().len(), 2);
    drop(relaying);
    shop.close().await;
    Ok(())
}

/// Runs the task as the server does, a tick every `tick`: the handover's sender, and the
/// task.
fn run(relaying: Relaying, tick: Duration) -> (mpsc::Sender<Delivery>, JoinHandle<Infallible>) {
    let (handovers, handed_over) = mpsc::channel(HANDOVERS);
    (handovers, tokio::spawn(relaying.run(tick, handed_over)))
}

/// Waits, ten seconds at most, until the success of each of `deliveries` is recorded.
async fn until_recorded(shop: &Shop, deliveries: &[Delivery]) -> TestResult {
    let mut expected: Vec<Uuid> = deliveries
        .iter()
        .map(|delivery| delivery.id.get())
        .collect();
    expected.sort_unstable();
    let deadline = Instant::now() + Duration::from_secs(10);
    while shop.successes().await? != expected {
        assert!(
            Instant::now() < deadline,
            "not recorded in time: {expected:?}"
        );
        sleep(SHORT).await;
    }
    Ok(())
}

#[sqlx::test]
async fn the_running_task_delivers_what_its_search_finds(pool: PgPool) -> TestResult {
    let (shop, relaying) = Shop::open(&pool, PLENTY, OffsetDateTime::now_utc()).await?;
    let found = shop.events([1]).await?;

    // The first tick comes at once. What it finds waits a drawn delay of up to two
    // seconds, which the ticks that follow reach.
    let (_handovers, task) = run(relaying, SHORT);
    until_recorded(&shop, &found).await?;
    task.abort();
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_handover_and_each_attempts_end_step_between_ticks(pool: PgPool) -> TestResult {
    let (shop, relaying) = Shop::open(&pool, PLENTY, OffsetDateTime::now_utc()).await?;
    shop.fake
        .answer(Scripted::Delay(Duration::from_millis(500)));
    // Ticks an hour apart: once the first has run, with nothing to find, only a handover
    // or the end of an attempt can make a step.
    let (handovers, task) = run(relaying, HOUR);
    sleep(Duration::from_secs(1)).await;
    let handed = shop.events(1..=6).await?;
    for delivery in &handed {
        handovers.send(*delivery).await?;
    }

    // The first four at once, then each of the others as an attempt ends.
    until_recorded(&shop, &handed).await?;
    assert_eq!(shop.fake.most_at_once(), 4);
    task.abort();
    shop.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_panicked_attempt_leaves_the_task_running(pool: PgPool) -> TestResult {
    let (shop, relaying) = Shop::open(&pool, PLENTY, OffsetDateTime::now_utc()).await?;
    let (handovers, task) = run(relaying, HOUR);
    sleep(Duration::from_secs(1)).await;

    handovers.send(bogus()).await?;
    let handed = shop.events([1]).await?;
    handovers.send(handed[0]).await?;
    until_recorded(&shop, &handed).await?;
    assert!(!task.is_finished());
    task.abort();
    shop.close().await;
    Ok(())
}
