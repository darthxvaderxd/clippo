//! Image blobs, the PNG thumbnail stored beside one, and the flavors that must
//! never be handed back to the compositor.
//!
//! DESIGN.md, `clippo-store` → "Images":
//!
//! > A PNG thumbnail is generated at capture with the `image` crate and stored
//! > as a separate `image/png;clippo-thumb` flavor, so the applet never decodes
//! > full-size images.
//!
//! The thumbnail is a *flavor row on the same entry*, not a column. That buys
//! the schema nothing extra and costs one thing that has to be paid for
//! deliberately: the copy-back path must not offer it. Pasting a 256-pixel
//! thumbnail where the user asked for their screenshot is a silent, plausible
//! wrong answer — the paste succeeds and the image is simply the wrong one — so
//! the exclusion lives here as [`NEVER_OFFERED`], next to the MIME it mirrors,
//! rather than as a condition M3c has to remember to write.

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

/// The derived-thumbnail flavor.
///
/// A parameter on `image/png` rather than a type of its own, so anything that
/// only knows about MIME essences — [`EntryKind::from_mime`][kind] included —
/// still sees a PNG.
///
/// [kind]: clippo_core::EntryKind::from_mime
pub const THUMBNAIL_MIME: &str = "image/png;clippo-thumb";

/// Flavors clippo stores but must **never** offer back to the compositor.
///
/// Read this when building a copy-back offer: every stored flavor is offered
/// except the ones named here. It is a list rather than a single constant
/// because the reason generalises — anything clippo derives for its own use,
/// as opposed to something an application actually put on the clipboard,
/// belongs in it.
pub const NEVER_OFFERED: &[&str] = &[THUMBNAIL_MIME];

/// The longest edge of a generated thumbnail, in pixels.
///
/// Enough for a crisp list row on a 2× display and small enough that the
/// re-encoded PNG is a few kilobytes, which is the point: the applet reads
/// these on every repaint and must never touch the full-size blob.
pub const THUMBNAIL_MAX_EDGE: u32 = 256;

/// Ceiling on what decoding one clipboard image may allocate.
///
/// A decompression bomb — a PNG that is kilobytes on disk and gigabytes
/// decoded — arrives through the clipboard as easily as through a download, and
/// the daemon is long-lived. `image` enforces this while decoding, so an
/// oversized picture fails as a thumbnail error rather than as an OOM kill that
/// takes the user's whole history process with it.
const DECODE_ALLOC_LIMIT: u64 = 256 * 1024 * 1024;

/// Why an image could not be turned into a thumbnail.
///
/// Never fatal to an insert: DESIGN.md wants the entry stored either way, and a
/// list row without a preview image is a far better outcome than a copy that
/// vanished because it was a format `image` does not build with.
#[derive(Debug, thiserror::Error)]
pub enum ThumbnailError {
    /// The bytes are not an image clippo can decode — corrupt, truncated, or a
    /// format this build has no decoder for.
    #[error("the copied image could not be decoded")]
    Decode(#[source] image::ImageError),

    /// The downscaled image could not be written back out as a PNG.
    #[error("the thumbnail could not be encoded as a PNG")]
    Encode(#[source] image::ImageError),
}

/// A PNG thumbnail of `data`, at most [`THUMBNAIL_MAX_EDGE`] on its longest
/// edge.
///
/// The format is guessed from the bytes rather than taken from the MIME type
/// the source advertised: applications mislabel clipboard images, and the
/// header is the thing that decides whether a decode will work.
///
/// The output is always a PNG whatever went in, which is what makes
/// [`THUMBNAIL_MIME`] honest — the applet decodes one format, not whichever the
/// source happened to use. An image already inside the box keeps its own size:
/// this only ever scales down.
pub fn thumbnail(data: &[u8]) -> Result<Vec<u8>, ThumbnailError> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|error| ThumbnailError::Decode(image::ImageError::IoError(error)))?;

    let mut limits = Limits::default();
    limits.max_alloc = Some(DECODE_ALLOC_LIMIT);
    reader.limits(limits);

    let decoded = reader.decode().map_err(ThumbnailError::Decode)?;
    // `DynamicImage::thumbnail` scales to the *largest* size that fits the box,
    // which means it enlarges a small picture as readily as it shrinks a big
    // one. A blown-up 16×16 favicon is bigger than the image it came from and
    // no more informative, so anything already inside the box is only
    // re-encoded.
    let scaled = if decoded.width() > THUMBNAIL_MAX_EDGE || decoded.height() > THUMBNAIL_MAX_EDGE {
        decoded.thumbnail(THUMBNAIL_MAX_EDGE, THUMBNAIL_MAX_EDGE)
    } else {
        decoded
    };

    let mut png = Vec::new();
    scaled
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(ThumbnailError::Encode)?;
    Ok(png)
}

/// Whether a MIME type is clippo's derived thumbnail rather than a captured
/// flavor.
pub fn is_thumbnail(mime: &str) -> bool {
    same_mime(mime, THUMBNAIL_MIME)
}

