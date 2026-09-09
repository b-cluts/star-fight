//! Scenarios: what the host chooses before a game — board size, obstacle
//! tokens, squad points, player count. Presets live in
//! `assets/data/scenarios.ron`; the host may tweak the numbers.

use serde::{Deserialize, Serialize};

use crate::board::Board;
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
        }
    }
}

/// Limits the server enforces on a host's setup.
pub const MAX_TOKENS: u8 = 12;
pub const POINTS_RANGE: (u32, u32) = (20, 400);
pub const BOARD_RANGE: (f64, f64) = (12.0, 40.0);

impl GameSetup {
    pub fn validate(&self) -> Result<(), String> {
        if self.players != 2 {
            return Err(format!("{}-player games are not supported yet (2 only)", self.players));
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

    /// One-line summary for lobbies and the HUD.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("{} pts", self.points)];
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
        assert!(GameSetup { players: 3, ..Default::default() }.validate().is_err());
        assert!(GameSetup { asteroids: 10, debris: 3, ..Default::default() }.validate().is_err());
        assert!(GameSetup { points: 10, ..Default::default() }.validate().is_err());
        assert!(GameSetup { board_width: 50.0, ..Default::default() }.validate().is_err());
        assert_eq!(GameSetup::default().obstacle_kinds().len(), 6);
        assert_eq!(GameSetup::default().summary(), "Standard dogfight (100 pts, 6 asteroids)");
    }
}
