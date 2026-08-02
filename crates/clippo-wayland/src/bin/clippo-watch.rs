//! `clippo-watch` — the M1 debug binary: print every selection and its flavors.
//!
//! This is the only way to find out whether the data-control client works
//! against the real compositor, because nothing in CI can run against one. It
//! is the M1 gate in `docs/ROADMAP.md`: run it from a host terminal, copy text
//! / an image / a file, and read the block each copy prints.
//!
//! **It must be run from a host terminal.** A Flatpak-proxied Wayland socket
//! filters out privileged protocols, so data-control is invisible from inside
//! one — which is why every failure path here prints `$WAYLAND_DISPLAY`.
//!
//! Everything below is formatting. All protocol handling lives in the library;
//! if a block needs a fact this binary cannot see, that fact belongs on
//! [`Selection`], not in a second copy of the client.

use std::io;

use clap::Parser;
use clippo_wayland::{
    Flavor, Selection, SelectionKind, WatchConfig, DEFAULT_MAX_FLAVOR_BYTES,
    PASSWORD_MANAGER_HINT_MIME,
};
use tracing_subscriber::EnvFilter;

/// How much of a text flavor to show before cutting it off.
const DEFAULT_PREVIEW_CHARS: usize = 72;

#[derive(Debug, Parser)]
#[command(
    name = "clippo-watch",
    version,
    about = "Print every clipboard selection and its flavors",
    long_about = "Print every clipboard selection and its flavors.\n\n\
                  Run this from a host terminal (cosmic-term), not from a \
                  Flatpak: the proxied Wayland socket filters out the \
                  data-control protocol this depends on."
)]
struct Args {
    /// Also watch the middle-click primary selection.
    #[arg(long)]
    primary: bool,

    /// Drop any flavor larger than this many bytes.
    ///
    /// Lower it to exercise the drop path by hand — `--max-bytes 1024` and a
    /// copied screenshot is enough to see an `image/png` reported as dropped.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_FLAVOR_BYTES)]
    max_bytes: usize,

    /// How many characters of a text flavor to preview.
    #[arg(long, value_name = "CHARS", default_value_t = DEFAULT_PREVIEW_CHARS)]
    preview: usize,
}

fn main() {
    let args = Args::parse();

    // Library logs go to stderr so they interleave without corrupting the
    // blocks on stdout. `RUST_LOG=clippo_wayland=trace` for the gory detail.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = WatchConfig {
        primary: args.primary,
        max_flavor_bytes: args.max_bytes,
        ..WatchConfig::default()
    };

    let (watcher, mut selections) = match clippo_wayland::watch(config) {
        Ok(started) => started,
        Err(error) => {
            report_failure(&error);
            std::process::exit(1);
        }
    };

    println!(
        "clippo-watch: bound {} on WAYLAND_DISPLAY={}",
        watcher.protocol(),
        wayland_display()
    );
    println!(
        "              per-flavor cap {} B, primary capture {}. Copy something; Ctrl-C to stop.",
        args.max_bytes,
        if args.primary { "on" } else { "off" }
    );

    let mut count: u64 = 0;
    while let Some(selection) = selections.blocking_recv() {
        count += 1;
        let mut stdout = io::stdout().lock();
        if write_selection(
            &mut stdout,
            count,
            watcher.protocol(),
            &selection,
            args.preview,
        )
        .is_err()
        {
            // stdout is gone — the pipe we were being read through closed.
            break;
        }
    }
    watcher.stop();
}

/// Print why the watcher would not start, and the one environment variable that
/// explains it nine times out of ten.
fn report_failure(error: &clippo_wayland::Error) {
    eprintln!("clippo-watch: {error}");
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
    eprintln!();
    eprintln!("WAYLAND_DISPLAY={}", wayland_display());
    eprintln!(
        "It must be wayland-0. wayland-1 is Flatpak's proxied socket, which filters out \
         data-control; run this from a host terminal instead."
    );
}

fn wayland_display() -> String {
    std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".to_owned())
}

/// One block per selection: what was offered, what came back, and what did not.
///
/// Writes to `out` rather than straight to stdout so the block can be rendered
/// and asserted on in a test — which is the only coverage this formatting can
/// get, there being no compositor in CI to produce a real selection.
fn write_selection(
    out: &mut impl io::Write,
    number: u64,
    protocol: &str,
    selection: &Selection,
    preview_chars: usize,
) -> io::Result<()> {
    let kind = match selection.kind {
        SelectionKind::Clipboard => "clipboard",
        SelectionKind::Primary => "primary",
    };

    writeln!(out)?;
    writeln!(out, "── selection #{number}  {kind}  ({protocol})")?;
    writeln!(
        out,
        "   advertised: {}",
        join_or_none(&selection.advertised)
    )?;

    // The one flavor whose *presence* is the signal, so it gets its own line
    // rather than being left to be spotted in a list.
    if selection.has_password_manager_hint() {
        writeln!(
            out,
            "   ⚠  {PASSWORD_MANAGER_HINT_MIME} present — the source marked this a credential"
        )?;
    }

    if selection.flavors.is_empty() {
        writeln!(out, "   fetched: <nothing>")?;
    } else {
        writeln!(out, "   fetched:")?;
        let width = mime_width(selection.flavors.iter().map(|flavor| flavor.mime.as_str()));
        for flavor in &selection.flavors {
            writeln!(
                out,
                "     {:width$}  {:>10}  {}",
                flavor.mime,
                format!("{} B", flavor.data.len()),
                render(flavor, preview_chars)
            )?;
        }
    }

    if !selection.dropped.is_empty() {
        writeln!(out, "   dropped:")?;
        let width = mime_width(
            selection
                .dropped
                .iter()
                .map(|dropped| dropped.mime.as_str()),
        );
        for dropped in &selection.dropped {
            writeln!(out, "     {:width$}  {}", dropped.mime, dropped.reason)?;
        }
    }

    let skipped = selection.skipped();
    if !skipped.is_empty() {
        writeln!(out, "   skipped (uninteresting): {}", skipped.join(", "))?;
    }
    Ok(())
}

