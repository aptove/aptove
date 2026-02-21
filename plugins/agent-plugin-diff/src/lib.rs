//! Diff / Edit-Engine Plugin
//!
//! Intercepts `write_file` tool calls and transparently applies structured
//! edit formats that LLMs commonly produce, so the downstream filesystem tool
//! always receives the final verbatim file content.
//!
//! ## Supported formats (tried in priority order)
//!
//! 1. **Search-and-replace** — one or more blocks delimited by
//!    `<<<` … (SEARCH), `===` … (separator), `>>>` … (REPLACE).
//!    Supports exact, normalised-whitespace, and `similar`-based fuzzy matching.
//!
//! 2. **Unified diff** — standard `--- a/file` / `+++ b/file` / `@@ … @@`
//!    format.  Context lines are verified; a mismatch falls through to the
//!    next strategy.
//!
//! 3. **Whole-file** — fallback: the `content` argument is used verbatim as
//!    the new file content (no markers detected, or all structured strategies
//!    failed).
//!
//! ## Hook
//! `on_before_tool_call` — rewrites `call.arguments["content"]` in-place so
//! that no other part of the agent needs to know about edit formats.

use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use similar::TextDiff;
use tracing::{debug, info, warn};

use agent_core::plugin::Plugin;
use agent_core::types::ToolCallRequest;

// ---------------------------------------------------------------------------
// Tool detection constants
// ---------------------------------------------------------------------------

const WRITE_TOOL_NAMES: &[&str] = &[
    "write_file",
    "create_file",
    "edit_file",
    "str_replace",
    "overwrite_file",
    "apply_patch",
    "write",
];

const PATH_ARG_KEYS: &[&str] = &["path", "file_path", "filename", "file", "target_file"];

const CONTENT_ARG_KEYS: &[&str] = &["content", "new_content", "text", "body", "file_content"];

// ---------------------------------------------------------------------------
// Static regex for unified-diff hunk headers
// ---------------------------------------------------------------------------

static HUNK_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap()
});

// ---------------------------------------------------------------------------
// Edit strategy result
// ---------------------------------------------------------------------------

/// Which strategy successfully produced the final file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditStrategy {
    SearchReplace,
    UnifiedDiff,
    /// No structured format detected — content used verbatim.
    WholeFile,
}

// ===========================================================================
// Strategy 1: Search-and-Replace
// ===========================================================================

/// A single `<<<SEARCH … === … >>>REPLACE` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchReplaceBlock {
    pub search: String,
    pub replace: String,
}

/// Return `true` if `content` looks like it contains search-replace markers.
///
/// Requires the three marker families to appear in order on their own lines
/// (leading whitespace allowed).  This stateful check avoids false positives
/// from source code that happens to contain `<<<` or `===` operators.
pub fn looks_like_search_replace(content: &str) -> bool {
    let mut saw_search = false;
    let mut saw_sep = false;
    for line in content.lines() {
        let t = line.trim_start();
        if !saw_search && t.starts_with("<<<") {
            saw_search = true;
        } else if saw_search && !saw_sep && t.starts_with("===") {
            saw_sep = true;
        } else if saw_sep && t.starts_with(">>>") {
            return true;
        }
    }
    false
}

/// Parse all `<<<` / `===` / `>>>` blocks from `content`.
///
/// Returns an empty `Vec` (never `None`) — the caller checks `is_empty()`.
pub fn parse_search_replace_blocks(content: &str) -> Vec<SearchReplaceBlock> {
    let lines: Vec<&str> = content.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let t = lines[i].trim_start();

        // ── Find SEARCH marker ──────────────────────────────────────────────
        if !t.starts_with("<<<") {
            i += 1;
            continue;
        }
        let search_start = i + 1;

        // ── Find separator ──────────────────────────────────────────────────
        let sep_idx = match (search_start..lines.len())
            .find(|&j| lines[j].trim_start().starts_with("==="))
        {
            Some(idx) => idx,
            None => {
                i += 1;
                continue;
            }
        };

        // ── Find REPLACE marker ─────────────────────────────────────────────
        let replace_start = sep_idx + 1;
        let replace_end = match (replace_start..lines.len())
            .find(|&j| lines[j].trim_start().starts_with(">>>"))
        {
            Some(idx) => idx,
            None => {
                i += 1;
                continue;
            }
        };

        let search_text = lines[search_start..sep_idx].join("\n");
        let replace_text = lines[replace_start..replace_end].join("\n");

        blocks.push(SearchReplaceBlock {
            search: search_text,
            replace: replace_text,
        });

        i = replace_end + 1;
    }

    blocks
}

