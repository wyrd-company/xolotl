//! Xolotl: an LLM subscription proxy and AI gateway backed by OpenBao.

pub mod catalog;
pub mod cli;
pub mod config;
pub mod db;
pub mod gateway;
pub mod keys;
pub mod limits;
pub mod mcp;
pub mod policy;
pub mod proxy;
pub mod secrets;
pub mod server;
pub mod subscription;
pub mod telemetry;
pub mod translate;
pub mod vault;