fn join_or_none(mimes: &[String]) -> String {
    if mimes.is_empty() {
        "<none>".to_owned()
    } else {
        mimes.join(", ")
    }
}

fn mime_width<'a>(mimes: impl Iterator<Item = &'a str>) -> usize {
    mimes.map(|mime| mime.chars().count()).max().unwrap_or(0)
}

/// What to show for a flavor's contents.
///
/// Only text is previewed. A PNG written raw to a terminal is at best noise and
/// at worst a pile of escape sequences the terminal obeys, so binary flavors
/// report their size and nothing else.
fn render(flavor: &Flavor, preview_chars: usize) -> String {
    if clippo_wayland::is_password_manager_hint(&flavor.mime) {
        format!(
            "<marker, {} bytes — its presence is the signal>",
            flavor.data.len()
        )
    } else if is_text(&flavor.mime) {
        preview(&flavor.data, preview_chars)
    } else {
        format!("<binary, {} bytes, not printed>", flavor.data.len())
    }
}

fn is_text(mime: &str) -> bool {
    mime.trim_start().to_ascii_lowercase().starts_with("text/")
}

/// The first `max_chars` characters, quoted, with control characters escaped so
/// a multi-line copy stays on one line of output.
fn preview(data: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(data);
    let mut out = String::new();
    let mut truncated = false;

    for (taken, character) in text.chars().enumerate() {
        if taken == max_chars {
            truncated = true;
            break;
        }
        match character {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // Anything else the terminal would act on rather than show.
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }

    if truncated {
        format!("\"{out}\"…")
    } else {
        format!("\"{out}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clippo_wayland::{DropReason, DroppedFlavor};

    fn flavor(mime: &str, data: &[u8]) -> Flavor {
        Flavor {
            mime: mime.to_owned(),
            data: data.to_vec(),
        }
    }

    fn block(selection: &Selection) -> String {
        let mut out = Vec::new();
        write_selection(&mut out, 3, "ext_data_control_v1", selection, 72).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_preview_is_quoted_and_left_alone_when_it_fits() {
        assert_eq!(
            preview(b"hello from cosmic-term", 72),
            "\"hello from cosmic-term\""
        );
        assert_eq!(preview(b"", 72), "\"\"");
    }

    #[test]
    fn a_preview_stops_at_the_character_limit_and_says_so() {
        assert_eq!(preview(b"abcdefghij", 4), "\"abcd\"…");
        // Counted in characters, not bytes, so multi-byte text is not cut mid
        // codepoint.
        assert_eq!(preview("äöüßabc".as_bytes(), 3), "\"äöü\"…");
    }

    #[test]
    fn newlines_and_control_characters_are_made_visible() {
        let rendered = preview(b"one\ntwo\tthree\r\n", 72);
        assert_eq!(rendered, "\"one\\ntwo\\tthree\\r\\n\"");
        assert!(!rendered.contains('\n'), "the block must stay on one line");

        // An escape sequence must not reach the terminal as one.
        let escaped = preview(b"\x1b[31mred", 72);
        assert_eq!(escaped, "\"\\u{1b}[31mred\"");
        assert!(!escaped.contains('\x1b'));
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(preview(br#"say "hi"\ok"#, 72), r#""say \"hi\"\\ok""#);
    }

    #[test]
    fn binary_flavors_report_their_size_instead_of_their_bytes() {
        let png = flavor("image/png", &[0x89, b'P', b'N', b'G', 0x00, 0x1b]);
        let rendered = render(&png, 72);
        assert_eq!(rendered, "<binary, 6 bytes, not printed>");
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains("PNG"));
    }

    #[test]
    fn text_flavors_are_previewed_whatever_the_charset_spelling() {
        assert!(is_text("text/plain"));
        assert!(is_text("text/plain;charset=UTF-8"));
        assert!(is_text("TEXT/HTML"));
        assert!(is_text("text/uri-list"));
        assert!(!is_text("image/png"));
        assert!(!is_text("image/jpeg"));
        assert!(!is_text(PASSWORD_MANAGER_HINT_MIME));

        assert_eq!(
            render(&flavor("text/plain;charset=utf-8", b"hi"), 72),
            "\"hi\""
        );
    }

    #[test]
    fn the_password_marker_is_never_previewed() {
        let rendered = render(&flavor(PASSWORD_MANAGER_HINT_MIME, b"secret"), 72);
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("presence is the signal"), "{rendered}");
    }

    #[test]
    fn mime_column_is_as_wide_as_its_widest_entry() {
        assert_eq!(mime_width(["text/html", "text/plain"].into_iter()), 10);
        assert_eq!(mime_width(std::iter::empty()), 0);
    }

    #[test]
    fn an_empty_advertised_list_reads_as_none() {
        assert_eq!(join_or_none(&[]), "<none>");
        assert_eq!(
            join_or_none(&["text/plain".to_owned(), "TARGETS".to_owned()]),
            "text/plain, TARGETS"
        );
    }

    /// The shape of a block, end to end: the number and protocol, every
    /// advertised type, what was fetched with its size and preview, and what was
    /// only advertised.
    #[test]
    fn a_text_selection_renders_a_full_block() {
        let selection = Selection {
            kind: SelectionKind::Clipboard,
            advertised: [
                "text/plain;charset=utf-8",
                "text/plain",
                "text/html",
                "TIMESTAMP",
                "TARGETS",
            ]
            .map(String::from)
            .to_vec(),
            flavors: vec![
                flavor("text/plain;charset=utf-8", b"hello from cosmic-term"),
                flavor("text/html", b"<meta charset=utf-8>hello"),
            ],
            dropped: Vec::new(),
        };

        assert_eq!(
            block(&selection),
            "\n\
             ── selection #3  clipboard  (ext_data_control_v1)\n   \
             advertised: text/plain;charset=utf-8, text/plain, text/html, TIMESTAMP, TARGETS\n   \
             fetched:\n     \
             text/plain;charset=utf-8        22 B  \"hello from cosmic-term\"\n     \
             text/html                       25 B  \"<meta charset=utf-8>hello\"\n   \
             skipped (uninteresting): TIMESTAMP, TARGETS\n"
        );
    }

    #[test]
    fn an_image_block_reports_the_size_and_the_dropped_flavor_that_hit_the_cap() {
        let selection = Selection {
            kind: SelectionKind::Clipboard,
            advertised: ["image/png", "image/jpeg"].map(String::from).to_vec(),
            flavors: vec![flavor("image/png", &[0u8; 4096])],
            dropped: vec![DroppedFlavor {
                mime: "image/jpeg".to_owned(),
                reason: DropReason::OverCap { cap: 1024 },
            }],
        };

        let rendered = block(&selection);
        assert!(
            rendered.contains("image/png      4096 B  <binary, 4096 bytes, not printed>"),
            "{rendered}"
        );
        // The cap that rejected it, named, rather than a silent omission.
        assert!(
            rendered
                .contains("   dropped:\n     image/jpeg  exceeded the 1024 byte per-flavor cap"),
            "{rendered}"
        );
    }

    #[test]
    fn the_password_hint_gets_its_own_line_in_the_block() {
        let selection = Selection {
            kind: SelectionKind::Clipboard,
            advertised: ["text/plain", PASSWORD_MANAGER_HINT_MIME]
                .map(String::from)
                .to_vec(),
            flavors: vec![
                flavor("text/plain", b"hunter2"),
                flavor(PASSWORD_MANAGER_HINT_MIME, b"secret"),
            ],
            dropped: Vec::new(),
        };

        let rendered = block(&selection);
        assert!(
            rendered.contains(&format!(
                "   ⚠  {PASSWORD_MANAGER_HINT_MIME} present — the source marked this a credential"
            )),
            "{rendered}"
        );
        // The marker's payload is not the signal and is never echoed.
        assert!(!rendered.contains("secret"), "{rendered}");
    }

    #[test]
    fn a_primary_selection_says_so() {
        let selection = Selection {
            kind: SelectionKind::Primary,
            advertised: vec!["text/plain".to_owned()],
            flavors: vec![flavor("text/plain", b"middle-click")],
            dropped: Vec::new(),
        };
        assert!(block(&selection).contains("── selection #3  primary  (ext_data_control_v1)"));
    }

    /// A selection where everything was dropped still has to say what it lost —
    /// that is the case `clippo-watch` exists to explain.
    #[test]
    fn a_wholly_dropped_selection_still_has_something_to_report() {
        let selection = Selection {
            kind: SelectionKind::Clipboard,
            advertised: ["image/png", "TIMESTAMP"].map(String::from).to_vec(),
            flavors: Vec::new(),
            dropped: vec![DroppedFlavor {
                mime: "image/png".to_owned(),
                reason: DropReason::OverCap { cap: 1024 },
            }],
        };
        assert!(selection.is_empty());

        let rendered = block(&selection);
        assert!(rendered.contains("   fetched: <nothing>"), "{rendered}");
        assert!(
            rendered.contains("     image/png  exceeded the 1024 byte per-flavor cap"),
            "{rendered}"
        );
        assert!(
            rendered.contains("   skipped (uninteresting): TIMESTAMP"),
            "{rendered}"
        );
    }
}