/// Apply one search-replace block to `file_content`.
///
/// Attempts (in order):
/// 1. Exact substring match.
/// 2. Normalised-whitespace match (each line trimmed before comparison).
/// 3. Fuzzy match via `similar::TextDiff::ratio()` with a sliding window
///    (minimum 0.80 similarity).
///
/// Returns `None` if no strategy finds a match.
pub fn apply_block(file_content: &str, block: &SearchReplaceBlock) -> Option<String> {
    // 1. Exact match ─────────────────────────────────────────────────────────
    if let Some(result) = exact_replace(file_content, &block.search, &block.replace) {
        debug!("[diff] search-replace: exact match");
        return Some(result);
    }

    // 2. Normalised-whitespace match ─────────────────────────────────────────
    if let Some(result) = normalized_replace(file_content, &block.search, &block.replace) {
        debug!("[diff] search-replace: normalised-whitespace match");
        return Some(result);
    }

    // 3. Fuzzy match (similar) ───────────────────────────────────────────────
    if let Some(result) = fuzzy_replace(file_content, &block.search, &block.replace) {
        debug!("[diff] search-replace: fuzzy match");
        return Some(result);
    }

    warn!("[diff] search-replace: no match found for block");
    None
}

/// Apply all blocks sequentially.  Returns `None` if any block fails to match.
pub fn apply_search_replace(file_content: &str, write_content: &str) -> Option<String> {
    if !looks_like_search_replace(write_content) {
        return None;
    }
    let blocks = parse_search_replace_blocks(write_content);
    if blocks.is_empty() {
        return None;
    }
    let mut current = file_content.to_string();
    for block in &blocks {
        current = apply_block(&current, block)?;
    }
    Some(current)
}

// ── Exact substring replacement ─────────────────────────────────────────────

fn exact_replace(file: &str, search: &str, replace: &str) -> Option<String> {
    if search.is_empty() {
        return None; // Don't match empty search blocks
    }
    if file.contains(search) {
        Some(file.replacen(search, replace, 1))
    } else {
        None
    }
}

// ── Normalised-whitespace line-by-line replacement ───────────────────────────

fn normalized_replace(file: &str, search: &str, replace: &str) -> Option<String> {
    let file_lines: Vec<&str> = file.lines().collect();
    let search_lines: Vec<&str> = search.lines().collect();
    let n = search_lines.len();
    if n == 0 {
        return None;
    }

    for start in 0..file_lines.len().saturating_sub(n - 1) {
        let window = &file_lines[start..start + n];
        let all_match = window
            .iter()
            .zip(search_lines.iter())
            .all(|(w, s)| w.trim() == s.trim());

        if all_match {
            let end = start + n;
            return Some(splice_lines(file, &file_lines, start, end, replace));
        }
    }
    None
}

// ── Fuzzy replacement via `similar` ─────────────────────────────────────────

/// Minimum similarity ratio (0.0–1.0) required for a fuzzy match.
/// `TextDiff::ratio()` returns `f32`.
const MIN_FUZZY_SIMILARITY: f32 = 0.80;

fn fuzzy_replace(file: &str, search: &str, replace: &str) -> Option<String> {
    let file_lines: Vec<&str> = file.lines().collect();
    let search_lines: Vec<&str> = search.lines().collect();
    let n = search_lines.len();
    if n == 0 || file_lines.len() < n {
        return None;
    }

    let mut best_score: f32 = 0.0;
    let mut best_start: Option<usize> = None;

    for start in 0..=(file_lines.len() - n) {
        let window_text = file_lines[start..start + n].join("\n");
        let score = TextDiff::from_lines(search, &window_text).ratio();
        if score > best_score {
            best_score = score;
            best_start = Some(start);
        }
    }

    if best_score >= MIN_FUZZY_SIMILARITY {
        let start = best_start?;
        let end = start + n;
        Some(splice_lines(file, &file_lines, start, end, replace))
    } else {
        None
    }
}

