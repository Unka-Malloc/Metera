use serde_json::{Value, json};
use wasmi::{Instance, Linker, Memory, Module, Store, TypedFunc};

use crate::PROTOCOL;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid wasm module")]
    InvalidModule,
    #[error("missing export: {0}")]
    MissingExport(&'static str),
    #[error("wasm ABI version mismatch")]
    AbiMismatch,
    #[error("wasm engine failure")]
    Engine,
    #[error("invalid guest buffer")]
    Buffer,
    #[error("request too large")]
    TooLarge,
    #[error("malformed response envelope")]
    Envelope,
    #[error("kernel refused: {code}")]
    Refused { code: String },
}

impl From<wasmi::Error> for Error {
    fn from(_: wasmi::Error) -> Self {
        Error::Engine
    }
}

pub struct Kernel {
    store: Store<()>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    call: TypedFunc<(u32, u32), u64>,
    release: TypedFunc<u32, ()>,
}

impl Kernel {
    pub fn new(wasm: &[u8]) -> Result<Self, Error> {
        if wasm.len() < 4 || &wasm[..4] != b"\0asm" {
            return Err(Error::InvalidModule);
        }
        let engine = wasmi::Engine::default();
        let module = Module::new(&engine, wasm).map_err(|_| Error::InvalidModule)?;
        let mut store = Store::new(&engine, ());
        let linker = <Linker<()>>::new(&engine);
        let instance: Instance = linker.instantiate_and_start(&mut store, &module)?;
        let abi: TypedFunc<(), u32> = instance
            .get_typed_func(&store, "sb_abi_version")
            .map_err(|_| Error::MissingExport("sb_abi_version"))?;
        if abi.call(&mut store, ())? != 1 {
            return Err(Error::AbiMismatch);
        }
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(Error::MissingExport("memory"))?;
        let alloc = instance
            .get_typed_func(&store, "sb_alloc")
            .map_err(|_| Error::MissingExport("sb_alloc"))?;
        let call = instance
            .get_typed_func(&store, "sb_call")
            .map_err(|_| Error::MissingExport("sb_call"))?;
        let release = instance
            .get_typed_func(&store, "sb_release")
            .map_err(|_| Error::MissingExport("sb_release"))?;
        Ok(Self {
            store,
            memory,
            alloc,
            call,
            release,
        })
    }

    pub fn invoke(&mut self, request: &[u8]) -> Result<Vec<u8>, Error> {
        let len = u32::try_from(request.len()).map_err(|_| Error::TooLarge)?;
        let in_ptr = self.alloc.call(&mut self.store, len)?;
        if in_ptr == 0 {
            return Err(Error::Buffer);
        }
        let write_ok = (|| {
            let start = in_ptr as usize;
            let end = start.checked_add(request.len())?;
            self.memory
                .data_mut(&mut self.store)
                .get_mut(start..end)?
                .copy_from_slice(request);
            Some(())
        })()
        .is_some();
        if !write_ok {
            let _ = self.release.call(&mut self.store, in_ptr);
            return Err(Error::Buffer);
        }
        let packed = match self.call.call(&mut self.store, (in_ptr, len)) {
            Ok(packed) => packed,
            Err(error) => {
                let _ = self.release.call(&mut self.store, in_ptr);
                return Err(error.into());
            }
        };
        let out_ptr = packed as u32;
        let out_len = (packed >> 32) as u32;
        if out_ptr == 0 {
            return Err(Error::Buffer);
        }
        let output = (|| {
            let start = out_ptr as usize;
            let end = start.checked_add(out_len as usize)?;
            self.memory
                .data(&self.store)
                .get(start..end)
                .map(<[u8]>::to_vec)
        })();
        let released = self.release.call(&mut self.store, out_ptr);
        let output = output.ok_or(Error::Buffer)?;
        released?;
        Ok(output)
    }

    pub fn call(&mut self, operation: &str, payload: serde_json::Value) -> Result<Value, Error> {
        let envelope = json!({
            "protocol": PROTOCOL,
            "operation": operation,
            "payload": payload,
        });
        let request = serde_json::to_vec(&envelope).map_err(|_| Error::Envelope)?;
        let response = self.invoke(&request)?;
        parse_response(&response)
    }
}

