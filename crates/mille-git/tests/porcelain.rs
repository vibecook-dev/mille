//! T-PORC: the porcelain-v2 `-z` golden shared with the JavaScript parser.

use mille_git::parse_porcelain_v2;

#[test]
fn the_mixed_golden_parses_exactly() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/porcelain");
    let input = std::fs::read(dir.join("v2-mixed.z")).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("v2-mixed.expected.json")).unwrap())
            .unwrap();

    let mut warnings = Vec::new();
    let entries = parse_porcelain_v2(&input, &mut |w| warnings.push(w));
    assert_eq!(serde_json::to_value(&entries).unwrap(), expected["entries"]);
    assert_eq!(
        warnings.len() as u64,
        expected["warnings"].as_u64().unwrap(),
        "{warnings:?}"
    );
}