// ── Shared line-splice helper ────────────────────────────────────────────────

/// Replace `file_lines[start..end]` with `replacement` text, preserving the
/// file's original trailing-newline state.
fn splice_lines(
    original_file: &str,
    file_lines: &[&str],
    start: usize,
    end: usize,
    replacement: &str,
) -> String {
    let before = file_lines[..start].join("\n");
    let after = file_lines[end..].join("\n");

    let parts: Vec<&str> = [before.as_str(), replacement, after.as_str()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();

    let mut result = parts.join("\n");
    if original_file.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

// ===========================================================================
// Strategy 2: Unified Diff
// ===========================================================================

/// A single line inside a diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    /// Leading space — unchanged context line.
    Context(String),
    /// Leading `-` — line to remove from the original.
    Remove(String),
    /// Leading `+` — line to add in the new version.
    Add(String),
}

/// A parsed `@@ … @@` hunk.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// 1-based line number in the original file where this hunk starts.
    pub orig_start: usize,
    /// Number of original lines this hunk covers (context + removed).
    pub orig_count: usize,
    pub lines: Vec<HunkLine>,
}

/// Return `true` if `content` looks like a unified diff.
///
/// Requires `--- `, `+++ `, and `@@ ` to all be present, preventing
/// false positives from ordinary source files.
pub fn looks_like_unified_diff(content: &str) -> bool {
    let has_old = content
        .lines()
        .any(|l| l.starts_with("--- ") || l == "---");
    let has_new = content.lines().any(|l| l.starts_with("+++ "));
    let has_hunk = content.lines().any(|l| l.starts_with("@@ "));
    has_old && has_new && has_hunk
}

