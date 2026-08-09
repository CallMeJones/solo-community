// SPDX-License-Identifier: Apache-2.0

//! Subcommand implementations. Each module corresponds to one
//! `Cli::command` arm.

pub mod audit;
pub mod backup;
pub mod common;
pub mod consolidate;
pub mod daemon;
pub mod doctor;
pub mod documents;
pub mod embedder_migrate;
pub mod entity_split_review;
pub mod eval;
pub mod forget;
pub mod gdpr;
pub mod http_serve;
pub mod import;
pub mod ingest;
pub mod init;
pub mod inspect;
pub mod mcp_stdio;
pub mod normalize_subjects;
pub mod project;
pub mod recall;
pub mod reembed;
pub mod remember;
pub mod restore;
pub mod setup_client;
