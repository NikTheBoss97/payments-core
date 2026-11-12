<!-- Author: Nicola Antonello -->

# ADR-0047: The tests leave the PSP's step commands first

- Status: accepted
- Date: 2026-11-12

## Context

ADR-0034 guards I8 four ways, and two of the guards are not in the code yet: the merchant
role's revocations with the restrictive policy `payment_transitions_tenant_steps`, and
`LG007` with the Rust guard that only the catch-up applies a fact. Since ADR-0046 no route
writes a step of the PSP's: the four routes record requests, and the catch-up applies the
PSP's facts (ADR-0031, ADR-0041). The library's commands for the PSP's steps, `authorize`,
`capture`, `refund`, `decline`, `void`, `expire` and `settle`, are still public, and tests
are their only callers: 105 tests in 11 files reach them, directly or through a helper, in
about 4,000 lines of test functions.

Each guard breaks some of those tests. The revocations and the policy refuse every step of
the PSP's that `payments_tenant` writes, as six of the files do. `LG007` refuses at
`COMMIT` such a step with no fact, as all eleven do, and once the commands leave the public
API no test can call them. So the tests move before the guards land.

Most of the tests attack something that the PSP's word still reaches. A step's record, its
posting and its fee, the table of moves and the limits, the checks at `COMMIT` and a death
at any statement all happen in the catch-up now. The merchant's side of the same moves is a
request (ADR-0028). And the path that every command shares, the lock, identity first, the
retries and ADR-0014's rule at `COMMIT`, still serves `cancel` and `create`, which stay
public. A few tests attack what only a caller that names a step of the PSP's can do: retry
the step, or send it again with other content. The catch-up names its own steps (ADR-0039),
so once the commands are private, no such caller is left.

## Options

The order of the pull requests:

| Option | Why not |
|---|---|
| **The tests first, in two pull requests, then the merchant role's guards, then `LG007` and the Rust guard** | Chosen |
| The merchant role's guards first, with the six files that they break | The guards' review would carry about 1,700 lines of moved tests, and the other five files would move in a pull request of their own anyway |
| The tests in one pull request | About 4,000 lines of test functions in one review, twice the size at which ADR-0045 split the switch |

What becomes of a test that only a step command of the PSP's can express:

| Option | Why not |
|---|---|
| **Retired, and listed below with its reason** | Chosen |
| Kept as a unit test of the crate's private functions | It needs PostgreSQL, so it belongs in `tests/`, which sees only the public API |
| Kept through a test-only export of the commands | A way back into the API that I8 closes, to test what no caller does |

## Decision

- **Four pull requests.**
  1. The merchant's side: `tenant_commands`, `keyed_commands`, `keyed_crashes`,
     `payment_model`, `recording_requests` and `api_reading` move, and this record lands
     with them. After it, no test calls a step command of the PSP's as
     `payments_tenant`.
  2. The library's side: `authorizing_payments`, `capturing_payments`,
     `refunding_payments`, `payment_steps` and `payment_crashes` move. After it, no test
     calls a step command of the PSP's.
  3. The merchant role's guards (ADR-0034): the revocations by column, the restrictive
     policy, the catalog test and the attacks, with the raw-SQL tests of what that role
     writes (`tenant_isolation.rs`), which assert the privileges that these guards
     change, so they cannot move first.
  4. `LG007`, and the Rust guard: the PSP's step commands leave the public API, so only the
     catch-up applies a fact. The raw-SQL tests that commit a step of the PSP's without a
     fact change with it, since how `LG007` meets the other checks at `COMMIT` is its own
     design.
- **Where a test moves.** A test stays in its file, under its name unless the name stops
  saying what it checks, and moves:
  - onto **requests and facts** when it attacks a move of the PSP's. The merchant records a
    request, a step of a `Sender` sends it to the fake, the fake decides it, and a catch-up
    applies the decision, as the server does (ADR-0043, ADR-0046). A fact that no PSP would
    give is appended to the fake's history (ADR-0038). A move that the table or a limit
    refuses becomes the request's refusal when the merchant asks, and a stop of the payment
    when the PSP's history holds it (ADR-0041).
  - onto **`cancel` or `create`**, the merchant's own steps, when it attacks the path that
    every command shares and used a step of the PSP's as its example.
  - nowhere: it is **retired** when it attacks what only a caller that names a step of the
    PSP's can do, or when, once moved, it would repeat a test that already exists at the
    same layer, which is named below.
