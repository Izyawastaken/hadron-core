use hadron_core::{
    diff,
    validation::{self, Limits},
    *,
};
use std::collections::BTreeMap;

fn manifest() -> Manifest {
    Manifest {
        pack: Pack {
            schema_version: 1,
            id: "test".into(),
            name: "Test".into(),
            description: String::new(),
            game: "minecraft".into(),
            game_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.10".into(),
        },
        mods: BTreeMap::new(),
        configs: BTreeMap::new(),
    }
}
fn sodium() -> ModEntry {
    ModEntry {
        name: "Sodium".into(),
        file: FileSpec {
            path: "mods/sodium.jar".into(),
            size: 100,
            sha256: Hash::of(b"test"),
        },
        source: Source::Modrinth {
            project: "AANobbMI".into(),
            version: "exact".into(),
        },
        client: Requirement::Required,
        server: Requirement::Unsupported,
    }
}
#[test]
fn strict_schema_hash_and_paths() {
    let l = Limits::default();
    let mut m = manifest();
    validation::manifest(&m, &l).unwrap();
    m.pack.schema_version = 2;
    assert!(matches!(
        validation::manifest(&m, &l),
        Err(Error::Schema(2))
    ));
    assert!(Hash::parse("abc").is_err());
    assert!(Hash::parse("A".repeat(64)).is_err());
    for p in [
        "../x",
        "/x",
        "C:/x",
        "a/../b",
        "config\\x",
        "config/CON.toml",
        "a//b",
        "a/x.",
        "config/é",
    ] {
        assert!(validation::path(p).is_err(), "{p}");
    }
    let mut value = serde_json::to_value(manifest()).unwrap();
    value["surprise"] = true.into();
    assert!(serde_json::from_value::<Manifest>(value).is_err());
}
#[test]
fn exact_operation_and_deterministic_diff() {
    let initial = manifest();
    let operation = Operation::Add {
        key: "sodium".into(),
        entry: sodium(),
    };
    let next = operation.apply(&ModList::default()).unwrap();
    assert!(operation.apply(&next).is_err());
    let mut target = initial.clone();
    target.mods = next.mods;
    let plan = diff::between(&initial, &target);
    assert_eq!(plan.mods.len(), 1);
    assert_eq!(plan.mods[0].key, "sodium");
    assert!(plan.mods[0].before.is_none());
    let old = sodium();
    let mut changed = old.clone();
    changed.file.sha256 = Hash::of(b"new");
    assert!(
        Operation::Remove {
            key: "sodium".into(),
            expected: changed
        }
        .apply(&ModList { mods: target.mods })
        .is_err()
    );
}
#[test]
fn collisions_sources_and_limits() {
    let mut m = manifest();
    m.mods.insert("sodium".into(), sodium());
    let mut other = sodium();
    other.file.path = "mods/SODIUM.jar".into();
    m.mods.insert("other".into(), other);
    assert!(validation::manifest(&m, &Limits::default()).is_err());
    m.mods.remove("other");
    m.mods.get_mut("sodium").unwrap().source = Source::Direct {
        url: "https://example.com/a.jar?token=secret".into(),
    };
    assert!(validation::manifest(&m, &Limits::default()).is_err());
    assert!(
        validation::json::<serde_json::Value>(
            b"{}",
            &Limits {
                json_bytes: 1,
                ..Limits::default()
            }
        )
        .is_err()
    );
}
#[test]
fn share_codes_are_identity_not_auth() {
    let code = share::encode(1_449_632_592).unwrap();
    assert_eq!(share::decode(&code).unwrap(), 1_449_632_592);
    assert!(share::encode(1u64 << 35).is_err());
    assert!(share::decode("invalid").is_err());
}
#[cfg(feature = "compression")]
#[test]
fn deterministic_config_roundtrip_and_limits() {
    use hadron_core::codec;
    let configs = BTreeMap::from([
        ("config/z.toml".into(), b"# comment\nx = 3\n".to_vec()),
        ("config/a.json".into(), b"{}".to_vec()),
        ("config/empty".into(), vec![]),
    ]);
    let p = codec::prepare(manifest(), &configs, &Limits::default()).unwrap();
    let q = codec::prepare(manifest(), &configs, &Limits::default()).unwrap();
    assert_eq!(p.manifest_artifact.bytes, q.manifest_artifact.bytes);
    assert_eq!(
        p.config_artifact.as_ref().unwrap().bytes,
        q.config_artifact.as_ref().unwrap().bytes
    );
    let a = p.config_artifact.unwrap();
    let descriptor = a.asset(1, "configs.tar.zst".into());
    assert_eq!(
        codec::configs(&a.bytes, &descriptor, &p.manifest, &Limits::default()).unwrap(),
        configs
    );
    let mut corrupt = a.bytes.clone();
    corrupt[0] ^= 1;
    assert!(matches!(
        codec::decode(&corrupt, &descriptor, &Limits::default()),
        Err(Error::Integrity)
    ));
    assert!(
        codec::decode(
            &a.bytes[..a.bytes.len() - 1],
            &descriptor,
            &Limits::default()
        )
        .is_err()
    );
    assert!(
        codec::decode(
            &a.bytes,
            &descriptor,
            &Limits {
                decoded_bytes: 1,
                ..Limits::default()
            }
        )
        .is_err()
    );
    let mut lie = descriptor.clone();
    lie.decoded_size = 2;
    assert!(codec::decode(&a.bytes, &lie, &Limits::default()).is_err());
    assert!(
        codec::decode(
            &a.bytes,
            &descriptor,
            &Limits {
                window_log: 9,
                ..Limits::default()
            }
        )
        .is_err()
    );
}
#[cfg(feature = "compression")]
#[test]
fn archive_link_and_inventory_attacks() {
    use hadron_core::codec;
    let limits = Limits::default();
    let mut tar = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(0);
    h.set_mode(0o644);
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_link_name("../outside").unwrap();
    h.set_cksum();
    tar.append_data(&mut h, "config/link", &[][..]).unwrap();
    let a = codec::encode(&tar.into_inner().unwrap(), &limits).unwrap();
    assert!(codec::configs(&a.bytes, &a.asset(1, "bad".into()), &manifest(), &limits).is_err());
    let mut m = manifest();
    m.configs.insert(
        "config/missing".into(),
        FileSpec {
            path: "config/missing".into(),
            size: 1,
            sha256: Hash::of(b"x"),
        },
    );
    let mut tar = tar::Builder::new(Vec::new());
    tar.finish().unwrap();
    let a = codec::encode(&tar.into_inner().unwrap(), &limits).unwrap();
    assert!(codec::configs(&a.bytes, &a.asset(1, "missing".into()), &m, &limits).is_err());
}
#[cfg(feature = "cache")]
#[test]
fn cache_concurrent_writers_and_corruption() {
    use hadron_core::cache::Cache;
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::new(dir.path()).unwrap();
    let hash = Hash::of(b"content");
    assert!(cache.get(&hash, 7, 100).unwrap().is_none());
    std::thread::scope(|s| {
        for _ in 0..4 {
            let c = cache.clone();
            let h = hash.clone();
            s.spawn(move || c.put(&h, b"content").unwrap());
        }
    });
    assert_eq!(cache.get(&hash, 7, 100).unwrap().unwrap(), b"content");
    std::fs::write(dir.path().join(hash.as_str()), b"damaged").unwrap();
    assert!(matches!(cache.get(&hash, 7, 100), Err(Error::Integrity)));
}

