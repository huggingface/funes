//! The dataset card a published memory carries: generated whole on first publish, its stats
//! refreshed on later pushes — a hand-written card is never touched. The card is what makes a
//! memory on the Hub identifiable (and, via the `funes` tag, discoverable) as a funes memory, and
//! it doubles as usage instructions for whoever lands on the dataset page.
//!
//! Pure text in/out: [`plan`] decides what to do from the remote README's current content; the
//! push path owns fetching that content and committing the result.

/// Opens the region [`plan`] may rewrite; everything outside it belongs to the owner.
const STATS_OPEN: &str = "<!-- funes:stats -->";
/// Closes the region [`plan`] may rewrite.
const STATS_CLOSE: &str = "<!-- /funes:stats -->";

/// Frontmatter tags. `funes` is the load-bearing one: it makes every published memory
/// discoverable via `huggingface.co/datasets?other=funes`. `agent-memory` names the category
/// (no incumbent Hub tag exists — the ecosystem's category word, scoped to agents).
const TAGS: [&str; 5] = ["funes", "agent-memory", "agent-traces", "embeddings", "lance"];

/// The Hub's `size_categories` bands.
const SIZE_BANDS: [(u64, &str); 10] = [
    (1_000, "n<1K"),
    (10_000, "1K<n<10K"),
    (100_000, "10K<n<100K"),
    (1_000_000, "100K<n<1M"),
    (10_000_000, "1M<n<10M"),
    (100_000_000, "10M<n<100M"),
    (1_000_000_000, "100M<n<1B"),
    (10_000_000_000, "1B<n<10B"),
    (100_000_000_000, "10B<n<100B"),
    (1_000_000_000_000, "100B<n<1T"),
];

/// What the card says about the memory.
pub struct CardCtx<'a> {
    /// The repo id the card lives in, `<org>/<name>` — named in the recall example.
    pub repo: &'a str,
    /// Chunk count after the push this card rides on.
    pub chunks: u64,
    /// The memory's pinned embedding model (schema metadata `embedding_model`).
    pub embedding_model: &'a str,
    /// UTC date of the push, `YYYY-MM-DD`.
    pub date: &'a str,
}

/// What a push should do about the card.
#[derive(Debug, PartialEq)]
pub enum CardAction {
    /// No README on the remote: write this full card.
    Create(String),
    /// The README carries the stats markers: replace the file with this (only the marker
    /// region differs).
    Refresh(String),
    /// A README without markers (hand-written), or one whose stats are already current.
    LeaveAlone,
}

/// Decide what a push does about the card, from the remote README's current content. Ownership
/// rules: no README → create the full card; markers present → refresh the region between them
/// (skipping a byte-identical result); markers absent → the file is the owner's, hands off.
pub fn plan(existing: Option<&str>, ctx: &CardCtx) -> CardAction {
    let Some(text) = existing else {
        return CardAction::Create(render(ctx));
    };
    let Some(refreshed) = splice(text, ctx) else {
        return CardAction::LeaveAlone;
    };
    if refreshed == text {
        CardAction::LeaveAlone
    } else {
        CardAction::Refresh(refreshed)
    }
}

/// The full generated card: frontmatter (tags + size band), what a funes memory is, how to
/// recall from it, and the stats region.
fn render(ctx: &CardCtx) -> String {
    let tags = TAGS.map(|t| format!("  - {t}\n")).concat();
    format!(
        "---\n\
         pretty_name: funes memory\n\
         tags:\n\
         {tags}\
         size_categories:\n  - {band}\n\
         ---\n\
         \n\
         # funes memory\n\
         \n\
         A [funes](https://github.com/huggingface/funes) memory: agent sessions chunked,\n\
         embedded, and written to a [Lance](https://lancedb.github.io/lance/) table — a derived\n\
         index holding verbatim passages with exact provenance, not raw transcripts.\n\
         \n\
         Any agent (or you) can recall from it directly — no local index needed:\n\
         \n\
         ```bash\n\
         funes recall \"what did we decide about …\" --memory {repo}\n\
         ```\n\
         \n\
         Get funes:\n\
         \n\
         ```bash\n\
         curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/install.sh | sh\n\
         ```\n\
         \n\
         {stats}\n\
         \n\
         *Published and kept current by `funes push`.*\n",
        band = size_category(ctx.chunks),
        repo = ctx.repo,
        stats = stats_block(ctx),
    )
}

