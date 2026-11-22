//! Runs the binary on every fixture `tests/fixtures/**/<name>.csv` and compares
//! stdout with `<name>.expected.csv` next to it. A fixture without one fails, so
//! no fixture can sit unused.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const EXPECTED_SUFFIX: &str = ".expected.csv";

#[test]
fn every_fixture_produces_its_expected_output() {
    let inputs = fixture_inputs(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"));
    assert!(!inputs.is_empty(), "no fixtures found");
    for input in inputs {
        let expected_path = expected_path_for(&input);
        let expected = fs::read_to_string(&expected_path)
            .unwrap_or_else(|_| panic!("missing {}", expected_path.display()));
        assert_eq!(run_engine(&input), expected, "fixture {}", input.display());
    }
}

fn run_engine(input: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_payments-engine"))
        .arg(input)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} exited with {}",
        input.display(),
        output.status
    );
    String::from_utf8(output.stdout).unwrap()
}

fn fixture_inputs(dir: &Path) -> Vec<PathBuf> {
    let mut inputs = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            inputs.extend(fixture_inputs(&path));
        } else if is_fixture_input(&path) {
            inputs.push(path);
        }
    }
    inputs.sort();
    inputs
}

fn is_fixture_input(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_string_lossy();
    name.ends_with(".csv") && !name.ends_with(EXPECTED_SUFFIX)
}

fn expected_path_for(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap().to_string_lossy();
    input.with_file_name(format!("{stem}{EXPECTED_SUFFIX}"))
}
