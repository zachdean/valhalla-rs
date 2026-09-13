use prost::Message;

use crate::{Config, Error, proto, proto::options::Format};

#[allow(clippy::needless_lifetimes)] // clippy goes nuts with cxx
#[cxx::bridge]
mod ffi {
    /// Helper struct to provide an access to C++'s buffer with serialized response data.
    struct Response {
        /// Raw response data, either a JSON string or binary data.
        data: UniquePtr<CxxString>,
        /// [`options::Format`] format of the response.
        format: i32,
    }

    unsafe extern "C++" {
        include!("valhalla/src/actor.hpp");

        #[namespace = "boost::property_tree"]
        type ptree = crate::config::ffi::ptree;

        type Actor;
        fn new_actor(config: &ptree) -> Result<UniquePtr<Actor>>;
        // All methods accept [`proto::Options`] object serialized as a byte slice.
        fn route(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn locate(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn matrix(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn optimized_route(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn isochrone(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn trace_route(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn trace_attributes(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn transit_available(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn expansion(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn centroid(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;
        fn status(self: Pin<&mut Actor>, request: &[u8]) -> Result<Response>;

        /// Returns [`proto::Options`] object serialized as C++ `std::string` from a Valhalla JSON string.
        fn parse_json_request(json: &str, action: i32) -> Result<UniquePtr<CxxString>>;

        // Additive: everything below exists only for `Actor::with_tile_getter`. Declared in
        // `src/tile_getter.hpp` (new file, not this one) -- reached transitively through the
        // `include!` above, since `actor.hpp` itself pulls that header in (see its own comment
        // for why the ordering matters); no separate `include!` is needed here for it.

        /// See [`Actor::with_tile_getter`] for the full contract; this is its C++ half.
        fn new_actor_with_tile_getter(
            config: &ptree,
            getter: Box<RustTileGetter>,
        ) -> Result<UniquePtr<Actor>>;
    }

    extern "Rust" {
        /// Opaque handle wrapping a user-supplied [`super::TileGetter`], so a `Box<dyn Trait>`
        /// (unsized) can cross into C++ as a single, `Sized` opaque Rust type -- `cxx` requires
        /// the latter for anything passed across the bridge.
        type RustTileGetter;

        /// Called synchronously by `RustTileGetterAdapter::get()` (`src/tile_getter.hpp`), on
        /// whatever thread is running Valhalla's graph expansion, whenever a tile is missing
        /// from disk. Never unwinds across the FFI boundary: a panic inside
        /// [`super::TileGetter::get`] is caught here and turned into an ordinary `Err`, which
        /// `cxx` surfaces to the C++ side as a thrown exception -- caught there and reported as
        /// `status_code_t::FAILURE`, exactly like a missing tile is today. Without this
        /// `catch_unwind`, that panic would instead unwind straight into C++, which is
        /// undefined behavior; `cxx`'s own per-function panic guard exists only to turn an
        /// *uncaught* one into a deterministic abort, not to recover it, so it is not a
        /// substitute for this.
        fn tile_getter_get(&self, path: &str) -> Result<Vec<u8>>;
    }
}

// Safety: `ffi::Actor` doesn't hold any reference to the shared state and all its methods require
// a mutable reference to `self`, so stronger borrowing rules apply here, preventing using mutable
// methods concurrently.
unsafe impl Send for ffi::Actor {}
unsafe impl Sync for ffi::Actor {}

/// Valhalla natively supports multiple response formats, such as JSON, OSRM-like JSON, PBF, and others.
/// This format is specified on per-request basis using [`proto::Options`] `format` field, selecting one of the
/// [`proto::options::Format`] options.
///
/// It is worth noting that at the moment not every endpoint supports all of the formats. For example,
/// [`Actor::locate()`] will always return a Valhalla JSON response regardless of the `format` field in the request.
#[derive(Debug, Clone)]
pub enum Response {
    /// Protocol Buffer response containing a structured [`proto::Api`] object.
    ///
    /// This format preserves full type information and is most efficient for
    /// programmatic access. Only available when [`proto::options::Format::Pbf`]
    /// is explicitly requested and the endpoint supports it.
    ///
    /// Set [`proto::PbfFieldSelector`] request option to get more detailed output.
    Pbf(Box<proto::Api>),
    /// JSON string response in either Valhalla or OSRM format.
    ///
    /// The JSON structure depends on which format was requested:
    /// - [`proto::options::Format::Json`] returns Valhalla-native JSON
    /// - [`proto::options::Format::Osrm`] returns OSRM-compatible JSON
    ///
    /// This is also the fallback format for most endpoints when other formats
    /// are not supported, like in the case of [`Actor::locate()`] that always
    /// returns a Valhalla JSON response.
    Json(String),
    /// Binary response for specialized formats.
    ///
    /// Currently used for:
    /// - GPX files (XML format for GPS data)
    /// - GeoTIFF files (raster geographic data)
    ///
    /// The actual format depends on the request and endpoint capabilities.
    Other(Vec<u8>),
}

impl From<ffi::Response> for Response {
    fn from(response: ffi::Response) -> Self {
        if response.format == Format::Pbf as i32 {
            let api = proto::Api::decode(response.data.as_bytes())
                .expect("Proper PBF data is guaranteed by Valhalla");
            Response::Pbf(Box::new(api))
        } else if response.format == Format::Json as i32 || response.format == Format::Osrm as i32 {
            Response::Json(String::from_utf8_lossy(response.data.as_bytes()).into_owned())
        } else {
            Response::Other(response.data.as_bytes().to_owned())
        }
    }
}

/// High-level interface to interact with [Valhalla's API](https://valhalla.github.io/valhalla/api/).
/// On contrary to the Valhalla REST and C++ APIs, this interface is designed to be used with [`proto::Options`] only,
/// to avoid unnecessary conversions and to provide a strongly typed interface.
pub struct Actor(cxx::UniquePtr<ffi::Actor>);

impl Actor {
    /// ```
    /// let config = valhalla::ConfigBuilder {
    ///     mjolnir: valhalla::config::Mjolnir {
    ///         tile_extract: "path/to/tiles.tar".into(),
    ///         traffic_extract: "path/to/traffic.tar".into(),
    ///         ..Default::default()
    ///     },
    ///     ..Default::default()
    /// }
    /// .build();
    /// let actor = valhalla::Actor::new(&config);
    /// ```
    pub fn new(config: &Config) -> Result<Self, Error> {
        Ok(Self(ffi::new_actor(config.inner())?))
    }

    /// Calculates a route between locations.
    ///
    /// # Example
    ///
    /// ```
    /// # fn call_route(actor: &mut valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     format: proto::options::Format::Pbf as i32,
    ///     costing_type: proto::costing::Type::Auto as i32,
    ///     locations: vec![
    ///         proto::Location {
    ///              ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///              ..Default::default()
    ///          },
    ///          proto::Location {
    ///              ll: valhalla::LatLon(55.5944, 13.0002).into(),
    ///              ..Default::default()
    ///          },
    ///     ],
    ///     ..Default::default()
    /// };
    /// let response = actor.route(&request);
    /// let Ok(valhalla::Response::Pbf(api)) = response else {
    ///     panic!("Expected PBF response, got: {response:?}");
    /// };
    /// # }
    /// ```
    pub fn route(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::route, request)
    }

    /// Finds the nearest roads and intersections to input coordinates. Always returns a Valhalla JSON response.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_locate(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     locations: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     has_verbose: Some(proto::options::HasVerbose::Verbose(true)),
    ///     ..Default::default()
    /// };
    /// let response = actor.locate(&request);
    /// let Ok(valhalla::Response::Json(json)) = response else {
    ///     panic!("Expected JSON response, got: {response:?}");
    /// };
    /// # }
    /// ```
    pub fn locate(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::locate, request)
    }

    /// Computes a time-distance matrix between sources and targets.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_matrix(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     costing_type: proto::costing::Type::Auto as i32,
    ///     sources: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     targets: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.5944, 13.0002).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     ..Default::default()
    /// };
    /// let response = actor.matrix(&request);
    /// # }
    /// ```
    pub fn matrix(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::matrix, request)
    }

    /// Solves the traveling salesman problem for multiple locations.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_optimized_route(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     costing_type: proto::costing::Type::Auto as i32,
    ///     locations: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///             ..Default::default()
    ///         },
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.5944, 13.0002).into(),
    ///             ..Default::default()
    ///         },
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6000, 13.0050).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     ..Default::default()
    /// };
    /// let response = actor.optimized_route(&request);
    /// # }
    /// ```
    pub fn optimized_route(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::optimized_route, request)
    }

    /// Computes areas reachable within specified time or distance intervals.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_isochrone(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     costing_type: proto::costing::Type::Pedestrian as i32,
    ///     locations: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     contours: vec![
    ///         proto::Contour {
    ///             has_time: Some(proto::contour::HasTime::Time(10.0)),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     ..Default::default()
    /// };
    /// let response = actor.isochrone(&request);
    /// # }
    /// ```
    pub fn isochrone(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::isochrone, request)
    }

    /// Map-matches a GPS trace to roads and returns a route with turn-by-turn directions.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_trace_route(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     costing_type: proto::costing::Type::Auto as i32,
    ///     has_encoded_polyline: Some(proto::options::HasEncodedPolyline::EncodedPolyline(
    ///         "_grbgAh~{nhF?lBAzBFvBHxBEtBKdB".into(),
    ///     )),
    ///     ..Default::default()
    /// };
    /// let response = actor.trace_route(&request).unwrap();
    /// # }
    /// ```
    pub fn trace_route(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::trace_route, request)
    }

    /// Map-matches a GPS trace and returns detailed edge attributes along the path.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_trace_attributes(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     costing_type: proto::costing::Type::Auto as i32,
    ///     has_encoded_polyline: Some(proto::options::HasEncodedPolyline::EncodedPolyline(
    ///         "_grbgAh~{nhF?lBAzBFvBHxBEtBKdB".into(),
    ///     )),
    ///     ..Default::default()
    /// };
    /// let response = actor.trace_attributes(&request).unwrap();
    /// # }
    /// ```
    pub fn trace_attributes(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::trace_attributes, request)
    }

    /// Checks if transit/public transportation is available at given locations.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_transit_available(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     locations: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     ..Default::default()
    /// };
    /// let response = actor.transit_available(&request).unwrap();
    /// # }
    /// ```
    pub fn transit_available(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::transit_available, request)
    }

