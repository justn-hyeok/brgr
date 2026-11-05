mod common;

use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
};

use common::brgr_command;
use tempfile::TempDir;

#[test]
fn concurrent_config_set_processes_keep_both_keys() {
    let home = TempDir::new().unwrap();
    let argv = serde_json::to_string(
        &(0..64)
            .map(|index| format!("argument-{index}-{}", "x".repeat(80)))
            .collect::<Vec<_>>(),
    )
    .unwrap();

    for round in 0..40 {
        fs::write(home.path().join("config.toml"), "").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let first_home = home.path().to_path_buf();
        let first_barrier = barrier.clone();
        let first_argv = argv.clone();
        let first = thread::spawn(move || {
            first_barrier.wait();
            let mut command = brgr_command();
            command.args([
                "--home",
                first_home.to_str().unwrap(),
                "config",
                "set",
                "argv",
                &first_argv,
            ]);
            command.output().unwrap()
        });
        let second_home = home.path().to_path_buf();
        let second_barrier = barrier.clone();
        let second = thread::spawn(move || {
            second_barrier.wait();
            let mut command = brgr_command();
            command.args([
                "--home",
                second_home.to_str().unwrap(),
                "config",
                "set",
                "effort",
                "high",
            ]);
            command.output().unwrap()
        });
        barrier.wait();
        let first = first.join().unwrap();
        let second_output = second.join().unwrap();
        assert!(
            first.status.success(),
            "round {round}: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(
            second_output.status.success(),
            "round {round}: {}",
            String::from_utf8_lossy(&second_output.stderr)
        );

        let config = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(
            config.contains("argument-63-"),
            "argv was lost in round {round}"
        );
        assert!(
            config.contains("effort = \"high\""),
            "effort was lost in round {round}"
        );
    }
}
