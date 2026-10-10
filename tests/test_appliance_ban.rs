#[test]
fn decision_files_do_not_name_products() {
    const FILES: &[&str] = &[
        "src/appliance.rs",
        "src/tunnel.rs",
        "src/adb_links.rs",
        "src/cluster/ladder.rs",
        "src/cluster/rank.rs",
        "src/gateway.rs",
        "src/discovery.rs",
        "src/doctor.rs",
    ];
    const BANNED: &[&str] = &["nothing", "s23", "galaxy", "sm-s"];
    for path in FILES {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let lower = text.to_ascii_lowercase();
        for word in BANNED {
            assert!(!lower.contains(word), "{path} contains {word}");
        }
    }
}
