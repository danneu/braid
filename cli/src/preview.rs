//! Shared preview model for `--dry-run` rendering.
//!
//! Successful `--dry-run` prints exactly one rendered `Preview` to
//! stdout. Anything that affects how a user interprets the structured
//! preview is a `PreviewNote`, not stray stderr. Long-running
//! side-effect-free probes may still emit status rows to stderr; see
//! `docs/decisions/022-dry-run-preview-model.md`.
//!
//! PR 0 lands the types and rendering primitives only -- no command
//! migrations.

use serde::Serialize;

use crate::cmd::Step;
use crate::status_tag::{StatusTag, color_enabled_for_stdout, status_line};

/// One renderable dry-run preview. `notes` and `steps` render in the
/// fixed order documented on `Preview::render`. `Step` is not yet
/// `Serialize`, so the field is skipped from the JSON shape; future
/// `--format json` work either derives `Serialize` on `Step` (and the
/// `CmdRequest` cascade) or projects steps separately.
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub completeness: PreviewCompleteness,
    pub notes: Vec<PreviewNote>,
    #[serde(skip)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PreviewCompleteness {
    Complete,
    Partial { reasons: Vec<PreviewGap> },
}

/// Reasons a preview is incomplete. Empty in PR 0; the first variant
/// lands alongside the first migration that needs to surface
/// incompleteness. Keep the `tag` / `content` discipline so later
/// variants are non-breaking JSON.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "reason", content = "detail", rename_all = "kebab-case")]
pub enum PreviewGap {}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PreviewNote {
    Info(String),
    Warn(String),
    PerDisk {
        name: String,
        level: NoteLevel,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoteLevel {
    Ok,
    Skip,
    Warn,
    Error,
}

/// Per-disk line shape. `Bracketed` matches the event-log shape
/// (`[ok]   disk <name>: <msg>`);
/// `Plain` matches today's `enroll` discovery lines
/// (`<tag>: <name> <msg>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerDiskStyle {
    Bracketed,
    Plain,
}

impl NoteLevel {
    fn to_status_tag(self) -> StatusTag {
        match self {
            NoteLevel::Ok => StatusTag::Ok,
            NoteLevel::Skip => StatusTag::Skip,
            NoteLevel::Warn => StatusTag::Warn,
            NoteLevel::Error => StatusTag::Fail,
        }
    }

    fn plain_label(self) -> &'static str {
        match self {
            NoteLevel::Ok => "ok",
            NoteLevel::Skip => "skip",
            NoteLevel::Warn => "warn",
            NoteLevel::Error => "error",
        }
    }
}

fn format_per_disk_line(
    name: &str,
    level: NoteLevel,
    message: &str,
    style: PerDiskStyle,
    color_enabled: bool,
) -> String {
    match style {
        PerDiskStyle::Bracketed => status_line(
            level.to_status_tag(),
            color_enabled,
            &format!("disk {name}: {message}"),
        ),
        PerDiskStyle::Plain => {
            format!("{}: {} {}\n", level.plain_label(), name, message)
        }
    }
}

/// Render only the `PerDisk` notes from `notes` in insertion order
/// using the given style. Non-`PerDisk` notes are skipped.
pub fn render_per_disk_notes(notes: &[PreviewNote], style: PerDiskStyle) -> String {
    render_per_disk_notes_with(notes, style, false)
}

pub fn render_per_disk_notes_with(
    notes: &[PreviewNote],
    style: PerDiskStyle,
    color_enabled: bool,
) -> String {
    let mut out = String::new();
    for note in notes {
        if let PreviewNote::PerDisk {
            name,
            level,
            message,
        } = note
        {
            out.push_str(&format_per_disk_line(
                name,
                *level,
                message,
                style,
                color_enabled,
            ));
        }
    }
    out
}

/// Render every note in `notes` for stderr (failure-path or real-run
/// prelude). `Info` renders unadorned, `Warn` renders as
/// `[warn] <body>` with a 7-column visible prefix, and `PerDisk` uses
/// the given style. Mirrors `Preview::render`'s notes section but lets
/// callers pick the per-disk style (e.g. `enroll` real-run uses
/// `Plain` to preserve today's `skip: <name> ...` wording).
pub fn render_notes_for_stderr(notes: &[PreviewNote], style: PerDiskStyle) -> String {
    render_notes_for_stderr_with(notes, style, false)
}