/// The marker-delimited stats region, markers included — the only part a refresh rewrites.
fn stats_block(ctx: &CardCtx) -> String {
    format!(
        "{STATS_OPEN}\n\
         | | |\n\
         |---|---|\n\
         | Chunks | {chunks} |\n\
         | Embedding model | `{model}` |\n\
         | Updated | {date} |\n\
         {STATS_CLOSE}",
        chunks = thousands(ctx.chunks),
        model = ctx.embedding_model,
        date = ctx.date,
    )
}

/// Replace the marker region of `text` with fresh stats, or None when the markers aren't there
/// (the close marker must follow the open one).
fn splice(text: &str, ctx: &CardCtx) -> Option<String> {
    let open = text.find(STATS_OPEN)?;
    let close = open + text[open..].find(STATS_CLOSE)? + STATS_CLOSE.len();
    Some(format!("{}{}{}", &text[..open], stats_block(ctx), &text[close..]))
}

/// The Hub `size_categories` band holding `n`.
fn size_category(n: u64) -> &'static str {
    SIZE_BANDS
        .iter()
        .find(|(upper, _)| n < *upper)
        .map(|(_, band)| *band)
        .unwrap_or("n>1T")
}

/// `21636` → `21,636`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(chunks: u64) -> CardCtx<'static> {
        CardCtx {
            repo: "acme/kb",
            chunks,
            embedding_model: "BAAI/bge-small-en-v1.5",
            date: "2026-07-16",
        }
    }

    #[test]
    fn create_when_the_remote_has_no_readme() {
        let CardAction::Create(card) = plan(None, &ctx(21_636)) else {
            panic!("expected Create");
        };
        // Frontmatter: the discovery tag, the computed band.
        assert!(card.starts_with("---\n"), "frontmatter first");
        // Column-anchored: the `\` line continuations strip leading whitespace, so keys sit at
        // column one and list items exactly two spaces in — what YAML requires. A reflow of the
        // literals that breaks this fails here, not on the Hub.
        assert!(card.contains("\ntags:\n  - funes\n"));
        assert!(card.contains("  - agent-memory\n"));
        assert!(card.contains("\nsize_categories:\n  - 10K<n<100K\n"));
        assert!(card.contains("\n# funes memory\n"));
        // Body: the recall example names this memory; the stats region is marker-delimited.
        assert!(card.contains("--memory acme/kb"));
        assert!(card.contains(STATS_OPEN) && card.contains(STATS_CLOSE));
        assert!(card.contains("| Chunks | 21,636 |"));
    }

    #[test]
    fn refresh_rewrites_only_the_marker_region() {
        // A card the owner has edited around the markers: everything they wrote survives.
        let owned = format!(
            "---\nlicense: mit\n---\n\n# My memory\n\nMy own intro.\n\n{}\n\nMy own footer.\n",
            stats_block(&ctx(999))
        );
        let CardAction::Refresh(refreshed) = plan(Some(&owned), &ctx(1_500)) else {
            panic!("expected Refresh");
        };
        assert!(refreshed.contains("license: mit"));
        assert!(refreshed.contains("My own intro."));
        assert!(refreshed.contains("My own footer."));
        assert!(refreshed.contains("| Chunks | 1,500 |"));
        assert!(!refreshed.contains("| Chunks | 999 |"));
    }

    #[test]
    fn current_stats_are_left_alone() {
        let card = render(&ctx(42));
        assert_eq!(plan(Some(&card), &ctx(42)), CardAction::LeaveAlone);
    }

    #[test]
    fn a_card_without_markers_is_never_touched() {
        let hand = "---\nlicense: agpl-3.0\n---\n\n# Hand-written card\n";
        assert_eq!(plan(Some(hand), &ctx(7)), CardAction::LeaveAlone);
    }

    #[test]
    fn a_close_marker_before_the_open_is_not_a_region() {
        let broken = format!("{STATS_CLOSE}\nstray\n{STATS_OPEN}\n");
        assert_eq!(plan(Some(&broken), &ctx(7)), CardAction::LeaveAlone);
    }

    #[test]
    fn size_bands_cover_the_edges() {
        assert_eq!(size_category(0), "n<1K");
        assert_eq!(size_category(999), "n<1K");
        assert_eq!(size_category(1_000), "1K<n<10K");
        assert_eq!(size_category(99_999), "10K<n<100K");
        assert_eq!(size_category(100_000), "100K<n<1M");
        assert_eq!(size_category(2_000_000_000_000), "n>1T");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(21_636), "21,636");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
