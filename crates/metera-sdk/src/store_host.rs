use crate::store::{AdvanceInput, SessionUsage, UsageAdvance};
use crate::{Error, Kernel, ReadPlan, SchemaCapture, SqliteUsageSpec, UsageSession};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|_| Error::Envelope)
}

fn field<T: DeserializeOwned>(mut value: Value, key: &str) -> Result<T, Error> {
    let value = value
        .as_object_mut()
        .and_then(|value| value.remove(key))
        .ok_or(Error::Envelope)?;
    decode(value)
}

impl Kernel {
    pub fn bundled() -> Result<Self, Error> {
        Self::new(include_bytes!("../wasm/metera.wasm"))
    }

    pub fn store_schema(&mut self, spec: &SqliteUsageSpec) -> Result<ReadPlan, Error> {
        decode(self.call("store.schema", json!({"spec": spec}))?)
    }

    pub fn store_plan(
        &mut self,
        spec: &SqliteUsageSpec,
        schema: &SchemaCapture,
    ) -> Result<ReadPlan, Error> {
        decode(self.call("store.plan", json!({"spec": spec, "schema": schema}))?)
    }

    pub fn store_decode(
        &mut self,
        spec: &SqliteUsageSpec,
        records: &[Value],
    ) -> Result<Vec<UsageSession>, Error> {
        field(
            self.call("store.decode", json!({"spec": spec, "records": records}))?,
            "sessions",
        )
    }

    pub fn store_normalize(
        &mut self,
        spec: &SqliteUsageSpec,
        records: &[Value],
    ) -> Result<Vec<SessionUsage>, Error> {
        field(
            self.call("store.normalize", json!({"spec": spec, "records": records}))?,
            "sessions",
        )
    }

    pub fn store_advance(&mut self, series: &[AdvanceInput]) -> Result<Vec<UsageAdvance>, Error> {
        field(
            self.call("store.advance", json!({"series": series}))?,
            "advances",
        )
    }
}
