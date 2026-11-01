// SPDX-License-Identifier: GPL-2.0-only OR MIT

//! Hardware configuration for T8140 (A18 Pro / G17P, board J700AP).
//!
//! # Grounding contract
//!
//! T8140 is an AGX3 part: its firmware does **not** consume the AGX2
//! `HwConfigA`/`HwConfigB`/shared-table/`io_mappings` init-data blocks at
//! all — the G17P init-data ABI is the genuinely different object graph in
//! [