//! Orchestration glue for `lumen serve`, exposed as a library so the
//! capture→encode→fan-out pipeline can be tested without a display.

pub mod pipeline;