/// Whether a stored flavor may be offered back to the compositor on a paste.
///
/// The copy-back path's filter: `stored.flavors.iter().filter(|f|
/// is_offerable(&f.mime))`.
pub fn is_offerable(mime: &str) -> bool {
    !NEVER_OFFERED
        .iter()
        .any(|excluded| same_mime(mime, excluded))
}

/// Whether two MIME types are the same one, ignoring case and the whitespace
/// toolkits sprinkle around parameters.
///
/// The same normalisation `clippo-wayland`'s `mime` module applies, and for the
/// same reason: `image/png; clippo-thumb` and `image/png;clippo-thumb` are one
/// type, and a thumbnail that escaped the filter on a stray space would paste
/// as the wrong image.
fn same_mime(mime: &str, other: &str) -> bool {
    let squeeze =
        |value: &str| -> String { value.chars().filter(|c| !c.is_ascii_whitespace()).collect() };
    squeeze(mime).eq_ignore_ascii_case(&squeeze(other))
}

#[cfg(test)]
pub(crate) mod testing {
    //! Real encoded images for the tests, built with the same crate that reads
    //! them back — a hand-rolled byte array would only prove the decoder
    //! rejects it.

    use image::{ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    /// A `width` × `height` PNG with a recognisable gradient in it.
    pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
        encode(width, height, ImageFormat::Png)
    }

    /// The same picture as a JPEG, for the "any image flavor" paths.
    pub(crate) fn jpeg(width: u32, height: u32) -> Vec<u8> {
        encode(width, height, ImageFormat::Jpeg)
    }

    fn encode(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let mut picture = RgbaImage::new(width, height);
        for (x, y, pixel) in picture.enumerate_pixels_mut() {
            *pixel = Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(picture)
            .into_rgb8()
            .write_to(&mut Cursor::new(&mut bytes), format)
            .expect("the test image should encode");
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dimensions of an encoded image, read back through `image`.
    fn size(bytes: &[u8]) -> (u32, u32) {
        let decoded = image::load_from_memory(bytes).expect("a decodable image");
        (decoded.width(), decoded.height())
    }

    #[test]
    fn the_thumbnail_mime_is_the_one_design_md_names() {
        assert_eq!(THUMBNAIL_MIME, "image/png;clippo-thumb");
    }

    #[test]
    fn a_large_image_is_scaled_into_the_box_with_its_aspect_ratio_intact() {
        let thumb = thumbnail(&testing::png(1_000, 500)).unwrap();
        assert_eq!(size(&thumb), (THUMBNAIL_MAX_EDGE, THUMBNAIL_MAX_EDGE / 2));
        assert!(
            thumb.len() < testing::png(1_000, 500).len(),
            "a thumbnail that is not smaller than its source has no purpose"
        );
    }

    #[test]
    fn an_image_already_smaller_than_the_box_is_not_upscaled() {
        let thumb = thumbnail(&testing::png(16, 8)).unwrap();
        assert_eq!(size(&thumb), (16, 8));
    }

    #[test]
    fn whatever_goes_in_a_png_comes_out() {
        // The applet decodes one format; that is what makes THUMBNAIL_MIME
        // truthful for a JPEG screenshot too.
        let thumb = thumbnail(&testing::jpeg(400, 400)).unwrap();
        assert_eq!(
            image::guess_format(&thumb).unwrap(),
            ImageFormat::Png,
            "the thumbnail must be a PNG whatever the source was"
        );
    }

    #[test]
    fn the_format_is_guessed_from_the_bytes_rather_than_trusted() {
        // A JPEG that some application advertised as `image/png` still
        // thumbnails, because nothing here reads the advertised type.
        assert!(thumbnail(&testing::jpeg(64, 64)).is_ok());
    }

    #[test]
    fn corrupt_or_unsupported_bytes_are_an_error_rather_than_a_panic() {
        let error = thumbnail(b"not an image at all").unwrap_err();
        assert!(matches!(error, ThumbnailError::Decode(_)), "{error:?}");
        assert!(error.to_string().contains("could not be decoded"));

        // A truncated PNG: a valid header with the pixels cut off, which is
        // what a capture interrupted mid-read actually looks like.
        let full = testing::png(64, 64);
        assert!(thumbnail(&full[..full.len() / 2]).is_err());

        assert!(thumbnail(&[]).is_err());
    }

    #[test]
    fn the_thumbnail_is_the_one_flavor_that_is_never_offered_back() {
        assert!(!is_offerable(THUMBNAIL_MIME));
        assert!(is_offerable("image/png"));
        assert!(is_offerable("image/jpeg"));
        assert!(is_offerable("text/plain;charset=utf-8"));
        assert_eq!(NEVER_OFFERED, &[THUMBNAIL_MIME]);
    }

    #[test]
    fn a_thumbnail_is_recognised_however_it_is_spelled() {
        for spelling in [
            "image/png;clippo-thumb",
            "image/png; clippo-thumb",
            "IMAGE/PNG;CLIPPO-THUMB",
            " image/png ; clippo-thumb ",
        ] {
            assert!(is_thumbnail(spelling), "{spelling}");
            assert!(!is_offerable(spelling), "{spelling}");
        }
        assert!(!is_thumbnail("image/png"));
    }
}
