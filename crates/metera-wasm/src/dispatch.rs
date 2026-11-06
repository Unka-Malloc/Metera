use acop_usage_core::{SchemaCapture, SqliteUsageSpec};
use metera_sdk::store::AdvanceInput;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::BTreeSet;

use metera_sdk::PROTOCOL;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: String,
    operation: String,
    payload: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Describe {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schema {
    spec: SqliteUsageSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    spec: SqliteUsageSpec,
    schema: SchemaCapture,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Records {
    spec: SqliteUsageSpec,
    records: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Advance {
    series: Vec<AdvanceInput>,
}

fn payload<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    if !value.is_object() {
        return Err("invalid_payload".to_owned());
    }
    serde_json::from_value(value).map_err(|_| "invalid_payload".to_owned())
}

fn encoded<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|_| "encoding_failed".to_owned())
}

#[derive(Default)]
pub(crate) struct Kernel;

impl Kernel {
    pub(crate) fn handle(&mut self, input: &[u8]) -> Vec<u8> {
        let body = match self.respond(input) {
            Ok(result) => json!({"protocol": PROTOCOL, "result": result}),
            Err(code) => json!({"protocol": PROTOCOL, "error": {"code": code}}),
        };
        serde_json::to_vec(&body).unwrap_or_else(|_| {
            b"{\"protocol\":\"metera.kernel.v1\",\"error\":{\"code\":\"encoding_failed\"}}".to_vec()
        })
    }

    fn respond(&mut self, input: &[u8]) -> Result<Value, String> {
        let request: Value =
            serde_json::from_slice(input).map_err(|_| "invalid_request".to_owned())?;
        if !request.is_object() {
            return Err("invalid_request".to_owned());
        }
        let request: Request =
            serde_json::from_value(request).map_err(|_| "invalid_request".to_owned())?;
        if request.protocol != PROTOCOL {
            return Err("protocol_mismatch".to_owned());
        }
        match request.operation.as_str() {
            "describe" => {
                let _: Describe = payload(request.payload)?;
                Ok(json!({
                    "protocol": PROTOCOL,
                    "version": env!("CARGO_PKG_VERSION"),
                    "abi": 1,
                    "operations": [
                        "describe", "store.schema", "store.plan", "store.decode",
                        "store.normalize", "store.advance"
                    ]
                }))
            }
            "store.schema" => {
                let req: Schema = payload(request.payload)?;
                encoded(acop_usage_core::kernel::schema_plan(&req.spec).map_err(|e| e.to_string())?)
            }
            "store.plan" => {
                let req: Plan = payload(request.payload)?;
                encoded(
                    acop_usage_core::kernel::data_plan(&req.spec, &req.schema)
                        .map_err(|e| e.to_string())?,
                )
            }
            "store.decode" | "store.normalize" => {
                let req: Records = payload(request.payload)?;
                let mut seen = BTreeSet::new();
                let mut output = Vec::with_capacity(req.records.len());
                for record in req.records {
                    let decoded = acop_usage_core::kernel::decode_record(&req.spec, &record)
                        .map_err(|e| e.to_string())?;
                    if !seen.insert(decoded.id.clone()) {
                        return Err("usage_record_invalid".to_owned());
                    }
                    output.push(if request.operation == "store.decode" {
                        encoded(decoded)?
                    } else {
                        encoded(
                            crate::store::normalize(
                                &decoded,
                                req.spec.input_accounting,
                                req.spec.sessions.requests.is_some(),
                            )
                            .map_err(|e| e.to_string())?,
                        )?
                    });
                }
                Ok(json!({"sessions": output}))
            }
            "store.advance" => {
                let req: Advance = payload(request.payload)?;
                let mut seen = BTreeSet::new();
                let mut output = Vec::with_capacity(req.series.len());
                for series in req.series {
                    if !seen.insert(series.current.id.clone()) {
                        return Err("usage_series_mismatch".to_owned());
                    }
                    output.push(
                        crate::store::advance(
                            series.previous.as_ref(),
                            &series.current,
                            series.count_initial,
                        )
                        .map_err(|e| e.to_string())?,
                    );
                }
                Ok(json!({"advances": output}))
            }
            _ => Err("unsupported_operation".to_owned()),
        }
    }
}
#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
