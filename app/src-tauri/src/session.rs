//! 卓の状態（役職・フェーズ・並び順・在室状況）と、そこから決まる各人のpan目標。I/Oを持たない。
//!
//! 保持するのは位置ではなく並び順（seat_order）。位置は数える席の数から毎回計算する。

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::pan::{seat_positions_in, Polar};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Pl,
    Gm,
    Spectator,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// シナリオ前
    #[default]
    Before,
    /// シナリオ中
    During,
    /// 感想戦
    After,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Pl => "pl",
            Role::Gm => "gm",
            Role::Spectator => "spectator",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        [Role::Pl, Role::Gm, Role::Spectator].into_iter().find(|r| r.as_str() == s)
    }
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Before => "before",
            Phase::During => "during",
            Phase::After => "after",
        }
    }

    pub fn parse(s: &str) -> Option<Phase> {
        [Phase::Before, Phase::During, Phase::After].into_iter().find(|p| p.as_str() == s)
    }

    pub fn seats(self, role: Role) -> bool {
        match self {
            Phase::Before => false,
            Phase::During => role == Role::Pl,
            Phase::After => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Presence {
    Present,
    /// 退出して猶予中。期限まで席を空けて待つ。
    Grace(Instant),
    /// 自分がVCを移動・退出したため、今いるVCにいない人。席は詰めるが並び順は覚えておき、戻れば元の順番に復帰する。
    Away,
    /// 猶予切れ。役職だけ覚えておく。
    Gone,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub presence: Presence,
    seq: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Channel {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Session {
    phase: Phase,
    channel: Option<Channel>,
    members: HashMap<String, Member>,
    seat_order: Vec<String>,
    next_seq: u64,
    grace: Duration,
    /// GM から届いた役職のうち、まだ VC に来ていない人の分。入室したときに使う。
    pending_roles: HashMap<String, Role>,
    /// 席を並べる範囲（2垢モードでは左半分・右半分）。既定は全幅
    range: (f64, f64),
}

impl Session {
    pub fn new(grace: Duration) -> Self {
        Self {
            phase: Phase::Before,
            channel: None,
            members: HashMap::new(),
            seat_order: Vec::new(),
            next_seq: 0,
            grace,
            pending_roles: HashMap::new(),
            range: (-1.0, 1.0),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn channel(&self) -> Option<&Channel> {
        self.channel.as_ref()
    }

    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.get(id)
    }

    /// 一覧に出す人（猶予切れを除く）を入室順で。
    pub fn visible_members(&self) -> Vec<&Member> {
        let mut v: Vec<_> = self.members.values().filter(|m| m.presence != Presence::Gone).collect();
        v.sort_by_key(|m| m.seq);
        v
    }

    fn default_role(&self) -> Role {
        if self.phase == Phase::Before {
            Role::Pl
        } else {
            Role::Spectator
        }
    }

    pub fn join(&mut self, id: &str, name: &str) {
        if !self.members.contains_key(id) {
            let seq = self.next_seq;
            self.next_seq += 1;
            let role = self.pending_roles.remove(id).unwrap_or_else(|| self.default_role());
            self.members.insert(
                id.to_string(),
                Member { id: id.to_string(), name: String::new(), role, presence: Presence::Gone, seq },
            );
        }
        let m = self.members.get_mut(id).unwrap();
        if !name.is_empty() {
            m.name = name.to_string();
        }
        m.presence = Presence::Present;
        let role = m.role;
        if self.phase.seats(role) && !self.seat_order.iter().any(|s| s == id) {
            self.seat_order.push(id.to_string());
        }
    }

    pub fn update_name(&mut self, id: &str, name: &str) -> bool {
        match self.members.get_mut(id) {
            Some(m) if !name.is_empty() && m.name != name => {
                m.name = name.to_string();
                true
            }
            _ => false,
        }
    }

    pub fn leave(&mut self, id: &str, now: Instant) {
        if let Some(m) = self.members.get_mut(id) {
            if m.presence == Presence::Present {
                m.presence = Presence::Grace(now + self.grace);
            }
        }
    }

    /// 猶予切れの人を詰める。変化があれば true。
    pub fn tick(&mut self, now: Instant) -> bool {
        let expired: Vec<String> = self
            .members
            .values()
            .filter(|m| matches!(m.presence, Presence::Grace(t) if t <= now))
            .map(|m| m.id.clone())
            .collect();
        for id in &expired {
            self.members.get_mut(id).unwrap().presence = Presence::Gone;
        }
        self.seat_order.retain(|id| !expired.contains(id));
        !expired.is_empty()
    }

    pub fn has_grace(&self) -> bool {
        self.members.values().any(|m| matches!(m.presence, Presence::Grace(_)))
    }

    /// 自分の今いるVCと、その参加者（自分を除く）を反映する。
    /// 同じVCの取り直し（再接続など）で消えていた人は退出扱い、VCが変わったなら Away。
    pub fn set_channel(&mut self, channel: Option<Channel>, present: &[(String, String)], now: Instant) {
        let same = channel.is_some() && self.channel.as_ref().map(|c| &c.id) == channel.as_ref().map(|c| &c.id);
        let ids: HashSet<&str> = present.iter().map(|(id, _)| id.as_str()).collect();
        let grace_until = now + self.grace;
        for m in self.members.values_mut() {
            if m.presence == Presence::Present && !ids.contains(m.id.as_str()) {
                m.presence = if same { Presence::Grace(grace_until) } else { Presence::Away };
            }
        }
        self.channel = channel;
        for (id, name) in present {
            self.join(id, name);
        }
    }

    fn ids_by_seq(&self, role: Role) -> Vec<String> {
        let mut v: Vec<&Member> =
            self.members.values().filter(|m| m.role == role && m.presence != Presence::Gone).collect();
        v.sort_by_key(|m| m.seq);
        v.into_iter().map(|m| m.id.clone()).collect()
    }

    pub fn set_phase(&mut self, phase: Phase) {
        let prev = self.phase;
        if prev == phase {
            return;
        }
        self.phase = phase;
        match (prev, phase) {
            (_, Phase::Before) => self.seat_order.clear(),
            (Phase::Before, Phase::During) => self.seat_order = self.ids_by_seq(Role::Pl),
            (Phase::Before, Phase::After) => {
                self.seat_order =
                    [Role::Pl, Role::Gm, Role::Spectator].into_iter().flat_map(|r| self.ids_by_seq(r)).collect();
            }
            (Phase::After, Phase::During) => {
                let members = &self.members;
                self.seat_order.retain(|id| members[id].role == Role::Pl);
            }
            (Phase::During, Phase::After) => {
                // PLの今の並びの右側に、GM→観戦者の順で追加
                for role in [Role::Gm, Role::Spectator] {
                    for id in self.ids_by_seq(role) {
                        if !self.seat_order.contains(&id) {
                            self.seat_order.push(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn set_role(&mut self, id: &str, role: Role) {
        let Some(m) = self.members.get_mut(id) else { return };
        if m.role == role {
            return;
        }
        m.role = role;
        if m.presence == Presence::Gone {
            return;
        }
        let seated = self.phase.seats(role);
        let had_seat = self.seat_order.iter().any(|s| s == id);
        if !seated {
            self.seat_order.retain(|s| s != id);
        } else if role == Role::Pl || !had_seat {
            // PLになった人は右端へ
            self.seat_order.retain(|s| s != id);
            self.seat_order.push(id.to_string());
        }
    }

    /// 並び順の変更（index は「数える席」の中での位置）。
    pub fn reorder(&mut self, id: &str, index: usize) {
        let counted = self.counted_seats();
        let Some(from) = self.seat_order.iter().position(|s| s == id) else { return };
        if !counted.iter().any(|s| s == id) {
            return;
        }
        self.seat_order.remove(from);
        let others: Vec<&String> = counted.iter().filter(|s| *s != id).collect();
        let insert_at = match others.get(index) {
            Some(anchor) => self.seat_order.iter().position(|s| s == *anchor).unwrap(),
            None => others
                .last()
                .map(|last| self.seat_order.iter().position(|s| s == *last).unwrap() + 1)
                .unwrap_or(self.seat_order.len()),
        };
        self.seat_order.insert(insert_at, id.to_string());
    }

    /// シナリオをリセット：フェーズを戻し、役職の割り当ても捨てる。
    /// GM の卓の役職表をまとめて反映する（同期）。VC にまだいない人の分は入室時に使う。
    pub fn apply_roles(&mut self, roles: &HashMap<String, Role>) -> bool {
        let mut changed = false;
        for (id, &role) in roles {
            match self.members.get(id) {
                Some(m) if m.role != role => {
                    self.set_role(id, role);
                    changed = true;
                }
                Some(_) => {}
                None => {
                    self.pending_roles.insert(id.clone(), role);
                }
            }
        }
        changed
    }

    /// 今の役職表（猶予切れを除く）。GM のアプリが同期のために配る。
    pub fn roles(&self) -> HashMap<String, Role> {
        self.members.values().filter(|m| m.presence != Presence::Gone).map(|m| (m.id.clone(), m.role)).collect()
    }

    pub fn reset(&mut self) {
        self.phase = Phase::Before;
        self.seat_order.clear();
        self.members.retain(|_, m| m.presence != Presence::Gone);
        self.pending_roles.clear();
        for m in self.members.values_mut() {
            m.role = Role::Pl;
        }
    }

    fn counts_seat(&self, m: &Member) -> bool {
        matches!(m.presence, Presence::Present | Presence::Grace(_))
    }

    /// 位置の計算に数える席（空席を含む）を並び順で。
    pub fn counted_seats(&self) -> Vec<String> {
        self.seat_order.iter().filter(|id| self.counts_seat(&self.members[*id])).cloned().collect()
    }

    /// 席を並べる範囲を変える（2垢モード）。変わったら true。
    pub fn set_range(&mut self, lo: f64, hi: f64) -> bool {
        let changed = self.range != (lo, hi);
        self.range = (lo, hi);
        changed
    }

    /// 各人の位置 p（−1〜+1 のうち、範囲の中）。席なしは None。
    pub fn positions(&self) -> HashMap<String, f64> {
        let seats = self.counted_seats();
        let (lo, hi) = self.range;
        seats.iter().cloned().zip(seat_positions_in(seats.len(), lo, hi)).collect()
    }

    /// 在室している人のpan目標。
    pub fn targets(&self) -> HashMap<String, Polar> {
        let positions = self.positions();
        self.members
            .values()
            .filter(|m| m.presence == Presence::Present)
            .map(|m| {
                let t = match self.phase {
                    Phase::Before => Polar::neutral(),
                    _ => positions.get(&m.id).map_or_else(Polar::center, |&p| Polar::from_position(p)),
                };
                (m.id.clone(), t)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: Duration = Duration::from_secs(60);

    fn s() -> Session {
        Session::new(G)
    }

    fn ch(id: &str) -> Option<Channel> {
        Some(Channel { id: id.into(), name: id.into() })
    }

    fn people(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter().map(|i| (i.to_string(), i.to_uppercase())).collect()
    }

    fn pos(s: &Session) -> Vec<(String, f64)> {
        let p = s.positions();
        let mut v: Vec<_> = p.into_iter().collect();
        v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        v
    }

    fn order(s: &Session) -> Vec<String> {
        s.counted_seats()
    }

    #[test]
    fn before_phase_is_neutral_and_default_pl() {
        let mut s = s();
        let now = Instant::now();
        s.set_channel(ch("vc"), &people(&["a", "b"]), now);
        assert!(s.positions().is_empty());
        assert!(s.targets().values().all(|t| t.approx_eq(Polar::neutral())));
        assert_eq!(s.member("a").unwrap().role, Role::Pl);
    }

    #[test]
    fn during_seats_only_pl_and_late_joiner_is_spectator() {
        let mut s = s();
        let now = Instant::now();
        s.set_channel(ch("vc"), &people(&["a", "g", "b"]), now);
        s.set_role("g", Role::Gm);
        s.set_phase(Phase::During);
        assert_eq!(order(&s), vec!["a", "b"]);
        assert_eq!(pos(&s), vec![("a".into(), -1.0), ("b".into(), 1.0)]);
        assert!(s.targets()["g"].approx_eq(Polar::center()));
        s.join("late", "LATE");
        assert_eq!(s.member("late").unwrap().role, Role::Spectator);
        assert_eq!(order(&s), vec!["a", "b"]);
    }

    #[test]
    fn during_to_after_appends_gm_then_spectators() {
        let mut s = s();
        let now = Instant::now();
        s.set_channel(ch("vc"), &people(&["sp1", "a", "g", "b"]), now);
        s.set_role("g", Role::Gm);
        s.set_role("sp1", Role::Spectator);
        s.set_phase(Phase::During);
        s.reorder("b", 0);
        assert_eq!(order(&s), vec!["b", "a"]);
        s.join("sp2", "SP2");
        s.set_phase(Phase::After);
        assert_eq!(order(&s), vec!["b", "a", "g", "sp1", "sp2"]);
        // 押し間違いで戻す：役職はそのまま、PLの並びも保つ
        s.set_phase(Phase::During);
        assert_eq!(order(&s), vec!["b", "a"]);
        assert_eq!(s.member("g").unwrap().role, Role::Gm);
    }

    #[test]
    fn before_to_after_seats_everyone() {
        let mut s = s();
        s.set_channel(ch("vc"), &people(&["g", "a", "b"]), Instant::now());
        s.set_role("g", Role::Gm);
        s.set_phase(Phase::After);
        assert_eq!(order(&s), vec!["a", "b", "g"]);
    }

    #[test]
    fn back_to_before_keeps_roles_and_resets_pan() {
        let mut s = s();
        s.set_channel(ch("vc"), &people(&["g", "a"]), Instant::now());
        s.set_role("g", Role::Gm);
        s.set_phase(Phase::During);
        s.set_phase(Phase::Before);
        assert!(s.targets().values().all(|t| t.approx_eq(Polar::neutral())));
        assert_eq!(s.member("g").unwrap().role, Role::Gm);
    }

    #[test]
    fn reset_discards_roles() {
        let mut s = s();
        s.set_channel(ch("vc"), &people(&["g", "a"]), Instant::now());
        s.set_role("g", Role::Gm);
        s.set_phase(Phase::During);
        s.reset();
        assert_eq!(s.phase(), Phase::Before);
        assert_eq!(s.member("g").unwrap().role, Role::Pl);
    }

    #[test]
    fn becoming_pl_goes_to_right_end() {
        let mut s = s();
        s.set_channel(ch("vc"), &people(&["a", "x", "b"]), Instant::now());
        s.set_role("x", Role::Spectator);
        s.set_phase(Phase::During);
        assert_eq!(order(&s), vec!["a", "b"]);
        s.set_role("x", Role::Pl);
        assert_eq!(order(&s), vec!["a", "b", "x"]);
        s.set_role("a", Role::Gm);
        assert_eq!(order(&s), vec!["b", "x"]);
        // 感想戦：GM→PL は右端へ移動、PL→観戦者は席を保つ
        s.set_phase(Phase::After);
        assert_eq!(order(&s), vec!["b", "x", "a"]);
        s.set_role("b", Role::Spectator);
        assert_eq!(order(&s), vec!["b", "x", "a"]);
        s.set_role("b", Role::Pl);
        assert_eq!(order(&s), vec!["x", "a", "b"]);
    }

    #[test]
    fn grace_keeps_seat_then_compacts() {
        let mut s = s();
        let t0 = Instant::now();
        s.set_channel(ch("vc"), &people(&["a", "b", "c"]), t0);
        s.set_phase(Phase::During);
        s.leave("b", t0);
        // 猶予中は席を空けたまま：位置は変わらない
        assert_eq!(pos(&s).len(), 3);
        assert!(!s.targets().contains_key("b"));
        assert!(!s.tick(t0 + G / 2));
        // 猶予内の再入室は元の位置に復帰
        s.join("b", "B");
        assert_eq!(order(&s), vec!["a", "b", "c"]);
        // 猶予切れで詰める
        s.leave("b", t0);
        assert!(s.tick(t0 + G));
        assert_eq!(order(&s), vec!["a", "c"]);
        assert_eq!(pos(&s), vec![("a".into(), -1.0), ("c".into(), 1.0)]);
        // 猶予切れ後の再入室は右端（役職は保持）
        s.join("b", "B");
        assert_eq!(order(&s), vec!["a", "c", "b"]);
        assert_eq!(s.member("b").unwrap().role, Role::Pl);
    }

    #[test]
    fn same_channel_refresh_treats_missing_as_leave() {
        let mut s = s();
        let t0 = Instant::now();
        s.set_channel(ch("vc"), &people(&["a", "b"]), t0);
        s.set_channel(ch("vc"), &people(&["a"]), t0);
        assert!(matches!(s.member("b").unwrap().presence, Presence::Grace(_)));
    }

    #[test]
    fn vc_move_compacts_and_keeps_order() {
        let mut s = s();
        let t0 = Instant::now();
        s.set_channel(ch("vc1"), &people(&["a", "b", "c"]), t0);
        s.set_phase(Phase::During);
        s.set_channel(ch("vc2"), &people(&["c", "a"]), t0);
        assert_eq!(s.member("b").unwrap().presence, Presence::Away);
        // 移動先にいない b の分は詰める（並びは移動先の入室順ではなく元の順）
        assert_eq!(order(&s), vec!["a", "c"]);
        assert_eq!(pos(&s), vec![("a".into(), -1.0), ("c".into(), 1.0)]);
        // 猶予で消えることはなく、来たら元の順番に復帰
        assert!(!s.tick(t0 + G * 10));
        s.join("b", "B");
        assert_eq!(order(&s), vec!["a", "b", "c"]);
        // 自分がVCを抜けても役職と並びは保持
        s.set_channel(None, &[], t0);
        assert!(s.targets().is_empty());
        s.set_channel(ch("vc1"), &people(&["a", "b", "c"]), t0);
        assert_eq!(order(&s), vec!["a", "b", "c"]);
    }

    #[test]
    fn apply_roles_from_gm() {
        let mut s = s();
        s.set_channel(ch("vc"), &people(&["a", "b"]), Instant::now());
        s.set_phase(Phase::During);
        let roles: HashMap<String, Role> =
            [("a".to_string(), Role::Gm), ("late".to_string(), Role::Pl)].into_iter().collect();
        assert!(s.apply_roles(&roles));
        assert_eq!(s.member("a").unwrap().role, Role::Gm);
        assert_eq!(order(&s), vec!["b"]);
        // まだ来ていない人は、入室したときに GM の役職で入る（シナリオ中の既定値の観戦者ではなく）
        s.join("late", "LATE");
        assert_eq!(s.member("late").unwrap().role, Role::Pl);
        assert_eq!(order(&s), vec!["b", "late"]);
        assert!(!s.apply_roles(&roles), "同じ内容なら変化なし");
        assert_eq!(s.roles().len(), 3);
    }

    #[test]
    fn range_for_dual_mode() {
        let mut s = s();
        s.set_channel(ch("vc"), &people(&["a", "b"]), Instant::now());
        s.set_phase(Phase::During);
        assert!(s.set_range(-1.0, -0.2));
        let p = pos(&s);
        assert_eq!(p[0], ("a".into(), -1.0));
        assert!(p[1].0 == "b" && (p[1].1 + 0.2).abs() < 1e-9, "{p:?}");
        assert!(!s.set_range(-1.0, -0.2), "同じ範囲なら変化なし");
        s.set_role("b", Role::Gm);
        // 1 人なら範囲の中点
        assert_eq!(pos(&s), vec![("a".into(), -0.6)]);
    }

    #[test]
    fn role_and_phase_strings_roundtrip() {
        for r in [Role::Pl, Role::Gm, Role::Spectator] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        for p in [Phase::Before, Phase::During, Phase::After] {
            assert_eq!(Phase::parse(p.as_str()), Some(p));
        }
        assert_eq!(Role::parse("x"), None);
    }

    #[test]
    fn reorder_within_counted_seats() {
        let mut s = s();
        let t0 = Instant::now();
        s.set_channel(ch("vc1"), &people(&["a", "b", "c", "d"]), t0);
        s.set_phase(Phase::During);
        s.set_channel(ch("vc2"), &people(&["a", "c", "d"]), t0);
        s.reorder("d", 0);
        assert_eq!(order(&s), vec!["d", "a", "c"]);
        s.reorder("d", 5);
        assert_eq!(order(&s), vec!["a", "c", "d"]);
        s.join("b", "B");
        assert_eq!(order(&s), vec!["a", "b", "c", "d"]);
    }
}
