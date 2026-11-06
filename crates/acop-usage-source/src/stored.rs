use acop::{
    Codec,
    store::{
        SqliteUsageSpec,
        sqlite_usage::{SqliteUsageReadFailure, SqliteUsageSnapshot, SqliteUsageVisitFailure},
    },
};
use metera_sdk::{
    Kernel,
    store::{AdvanceInput, SessionUsage, UsageAdvance},
};
use std::path::Path;

const BATCH_RECORDS: usize = 256;

#[derive(Debug)]
pub enum StoreSourceError {
    Read(SqliteUsageReadFailure),
    Kernel(metera_sdk::Error),
}
impl std::fmt::Display for StoreSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => std::fmt::Display::fmt(error, f),
            Self::Kernel(error) => std::fmt::Display::fmt(error, f),
        }
    }
}
impl std::error::Error for StoreSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Kernel(error) => Some(error),
        }
    }
}
impl From<SqliteUsageReadFailure> for StoreSourceError {
    fn from(value: SqliteUsageReadFailure) -> Self {
        Self::Read(value)
    }
}
impl From<metera_sdk::Error> for StoreSourceError {
    fn from(value: metera_sdk::Error) -> Self {
        Self::Kernel(value)
    }
}

pub struct StoredSource {
    spec: &'static SqliteUsageSpec,
    kernel: Kernel,
}
impl StoredSource {
    pub fn new(spec: &'static SqliteUsageSpec) -> Result<Self, StoreSourceError> {
        Ok(Self {
            spec,
            kernel: Kernel::bundled()?,
        })
    }
    pub fn of(codec: &dyn Codec) -> Result<Option<Self>, StoreSourceError> {
        codec.stored_usage().map(Self::new).transpose()
    }
    pub fn collect(&mut self, path: &Path) -> Result<Vec<SessionUsage>, StoreSourceError> {
        let snapshot = SqliteUsageSnapshot::open(path)?;
        let schema_plan = self.kernel.store_schema(self.spec)?;
        let schema = snapshot.capture_schema(&schema_plan)?;
        let data_plan = self.kernel.store_plan(self.spec, &schema)?;
        let mut pending = Vec::with_capacity(BATCH_RECORDS);
        let mut sessions = Vec::new();
        snapshot
            .visit_records(&data_plan, |record| {
                pending.push(record);
                if pending.len() == BATCH_RECORDS {
                    sessions.extend(self.kernel.store_normalize(self.spec, &pending)?);
                    pending.clear();
                }
                Ok::<_, metera_sdk::Error>(())
            })
            .map_err(|error| match error {
                SqliteUsageVisitFailure::Read(error) => StoreSourceError::Read(error),
                SqliteUsageVisitFailure::Consumer(error) => StoreSourceError::Kernel(error),
            })?;
        if !pending.is_empty() {
            sessions.extend(self.kernel.store_normalize(self.spec, &pending)?);
        }
        Ok(sessions)
    }
    pub fn advance(
        &mut self,
        series: &[AdvanceInput],
    ) -> Result<Vec<UsageAdvance>, StoreSourceError> {
        self.kernel
            .store_advance(series)
            .map_err(StoreSourceError::Kernel)
    }
}
#[cfg(test)]
#[path = "stored_tests.rs"]
mod tests;