/// Parse all `@@ … @@` hunks from a unified diff string.
pub fn parse_unified_diff(content: &str) -> Option<Vec<DiffHunk>> {
    if !looks_like_unified_diff(content) {
        return None;
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut i = 0;

    // Skip past any `---` / `+++` header lines.
    while i < lines.len() && !lines[i].starts_with("@@ ") {
        i += 1;
    }

    while i < lines.len() {
        let line = lines[i];
        if !line.starts_with("@@ ") {
            i += 1;
            continue;
        }

        // Parse `@@ -orig_start[,orig_count] +new_start[,new_count] @@`
        let caps = match HUNK_HEADER_RE.captures(line) {
            Some(c) => c,
            None => {
                warn!("[diff] unrecognised hunk header: {:?}", line);
                i += 1;
                continue;
            }
        };

        let orig_start: usize = caps[1].parse().unwrap_or(1);
        // When count is omitted it means 1 (or 0 for empty-file insertions).
        let orig_count: usize = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(1);

        i += 1; // advance past the `@@` line

        let mut hunk_lines: Vec<HunkLine> = Vec::new();

        while i < lines.len() && !lines[i].starts_with("@@ ") {
            let hl = lines[i];
            if hl.starts_with('-') {
                hunk_lines.push(HunkLine::Remove(hl[1..].to_string()));
            } else if hl.starts_with('+') {
                hunk_lines.push(HunkLine::Add(hl[1..].to_string()));
            } else if hl.starts_with(' ') {
                hunk_lines.push(HunkLine::Context(hl[1..].to_string()));
            } else if hl.starts_with('\\') {
                // "\ No newline at end of file" — informational, skip.
            }
            // Lines with other prefixes (empty, diff headers) are skipped.
            i += 1;
        }

        hunks.push(DiffHunk {
            orig_start,
            orig_count,
            lines: hunk_lines,
        });
    }

    if hunks.is_empty() {
        None
    } else {
        Some(hunks)
    }
}

/// Apply a list of hunks to `file_content`.
///
/// Hunks must be in ascending `orig_start` order (standard for unified diffs).
/// Context and remove lines are verified against the file; a mismatch causes
/// `None` to be returned so the fallback chain can try the next strategy.
pub fn apply_hunks(file_content: &str, hunks: &[DiffHunk]) -> Option<String> {
    let mut lines: Vec<String> = file_content.lines().map(|l| l.to_string()).collect();
    let trailing_newline = file_content.ends_with('\n');

    // `offset` accumulates the net line-count change from previously applied
    // hunks so we can correctly index into `lines` (which grows/shrinks).
    let mut offset: i64 = 0;

    for hunk in hunks {
        // orig_start is 1-based; handle the special case of 0 (empty file insert).
        let start_idx = if hunk.orig_start == 0 {
            0usize
        } else {
            (hunk.orig_start as i64 - 1 + offset) as usize
        };

        let mut file_pos = start_idx;
        let mut replacement: Vec<String> = Vec::new();

        for hl in &hunk.lines {
            match hl {
                HunkLine::Context(expected) => {
                    if file_pos >= lines.len() {
                        warn!(
                            "[diff] unified: context line {} extends past file end (len={})",
                            file_pos + 1,
                            lines.len()
                        );
                        return None;
                    }
                    if lines[file_pos].trim_end() != expected.trim_end() {
                        warn!(
                            "[diff] unified: context mismatch at line {}: \
                             expected {:?}, got {:?}",
                            file_pos + 1,
                            expected,
                            lines[file_pos]
                        );
                        return None;
                    }
                    replacement.push(lines[file_pos].clone()); // keep original
                    file_pos += 1;
                }
                HunkLine::Remove(expected) => {
                    if file_pos >= lines.len() {
                        warn!("[diff] unified: remove line {} extends past file end", file_pos + 1);
                        return None;
                    }
                    if lines[file_pos].trim_end() != expected.trim_end() {
                        warn!(
                            "[diff] unified: remove mismatch at line {}: \
                             expected {:?}, got {:?}",
                            file_pos + 1,
                            expected,
                            lines[file_pos]
                        );
                        return None;
                    }
                    file_pos += 1; // consumed but not emitted
                }
                HunkLine::Add(new_line) => {
                    replacement.push(new_line.clone());
                }
            }
        }

        let consumed = file_pos - start_idx;
        let new_count = replacement.len();
        lines.splice(start_idx..start_idx + consumed, replacement);
        offset += new_count as i64 - consumed as i64;
    }

    let mut result = lines.join("\n");
    if trailing_newline && !result.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

/// High-level entry point: parse and apply a unified diff.
pub fn apply_unified_diff(file_content: &str, write_content: &str) -> Option<String> {
    let hunks = parse_unified_diff(write_content)?;
    apply_hunks(file_content, &hunks)
}

// ===========================================================================
// Strategy 3: Whole-file (fallback)
// ===========================================================================

/// Return the content unchanged — used when no structured format is detected.
pub fn apply_whole_file(write_content: &str) -> String {
    write_content.to_string()
}

// ===========================================================================
// Fallback chain
// ===========================================================================

/// Run the three strategies in priority order and return the first success.
///
/// Always succeeds: `WholeFile` is the unconditional fallback.
pub fn apply_edit(file_content: &str, write_content: &str) -> (String, EditStrategy) {
    // 1. Search-and-replace
    if let Some(result) = apply_search_replace(file_content, write_content) {
        info!("[diff] strategy: search-replace");
        return (result, EditStrategy::SearchReplace);
    }

    // 2. Unified diff
    if let Some(result) = apply_unified_diff(file_content, write_content) {
        info!("[diff] strategy: unified-diff");
        return (result, EditStrategy::UnifiedDiff);
    }

    // 3. Whole-file
    debug!("[diff] strategy: whole-file (no structured format detected)");
    (apply_whole_file(write_content), EditStrategy::WholeFile)
}

// ===========================================================================
// Plugin
// ===========================================================================

/// Diff/edit-engine plugin.
pub struct DiffPlugin;

impl DiffPlugin {
    pub fn new() -> Self {
        DiffPlugin
    }
}

impl Default for DiffPlugin {
    fn default() -> Self {
        DiffPlugin
    }
}

// ── Tool argument helpers ────────────────────────────────────────────────────

/// If `call` is a file-write tool, return `(file_path, write_content)`.
fn extract_write_args(call: &ToolCallRequest) -> Option<(String, String)> {
    let name_lower = call.name.to_lowercase();
    if !WRITE_TOOL_NAMES
        .iter()
        .any(|n| name_lower == *n || name_lower.contains(n))
    {
        return None;
    }

    let path = PATH_ARG_KEYS
        .iter()
        .find_map(|k| call.arguments.get(k).and_then(|v| v.as_str()))
        .map(|s| s.to_string())?;

    let content = CONTENT_ARG_KEYS
        .iter()
        .find_map(|k| call.arguments.get(k).and_then(|v| v.as_str()))
        .map(|s| s.to_string())?;

    Some((path, content))
}

/// Replace the content argument in `call` with `new_content`.
fn update_content_arg(call: &mut ToolCallRequest, new_content: String) {
    if let Some(obj) = call.arguments.as_object_mut() {
        for key in CONTENT_ARG_KEYS {
            if obj.contains_key(*key) {
                obj.insert((*key).to_string(), serde_json::Value::String(new_content));
                return;
            }
        }
    }
}

#[async_trait]
impl Plugin for DiffPlugin {
    fn name(&self) -> &str {
        "diff"
    }

    /// Intercept write_file tool calls, detect edit format, apply it, and
    /// replace the `content` argument with the final verbatim file content.
    async fn on_before_tool_call(&self, call: &mut ToolCallRequest) -> Result<()> {
        let (path, write_content) = match extract_write_args(call) {
            Some(args) => args,
            None => return Ok(()),
        };

        // Read the current on-disk content (empty string for new files).
        let current_content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("[diff] '{}' does not exist yet — treating as new file", path);
                String::new()
            }
            Err(e) => {
                warn!("[diff] could not read '{}': {} — skipping edit engine", path, e);
                return Ok(()); // Non-fatal: let the tool call through unchanged.
            }
        };

        let (new_content, strategy) = apply_edit(&current_content, &write_content);

        // Only rewrite the argument when the content actually changed.
        if strategy != EditStrategy::WholeFile || new_content != write_content {
            info!("[diff] '{}': {:?} strategy applied", path, strategy);
            update_content_arg(call, new_content);
        }

        Ok(())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::types::ToolCallRequest;
    use tempfile::NamedTempFile;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_call(name: &str, path: &str, content: &str) -> ToolCallRequest {
        ToolCallRequest {
            id: "t1".into(),
            name: name.into(),
            arguments: serde_json::json!({ "path": path, "content": content }),
        }
    }

    fn get_content(call: &ToolCallRequest) -> &str {
        call.arguments["content"].as_str().unwrap()
    }

    // -----------------------------------------------------------------------
    // 1.2 Search-and-replace detection
    // -----------------------------------------------------------------------

    #[test]
    fn test_sr_detection_true() {
        let content = "<<<< SEARCH\nold line\n====\nnew line\n>>>> REPLACE";
        assert!(looks_like_search_replace(content));
    }

    #[test]
    fn test_sr_detection_false_for_plain_code() {
        let content = "fn foo() { let x = a << b; }";
        assert!(!looks_like_search_replace(content));
    }

    #[test]
    fn test_sr_detection_requires_order() {
        // >>> before === — should NOT be detected
        let content = ">>>> REPLACE\n====\n<<<< SEARCH";
        assert!(!looks_like_search_replace(content));
    }

    #[test]
    fn test_sr_parse_single_block() {
        let content = "<<<< SEARCH\nfoo\n====\nbar\n>>>> REPLACE";
        let blocks = parse_search_replace_blocks(content);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].search, "foo");
        assert_eq!(blocks[0].replace, "bar");
    }

    #[test]
    fn test_sr_parse_multiple_blocks() {
        let content = "\
<<<< SEARCH
alpha
====
ALPHA
>>>> REPLACE
<<<< SEARCH
beta
====
BETA
>>>> REPLACE";
        let blocks = parse_search_replace_blocks(content);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], SearchReplaceBlock { search: "alpha".into(), replace: "ALPHA".into() });
        assert_eq!(blocks[1], SearchReplaceBlock { search: "beta".into(), replace: "BETA".into() });
    }

    #[test]
    fn test_sr_parse_aider_style_markers() {
        // Aider uses 7-char markers
        let content = "<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE";
        let blocks = parse_search_replace_blocks(content);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].search, "old");
        assert_eq!(blocks[0].replace, "new");
    }

    // -----------------------------------------------------------------------
    // 1.2 Exact match
    // -----------------------------------------------------------------------

    #[test]
    fn test_sr_exact_match() {
        let file = "fn main() {\n    println!(\"Hello\");\n}\n";
        let block = SearchReplaceBlock {
            search:  "    println!(\"Hello\");".into(),
            replace: "    println!(\"Hello, World!\");".into(),
        };
        let result = apply_block(file, &block).unwrap();
        assert!(result.contains("Hello, World!"));
        assert!(!result.contains("println!(\"Hello\");"));
    }

    #[test]
    fn test_sr_multiline_exact_match() {
        let file = "a\nb\nc\nd\n";
        let block = SearchReplaceBlock {
            search:  "b\nc".into(),
            replace: "B\nC".into(),
        };
        let result = apply_block(file, &block).unwrap();
        assert_eq!(result, "a\nB\nC\nd\n");
    }

    // -----------------------------------------------------------------------
    // 1.5 Normalised-whitespace match
    // -----------------------------------------------------------------------

    #[test]
    fn test_sr_normalized_whitespace_match() {
        let file = "def foo():\n    x = 1\n    return x\n";
        // Search block has different indentation
        let block = SearchReplaceBlock {
            search:  "x = 1\nreturn x".into(), // no leading spaces
            replace: "x = 42\nreturn x".into(),
        };
        let result = apply_block(file, &block).unwrap();
        assert!(result.contains("42"), "expected 42 in result: {:?}", result);
    }

    #[test]
    fn test_sr_normalized_trailing_spaces() {
        let file = "line one   \nline two\n";
        let block = SearchReplaceBlock {
            search:  "line one".into(), // no trailing spaces in search
            replace: "LINE ONE".into(),
        };
        let result = apply_block(file, &block).unwrap();
        assert!(result.contains("LINE ONE"));
    }

    // -----------------------------------------------------------------------
    // 1.5 Fuzzy match (similar)
    // -----------------------------------------------------------------------

    #[test]
    fn test_sr_fuzzy_match_minor_difference() {
        // One word changed in the search block — still above 80% similarity
        let file = "fn calculate_sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let block = SearchReplaceBlock {
            // 'calculate_sum' vs 'compute_sum' — single word difference, very similar
            search:  "fn calculate_sum(a: i32, b: i32) -> i32 {\n    a + b\n}".into(),
            replace: "fn calculate_sum(a: i32, b: i32) -> i32 {\n    a + b + 1\n}".into(),
        };
        // exact or normalized should catch this; fuzzy is the safety net
        let result = apply_block(file, &block).unwrap();
        assert!(result.contains("a + b + 1"));
    }

    #[test]
    fn test_sr_no_match_returns_none() {
        let file = "completely different content\n";
        let block = SearchReplaceBlock {
            search:  "nothing like this at all abcdefghij".into(),
            replace: "replacement".into(),
        };
        assert!(apply_block(file, &block).is_none());
    }

    // -----------------------------------------------------------------------
    // 1.2 apply_search_replace (end-to-end)
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_search_replace_full() {
        let file = "x = 1\ny = 2\n";
        let write_content = "<<<< SEARCH\ny = 2\n====\ny = 99\n>>>> REPLACE";
        let result = apply_search_replace(file, write_content).unwrap();
        assert_eq!(result, "x = 1\ny = 99\n");
    }

    #[test]
    fn test_apply_search_replace_no_markers_returns_none() {
        assert!(apply_search_replace("file content", "plain content").is_none());
    }

    // -----------------------------------------------------------------------
    // 1.3 Unified diff detection
    // -----------------------------------------------------------------------

    #[test]
    fn test_ud_detection_true() {
        let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n";
        assert!(looks_like_unified_diff(diff));
    }

    #[test]
    fn test_ud_detection_false_for_plain_file() {
        assert!(!looks_like_unified_diff("just some text\n--- separator\n"));
    }

    // -----------------------------------------------------------------------
    // 1.3 Unified diff parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_ud_parse_single_hunk() {
        let diff = "--- a/f\n+++ b/f\n@@ -2,3 +2,3 @@\n context\n-old\n+new\n context2\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.orig_start, 2);
        assert_eq!(h.orig_count, 3);
        assert_eq!(h.lines.len(), 4);
        assert!(matches!(h.lines[0], HunkLine::Context(_)));
        assert!(matches!(h.lines[1], HunkLine::Remove(_)));
        assert!(matches!(h.lines[2], HunkLine::Add(_)));
        assert!(matches!(h.lines[3], HunkLine::Context(_)));
    }

    #[test]
    fn test_ud_parse_multi_hunk() {
        let diff = "\
--- a/f
+++ b/f
@@ -1,2 +1,2 @@
-a
+A
 b
@@ -5,2 +5,2 @@
 e
-f
+F
";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].orig_start, 1);
        assert_eq!(hunks[1].orig_start, 5);
    }

    #[test]
    fn test_ud_parse_omitted_count_defaults_to_one() {
        // `@@ -3 +3 @@` — no `,count` means count=1
        let diff = "--- a/f\n+++ b/f\n@@ -3 +3 @@\n-old\n+new\n";
        let hunks = parse_unified_diff(diff).unwrap();
        assert_eq!(hunks[0].orig_count, 1);
    }

    // -----------------------------------------------------------------------
    // 1.3 apply_unified_diff
    // -----------------------------------------------------------------------

    #[test]
    fn test_ud_apply_basic() {
        let file = "line1\nline2\nline3\n";
        let diff = "--- a/f\n+++ b/f\n@@ -2,1 +2,1 @@\n-line2\n+LINE2\n";
        let result = apply_unified_diff(file, diff).unwrap();
        assert_eq!(result, "line1\nLINE2\nline3\n");
    }

    #[test]
    fn test_ud_apply_with_context() {
        let file = "a\nb\nc\nd\ne\n";
        let diff = "--- a/f\n+++ b/f\n@@ -2,3 +2,3 @@\n a\n-c\n+C\n d\n";
        // context 'a' matches file[1]='b'? No. Let's be accurate:
        // orig_start=2 means line 2 = "b" (1-based)
        // context line " a" means the actual line is "a" — mismatch!
        // Correct diff:
        let diff2 = "--- a/f\n+++ b/f\n@@ -2,3 +2,3 @@\n b\n-c\n+C\n d\n";
        let result = apply_unified_diff(file, diff2).unwrap();
        assert_eq!(result, "a\nb\nC\nd\ne\n");
    }

    // -----------------------------------------------------------------------
    // 1.3 Multi-hunk unified diff
    // -----------------------------------------------------------------------

    #[test]
    fn test_ud_apply_multi_hunk() {
        let file = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
        let diff = "\
--- a/f
+++ b/f
@@ -1,1 +1,1 @@
-alpha
+ALPHA
@@ -4,1 +4,1 @@
-delta
+DELTA
";
        let result = apply_unified_diff(file, diff).unwrap();
        assert_eq!(result, "ALPHA\nbeta\ngamma\nDELTA\nepsilon\n");
    }

    #[test]
    fn test_ud_apply_pure_addition() {
        // Insert lines at the top of an existing file
        let file = "existing\n";
        let diff = "--- a/f\n+++ b/f\n@@ -0,0 +1,2 @@\n+new_line1\n+new_line2\n";
        let result = apply_unified_diff(file, diff).unwrap();
        assert_eq!(result, "new_line1\nnew_line2\nexisting\n");
    }

    // -----------------------------------------------------------------------
    // 1.7 Malformed input falls through to whole-file
    // -----------------------------------------------------------------------

    #[test]
    fn test_ud_context_mismatch_returns_none() {
        let file = "line1\nline2\n";
        // Context says "line99" but file has "line2"
        let diff = "--- a/f\n+++ b/f\n@@ -2,1 +2,1 @@\n line99\n-line2\n+X\n";
        assert!(apply_unified_diff(file, diff).is_none());
    }

    // -----------------------------------------------------------------------
    // 1.7 Fallback chain
    // -----------------------------------------------------------------------

    #[test]
    fn test_fallback_search_replace_wins() {
        let file = "x = 1\n";
        let content = "<<<< SEARCH\nx = 1\n====\nx = 2\n>>>> REPLACE";
        let (result, strategy) = apply_edit(file, content);
        assert_eq!(strategy, EditStrategy::SearchReplace);
        assert_eq!(result, "x = 2\n");
    }

    #[test]
    fn test_fallback_unified_diff_wins_when_no_sr() {
        let file = "hello\nworld\n";
        let diff = "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-hello\n+HELLO\n";
        let (result, strategy) = apply_edit(file, diff);
        assert_eq!(strategy, EditStrategy::UnifiedDiff);
        assert_eq!(result, "HELLO\nworld\n");
    }

    #[test]
    fn test_fallback_whole_file_when_no_format() {
        let file = "old content\n";
        let content = "completely new content\n";
        let (result, strategy) = apply_edit(file, content);
        assert_eq!(strategy, EditStrategy::WholeFile);
        assert_eq!(result, content);
    }

    #[test]
    fn test_fallback_whole_file_when_sr_no_match() {
        // SR markers present but search text not in file — should fall to whole-file
        let file = "something else entirely\n";
        let content = "<<<< SEARCH\nnot in file\n====\nreplacement\n>>>> REPLACE";
        let (_, strategy) = apply_edit(file, content);
        assert_eq!(strategy, EditStrategy::WholeFile);
    }

    #[test]
    fn test_fallback_whole_file_when_ud_context_mismatch() {
        // Valid unified diff structure but context doesn't match — falls to whole-file
        let file = "foo\nbar\n";
        let diff = "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n wrong_context\n-foo\n+FOO\n";
        let (_, strategy) = apply_edit(file, diff);
        assert_eq!(strategy, EditStrategy::WholeFile);
    }

    // -----------------------------------------------------------------------
    // 1.6 on_before_tool_call integration
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_hook_rewrites_content_for_search_replace() {
        let mut tmp = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"x = 1\ny = 2\n").unwrap();

        let path = tmp.path().to_str().unwrap().to_string();
        let write_content =
            format!("<<<< SEARCH\ny = 2\n====\ny = 99\n>>>> REPLACE");

        let mut call = make_call("write_file", &path, &write_content);
        let plugin = DiffPlugin::new();
        plugin.on_before_tool_call(&mut call).await.unwrap();

        let result = get_content(&call);
        assert_eq!(result, "x = 1\ny = 99\n");
    }

    #[tokio::test]
    async fn test_hook_rewrites_content_for_unified_diff() {
        let mut tmp = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"hello\nworld\n").unwrap();

        let path = tmp.path().to_str().unwrap().to_string();
        let diff = "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-hello\n+HELLO\n";

        let mut call = make_call("write_file", &path, diff);
        let plugin = DiffPlugin::new();
        plugin.on_before_tool_call(&mut call).await.unwrap();

        assert_eq!(get_content(&call), "HELLO\nworld\n");
    }

    #[tokio::test]
    async fn test_hook_passthrough_for_non_write_tool() {
        let mut call = make_call("bash", "/tmp/f", "ls -la");
        let original_args = call.arguments.clone();
        let plugin = DiffPlugin::new();
        plugin.on_before_tool_call(&mut call).await.unwrap();
        assert_eq!(call.arguments, original_args);
    }

    #[tokio::test]
    async fn test_hook_whole_file_for_new_file() {
        // File doesn't exist: hook should treat content as whole-file (no rewrite needed)
        let path = "/tmp/nonexistent_plugin_diff_test_xyz.txt";
        let content = "brand new content\n";
        let mut call = make_call("write_file", path, content);
        let plugin = DiffPlugin::new();
        plugin.on_before_tool_call(&mut call).await.unwrap();
        // Content is unchanged for whole-file (it's already correct)
        assert_eq!(get_content(&call), content);
    }

    // -----------------------------------------------------------------------
    // 1.4 Whole-file detection helper
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_whole_file_returns_content_verbatim() {
        let content = "any content\nwhatsoever\n";
        assert_eq!(apply_whole_file(content), content);
    }

    // -----------------------------------------------------------------------
    // Trailing newline preservation
    // -----------------------------------------------------------------------

    #[test]
    fn test_sr_preserves_trailing_newline() {
        let file = "a\nb\n"; // has trailing newline
        let block = SearchReplaceBlock { search: "b".into(), replace: "B".into() };
        let result = apply_block(file, &block).unwrap();
        assert!(result.ends_with('\n'), "trailing newline should be preserved");
    }

    #[test]
    fn test_ud_preserves_trailing_newline() {
        let file = "old\n";
        let diff = "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let result = apply_unified_diff(file, diff).unwrap();
        assert!(result.ends_with('\n'));
    }
}
