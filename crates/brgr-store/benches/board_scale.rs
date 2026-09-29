//! Board projection and task listing cost as the store grows.
//!
//! The Herdr plugin refreshes [`BoardStore::rows`] every two seconds for the
//! lifetime of a pane, and nothing in brgr ever prunes a settled task. Cost per
//! refresh therefore has to stay bounded as the store accumulates, so this
//! bench reports the shape of that curve rather than a single number.
//!
//! ```text
//! cargo bench -p brgr-store --bench board_scale
//! ```
//!
//! Seeding dominates the run time; the reported figures are read-only samples
//! taken after each size is fully committed.

use std::hint::black_box;
use std::time::{Duration, Instant};

use brgr_protocol::{
    ArtifactContract, AttemptBudget, AttemptId, EvidenceSpec, OwnerId, ResultEnvelope, ResultId,
    Route, SCHEMA_V1, TaskId, TaskInstructions, TaskSpec, TerminalOutcome,
};
use brgr_store::{BoardStore, Store};
use tempfile::TempDir;

/// Task counts to project. The largest stands in for a store that has been in
/// daily use for roughly a year without pruning.
const SIZES: &[usize] = &[100, 400, 1_600, 6_400];
/// Board rows the plugin actually renders.
const BOARD_LIMIT: usize = 20;
const SAMPLES: usize = 5;

fn main() {
    let root = TempDir::new().expect("bench temp dir");
    let mut store = Store::open(root.path()).expect("open store");
    let owner = OwnerId::new("codex:bench-owner").expect("owner id");

    println!("board projection cost vs stored task count");
    println!("  limit={BOARD_LIMIT} samples={SAMPLES}");
    println!();
    println!("     tasks   board rows      tasks(limit)   per-task board");
    println!("  --------   ----------      ------------   --------------");

    let mut seeded = 0usize;
    for &size in SIZES {
        while seeded < size {
            seed_settled_task(&mut store, &owner, seeded);
            seeded += 1;
        }

        // Opened once: re-opening inside the timed body would charge connection
        // setup and statement preparation to every sample, and that constant
        // dominates the per-task figure at the small end.
        let projection = BoardStore::open_existing(root.path()).expect("open board");
        let board = sample(SAMPLES, || {
            let rows = projection.rows(BOARD_LIMIT).expect("board rows");
            black_box(rows.len());
        });
        let listing = sample(SAMPLES, || {
            let tasks = store.tasks(BOARD_LIMIT).expect("task listing");
            black_box(tasks.len());
        });

        println!(
            "  {size:>8}   {:>8.2?}      {:>10.2?}   {:>12.1?}",
            board,
            listing,
            board / u32::try_from(size).unwrap_or(u32::MAX),
        );
    }

    println!();
    println!("`per-task board` is the board cost divided by the stored task count.");
    println!("A projection bounded by the rendered limit makes it FALL as the store");
    println!("grows; a flat or rising column means cost tracks total tasks, and a");
    println!("long-lived store will outrun the two-second refresh interval. The");
    println!("`board rows` column is the number that has to stay under two seconds.");
}

fn sample(samples: usize, mut body: impl FnMut()) -> Duration {
    // One untimed pass so page cache and prepared-statement costs are not
    // attributed to the first sample.
    body();
    let start = Instant::now();
    for _ in 0..samples {
        body();
    }
    start.elapsed() / u32::try_from(samples).unwrap_or(1)
}

/// Records one task that already reached a sealed candidate result, which is the
/// shape the board has to join across `tasks`, `attempts`, and `results`.
fn seed_settled_task(store: &mut Store, owner: &OwnerId, index: usize) {
    let task = bench_task(owner, index);
    let attempt_id = AttemptId::new();
    store
        .record_task(&task, &format!("digest-{index}"))
        .expect("record task");
    store
        .create_attempt(task.task_id, task.revision, attempt_id)
        .expect("create attempt");
    let artifact = store
        .seal_artifact_reader(
            std::io::Cursor::new(b"benchmark report"),
            &task.artifact_contract.media_type,
            task.artifact_contract.max_bytes,
        )
        .expect("seal artifact");
    let result = ResultEnvelope {
        schema: SCHEMA_V1.to_owned(),
        task_id: task.task_id,
        revision: task.revision,
        attempt_id,
        result_id: ResultId::new(),
        outcome: TerminalOutcome::Candidate,
        artifacts: vec![artifact],
        error: None,
        legacy_embedded_route_observation: None,
        route_observation: None,
        unresolved_effects: vec![],
    };
    store
        .commit_terminal_result(&task.owner_id, &result)
        .expect("commit result");
}

fn bench_task(owner: &OwnerId, index: usize) -> TaskSpec {
    TaskSpec {
        schema: SCHEMA_V1.to_owned(),
        task_id: TaskId::new(),
        revision: 1,
        create_request_id: format!("bench-request-{index}"),
        owner_id: owner.clone(),
        objective: "Project one settled task onto the board".to_owned(),
        workspace: "/tmp/brgr-bench".to_owned(),
        route: Route {
            harness_id: "local.fixture".to_owned(),
            requested_model: None,
            requested_effort: None,
        },
        required_capabilities: vec!["completion".to_owned()],
        artifact_contract: ArtifactContract {
            media_type: "text/plain".to_owned(),
            max_bytes: 1_024,
        },
        acceptance_criteria: vec!["result is sealed".to_owned()],
        budget: AttemptBudget {
            deadline_seconds: 30,
            max_attempts: 2,
        },
        instructions: TaskInstructions::default(),
        evidence: EvidenceSpec::default(),
        max_concurrent_children: None,
    }
}