pub fn render_notes_for_stderr_with(
    notes: &[PreviewNote],
    style: PerDiskStyle,
    color_enabled: bool,
) -> String {
    let mut out = String::new();
    for note in notes {
        match note {
            PreviewNote::Info(msg) => {
                out.push_str(msg);
                out.push('\n');
            }
            PreviewNote::Warn(msg) => {
                out.push_str(&status_line(StatusTag::Warn, color_enabled, msg));
            }
            PreviewNote::PerDisk {
                name,
                level,
                message,
            } => {
                out.push_str(&format_per_disk_line(
                    name,
                    *level,
                    message,
                    style,
                    color_enabled,
                ));
            }
        }
    }
    out
}

impl Preview {
    /// Render the preview to a single string in the canonical order:
    ///
    /// 1. `notes` in insertion order. `PerDisk` always renders in
    ///    `Bracketed` style; `Info` renders unadorned; `Warn` renders
    ///    as `[warn] <body>` with a 7-column visible prefix.
    /// 2. Steps via `Step::render_dry_run`. If `steps` is empty *and*
    ///    no `Info` note is present, the literal `nothing to do.\n`
    ///    is emitted (preserves `lock`'s today contract).
    /// 3. If `completeness == Partial`, one
    ///    `note: preview incomplete -- <reason>` line per
    ///    `PreviewGap`. Empty in PR 0.
    pub fn render(&self) -> String {
        self.render_with(false)
    }

    pub fn render_with(&self, color_enabled: bool) -> String {
        let mut out = String::new();

        for note in &self.notes {
            match note {
                PreviewNote::Info(msg) => {
                    out.push_str(msg);
                    out.push('\n');
                }
                PreviewNote::Warn(msg) => {
                    out.push_str(&status_line(StatusTag::Warn, color_enabled, msg));
                }
                PreviewNote::PerDisk {
                    name,
                    level,
                    message,
                } => {
                    out.push_str(&format_per_disk_line(
                        name,
                        *level,
                        message,
                        PerDiskStyle::Bracketed,
                        color_enabled,
                    ));
                }
            }
        }

        if self.steps.is_empty() {
            let has_info_noop = self.notes.iter().any(|n| matches!(n, PreviewNote::Info(_)));
            if !has_info_noop {
                out.push_str("nothing to do.\n");
            }
        } else {
            out.push_str(&Step::render_dry_run(&self.steps));
        }

        if let PreviewCompleteness::Partial { reasons } = &self.completeness {
            for _reason in reasons {
                // PreviewGap is uninhabited in PR 0; this body is
                // unreachable today. The first variant adds:
                //   out.push_str(&format!(
                //       "note: preview incomplete -- {}\n",
                //       reason.label(),
                //   ));
            }
        }

        out
    }

    pub fn print(&self) {
        print!("{}", self.render());
    }

