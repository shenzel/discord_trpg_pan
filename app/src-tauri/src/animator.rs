//! ゆっくり移動。左右の値ではなく角度（と半径）で補間し、移動中も音量を一定に保つ。

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::pan::{Pan, Polar};

/// これ未満の変化は送らない（移動の途中だけ。終点は必ず送る）。
const SEND_EPSILON: f64 = 0.002;

struct Track {
    from: Polar,
    to: Polar,
    start: Instant,
    last_sent: Option<Pan>,
}

impl Track {
    fn progress(&self, now: Instant, duration: Duration) -> f64 {
        if duration.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.start).as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    fn value(&self, now: Instant, duration: Duration) -> Polar {
        let t = self.progress(now, duration);
        // smoothstep
        Polar::lerp(self.from, self.to, t * t * (3.0 - 2.0 * t))
    }
}

pub struct Animator {
    tracks: HashMap<String, Track>,
    duration: Duration,
}

impl Animator {
    pub fn new(duration: Duration) -> Self {
        Self { tracks: HashMap::new(), duration }
    }

    /// Discordから報告された今のpanを起点にする（入室時・VC取得時）。
    pub fn observe(&mut self, id: &str, pan: Pan, now: Instant) {
        let p = Polar::from_pan(pan);
        self.tracks.insert(id.to_string(), Track { from: p, to: p, start: now, last_sent: Some(pan) });
    }

    pub fn set_target(&mut self, id: &str, target: Polar, now: Instant) {
        let duration = self.duration;
        match self.tracks.get_mut(id) {
            Some(t) => {
                if !t.to.approx_eq(target) {
                    t.from = t.value(now, duration);
                    t.to = target;
                    t.start = now;
                }
            }
            None => {
                // 今の値が分からないときはDiscordの初期値から動かす
                let from = Polar::neutral();
                self.tracks.insert(id.to_string(), Track { from, to: target, start: now, last_sent: None });
            }
        }
    }

    /// 最後に送った pan（音量だけ変えたときに送り直す）
    pub fn last_sent(&self, id: &str) -> Option<Pan> {
        self.tracks.get(id)?.last_sent
    }

    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.tracks.retain(|id, _| keep(id));
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
    }

    /// 今送るべき (user_id, pan)。
    pub fn tick(&mut self, now: Instant) -> Vec<(String, Pan)> {
        let duration = self.duration;
        let mut out = Vec::new();
        for (id, t) in &mut self.tracks {
            let done = t.progress(now, duration) >= 1.0;
            let pan = t.value(now, duration).to_pan();
            let send = match t.last_sent {
                None => true,
                Some(last) if done => last != pan,
                Some(last) => last.max_diff(pan) >= SEND_EPSILON,
            };
            if send {
                t.last_sent = Some(pan);
                out.push((id.clone(), pan));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Duration = Duration::from_secs(3);

    #[test]
    fn moves_from_observed_to_target_over_duration() {
        let mut a = Animator::new(D);
        let t0 = Instant::now();
        a.observe("u", Pan::NEUTRAL, t0);
        assert!(a.tick(t0).is_empty(), "変化がなければ送らない");
        let target = Polar::from_position(-1.0);
        a.set_target("u", target, t0);
        let mid = a.tick(t0 + D / 2);
        assert_eq!(mid.len(), 1);
        assert!(mid[0].1.left > mid[0].1.right && mid[0].1.right > 0.3);
        let end = a.tick(t0 + D);
        assert_eq!(end, vec![("u".to_string(), target.to_pan())]);
        assert!(a.tick(t0 + D * 2).is_empty());
    }

    #[test]
    fn constant_power_while_moving_between_seats() {
        let mut a = Animator::new(D);
        let t0 = Instant::now();
        a.observe("u", Polar::from_position(-1.0).to_pan(), t0);
        a.set_target("u", Polar::from_position(1.0), t0);
        for i in 1..=30 {
            for (_, pan) in a.tick(t0 + D * i / 30) {
                assert!((pan.left.powi(2) + pan.right.powi(2) - 1.09).abs() < 1e-9, "{pan:?}");
            }
        }
    }

    #[test]
    fn retarget_midway_starts_from_current_value() {
        let mut a = Animator::new(D);
        let t0 = Instant::now();
        a.observe("u", Polar::from_position(-1.0).to_pan(), t0);
        a.set_target("u", Polar::from_position(1.0), t0);
        let mid = a.tick(t0 + D / 2)[0].1;
        a.set_target("u", Polar::from_position(-1.0), t0 + D / 2);
        // 切り替え直後に値が飛ばない
        let after = a.tick(t0 + D / 2 + Duration::from_millis(100));
        assert!(after.is_empty() || after[0].1.max_diff(mid) < 0.05);
    }

    #[test]
    fn unknown_member_sends_immediately() {
        let mut a = Animator::new(D);
        let t0 = Instant::now();
        a.set_target("u", Polar::center(), t0);
        assert_eq!(a.tick(t0).len(), 1);
    }
}
