use anyhow::Result;
use serde_json::Value;
use std::fmt;

#[derive(Debug)]
pub struct StatusFailed(pub Value);

impl fmt::Display for StatusFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Required services are not ready")
    }
}

impl std::error::Error for StatusFailed {}

#[derive(Debug)]
pub struct StatusReport(pub Value);

impl fmt::Display for StatusReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Status observations retained")
    }
}

impl std::error::Error for StatusReport {}

#[derive(Debug)]
pub struct StatusConfiguration;

impl fmt::Display for StatusConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Invalid status configuration")
    }
}

impl std::error::Error for StatusConfiguration {}

pub(crate) fn finish(report: Value) -> Result<Value> {
    if report["ready"] == true || report["inspectOnly"] == true {
        Ok(report)
    } else {
        Err(StatusFailed(report).into())
    }
}
