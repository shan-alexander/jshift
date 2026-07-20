//! Span / arena evaluation model (design contract for the projector).
//!
//! # Model
//!
//! * A **span** is a half-open byte range into either the **input document** or a
//!   short-lived intermediate buffer (synthesized multi-select / function results).
//! * Pure navigation (`field`, `index`, identity, pipe of pure paths) resolves to
//!   **document spans** — zero intermediate tree allocation. See
//!   `emit::resolve_focus_idx` / `resolve_focus_start_idx`.
//! * Synthesized values materialize once into a temporary `Vec` / reclaimable buffer;
//!   pure right-hand sides then walk those bytes as spans.
//! * Final output is streamed via [`crate::project::sink::EmitOut`] (not an arena).
//!
//! Emit implements this model inside [`crate::project::emit::EmitCtx`] (plan, depth,
//! optional [`crate::IndexedDocument`]). This module documents the contract so the
//! product story stays explicit without a second parallel state machine.
//!
//! This is jshift’s byte-oriented JMESPath design — not a DOM clone of
//! [jmespath.rs](https://github.com/jmespath/jmespath.rs).
