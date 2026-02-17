//! Moonbag Analyzer - VWAP calculation and opportunity detection
//!
//! This crate provides the mathematical analysis for finding
//! profitable moonbag opportunities.

pub mod gas;
pub mod opportunity;
pub mod quick_check;
pub mod vwap;

pub use opportunity::MoonbagAnalyzer;
pub use quick_check::{quick_check, quick_check_with_best_profit};
pub use vwap::{calculate_vwap, calculate_vwap_and_impact};