    /// Returns a GeoJSON representation of graph traversal for visualization.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_expansion(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     action: proto::options::Action::Route as i32,
    ///     has_expansion_action: Some(proto::options::HasExpansionAction::ExpansionAction(
    ///         proto::options::Action::Route as i32,
    ///     )),
    ///     costing_type: proto::costing::Type::Pedestrian as i32,
    ///     locations: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///             ..Default::default()
    ///         },
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.5944, 13.0002).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     ..Default::default()
    /// };
    /// let response = actor.expansion(&request).unwrap();
    /// # }
    /// ```
    pub fn expansion(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::expansion, request)
    }

    /// Finds the least cost convergence point from multiple locations.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_centroid(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     costing_type: proto::costing::Type::Auto as i32,
    ///     locations: vec![
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.6086, 13.0005).into(),
    ///             ..Default::default()
    ///         },
    ///         proto::Location {
    ///             ll: valhalla::LatLon(55.5944, 13.0002).into(),
    ///             ..Default::default()
    ///         },
    ///     ],
    ///     ..Default::default()
    /// };
    /// let response = actor.centroid(&request).unwrap();
    /// # }
    /// ```
    pub fn centroid(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::centroid, request)
    }

    /// Returns status information about the Valhalla instance and loaded tileset.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn call_status(mut actor: valhalla::Actor) {
    /// use valhalla::proto;
    ///
    /// let request = proto::Options {
    ///     has_verbose: Some(proto::options::HasVerbose::Verbose(true)),
    ///     ..Default::default()
    /// };
    /// let response = actor.status(&request).unwrap();
    /// # }
    /// ```
    pub fn status(&mut self, request: &proto::Options) -> Result<Response, Error> {
        self.act(ffi::Actor::status, request)
    }

    /// Generic helper function to process request encoding, calling the endpoint and handling response.
    fn act<F>(&mut self, action_fn: F, request: &proto::Options) -> Result<Response, Error>
    where
        F: for<'a> Fn(
            std::pin::Pin<&'a mut ffi::Actor>,
            &'a [u8],
        ) -> Result<ffi::Response, cxx::Exception>,
    {
        let buffer = request.encode_to_vec();
        let result = action_fn(self.0.as_mut().unwrap(), &buffer);
        Ok(Response::from(result?))
    }

    /// Helper function to convert a Valhalla JSON string into Valhalla PBF request as [`proto::Options`] object.
    /// This function is not optimized for performance and should be considered as a convenience method.
    /// For best performance construct [`proto::Options`] directly if possible.
    pub fn parse_json_request(
        json: &str,
        action: proto::options::Action,
    ) -> Result<proto::Options, Error> {
        if json.is_empty() {
            // Empty string is a special for Valhalla, so we should return an error here.
            return Err(Error("Failed to parse json request".into()));
        }

        let cxx_string = ffi::parse_json_request(json, action as i32)?;
        let mut options = proto::Options::decode(cxx_string.as_bytes())
            .map_err(|err| Error(err.to_string().into()))?;

        // Workaround for "ignore_closures in costing and exclude_closures in search_filter cannot both be specified"
        // that is happened because this check is happens before that value is set to the default false and processing
        // json request actually causes parsing function to be called twice because it parses and sets default values.
        for costing in options.costings.values_mut() {
            if let Some(proto::costing::HasOptions::Options(costing_options)) =
                &mut costing.has_options
            {
                // If ignore_closures is false, we can clear it
                if let Some(proto::costing::options::HasIgnoreClosures::IgnoreClosures(false)) =
                    costing_options.has_ignore_closures
                {
                    costing_options.has_ignore_closures = None;
                }
            }
        }

        Ok(options)
    }
}

