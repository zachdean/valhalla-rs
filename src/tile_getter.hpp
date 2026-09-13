#pragma once

// New file, added for the on-demand tile getter injection point. Nothing here is upstream
// `kinkard/valhalla-rs` code; it exists so that `src/actor.hpp` and `src/actor.rs` need only the
// smallest possible additive edit (one new `Actor` constructor overload, one new `#include`,
// one tiny factory function next to `new_actor`, one new bridge function + `extern "Rust"`
// block) instead of growing this feature inline in files a rebase must diff against upstream
// every time.
//
// Included from exactly one place: `src/actor.hpp`, immediately after its own
// `#include "valhalla/src/actor.rs.h"`. That ordering is required, not stylistic -- `rust::Box`
// and the generated `RustTileGetter` type this file uses are defined *inside* `actor.rs.h`
// itself (`cxx` emits its `namespace rust { ... }` boilerplate there), so this header cannot be
// included any earlier than that, including via `src/actor.rs`'s bridge `include!` list, which
// would land here before `actor.rs.h` has been processed at all. See `actor.hpp`'s own comment
// at that `#include` for the same note from the other side.
//
// Everything below is defined directly in this header (no companion `.cpp`) precisely because
// it is only ever included from that one place: `actor.hpp` itself defines `new_actor` and
// `parse_json_request` the same un-inlined way, relying on `actor.hpp` (and everything it pulls
// in) never being compiled into more than one translation unit. A second `.cpp` including
// `actor.hpp` again -- the obvious way to give `new_actor_with_tile_getter` a home of its own --
// broke exactly that assumption (duplicate-symbol linker errors for `new_actor` and
// `parse_json_request` in an initial version of this file). `new_actor_with_tile_getter` itself
// therefore lives in `actor.hpp` right next to `new_actor`, not here, since only `actor.hpp`
// legitimately needs the complete `Actor` type; this header stays Actor-agnostic.

#include <valhalla/baldr/tilegetter.h>

/// Adapts a Rust-implemented `valhalla::TileGetter` (see the `extern "Rust"` block in
/// `src/actor.rs`, and the public `TileGetter` trait it forwards to) into Valhalla's C++
/// `tile_getter_t` interface, so `GraphReader` can be handed one without knowing anything about
/// Rust or `cxx`. Method bodies are defined in-class (implicitly inline) rather than split into
/// declaration/definition, since that's what lets this whole header stay a single file with no
/// companion `.cpp` -- see the top comment for why a `.cpp` is a trap here.
class RustTileGetterAdapter final : public valhalla::baldr::tile_getter_t {
public:
  explicit RustTileGetterAdapter(rust::Box<RustTileGetter> getter) : getter_(std::move(getter)) {}

  /// `url` is whatever `GraphTile::CacheTileURL` built from `mjolnir.tile_url` and the tile's
  /// own relative path. `new_actor_with_tile_getter()`'s caller is required to set
  /// `mjolnir.tile_url` to the bare `"{tilePath}"` placeholder (see that function's doc), which
  /// makes Valhalla substitute nothing around it -- so by construction `url` here is already
  /// exactly the tile's relative path (e.g. `"859/318/196.gph"`), not a real URL, and there is
  /// nothing for this adapter to strip.
  ///
  /// Never throws: any failure -- the getter returning nothing, or panicking (caught on the
  /// Rust side, see `tile_getter_get`'s doc, and surfacing here as a thrown `rust::Error`) --
  /// is reported as `status_code_t::FAILURE`, exactly like a 404 from the built-in curl getter.
  /// `offset`/`size` (Valhalla's HTTP-range-on-a-tar-file path) are unused: the `{tilePath}`
  /// sentinel above keeps `GraphReader` out of that path entirely, so this override is only ever
  /// called with the defaults.
  GET_response_t get(const std::string& url, uint64_t /*offset*/ = 0, uint64_t /*size*/ = 0) override {
    GET_response_t resp;
    try {
      const rust::Vec<uint8_t> bytes = getter_->tile_getter_get(rust::Str(url));
      if (bytes.empty()) {
        // Rust's "not found" sentinel (`TileGetter::get` returned `None`) -- indistinguishable
        // from a genuinely empty tile, which Valhalla never produces, so this is unambiguous.
        resp.status_ = status_code_t::FAILURE;
        resp.http_code_ = 404;
      } else {
        resp.bytes_.assign(bytes.begin(), bytes.end());
        resp.status_ = status_code_t::SUCCESS;
        resp.http_code_ = 200;
      }
    } catch (const std::exception&) {
      // A panicking getter is caught on the Rust side (see `tile_getter_get`'s doc) and turned
      // into an `Err`, which `cxx` throws here as a C++ exception. Either way, this must never
      // escape: nothing upstream of `tile_getter_t::get()` expects it to throw, and letting it
      // propagate would take the whole request down instead of degrading just this one tile,
      // same as a miss always has.
      resp.status_ = status_code_t::FAILURE;
      resp.http_code_ = 500;
    } catch (...) {
      resp.status_ = status_code_t::FAILURE;
      resp.http_code_ = 500;
    }
    return resp;
  }

  /// Never invoked by anything `valhalla::Actor` exercises: `GraphTile::CacheTileURL` -- the
  /// only caller of a `GraphReader`'s `tile_getter_` -- always calls `get()`, never `head()`.
  /// `head()` exists only for Valhalla's own remote-tar-offset bookkeeping
  /// (`GraphReader::load_remote_tar_offsets`), which the `{tilePath}` sentinel steers around
  /// (see `get()`'s doc). Implemented anyway because `tile_getter_t` is a pure abstract class
  /// requiring an override; reports "unavailable" so a future caller that does reach this
  /// degrades exactly like a miss would, rather than crashing on a pure-virtual call.
  HEAD_response_t head(const std::string& /*url*/, header_mask_t /*header_mask*/) override {
    HEAD_response_t resp;
    resp.status_ = status_code_t::FAILURE;
    return resp;
  }

private:
  rust::Box<RustTileGetter> getter_;
};