- **One helper for the PSP's side.** `tests/common/psp.rs` gains a type that holds a fake,
  the sender that the server's task would hold for it, and the system role's pool, which
  sends and catches up, with the steps that take a payment to `authorized` or `captured`
  through the fake.
- **The correctness pages** say where each invariant's tests went, in the pull request that
  moves them.

## The tests, file by file

By their names before the move.

The merchant's side, in the first pull request:

- **`tenant_commands.rs`** (9). Onto requests and facts:
  `a_merchant_runs_a_payment_through_its_life`,
  `a_merchant_runs_the_commands_that_carry_nothing`,
  `another_merchants_payment_is_not_found`,
  `the_connection_keeps_no_merchant_once_a_command_ends`,
  `a_merchant_sees_what_it_wrote_and_the_next_one_does_not`,
  `the_system_role_still_runs_a_command_with_no_merchant`. Onto `cancel`:
  `a_retry_gets_the_stored_reply_and_a_changed_one_a_mismatch`,
  `eight_concurrent_copies_apply_once_for_a_merchant`,
  `a_duplicate_key_is_settled_for_a_merchant_too`.
- **`keyed_commands.rs`** (8). Onto requests and facts:
  `an_applied_command_stores_its_answer_with_its_effect`,
  `a_refusal_is_stored_and_changes_nothing_else`,
  `a_late_duplicate_of_a_refusal_cannot_become_an_effect`,
  `what_is_not_final_stores_nothing`,
  `an_answer_that_is_not_final_takes_the_effect_down_with_it`,
  `a_second_attempt_under_a_key_rolls_back_whole`,
  `one_key_on_two_payments_gives_one_effect`. One retires.
- **`keyed_crashes.rs`** (6), onto requests and facts:
  `a_keyed_authorization_killed_anywhere_leaves_no_answer_and_settles`,
  `a_keyed_void_killed_anywhere_leaves_no_answer_and_settles`,
  `a_keyed_capture_killed_anywhere_moves_no_money_and_settles`,
  `a_keyed_refund_killed_anywhere_moves_no_money_and_settles`,
  `a_refusal_killed_as_it_is_stored_is_decided_again`,
  `a_commit_that_commits_and_loses_its_session_is_settled_by_the_key`.
- **`payment_model.rs`** (2). Onto requests and facts:
  `random_histories_of_three_merchants_match_the_model`, whose model gains the requests in
  flight that hold a capture or a void back and count against a refund's limit
  (ADR-0039). One retires.
- **`recording_requests.rs`** (9), whose payments reach `authorized` and `captured` through
  the fake: `a_request_of_each_kind_is_recorded_and_moves_nothing`,
  `a_move_that_the_table_or_a_limit_refuses_records_nothing`,
  `a_payment_gets_one_authorization_request`,
  `a_capture_or_a_void_waits_for_the_one_in_flight`,
  `a_refund_counts_the_refunds_in_flight`, `a_stopped_payment_takes_no_request`,
  `copies_of_one_request_racing_record_it_once`,
  `a_request_waits_for_the_payments_lock_and_reads_what_its_holder_committed`,
  `a_cancel_is_refused_once_an_authorization_is_requested`.
- **`api_reading.rs`** (1), onto facts: `a_read_shows_where_a_payment_stands_now`.

The library's side, in the second pull request:

- **`authorizing_payments.rs`** (15). Onto facts:
  `an_authorization_stores_its_step_and_its_record`, `the_amount_is_at_most_the_request`,
  `a_record_that_a_trigger_loses_fails_at_commit`,
  `a_record_that_a_trigger_corrupts_is_a_bug`. Onto requests:
  `the_status_is_judged_before_the_amount`,
  `an_authorization_racing_a_cancel_gets_one_winner`. Nine retire.
