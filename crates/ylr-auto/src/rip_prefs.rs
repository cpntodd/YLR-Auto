//! What a drive does: each option's choices, its presets, and how a drive's own choice
//! overrides the default.
//!
//! Options are described in one table so the settings page and each drive's section
//! cannot disagree. Values are saved as text, so a choice from a newer version reads as
//! the default here and survives being saved again.

use crate::config::Config;
use ylr_core::ripper;

pub struct PrefOption {
    pub key: &'static str,
    pub title: &'static str,
    /// Saved value and label, with the built-in default first.
    pub choices: &'static [(&'static str, &'static str)],
}

pub const RIP_ACTION: &str = "rip_action";
pub const ON_INSERT: &str = "on_insert";
pub const TITLE_LIST: &str = "title_list";
pub const ROW_PROGRESS: &str = "row_progress";
pub const ROW_LOOK: &str = "row_look";
pub const STOP: &str = "stop";
pub const DESTINATION: &str = "destination";
pub const FAILURE: &str = "failure";

pub const OPTIONS: [PrefOption; 8] = [
    PrefOption {
        key: RIP_ACTION,
        title: "Drive button",
        choices: &[
            ("open", "Open the disc and choose"),
            ("auto", "Rip the main title if obvious, otherwise choose"),
            ("all", "Rip every title over the minimum length"),
            ("per_press", "Choose from a menu on each press"),
        ],
    },
    PrefOption {
        key: ON_INSERT,
        title: "When a disc is inserted",
        choices: &[
            ("nothing", "Do nothing"),
            ("scan", "Scan it, then wait"),
            ("rip", "Rip it, following the drive button"),
        ],
    },
    PrefOption {
        key: TITLE_LIST,
        title: "Title list",
        choices: &[
            ("expandable", "Expand each title to see its tracks"),
            ("compact", "Compact, without tracks"),
            ("detailed", "Detailed, with every title's tracks shown"),
        ],
    },
    PrefOption {
        key: ROW_PROGRESS,
        title: "Progress in the drive's row",
        choices: &[
            ("summary", "Bar, percentage and time left"),
            ("full", "Bar, time left, title and speed"),
            ("two_bars", "Bars for the title and the disc"),
            ("minimal", "Bar and percentage"),
        ],
    },
    PrefOption {
        key: ROW_LOOK,
        title: "Drive status",
        choices: &[
            ("text", "Text and accent colours"),
            ("pill", "Coloured label"),
            ("icon", "Icon"),
            ("tint", "Tinted row"),
        ],
    },
    PrefOption {
        key: STOP,
        title: "Stop button",
        choices: &[
            (
                "confirm_remove",
                "Confirm, then remove the unfinished title",
            ),
            ("confirm_keep", "Confirm, then keep the unfinished title"),
            ("remove", "Stop at once and remove the unfinished title"),
            ("ask", "Ask what to do each time"),
        ],
    },
    PrefOption {
        key: DESTINATION,
        title: "Where rips go",
        choices: &[
            ("label", "Rip folder, named from the disc"),
            ("editable", "Rip folder, with a name you can edit"),
            ("drive", "The drive's own folder, named from the disc"),
            (
                "drive_editable",
                "The drive's own folder, with a name you can edit",
            ),
        ],
    },
    PrefOption {
        key: FAILURE,
        title: "When a rip fails",
        choices: &[
            ("stop", "Stop and keep the disc in the drive"),
            ("retry", "Try once more, then stop"),
            ("eject", "Eject the disc and carry on"),
        ],
    },
];

pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    pub values: [(&'static str, &'static str); 4],
}

pub const PRESETS: [Preset; 3] = [
    Preset {
        name: "Cautious",
        description: "Nothing starts until you open a disc and press Rip.",
        values: [
            (RIP_ACTION, "open"),
            (ON_INSERT, "nothing"),
            (STOP, "confirm_remove"),
            (FAILURE, "stop"),
        ],
    },
    Preset {
        name: "Hands-off",
        description: "Inserting a disc rips it, for feeding discs through unattended.",
        values: [
            (RIP_ACTION, "auto"),
            (ON_INSERT, "rip"),
            (STOP, "remove"),
            (FAILURE, "eject"),
        ],
    },
    Preset {
        name: "Always ask",
        description: "Discs are scanned when inserted, and every choice is yours.",
        values: [
            (RIP_ACTION, "open"),
            (ON_INSERT, "scan"),
            (STOP, "ask"),
            (FAILURE, "stop"),
        ],
    },
];

pub fn option(key: &str) -> Option<&'static PrefOption> {
    OPTIONS.iter().find(|option| option.key == key)
}

/// The value a drive uses: its own choice, the default, or the built-in default. A value
/// this version does not list counts as unset at that level.
pub fn resolved(config: &Config, drive_id: &str, key: &str) -> &'static str {
    let Some(option) = option(key) else {
        return "";
    };
    let listed = |saved: Option<String>| {
        let saved = saved?;
        option
            .choices
            .iter()
            .find(|(value, _)| *value == saved)
            .map(|(value, _)| *value)
    };
    listed(config.ripping_value(Some(drive_id), key))
        .or_else(|| listed(config.ripping_value(None, key)))
        .unwrap_or(option.choices[0].0)
}

