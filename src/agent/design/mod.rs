//! Scoped design knowledge, task state and honest visual evidence. No frontend-mode singleton.

pub mod guide;
pub mod prompt;
pub mod reference_compare;
pub mod reference_trace;
mod state;
mod tools;

pub use reference_compare::FidelityMode;
#[cfg(any(feature = "browser", test))]
pub use state::CaptureIdentity;
pub use state::Viewport;
#[cfg(test)]
pub use state::VisualEvidence;
pub use state::{
    DesignBrief, DesignContext, DesignIntent, DesignMode, EvidenceStatus, GateDecision,
    ReferenceObservationKind, TaskDesignState,
};
pub use tools::{
    DesignBriefTool, DesignGuideTool, DesignReviewTool, ReferenceCompareTool, ReferenceTraceTool,
};

#[cfg(test)]
mod loop_tests;
#[cfg(test)]
mod tests;
