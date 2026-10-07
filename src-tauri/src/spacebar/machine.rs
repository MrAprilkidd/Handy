//! Pure hold-versus-tap state machine for spacebar dictation.
#![allow(dead_code)]
//!
//! The Windows low-level hook is the only caller. It must run the transition
//! and, when `inject_space` is set, synthesize the space *before* forwarding
//! the event that produced the decision. That is what keeps "hello world"
//! from coming out as "hellow orld": the space is inserted ahead of the key
//! that interrupted the hold, not after it.

/// Where the current physical Space press is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Space is not being swallowed.
    Idle,
    /// Space went down with no modifier. Waiting to see a tap, a hold, or
    /// another key.
    Pending,
    /// A space character was already synthesized for this press. Further
    /// Space events from the same press are swallowed so auto-repeat cannot
    /// insert more spaces.
    Emitted,
    /// The hold crossed the threshold. Recording is in progress.
    Dictating,
}

/// Input the hook reports. Injected keystrokes never reach this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Physical Space went down. `modifiers` is true when Shift, Ctrl, Alt,
    /// or Win was already down. `armed` is consulted only for a fresh press
    /// from [`Phase::Idle`]; repeats ignore it.
    SpaceDown {
        modifiers: bool,
        armed: bool,
    },
    SpaceUp,
    /// Some other key went down while Space may still be down.
    OtherDown,
    /// The hold timer elapsed.
    Threshold,
}

/// What the hook should do with the event that was just classified.
///
/// `inject_space` means "synthesize one Space down+up before this event
/// continues." `swallow` means "do not forward this event."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Decision {
    pub swallow: bool,
    pub inject_space: bool,
    pub start: bool,
    pub stop: bool,
    pub arm_timer: bool,
    pub cancel_timer: bool,
}

#[derive(Debug)]
pub struct Machine {
    phase: Phase,
    space_held: bool,
}