// --- On-demand tile getter injection (additive; see `src/tile_getter.hpp` for the C++ half) ---
//
// This whole section is why this crate is forked from `kinkard/valhalla-rs`: it builds Valhalla
// with `ENABLE_HTTP=OFF`, which compiles out the built-in curl `tile_url` fetcher entirely, so a
// tile a caller's own prefetch missed is normally unrecoverable -- the leg it falls in just
// silently degrades. `Actor::with_tile_getter` gives a Rust caller a way to plug in its own
// synchronous fetch (e.g. one more S3 `GetObject`) that Valhalla consults mid-request, without
// re-enabling curl or touching anything about how `Actor::new` behaves.

/// A source of Valhalla map tiles supplied by the embedding application.
///
/// Implement this and pass it to [`Actor::with_tile_getter`] to let Valhalla ask for a tile it
/// doesn't already have on disk (under `mjolnir.tile_dir`/`tile_extract`), instead of treating a
/// missing tile as a permanent hole in the graph. This is a *fallback*, called synchronously,
/// mid-request, on whatever thread is running the graph expansion -- it is not a substitute for
/// prefetching the tiles a request needs ahead of time, only a safety net for the rare miss.
///
/// # Contract
///
/// - `get` is synchronous and may block; it is called from Valhalla's own worker thread(s), not
///   from an async runtime, so a `tokio` (or similar) call inside it must use a blocking handle
///   (e.g. `Handle::block_on`), never `.await` directly.
/// - Return `None` for "this tile doesn't exist" (Valhalla treats it exactly like an HTTP 404
///   from the built-in curl getter: the region stays a hole, and the caller degrades to its
///   normal heuristic fallback for whatever leg needed it). Reserve `Err`-like signaling for
///   nothing -- there is no separate "retry later" outcome; every failure looks like a miss.
/// - A panic inside `get` is caught (see `tile_getter_get` below) and treated the same as
///   returning `None`. It will never crash the process or unwind into Valhalla's C++, but it is
///   still a bug worth fixing: prefer returning `None` on purpose over panicking.
/// - `path` is the tile's path relative to the tileset root, in Valhalla's own on-disk layout
///   (e.g. `"859/318/196.gph"` -- the same shape `GraphTile::FileSuffix` produces), **not** a
///   URL. [`Actor::with_tile_getter`]'s doc explains why: it configures Valhalla with a bare
///   `"{tilePath}"` `tile_url` sentinel specifically so Valhalla substitutes nothing else around
///   it.
/// - `Send + Sync` because [`Actor::with_tile_getter`] hands ownership to Valhalla's C++ side,
///   which may call `get` from more than one worker; there is exactly one `GraphReader` (and so
///   one getter) per `Actor`, and an application pooling multiple `Actor`s gets one getter call
///   site per pool member, not one shared across the pool.
pub trait TileGetter: Send + Sync {
    /// Returns the raw bytes of the tile at `path`, or `None` if it isn't available.
    fn get(&self, path: &str) -> Option<Vec<u8>>;
}

