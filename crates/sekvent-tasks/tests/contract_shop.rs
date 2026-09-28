//! Dogfood: the contracts of `examples/shop` match their committed
//! baselines, and emitting them again reproduces those files byte for byte.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sekvent_tasks::config::{CONFIG_FILE, Config};
use sekvent_tasks::contract::{self, ContractConfig};

fn shop() -> (PathBuf, ContractConfig) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/shop");
    let config = Config::load(&root.join(CONFIG_FILE)).expect("examples/shop/sekvent.toml");
    assert!(
        !config.contract.roots.is_empty(),
        "examples/shop/sekvent.toml configures [contract] roots"
    );
    (root, config.contract)
}

fn json_files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(path).unwrap())
        })
        .collect()
}

#[test]
fn the_shop_contracts_match_their_baselines() {
    let (root, config) = shop();
    let report = contract::check(&root, &config, &[]).unwrap();
    assert!(report.is_ok(), "{}", report.render());
    assert_eq!(
        report.compatible,
        [
            "shop.inventory.v1.Inventory",
            "shop.notifications.v1.Notifications",
            "shop.orders.v1.Orders",
        ]
    );
}

#[test]
fn emitting_again_reproduces_the_committed_baselines() {
    let (root, config) = shop();
    let committed = json_files(&root.join(&config.baseline));
    let scratch = tempfile::tempdir().unwrap();
    let config = ContractConfig {
        baseline: scratch.path().to_owned(),
        ..config
    };
    let written = contract::emit(&root, &config, &[]).unwrap();
    assert_eq!(written.len(), committed.len());
    let emitted = json_files(scratch.path());
    assert_eq!(
        emitted.keys().collect::<Vec<_>>(),
        committed.keys().collect::<Vec<_>>()
    );
    for (name, bytes) in &committed {
        assert!(
            emitted[name] == *bytes,
            "{name} differs from a fresh `cargo sekvent contract emit`"
        );
    }
}
