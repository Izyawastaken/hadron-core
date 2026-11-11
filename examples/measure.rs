//! Synthetic, reproducible performance smoke check; not a real pack benchmark.
use hadron_core::{
    codec, diff,
    validation::{self, Limits},
    *,
};
use std::{collections::BTreeMap, time::Instant};
fn main() -> Result<()> {
    let limits = Limits::default();
    let mut m = Manifest {
        pack: Pack {
            schema_version: 1,
            id: "synthetic".into(),
            name: "Synthetic corpus".into(),
            description: "500 exact mods and 100 configs; illustrative data".into(),
            game: "minecraft".into(),
            game_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.10".into(),
        },
        mods: BTreeMap::new(),
        configs: BTreeMap::new(),
    };
    for i in 0..500 {
        m.mods.insert(
            format!("mod-{i:04}"),
            ModEntry {
                name: format!("Synthetic mod {i}"),
                file: FileSpec {
                    path: format!("mods/mod-{i:04}.jar"),
                    size: 1000 + i,
                    sha256: Hash::of(format!("synthetic-{i}").as_bytes()),
                },
                source: Source::Modrinth {
                    project: format!("project-{i}"),
                    version: format!("version-{i}"),
                },
                client: Requirement::Required,
                server: Requirement::Optional,
            },
        );
    }
    let configs: BTreeMap<String, Vec<u8>> = (0..100)
        .map(|i| {
            (
                format!("config/mod-{i:04}.toml"),
                format!(
                    "# Synthetic configuration {i}\n{}",
                    "enabled = true\nvalue = 42\n".repeat(100)
                )
                .into_bytes(),
            )
        })
        .collect();
    let start = Instant::now();
    let p = codec::prepare(m.clone(), &configs, &limits)?;
    let encode_us = start.elapsed().as_micros();
    let a = p.manifest_artifact.asset(1, "manifest.json.zst".into());
    let start = Instant::now();
    for _ in 0..100 {
        let decoded = codec::decode(&p.manifest_artifact.bytes, &a, &limits)?;
        std::hint::black_box(decoded);
    }
    let decode_us = start.elapsed().as_micros() / 100;
    let raw = serde_json::to_vec(&p.manifest)?;
    let start = Instant::now();
    for _ in 0..100 {
        let parsed: Manifest = validation::json(&raw, &limits)?;
        validation::manifest(&parsed, &limits)?;
        std::hint::black_box(diff::between(&p.manifest, &parsed));
    }
    let parse_diff_us = start.elapsed().as_micros() / 100;
    let mut next = m.clone();
    next.mods.get_mut("mod-0000").unwrap().file.sha256 = Hash::of(b"new jar");
    let q = codec::prepare(next, &configs, &limits)?;
    assert_eq!(
        p.config_artifact.as_ref().unwrap().sha256,
        q.config_artifact.as_ref().unwrap().sha256
    );
    let mut changed = configs.clone();
    changed
        .get_mut("config/mod-0000.toml")
        .unwrap()
        .extend_from_slice(b"# one config edit\n");
    let r = codec::prepare(m, &changed, &limits)?;
    let c = p.config_artifact.as_ref().unwrap();
    println!("synthetic_mods=500 configs=100 encoder=zstd-9 threads=1");
    println!(
        "manifest_raw={} manifest_compressed={} configs_tar_raw={} configs_compressed={}",
        p.manifest_artifact.decoded_size,
        p.manifest_artifact.bytes.len(),
        c.decoded_size,
        c.bytes.len()
    );
    println!(
        "prepare_us={encode_us} manifest_decode_us={decode_us} parse_validate_diff_us={parse_diff_us}"
    );
    println!(
        "cold_artifact_bytes={} warm_artifact_bytes=0 one_mod_edit_artifact_bytes={} one_config_edit_artifact_bytes={}",
        p.manifest_artifact.bytes.len() + c.bytes.len(),
        q.manifest_artifact.bytes.len(),
        r.manifest_artifact.bytes.len() + r.config_artifact.as_ref().unwrap().bytes.len()
    );
    println!(
        "byte totals exclude discovery/index and mod binaries; peak RAM measured by host, not codec window"
    );
    Ok(())
}