/// Bridges a `Box<dyn TileGetter>` across the `cxx` boundary as a single opaque type -- `cxx`'s
/// `extern "Rust" type` machinery requires `Sized`, which `dyn TileGetter` alone is not.
struct RustTileGetter(Box<dyn TileGetter>);

impl RustTileGetter {
    /// The `extern "Rust"` method the bridge above declares. Turning a panic into `Err` here,
    /// before returning through `cxx`'s own generated shim, is what keeps it from unwinding
    /// across the FFI boundary at all -- see the bridge declaration's doc for why that matters.
    fn tile_getter_get(&self, path: &str) -> Result<Vec<u8>, String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.get(path))) {
            Ok(bytes) => Ok(bytes.unwrap_or_default()),
            Err(_) => Err(format!("tile getter panicked while fetching {path:?}")),
        }
    }
}

impl Actor {
    /// Like [`Actor::new`], but lets Valhalla fall back to `getter` for any tile missing from
    /// `mjolnir.tile_dir`/`tile_extract`, instead of only ever answering from what's already on
    /// disk. See [`TileGetter`] for the contract `getter` must satisfy.
    ///
    /// # `config`'s `mjolnir.tile_url`
    ///
    /// Valhalla's `GraphReader` only ever consults a tile getter -- built-in or, as here,
    /// caller-supplied -- when `mjolnir.tile_url` is a non-empty string; that's true regardless
    /// of `ENABLE_HTTP`. Set it, via [`crate::config::Mjolnir::tile_url`], to the literal
    /// sentinel `"{tilePath}"` (`GraphTile::kTilePathPattern`) -- not a real URL, and not left
    /// empty:
    /// - Left empty, `getter` is never called at all; `with_tile_getter` would behave exactly
    ///   like [`Actor::new`].
    /// - Anything else non-empty risks Valhalla treating `tile_url` as pointing at one remote
    ///   `.tar` extract rather than plain per-tile files, which makes its constructor issue a
    ///   synchronous `get()` for the tar header before `with_tile_getter` even returns.
    /// - With the bare `"{tilePath}"` sentinel, Valhalla substitutes each tile's own relative
    ///   path into the (empty) space around the placeholder and calls `get()` with exactly that
    ///   path (see `TileGetter::get`'s doc) -- nothing for `getter` to strip or parse.
    ///
    /// # Example
    ///
    /// ```
    /// # struct MyGetter;
    /// # impl valhalla::TileGetter for MyGetter {
    /// #     fn get(&self, _path: &str) -> Option<Vec<u8>> { None }
    /// # }
    /// let config = valhalla::ConfigBuilder {
    ///     mjolnir: valhalla::config::Mjolnir {
    ///         tile_dir: "/tmp/tiles".into(),
    ///         tile_url: Some("{tilePath}".into()),
    ///         ..Default::default()
    ///     },
    ///     ..Default::default()
    /// }
    /// .build();
    /// let actor = valhalla::Actor::with_tile_getter(&config, MyGetter);
    /// ```
    pub fn with_tile_getter(
        config: &Config,
        getter: impl TileGetter + 'static,
    ) -> Result<Self, Error> {
        let getter: Box<RustTileGetter> = Box::new(RustTileGetter(Box::new(getter)));
        Ok(Self(ffi::new_actor_with_tile_getter(
            config.inner(),
            getter,
        )?))
    }
}