/// Every title shorter than this is ignored. The same value must reach both the scan and
/// the rip, because title numbers depend on it, so read it once per disc.
pub fn minimum_title_seconds(config: &Config) -> u32 {
    let minutes = config
        .ripping_value(None, MINIMUM_MINUTES)
        .and_then(|text| text.parse::<u32>().ok());
    minutes.map_or(ripper::DEFAULT_MINIMUM_TITLE_SECONDS, |minutes| {
        minutes * 60
    })
}

pub const MINIMUM_MINUTES: &str = "minimum_minutes";
pub const DRIVE_NAME: &str = "name";
pub const DRIVE_FOLDER: &str = "folder";
pub const LAST_RIP_MODE: &str = "last_rip_mode";
pub const ALERT_FINISHED: &str = "alert_finished";
pub const ALERT_FAILED: &str = "alert_failed";
pub const ALERT_NEEDS_INPUT: &str = "alert_needs_input";
pub const ALERT_SOUND: &str = "alert_sound";

/// Alerts are on unless turned off, so an old config keeps notifying.
pub fn alert_enabled(config: &Config, key: &str) -> bool {
    config.ripping_value(None, key).as_deref() != Some("off")
}

/// The drive's own folder, when it should be used, or the rip folder.
pub fn rip_parent_folder(config: &Config, drive_id: &str) -> String {
    let uses_drive_folder = matches!(
        resolved(config, drive_id, DESTINATION),
        "drive" | "drive_editable"
    );
    let own_folder = config
        .ripping_value(Some(drive_id), DRIVE_FOLDER)
        .filter(|folder| uses_drive_folder && !folder.is_empty());
    own_folder.unwrap_or_else(|| config.text("rip_dir"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config() -> Config {
        Config::defaults(Path::new("/home/example"))
    }

    #[test]
    fn a_new_install_is_cautious() {
        let config = config();
        for (key, value) in PRESETS[0].values {
            assert_eq!(resolved(&config, "any", key), value, "{key}");
        }
    }

    #[test]
    fn a_drives_choice_overrides_the_default_for_that_drive_only() {
        let mut config = config();
        config.set_ripping_value(None, STOP, Some("ask"));
        config.set_ripping_value(Some("A"), STOP, Some("remove"));

        assert_eq!(resolved(&config, "A", STOP), "remove");
        assert_eq!(resolved(&config, "B", STOP), "ask");
    }

    #[test]
    fn clearing_a_drives_choice_returns_to_the_default() {
        let mut config = config();
        config.set_ripping_value(Some("A"), STOP, Some("remove"));
        config.set_ripping_value(Some("A"), STOP, None);
        assert_eq!(resolved(&config, "A", STOP), "confirm_remove");
    }

    #[test]
    fn an_unknown_saved_choice_reads_as_the_default() {
        let mut config = config();
        config.set_ripping_value(None, STOP, Some("from_a_newer_version"));
        assert_eq!(resolved(&config, "A", STOP), "confirm_remove");
    }

    #[test]
    fn every_preset_value_is_a_listed_choice() {
        for preset in &PRESETS {
            for (key, value) in preset.values {
                let listed = option(key).expect("a listed option").choices;
                assert!(listed.iter().any(|(choice, _)| *choice == value), "{value}");
            }
        }
    }

    #[test]
    fn the_minimum_length_falls_back_to_makemkvs() {
        let mut config = config();
        assert_eq!(minimum_title_seconds(&config), 120);
        config.set_ripping_value(None, MINIMUM_MINUTES, Some("20"));
        assert_eq!(minimum_title_seconds(&config), 1200);
    }

    #[test]
    fn alerts_default_to_on() {
        let mut config = config();
        assert!(alert_enabled(&config, ALERT_FAILED));
        config.set_ripping_value(None, ALERT_FAILED, Some("off"));
        assert!(!alert_enabled(&config, ALERT_FAILED));
    }

    #[test]
    fn a_drive_folder_applies_only_when_its_destination_uses_it() {
        let mut config = config();
        config.set_text("rip_dir", "/rips");
        config.set_ripping_value(Some("A"), DRIVE_FOLDER, Some("/big"));
        assert_eq!(rip_parent_folder(&config, "A"), "/rips");

        config.set_ripping_value(Some("A"), DESTINATION, Some("drive"));
        assert_eq!(rip_parent_folder(&config, "A"), "/big");
    }

    #[test]
    fn a_drive_destination_with_no_folder_uses_the_rip_folder() {
        let mut config = config();
        config.set_text("rip_dir", "/rips");
        config.set_ripping_value(Some("A"), DESTINATION, Some("drive"));
        assert_eq!(rip_parent_folder(&config, "A"), "/rips");
    }
}
