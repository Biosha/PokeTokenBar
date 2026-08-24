//! Official Pokémon animated sprites via the PokéAPI, accessed through the [`rustemon`]
//! client (<https://crates.io/crates/rustemon>).
//!
//! The companion's sprite is the generation-V Black-White **animated GIF** (96×96,
//! `sprites.versions.generation-v.black-white.animated.front_default`), with the shiny
//! variant (`front_shiny`) when the companion is shiny and one exists. Sprites are
//! downloaded and cached to disk under the user cache dir so steady-state launches have
//! no per-species network round-trip. The on-disk GIF cache is the fast path; `rustemon`
//! is only consulted on a cache miss, and it is configured with `NoStore` (plus a
//! never-created cache dir) so it performs no HTTP caching of its own and does not
//! pollute the working directory.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// `$XDG_CACHE_HOME/PokeTokenBar` (falls back to `~/.cache/PokeTokenBar` if XDG unset).
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("PokeTokenBar")
}

/// Filesystem path a species sprite is stored at. Shiny sprites are cached under
/// `{slug}-shiny.gif` so a shiny re-roll of the same species does not shadow the
/// regular one (and vice versa).
pub fn cache_path(slug: &str) -> PathBuf {
    cache_dir()
        .join("pokeapi-sprites")
        .join(format!("{slug}.gif"))
}

/// One decoded sprite frame: a full canvas in RGBA8 plus its display delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpriteFrame {
    pub width: i32,
    pub height: i32,
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
}

