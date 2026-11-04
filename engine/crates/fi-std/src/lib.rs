//! Standard definitions on the kernel: simple contracts, example models, decision models and a
//! synthetic universe. Everything here is a definition built from the core objects; none of it is
//! part of the core.

pub mod contracts;
pub mod debt;
pub mod decisions;
pub mod models;
pub mod structured;
pub mod synth;
pub mod worlds;

pub use contracts::{CashAccount, DailyFuture, SuppliedBond, TotalReturn};
pub use debt::{Compounding, DebtContract, Spread, Valuation};
pub use decisions::{Branch, ExitAbove, Reinvest, Replay, RiskParity, RiskParityMethod, TopN, Trades};
pub use models::{EwmaVol, Merton, NelsonSiegel, ZScore, merton, norm_cdf};
pub use worlds::{Defaults, GaussianPaths, LoanTerms};
