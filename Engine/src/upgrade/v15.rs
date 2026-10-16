//! Format 14 → 15 lays out the default-enabled sanitizer once. A person's
//! explicit opt-out survives this step; format 16 then tells a `false` an
//! earlier build wrote as its own default apart from a choice ([`super::defaults`]).
//! The owner’s approved live activation goes through the normal config writer.
use super::Plan;
use crate::error::Result;
use std::path::Path;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(
        settings,
        backup,
        plan,
        crate::settings::configuration::laid_out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_layout_enables_sanitizer_and_preserves_an_explicit_opt_out() {
        use crate::settings::configuration::{laid_out, Configuration};
        for (text, enabled) in [
            ("[ui]\ntheme=\"amber\"\n", true),
            (
                "[sanitizer]\nenabled=false\nmax_output_tokens=8192\n",
                false,
            ),
        ] {
            let next = laid_out(text).unwrap();
            let values = toml_edit::de::from_str(&next).unwrap();
            let config = Configuration::from_values(values).unwrap();
            assert_eq!(config.flag("sanitizer.enabled"), enabled);
            if !enabled {
                assert_eq!(config.number("sanitizer.max_output_tokens"), 8192);
            }
            assert_eq!(laid_out(&next).unwrap(), next);
        }
    }

    #[test]
    fn upgrade_backs_up_every_original_byte_before_laying_out_default_on() {
        let root =
            std::env::temp_dir().join(format!("sanitizer-on-upgrade-{}", std::process::id()));
        let home = root.join("home");
        let backup = root.join("backup");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&backup).unwrap();
        // Nothing to lay out: the file is not rewritten.
        let settled = "# keep my comment\n[ui]\ntheme=\"amber\"\n";
        std::fs::write(home.join("rune.toml"), settled).unwrap();
        let mut plan = Plan::default();
        apply(&home, &backup, &mut plan).unwrap();
        assert!(plan.config.is_none());
        assert_eq!(
            std::fs::read_to_string(home.join("rune.toml")).unwrap(),
            settled
        );
        // Laid out anew: every original byte is in the backup first.
        let original = "# keep my comment\n[ui]\ntheme=\"amber\"\n[classifier]\nenabled=true\n";
        std::fs::write(home.join("rune.toml"), original).unwrap();
        apply(&home, &backup, &mut plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(plan.config.unwrap()).unwrap(),
            original
        );
        assert!(crate::settings::configuration::Configuration::load(&home)
            .unwrap()
            .flag("sanitizer.enabled"));
        let _ = std::fs::remove_dir_all(root);
    }
}
