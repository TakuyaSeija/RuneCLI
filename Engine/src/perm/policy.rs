//! The decision table. Five inputs, three outcomes, no side effects.
//!
//! Keeping this pure is the point: the table is small enough to read in one
//! sitting and to test exhaustively, and every "why did it ask me that" question
//! is answered by one function.

use serde::{Deserialize, Serialize};

use super::denylist::Risk;

/// How much the session trusts the agent.
///
/// One axis, three settings. Ask and Auto put every dangerous or
/// outside-workspace call to the person and refuse nothing on their own — the
/// one autonomous denier is the thinking classifier, and only where a
/// CLASSIFIER.md policy says so. Full means the host access is broad and
/// permission questions are skipped entirely: nothing is asked, nothing is
/// confined, nothing is refused. That is intentionally stronger than an
/// "allow always" grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Read freely inside the workspace; ask about everything else, every time.
    ///
    /// Writes ask unless the user chose to remember that exact grant, anything
    /// naming a path outside the workspace asks, and **every command asks** —
    /// see [`Situation::runs`]. The default, and the only mode where nothing
    /// changes on disk and nothing runs without a person seeing it first.
    #[default]
    Ask,
    /// Anything confined to the workspace runs without asking. Dangerous calls
    /// still ask, every time. This is the mode for getting work done.
    Auto,
    /// Nothing is asked and nothing is confined: the workspace jail is off and
    /// so is the kernel sandbox. The agent can reach the whole machine as the
    /// user. Even the catastrophic deny-list class does not ask here — the
    /// owner chose this mode with open eyes, and choosing it is the answer.
    Full,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "ask" => Some(Mode::Ask),
            "auto" => Some(Mode::Auto),
            "full" => Some(Mode::Full),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Ask => "ask",
            Mode::Auto => "auto",
            Mode::Full => "full",
        }
    }

    /// Does this mode confine tools to the workspace?
    pub fn confines(&self) -> bool {
        !matches!(self, Mode::Full)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run it without interrupting anyone.
    Allow,
    /// Ask first. `remember` says whether "allow always" may even be offered.
    Ask { remember: bool },
}

/// Everything the table is allowed to look at. A struct rather than five
/// positional arguments: `verdict(mode, true, &risk, false, true)` reads as
/// nothing at all, and the two booleans at the end are exactly the pair a
/// caller will one day swap.
pub struct Situation<'a> {
    /// How much the session trusts the agent.
    pub mode: Mode,
    /// The tool changes something (files, processes, the machine).
    pub mutates: bool,
    /// What the deny-list made of the specific arguments.
    pub risk: &'a Risk,
    /// This project already has a standing grant for this exact key.
    pub granted: bool,
    /// The call names paths outside the workspace that are **not** yet allowed.
    ///
    /// This overrides "reads never prompt". A read is confined by the jail, and
    /// that is the whole reason it needs no prompt — a read that leaves the jail
    /// has no such argument behind it. `cat ~/.ssh/id_ed25519` mutates nothing.
    pub reaches: bool,
    /// The requested reach is too broad to express as a narrow path grant.
    /// Consent therefore drops both confinements for this call only and can
    /// never be remembered.
    pub full_access: bool,
    /// At least one reach candidate names something the machine itself runs
    /// on — credentials, startup files, system prefixes. The gate still asks
    /// rather than refusing — the system has no voice of its own to say no —
    /// but consent is never stored, and the card carries the reason.
    pub sensitive: bool,
    /// The call runs a command line rather than touching a named path.
    ///
    /// # Why this is its own axis
    ///
    /// Every other tool declares exactly what it will touch, and the gate can
    /// read that declaration. A shell command cannot be read that way by
    /// anything short of a shell: `cat $(find / -name id_rsa)`, `curl x | sh`
    /// and `rm -rf $DIR` all name their real targets only at the moment they
    /// run, and [`crate::perm::reach`] is honest about being a scanner rather
    /// than an interpreter.
    ///
    /// So in `Ask` — the mode whose whole promise is *nothing runs without a
    /// person seeing it* — a command asks. Every time, with no "always" on
    /// offer: a grant remembered against `cargo` would cover a `cargo` line
    /// nobody has read yet. `Auto` is the mode for working without that, and
    /// choosing it is the answer.
    pub runs: bool,
    /// A command this session runs now is confined by the kernel sandbox.
    ///
    /// `Auto` lets commands run unasked because the sandbox keeps them inside
    /// the workspace and what the person allowed. Where it is not enforcing —
    /// a kernel without Landlock, a ruleset that could not be built — nothing
    /// does, so `Auto` cannot keep that promise: see [`unconfined_command`].
    pub confined: bool,
}

/// A command line in `Auto` with no kernel sandbox under it: read to the
/// person, as in `Ask`, with no "always" — failing closed rather than letting
/// the mode's promise relax into running commands nothing confines. The one
/// rule both the decision and the card's reason come from.
pub fn unconfined_command(s: &Situation<'_>) -> bool {
    s.runs && matches!(s.mode, Mode::Auto) && !s.confined
}

