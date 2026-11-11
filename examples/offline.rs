use hadron_core::{Manifest, Pack, codec, diff, validation::Limits};
use std::collections::BTreeMap;
fn main() -> hadron_core::Result<()> {
    let manifest = Manifest {
        pack: Pack {
            schema_version: 1,
            id: "example".into(),
            name: "Example pack".into(),
            description: "Offline example".into(),
            game: "minecraft".into(),
            game_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.10".into(),
        },
        mods: BTreeMap::new(),
        configs: BTreeMap::new(),
    };
    let configs = BTreeMap::from([(
        "config/example.toml".into(),
        b"# preserved comment\nenabled = true\n".to_vec(),
    )]);
    let prepared = codec::prepare(manifest.clone(), &configs, &Limits::default())?;
    let artifact = prepared
        .manifest_artifact
        .asset(1, "manifest.json.zst".into());
    let decoded = codec::decode(
        &prepared.manifest_artifact.bytes,
        &artifact,
        &Limits::default(),
    )?;
    let round_trip: Manifest = serde_json::from_slice(&decoded)?;
    assert_eq!(prepared.manifest, round_trip);
    assert_eq!(diff::between(&manifest, &round_trip).configs.len(), 1);
    println!(
        "Verified {} compressed manifest bytes",
        prepared.manifest_artifact.bytes.len()
    );
    Ok(())
}
