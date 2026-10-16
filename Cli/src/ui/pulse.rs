//! The one animation on the screen: proof that the engine is still working.
//!
//! A smooth braille spinner that rotates gracefully while the engine is generating
//! or executing tools, without cluttering the screen or masking project context.

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// How many app ticks (16ms each) elapse per spinner animation frame.
/// 6 ticks = 96ms per frame (~10 fps), providing a calm, smooth rotation.
#[cfg(test)]
const TICKS_PER_FRAME: usize = 6;

/// Returns the current spinner character for the given frame count.
pub fn spinner(frame: usize) -> char {
    SPINNER[(frame / super::runtime_configuration().number("ui.spinner_frames") as usize)
        % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_cycles_smoothly() {
        for frame in 0..SPINNER.len() * TICKS_PER_FRAME * 2 {
            let c = spinner(frame);
            assert!(SPINNER.contains(&c));
        }
    }
}
