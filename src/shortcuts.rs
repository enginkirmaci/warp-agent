//! The shortcut sheet's content: every key the window answers to, grouped by
//! the surface that owns it.
//!
//! The rows live here once, next to the handlers they describe. Only real
//! bindings are listed: the window root's two chords, the sidebar's row keys,
//! the transcript's scroll keys, the composer's Enter/history/agent keys and
//! the composer's command-menu keys. Settings, About and the context menus are
//! pointer surfaces, so they add no rows.

use gpui::SharedString;
use warpui_kit_core::shortcuts::Group;

fn row(keys: &str, label: &str) -> (SharedString, SharedString) {
    (SharedString::from(keys), SharedString::from(label))
}

/// The whole sheet, in the order the window reads: window, sidebar,
/// transcript, composer, then the composer's command menu.
pub fn groups() -> Vec<Group> {
    let window = Group::new(
        "Window",
        vec![
            row("? / Ctrl /", "Show or hide this shortcut sheet"),
            row("Ctrl \\", "Show or hide the sidebar"),
            row("Esc", "Close this shortcut sheet"),
        ],
    );
    let sidebar = Group::new(
        "Sidebar",
        vec![
            row("↑ / ↓", "Move the row cursor"),
            row("Home / End", "First or last row"),
            row("PageUp / PageDown", "Move a page"),
            row("Enter", "Open the selected chat or folder"),
            row("→ / Tab", "Focus the first chat's transcript"),
        ],
    );
    let transcript = Group::new(
        "Transcript",
        vec![
            row("↑ / ↓", "Scroll one line"),
            row("PageUp / PageDown", "Scroll one page"),
            row("Home / End", "First or newest message"),
            row("← / Shift Tab", "Focus the sidebar, or the previous chat"),
            row("→ / Tab", "Focus the next chat"),
        ],
    );
    let composer = Group::new(
        "Composer",
        vec![
            row("Enter", "Send the message"),
            row("Shift Enter", "Start a new line"),
            row(
                "↑ / ↓",
                "Move the caret; at the first or last line, step prompt history",
            ),
            row("Shift Tab", "Cycle the tile's agent"),
        ],
    );
    let command_menu = Group::new(
        "Command menu",
        vec![
            row("↑ / ↓", "Choose a suggestion"),
            row("Enter", "Accept the suggestion"),
            row("Esc", "Close the menu"),
        ],
    );
    vec![window, sidebar, transcript, composer, command_menu]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn keys(groups: &[Group], title: &str) -> Vec<String> {
        groups
            .iter()
            .find(|group| group.title.as_ref() == title)
            .map(|group| {
                group
                    .rows
                    .iter()
                    .map(|(keys, _)| keys.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn every_group_lists_a_key_and_a_plain_description() {
        let groups = groups();
        assert!(!groups.is_empty(), "the sheet has groups");
        for group in &groups {
            assert!(!group.title.is_empty(), "a group has no title");
            assert!(!group.rows.is_empty(), "{} lists no rows", group.title);
            for (keys, label) in &group.rows {
                assert!(!keys.is_empty(), "{} has an unnamed key", group.title);
                assert!(
                    !label.is_empty(),
                    "{}'s {keys} has no description",
                    group.title
                );
            }
        }
    }

    #[test]
    fn no_group_repeats_a_key() {
        let groups = groups();
        for group in &groups {
            let mut seen = HashSet::new();
            for (keys, _) in &group.rows {
                assert!(
                    seen.insert(keys.to_string()),
                    "{} repeats {keys}",
                    group.title
                );
            }
        }
    }

    #[test]
    fn the_window_chords_are_listed() {
        let window = keys(&groups(), "Window");
        for key in ["? / Ctrl /", "Ctrl \\", "Esc"] {
            assert!(window.iter().any(|row| row == key), "{key} is missing");
        }
    }

    #[test]
    fn the_sidebar_row_keys_and_pane_move_are_listed() {
        let sidebar = keys(&groups(), "Sidebar");
        for key in [
            "↑ / ↓",
            "Home / End",
            "PageUp / PageDown",
            "Enter",
            "→ / Tab",
        ] {
            assert!(sidebar.iter().any(|row| row == key), "{key} is missing");
        }
    }

    #[test]
    fn the_transcript_scroll_and_pane_moves_are_listed() {
        let transcript = keys(&groups(), "Transcript");
        for key in [
            "↑ / ↓",
            "PageUp / PageDown",
            "Home / End",
            "← / Shift Tab",
            "→ / Tab",
        ] {
            assert!(transcript.iter().any(|row| row == key), "{key} is missing");
        }
    }

    #[test]
    fn the_composer_and_its_menu_are_listed() {
        let groups = groups();
        let composer = keys(&groups, "Composer");
        for key in ["Enter", "Shift Enter", "↑ / ↓", "Shift Tab"] {
            assert!(composer.iter().any(|row| row == key), "{key} is missing");
        }
        let menu = keys(&groups, "Command menu");
        for key in ["↑ / ↓", "Enter", "Esc"] {
            assert!(menu.iter().any(|row| row == key), "{key} is missing");
        }
    }
}
