//! Damage-driven capture scheduler policy.
//!
//! XDamage event collection is kept separate from this deterministic state
//! machine so driver fallback and timing can be fault-injected in tests.

#![allow(dead_code)] // wired by the XDamage treatment; fixed-rate remains default

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerState {
    Static,
    Burst,
    Coalescing,
    RefreshDue,
    Background,
    FallbackFixedRate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    Damage,
    InputAnticipation,
    Recovery,
    PeriodicRefresh,
    FrameDeadline,
    Backgrounded,
    Foregrounded,
    ExtensionFailed,
    WatchdogMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerAction {
    Wait,
    CaptureDamage,
    CaptureFull,
    CaptureFixedRate,
}

#[derive(Debug, Clone, Copy)]
pub struct DamageScheduler {
    pub state: SchedulerState,
    pub damage_pending: bool,
    pub last_capture_us: u64,
    pub last_full_refresh_us: u64,
    pub frame_interval_us: u64,
    pub full_refresh_interval_us: u64,
}

impl DamageScheduler {
    pub fn new(fps: u32, full_refresh_interval_us: u64, extension_available: bool) -> Self {
        Self {
            state: if extension_available {
                SchedulerState::Static
            } else {
                SchedulerState::FallbackFixedRate
            },
            damage_pending: false,
            last_capture_us: 0,
            last_full_refresh_us: 0,
            frame_interval_us: 1_000_000 / fps.max(1) as u64,
            full_refresh_interval_us,
        }
    }

    pub fn event(&mut self, reason: WakeReason, now_us: u64) -> SchedulerAction {
        match reason {
            WakeReason::ExtensionFailed | WakeReason::WatchdogMismatch => {
                self.state = SchedulerState::FallbackFixedRate;
            }
            WakeReason::Backgrounded => {
                self.state = SchedulerState::Background;
                return SchedulerAction::Wait;
            }
            WakeReason::Foregrounded if self.state == SchedulerState::Background => {
                self.state = SchedulerState::RefreshDue;
            }
            WakeReason::Recovery | WakeReason::PeriodicRefresh => {
                self.state = SchedulerState::RefreshDue;
            }
            WakeReason::Damage => {
                self.damage_pending = true;
                self.state =
                    if now_us.saturating_sub(self.last_capture_us) >= self.frame_interval_us {
                        SchedulerState::Burst
                    } else {
                        SchedulerState::Coalescing
                    };
            }
            // Input anticipates likely damage but is never proof pixels changed.
            WakeReason::InputAnticipation | WakeReason::FrameDeadline => {}
            WakeReason::Foregrounded => {}
        }

        if self.state == SchedulerState::FallbackFixedRate {
            if now_us.saturating_sub(self.last_capture_us) >= self.frame_interval_us {
                self.last_capture_us = now_us;
                return SchedulerAction::CaptureFixedRate;
            }
            return SchedulerAction::Wait;
        }
        if self.state == SchedulerState::RefreshDue
            || now_us.saturating_sub(self.last_full_refresh_us) >= self.full_refresh_interval_us
        {
            self.last_capture_us = now_us;
            self.last_full_refresh_us = now_us;
            self.damage_pending = false;
            self.state = SchedulerState::Static;
            return SchedulerAction::CaptureFull;
        }
        if self.damage_pending
            && now_us.saturating_sub(self.last_capture_us) >= self.frame_interval_us
        {
            self.last_capture_us = now_us;
            self.damage_pending = false;
            self.state = SchedulerState::Static;
            return SchedulerAction::CaptureDamage;
        }
        SchedulerAction::Wait
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Clip, merge and decide whether fragmented/large damage should become a full
/// capture. Returns `None` for full-frame fallback and an empty vector for no
/// valid damage.
pub fn coalesce_damage(
    rects: &[DamageRect],
    frame_width: u32,
    frame_height: u32,
    max_rects: usize,
    full_area_percent: u32,
) -> Option<Vec<DamageRect>> {
    if rects.len() > max_rects {
        return None;
    }
    let mut min_x = frame_width;
    let mut min_y = frame_height;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for rect in rects {
        let x2 = rect.x.saturating_add(rect.width).min(frame_width);
        let y2 = rect.y.saturating_add(rect.height).min(frame_height);
        let x = rect.x.min(frame_width);
        let y = rect.y.min(frame_height);
        if x >= x2 || y >= y2 {
            continue;
        }
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x2);
        max_y = max_y.max(y2);
        found = true;
    }
    if !found {
        return Some(Vec::new());
    }
    let union = DamageRect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    };
    let area = union.width as u64 * union.height as u64;
    let frame_area = frame_width as u64 * frame_height as u64;
    if frame_area == 0 || area * 100 >= frame_area * full_area_percent.clamp(1, 100) as u64 {
        None
    } else {
        Some(vec![union])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_waits_until_periodic_refresh() {
        let mut s = DamageScheduler::new(60, 1_000_000, true);
        assert_eq!(
            s.event(WakeReason::FrameDeadline, 10_000),
            SchedulerAction::Wait
        );
        assert_eq!(
            s.event(WakeReason::FrameDeadline, 1_000_000),
            SchedulerAction::CaptureFull
        );
    }

    #[test]
    fn damage_captures_immediately_then_coalesces_until_deadline() {
        let mut s = DamageScheduler::new(60, 1_000_000, true);
        assert_eq!(
            s.event(WakeReason::Damage, 20_000),
            SchedulerAction::CaptureDamage
        );
        assert_eq!(s.event(WakeReason::Damage, 21_000), SchedulerAction::Wait);
        assert_eq!(s.state, SchedulerState::Coalescing);
        assert_eq!(
            s.event(WakeReason::FrameDeadline, 37_000),
            SchedulerAction::CaptureDamage
        );
    }

    #[test]
    fn input_alone_does_not_capture() {
        let mut s = DamageScheduler::new(60, 1_000_000, true);
        assert_eq!(
            s.event(WakeReason::InputAnticipation, 20_000),
            SchedulerAction::Wait
        );
    }

    #[test]
    fn extension_failure_uses_fixed_rate() {
        let mut s = DamageScheduler::new(60, 1_000_000, true);
        assert_eq!(
            s.event(WakeReason::ExtensionFailed, 20_000),
            SchedulerAction::CaptureFixedRate
        );
    }

    #[test]
    fn fragmented_or_large_damage_becomes_full_frame() {
        let many = vec![
            DamageRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            };
            9
        ];
        assert_eq!(coalesce_damage(&many, 100, 100, 8, 50), None);
        assert_eq!(
            coalesce_damage(
                &[DamageRect {
                    x: 0,
                    y: 0,
                    width: 90,
                    height: 90
                }],
                100,
                100,
                8,
                50
            ),
            None
        );
    }

    #[test]
    fn rectangles_are_clipped_and_unioned() {
        let result = coalesce_damage(
            &[
                DamageRect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 20,
                },
                DamageRect {
                    x: 25,
                    y: 25,
                    width: 1000,
                    height: 1000,
                },
            ],
            100,
            100,
            8,
            95,
        )
        .unwrap();
        assert_eq!(
            result,
            vec![DamageRect {
                x: 10,
                y: 10,
                width: 90,
                height: 90
            }]
        );
    }
}