#[cfg(feature = "compression")]
#[test]
fn manifest_json_budget_matches_consumer_and_streaming_checks_full_frame() {
    use hadron_core::codec;
    let m = manifest();
    let raw = serde_json::to_vec(&m).unwrap();
    let mut limits = Limits {
        json_bytes: raw.len() as u64,
        ..Limits::default()
    };
    let p = codec::prepare(m.clone(), &BTreeMap::new(), &limits).unwrap();
    let a = p.manifest_artifact.asset(1, "manifest".into());
    let decoded = codec::decode(&p.manifest_artifact.bytes, &a, &limits).unwrap();
    assert_eq!(validation::json::<Manifest>(&decoded, &limits).unwrap(), m);
    limits.json_bytes -= 1;
    assert!(matches!(
        codec::prepare(m.clone(), &BTreeMap::new(), &limits),
        Err(Error::Limit("manifest JSON"))
    ));
    let small_window = Limits {
        window_log: 10,
        ..Limits::default()
    };
    let p = codec::prepare(m.clone(), &BTreeMap::new(), &small_window).unwrap();
    codec::decode(
        &p.manifest_artifact.bytes,
        &p.manifest_artifact.asset(1, "small".into()),
        &small_window,
    )
    .unwrap();

    let limits = Limits::default();
    let files = BTreeMap::from([("config/a".into(), vec![42; 1024])]);
    let p = codec::prepare(m, &files, &limits).unwrap();
    let frame = p.config_artifact.unwrap();
    let mut a = frame.asset(1, "config".into());
    a.decoded_sha256 = Hash::of(b"wrong");
    assert!(matches!(
        codec::configs(&frame.bytes, &a, &p.manifest, &limits),
        Err(Error::Integrity)
    ));
    let a = frame.asset(1, "config".into());
    let mut short = a.clone();
    short.decoded_size -= 1;
    assert!(codec::configs(&frame.bytes, &short, &p.manifest, &limits).is_err());
    // Trailing decoded bytes count toward the full hash and byte budget.
    let mut tar = codec::decode(&frame.bytes, &a, &limits).unwrap();
    tar.extend_from_slice(b"trailing");
    let extra = codec::encode(&tar, &limits).unwrap();
    assert_eq!(
        codec::configs(
            &extra.bytes,
            &extra.asset(1, "extra".into()),
            &p.manifest,
            &limits
        )
        .unwrap(),
        files
    );
    let tiny = Limits {
        decoded_bytes: 100,
        ..limits
    };
    assert!(codec::configs(&frame.bytes, &a, &p.manifest, &tiny).is_err());
}

#[cfg(feature = "cache")]
#[test]
fn explicit_cache_budget_preserves_foreign_files() {
    use hadron_core::cache::Cache;
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::new(dir.path()).unwrap();
    cache.put(&Hash::of(b"abc"), b"abc").unwrap();
    cache.put(&Hash::of(b"defg"), b"defg").unwrap();
    std::fs::write(dir.path().join("foreign.txt"), b"retain").unwrap();
    assert_eq!(cache.size().unwrap(), 7);
    assert!(cache.prune(3).unwrap() >= 4);
    assert!(cache.size().unwrap() <= 3);
    assert!(dir.path().join("foreign.txt").exists());
}