    pub fn print_colored(&self) {
        print!("{}", self.render_with(color_enabled_for_stdout()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, Step};

    fn sample_step(description: &str) -> Step {
        Step {
            risk: "safe",
            description: description.to_owned(),
            commands: vec![CmdRequest::BtrfsDeviceScanAll],
        }
    }

    /* Intent: the canonical render order is notes-first, then steps.
     * Why it exists: per-command migrations rely on Preview::render
     * placing every accumulated note above the dry-run step block, the
     * shape `lock`'s legacy `render_lock_dry_run` already produces.
     * Scenario: a Warn note plus one Step; the rendered string must
     * begin with the [warn] line and only then emit the step row.
     */
    #[test]
    fn render_emits_notes_before_steps() {
        let preview = Preview {
            completeness: PreviewCompleteness::Complete,
            notes: vec![PreviewNote::Warn("scan failed".into())],
            steps: vec![sample_step("btrfs device scan")],
        };
        let rendered = preview.render();
        let expected = "\
[warn] scan failed
[safe       ] btrfs device scan
               $ btrfs device scan
";
        assert_eq!(rendered, expected);
    }

    /* Intent: color-aware rendering only wraps status tags.
     * Why it exists: dry-run stdout can opt into color for TTYs, but
     * step rendering and note bodies must stay byte-identical after
     * ANSI stripping so the dry-run contract remains stable.
     * Scenario: a Warn note plus one Step rendered through the colored
     * companion.
     */
    #[test]
    fn render_with_colors_only_warn_tag_before_steps() {
        let preview = Preview {
            completeness: PreviewCompleteness::Complete,
            notes: vec![PreviewNote::Warn("scan failed".into())],
            steps: vec![sample_step("btrfs device scan")],
        };
        let rendered = preview.render_with(true);
        let expected = "\
\x1b[33m[warn]\x1b[0m scan failed
[safe       ] btrfs device scan
               $ btrfs device scan
";
        assert_eq!(rendered, expected);
    }

    /* Intent: with zero steps and no Info note, the renderer falls back
     * to the literal `nothing to do.\n` line.
     * Why it exists: lock's current dry-run prints `nothing to do.`
     * when there is nothing to lock; the new shared Preview must
     * preserve that exact byte sequence for byte-identical migration.
     * Scenario: an empty Preview (no notes, no steps, Complete).
     */
    #[test]
    fn render_emits_nothing_to_do_when_empty() {
        let preview = Preview {
            completeness: PreviewCompleteness::Complete,
            notes: vec![],
            steps: vec![],
        };
        assert_eq!(preview.render(), "nothing to do.\n");
    }

    /* Intent: an Info note suppresses the `nothing to do.` fallback,
     * because the Info note itself is the no-op signal.
     * Why it exists: commands like `add` (already-in-pool) and `unlock`
     * (already-mounted) want to surface a specific Info note instead of
     * the generic fallback. Without this rule the renderer would emit
     * BOTH the Info note and `nothing to do.`, double-stating the
     * outcome.
     * Scenario: zero steps, one Info note.
     */
    #[test]
    fn render_info_note_suppresses_nothing_to_do() {
        let preview = Preview {
            completeness: PreviewCompleteness::Complete,
            notes: vec![PreviewNote::Info("nothing to do -- already in pool".into())],
            steps: vec![],
        };
        assert_eq!(
            preview.render(),
            "nothing to do -- already in pool\n",
            "Info note must replace the generic fallback, not stack with it",
        );
    }

    /* Intent: a Warn or PerDisk note alone does NOT suppress the
     * `nothing to do.` fallback -- only an Info note does.
     * Why it exists: a Warn that surfaces a soft failure (e.g. orphan
     * scan failed) does not itself say "the pool has no work"; the
     * fallback line is what tells the user the dry-run still planned
     * zero steps. Same for PerDisk skip notes that arise during probe.
     * Scenario: a Warn note plus zero steps.
     */
    #[test]
    fn render_warn_note_does_not_suppress_nothing_to_do() {
        let preview = Preview {
            completeness: PreviewCompleteness::Complete,
            notes: vec![PreviewNote::Warn("orphan scan failed".into())],
            steps: vec![],
        };
        assert_eq!(
            preview.render(),
            "[warn] orphan scan failed\nnothing to do.\n",
        );
    }

    /* Intent: PerDisk notes inside Preview::render always emit in the
     * Bracketed style regardless of which command produced the
     * Preview.
     * Why it exists: Preview::render is the canonical dry-run
     * stdout shape; mixing per-disk styles between commands would
     * break dry-run wording uniformity. Real-run stderr can pick a
     * different style via render_notes_for_stderr.
     * Scenario: one PerDisk(Skip) note rendered through Preview::render.
     */
    #[test]
    fn render_per_disk_note_uses_bracketed_style() {
        let preview = Preview {
            completeness: PreviewCompleteness::Complete,
            notes: vec![PreviewNote::PerDisk {
                name: "disk1".into(),
                level: NoteLevel::Skip,
                message: "not present".into(),
            }],
            steps: vec![sample_step("scan")],
        };
        let rendered = preview.render();
        assert!(
            rendered.starts_with("[skip] disk disk1: not present\n"),
            "unexpected rendering: {rendered:?}",
        );
    }

    /* Intent: long disk names keep a visible delimiter before the action.
     * Why it exists: the old fixed-width name column let validated disk
     * names longer than the column run directly into the message text.
     * Scenario: a bracketed per-disk line for a long but valid disk name.
     */
    #[test]
    fn format_per_disk_line_long_name_keeps_action_separated() {
        let long_name = "diskname-with-30-character-id";
        let notes = vec![PreviewNote::PerDisk {
            name: long_name.into(),
            level: NoteLevel::Ok,
            message: "locked".into(),
        }];

        let rendered = render_per_disk_notes(&notes, PerDiskStyle::Bracketed);

        assert!(
            rendered.contains(&format!("disk {long_name}: locked")),
            "long-name row must keep a colon+space delimiter, got: {rendered:?}",
        );
    }

    /* Intent: Partial completeness with zero PreviewGap reasons
     * renders identically to Complete -- the footer loop has nothing
     * to iterate.
     * Why it exists: PR 0 ships an empty PreviewGap enum; the Partial
     * branch must still type-check and remain a no-op until the first
     * variant lands. This test catches accidental footer emission
     * (e.g. a stray "preview incomplete" header) before there are any
     * gap reasons to justify it.
     * Scenario: Partial with reasons: vec![] vs Complete with the same
     * notes/steps -- both renderings must be byte-identical.
     */
    #[test]
    fn render_partial_with_no_reasons_matches_complete() {
        let make = |c| Preview {
            completeness: c,
            notes: vec![PreviewNote::Info("entry".into())],
            steps: vec![sample_step("scan")],
        };
        let complete = make(PreviewCompleteness::Complete);
        let partial = make(PreviewCompleteness::Partial { reasons: vec![] });
        assert_eq!(complete.render(), partial.render());
    }

    /* Intent: render_per_disk_notes filters out non-PerDisk notes and
     * renders only the per-disk lines in the chosen style.
     * Why it exists: real-run stderr paths (e.g. enroll's pre-passphrase
     * skip lines) only want the per-disk slice from a notes vec that
     * may also carry Info/Warn entries; the helper must drop the
     * non-PerDisk notes silently.
     * Scenario: a vec of one Info, one Warn, two PerDisks; render in
     * Plain style and assert only the two per-disk lines come back.
     */
    #[test]
    fn render_per_disk_notes_filters_and_uses_chosen_style() {
        let notes = vec![
            PreviewNote::Info("ignored".into()),
            PreviewNote::Warn("ignored too".into()),
            PreviewNote::PerDisk {
                name: "diskA".into(),
                level: NoteLevel::Skip,
                message: "not present".into(),
            },
            PreviewNote::PerDisk {
                name: "diskB".into(),
                level: NoteLevel::Ok,
                message: "found".into(),
            },
        ];
        let plain = render_per_disk_notes(&notes, PerDiskStyle::Plain);
        assert_eq!(plain, "skip: diskA not present\nok: diskB found\n");

        let bracketed = render_per_disk_notes(&notes, PerDiskStyle::Bracketed);
        assert_eq!(
            bracketed,
            "[skip] disk diskA: not present\n[ok]   disk diskB: found\n",
        );
    }

    /* Intent: render_notes_for_stderr emits every note kind in
     * insertion order: Info unadorned, Warn as `[warn] <body>`,
     * PerDisk via the chosen style.
     * Why it exists: this helper is the failure-path / real-run
     * prelude renderer for Shape A commands. Wording or ordering
     * drift here changes user-visible stderr for unlock/recover/etc.
     * once those commands migrate.
     * Scenario: one note of each kind in a fixed order, rendered in
     * Bracketed style.
     */
    #[test]
    fn render_notes_for_stderr_handles_all_kinds_in_order() {
        let notes = vec![
            PreviewNote::Info("pool already mounted at /mnt/storage".into()),
            PreviewNote::PerDisk {
                name: "disk1".into(),
                level: NoteLevel::Skip,
                message: "not found (unplugged?)".into(),
            },
            PreviewNote::Warn("pool has 1 missing device(s)".into()),
        ];
        let rendered = render_notes_for_stderr(&notes, PerDiskStyle::Bracketed);
        let expected = "\
pool already mounted at /mnt/storage
[skip] disk disk1: not found (unplugged?)
[warn] pool has 1 missing device(s)
";
        assert_eq!(rendered, expected);
    }

    /* Intent: color-aware stderr rendering only wraps bracketed status
     * tags, never Info lines or message bodies.
     * Why it exists: live command output should become easier to scan
     * in TTYs without changing the text contract once ANSI is stripped.
     * Scenario: one note of each kind in a fixed order, rendered in
     * Bracketed style with color enabled.
     */
    #[test]
    fn render_notes_for_stderr_with_colors_bracketed_tags_only() {
        let notes = vec![
            PreviewNote::Info("pool already mounted at /mnt/storage".into()),
            PreviewNote::PerDisk {
                name: "disk1".into(),
                level: NoteLevel::Skip,
                message: "not found (unplugged?)".into(),
            },
            PreviewNote::Warn("pool has 1 missing device(s)".into()),
        ];
        let rendered = render_notes_for_stderr_with(&notes, PerDiskStyle::Bracketed, true);
        let expected = "\
pool already mounted at /mnt/storage
\x1b[90m[skip]\x1b[0m disk disk1: not found (unplugged?)
\x1b[33m[warn]\x1b[0m pool has 1 missing device(s)
";
        assert_eq!(rendered, expected);
    }
}
