// Adapted from Voxtype (MIT), copyright 2025 Peter Jackson.
// Original license: ../docs/licenses/voxtype.txt.
/// What happened to a run of words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// Survived the cleanup unchanged.
    Same,
    /// Present before, gone after.
    Deleted,
    /// Not present before; the cleanup put it there.
    Inserted,
}

/// A run of words sharing one fate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub kind: SpanKind,
    pub text: String,
}

fn lcs_table(before: &[&str], after: &[&str]) -> Vec<Vec<usize>> {
    let mut table = vec![vec![0usize; after.len() + 1]; before.len() + 1];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            table[i][j] = if before[i] == after[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    table
}

/// Diff `before` against `after` at word granularity.
pub fn word_diff(before: &str, after: &str) -> Vec<Span> {
    let b: Vec<&str> = before.split_whitespace().collect();
    let a: Vec<&str> = after.split_whitespace().collect();
    let table = lcs_table(&b, &a);

    let mut spans: Vec<Span> = Vec::new();
    let push = |kind: SpanKind, word: &str, spans: &mut Vec<Span>| {
        // Merge into the previous run when it shares a fate, so the rendered
        // output reads as phrases rather than word-by-word tagging.
        match spans.last_mut() {
            Some(last) if last.kind == kind => {
                last.text.push(' ');
                last.text.push_str(word);
            }
            _ => spans.push(Span {
                kind,
                text: word.to_string(),
            }),
        }
    };

    let (mut i, mut j) = (0usize, 0usize);
    while i < b.len() && j < a.len() {
        if b[i] == a[j] {
            push(SpanKind::Same, b[i], &mut spans);
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            push(SpanKind::Deleted, b[i], &mut spans);
            i += 1;
        } else {
            push(SpanKind::Inserted, a[j], &mut spans);
            j += 1;
        }
    }
    while i < b.len() {
        push(SpanKind::Deleted, b[i], &mut spans);
        i += 1;
    }
    while j < a.len() {
        push(SpanKind::Inserted, a[j], &mut spans);
        j += 1;
    }
    spans
}