- **`capturing_payments.rs`** (19). Onto facts:
  `a_capture_stores_its_step_its_posting_and_its_record`,
  `the_fee_follows_the_rate_and_rounds_half_up`,
  `a_payment_written_without_its_accounts_is_reported`, `eight_concurrent_copies_apply_once`,
  `a_posting_that_a_trigger_corrupts_fails_at_commit`,
  `a_record_that_a_trigger_loses_fails_at_commit`, `a_record_that_a_trigger_corrupts_is_a_bug`,
  `an_unproven_commit_failure_is_settled_by_sending_again`. Onto requests and facts:
  `the_amount_is_at_most_the_authorization`, `the_status_is_judged_before_the_amount`,
  `a_capture_racing_a_void_gets_one_winner`, `a_partial_authorization_limits_the_capture`.
  Seven retire.
- **`refunding_payments.rs`** (13). Onto facts:
  `a_partial_refund_keeps_the_status_and_lowers_what_is_left`,
  `after_a_full_refund_the_merchant_owes_the_platform_its_fee`,
  `a_settled_payment_can_be_refunded`, `a_partial_refund_racing_a_settle_applies_both`,
  `a_final_refund_racing_a_settle_ends_refunded_either_way`,
  `a_posting_that_a_trigger_corrupts_fails_at_commit`,
  `a_record_that_a_trigger_loses_fails_at_commit`. Onto requests and facts:
  `the_amount_is_at_most_what_is_left`, `only_a_captured_or_settled_payment_can_be_refunded`,
  `of_many_refunds_of_one_exactly_what_is_left_apply`. Three retire.
- **`payment_steps.rs`** (17). Onto facts and `cancel`:
  `every_command_follows_the_table_from_every_status`, with the PSP's steps that carry no
  money as facts from every status and `cancel` as before. Onto `create`, with a newer step
  from a fact: `a_retry_gets_its_own_steps_reply_and_not_the_newest_steps`. Onto `cancel`:
  `a_command_on_an_unknown_payment_is_not_found`,
  `a_retry_gets_the_stored_reply_although_the_status_has_moved_on`,
  `a_changed_retry_is_a_mismatch`, `a_refused_command_is_decided_again`,
  `eight_concurrent_copies_apply_once`, `racing_commands_get_one_winner_and_a_domain_answer`,
  `a_command_waits_for_the_lock_and_reads_what_its_holder_committed`,
  `a_lock_timeout_is_not_confirmed_and_a_retry_settles_it`,
  `a_retry_applies_after_a_first_attempt_rolls_back`,
  `a_retry_outlasts_a_first_attempt_left_idle`,
  `a_writer_that_skips_the_lock_and_commits_first_makes_a_command_a_bug`,
  `a_duplicate_key_is_settled_by_the_step_with_the_commands_id`,
  `a_batch_queues_behind_a_command_and_a_command_behind_the_batch`,
  `a_write_that_a_trigger_corrupts_is_a_bug`,
  `an_unproven_commit_failure_is_settled_by_sending_again`.
- **`payment_crashes.rs`** (6), onto facts, the catch-up killed around each of its
  statements as ADR-0034 asks: `an_authorization_killed_anywhere_stores_nothing_and_settles`,
  `a_void_killed_anywhere_stores_nothing_and_settles`,
  `a_capture_killed_anywhere_moves_no_money_and_settles`,
  `a_refund_killed_anywhere_moves_no_money_and_settles`,
  `a_command_killed_while_it_waits_for_the_lock_can_be_sent_again`,
  `a_commit_that_commits_and_loses_its_session_is_settled_by_sending_again`.

The 21 that retire:

