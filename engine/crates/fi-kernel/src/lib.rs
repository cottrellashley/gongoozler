//! The core of the engine. It makes six assumptions and has four primitive objects
//! (`docs/spec.md`):
//!
//! * [`series::Series`]: typed, columnar values keyed by entity and event time, each with a
//!   knowledge time;
//! * [`entity::Entities`]: anything a series describes, interned to a dense index;
//! * [`model::Model`]: a declared, point-in-time transform from series to series;
//! * [`graph::Graph`]: models wired over series, evaluated in parallel and cached by content.
//!
//! And three built-in kinds of model:
//!
//! * [`contract::Contract`]: a per-unit state machine whose outputs are [`records::UnitRecords`];
//! * a portfolio: holdings in contracts, itself a contract ([`composite::Composite`]);
//! * the [`ledger`]: holdings × unit records + trades → P&L terms and NAV.
//!
//! Nothing here names a bond, a field, a calendar, a frequency, a rating scale or an optimiser.
//! This crate depends on neither Nautilus, Polars nor Python.

pub mod cash;
pub mod composite;
pub mod contract;
pub mod entity;
pub mod fill;
pub mod graph;
pub mod hash;
pub mod ledger;
pub mod measure;
pub mod model;
pub mod policy;
pub mod progress;
pub mod random;
pub mod records;
pub mod series;
pub mod time;
pub mod transition;
pub mod view;
pub mod workspace;

pub use cash::{Cash, Rounding, RoundingMode};
pub use composite::Composite;
pub use contract::{Contract, Deferred, Flows, InCurrency, Role, StepCtx, Stepper, Terms, UnitStep, ValuationState};
pub use entity::{Entities, EntityId};
pub use fill::{Fill, FillModel, Order, ResearchFill};
pub use graph::{Graph, GraphReport, Mode};
pub use ledger::{
    Advance, Agreement, AgreementKind, AgreementPath, Basis, Breach, BreachRecord, CapitalFlows, CashPolicy, Cause, Collected, Constraint, ConstraintScope,
    DecisionCtx, DecisionModel, Distribution, Distributor, DistributorRef, DistributorState, EachOrder, Entry, EntryKind, ExecRef, Execution, FlowCtx,
    FlowModel, FxConversion, Horizon, Ledger, LedgerConfig, LedgerOutput, LedgerState, OpeningDue, OrderRecord, OrderStats, OrderStatus, Phase, Policy,
    Response, Routing, Settlement, ShortPolicy, StepOrder, Targets, WorkingOrder,
};
pub use measure::{Face, FromSeries, Measure, Value};
pub use model::{BatchModel, CrossSectionModel, Input, Model, ModelSpec, Shape, TimeSeriesModel, Vintaged};
pub use policy::{Diagnostics, HoldingPolicy, Information, KnowledgePolicy, MissingPolicy, Outcomes};
pub use progress::{Progress, Reporter};
pub use records::UnitRecords;
pub use series::{Column, Hit, Intervention, KnowledgeBasis, Layout, ModelRole, Producer, Provenance, Series, Unit, Vintage};
pub use time::{Calendar, Ts};
pub use transition::Transition;
pub use view::View;
pub use workspace::{ModelRecord, Workspace};

use std::fmt;

/// Every fallible operation in the core fails fast with one of these (spec, principle 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The input breaks the contract: duplicate keys, wrong lengths, non-finite values.
    Invalid(String),
    /// A named series, column, entity or model does not exist.
    Unknown(String),
    /// A value needed by the run is missing, and the policy says fail.
    Missing(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid: {m}"),
            Self::Unknown(m) => write!(f, "unknown: {m}"),
            Self::Missing(m) => write!(f, "missing: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// The core's result type.
pub type Result<T> = std::result::Result<T, Error>;
