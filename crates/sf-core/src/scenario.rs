//! Scenarios: what the host chooses before a game — board size, obstacle
//! tokens, squad points, player count. Presets live in
//! `assets/data/scenarios.ron`; the host may tweak the numbers.

use serde::{Deserialize, Serialize};

use crate::board::Board;
use crate::mission::MissionKind;
use crate::obstacle::ObstacleKind;

fn two() -> u8 {
    2
}
fn hundred() -> u32 {
    100
}
fn twenty() -> f64 {
    20.0
}

/// A preset from the data file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default = "two")]
    pub players: u8,
    #[serde(default = "hundred")]
    pub points: u32,
    #[serde(default)]
    pub asteroids: u8,
    #[serde(default)]
    pub debris: u8,
    #[serde(default)]
    pub black_holes: u8,
    #[serde(default = "twenty")]
    pub board_width: f64,
    #[serde(default = "twenty")]
    pub board_height: f64,
    /// Side (team) of each seat; empty = every seat its own side.
    #[serde(default)]
    pub teams: Vec<u8>,
    /// A rulebook mission (p.21-24): two sides, Rebels on side 0.
    #[serde(default)]
    pub mission: Option<MissionKind>,
}

/// What the host actually sends: a scenario name plus the numbers, which
/// may differ from the preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameSetup {
    pub scenario: String,
    pub players: u8,
    pub points: u32,
    pub asteroids: u8,
    pub debris: u8,
    #[serde(default)]
    pub black_holes: u8,
    pub board_width: f64,
    pub board_height: f64,
    /// Side (team) of each seat, `players` entries, ids 0..sides. Empty
    /// = every seat its own side (a free-for-all; the plain duel for 2).
    #[serde(default)]
    pub teams: Vec<u8>,
    /// A rulebook mission: side 0 flies Rebel squads, side 1 Imperial;
    /// the mission's setup, special rules and objectives apply.
    #[serde(default)]
    pub mission: Option<MissionKind>,
}

impl Default for GameSetup {
    fn default() -> Self {
        Self {
            scenario: "Standard dogfight".into(),
            players: 2,
            points: 100,
            asteroids: 6,
            debris: 0,
            black_holes: 0,
            board_width: 20.0,
            board_height: 20.0,
            teams: Vec::new(),
            mission: None,
        }
    }
}

impl From<&Scenario> for GameSetup {
    fn from(s: &Scenario) -> Self {
        Self {
            scenario: s.name.clone(),
            players: s.players,
            points: s.points,
            asteroids: s.asteroids,
            debris: s.debris,
            black_holes: s.black_holes,
            board_width: s.board_width,
            board_height: s.board_height,
            teams: s.teams.clone(),
            mission: s.mission,
        }
    }
}

/// Most seats a game can hold (one board edge per side).
pub const MAX_PLAYERS: u8 = 4;

/// Limits the server enforces on a host's setup.
pub const MAX_TOKENS: u8 = 12;
pub const POINTS_RANGE: (u32, u32) = (20, 400);
pub const BOARD_RANGE: (f64, f64) = (12.0, 40.0);

impl GameSetup {
    /// The side of `seat` (its own index when no teams are set).
    pub fn team_of(&self, seat: u8) -> u8 {
        self.teams.get(seat as usize).copied().unwrap_or(seat)
    }

    /// Side of every seat, `players` long.
    pub fn team_list(&self) -> Vec<u8> {
        (0..self.players).map(|s| self.team_of(s)).collect()
    }

    /// Number of sides.
    pub fn sides(&self) -> u8 {
        let mut ids: Vec<u8> = self.team_list();
        ids.sort_unstable();
        ids.dedup();
        ids.len() as u8
    }

    /// Seats on the side of `seat`.
    pub fn seats_on_side(&self, seat: u8) -> u8 {
        let t = self.team_of(seat);
        self.team_list().iter().filter(|x| **x == t).count() as u8
    }

    /// Squad points for one seat: every side gets `points`, shared out
    /// among its players (core rules p.20, team play).
    pub fn points_for_seat(&self, seat: u8) -> u32 {
        self.points / u32::from(self.seats_on_side(seat).max(1))
    }

    /// Two-sided game (teams) or every seat for itself?
    pub fn is_team_game(&self) -> bool {
        self.players > 2 && self.sides() == 2
    }