/// Fetch the official animated GIF bytes for a species, using `rustemon` to talk to the
/// PokéAPI.
///
/// Resolves the artwork URL with
/// [`rustemon::pokemon::pokemon::get_by_name`](https://docs.rs/rustemon) (on a per-call
/// current-thread tokio runtime so this stays a sync API a worker thread can call),
/// downloads the bytes, and caches them to disk.
///
/// With `shiny = true` the shiny front variant is fetched when the API has one; when it
/// does not, the regular animated sprite is returned (and cached under the regular key).
///
/// Returns `Ok(None)` when the API returns no animated front URL.
///
/// # Errors
///
/// Returns an error when the artwork cannot be resolved, downloaded, or is not a GIF
/// (offline, unknown species, network failure).
pub fn fetch_gif(english_name: &str, shiny: bool) -> anyhow::Result<Option<Vec<u8>>> {
    let slug = slug(english_name);
    let key = if shiny {
        format!("{slug}-shiny")
    } else {
        slug.clone()
    };
    let dest = cache_path(&key);

    if let Ok(bytes) = fs::read(&dest) {
        if is_gif(&bytes) {
            return Ok(Some(bytes));
        }
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let resolved: anyhow::Result<(Option<String>, Option<String>)> = runtime.block_on(async {
        let client =
            rustemon::client::RustemonClientBuilder::<rustemon::client::MokaManager>::default()
                .with_mode(rustemon::client::CacheMode::NoStore)
                .try_build()
                .map_err(|e| anyhow::anyhow!("build rustemon client: {e}"))?;
        let pokemon = rustemon::pokemon::pokemon::get_by_name(&slug, &client)
            .await
            .map_err(|e| anyhow::anyhow!("fetch pokemon `{slug}`: {e}"))?;
        let animated = &pokemon.sprites.versions.generation_v.black_white.animated;
        Ok((animated.front_default.clone(), animated.front_shiny.clone()))
    });
    let (front_default, front_shiny) = resolved?;

    // Shiny requested but absent → serve (and cache) the regular sprite instead.
    if shiny
        && front_shiny
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .is_none()
    {
        return fetch_gif(english_name, false);
    }
    let url = match if shiny { front_shiny } else { front_default } {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => anyhow::bail!("no animated sprite URL for {slug}"),
    };

    let mut gif = Vec::new();
    ureq::get(url.as_str())
        .timeout(DOWNLOAD_TIMEOUT)
        .call()?
        .into_reader()
        .read_to_end(&mut gif)?;
    if !is_gif(&gif) {
        anyhow::bail!("downloaded sprite for {slug} is not a GIF");
    }

    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = dest.with_extension("gif.tmp");
    fs::write(&tmp, &gif)?;
    fs::rename(&tmp, &dest)?;

    Ok(Some(gif))
}

/// GIF file magic (`GIF87a` / `GIF89a`).
fn is_gif(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GIF8")
}

/// Canvas rectangle: `(x, y, width, height)`.
type Rect = (usize, usize, usize, usize);

/// The previous frame's pending disposal: its method, its rectangle, and the canvas
/// state from before it was composited (the state `Previous` restores).
struct PendingDisposal {
    method: gif::DisposalMethod,
    rect: Rect,
    state_before: Vec<u8>,
}

/// Decode a GIF into full-canvas RGBA frames with per-frame display delays. The decoder
/// runs in its default indexed mode (`read_next_frame` yields palette indices, which the
/// RGBA `read_into_buffer` path mangles for small palettes), and each frame is mapped
/// through its palette (falling back to the global one) and composited at its `(left, top)`
/// offset onto the screen-size canvas — so every frame we ship is a complete
/// width×height RGBA image.
///
/// Frames are usually *partial* (smaller than the canvas, offset) and carry a **disposal
/// method** describing what must happen to the previous frame's rectangle before the next
/// one is drawn: `Background` clears it, `Previous` restores the state from before that
/// frame, `Keep`/`Any` leaves it. Ignoring it leaves ghost pixels behind ("bavures") —
/// the PokéAPI Gen-V animated sprites lean on `Background` for exactly this.
///
/// A **transparent** pixel (index == the frame's transparency index) does not erase the
/// canvas: it leaves the previous content in place. The PokéAPI sprites even contain
/// fully transparent "rest" frames, which must display the retained canvas, not nothing.
/// The composed frames are pixel-identical (visible pixels) to Pillow's reference
/// compositor on the live Charmander sprite.
pub fn decode_gif_frames(bytes: &[u8]) -> anyhow::Result<Vec<SpriteFrame>> {
    if !is_gif(bytes) {
        anyhow::bail!("not a GIF");
    }
    let mut decoder = gif::Decoder::new(bytes)?;
    let width = decoder.width() as i32;
    let height = decoder.height() as i32;
    let w = width as usize;
    let h = height as usize;
    let mut canvas = vec![0u8; w * h * 4];
    let mut frames = Vec::new();
    let mut pending: Option<PendingDisposal> = None;
    loop {
        let frame = match decoder.read_next_frame()? {
            Some(frame) => frame.clone(),
            None => break,
        };
        if let Some(p) = &pending {
            match p.method {
                gif::DisposalMethod::Background => clear_rect(&mut canvas, w, h, p.rect),
                gif::DisposalMethod::Previous => {
                    copy_rect(&mut canvas, &p.state_before, w, h, p.rect)
                }
                gif::DisposalMethod::Any | gif::DisposalMethod::Keep => {}
            }
        }
        let palette: Option<&[u8]> = frame.palette.as_deref().or(decoder.global_palette());
        let transparent = frame.transparent;
        let fw = frame.width as usize;
        let fh = frame.height as usize;
        if fw == 0 || fh == 0 || frame.buffer.len() != fw * fh {
            anyhow::bail!("unexpected indexed frame size {fw}x{fh}");
        }
        let ox = frame.left as usize;
        let oy = frame.top as usize;
        let x0 = ox.min(w);
        let y0 = oy.min(h);
        let x1 = (ox + fw).min(w);
        let y1 = (oy + fh).min(h);
        let state_before = canvas.clone();
        if x0 < x1 && y0 < y1 {
            for (dy, y) in (y0..y1).enumerate() {
                for (dx, x) in (x0..x1).enumerate() {
                    let idx = frame.buffer[(oy + dy - y0) * fw + (ox + dx - x0)];
                    // A transparent pixel does NOT erase the canvas — it leaves whatever
                    // the previous frame (and its disposal) left there. Only the disposal
                    // methods clear pixels. Erasing here would double-erase and desync the
                    // frame from reference renderers.
                    if transparent == Some(idx) {
                        continue;
                    }
                    let dst = (y * w + x) * 4;
                    if let Some(pal) = &palette {
                        canvas[dst] = pal.get(idx as usize * 3).copied().unwrap_or(0);
                        canvas[dst + 1] = pal.get(idx as usize * 3 + 1).copied().unwrap_or(0);
                        canvas[dst + 2] = pal.get(idx as usize * 3 + 2).copied().unwrap_or(0);
                    }
                    canvas[dst + 3] = 255;
                }
            }
        }
        // Delays are in centiseconds; 0 is "unspecified" — clamp a minimum so a 0/1cs
        // frame does not strobe.
        let centis = frame.delay.max(2);
        frames.push(SpriteFrame {
            width,
            height,
            rgba: canvas.clone(),
            delay_ms: (centis * 10) as u32,
        });
        pending = Some(PendingDisposal {
            method: frame.dispose,
            rect: (ox, oy, fw, fh),
            state_before,
        });
    }
    if frames.is_empty() {
        anyhow::bail!("GIF has no frames");
    }
    Ok(frames)
}

/// Zero a canvas rectangle (bounds-clamped).
fn clear_rect(canvas: &mut [u8], w: usize, h: usize, rect: Rect) {
    let (x, y, rw, rh) = rect;
    for row in y.min(h)..(y + rh).min(h) {
        for col in x.min(w)..(x + rw).min(w) {
            canvas[(row * w + col) * 4..(row * w + col) * 4 + 4].fill(0);
        }
    }
}

/// Restore a canvas rectangle from a previously saved state.
fn copy_rect(dst: &mut [u8], src: &[u8], w: usize, h: usize, rect: Rect) {
    let (x, y, rw, rh) = rect;
    for row in y.min(h)..(y + rh).min(h) {
        for col in x.min(w)..(x + rw).min(w) {
            let i = (row * w + col) * 4;
            dst[i..i + 4].copy_from_slice(&src[i..i + 4]);
        }
    }
}

/// PokéAPI slug for an English species name (lowercase, spaces → `-`).
///
/// The on-disk cache is keyed by this slug (see [`cache_path`]), so callers deriving a
/// cache path from a species name must slug it first — a case-sensitive filesystem
/// (Linux) will not match `Charmander.png` against `charmander.png`.
pub fn slug(english_name: &str) -> String {
    english_name.trim().to_lowercase().replace(' ', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test frame spec: RGBA color + delay in centiseconds.
    type TestFrame = ((u8, u8, u8, u8), u16);

    fn encode_gif(width: u16, height: u16, frames: &[TestFrame]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = gif::Encoder::new(&mut out, width, height, &[]).expect("encoder");
        encoder.set_repeat(gif::Repeat::Infinite).expect("repeat");
        for ((r, g, b, a), delay) in frames {
            let mut rgba = vec![0u8; (width * height) as usize * 4];
            for px in rgba.chunks_mut(4) {
                px.copy_from_slice(&[*r, *g, *b, *a]);
            }
            let mut frame = gif::Frame::from_rgba(width, height, &mut rgba);
            frame.delay = *delay;
            encoder.write_frame(&frame).expect("write frame");
        }
        drop(encoder);
        out
    }

    #[test]
    fn slug_lowercases_and_hyphenates() {
        assert_eq!(slug("Charmander"), "charmander");
        assert_eq!(slug("  Gyarados "), "gyarados");
    }

    #[test]
    fn cache_roundtrip_reads_cached_gif() {
        let dir = cache_dir();
        let _ = fs::create_dir_all(dir.join("pokeapi-sprites"));
        let path = cache_path("pikachu");
        let payload = b"GIF89a fake-bytes";
        assert!(fs::write(&path, payload).is_ok());
        let got = fetch_gif("Pikachu", false)
            .expect("cache hit")
            .expect("bytes");
        assert_eq!(got, payload);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn shiny_cache_key_does_not_shadow_regular() {
        let dir = cache_dir();
        let _ = fs::create_dir_all(dir.join("pokeapi-sprites"));
        let regular = b"GIF89a regular-bytes";
        let shiny = b"GIF89a shiny-bytes";
        assert!(fs::write(cache_path("eevee"), regular).is_ok());
        assert!(fs::write(cache_path("eevee-shiny"), shiny).is_ok());
        assert_eq!(
            fetch_gif("Eevee", false).unwrap().unwrap(),
            regular.to_vec()
        );
        assert_eq!(fetch_gif("Eevee", true).unwrap().unwrap(), shiny.to_vec());
        let _ = fs::remove_file(cache_path("eevee"));
        let _ = fs::remove_file(cache_path("eevee-shiny"));
    }

    #[test]
    fn decode_roundtrip_yields_frames_and_delays() {
        let gif_bytes = encode_gif(2, 2, &[((255, 0, 0, 255), 5), ((0, 255, 0, 255), 15)]);
        let frames = decode_gif_frames(&gif_bytes).expect("decode");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].width, 2);
        assert_eq!(frames[0].height, 2);
        assert_eq!(frames[0].rgba.len(), 2 * 2 * 4);
        assert_eq!(frames[0].rgba[0], 255);
        assert_eq!(frames[1].rgba[0], 0);
        assert_eq!(frames[0].delay_ms, 50);
        assert_eq!(frames[1].delay_ms, 150);
    }

    #[test]
    fn decode_rejects_non_gif() {
        assert!(decode_gif_frames(b"not a gif at all").is_err());
    }

    /// A partial frame (offset, smaller than the canvas) with a local palette and an
    /// explicit disposal method — the shape the PokéAPI Gen-V sprites actually use.
    /// `rect` is `(width, height, left, top)`; the delay is the 10 cs (100 ms) every
    /// test frame shares.
    fn partial_frame(
        rect: (u16, u16, u16, u16),
        indices: Vec<u8>,
        palette: Vec<u8>,
        transparent: Option<u8>,
        dispose: gif::DisposalMethod,
    ) -> gif::Frame<'static> {
        let (width, height, left, top) = rect;
        gif::Frame {
            delay: 10,
            dispose,
            transparent,
            needs_user_input: false,
            top,
            left,
            width,
            height,
            interlaced: false,
            palette: Some(palette),
            buffer: std::borrow::Cow::Owned(indices),
        }
    }

    fn encode_partial(width: u16, height: u16, frames: Vec<gif::Frame<'static>>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = gif::Encoder::new(&mut out, width, height, &[]).expect("encoder");
        encoder.set_repeat(gif::Repeat::Infinite).expect("repeat");
        for f in frames {
            encoder.write_frame(&f).expect("write frame");
        }
        drop(encoder);
        out
    }

    fn pixel(rgba: &[u8], x: usize, y: usize, w: usize) -> (u8, u8, u8, u8) {
        let i = (y * w + x) * 4;
        (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
    }

    /// 4-slot local palette with `slot0` painted, remaining slots black.
    fn palette4(r: u8, g: u8, b: u8) -> Vec<u8> {
        vec![r, g, b, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    /// `Background` disposal: when the *next* frame is drawn, the previous frame's rectangle
    /// is cleared first, so no pixel from it survives there (the old "bavures"). Expected
    /// values cross-checked against Pillow's compositor.
    #[test]
    fn disposal_background_clears_stale_pixels() {
        let frames = vec![
            partial_frame(
                (3, 1, 0, 0),
                vec![0; 3],
                palette4(255, 0, 0),
                None,
                gif::DisposalMethod::Keep,
            ),
            partial_frame(
                (1, 1, 0, 0),
                vec![0],
                palette4(0, 255, 0),
                None,
                gif::DisposalMethod::Background,
            ),
            partial_frame(
                (1, 1, 2, 0),
                vec![0],
                palette4(0, 0, 255),
                None,
                gif::DisposalMethod::Keep,
            ),
        ];
        let decoded = decode_gif_frames(&encode_partial(3, 1, frames)).expect("decode");
        // While frame 1 is on screen, frame 0's red is still there (its disposal is Keep):
        // green drawn over it, red untouched elsewhere.
        let f1 = &decoded[1].rgba;
        assert_eq!(pixel(f1, 0, 0, 3), (0, 255, 0, 255));
        assert_eq!(pixel(f1, 1, 0, 3), (255, 0, 0, 255));
        // When frame 2 is drawn, frame 1's `Background` disposal cleared (0,0): neither the
        // stale green nor the red beneath survives — transparent.
        let f2 = &decoded[2].rgba;
        assert_eq!(pixel(f2, 0, 0, 3).3, 0, "stale pixel at (0,0)");
        assert_eq!(pixel(f2, 2, 0, 3), (0, 0, 255, 255));
        assert_eq!(pixel(f2, 1, 0, 3), (255, 0, 0, 255));
    }

    /// A fully transparent frame must display the retained canvas (previous frame's
    /// pixels, since its `Keep` disposal did not clear them) — not a blank image.
    #[test]
    fn transparent_frame_preserves_canvas() {
        let frames = vec![
            partial_frame(
                (2, 1, 0, 0),
                vec![0, 0],
                palette4(255, 0, 0),
                None,
                gif::DisposalMethod::Keep,
            ),
            // all pixels use the transparent slot (index 1, color unused)
            partial_frame(
                (2, 1, 0, 0),
                vec![1, 1],
                palette4(255, 0, 0),
                Some(1),
                gif::DisposalMethod::Background,
            ),
        ];
        let decoded = decode_gif_frames(&encode_partial(2, 1, frames)).expect("decode");
        assert_eq!(pixel(&decoded[1].rgba, 0, 0, 2), (255, 0, 0, 255));
        assert_eq!(pixel(&decoded[1].rgba, 1, 0, 2), (255, 0, 0, 255));
    }

    /// `Previous` disposal: the rectangle is restored to the state from *before* the
    /// previous frame, not to its result.
    #[test]
    fn disposal_previous_restores_prior_state() {
        let frames = vec![
            partial_frame(
                (2, 1, 0, 0),
                vec![0, 0],
                palette4(255, 0, 0),
                None,
                gif::DisposalMethod::Keep,
            ),
            partial_frame(
                (1, 1, 0, 0),
                vec![0],
                palette4(0, 255, 0),
                None,
                gif::DisposalMethod::Previous,
            ),
            partial_frame(
                (1, 1, 0, 0),
                vec![0],
                palette4(255, 255, 0),
                None,
                gif::DisposalMethod::Keep,
            ),
        ];
        let decoded = decode_gif_frames(&encode_partial(2, 1, frames)).expect("decode");
        let f3 = &decoded[2].rgba;
        // Before the third frame, the green frame's `Previous` disposal restored (0,0) to
        // red; then the third frame drew yellow there. (1,0) kept its red from frame one.
        assert_eq!(pixel(f3, 0, 0, 2), (255, 255, 0, 255));
        assert_eq!(pixel(f3, 1, 0, 2), (255, 0, 0, 255));
    }

    /// Dump every decoded frame of a cached sprite as PAM (RGBA) files for pixel-level
    /// comparison against a reference renderer (e.g. Pillow). Run with:
    /// `cargo test -p poketoken-core dump_frames_pam -- --ignored --nocapture`
    #[test]
    #[ignore = "manual (writes PAM dumps next to the gif cache)"]
    fn dump_frames_pam() {
        let path = cache_path("charmander");
        let bytes = fs::read(&path).expect("cached charmander.gif");
        let frames = decode_gif_frames(&bytes).expect("decode");
        for (i, f) in frames.iter().enumerate() {
            let header = format!(
                "P7\nHEIGHT {}\nWIDTH {}\nDEPTHS 4\nMAXVAL 255\nTYPE RGB\n",
                f.height, f.width
            );
            let mut out = header.into_bytes();
            out.extend_from_slice(&f.rgba);
            let out_path = path.with_extension(format!("pam.{i:03}"));
            fs::write(&out_path, out).expect("write pam");
        }
        println!(
            "wrote {} frames as PAM next to {}",
            frames.len(),
            path.display()
        );
    }

    /// End-to-end through `rustemon`: the animated sprite resolves and downloads as GIF.
    #[test]
    #[ignore = "live network (requires internet)"]
    fn fetches_animated_sprite_live_via_rustemon() {
        let Some(bytes) = fetch_gif("Charmander", false).expect("live resolve") else {
            panic!("expected a Charmander sprite");
        };
        assert!(is_gif(&bytes));
        let frames = decode_gif_frames(&bytes).expect("decode");
        assert!(frames.len() >= 2);
        // Gen-V Black-White animated sprites (41×42 on the current PokéAPI).
        assert!(frames[0].width >= 32 && frames[0].height >= 32);
        // At least one visible (non-transparent) pixel.
        assert!(frames[0].rgba.chunks(4).any(|p| p[3] > 0));
    }
}
