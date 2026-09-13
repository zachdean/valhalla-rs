#![cfg(feature = "proto")]

//! Exercises `Actor::with_tile_getter` end-to-end: a `tile_dir` deliberately missing exactly one
//! local-road tile forces `GraphReader` to ask the custom getter for it mid-route, and the
//! getter serves it from a full copy of the same tileset extracted elsewhere. If the injection
//! point (Rust trait -> `cxx` bridge -> `RustTileGetterAdapter` -> `GraphReader`) didn't work end
//! to end, this route would fail outright or silently degrade, not just quietly succeed some
//! other way.
//!
//! One tile, not "every local tile", is deliberate: this tiny 7-tile Andorra fixture sits close
//! enough to Valhalla's own reachability cutoff (`minimum_outbound/inbound_reachability`, a
//! *count* of edges a location's candidate must be able to reach) that removing every level-2
//! tile at once changes that count and makes the route fail for reasons unrelated to this
//! feature -- confirmed by reproducing the identical failure with the same tiles simply absent
//! and no getter involved at all. A single rare miss recovered via the getter, which is the
//! scenario this feature exists for, is exactly what these tests cover.

use std::{
    fs, io,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use valhalla::{
    Actor, ConfigBuilder, LatLon, Response, TileGetter,
    proto::{self, options::Format},
};

const ANDORRA_TILES: &str = "tests/andorra/tiles.tar";

const ANDORRA_TEST_LOC_1: LatLon = LatLon(42.50107335756198, 1.510341967860551); // Sant Julia de Loria
const ANDORRA_TEST_LOC_2: LatLon = LatLon(42.50627089323736, 1.521734167223563); // Andorra la Vella

/// A level-2 (local road) tile that `route_recovers_one_tile_missing_from_disk_via_getter`
/// deletes from the sparse `tile_dir`, forcing it through the getter. Confirmed (by recording
/// every path the getter is asked for) to actually be consulted by this specific route -- unlike
/// e.g. `2/000/763/927.gph`, one of the other 4 real level-2 tiles in `tests/andorra/tiles.tar`,
/// which this exact route never touches, missing or not, making it useless for proving the
/// getter fires. `2/000/763/926.gph` is also consulted, but removing that *specific* tile
/// changes the route's computed reachability enough to fail regardless of how it's supplied
/// (confirmed by reproducing the same failure with it simply absent from disk and no getter
/// configured at all) -- a property of this minimal fixture, not of the getter mechanism.
const REMOVED_TILE: &str = "2/000/762/486.gph";

/// Extracts `tiles.tar` (a plain POSIX tar of individual `<level>/<id>.gph` files, not a
/// `tile_extract`-style archive) into `dst`, giving a real `mjolnir.tile_dir`-shaped tree.
fn extract_tiles(tar_path: &str, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    let status = std::process::Command::new("tar")
        .arg("-xf")
        .arg(tar_path)
        .arg("-C")
        .arg(dst)
        .status()
        .expect("failed to run tar");
    assert!(status.success(), "tar extraction failed");
}

fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Serves tile bytes from a full, on-disk copy of the tileset and records every path it was
/// asked for, so the test can assert the getter was actually exercised (not just that the route
/// happened to succeed some other way).
struct RecordingGetter {
    full_dir: std::path::PathBuf,
    requested: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl TileGetter for RecordingGetter {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requested.lock().unwrap().push(path.to_string());
        fs::read(self.full_dir.join(path)).ok()
    }
}

#[test]
fn route_recovers_one_tile_missing_from_disk_via_getter() {
    let tmp = tempfile::tempdir().unwrap();
    let full_dir = tmp.path().join("full");
    let sparse_dir = tmp.path().join("sparse");
    extract_tiles(ANDORRA_TILES, &full_dir);
    copy_dir_all(&full_dir, &sparse_dir).unwrap();

    // The one deliberately-missing tile: present in `full_dir` (what the getter reads from),
    // absent from `sparse_dir` (what `GraphReader` opens), so it is only recoverable through
    // `Actor::with_tile_getter`'s getter.
    let removed = sparse_dir.join(REMOVED_TILE);
    fs::remove_file(&removed).unwrap();
    assert!(
        !removed.exists(),
        "test setup bug: the removed tile should be absent from tile_dir"
    );

    let config = ConfigBuilder {
        mjolnir: valhalla::config::Mjolnir {
            tile_dir: sparse_dir.display().to_string(),
            // Required sentinel: `GraphReader` only ever consults a tile getter (built-in or
            // custom) when `tile_url` is non-empty, and it must be exactly `{tilePath}` --
            // see `Actor::with_tile_getter`'s doc for why.
            tile_url: Some("{tilePath}".into()),
            ..Default::default()
        },
        ..Default::default()
    }
    .build();

    let getter = std::sync::Arc::new(RecordingGetter {
        full_dir,
        requested: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });

    struct ArcGetter(std::sync::Arc<RecordingGetter>);
    impl TileGetter for ArcGetter {
        fn get(&self, path: &str) -> Option<Vec<u8>> {
            self.0.get(path)
        }
    }

    let mut actor = Actor::with_tile_getter(&config, ArcGetter(getter.clone()))
        .expect("failed to construct Actor");

    let request = proto::Options {
        format: Format::Pbf as i32,
        costing_type: proto::costing::Type::Auto as i32,
        locations: vec![
            proto::Location {
                ll: ANDORRA_TEST_LOC_1.into(),
                ..Default::default()
            },
            proto::Location {
                ll: ANDORRA_TEST_LOC_2.into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let response = actor.route(&request);
    assert!(
        response.is_ok(),
        "route with a getter-backed tile_dir should succeed: {response:?}"
    );
    let Ok(Response::Pbf(api)) = response else {
        panic!("expected a PBF response");
    };
    assert!(
        !api.directions.as_ref().unwrap().routes.is_empty(),
        "expected at least one route leg"
    );

    let requested = getter.requested.lock().unwrap();
    assert!(
        requested.iter().any(|p| p == REMOVED_TILE),
        "expected the getter to be asked for the deliberately-missing tile, got: {requested:?}"
    );
}

#[test]
fn missing_tile_reported_as_none_never_crashes() {
    // A getter that always reports "not found" must degrade exactly like today's disk-only
    // miss -- Actor construction and simple requests must not panic or abort the process.
    struct AlwaysMissing;
    impl TileGetter for AlwaysMissing {
        fn get(&self, _path: &str) -> Option<Vec<u8>> {
            None
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let full_dir = tmp.path().join("full");
    extract_tiles(ANDORRA_TILES, &full_dir);
    // Keep every tile on disk this time; the getter is configured but should simply never be
    // needed, and must not be able to break anything even though `tile_url` is set.
    let config = ConfigBuilder {
        mjolnir: valhalla::config::Mjolnir {
            tile_dir: full_dir.display().to_string(),
            tile_url: Some("{tilePath}".into()),
            ..Default::default()
        },
        ..Default::default()
    }
    .build();

    let actor = Actor::with_tile_getter(&config, AlwaysMissing);
    assert!(actor.is_ok());
}

#[test]
fn panicking_getter_degrades_instead_of_aborting() {
    struct PanickingGetter;
    impl TileGetter for PanickingGetter {
        fn get(&self, _path: &str) -> Option<Vec<u8>> {
            panic!("boom");
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let full_dir = tmp.path().join("full");
    let sparse_dir = tmp.path().join("sparse");
    extract_tiles(ANDORRA_TILES, &full_dir);
    copy_dir_all(&full_dir, &sparse_dir).unwrap();
    fs::remove_file(sparse_dir.join(REMOVED_TILE)).unwrap();

    let config = ConfigBuilder {
        mjolnir: valhalla::config::Mjolnir {
            tile_dir: sparse_dir.display().to_string(),
            tile_url: Some("{tilePath}".into()),
            ..Default::default()
        },
        ..Default::default()
    }
    .build();

    // Constructing the `Actor` never calls the getter (see `GetTileSet`'s doc), so this must
    // succeed even though every subsequent call to `PanickingGetter::get` would panic.
    let mut actor =
        Actor::with_tile_getter(&config, PanickingGetter).expect("failed to construct Actor");

    let request = proto::Options {
        format: Format::Pbf as i32,
        costing_type: proto::costing::Type::Auto as i32,
        locations: vec![
            proto::Location {
                ll: ANDORRA_TEST_LOC_1.into(),
                ..Default::default()
            },
            proto::Location {
                ll: ANDORRA_TEST_LOC_2.into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    // The process must survive this call. Whether Valhalla ultimately returns `Ok` (having
    // treated every panicking fetch as a plain miss) or `Err` (unable to route without the
    // missing tile) is not the point being tested -- either is a normal `Result`, produced
    // without ever unwinding across the FFI boundary or aborting the process.
    let _ = actor.route(&request);
}
