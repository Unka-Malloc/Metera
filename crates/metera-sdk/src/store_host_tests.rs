use crate::{Kernel, PROTOCOL};
use serde_json::json;

#[test]
fn bundled_kernel_exposes_the_store_surface() {
    let mut kernel = Kernel::bundled().unwrap();
    let result = kernel.call("describe", json!({})).unwrap();
    assert_eq!(result["protocol"], PROTOCOL);
    assert_eq!(result["abi"], 1);
    assert_eq!(
        result["operations"],
        json!([
            "describe",
            "store.schema",
            "store.plan",
            "store.decode",
            "store.normalize",
            "store.advance"
        ])
    );
    assert!(kernel.store_advance(&[]).unwrap().is_empty());
}