    /// Choose the mode for the current player count: two teams split as
    /// equally as possible (the odd player joins the first side alone),
    /// or a free-for-all with one side per seat.
    pub fn set_teams(&mut self, teams: bool) {
        self.teams = if teams && self.players > 2 {
            let half = self.players.div_ceil(2);
            (0..self.players).map(|s| u8::from(s >= half)).collect()
        } else {
            Vec::new()
        };
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(2..=MAX_PLAYERS).contains(&self.players) {
            return Err(format!(
                "{}-player games are not supported (2-{MAX_PLAYERS})",
                self.players
            ));
        }
        if !self.teams.is_empty() && self.teams.len() != self.players as usize {
            return Err("one team entry per player".into());
        }
        let sides = self.sides();
        if sides < 2 {
            return Err("at least two sides".into());
        }
        if self.team_list().iter().any(|t| *t >= sides) {
            return Err("team ids must be 0..sides".into());
        }
        if self.mission.is_some() && sides != 2 {
            return Err("missions are played between two sides".into());
        }
        if self.points < POINTS_RANGE.0 || self.points > POINTS_RANGE.1 {
            return Err(format!("squad points must be {}-{}", POINTS_RANGE.0, POINTS_RANGE.1));
        }
        if self.asteroids.saturating_add(self.debris).saturating_add(self.black_holes) > MAX_TOKENS
        {
            return Err(format!("at most {MAX_TOKENS} obstacle tokens"));
        }
        if self.black_holes > 2 {
            return Err("at most 2 black holes".into());
        }
        for d in [self.board_width, self.board_height] {
            if !(BOARD_RANGE.0..=BOARD_RANGE.1).contains(&d) || !d.is_finite() {
                return Err(format!(
                    "board sides must be {}-{} units",
                    BOARD_RANGE.0, BOARD_RANGE.1
                ));
            }
        }
        Ok(())
    }

    pub fn board(&self) -> Board {
        Board { width: self.board_width, height: self.board_height, deploy_depth: 3.0 }
    }

    /// Obstacle tokens to scatter, asteroids first.
    pub fn obstacle_kinds(&self) -> Vec<ObstacleKind> {
        let mut kinds = vec![ObstacleKind::BlackHole; self.black_holes as usize];
        kinds.extend(std::iter::repeat_n(ObstacleKind::Asteroid, self.asteroids as usize));
        kinds.extend(std::iter::repeat_n(ObstacleKind::Debris, self.debris as usize));
        kinds
    }

    /// "2 vs 2", "1 vs 2", "3-way free-for-all"…
    pub fn mode_name(&self) -> String {
        if self.players <= 2 {
            return "duel".into();
        }
        if self.sides() == 2 {
            let list = self.team_list();
            let a = list.iter().filter(|t| **t == 0).count();
            let b = list.len() - a;
            format!("{a} vs {b}")
        } else {
            format!("{}-way free-for-all", self.players)
        }
    }

    /// One-line summary for lobbies and the HUD.
    /// The faction a seat must fly in a mission (Rebels on side 0).
    pub fn faction_for_seat(&self, seat: u8) -> Option<crate::ship::Faction> {
        self.mission.map(|_| {
            if self.team_of(seat) == 0 {
                crate::ship::Faction::RebelAlliance
            } else {
                crate::ship::Faction::Empire
            }
        })
    }

    pub fn summary(&self) -> String {
        let mut parts = vec![format!("{} pts", self.points)];
        if let Some(m) = self.mission {
            parts.push(format!("mission {}", m.number()));
        }
        if self.players > 2 {
            parts.push(format!("{} players, {}", self.players, self.mode_name()));
        }
        match (self.asteroids, self.debris) {
            (0, 0) if self.black_holes == 0 => parts.push("open space".into()),
            (0, 0) => {}
            (a, 0) => parts.push(format!("{a} asteroids")),
            (0, d) => parts.push(format!("{d} debris")),
            (a, d) => parts.push(format!("{a} asteroids, {d} debris")),
        }
        match self.black_holes {
            0 => {}
            1 => parts.push("a black hole".into()),
            n => parts.push(format!("{n} black holes")),
        }
        if (self.board_width, self.board_height) != (20.0, 20.0) {
            parts.push(format!("{}x{} board", self.board_width, self.board_height));
        }
        format!("{} ({})", self.scenario, parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_limits() {
        assert!(GameSetup::default().validate().is_ok());
        assert!(GameSetup { players: 5, ..Default::default() }.validate().is_err());
        assert!(
            GameSetup { players: 3, teams: vec![0, 0, 0], ..Default::default() }
                .validate()
                .is_err()
        );
        let mut three = GameSetup { players: 3, ..Default::default() };
        three.set_teams(true);
        assert_eq!(three.teams, vec![0, 0, 1]);
        assert_eq!(three.mode_name(), "2 vs 1");
        assert_eq!((three.points_for_seat(0), three.points_for_seat(2)), (50, 100));
        three.set_teams(false);
        assert_eq!(three.sides(), 3);
        assert_eq!(three.points_for_seat(1), 100);
        assert!(three.validate().is_ok());
        let mut four = GameSetup { players: 4, ..Default::default() };
        four.set_teams(true);
        assert_eq!(four.teams, vec![0, 0, 1, 1]);
        assert!(four.is_team_game());
        assert!(four.validate().is_ok());
        assert!(GameSetup { asteroids: 10, debris: 3, ..Default::default() }.validate().is_err());
        assert!(GameSetup { points: 10, ..Default::default() }.validate().is_err());
        assert!(GameSetup { board_width: 50.0, ..Default::default() }.validate().is_err());
        assert_eq!(GameSetup::default().obstacle_kinds().len(), 6);
        assert_eq!(GameSetup::default().summary(), "Standard dogfight (100 pts, 6 asteroids)");
    }
}
