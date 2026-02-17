//! Moonbag Core - Shared types, models, and configuration
//!
//! This crate provides the foundational data structures used across
//! the moonbag trading system.

pub mod config;
pub mod error;
pub mod models;

pub use config::Config;
pub use error::{MoonbagError, Result};
pub use models::{
    ExecutionMode, ExecutionResult, MarketState, MoonbagOpportunity, OpportunityStatus,
    OutcomeState, PriceLevel, RemainingOutcome, SelectedOutcome, StoredMarket, StoredOutcome,
};