| Test | Why it retires |
|---|---|
| `authorizing_payments::a_retry_gets_the_stored_reply_although_the_status_has_moved_on` | A retry of a step of the PSP's. The same answer of `cancel` moves |
| `authorizing_payments::a_changed_retry_is_a_mismatch_for_each_field` | A step of the PSP's sent again with other content |
| `authorizing_payments::a_step_id_of_another_kind_is_a_mismatch_both_ways` | A step ID of the PSP's named by a caller |
| `authorizing_payments::a_time_finer_than_the_database_still_settles_a_retry` | A retry of a step of the PSP's. That a time comes back equal stays in `business_times::every_time_that_can_be_built_comes_back_equal` |
| `authorizing_payments::the_first_and_last_times_that_can_be_built_are_stored` | Repeats `payment_records::a_records_time_is_one_that_rust_can_read`, which writes both bounds into every record's column. A fact's time is RFC 3339, whose year has four digits (section 5.6), so no fact carries either bound |
| `authorizing_payments::a_refused_command_leaves_its_step_id_free` | A step ID of the PSP's named by a caller. The same rule moves with `payment_steps::a_refused_command_is_decided_again` |
| `authorizing_payments::an_unknown_payment_is_not_found` | Repeats `catching_up::a_payment_that_does_not_exist_is_not_found`, and the request's case moves with `keyed_commands::what_is_not_final_stores_nothing` |
| `authorizing_payments::eight_concurrent_copies_apply_once` | Repeats `catching_up::copies_of_one_catch_up_racing_apply_each_fact_once`, which races an authorization |
| `authorizing_payments::void_and_expire_end_a_real_authorization` | Repeats `catching_up::a_decline_a_void_and_a_lapse_make_their_steps` |
| `capturing_payments::a_retry_gets_the_stored_reply_although_the_status_has_moved_on` | A retry of a step of the PSP's |
| `capturing_payments::a_changed_retry_is_a_mismatch_for_each_field` | A step of the PSP's sent again with other content |
| `capturing_payments::a_first_attempt_with_the_same_content_settles_a_retry` | The control of the next two, which retire |
| `capturing_payments::a_retry_compares_the_postings_entries` | A retry of a step of the PSP's, against a first attempt written by hand |
| `capturing_payments::a_retry_compares_the_stored_fee` | The same |
| `capturing_payments::a_posting_id_that_exists_without_this_step_is_a_bug` | A posting ID named by a caller. The catch-up makes its own |
| `capturing_payments::an_unknown_payment_is_not_found` | As `authorizing_payments::an_unknown_payment_is_not_found` |
| `refunding_payments::a_retry_of_an_older_refund_gets_its_own_reply` | A retry of a step of the PSP's. An older step's own answer moves with `payment_steps::a_retry_gets_its_own_steps_reply_and_not_the_newest_steps` |
| `refunding_payments::a_changed_retry_is_a_mismatch_for_each_field` | A step of the PSP's sent again with other content |
| `refunding_payments::an_unknown_payment_is_not_found` | As `authorizing_payments::an_unknown_payment_is_not_found` |
| `keyed_commands::copies_of_one_request_racing_give_one_effect` | Once moved, the same test as `recording_requests::copies_of_one_request_racing_record_it_once` |
| `payment_model::random_histories_match_the_model` | The system role moving a payment as a merchant would. Only the merchant's role records a request (ADR-0037), so every history runs as merchants, in the test that stays |

## Consequences

- After the second pull request no test calls a step command of the PSP's. The guards still
  change the raw-SQL tests of what they guard: the merchant role's writes, and the steps
  of the PSP's that the schema tests commit without a fact.
- The tests of the PSP's moves start a fake on a local port, as every test of the PSP does
  (ADR-0029).
- What only the retired tests reached: a step of the PSP's retried, which the catch-up never
  does (ADR-0039), and the comparison of a capture's or a refund's stored posting and fee on
  such a retry (ADR-0019). Once the commands are private no caller reaches either, and the
  fourth pull request says what becomes of that code.
- The suite loses 21 tests. The path that every command shares keeps its tests, on `cancel`
  and `create`.

## References

- Erik Kuefler, [Unit Testing](https://abseil.io/resources/swe-book/html/ch12.html), chapter
  12 of Software Engineering at Google (O'Reilly, 2020), section "Test via Public APIs":
  "write tests that invoke the system being tested in the same way its users would."
- John Hughes, [Experiences with QuickCheck: Testing the Hard Stuff and Staying Sane](https://publications.lib.chalmers.se/records/fulltext/232550/local_232550.pdf)
  (2016), for the model of `payment_model.rs`.
- The records that it builds on: ADR-0019, ADR-0028, ADR-0029, ADR-0031, ADR-0034,
  ADR-0037, ADR-0038, ADR-0039, ADR-0041, ADR-0043, ADR-0045 and ADR-0046.