pub fn verdict(s: &Situation<'_>) -> Verdict {
    let unconfined = unconfined_command(s);
    let Situation {
        mode,
        mutates,
        risk,
        granted,
        reaches,
        full_access,
        sensitive,
        runs,
        confined: _,
    } = *s;
    // Full means the host itself is the boundary, and the owner chose it with
    // open eyes: nothing is asked, nothing is confined, nothing is refused.
    // The deny-list still classifies — its reasons lead the wizard's cards in
    // the other two modes — but here it gates nothing.
    if matches!(mode, Mode::Full) {
        return Verdict::Allow;
    }
    match risk {
        // Consent for these is never stored, so a standing grant cannot apply and
        // is not offered. Auto mode does not cover them either: "run my builds
        // without asking" is not consent to `sudo` — or to `rm -rf /`. The
        // question is the gate's whole authority; refusing is not in it.
        Risk::AskEvery { .. } | Risk::Severe { .. } => Verdict::Ask { remember: false },

        Risk::Normal => {
            if full_access || sensitive {
                // A reach too broad to anchor as a narrow grant, or a path the
                // machine runs on its own: the question is put, but no answer
                // may be remembered against it.
                Verdict::Ask { remember: false }
            } else if reaches {
                // Leaving the workspace is never covered by "work freely inside
                // the workspace", so auto mode asks here too — and the prompt
                // names the paths rather than the tool.
                Verdict::Ask { remember: true }
            } else if runs && matches!(mode, Mode::Ask) || unconfined {
                // A command line cannot be read in advance, so `Ask` reads it
                // aloud instead — and so does `Auto` where no sandbox holds it.
                // No "always": the thing that would be remembered is not the
                // thing that will run next time.
                Verdict::Ask { remember: false }
            } else if !mutates {
                // Reads are already confined by the jail; asking about them would
                // train the user to click "allow" without reading.
                Verdict::Allow
            } else if granted || matches!(mode, Mode::Auto) {
                Verdict::Allow
            } else {
                Verdict::Ask { remember: true }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask_every() -> Risk {
        Risk::AskEvery { why: "x".into() }
    }

    fn severe() -> Risk {
        Risk::Severe { why: "x".into() }
    }

    /// The ordinary case, so each test states only what it is about.
    fn at(mode: Mode, mutates: bool, risk: &Risk, granted: bool) -> Verdict {
        verdict(&Situation {
            mode,
            mutates,
            risk,
            granted,
            reaches: false,
            full_access: false,
            sensitive: false,
            runs: false,
            confined: true,
        })
    }

    fn reaching(mode: Mode, mutates: bool) -> Verdict {
        verdict(&Situation {
            mode,
            mutates,
            risk: &Risk::Normal,
            granted: true,
            reaches: true,
            full_access: false,
            sensitive: false,
            runs: false,
            confined: true,
        })
    }

    fn running(mode: Mode, granted: bool) -> Verdict {
        verdict(&Situation {
            mode,
            mutates: true,
            risk: &Risk::Normal,
            granted,
            reaches: false,
            full_access: false,
            sensitive: false,
            runs: true,
            confined: true,
        })
    }

    /// `Auto` without a kernel sandbox under it reads every command to the
    /// person, as `Ask` does — no grant gets past it and nothing is offered to
    /// remember. Everything else about the mode stands: a file tool is still
    /// held by the engine's own jail, and `Full` is still the person's choice
    /// to stop being asked.
    #[test]
    fn auto_reads_commands_aloud_where_no_sandbox_holds_them() {
        let unconfined = |mode, runs, granted| {
            verdict(&Situation {
                mode,
                mutates: true,
                risk: &Risk::Normal,
                granted,
                reaches: false,
                full_access: false,
                sensitive: false,
                runs,
                confined: false,
            })
        };
        assert_eq!(
            unconfined(Mode::Auto, true, false),
            Verdict::Ask { remember: false }
        );
        assert_eq!(
            unconfined(Mode::Auto, true, true),
            Verdict::Ask { remember: false }
        );
        assert_eq!(
            unconfined(Mode::Ask, true, false),
            Verdict::Ask { remember: false }
        );
        assert_eq!(unconfined(Mode::Full, true, false), Verdict::Allow);
        assert_eq!(
            unconfined(Mode::Auto, false, false),
            Verdict::Allow,
            "a file tool is held by the jail, not the kernel sandbox"
        );
        // Confined, nothing changes.
        assert_eq!(running(Mode::Auto, false), Verdict::Allow);
    }

    /// The owner's rule, in the one place that decides it: in `Ask`, a command
    /// is read by a person before it runs. Not "unless it was allowed once
    /// before" — the grant would be against a name, and the name is not what
    /// runs.
    #[test]
    fn every_command_asks_in_ask_mode_and_no_grant_gets_past_it() {
        assert_eq!(running(Mode::Ask, false), Verdict::Ask { remember: false });
        assert_eq!(running(Mode::Ask, true), Verdict::Ask { remember: false });
        // The other two modes are what a person chooses when they have decided
        // not to read each one.
        assert_eq!(running(Mode::Auto, false), Verdict::Allow);
        assert_eq!(running(Mode::Full, false), Verdict::Allow);
    }

    #[test]
    fn reads_never_prompt() {
        for m in [Mode::Ask, Mode::Auto, Mode::Full] {
            assert_eq!(at(m, false, &Risk::Normal, false), Verdict::Allow);
        }
    }

    #[test]
    fn ask_mode_prompts_for_writes_until_granted() {
        assert_eq!(
            at(Mode::Ask, true, &Risk::Normal, false),
            Verdict::Ask { remember: true }
        );
        assert_eq!(at(Mode::Ask, true, &Risk::Normal, true), Verdict::Allow);
    }

    #[test]
    fn auto_mode_stops_asking_about_ordinary_work() {
        assert_eq!(at(Mode::Auto, true, &Risk::Normal, false), Verdict::Allow);
    }

    #[test]
    fn auto_mode_still_asks_about_the_dangerous_ones() {
        assert_eq!(
            at(Mode::Auto, true, &ask_every(), true),
            Verdict::Ask { remember: false }
        );
    }

    #[test]
    fn full_access_asks_nothing_at_all() {
        assert_eq!(at(Mode::Full, true, &Risk::Normal, false), Verdict::Allow);
        assert_eq!(at(Mode::Full, true, &ask_every(), false), Verdict::Allow);
    }

    #[test]
    fn dangerous_calls_ask_every_time_even_when_granted() {
        assert_eq!(
            at(Mode::Ask, true, &ask_every(), true),
            Verdict::Ask { remember: false }
        );
        assert_eq!(
            at(Mode::Ask, false, &ask_every(), true),
            Verdict::Ask { remember: false }
        );
    }

    /// The owner's rule, 2026-09-28: the system has no voice of its own to say
    /// no. The catastrophic class is a question the person answers — every
    /// time, in both Ask and Auto — and Full, chosen with open eyes, asks
    /// nothing at all.
    #[test]
    fn severe_calls_are_questions_in_ask_and_auto_and_nothing_in_full() {
        for m in [Mode::Ask, Mode::Auto] {
            assert_eq!(
                at(m, true, &severe(), true),
                Verdict::Ask { remember: false }
            );
            assert_eq!(
                at(m, false, &severe(), false),
                Verdict::Ask { remember: false }
            );
        }
        assert_eq!(at(Mode::Full, true, &severe(), true), Verdict::Allow);
    }

    #[test]
    fn a_sensitive_reach_is_asked_and_never_remembered_in_either_mode() {
        // The old bug this replaces: a sensitive path used to be refused
        // outright, before any person saw it. Now it is the person's call.
        let v = |mode, reaches| {
            verdict(&Situation {
                mode,
                mutates: false,
                risk: &Risk::Normal,
                granted: false,
                reaches,
                full_access: false,
                sensitive: true,
                runs: false,
                confined: true,
            })
        };
        assert_eq!(v(Mode::Ask, true), Verdict::Ask { remember: false });
        assert_eq!(v(Mode::Auto, true), Verdict::Ask { remember: false });
        assert_eq!(v(Mode::Ask, false), Verdict::Ask { remember: false });
    }

    #[test]
    fn leaving_the_workspace_always_asks_even_for_a_read_and_even_in_auto() {
        // The bug this closes: a read was allowed without a prompt because the
        // jail confined it — and then the same call left the jail.
        assert_eq!(reaching(Mode::Ask, false), Verdict::Ask { remember: true });
        assert_eq!(reaching(Mode::Auto, false), Verdict::Ask { remember: true });
        assert_eq!(reaching(Mode::Auto, true), Verdict::Ask { remember: true });
    }

    #[test]
    fn full_access_has_nothing_to_widen_so_it_does_not_ask() {
        assert_eq!(reaching(Mode::Full, false), Verdict::Allow);
        assert_eq!(reaching(Mode::Full, true), Verdict::Allow);
    }

    #[test]
    fn a_dangerous_command_that_also_leaves_the_workspace_is_still_unrememberable() {
        let v = verdict(&Situation {
            mode: Mode::Ask,
            mutates: true,
            risk: &ask_every(),
            granted: false,
            reaches: true,
            full_access: false,
            sensitive: false,
            runs: false,
            confined: true,
        });
        assert_eq!(
            v,
            Verdict::Ask { remember: false },
            "sudo does not become rememberable"
        );
    }

    #[test]
    fn modes_round_trip_through_their_names() {
        for m in [Mode::Ask, Mode::Auto, Mode::Full] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        assert_eq!(Mode::parse("yolo"), None);
        assert_eq!(Mode::default(), Mode::Ask);
        assert!(Mode::Ask.confines() && Mode::Auto.confines() && !Mode::Full.confines());
    }
}
