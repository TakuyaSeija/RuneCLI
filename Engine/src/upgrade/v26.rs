//! The footer's contested-message interval is a new optional choice. Its
//! default comes from FIELDS; old homes need no rewritten configuration or
//! history. The ordered barrier still makes the settings shape explicit.
use super::Plan;
use crate::Result;
use std::path::Path;

pub(super) fn apply(_settings: &Path, _backup: &Path, _plan: &mut Plan) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_timer_step_preserves_existing_choices_and_damaged_items() {
        let root = std::env::temp_dir().join(format!("footer-v26-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        let settings = root.join("rune.toml");
        let text = "[ui]\nstatus_lifetime='11s'\nstop_lifetime='5s'\n";
        let damaged = root.join("sessions/torn.json");
        std::fs::write(&settings, text).unwrap();
        std::fs::write(&damaged, "{broken").unwrap();
        apply(&settings, &root.join("backup"), &mut Plan::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), text);
        assert_eq!(std::fs::read_to_string(&damaged).unwrap(), "{broken");
        let config = crate::settings::configuration::Configuration::load(&root).unwrap();
        assert_eq!(
            config.duration("ui.status_replace_lifetime"),
            std::time::Duration::from_secs(3)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
