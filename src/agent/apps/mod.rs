//! Creative-production foundations. No app tool or live adapter is registered by this module.
//!
//! Contract validation is not enrollment. Durable intent is not approval, and a job's terminal
//! execution state is not evidence that external effects have settled. Keep these modules inert
//! until authenticated adapter dispatch and whole-effect resource admission are integrated.

// Dispatch is intentionally dormant until the controller's lifetime-admission gate exists.
#[allow(dead_code)]
pub(crate) mod contract;
#[allow(dead_code)]
pub(crate) mod jobs;
pub(crate) mod status;