fn parse_response(response: &[u8]) -> Result<Value, Error> {
    let body: Value = serde_json::from_slice(response).map_err(|_| Error::Envelope)?;
    if body.get("protocol") != Some(&Value::from(PROTOCOL)) {
        return Err(Error::Envelope);
    }
    match (body.get("result"), body.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => {
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .ok_or(Error::Envelope)?;
            if !matches!(
                code,
                "invalid_request"
                    | "protocol_mismatch"
                    | "unsupported_operation"
                    | "invalid_payload"
                    | "encoding_failed"
                    | "usage_declaration_invalid"
                    | "usage_schema_unsupported"
                    | "usage_record_invalid"
                    | "usage_detail_session_missing"
                    | "usage_counter_overflow"
                    | "usage_reading_incomplete"
                    | "usage_reading_inconsistent"
                    | "usage_series_mismatch"
            ) {
                return Err(Error::Envelope);
            }
            Err(Error::Refused {
                code: code.to_owned(),
            })
        }
        _ => Err(Error::Envelope),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_binaries() {
        assert!(matches!(Kernel::new(b""), Err(Error::InvalidModule)));
        assert!(matches!(Kernel::new(b"garbage"), Err(Error::InvalidModule)));
        assert!(matches!(
            Kernel::new(b"\0asm\x01\x00\x00\x00garbage"),
            Err(Error::InvalidModule)
        ));
    }

    #[test]
    fn parses_response_envelopes() {
        let ok = serde_json::to_vec(&json!({"protocol": PROTOCOL, "result": {"a": 1}})).unwrap();
        assert_eq!(parse_response(&ok).unwrap(), json!({"a": 1}));
        let null_result =
            serde_json::to_vec(&json!({"protocol": PROTOCOL, "result": null})).unwrap();
        assert_eq!(parse_response(&null_result).unwrap(), Value::Null);
        let refused = serde_json::to_vec(&json!({
            "protocol": PROTOCOL, "error": {"code": "invalid_payload"}
        }))
        .unwrap();
        assert!(matches!(
            parse_response(&refused),
            Err(Error::Refused { ref code }) if code == "invalid_payload"
        ));
        for bad in [
            serde_json::to_vec(&json!({"protocol": "other", "result": {}})).unwrap(),
            serde_json::to_vec(&json!({"protocol": PROTOCOL})).unwrap(),
            serde_json::to_vec(
                &json!({"protocol": PROTOCOL, "result": {}, "error": {"code": "invalid_request"}}),
            )
            .unwrap(),
            serde_json::to_vec(&json!({"protocol": PROTOCOL, "result": {}, "error": null}))
                .unwrap(),
            serde_json::to_vec(&json!({"protocol": PROTOCOL, "error": null})).unwrap(),
            serde_json::to_vec(&json!({"protocol": PROTOCOL, "error": {}})).unwrap(),
            serde_json::to_vec(&json!({"protocol": PROTOCOL, "error": {"code": ""}})).unwrap(),
            serde_json::to_vec(&json!({"protocol": PROTOCOL, "error": {"code": 1}})).unwrap(),
            serde_json::to_vec(
                &json!({"protocol": PROTOCOL, "error": {"code": "synthetic-private-sentinel"}}),
            )
            .unwrap(),
            b"not-json".to_vec(),
        ] {
            assert!(matches!(parse_response(&bad), Err(Error::Envelope)));
        }
        for code in [
            "usage_declaration_invalid",
            "usage_schema_unsupported",
            "usage_record_invalid",
            "usage_detail_session_missing",
            "usage_counter_overflow",
            "usage_reading_incomplete",
            "usage_reading_inconsistent",
            "usage_series_mismatch",
        ] {
            let refused = serde_json::to_vec(&json!({
                "protocol": PROTOCOL, "error": {"code": code}
            }))
            .unwrap();
            assert!(matches!(
                parse_response(&refused),
                Err(Error::Refused { code: found }) if found == code
            ));
        }
    }
}
