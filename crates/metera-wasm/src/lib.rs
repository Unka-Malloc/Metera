#[cfg(any(target_arch = "wasm32", test))]
use std::collections::BTreeMap;

#[cfg(any(target_arch = "wasm32", test))]
mod dispatch;
#[cfg(any(target_arch = "wasm32", test))]
mod store;

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Default)]
struct State {
    kernel: dispatch::Kernel,
    buffers: BTreeMap<u32, Box<[u8]>>,
}

#[cfg(any(target_arch = "wasm32", test))]
fn sb_alloc(state: &mut State, len: u32) -> u32 {
    let mut bytes = vec![0u8; usize::try_from(len).unwrap_or(1).max(1)].into_boxed_slice();
    let ptr = bytes.as_mut_ptr() as u32;
    state.buffers.insert(ptr, bytes);
    ptr
}

#[cfg(any(target_arch = "wasm32", test))]
fn sb_call(state: &mut State, ptr: u32, len: u32) -> u64 {
    let Some(input) = state.buffers.remove(&ptr) else {
        return 0;
    };
    let len = len as usize;
    if len > input.len() {
        return 0;
    }
    let mut output = state.kernel.handle(&input[..len]).into_boxed_slice();
    let out_ptr = output.as_mut_ptr() as u32;
    let out_len = output.len() as u64;
    state.buffers.insert(out_ptr, output);
    (out_len << 32) | u64::from(out_ptr)
}

#[cfg(any(target_arch = "wasm32", test))]
fn sb_release(state: &mut State, ptr: u32) {
    state.buffers.remove(&ptr);
}

#[cfg(target_arch = "wasm32")]
mod abi {
    use std::cell::RefCell;

    use super::{State, sb_alloc as alloc_impl, sb_call as call_impl, sb_release as release_impl};

    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State::default());
    }

    fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
        STATE.with(|state| f(&mut state.borrow_mut()))
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn sb_abi_version() -> u32 {
        1
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn sb_alloc(len: u32) -> u32 {
        with(|state| alloc_impl(state, len))
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn sb_call(ptr: u32, len: u32) -> u64 {
        with(|state| call_impl(state, ptr, len))
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn sb_release(ptr: u32) {
        with(|state| release_impl(state, ptr))
    }
}

#[cfg(test)]
mod abi_tests {
    use super::*;

    #[test]
    fn abi_consumes_input_and_releases_output() {
        let mut state = State::default();
        let request = br#"{"protocol":"metera.kernel.v1","operation":"describe","payload":{}}"#;
        let input = sb_alloc(&mut state, request.len() as u32);
        state.buffers.get_mut(&input).unwrap()[..request.len()].copy_from_slice(request);
        let packed = sb_call(&mut state, input, request.len() as u32);
        let output = packed as u32;
        assert_ne!(output, 0);
        assert_eq!(state.buffers.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&state.buffers[&output]).unwrap();
        assert_eq!(body["protocol"], metera_sdk::PROTOCOL);
        assert_eq!(body["result"]["abi"], 1);
        sb_release(&mut state, output);
        assert!(state.buffers.is_empty());
        assert_eq!(sb_call(&mut state, input, request.len() as u32), 0);
    }
}