impl Machine {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            space_held: false,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle && !self.space_held
    }

    pub fn space_held(&self) -> bool {
        self.space_held
    }

    pub fn on_event(&mut self, event: Event) -> Decision {
        match event {
            Event::SpaceDown { modifiers, armed } => {
                if self.space_held {
                    // Auto-repeat of a press we already classified.
                    return Decision {
                        swallow: self.phase != Phase::Idle,
                        ..Decision::default()
                    };
                }
                self.space_held = true;
                if self.phase != Phase::Idle || modifiers || !armed {
                    // Modifier+Space, a disarmed trigger, or a press we are
                    // not tracking: the key must hit the focused app unchanged.
                    return Decision::default();
                }
                self.phase = Phase::Pending;
                Decision {
                    swallow: true,
                    arm_timer: true,
                    ..Decision::default()
                }
            }
            Event::SpaceUp => {
                self.space_held = false;
                match self.phase {
                    Phase::Idle => Decision::default(),
                    Phase::Pending => {
                        self.phase = Phase::Idle;
                        Decision {
                            swallow: true,
                            inject_space: true,
                            cancel_timer: true,
                            ..Decision::default()
                        }
                    }
                    Phase::Emitted => {
                        self.phase = Phase::Idle;
                        Decision {
                            swallow: true,
                            cancel_timer: true,
                            ..Decision::default()
                        }
                    }
                    Phase::Dictating => {
                        self.phase = Phase::Idle;
                        Decision {
                            swallow: true,
                            stop: true,
                            cancel_timer: true,
                            ..Decision::default()
                        }
                    }
                }
            }
            Event::OtherDown => {
                if self.phase != Phase::Pending {
                    return Decision::default();
                }
                // The interrupting key has not been forwarded yet. The hook
                // injects the space first, then lets this key through, so the
                // characters stay in the order they were pressed.
                self.phase = Phase::Emitted;
                Decision {
                    inject_space: true,
                    cancel_timer: true,
                    ..Decision::default()
                }
            }
            Event::Threshold => {
                if self.phase != Phase::Pending {
                    return Decision::default();
                }
                self.phase = Phase::Dictating;
                Decision {
                    start: true,
                    cancel_timer: true,
                    ..Decision::default()
                }
            }
        }
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(armed: bool, modifiers: bool) -> Event {
        Event::SpaceDown { modifiers, armed }
    }

    #[test]
    fn tap_injects_one_space_and_swallows_the_physical_key() {
        let mut machine = Machine::new();
        let press = machine.on_event(down(true, false));
        assert!(press.swallow);
        assert!(press.arm_timer);
        assert!(!press.inject_space);
        assert_eq!(machine.phase(), Phase::Pending);

        let release = machine.on_event(Event::SpaceUp);
        assert!(release.swallow);
        assert!(release.inject_space);
        assert!(release.cancel_timer);
        assert!(!release.start);
        assert!(machine.is_idle());
    }

    #[test]
    fn hold_starts_on_the_threshold_and_stops_on_release_without_a_space() {
        let mut machine = Machine::new();
        assert!(machine.on_event(down(true, false)).swallow);

        let fired = machine.on_event(Event::Threshold);
        assert!(fired.start);
        assert!(!fired.inject_space);
        assert_eq!(machine.phase(), Phase::Dictating);

        let release = machine.on_event(Event::SpaceUp);
        assert!(release.stop);
        assert!(release.swallow);
        assert!(!release.inject_space);
        assert!(machine.is_idle());
    }

    #[test]
    fn other_key_before_the_threshold_injects_space_on_that_key_not_after() {
        let mut machine = Machine::new();
        machine.on_event(down(true, false));

        let interrupted = machine.on_event(Event::OtherDown);
        assert!(interrupted.inject_space);
        assert!(
            !interrupted.swallow,
            "the interrupting key must be forwarded"
        );
        assert!(interrupted.cancel_timer);
        assert!(!interrupted.start);
        assert_eq!(machine.phase(), Phase::Emitted);

        // A second key in the same press must not insert another space.
        let again = machine.on_event(Event::OtherDown);
        assert!(!again.inject_space);
        assert!(!again.swallow);

        let release = machine.on_event(Event::SpaceUp);
        assert!(release.swallow);
        assert!(!release.inject_space);
        assert!(machine.is_idle());
    }

    #[test]
    fn auto_repeat_while_pending_does_not_inject_extra_spaces() {
        let mut machine = Machine::new();
        machine.on_event(down(true, false));

        let repeat = machine.on_event(down(true, false));
        assert!(repeat.swallow);
        assert!(!repeat.inject_space);
        assert!(!repeat.arm_timer);
        assert_eq!(machine.phase(), Phase::Pending);

        let release = machine.on_event(Event::SpaceUp);
        assert!(release.inject_space);
        assert!(machine.is_idle());
    }

    #[test]
    fn auto_repeat_while_dictating_is_swallowed() {
        let mut machine = Machine::new();
        machine.on_event(down(true, false));
        machine.on_event(Event::Threshold);

        let repeat = machine.on_event(down(false, false));
        assert!(repeat.swallow);
        assert!(!repeat.inject_space);
        assert!(!repeat.stop);
        assert_eq!(machine.phase(), Phase::Dictating);
    }

    #[test]
    fn modifier_combo_passes_through_including_repeat_and_release() {
        let mut machine = Machine::new();
        let press = machine.on_event(down(true, true));
        assert_eq!(press, Decision::default());
        assert!(machine.space_held());
        assert_eq!(machine.phase(), Phase::Idle);

        let repeat = machine.on_event(down(true, true));
        assert_eq!(repeat, Decision::default());

        let release = machine.on_event(Event::SpaceUp);
        assert_eq!(release, Decision::default());
        assert!(machine.is_idle());
    }

    #[test]
    fn disarmed_trigger_passes_space_through() {
        let mut machine = Machine::new();
        assert_eq!(machine.on_event(down(false, false)), Decision::default());
        assert_eq!(machine.on_event(Event::OtherDown), Decision::default());
        assert_eq!(machine.on_event(Event::SpaceUp), Decision::default());
        assert!(machine.is_idle());
    }

    #[test]
    fn threshold_after_a_tap_or_interruption_does_not_start() {
        let mut tapped = Machine::new();
        tapped.on_event(down(true, false));
        tapped.on_event(Event::SpaceUp);
        assert_eq!(tapped.on_event(Event::Threshold), Decision::default());

        let mut typed = Machine::new();
        typed.on_event(down(true, false));
        typed.on_event(Event::OtherDown);
        assert_eq!(typed.on_event(Event::Threshold), Decision::default());
        assert_eq!(typed.phase(), Phase::Emitted);
    }

    #[test]
    fn keys_typed_during_dictation_pass_through() {
        let mut machine = Machine::new();
        machine.on_event(down(true, false));
        machine.on_event(Event::Threshold);
        assert_eq!(machine.on_event(Event::OtherDown), Decision::default());
    }

    #[test]
    fn a_later_press_can_arm_after_a_disarmed_one() {
        let mut machine = Machine::new();
        machine.on_event(down(false, false));
        machine.on_event(Event::SpaceUp);

        let press = machine.on_event(down(true, false));
        assert!(press.swallow);
        assert_eq!(machine.phase(), Phase::Pending);
    }
}
