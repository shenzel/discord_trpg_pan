//! pan値と位置の変換。
//!
//! pan は極座標 (θ, r) で扱う。left = r·cosθ, right = r·sinθ。
//! 席の位置 p（左端 −1 〜 右端 +1）は等パワー則で θ に写し、r は一定（left² + right² = 1.09）。

use serde::Serialize;
use std::f64::consts::{FRAC_PI_4, SQRT_2};

/// 端の小さい側の値（完全な片耳にしない）。
pub const EDGE_RATIO: f64 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Pan {
    pub left: f64,
    pub right: f64,
}

impl Pan {
    pub const NEUTRAL: Pan = Pan { left: 1.0, right: 1.0 };

    pub fn max_diff(self, other: Pan) -> f64 {
        (self.left - other.left).abs().max((self.right - other.right).abs())
    }

    /// 人ごとの音量の倍率を掛ける（1 を超えてもよい。ミキサー側で音割れ前に止める）
    pub fn scaled(self, volume: f64) -> Pan {
        Pan { left: self.left * volume, right: self.right * volume }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Polar {
    pub theta: f64,
    pub r: f64,
}

fn edge_angle() -> f64 {
    EDGE_RATIO.atan()
}

/// 席がある人の半径。1/cos(arctan 0.3) ≈ 1.044、r² = 1.09。
pub fn seated_radius() -> f64 {
    1.0 / edge_angle().cos()
}

impl Polar {
    /// 位置 p（−1〜+1）の席。
    pub fn from_position(p: f64) -> Self {
        Polar { theta: FRAC_PI_4 + p * (FRAC_PI_4 - edge_angle()), r: seated_radius() }
    }

    /// 席なしの人（中央 {0.74, 0.74}）。
    pub fn center() -> Self {
        Self::from_position(0.0)
    }

    /// Discord の初期値 {1.0, 1.0}。
    pub fn neutral() -> Self {
        Polar { theta: FRAC_PI_4, r: SQRT_2 }
    }

    pub fn from_pan(pan: Pan) -> Self {
        let r = pan.left.hypot(pan.right);
        if r < 1e-9 {
            return Polar { theta: FRAC_PI_4, r: 0.0 };
        }
        Polar { theta: pan.right.atan2(pan.left), r }
    }

    pub fn to_pan(self) -> Pan {
        Pan {
            left: (self.r * self.theta.cos()).clamp(0.0, 1.0),
            right: (self.r * self.theta.sin()).clamp(0.0, 1.0),
        }
    }

    pub fn lerp(a: Polar, b: Polar, t: f64) -> Polar {
        Polar { theta: a.theta + (b.theta - a.theta) * t, r: a.r + (b.r - a.r) * t }
    }

    pub fn approx_eq(self, other: Polar) -> bool {
        (self.theta - other.theta).abs() < 1e-9 && (self.r - other.r).abs() < 1e-9
    }
}

/// n 人を [lo, hi] に等間隔で並べる。1人なら中点。
pub fn seat_positions_in(n: usize, lo: f64, hi: f64) -> Vec<f64> {
    match n {
        0 => vec![],
        1 => vec![(lo + hi) / 2.0],
        _ => (0..n).map(|i| lo + (hi - lo) * i as f64 / (n - 1) as f64).collect(),
    }
}

/// 全幅（−1〜+1）に n 人を並べる。i 番目は −1 + 2i/(n−1)。
pub fn seat_positions(n: usize) -> Vec<f64> {
    seat_positions_in(n, -1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn edges_and_center() {
        let l = Polar::from_position(-1.0).to_pan();
        assert!(close(l.left, 1.0) && close(l.right, 0.3), "{l:?}");
        let r = Polar::from_position(1.0).to_pan();
        assert!(close(r.left, 0.3) && close(r.right, 1.0), "{r:?}");
        let c = Polar::center().to_pan();
        assert!(close(c.left, c.right));
        assert!((c.left - 0.74).abs() < 0.005, "{c:?}");
    }

    #[test]
    fn equal_power_everywhere() {
        for i in 0..=20 {
            let p = -1.0 + i as f64 / 10.0;
            let pan = Polar::from_position(p).to_pan();
            assert!(close(pan.left.powi(2) + pan.right.powi(2), 1.09), "p={p} {pan:?}");
        }
    }

    #[test]
    fn neutral_roundtrip() {
        let n = Polar::neutral().to_pan();
        assert!(close(n.left, 1.0) && close(n.right, 1.0));
        let back = Polar::from_pan(Pan::NEUTRAL);
        assert!(back.approx_eq(Polar::neutral()));
    }

    #[test]
    fn positions() {
        assert_eq!(seat_positions(0), Vec::<f64>::new());
        assert_eq!(seat_positions(1), vec![0.0]);
        assert_eq!(seat_positions(3), vec![-1.0, 0.0, 1.0]);
        let four = seat_positions(4);
        assert!(close(four[1], -1.0 / 3.0) && close(four[2], 1.0 / 3.0));
        // 2分割：左領域 −1〜−g、右領域 +g〜+1
        assert_eq!(seat_positions_in(2, 0.2, 1.0), vec![0.2, 1.0]);
        assert_eq!(seat_positions_in(1, -1.0, -0.2), vec![-0.6]);
    }

    #[test]
    fn interpolation_keeps_power_between_seats() {
        let a = Polar::from_position(-1.0);
        let b = Polar::from_position(1.0);
        for i in 0..=10 {
            let pan = Polar::lerp(a, b, i as f64 / 10.0).to_pan();
            assert!(close(pan.left.powi(2) + pan.right.powi(2), 1.09));
        }
    }
}
